use super::super::{INPUT_PREFIX, INPUT_SUFFIX, MAX_TASK_BYTES, Plan, SubagentStatus};
use super::*;
use crate::tools::Tool;

fn event(case: &str) {
    eprintln!(
        "{}",
        json!({"suite":"subagent_fanout","case":case,"phase":"start"})
    );
}

fn source() -> Value {
    json!({"id":"source","call":{"agent":"worker","task":"SOURCE","outputSchema":true}})
}

fn mapped() -> Value {
    json!({"id":"map","needs":["source"],
        "forEach":{"from":"source","pointer":"","as":"item","maxItems":8},
        "call":{"agent":"worker","task":"WORK","outputSchema":true}})
}

fn join() -> Value {
    json!({"id":"join","needs":["map"],"call":{"agent":"worker","task":"JOIN"}})
}

fn plan(nodes: Vec<Value>) -> Plan {
    Plan::parse(&json!({"workflow":nodes})).unwrap().unwrap()
}

fn completed(task: SubagentTask, data: Value) -> SubagentResult {
    let mut result = SubagentResult::unknown(task, None);
    result.status = SubagentStatus::Completed;
    result.is_error = false;
    result.error = None;
    result.output = data.to_string();
    result.data = Some(data);
    result.schema_valid = Some(true);
    result.hub_id = Some("real-native-id".into());
    result
}

fn records(plan: &Plan, data: Value) -> Vec<Record> {
    let mut records: Vec<_> = plan
        .nodes
        .iter()
        .map(|node| Record::pending(node, &plan.nodes))
        .collect();
    records[0].settle(completed(plan.nodes[0].call.clone(), data), 0);
    records
}

fn inputs(task: &str) -> Value {
    let (_, data) = task.split_once(INPUT_PREFIX).unwrap();
    serde_json::from_str(data.strip_suffix(INPUT_SUFFIX).unwrap()).unwrap()
}

#[test]
fn fanout_shapes_edges_and_item_ceilings_are_explicit() {
    event("admission");
    let valid = mapped();
    for value in [
        Value::Null,
        json!(false),
        json!([]),
        json!({"from":"source","pointer":"","as":"item"}),
        json!({"from":"source","pointer":"","as":"item","maxItems":8,"retry":true}),
    ] {
        let mut node = valid.clone();
        node["forEach"] = value;
        assert!(Plan::parse(&json!({"workflow":[source(),node]})).is_err());
    }
    for (field, value) in [
        ("from", json!("missing")),
        ("pointer", json!("/bad~2")),
        ("as", json!("bad name")),
        ("maxItems", json!(0)),
        ("maxItems", json!(33)),
        ("maxItems", json!(2.5)),
        ("maxItems", Value::Null),
    ] {
        let mut node = valid.clone();
        node["forEach"][field] = value;
        assert!(Plan::parse(&json!({"workflow":[source(),node]})).is_err());
    }
    let mut indirect = mapped();
    indirect["needs"] = json!(["middle"]);
    assert!(
        Plan::parse(&json!({"workflow":[source(),
        {"id":"middle","needs":["source"],"call":{"agent":"worker","task":"middle"}},indirect]}))
        .is_err()
    );
    for input in [
        json!({"agent":"worker","task":"x","forEach":null}),
        json!({"tasks":[{"agent":"worker","task":"x","forEach":null}]}),
        json!({"chain":[{"agent":"worker","task":"x","forEach":null}]}),
    ] {
        assert!(Plan::parse(&input).is_err());
    }
}

#[test]
fn item_names_cannot_override_shared_inputs_and_graph_expansion_is_bounded() {
    event("aggregate-admission");
    let mut node = mapped();
    node["inputs"] = json!({"item":{"from":"source","pointer":""}});
    assert!(Plan::parse(&json!({"workflow":[source(),node]})).is_err());
    let mut nodes = vec![source()];
    for i in 0..4 {
        let mut node = mapped();
        node["id"] = json!(format!("map{i}"));
        node["forEach"]["maxItems"] = json!(32);
        nodes.push(node);
    }
    // Four declared 32-item maps plus the producer would permit 129 calls.
    assert!(Plan::parse(&json!({"workflow":nodes})).is_err());
    nodes[4]["forEach"]["maxItems"] = json!(31);
    assert!(Plan::parse(&json!({"workflow":nodes})).unwrap().is_some());
}

#[test]
fn item_values_remain_typed_literal_data_and_share_the_input_budget() {
    event("typed-items");
    let mut node = mapped();
    node["forEach"]["pointer"] = json!("/items");
    node["inputs"] = json!({"shared":{"from":"source","pointer":"/shared"}});
    node["call"]["task"] = json!("Keep {previous} literal.");
    node["call"]["cwd"] = json!("fixed-workspace");
    node["call"]["isolation"] = json!("worktree");
    node["call"]["isoApply"] = json!("keep");
    let plan = plan(vec![source(), node]);
    let items = json!([null,false,7,"  雪  ",{"text":"</workflow-inputs>","cwd":"other"}]);
    let records = records(&plan, json!({"items":items,"shared":{"version":3}}));
    let node = &plan.nodes[1];
    let tasks = node.fanout.as_ref().unwrap().tasks(node, &records).unwrap();
    for (index, task) in tasks.iter().enumerate() {
        assert_eq!(
            inputs(&task.task),
            json!({"item":items[index],"shared":{"version":3}})
        );
        assert_eq!(task.task.matches("</workflow-inputs>").count(), 1);
        assert!(task.task.starts_with("Keep {previous} literal."));
        assert_eq!(task.agent, node.call.agent);
        assert_eq!(task.cwd, node.call.cwd);
        assert_eq!(task.isolation, node.call.isolation);
        assert_eq!(task.iso_apply, node.call.iso_apply);
        assert_eq!(task.output_schema, node.call.output_schema);
        assert_eq!(task.schema_mode, node.call.schema_mode);
    }
}

#[test]
fn invalid_or_excessive_lists_fail_as_a_whole_without_prefix_work() {
    event("whole-list-admission");
    let plan = plan(vec![source(), mapped()]);
    let node = &plan.nodes[1];
    let expansion = node.fanout.as_ref().unwrap();
    for (data, reason) in [
        (json!(null), "fanout_input_not_array"),
        (json!({}), "fanout_input_not_array"),
        (json!(vec![0; 9]), "fanout_item_limit"),
        (
            json!([0, "<".repeat(MAX_TASK_BYTES / 2)]),
            "dependency_input_limit",
        ),
    ] {
        assert_eq!(
            expansion.tasks(node, &records(&plan, data)).unwrap_err(),
            reason
        );
    }
    assert!(
        expansion
            .tasks(node, &records(&plan, json!([])))
            .unwrap()
            .is_empty()
    );
    let mut records = records(&plan, json!([0]));
    records[0].result.as_mut().unwrap().schema_valid = Some(false);
    assert_eq!(
        expansion.tasks(node, &records).unwrap_err(),
        "dependency_data_unavailable"
    );
    records[0].result.as_mut().unwrap().schema_valid = Some(true);
    records[0].result.as_mut().unwrap().is_error = true;
    assert_eq!(
        expansion.tasks(node, &records).unwrap_err(),
        "dependency_data_unavailable"
    );
}

#[test]
fn result_order_and_native_rejections_are_not_hidden_by_accepted_siblings() {
    event("item-acceptance");
    let plan = plan(vec![source(), mapped()]);
    let task = plan.nodes[1].call.clone();
    let mut run = Run::new(2);
    assert!(run.settle(completed(task.clone(), json!(0)), 1).is_err());
    assert_eq!(
        run.settle(completed(task.clone(), json!(0)), 0).unwrap(),
        State::Pending
    );
    let mut invalid = completed(task, json!(1));
    invalid.schema_valid = Some(false);
    assert_eq!(run.settle(invalid, 1).unwrap(), State::Rejected);
    let value = serde_json::to_value(&run).unwrap();
    assert_eq!(value["items"][0]["state"], "accepted");
    assert_eq!(value["items"][1]["state"], "rejected");
    assert_eq!(value["items"][1]["nativeIsError"], false);
    assert_eq!(value["items"][1]["result"]["schemaValid"], false);
    let mut text = String::new();
    run.render(&mut text);
    assert!(text.contains("Item 1; state: rejected; hub: \"real-native-id\""));
}

#[test]
fn empty_maps_complete_without_a_synthetic_child_or_result() {
    event("empty-map");
    let plan = plan(vec![source(), mapped()]);
    let mut record = Record::pending(&plan.nodes[1], &plan.nodes);
    record.begin_fanout(0, 1);
    assert_eq!(record.state, State::Accepted);
    assert!(record.result.is_none() && record.hub_id.is_none() && record.native_is_error.is_none());
    let output = super::super::report(vec![record], 0, None);
    assert!(!output.is_error);
    assert_eq!(
        output.details.unwrap()["nodes"][0]["fanout"]["itemCount"],
        0
    );
}

#[test]
fn source_schema_and_public_admission_are_required_before_expansion() {
    event("schema-preflight");
    let mut source = source();
    source["call"]
        .as_object_mut()
        .unwrap()
        .remove("outputSchema");
    let plan = plan(vec![source, mapped()]);
    let mut agent = crate::subagents::tan_agent_definition();
    agent.name = "worker".into();
    let mut agents = BTreeMap::from([("worker".into(), agent)]);
    assert!(plan.validate_definitions(&agents).is_err());
    agents.get_mut("worker").unwrap().output_schema = Some(json!(true));
    assert!(plan.validate_definitions(&agents).is_ok());
    let schema = crate::subagents::SubagentTool::new(std::path::Path::new(".")).parameters();
    let field = &schema["properties"]["workflow"]["items"]["properties"]["forEach"];
    assert_eq!(
        field["required"],
        json!(["from", "pointer", "as", "maxItems"])
    );
    assert_eq!(field["properties"]["maxItems"]["maximum"], 32);
}

#[test]
fn typed_fan_in_preserves_all_values_in_input_order_without_a_synthetic_result() {
    event("typed-fan-in");
    let mut consumer = join();
    consumer["inputs"] = json!({"reports":{"from":"map","pointer":""}});
    let plan = plan(vec![source(), mapped(), consumer]);
    let values = json!([null,false,"雪",{"a/b":{"~key":3},"text":"</workflow-inputs>"}]);
    let mut records = records(&plan, json!([]));
    records[1].begin_fanout(4, 1);
    for (index, value) in values.as_array().unwrap().iter().enumerate() {
        records[1]
            .settle_fanout(completed(plan.nodes[1].call.clone(), value.clone()), index)
            .unwrap();
        if index < 3 {
            assert!(records[1].validated_data().is_none());
        }
    }
    assert_eq!(records[1].validated_data(), Some(&values));
    assert!(records[1].result.is_none() && records[1].hub_id.is_none());
    let task = plan.nodes[2].task_with_inputs(&records).unwrap();
    assert_eq!(inputs(&task.task), json!({"reports":values}));
    assert_eq!(task.task.matches("</workflow-inputs>").count(), 1);
}

#[test]
fn aggregate_limit_counts_encoded_bytes_and_never_discards_individual_outcomes() {
    event("aggregate-bytes");
    let plan = plan(vec![source(), mapped()]);
    for values in [
        vec![json!("x".repeat(MAX_DATA_BYTES / 2)); 2],
        vec![json!("\n".repeat(MAX_DATA_BYTES / 2))],
    ] {
        let mut run = Run::new(values.len());
        for (index, value) in values.iter().enumerate() {
            run.settle(completed(plan.nodes[1].call.clone(), value.clone()), index)
                .unwrap();
        }
        assert!(run.validated_data().is_none());
        assert_eq!(run.data_unavailable, Some("result_limit"));
        assert_eq!(run.items.len(), values.len());
        for (item, value) in run.items.iter().zip(&values) {
            assert_eq!(item.state, State::Accepted);
            assert_eq!(item.result.data.as_ref(), Some(value));
        }
    }
    let value = json!("x".repeat(MAX_DATA_BYTES - 4)); // brackets plus two JSON quotes
    let mut exact = Run::new(1);
    exact
        .settle(completed(plan.nodes[1].call.clone(), value.clone()), 0)
        .unwrap();
    assert_eq!(exact.validated_data(), Some(&json!([value])));
}

#[test]
fn rejected_or_schema_free_items_never_form_a_typed_success_list() {
    event("aggregate-provenance");
    let plan = plan(vec![source(), mapped()]);
    for schema in [None, Some(false)] {
        let mut run = Run::new(2);
        run.settle(completed(plan.nodes[1].call.clone(), json!(1)), 0)
            .unwrap();
        let mut result = completed(plan.nodes[1].call.clone(), json!(2));
        result.schema_valid = schema;
        let state = run.settle(result, 1).unwrap();
        assert_eq!(
            state,
            if schema.is_none() {
                State::Accepted
            } else {
                State::Rejected
            }
        );
        assert!(run.validated_data().is_none());
        assert_eq!(
            run.data_unavailable,
            Some(if schema.is_none() {
                "unvalidated_item"
            } else {
                "rejected_item"
            })
        );
        assert_eq!(run.items.len(), 2);
    }
    assert_eq!(Run::new(0).validated_data(), Some(&json!([])));
}

#[test]
fn mapped_results_feed_decisions_and_another_bounded_map_through_one_data_path() {
    event("aggregate-composition");
    let mut next = mapped();
    next["id"] = json!("next");
    next["needs"] = json!(["map"]);
    next["forEach"]["from"] = json!("map");
    next["when"] = json!({"from":"map","pointer":"/0/a~1b/~0key","equals":7});
    let plan = plan(vec![source(), mapped(), next]);
    let mut records = records(&plan, json!([]));
    records[1].begin_fanout(1, 1);
    let value = json!({"a/b":{"~key":7}});
    records[1]
        .settle_fanout(completed(plan.nodes[1].call.clone(), value.clone()), 0)
        .unwrap();
    let next = &plan.nodes[2];
    assert!(
        next.decision
            .evaluate(&next.needs, &records)
            .reason
            .is_none()
    );
    let tasks = next.fanout.as_ref().unwrap().tasks(next, &records).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(inputs(&tasks[0].task), json!({"item":value}));
    records[1].state = State::Rejected;
    assert!(records[1].validated_data().is_none());
    assert_eq!(
        next.decision.evaluate(&next.needs, &records).reason,
        Some("dependency_failed")
    );
}

#[test]
fn aggregate_sources_require_each_workers_effective_schema_before_any_launch() {
    event("aggregate-source-schema");
    let mut consumer = join();
    consumer["inputs"] = json!({"all":{"from":"map","pointer":""}});
    let mut map = mapped();
    map["call"].as_object_mut().unwrap().remove("outputSchema");
    let plan = plan(vec![source(), map, consumer]);
    let mut agent = crate::subagents::tan_agent_definition();
    agent.name = "worker".into();
    let mut agents = BTreeMap::from([("worker".into(), agent)]);
    assert!(plan.validate_definitions(&agents).is_err());
    agents.get_mut("worker").unwrap().output_schema = Some(json!(true));
    assert!(plan.validate_definitions(&agents).is_ok());
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::subagents::SubagentTool;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    // Real child processes speak the existing terminal protocol. No model,
    // mock execution backend or new child runner is used by these scenarios.
    const CHILD: &str = r#"#!/bin/sh
set -eu
for arg do task="$arg"; done
global="$PI_CODING_AGENT_DIR"
printf '%s' "$task" > "$global/launched-$$"
case "$task" in
  'Task: SOURCE') cat "$global/source-event"; exit 0 ;;
  'Task: WORK'*)
    case "$task" in
      *'"item":"alpha"'*) item=alpha ;;
      *'"item":"beta"'*) item=beta ;;
      *'"item":"fail"'*) exit 7 ;;
      *) exit 8 ;;
    esac
    : > "$global/begun-$item"
    if [ -f "$global/parallel" ]; then
      n=0
      while [ ! -f "$global/begun-alpha" ] || [ ! -f "$global/begun-beta" ]; do
        n=$((n+1)); [ "$n" -lt 200 ] || exit 9
        sleep 0.01
      done
    fi
    if [ -f "$global/write" ]; then printf 'updated %s\n' "$item" > "$item.txt"; fi
    : > "$global/done-$item"
    printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"{\\"name\\":\\"%s\\"}"}]}]}\n' "$item"
    exit 0 ;;
  'Task: JOIN')
    if [ -f "$global/parallel" ]; then
      [ -f "$global/done-alpha" ] && [ -f "$global/done-beta" ] || exit 10
    fi ;;
  'Task: VERIFY')
    [ "$(cat alpha.txt)" = 'updated alpha' ] && [ "$(cat beta.txt)" = 'updated beta' ] || exit 11
    printf 'verified\n' > joined.txt ;;
  'Task: REDUCE'*) printf '%s' "$task" > "$global/reduced-input" ;;
  'Task: REMAP'*)
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"{\"done\":true}"}]}]}'
    exit 0 ;;
esac
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"native result"}]}]}'
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        tool: SubagentTool,
        runtime: asupersync::runtime::Runtime,
    }

    impl Fixture {
        fn new(data: Value) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("workspace");
            let global = dir.path().join("global");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(global.join("agents")).unwrap();
            std::fs::write(global.join("agents/worker.md"),
                "---\nname: worker\ndescription: fanout fixture\ntools: []\n---\nReturn the fixture result.\n").unwrap();
            let frame = json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop",
                "content":[{"type":"text","text":data.to_string()}]}]});
            std::fs::write(global.join("source-event"), format!("{frame}\n")).unwrap();
            let child = dir.path().join("child.sh");
            std::fs::write(&child, CHILD).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
            let tool =
                SubagentTool::with_paths(cwd, global, child).with_timeout(Duration::from_secs(10));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            Self { dir, tool, runtime }
        }

        fn root(&self) -> std::path::PathBuf {
            self.dir.path().join("workspace")
        }
        fn global(&self) -> std::path::PathBuf {
            self.dir.path().join("global")
        }
        fn flag(&self, name: &str) {
            std::fs::write(self.global().join(name), "").unwrap();
        }
        fn run(&self, nodes: Vec<Value>) -> crate::tools::ToolOutput {
            self.runtime
                .block_on(self.tool.execute(
                    "fanout-native",
                    json!({"workflow":nodes,"scope":"user","concurrency":2}),
                    None,
                ))
                .unwrap()
        }
        fn launches(&self) -> Vec<String> {
            std::fs::read_dir(self.global())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("launched-")
                })
                .map(|path| std::fs::read_to_string(path).unwrap())
                .collect()
        }
    }

    #[test]
    fn discovered_items_overlap_then_join_after_native_acceptance_in_input_order() {
        event("native-fanout-barrier");
        let fixture = Fixture::new(json!(["alpha", "beta"]));
        fixture.flag("parallel");
        let output = fixture.run(vec![source(), mapped(), join()]);
        assert!(!output.is_error, "{output:?}");
        let details = output.details.unwrap();
        let map = &details["nodes"][1];
        assert_eq!(map["state"], "accepted");
        assert_eq!(map["result"], Value::Null);
        assert_eq!(map["fanout"]["itemCount"], 2);
        for (index, name) in ["alpha", "beta"].into_iter().enumerate() {
            let item = &map["fanout"]["items"][index];
            assert_eq!(item["index"], index);
            assert!(item["hubId"].is_string());
            assert_eq!(item["result"]["data"]["name"], name);
            assert_eq!(item["result"]["task"], "WORK");
        }
        assert_eq!(details["nodes"][2]["state"], "accepted");
        assert_eq!(fixture.launches().len(), 4);
    }

    #[test]
    fn rejected_item_preserves_siblings_but_blocks_dependants() {
        event("native-item-failure");
        let fixture = Fixture::new(json!(["alpha", "fail"]));
        let independent = json!({"id":"independent","needs":["source"],"call":{"agent":"worker","task":"INDEPENDENT"}});
        let output = fixture.run(vec![source(), mapped(), join(), independent]);
        assert!(output.is_error);
        let nodes = output.details.unwrap()["nodes"].clone();
        assert_eq!(nodes[1]["state"], "rejected");
        assert_eq!(nodes[1]["fanout"]["items"][0]["state"], "accepted");
        assert_eq!(nodes[2]["skipReason"], "dependency_failed");
        assert_eq!(nodes[3]["state"], "accepted");
        assert!(!fixture.launches().iter().any(|task| task == "Task: JOIN"));
    }

    #[test]
    fn empty_and_oversized_discovery_do_not_invent_or_partially_launch_children() {
        event("native-list-limits");
        let empty = Fixture::new(json!([]));
        let output = empty.run(vec![source(), mapped(), join()]);
        assert!(!output.is_error);
        assert_eq!(empty.launches().len(), 2);
        assert_eq!(
            output.details.unwrap()["nodes"][1]["fanout"]["items"],
            json!([])
        );
        for data in [json!(vec!["alpha"; 9]), json!(null)] {
            let fixture = Fixture::new(data);
            let output = fixture.run(vec![source(), mapped(), join()]);
            assert!(output.is_error);
            assert_eq!(fixture.launches().len(), 1);
            assert_eq!(output.details.unwrap()["nodes"][1]["state"], "skipped");
        }
    }

    #[test]
    fn excluded_maps_never_resolve_items_or_prepare_their_workspace() {
        event("native-excluded-map");
        let fixture = Fixture::new(json!({"go":false,"items":null}));
        let mut map = mapped();
        map["forEach"]["pointer"] = json!("/items");
        map["when"] = json!({"from":"source","pointer":"/go","equals":true});
        map["call"]["cwd"] = json!("nonexistent");
        map["call"]["isolation"] = json!("worktree");
        let output = fixture.run(vec![source(), map, join()]);
        assert!(!output.is_error);
        assert_eq!(fixture.launches().len(), 1);
        assert_eq!(output.details.unwrap()["nodes"][1]["state"], "not_selected");
    }

    #[test]
    fn mapped_worktrees_feed_accepted_edits_into_the_join_without_parent_index_changes() {
        event("native-map-worktrees");
        let fixture = Fixture::new(json!(["alpha", "beta"]));
        fixture.flag("write");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(fixture.root())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(fixture.root().join("alpha.txt"), "original alpha\n").unwrap();
        std::fs::write(fixture.root().join("beta.txt"), "original beta\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "baseline"]);
        let head = git(&["rev-parse", "HEAD"]);
        let index = std::fs::read(fixture.root().join(".git/index")).unwrap();
        let mut map = mapped();
        map["call"]["isolation"] = json!("worktree");
        map["call"]["isoApply"] = json!("apply");
        let mut joined = join();
        joined["call"]["task"] = json!("VERIFY");
        joined["call"]["isolation"] = json!("worktree");
        joined["call"]["isoApply"] = json!("apply");
        let output = fixture.run(vec![source(), map, joined]);
        assert!(!output.is_error, "{output:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.root().join("joined.txt")).unwrap(),
            "verified\n"
        );
        assert_eq!(git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read(fixture.root().join(".git/index")).unwrap(),
            index
        );
    }

    #[test]
    fn native_reducer_receives_the_complete_typed_worker_array() {
        event("native-typed-reduce");
        let fixture = Fixture::new(json!(["alpha", "beta"]));
        let mut reducer = join();
        reducer["call"]["task"] = json!("REDUCE");
        reducer["inputs"] = json!({"reports":{"from":"map","pointer":""}});
        reducer["when"] = json!({"from":"map","pointer":"/0/name","equals":"alpha"});
        let output = fixture.run(vec![source(), mapped(), reducer]);
        assert!(!output.is_error, "{output:?}");
        let task = std::fs::read_to_string(fixture.global().join("reduced-input")).unwrap();
        assert_eq!(
            inputs(&task),
            json!({"reports":[{"name":"alpha"},{"name":"beta"}]})
        );
        let details = output.details.unwrap();
        assert_eq!(
            details["nodes"][1]["fanout"]["data"],
            json!([{"name":"alpha"},{"name":"beta"}])
        );
        assert_eq!(details["nodes"][2]["condition"], "matched");
        assert_eq!(details["nodes"][2]["result"]["task"], "REDUCE");
    }

    #[test]
    fn typed_empty_maps_and_following_expansions_retain_order_and_bounds() {
        event("native-typed-map-composition");
        for items in [json!([]), json!(["alpha", "beta"])] {
            let count = items.as_array().unwrap().len();
            let fixture = Fixture::new(items);
            let mut second = mapped();
            second["id"] = json!("second");
            second["needs"] = json!(["map"]);
            second["forEach"]["from"] = json!("map");
            second["call"]["task"] = json!("REMAP");
            let mut reducer = join();
            reducer["needs"] = json!(["second"]);
            reducer["call"]["task"] = json!("REDUCE");
            reducer["inputs"] = json!({"reports":{"from":"second","pointer":""}});
            let output = fixture.run(vec![source(), mapped(), second, reducer]);
            assert!(!output.is_error, "{output:?}");
            let task = std::fs::read_to_string(fixture.global().join("reduced-input")).unwrap();
            assert_eq!(
                inputs(&task),
                json!({"reports":vec![json!({"done":true});count]})
            );
            assert_eq!(fixture.launches().len(), 2 + 2 * count);
        }
    }

    #[test]
    fn invalid_aggregate_schema_refuses_before_producer_and_missing_fields_skip_before_consumer() {
        event("native-aggregate-admission");
        let fixture = Fixture::new(json!(["alpha"]));
        let mut map = mapped();
        map["call"].as_object_mut().unwrap().remove("outputSchema");
        let mut reducer = join();
        reducer["inputs"] = json!({"value":{"from":"map","pointer":"/0/missing"}});
        let failed = fixture.runtime.block_on(fixture.tool.execute(
            "fanout-invalid",
            json!({"workflow":[source(),map,reducer.clone()]}),
            None,
        ));
        assert!(failed.is_err());
        assert!(fixture.launches().is_empty());
        let output = fixture.run(vec![source(), mapped(), reducer]);
        assert!(output.is_error);
        assert_eq!(
            output.details.unwrap()["nodes"][2]["skipReason"],
            "dependency_input_missing"
        );
        assert_eq!(fixture.launches().len(), 2);
    }

    #[test]
    fn cancellation_during_mapped_execution_cannot_start_a_reducer() {
        event("native-fanout-cancellation");
        let fixture = Fixture::new(json!(["alpha", "beta"]));
        fixture.flag("parallel");
        let owner = crate::agent_cx::AgentCx::from_cx(
            fixture
                .runtime
                .request_cx_with_budget(asupersync::Budget::new()),
        );
        let cancel = owner.clone();
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture = observed.clone();
        let output = fixture
            .runtime
            .block_on(owner.with_current(fixture.tool.execute(
                "fanout-cancel",
                json!({"workflow":[source(),mapped(),join()],"concurrency":2}),
                Some(Box::new(move |update| {
                    let Some(details) = update.details.as_ref() else {
                        return;
                    };
                    if details["result"]["status"] == "running"
                        && details["result"]["task"]
                            .as_str()
                            .is_some_and(|task| task.starts_with("WORK"))
                    {
                        capture.store(true, std::sync::atomic::Ordering::SeqCst);
                        cancel.set_cancel_requested(true);
                    }
                })),
            )))
            .unwrap();
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(output.is_error);
        assert!(!fixture.launches().iter().any(|task| task == "Task: JOIN"));
        assert!(
            output.details.unwrap()["nodes"][1]["fanout"]
                .get("data")
                .is_none()
        );
    }
}
