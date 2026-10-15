use super::*;
use super::super::{Plan, SubagentResult, report};
use crate::tools::Tool;

fn event(case: &str) {
    eprintln!("{}", json!({"suite":"subagent_decision", "case":case, "phase":"start"}));
}

fn node(id: &str, needs: &[&str]) -> Value {
    json!({"id":id,"needs":needs,"call":{"agent":"worker","task":id}})
}

fn source() -> Value {
    let mut value = node("source", &[]);
    value["call"]["outputSchema"] = json!(true);
    value
}

fn branch(id: &str, expected: Value) -> Value {
    let mut value = node(id, &["source"]);
    value["when"] = json!({"from":"source","pointer":"/verdict","equals":expected});
    value
}

fn plan(nodes: Vec<Value>) -> Plan {
    Plan::parse(&json!({"workflow":nodes})).unwrap().unwrap()
}

fn records(plan: &Plan, data: Value) -> Vec<Record> {
    let mut records: Vec<_> = plan.nodes.iter().map(|node| Record::pending(node, &plan.nodes)).collect();
    let mut result = SubagentResult::unknown(plan.nodes[0].call.clone(), None);
    result.status = SubagentStatus::Completed;
    result.is_error = false;
    result.error = None;
    result.schema_valid = Some(true);
    result.data = Some(data);
    records[0].settle(result, 0);
    records
}

#[test]
fn gates_refuse_missing_null_unknown_or_indirect_policy_before_execution() {
    event("gate-preflight");
    for when in [Value::Null, json!(false), json!([]),
        json!({"from":"source","pointer":"/verdict"}),
        json!({"from":"source","pointer":"/verdict","equals":true,"retry":true}),
        json!({"from":"source","pointer":"/verdict","exists":true}),
        json!({"from":"missing","pointer":"/verdict","equals":true}),
        json!({"from":"source","pointer":"/bad~2","equals":true}),
        json!({"from":"source","pointer":"#/verdict","equals":true}),
        json!({"from":"source","pointer":"/verdict","equals":"x".repeat(MAX_CONDITION_BYTES)})] {
        let mut dependent = branch("dependent", json!(true));
        dependent["when"] = when;
        assert!(Plan::parse(&json!({"workflow":[source(),dependent]})).is_err());
    }
    let mut dependent = branch("dependent", json!(true));
    dependent["needs"] = json!(["middle"]);
    assert!(Plan::parse(&json!({"workflow":[source(),node("middle", &["source"]),dependent]})).is_err());
    for request in [
        json!({"agent":"worker","task":"run","when":null}),
        json!({"tasks":[{"agent":"worker","task":"run","when":null}]}),
        json!({"chain":[{"agent":"worker","task":"run","when":null}]})
    ] {
        assert!(Plan::parse(&request).is_err());
    }
}

#[test]
fn gate_equality_preserves_json_types_null_escaped_keys_and_literal_strings() {
    event("typed-comparisons");
    for (actual, expected, matches) in [
        (json!(false),json!(false),true), (json!(false),json!(0),false),
        (json!(null),json!(null),true), (json!("0"),json!(0),false),
        (json!([false,null,0]),json!([false,null,0]),true),
        (json!("  雪  "),json!("雪"),false),
        (json!({"a":1,"b":2}),json!({"b":2,"a":1}),true),
        (json!("{{previous}}"),json!("{{previous}}"),true),
    ] {
        let mut dependent = branch("dependent", expected);
        dependent["when"]["pointer"] = json!("/a~1b/~0key/0");
        let plan = plan(vec![source(),dependent]);
        let records = records(&plan,json!({"a/b":{"~key":[actual]}}));
        let admission = plan.nodes[1].decision.evaluate(&[0],&records);
        assert_eq!(admission.reason.is_none(),matches);
        assert_eq!(admission.not_selected,!matches);
        assert_eq!(admission.condition,Some(if matches {"matched"} else {"not_matched"}));
    }
    let mut dependent = branch("dependent",json!(null));
    dependent["when"]["pointer"] = json!("");
    let plan = plan(vec![source(),dependent]);
    assert!(plan.nodes[1].decision.evaluate(&[0],&records(&plan,Value::Null)).reason.is_none());
}

#[test]
fn absent_or_unvalidated_data_is_never_a_false_or_successful_gate() {
    event("gate-evidence");
    let plan = plan(vec![source(),branch("dependent",json!(null))]);
    let missing = records(&plan,json!({}));
    let admission = plan.nodes[1].decision.evaluate(&[0],&missing);
    assert_eq!(admission.reason,Some("condition_value_missing"));
    assert!(!admission.not_selected);
    for invalid in 0..4 {
        let mut records = records(&plan,json!({"verdict":null}));
        let result = records[0].result.as_mut().unwrap();
        match invalid {
            0 => result.schema_valid = Some(false),
            1 => result.data = None,
            2 => result.is_error = true,
            _ => result.status = SubagentStatus::Running,
        }
        let admission = plan.nodes[1].decision.evaluate(&[0],&records);
        assert_eq!(admission.reason,Some("condition_data_unavailable"));
        assert!(!admission.not_selected);
    }
}

#[test]
fn dependency_failure_wins_over_exclusion_and_provisional_sources() {
    event("failure-precedence");
    let plan = plan(vec![source(),node("excluded",&[]),node("join", &["source","excluded"])]);
    let mut records = records(&plan,json!({}));
    records[1].state = State::NotSelected;
    let admission = plan.nodes[2].decision.evaluate(&[0,1],&records);
    assert!(admission.not_selected);
    assert_eq!(admission.reason,Some("dependency_not_selected"));
    for state in [State::Rejected,State::Skipped,State::Pending] {
        records[0].state = state;
        let admission = plan.nodes[2].decision.evaluate(&[0,1],&records);
        assert!(!admission.not_selected);
        assert_eq!(admission.reason,Some("dependency_failed"));
        assert_eq!(admission.sources,vec![0]);
    }
    assert_eq!(plan.nodes[2].decision.evaluate(&[0,1],&[]).reason,Some("dependency_failed"));
}

#[test]
fn false_conditions_preserve_nonexecution_in_text_and_structured_reports() {
    event("nonexecution-report");
    let plan = plan(vec![source(),branch("dependent",json!("EXPECTED-PRIVATE-CANARY"))]);
    let mut records = records(&plan,json!({"verdict":"no"}));
    records[1].state = State::NotSelected;
    records[1].skip_reason = Some("condition_false");
    records[1].condition = Some("not_matched");
    records[1].blocked_by = vec!["source".into()];
    let output = report(records,1,None);
    assert!(!output.is_error);
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(!serialized.contains("EXPECTED-PRIVATE-CANARY"));
    let details = output.details.unwrap();
    assert_eq!(details["acceptedNodes"],1);
    assert_eq!(details["notSelectedNodes"],1);
    assert_eq!(details["skippedNodes"],0);
    assert_eq!(details["nodes"][1]["result"],Value::Null);
    assert_eq!(details["nodes"][1]["hubId"],Value::Null);
    assert_eq!(details["nodes"][1]["wave"],Value::Null);
    assert!(serialized.contains("State: not_selected"));
    assert!(serialized.contains("Condition: not_matched"));
}

#[test]
fn effective_schema_is_required_even_when_expected_value_will_not_match() {
    event("gate-schema-provenance");
    let mut untyped = source();
    untyped["call"].as_object_mut().unwrap().remove("outputSchema");
    let plan = plan(vec![untyped,branch("dependent",json!(false))]);
    let mut agent = crate::subagents::tan_agent_definition();
    agent.name = "worker".into();
    let mut agents = BTreeMap::from([("worker".into(),agent)]);
    assert!(plan.validate_definitions(&agents).is_err());
    agents.get_mut("worker").unwrap().output_schema = Some(json!(true));
    assert!(plan.validate_definitions(&agents).is_ok());
    let schema = crate::subagents::SubagentTool::new(std::path::Path::new(".")).parameters();
    assert_eq!(schema["properties"]["workflow"]["items"]["properties"]["when"]["required"],
        json!(["from","pointer","equals"]));
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::subagents::SubagentTool;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    // Executes as a real native child, not a simulated ToolOutput. No models
    // are contacted. A gate may admit only the intended branch's file effects.
    const CHILD: &str = r#"#!/bin/sh
set -eu
for arg do task="$arg"; done
printf '%s\n' "$task" >> launches
case "$task" in
  'Task: source') answer='{"verdict":"repair"}' ;;
  'Task: missing') answer='{}' ;;
  'Task: invalid'*) answer='not JSON' ;;
  'Task: broken') exit 7 ;;
  'Task: repair') printf 'repaired\n' > repaired.txt; answer='"done"' ;;
  *) answer='"done"' ;;
esac
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":'"$(printf '%s' "$answer" | sed 's/\\/\\\\/g;s/"/\\"/g;s/^/"/;s/$/"/')"'}]}]}'
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        tool: SubagentTool,
        runtime: asupersync::runtime::Runtime,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let global = dir.path().join("global");
            std::fs::create_dir_all(global.join("agents")).unwrap();
            std::fs::write(global.join("agents/worker.md"),
                "---\nname: worker\ndescription: decision fixture\ntools: []\n---\nReturn the fixture result.\n").unwrap();
            let child = dir.path().join("child.sh");
            std::fs::write(&child,CHILD).unwrap();
            std::fs::set_permissions(&child,std::fs::Permissions::from_mode(0o700)).unwrap();
            let cwd = dir.path().join("workspace");
            std::fs::create_dir_all(&cwd).unwrap();
            let tool = SubagentTool::with_paths(cwd,global,child).with_timeout(Duration::from_secs(5));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
            Self { dir,tool,runtime }
        }
        fn execute(&self,nodes: Vec<Value>) -> Result<crate::tools::ToolOutput> {
            self.runtime.block_on(self.tool.execute("decision-native",
                json!({"workflow":nodes,"scope":"user","concurrency":2}),None))
        }
        fn launches(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("workspace/launches")).unwrap_or_default()
        }
    }

    #[test]
    fn typed_verdict_selects_one_native_branch_and_excludes_its_alternative_tree() {
        event("native-choice");
        let fixture = Fixture::new();
        let output = fixture.execute(vec![source(),branch("accept",json!("approve")),
            branch("repair",json!("repair")),node("never",&["accept"]),node("after",&["repair"])]).unwrap();
        assert!(!output.is_error,"{output:?}");
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"],3);
        assert_eq!(details["notSelectedNodes"],2);
        assert_eq!(details["skippedNodes"],0);
        assert_eq!(details["wavesExecuted"],3);
        assert_eq!(details["nodes"][1]["condition"],"not_matched");
        assert_eq!(details["nodes"][2]["condition"],"matched");
        assert_eq!(details["nodes"][3]["skipReason"],"dependency_not_selected");
        assert_eq!(fixture.launches(),"Task: source\nTask: repair\nTask: after\n");
        assert_eq!(std::fs::read(fixture.dir.path().join("workspace/repaired.txt")).unwrap(),b"repaired\n");
        for index in [1,3] {
            assert_eq!(details["nodes"][index]["result"],Value::Null);
            assert_eq!(details["nodes"][index]["hubId"],Value::Null);
        }
    }

    #[test]
    fn missing_and_schema_invalid_verdicts_block_instead_of_selecting_false_branch() {
        event("native-invalid-evidence");
        for (task,reason) in [("missing","condition_value_missing"),("invalid","dependency_failed")] {
            let fixture = Fixture::new();
            let mut producer = source();
            producer["call"]["task"] = json!(task);
            let output = fixture.execute(vec![producer,branch("never",json!("repair"))]).unwrap();
            assert!(output.is_error);
            let details = output.details.unwrap();
            assert_eq!(details["notSelectedNodes"],0);
            assert_eq!(details["nodes"][1]["skipReason"],reason);
            assert!(!fixture.launches().contains("never"));
        }
    }

    #[test]
    fn malformed_or_untyped_later_gates_prevent_all_native_launches() {
        event("native-gate-admission");
        let fixture = Fixture::new();
        let mut untyped = source();
        untyped["call"].as_object_mut().unwrap().remove("outputSchema");
        assert!(fixture.execute(vec![untyped,branch("never",json!("repair"))]).is_err());
        let mut malformed = branch("never",json!("repair"));
        malformed["when"] = Value::Null;
        assert!(fixture.execute(vec![source(),malformed]).is_err());
        // Even an eventually false branch must name a valid native agent.
        let mut unknown = branch("never",json!("approve"));
        unknown["call"]["agent"] = json!("missing-agent");
        assert!(fixture.execute(vec![source(),unknown]).is_err());
        assert!(fixture.launches().is_empty());
    }

    #[test]
    fn unselected_gate_does_not_expand_inputs_or_capture_its_workspace() {
        event("native-no-unselected-effects");
        let fixture = Fixture::new();
        let mut branch = branch("never",json!("approve"));
        branch["call"]["cwd"] = json!("nonexistent-child-workspace");
        branch["call"]["isolation"] = json!("worktree");
        branch["inputs"] = json!({"missing":{"from":"source","pointer":"/missing"}});
        let output = fixture.execute(vec![source(),branch]).unwrap();
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["nodes"][1]["state"],"not_selected");
        assert_eq!(fixture.launches(),"Task: source\n");
        assert!(!fixture.dir.path().join("workspace/nonexistent-child-workspace").exists());
    }
}
