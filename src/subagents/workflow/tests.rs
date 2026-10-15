use super::*;
use crate::tools::Tool;

fn event(case: &str) {
    eprintln!("{}", json!({"suite":"subagent_workflow", "case":case, "phase":"start"}));
}

fn node(id: &str, needs: &[&str]) -> Value {
    json!({"id":id, "needs":needs, "call":{"agent":"worker", "task":id}})
}

fn plan(nodes: Vec<Value>) -> Plan {
    Plan::parse(&json!({"workflow":nodes})).unwrap().unwrap()
}

#[test]
fn forward_references_form_deterministic_fork_join_waves() {
    event("topological-waves");
    let plan = plan(vec![node("join", &["left","right"]), node("left", &["seed"]),
        node("right", &["seed"]), node("independent", &[]), node("seed", &[])]);
    assert_eq!(plan.waves, vec![vec![3,4], vec![1,2], vec![0]]);
    assert_eq!(plan.nodes[0].needs, vec![1,2]);
    assert_eq!(plan.nodes[0].id, "join");
}

#[test]
fn cycles_unknown_edges_and_ambiguous_shapes_fail_before_execution() {
    event("graph-admission");
    for nodes in [
        vec![node("a", &["a"])],
        vec![node("a", &["b"]), node("b", &["a"])],
        vec![node("a", &[]), node("b", &["c"]), node("c", &["b"])],
        vec![node("a", &["missing"])],
        vec![node("a", &[]), node("a", &[])],
        vec![node("a", &[]), node("b", &["a","a"])],
        vec![Value::Null],
        vec![node("with spaces", &[])],
    ] {
        assert!(Plan::parse(&json!({"workflow":nodes})).is_err());
    }
    for value in [Value::Null, json!("a"), json!(false)] {
        let mut entry = node("a", &[]);
        entry["needs"] = value;
        assert!(Plan::parse(&json!({"workflow":[entry]})).is_err());
    }
    let mut entry = node("a", &[]);
    entry["retry"] = json!(true);
    assert!(Plan::parse(&json!({"workflow":[entry]})).is_err());
}

#[test]
fn workflow_mode_is_explicit_and_cannot_silently_ignore_execution_policy() {
    event("mode-and-limits");
    assert!(Plan::parse(&json!({"agent":"worker","task":"normal"})).unwrap().is_none());
    for field in ["agent", "task", "tasks", "chain", "cwd", "isolation", "isoApply", "outputSchema"] {
        let mut request = json!({"workflow":[node("a", &[])]});
        request[field] = Value::Null;
        assert!(Plan::parse(&request).is_err());
    }
    for value in [Value::Null, json!([]), json!({}), json!(vec![node("a", &[]); MAX_NODES + 1])] {
        assert!(Plan::parse(&json!({"workflow":value})).is_err());
    }
    for (key, values) in [
        ("concurrency", vec![Value::Null,json!(0),json!(9),json!(1.5),json!("2")]),
        ("timeoutSeconds", vec![Value::Null,json!(0),json!(86401),json!(false)]),
        ("scope", vec![Value::Null,json!("unknown"),json!(true)]),
    ] {
        for value in values {
            let mut request = json!({"workflow":[node("a", &[])]});
            request[key] = value;
            assert!(Plan::parse(&request).is_err());
        }
    }
    let mut large = node("a", &[]);
    large["call"]["task"] = json!("x".repeat(MAX_REQUEST_BYTES));
    assert!(Plan::parse(&json!({"workflow":[large]})).is_err());
}

#[test]
fn every_native_task_is_validated_including_later_nodes() {
    event("call-policy");
    for (field, value) in [
        ("cwd", Value::Null), ("isolation", json!("copy")), ("isoApply", json!("apply")),
        ("schemaMode", json!("ignore")), ("outputSchema", Value::Null), ("task",json!("")),
        ("agent",json!(3)), ("tools",json!(["bash"])), ("model",json!("another-model")),
    ] {
        let mut later = node("b", &["a"]);
        later["call"][field] = value;
        assert!(Plan::parse(&json!({"workflow":[node("a", &[]),later]})).is_err());
    }
    let mut isolated = node("a", &[]);
    isolated["call"]["isolation"] = json!("worktree");
    isolated["call"]["isoApply"] = json!("keep");
    isolated["call"]["outputSchema"] = json!(true);
    let plan = plan(vec![isolated]);
    assert_eq!(plan.nodes[0].call.iso_apply.as_deref(), Some("keep"));
    assert_eq!(plan.nodes[0].call.output_schema, Some(json!(true)));
}

#[test]
fn encoded_limits_count_escape_bytes_without_an_extra_payload_copy() {
    event("encoded-size");
    let value = json!({"x":"\"雪\"\n\0"});
    let size = serde_json::to_vec(&value).unwrap().len();
    assert_eq!(encoded_size(&value, size), Some(size));
    assert_eq!(encoded_size(&value, size - 1), None);
}

fn completed(call: SubagentTask) -> SubagentResult {
    let mut result = SubagentResult::unknown(call, None);
    result.status = SubagentStatus::Completed;
    result.error = None;
    result.is_error = false;
    result.output = "accepted native output".into();
    result.hub_id = Some("native-fixture-id".into());
    result
}

#[test]
fn permissive_invalid_results_cannot_become_dependency_evidence() {
    event("acceptance-state");
    let plan = plan(vec![node("a", &[]), node("b", &["a"])]);
    let mut first = Record::pending(&plan.nodes[0], &plan.nodes);
    let mut result = completed(plan.nodes[0].call.clone());
    result.schema_valid = Some(false);
    result.data = Some(json!({"unvalidated":true}));
    first.settle(result, 0);
    assert_eq!(first.state, State::Rejected);
    assert_eq!(first.native_is_error, Some(false));
    let mut second = Record::pending(&plan.nodes[1], &plan.nodes);
    second.skip("dependency_failed", vec!["a".into()]);
    let output = report(vec![first, second], 1, None);
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["rejectedNodes"], 1);
    assert_eq!(details["skippedNodes"], 1);
    assert_eq!(details["nodes"][0]["hubId"], "native-fixture-id");
    assert_eq!(details["nodes"][0]["result"]["schemaValid"], false);
    assert_eq!(details["nodes"][1]["result"], Value::Null);
    assert_eq!(details["nodes"][1]["blockedBy"], json!(["a"]));
}

#[test]
fn public_schema_preserves_existing_modes_and_advertises_native_calls() {
    event("public-schema");
    let schema = SubagentTool::new(std::path::Path::new(".")).parameters();
    for field in ["agent", "tasks", "chain", "workflow"] {
        assert!(schema["properties"].get(field).is_some());
    }
    assert_eq!(schema["properties"]["workflow"]["maxItems"], MAX_NODES);
    assert_eq!(schema["properties"]["workflow"]["items"]["properties"]["call"]["$ref"], "#/definitions/workflowTask");
    assert_eq!(schema["definitions"]["task"]["properties"]["outputSchema"]["type"], "object");
    assert_eq!(schema["definitions"]["workflowTask"]["properties"]["outputSchema"]["type"], json!(["object", "boolean"]));
}

fn input_node(pointer: &str) -> Value {
    let mut entry = node("consumer", &["source"]);
    entry["inputs"] = json!({"payload":{"from":"source","pointer":pointer}});
    entry
}

fn source_records(plan: &Plan, data: Value) -> Vec<Record> {
    let mut records: Vec<_> = plan.nodes.iter().map(|node| Record::pending(node, &plan.nodes)).collect();
    let mut result = completed(plan.nodes[0].call.clone());
    result.schema_valid = Some(true);
    result.data = Some(data);
    records[0].settle(result, 0);
    records
}

fn decoded_inputs(task: &str) -> Value {
    let (_, data) = task.split_once(INPUT_PREFIX).unwrap();
    serde_json::from_str(data.strip_suffix(INPUT_SUFFIX).unwrap()).unwrap()
}

#[test]
fn typed_input_references_require_explicit_direct_edges_and_valid_json_pointers() {
    event("input-admission");
    for reference in [Value::Null, json!({"from":"source"}),
        json!({"from":"absent","pointer":""}),
        json!({"from":"source","pointer":3}),
        json!({"from":"source","pointer":"/x","fallback":0})] {
        let mut consumer = input_node("");
        consumer["inputs"]["payload"] = reference;
        assert!(Plan::parse(&json!({"workflow":[node("source", &[]),consumer]})).is_err());
    }
    for pointer in ["relative", "#/uri", "/bad~", "/bad~3", "/control\n"] {
        assert!(Plan::parse(&json!({"workflow":[node("source", &[]),input_node(pointer)]})).is_err());
    }
    assert!(validate_pointer(&format!("/{}", "a".repeat(1024))).is_err());
    assert!(validate_pointer(&"/a".repeat(33)).is_err());
    for pointer in ["", "/a~1b/~0key/0", "/雪/", "/~01"] {
        assert!(validate_pointer(pointer).is_ok());
    }
    let mut consumer = input_node("");
    consumer["needs"] = json!(["middle"]);
    assert!(Plan::parse(&json!({"workflow":[node("source", &[]),node("middle", &["source"]),consumer]})).is_err());
}

#[test]
fn null_unknown_and_excessive_input_maps_never_disable_binding() {
    event("input-map-shapes");
    for value in [Value::Null, json!([]), json!(true), json!({"bad name":{"from":"source","pointer":""}})] {
        let mut consumer = input_node("");
        consumer["inputs"] = value;
        assert!(Plan::parse(&json!({"workflow":[node("source", &[]),consumer]})).is_err());
    }
    let mut consumer = input_node("");
    consumer["inputs"] = json!((0..33).map(|i| (format!("key{i}"),json!({"from":"source","pointer":""})))
        .collect::<BTreeMap<_,_>>());
    assert!(Plan::parse(&json!({"workflow":[node("source", &[]),consumer]})).is_err());
    let mut empty = node("consumer", &[]);
    empty["inputs"] = json!({});
    let plan = plan(vec![empty]);
    assert_eq!(plan.nodes[0].task_with_inputs(&[]).unwrap().task, "consumer");
}

#[test]
fn handoffs_preserve_json_types_null_and_literal_keys_without_template_expansion() {
    event("input-types");
    let mut consumer = input_node("/a~1b/~0key");
    consumer["call"]["task"] = json!("Keep {previous} and {{previous.data.x}} literal.");
    let plan = plan(vec![node("source", &[]),consumer]);
    let value = json!([false,null,0,"  雪  ",{"close":"</workflow-inputs>","flag":"--model injected"}]);
    let records = source_records(&plan, json!({"a/b":{"~key":value}}));
    let task = plan.nodes[1].task_with_inputs(&records).unwrap();
    assert!(task.task.starts_with("Keep {previous} and {{previous.data.x}} literal."));
    assert_eq!(decoded_inputs(&task.task)["payload"], value);
    assert_eq!(task.task.matches("</workflow-inputs>").count(), 1);
    assert_eq!(task.task.matches("<workflow-inputs>").count(), 1);
    assert_eq!(task.agent, plan.nodes[1].call.agent);
    assert_eq!(task.cwd, plan.nodes[1].call.cwd);
    assert_eq!(task.isolation, plan.nodes[1].call.isolation);
    assert_eq!(task.output_schema, plan.nodes[1].call.output_schema);
    assert_eq!(task.schema_mode, plan.nodes[1].call.schema_mode);
}

#[test]
fn absent_or_unvalidated_data_is_not_guessed_from_prose_or_coerced_to_null() {
    event("input-evidence");
    let plan = plan(vec![node("source", &[]),input_node("/value")]);
    let mut records = source_records(&plan, json!({"value":null}));
    assert_eq!(decoded_inputs(&plan.nodes[1].task_with_inputs(&records).unwrap().task), json!({"payload":null}));
    records[0].result.as_mut().unwrap().data = Some(json!({"other":0}));
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_input_missing");
    records[0].result.as_mut().unwrap().data = None;
    records[0].result.as_mut().unwrap().output = "{\"value\":null}".into();
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_data_unavailable");
    records[0].result.as_mut().unwrap().data = Some(json!({"value":0}));
    records[0].result.as_mut().unwrap().schema_valid = None;
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_data_unavailable");
    records[0].result.as_mut().unwrap().schema_valid = Some(true);
    records[0].state = State::Rejected;
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_data_unavailable");
}

#[test]
fn escaping_and_authored_prompt_bytes_share_one_handoff_limit() {
    event("input-size");
    let mut plan = plan(vec![node("source", &[]),input_node("")]);
    for data in [json!("x".repeat(MAX_TASK_BYTES)), json!("<".repeat(MAX_TASK_BYTES / 2))] {
        let records = source_records(&plan, data);
        assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_input_limit");
    }
    let records = source_records(&plan, json!(0));
    let task = plan.nodes[1].task_with_inputs(&records).unwrap();
    let overhead = task.task.len() - plan.nodes[1].call.task.len();
    plan.nodes[1].call.task = "x".repeat(MAX_TASK_BYTES - overhead);
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap().task.len(), MAX_TASK_BYTES);
    plan.nodes[1].call.task.push('x');
    assert_eq!(plan.nodes[1].task_with_inputs(&records).unwrap_err(), "dependency_input_limit");
}

#[test]
fn input_writer_is_lossless_across_split_unicode_bytes_and_refuses_overflow() {
    event("input-writer");
    let mut writer = InputWriter { bytes:Vec::new(), limit:9 };
    for byte in "雪<".as_bytes() {
        writer.write_all(std::slice::from_ref(byte)).unwrap();
    }
    assert_eq!(String::from_utf8(writer.bytes.clone()).unwrap(), "雪\\u003c");
    assert!(writer.write_all(b"x").is_err());
    assert_eq!(writer.bytes.len(), 9);
}

#[test]
fn input_sources_require_effective_schemas_before_any_native_work() {
    event("input-schema-preflight");
    let plan = plan(vec![node("source", &[]),input_node("")]);
    let mut agent = crate::subagents::tan_agent_definition();
    agent.name = "worker".into();
    let mut agents = BTreeMap::from([("worker".into(),agent)]);
    assert!(plan.validate_definitions(&agents).is_err());
    agents.get_mut("worker").unwrap().output_schema = Some(json!(true));
    assert!(plan.validate_definitions(&agents).is_ok());
}

#[test]
fn text_only_providers_receive_acceptance_and_real_child_handles() {
    event("model-visible-identities");
    let plan = plan(vec![node("source", &[]),node("blocked", &["source"])]);
    let mut records = source_records(&plan, json!(null));
    records[1].skip("dependency_input_missing", vec!["source".into()]);
    let output = report(records, 1, None);
    let ContentBlock::Text(text) = &output.content[0] else { panic!("text output"); };
    assert!(text.text.contains("State: accepted; wave: 0; hub: \"native-fixture-id\""));
    assert!(text.text.contains("State: skipped; wave: not started; hub: none"));
    assert!(text.text.contains("dependency_input_missing"));
}

#[cfg(unix)]
mod native {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    // An actual child process speaks the native terminal protocol. It does
    // not call a model. Barrier files prove overlapping roots; successful
    // shell exit alone cannot stand in for an agent_end result.
    const CHILD: &str = r#"#!/bin/sh
set -eu
for arg do task="$arg"; done
printf '%s\n' "$task" >> launches
case "$task" in
  'Task: LEFT'|'Task: RIGHT')
    side=${task#Task: }
    : > "begun-$side"
    n=0
    while [ ! -f begun-LEFT ] || [ ! -f begun-RIGHT ]; do
      n=$((n + 1)); [ "$n" -lt 200 ] || exit 19
      sleep 0.01
    done
    : > "done-$side"
    ;;
  'Task: JOIN') [ -f done-LEFT ] && [ -f done-RIGHT ] || exit 20 ;;
  'Task: FAIL') exit 7 ;;
  'Task: BADJSON'*)
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"not JSON"}]}]}'
    exit 0 ;;
  'Task: WRITE_A') printf 'new alpha\n' > alpha.txt ;;
  'Task: WRITE_B') printf 'new beta\n' > beta.txt ;;
  'Task: VERIFY')
    [ "$(cat alpha.txt)" = 'new alpha' ] && [ "$(cat beta.txt)" = 'new beta' ] || exit 21
    printf 'verified both\n' > joined.txt ;;
  'Task: SLOW') sleep 10 ;;
  'Task: PRODUCE')
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"{\"a/b\":{\"~key\":[false,null,0,\"  雪  \"]},\"flag\":\"--model injected\"}"}]}]}'
    exit 0 ;;
  'Task: PRODUCE_TWO')
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"{\"count\":2}"}]}]}'
    exit 0 ;;
  'Task: CONSUME'*)
    printf '%s' "$task" > received
    next=0; count=0; model=''
    for arg do
      if [ "$next" -eq 1 ]; then model="$arg"; next=0; fi
      if [ "$arg" = '--model' ]; then next=1; count=$((count + 1)); fi
    done
    [ "$count" -eq 1 ] && [ "$model" = 'pinned-fixture' ] || exit 29 ;;
esac
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"native result"}]}]}'
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        tool: SubagentTool,
        runtime: asupersync::runtime::Runtime,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("workspace");
            let global = dir.path().join("global");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(global.join("agents")).unwrap();
            std::fs::write(global.join("agents/worker.md"),
                "---\nname: worker\ndescription: workflow fixture\ntools: []\n---\nReturn the fixture result.\n").unwrap();
            let child = dir.path().join("child.sh");
            std::fs::write(&child, CHILD).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
            let tool = SubagentTool::with_paths(cwd, global, child).with_timeout(Duration::from_secs(10));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
            Self { dir, tool, runtime }
        }

        fn root(&self) -> std::path::PathBuf { self.dir.path().join("workspace") }

        fn run(&self, nodes: Vec<Value>) -> ToolOutput {
            self.runtime.block_on(self.tool.execute("workflow-native",
                json!({"workflow":nodes,"scope":"user","concurrency":2}), None)).unwrap()
        }

        fn launches(&self) -> String {
            std::fs::read_to_string(self.root().join("launches")).unwrap_or_default()
        }
    }

    #[test]
    fn fork_join_uses_real_parallel_children_then_preserves_authored_result_order() {
        event("native-fork-join");
        let fixture = Fixture::new();
        let output = fixture.run(vec![node("JOIN", &["LEFT","RIGHT"]), node("LEFT", &[]), node("RIGHT", &[])]);
        assert!(!output.is_error, "{output:?}");
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"], 3);
        assert_eq!(details["wavesExecuted"], 2);
        assert_eq!(details["nodes"][0]["id"], "JOIN");
        assert_eq!(details["nodes"][0]["wave"], 1);
        assert_eq!(details["nodes"][1]["wave"], 0);
        assert_eq!(details["nodes"][2]["wave"], 0);
        assert!(details["nodes"].as_array().unwrap().iter().all(|record| record["hubId"].is_string()));
        assert!(fixture.launches().ends_with("Task: JOIN\n"));
        assert_eq!(fixture.launches().lines().count(), 3);
    }

    #[test]
    fn rejected_dependencies_skip_descendants_but_independent_branches_continue() {
        event("native-branch-failure");
        let fixture = Fixture::new();
        let output = fixture.run(vec![node("FAIL", &[]), node("BLOCKED", &["FAIL"]),
            node("TRANSITIVE", &["BLOCKED"]), node("OTHER", &[]), node("AFTER_OTHER", &["OTHER"])]);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"], 2);
        assert_eq!(details["rejectedNodes"], 1);
        assert_eq!(details["skippedNodes"], 2);
        assert_eq!(details["nodes"][1]["blockedBy"], json!(["FAIL"]));
        assert_eq!(details["nodes"][2]["blockedBy"], json!(["BLOCKED"]));
        let launches = fixture.launches();
        assert!(!launches.contains("BLOCKED") && !launches.contains("TRANSITIVE"));
        assert!(launches.contains("Task: AFTER_OTHER\n"));
    }

    #[test]
    fn permissive_schema_failure_is_retained_but_not_forwarded_to_dependants() {
        event("native-schema-acceptance");
        let fixture = Fixture::new();
        let mut typed = node("BADJSON", &[]);
        typed["call"]["outputSchema"] = json!({"type":"object"});
        let output = fixture.run(vec![typed,node("BLOCKED", &["BADJSON"])]);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][0]["state"], "rejected");
        assert_eq!(details["nodes"][0]["nativeIsError"], false);
        assert_eq!(details["nodes"][0]["result"]["schemaRetries"], 1);
        assert_eq!(details["nodes"][1]["state"], "skipped");
        assert_eq!(fixture.launches().matches("Task: BADJSON").count(), 2);
        assert!(!fixture.launches().contains("BLOCKED"));
    }

    #[test]
    fn unknown_agents_and_invalid_schemas_prevent_even_earlier_root_launches() {
        event("native-preflight");
        let fixture = Fixture::new();
        for (field, value) in [("agent",json!("not-discovered")), ("outputSchema",json!({"type":"not-a-json-type"}))] {
            let mut later = node("LATER", &["EARLIER"]);
            later["call"][field] = value;
            assert!(fixture.runtime.block_on(fixture.tool.execute("workflow-preflight",
                json!({"workflow":[node("EARLIER", &[]),later],"scope":"user"}),None)).is_err());
            assert!(!fixture.root().join("launches").exists());
        }
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        output.stdout
    }

    #[test]
    fn downstream_worktrees_start_from_both_accepted_predecessor_edits() {
        event("native-worktree-waves");
        let fixture = Fixture::new();
        let root = fixture.root();
        git(&root, &["init","-q"]);
        git(&root, &["config","user.name","Workflow Fixture"]);
        git(&root, &["config","user.email","workflow@example.invalid"]);
        git(&root, &["config","commit.gpgsign","false"]);
        // The protocol fixture's launch log is deliberately outside patches.
        std::fs::write(root.join(".gitignore"), "launches\n").unwrap();
        std::fs::write(root.join("alpha.txt"), "old alpha\n").unwrap();
        std::fs::write(root.join("beta.txt"), "old beta\n").unwrap();
        git(&root, &["add","."]);
        git(&root, &["commit","-qm","fixture"]);
        let head = git(&root, &["rev-parse","HEAD"]);
        let index = std::fs::read(root.join(".git/index")).unwrap();
        std::fs::write(root.join("untracked-parent.txt"), "keep dirty state\n").unwrap();
        let mut nodes = vec![node("VERIFY", &["WRITE_A","WRITE_B"]),node("WRITE_A", &[]),node("WRITE_B", &[])];
        for entry in &mut nodes {
            entry["call"]["isolation"] = json!("worktree");
            entry["call"]["isoApply"] = json!("apply");
        }
        let output = fixture.run(nodes);
        assert!(!output.is_error, "{output:?}");
        assert_eq!(std::fs::read(root.join("joined.txt")).unwrap(), b"verified both\n");
        assert_eq!(std::fs::read(root.join("alpha.txt")).unwrap(), b"new alpha\n");
        assert_eq!(std::fs::read(root.join("beta.txt")).unwrap(), b"new beta\n");
        assert_eq!(std::fs::read(root.join("untracked-parent.txt")).unwrap(), b"keep dirty state\n");
        assert_eq!(git(&root, &["rev-parse","HEAD"]), head);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(output.details.unwrap()["nodes"][0]["wave"], 1);
    }

    #[test]
    fn one_request_deadline_stops_running_children_and_never_launches_the_next_wave() {
        event("native-deadline");
        let mut fixture = Fixture::new();
        fixture.tool = fixture.tool.with_timeout(Duration::from_secs(1));
        let output = fixture.run(vec![node("SLOW", &[]),node("NEVER", &["SLOW"])]);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][0]["state"], "rejected");
        assert_eq!(details["nodes"][1]["state"], "skipped");
        assert!(!fixture.launches().contains("NEVER"));
        assert_eq!(fixture.launches().matches("Task: SLOW").count(), 1);
    }

    #[test]
    fn cancelled_parent_cannot_start_a_workflow() {
        event("native-cancelled-owner");
        let fixture = Fixture::new();
        let owner = AgentCx::for_request();
        owner.set_cancel_requested(true);
        let error = fixture.runtime.block_on(owner.with_current(fixture.tool.execute("cancelled-workflow",
            json!({"workflow":[node("NEVER", &[])],"scope":"user"}),None))).unwrap_err();
        assert!(error.to_string().contains("CANCELLED"));
        assert!(!fixture.root().join("launches").exists());
    }

    #[test]
    fn two_validated_children_feed_one_native_join_without_rewriting_launch_controls() {
        event("native-typed-join");
        let mut fixture = Fixture::new();
        fixture.tool = fixture.tool.with_parent_model(Some(crate::tools::ToolModelSelection {
            provider:"fixture".into(), model:"pinned-fixture".into(),
        }));
        let mut source = node("PRODUCE", &[]);
        source["call"]["outputSchema"] = json!({"type":"object","required":["a/b","flag"]});
        let mut other = node("PRODUCE_TWO", &[]);
        other["call"]["outputSchema"] = json!(true);
        let mut consumer = node("CONSUME", &["PRODUCE","PRODUCE_TWO"]);
        consumer["call"]["task"] = json!("CONSUME {previous}");
        consumer["inputs"] = json!({
            "typed":{"from":"PRODUCE","pointer":"/a~1b/~0key"},
            "flag":{"from":"PRODUCE","pointer":"/flag"},
            "count":{"from":"PRODUCE_TWO","pointer":"/count"}
        });
        let output = fixture.run(vec![consumer,source,other]);
        assert!(!output.is_error, "{output:?}");
        let received = std::fs::read_to_string(fixture.root().join("received")).unwrap();
        assert!(received.starts_with("Task: CONSUME {previous}"));
        assert_eq!(decoded_inputs(&received), json!({"typed":[false,null,0,"  雪  "],"flag":"--model injected","count":2}));
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"], 3);
        assert_eq!(details["nodes"][0]["result"]["task"], "CONSUME {previous}");
        assert_eq!(fixture.launches().matches("Task: CONSUME").count(), 1);
    }

    #[test]
    fn missing_handoff_data_skips_descendants_without_launch_or_fallback() {
        event("native-missing-input");
        let fixture = Fixture::new();
        let mut source = node("PRODUCE", &[]);
        source["call"]["outputSchema"] = json!(true);
        let mut consumer = node("CONSUME", &["PRODUCE"]);
        consumer["inputs"] = json!({"missing":{"from":"PRODUCE","pointer":"/absent"}});
        let output = fixture.run(vec![source,consumer,node("NEVER", &["CONSUME"]),node("OTHER", &[])]);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"], 2);
        assert_eq!(details["nodes"][1]["skipReason"], "dependency_input_missing");
        assert_eq!(details["nodes"][1]["result"], Value::Null);
        assert_eq!(details["nodes"][2]["blockedBy"], json!(["CONSUME"]));
        let launches = fixture.launches();
        assert!(!launches.contains("CONSUME") && !launches.contains("NEVER"));
        assert!(!fixture.root().join("received").exists());
    }

    #[test]
    fn schema_free_handoff_sources_are_refused_before_any_child_launch() {
        event("native-untyped-input-preflight");
        let fixture = Fixture::new();
        let mut consumer = node("CONSUME", &["PRODUCE"]);
        consumer["inputs"] = json!({"value":{"from":"PRODUCE","pointer":"/flag"}});
        let error = fixture.runtime.block_on(fixture.tool.execute("untyped-workflow",
            json!({"workflow":[node("PRODUCE", &[]),consumer],"scope":"user"}),None)).unwrap_err();
        assert!(error.to_string().contains("output schema"));
        assert!(!fixture.root().join("launches").exists());
    }
}
