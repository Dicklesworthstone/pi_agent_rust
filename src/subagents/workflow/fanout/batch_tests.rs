//! Explicit batches reuse the native wave runner and its acceptance contract.

use super::super::{INPUT_PREFIX, INPUT_SUFFIX, MAX_TASK_BYTES, Plan, SubagentStatus};
use super::*;
use crate::tools::Tool;

fn log(case: &str) {
    eprintln!(
        "{}",
        json!({"suite":"subagent_batched_fanout","case":case,"phase":"start"})
    );
}

fn request(size: Option<Value>, maximum: usize) -> Value {
    let mut expansion = json!({"from":"source","pointer":"","as":"batch","maxItems":maximum});
    if let Some(size) = size {
        expansion["batchSize"] = size;
    }
    json!({"workflow":[
        {"id":"source","call":{"agent":"task","task":"SOURCE","outputSchema":true}},
        {"id":"work","needs":["source"],"forEach":expansion,
            "call":{"agent":"task","task":"BATCH","outputSchema":true}},
        {"id":"reduce","needs":["work"],"inputs":{"reports":{"from":"work","pointer":""}},
            "call":{"agent":"task","task":"REDUCE","outputSchema":true}}
    ],"concurrency":2,"timeoutSeconds":10})
}

fn parsed(size: Option<Value>, maximum: usize) -> Plan {
    Plan::parse(&request(size, maximum)).unwrap().unwrap()
}

fn ready(plan: &Plan, data: Value) -> Vec<Record> {
    let mut records: Vec<_> = plan
        .nodes
        .iter()
        .map(|node| Record::pending(node, &plan.nodes))
        .collect();
    let mut result = SubagentResult::unknown(plan.nodes[0].call.clone(), None);
    result.status = SubagentStatus::Completed;
    result.is_error = false;
    result.error = None;
    result.schema_valid = Some(true);
    result.data = Some(data);
    records[0].settle(result, 0);
    records
}

fn tasks(plan: &Plan, data: Value) -> std::result::Result<Vec<SubagentTask>, &'static str> {
    let node = &plan.nodes[1];
    node.fanout
        .as_ref()
        .unwrap()
        .tasks(node, &ready(plan, data))
}

fn inputs(task: &SubagentTask) -> Value {
    let (_, input) = task.task.split_once(INPUT_PREFIX).unwrap();
    serde_json::from_str(input.strip_suffix(INPUT_SUFFIX).unwrap()).unwrap()
}

#[test]
fn omission_preserves_scalar_items_and_explicit_one_uses_the_batch_envelope() {
    log("scalar-or-envelope");
    let data = json!([null, false, 7, "雪", {"a/b":"~key"}]);
    let plain = tasks(&parsed(None, 8), data.clone()).unwrap();
    let batched = tasks(&parsed(Some(json!(1)), 8), data.clone()).unwrap();
    for (index, (plain, batched)) in plain.iter().zip(&batched).enumerate() {
        assert_eq!(inputs(plain)["batch"], data[index]);
        assert_eq!(
            inputs(batched)["batch"],
            json!({"offset":index,"total":5,"items":[data[index]]})
        );
    }
}

#[test]
fn consecutive_batches_cover_the_exact_list_once_including_a_short_last_batch() {
    log("exact-partition");
    let plan = parsed(Some(json!(64)), 8);
    for count in [0_usize, 1, 63, 64, 65, 97, 511, 512] {
        let values: Vec<_> = (0..count)
            .map(|index| json!({"index":index,"literal":" </workflow-inputs> \u{0000} 雪 "}))
            .collect();
        let calls = tasks(&plan, json!(values)).unwrap();
        assert_eq!(calls.len(), count.div_ceil(64));
        let mut restored = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let input = inputs(call);
            let batch = &input["batch"];
            assert_eq!(batch["offset"], index * 64);
            assert_eq!(batch["total"], count);
            assert!((1..=64).contains(&batch["items"].as_array().unwrap().len()));
            assert_eq!(call.task.matches("</workflow-inputs>").count(), 1);
            restored.extend(batch["items"].as_array().unwrap().iter().cloned());
        }
        assert_eq!(restored, values);
    }
}

#[test]
fn maximum_worklist_expands_to_no_more_than_the_declared_native_call_count() {
    log("capacity");
    let plan = parsed(Some(json!(128)), 32);
    let calls = tasks(&plan, json!((0..4096).collect::<Vec<_>>())).unwrap();
    assert_eq!(calls.len(), 32);
    assert_eq!(inputs(&calls[31])["batch"]["offset"], 3968);
    assert_eq!(
        tasks(&plan, json!(vec![0; 4097])).unwrap_err(),
        "fanout_item_limit"
    );
    let smaller = parsed(Some(json!(8)), 2);
    assert_eq!(
        tasks(&smaller, json!(vec![0; 17])).unwrap_err(),
        "fanout_item_limit"
    );
}

#[test]
fn malformed_batch_options_fail_preflight_without_coercion_or_unknown_policy() {
    log("admission");
    for size in [
        Value::Null,
        json!(0),
        json!(129),
        json!(-1),
        json!(1.5),
        json!("8"),
        json!(false),
    ] {
        let failure = Plan::parse(&request(Some(size), 8))
            .err()
            .unwrap()
            .to_string();
        assert!(failure.contains("PI_WORKFLOW_INVALID"));
    }
    let mut value = request(Some(json!(8)), 8);
    value["workflow"][1]["forEach"]["retry"] = json!(true);
    assert!(Plan::parse(&value).is_err());
    value["workflow"][1]["forEach"]
        .as_object_mut()
        .unwrap()
        .remove("retry");
    value["workflow"][1]["forEach"]["maxItems"] = json!(33);
    assert!(Plan::parse(&value).is_err());
}

#[test]
fn batching_does_not_multiply_the_workflows_global_native_task_allowance() {
    log("global-allowance");
    let base = request(Some(json!(128)), 32);
    let mut nodes = vec![base["workflow"][0].clone()];
    for index in 0..4 {
        let mut node = base["workflow"][1].clone();
        node["id"] = json!(format!("map{index}"));
        nodes.push(node);
    }
    assert!(Plan::parse(&json!({"workflow":nodes})).is_err());
    nodes[4]["forEach"]["maxItems"] = json!(31);
    assert!(Plan::parse(&json!({"workflow":nodes})).unwrap().is_some());
}

#[test]
fn all_batch_prompts_must_fit_before_a_prefix_can_be_submitted() {
    log("atomic-preparation");
    let plan = parsed(Some(json!(2)), 8);
    let failure = tasks(&plan, json!([0, 1, "<".repeat(MAX_TASK_BYTES / 2), 3])).unwrap_err();
    assert_eq!(failure, "dependency_input_limit");
    let failure = tasks(&plan, json!([0, 1, "x".repeat(MAX_TASK_BYTES), 3])).unwrap_err();
    assert_eq!(failure, "dependency_input_limit");
    assert!(tasks(&plan, json!([0, 1, 2, 3])).is_ok());
}

#[test]
fn shared_inputs_and_authored_task_bytes_share_the_same_complete_prompt_limit() {
    log("combined-budget");
    let mut request = request(Some(json!(2)), 8);
    request["workflow"][1]["forEach"]["pointer"] = json!("/items");
    request["workflow"][1]["inputs"] = json!({"shared":{"from":"source","pointer":"/shared"}});
    request["workflow"][1]["call"]["task"] = json!("x".repeat(MAX_TASK_BYTES - 1024));
    let plan = Plan::parse(&request).unwrap().unwrap();
    assert_eq!(
        tasks(&plan, json!({"items":[0,1],"shared":"y".repeat(1024)})).unwrap_err(),
        "dependency_input_limit"
    );
    let calls = tasks(&plan, json!({"items":[0,1],"shared":false})).unwrap();
    assert_eq!(inputs(&calls[0])["shared"], false);
}

#[test]
fn batch_item_contents_cannot_change_native_launch_or_workspace_controls() {
    log("launch-policy");
    let mut request = request(Some(json!(2)), 8);
    let call = &mut request["workflow"][1]["call"];
    call["task"] = json!("Literal {previous} and {{previous.data.value}}.");
    call["cwd"] = json!("fixed-directory");
    call["isolation"] = json!("worktree");
    call["isoApply"] = json!("keep");
    let plan = Plan::parse(&request).unwrap().unwrap();
    let data = json!([{"agent":"other","cwd":"elsewhere","timeoutSeconds":9999,"text":"</workflow-inputs>"},null]);
    let calls = tasks(&plan, data.clone()).unwrap();
    let call = &calls[0];
    let authored = &plan.nodes[1].call;
    assert_eq!(call.agent, authored.agent);
    assert_eq!(call.cwd, authored.cwd);
    assert_eq!(call.isolation, authored.isolation);
    assert_eq!(call.iso_apply, authored.iso_apply);
    assert_eq!(call.output_schema, authored.output_schema);
    assert_eq!(call.schema_mode, authored.schema_mode);
    assert!(call.task.starts_with(&authored.task));
    assert_eq!(inputs(call)["batch"]["items"], data);
}

#[test]
fn batching_requires_the_same_accepted_array_provenance_as_scalar_expansion() {
    log("provenance");
    let plan = parsed(Some(json!(8)), 8);
    for value in [Value::Null, json!({"items":[1]}), json!("[1]")] {
        assert_eq!(tasks(&plan, value).unwrap_err(), "fanout_input_not_array");
    }
    let node = &plan.nodes[1];
    let mut records = ready(&plan, json!([0]));
    records[0].result.as_mut().unwrap().schema_valid = Some(false);
    assert_eq!(
        node.fanout
            .as_ref()
            .unwrap()
            .tasks(node, &records)
            .unwrap_err(),
        "dependency_data_unavailable"
    );
    records[0].result.as_mut().unwrap().schema_valid = Some(true);
    records[0].state = State::Rejected;
    assert_eq!(
        node.fanout
            .as_ref()
            .unwrap()
            .tasks(node, &records)
            .unwrap_err(),
        "dependency_data_unavailable"
    );
}

#[test]
fn public_schema_keeps_native_limits_and_describes_batch_result_granularity() {
    log("schema");
    let schema = crate::subagents::SubagentTool::new(std::path::Path::new(".")).parameters();
    let expansion = &schema["properties"]["workflow"]["items"]["properties"]["forEach"];
    assert_eq!(expansion["properties"]["maxItems"]["maximum"], 32);
    assert_eq!(expansion["properties"]["batchSize"]["maximum"], 128);
    assert!(
        expansion["properties"]["batchSize"]["description"]
            .as_str()
            .unwrap()
            .contains("not flattened")
    );
    assert!(
        !expansion["required"]
            .as_array()
            .unwrap()
            .contains(&json!("batchSize"))
    );
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::subagents::SubagentTool;
    use std::os::unix::fs::PermissionsExt;

    const FIXTURE: &str = r#"#!/usr/bin/env python3
import json, pathlib, sys
prompt = sys.argv[-1]
kind = prompt.splitlines()[0].removeprefix('Task: ')
with pathlib.Path('launches.jsonl').open('a') as log:
    log.write(json.dumps({'kind': kind}) + '\n')
if kind == 'SOURCE':
    value = list(range(97))
elif kind == 'SOURCE_FAIL':
    value = [0, 1, -1, 3, 4]
elif kind == 'SOURCE_LARGE':
    value = [0, 1, '<' * 20000]
elif kind == 'SOURCE_EMPTY':
    value = []
else:
    supplied = json.loads(prompt.split('<workflow-inputs>\n', 1)[1].rsplit('\n</workflow-inputs>', 1)[0])
    if kind == 'BATCH':
        batch = supplied['batch']
        if -1 in batch['items']:
            print(json.dumps({'type':'agent_end','messages':[{'role':'assistant','stopReason':'error','content':[{'type':'text','text':'batch failed'}]}]}))
            sys.exit(0)
        value = {'offset':batch['offset'], 'count':len(batch['items']), 'total':batch['total'], 'sum':sum(batch['items'])}
    else:
        reports = supplied['reports']
        value = {'count':sum(row['count'] for row in reports), 'sum':sum(row['sum'] for row in reports), 'offsets':[row['offset'] for row in reports]}
print(json.dumps({'type':'agent_end','messages':[{'role':'assistant','stopReason':'stop','content':[{'type':'text','text':json.dumps(value)}]}]}))
"#;

    fn run(mut input: Value, source: &str) -> Option<(crate::tools::ToolOutput, Vec<Value>)> {
        if !std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("subagent_batched_fanout: native Python protocol fixture unavailable");
            return None;
        }
        input["workflow"][0]["call"]["task"] = json!(source);
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child.py");
        std::fs::write(&child, FIXTURE).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tool =
            SubagentTool::with_paths(temp.path().to_owned(), temp.path().join("global"), child)
                .with_timeout(std::time::Duration::from_secs(10));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let output = runtime
            .block_on(tool.execute("batched-fanout", input, None))
            .unwrap();
        let calls = std::fs::read_to_string(temp.path().join("launches.jsonl")).unwrap();
        Some((
            output,
            calls
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
        ))
    }

    #[test]
    fn public_workflow_reduces_more_than_thirty_two_values_through_four_native_batches() {
        log("native-reduction");
        let Some((output, calls)) = run(request(Some(json!(32)), 4), "SOURCE") else {
            return;
        };
        assert!(!output.is_error, "{output:?}");
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][1]["fanout"]["itemCount"], 4);
        assert_eq!(
            details["nodes"][1]["fanout"]["data"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            details["nodes"][2]["result"]["data"],
            json!({"count":97,"sum":4656,"offsets":[0,32,64,96]})
        );
        assert_eq!(calls.len(), 6);
        assert_eq!(
            calls.iter().filter(|call| call["kind"] == "BATCH").count(),
            4
        );
    }

    #[test]
    fn one_rejected_batch_preserves_other_results_and_blocks_the_reducer() {
        log("native-rejection");
        let Some((output, calls)) = run(request(Some(json!(2)), 3), "SOURCE_FAIL") else {
            return;
        };
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][1]["state"], "rejected");
        assert_eq!(
            details["nodes"][1]["fanout"]["items"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(details["nodes"][1]["fanout"].get("data").is_none());
        assert_eq!(details["nodes"][2]["state"], "skipped");
        assert!(!calls.iter().any(|call| call["kind"] == "REDUCE"));
    }

    #[test]
    fn oversized_later_batch_stops_before_any_worker_is_submitted() {
        log("native-prelaunch-limit");
        let Some((output, calls)) = run(request(Some(json!(2)), 3), "SOURCE_LARGE") else {
            return;
        };
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][1]["skipReason"], "dependency_input_limit");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["kind"], "SOURCE_LARGE");
    }

    #[test]
    fn empty_batched_worklist_reduces_without_a_synthetic_worker() {
        log("native-empty");
        let Some((output, calls)) = run(request(Some(json!(32)), 4), "SOURCE_EMPTY") else {
            return;
        };
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][1]["fanout"]["itemCount"], 0);
        assert_eq!(details["nodes"][1]["fanout"]["data"], json!([]));
        assert_eq!(
            details["nodes"][2]["result"]["data"],
            json!({"count":0,"sum":0,"offsets":[]})
        );
        assert_eq!(calls.len(), 2);
        assert!(!calls.iter().any(|call| call["kind"] == "BATCH"));
    }
}
