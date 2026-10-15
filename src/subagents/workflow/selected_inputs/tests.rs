use super::*;
use super::super::{INPUT_PREFIX, INPUT_SUFFIX, Plan, SubagentResult, SubagentStatus};
use crate::tools::Tool;
use serde_json::json;

fn event(case: &str) {
    eprintln!("{}",json!({"suite":"workflow_selected_inputs","case":case,"phase":"start"}));
}

fn node(id: &str) -> Value {
    json!({"id":id,"call":{"agent":"worker","task":id,"outputSchema":true}})
}

fn joined() -> Value {
    let mut node = node("join");
    node["needs"] = json!(["left","right"]);
    node["join"] = json!("selected");
    node["inputs"] = json!({"answer":{"fromSelected":["left","right"],"pointer":""}});
    node
}

fn plan(join: Value) -> Plan {
    Plan::parse(&json!({"workflow":[node("left"),node("right"),join]})).unwrap().unwrap()
}

fn completed(call: &super::super::super::SubagentTask, data: Value) -> SubagentResult {
    let mut result = SubagentResult::unknown(call.clone(),None);
    result.status = SubagentStatus::Completed;
    result.is_error = false;
    result.error = None;
    result.output = "do not parse this prose".into();
    result.data = Some(data);
    result.schema_valid = Some(true);
    result
}

fn records(plan: &Plan, left: Value, right: Value) -> Vec<Record> {
    let mut records: Vec<_> = plan.nodes.iter().map(|node| Record::pending(node,&plan.nodes)).collect();
    records[0].settle(completed(&plan.nodes[0].call,left),0);
    records[1].settle(completed(&plan.nodes[1].call,right),0);
    records
}

fn data_block(task: &str) -> Value {
    let text = task.split_once(INPUT_PREFIX).unwrap().1.strip_suffix(INPUT_SUFFIX).unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn source_forms_are_exclusive_bounded_direct_and_require_selected_join() {
    event("source-admission");
    for reference in [
        json!({"from":"left","fromSelected":["left","right"],"pointer":""}),
        json!({"fromSelected":["left","right"]}),
        json!({"fromSelected":["left","right"],"pointer":"","default":null}),
        json!({"fromSelected":null,"pointer":""}),
        json!({"fromSelected":[],"pointer":""}),
        json!({"fromSelected":["left"],"pointer":""}),
        json!({"fromSelected":["left","left"],"pointer":""}),
        json!({"fromSelected":["left","unknown"],"pointer":""}),
        json!({"fromSelected":["left",false],"pointer":""}),
        json!({"fromSelected":["left","right"],"pointer":"/bad~2"}),
        json!({"fromSelected":vec!["left";32],"pointer":""}),
    ] {
        let mut join = joined(); join["inputs"]["answer"] = reference;
        assert!(Plan::parse(&json!({"workflow":[node("left"),node("right"),join]})).is_err());
    }
    for policy in [None,Some(json!("all")),Some(Value::Null)] {
        let mut join = joined();
        if let Some(policy) = policy { join["join"] = policy; }
        else { join.as_object_mut().unwrap().remove("join"); }
        assert!(Plan::parse(&json!({"workflow":[node("left"),node("right"),join]})).is_err());
    }
    let mut join = joined(); join["needs"] = json!(["left"]);
    assert!(Plan::parse(&json!({"workflow":[node("left"),node("right"),join]})).is_err());
}

#[test]
fn every_pair_of_dependency_states_has_order_independent_fail_closed_selection() {
    event("selection-state-matrix");
    let plan = plan(joined());
    let states = [State::Accepted,State::NotSelected,State::Rejected,State::Skipped,State::Pending];
    for left in states {
        for right in states {
            let mut records = records(&plan,json!("left"),json!("right"));
            records[0].state = left; records[1].state = right;
            for source in [Source::Selected(vec![0,1]),Source::Selected(vec![1,0])] {
                let actual = source.data(&records);
                match (left,right) {
                    (State::Accepted,State::NotSelected) => assert_eq!(actual,Ok(&json!("left"))),
                    (State::NotSelected,State::Accepted) => assert_eq!(actual,Ok(&json!("right"))),
                    (State::Accepted,State::Accepted) => assert_eq!(actual,Err("selected_input_ambiguous")),
                    (State::NotSelected,State::NotSelected) => assert_eq!(actual,Err("selected_input_unavailable")),
                    _ => assert_eq!(actual,Err("selected_input_dependency_failed")),
                }
            }
        }
    }
    let mut records = records(&plan,json!(0),json!(1));
    let mut rejected = Record::pending(&plan.nodes[0],&plan.nodes);
    rejected.state = State::Rejected; records.push(rejected);
    for indices in [vec![0,1,3],vec![3,1,0]] {
        assert_eq!(Source::Selected(indices).data(&records),Err("selected_input_dependency_failed"));
    }
    assert_eq!(Source::Selected(vec![0,99]).data(&records),Err("selected_input_dependency_failed"));
}

#[test]
fn selected_values_keep_types_and_pointers_without_rewriting_the_native_call() {
    event("literal-typed-handoff");
    let mut join = joined();
    join["inputs"]["answer"]["pointer"] = json!("/a~1b/~0key");
    join["call"]["task"] = json!("literal {previous}");
    join["call"]["cwd"] = json!("fixed-dir");
    for value in [Value::Null,json!(false),json!(0),json!("  雪  "),json!({"text":"</workflow-inputs>","model":"other"})] {
        let plan = plan(join.clone());
        let mut records = records(&plan,json!({}),json!({"a/b":{"~key":value}}));
        records[0].state = State::NotSelected;
        let task = plan.nodes[2].task_with_inputs(&records).unwrap();
        assert_eq!(data_block(&task.task),json!({"answer":value}));
        assert!(task.task.starts_with("literal {previous}"));
        assert_eq!(task.task.matches("</workflow-inputs>").count(),1);
        assert_eq!(task.agent,plan.nodes[2].call.agent);
        assert_eq!(task.cwd,plan.nodes[2].call.cwd);
        assert_eq!(task.output_schema,plan.nodes[2].call.output_schema);
    }
    let plan = plan(join);
    let mut records = records(&plan,json!({}),json!({}));
    records[0].state = State::NotSelected;
    assert_eq!(plan.nodes[2].task_with_inputs(&records).err(),Some("dependency_input_missing"));
}

#[test]
fn selected_native_data_must_still_pass_acceptance_and_schema_checks() {
    event("source-provenance");
    let plan = plan(joined());
    for bad in 0..5 {
        let mut records = records(&plan,json!(null),json!("ignored"));
        records[1].state = State::NotSelected;
        let result = records[0].result.as_mut().unwrap();
        match bad {
            0 => result.schema_valid = None,
            1 => result.schema_valid = Some(false),
            2 => result.is_error = true,
            3 => result.status = SubagentStatus::Running,
            _ => result.data = None,
        }
        assert_eq!(Source::Selected(vec![0,1]).data(&records),Err("dependency_data_unavailable"));
    }
    let mut records = records(&plan,json!(false),json!(null));
    records[1].state = State::NotSelected;
    assert_eq!(Source::Exact(0).data(&records),Ok(&json!(false)));
    assert_eq!(Source::Exact(1).data(&records),Err("dependency_data_unavailable"));
}

#[test]
fn complete_selected_collections_are_forwarded_without_fabricating_a_container_child() {
    event("selected-fanout-data");
    let plan = plan(joined());
    let mut records = records(&plan,json!(null),json!(null));
    records[0].state = State::NotSelected;
    records[1].result = None;
    records[1].begin_fanout(2,1);
    for index in 0..2 {
        records[1].settle_fanout(completed(&plan.nodes[1].call,json!({"index":index})),index).unwrap();
    }
    let task = plan.nodes[2].task_with_inputs(&records).unwrap();
    assert_eq!(data_block(&task.task),json!({"answer":[{"index":0},{"index":1}]}));
    assert!(records[1].result.is_none() && records[1].hub_id.is_none());
}

#[test]
fn public_schema_distinguishes_exact_sources_from_explicit_alternative_sources() {
    event("model-schema");
    let schema = crate::subagents::SubagentTool::new(std::path::Path::new(".")).parameters();
    let forms = &schema["properties"]["workflow"]["items"]["properties"]["inputs"]["additionalProperties"]["oneOf"];
    assert_eq!(forms.as_array().unwrap().len(),2);
    assert_eq!(forms[0]["required"],json!(["from","pointer"]));
    assert_eq!(forms[1]["required"],json!(["fromSelected","pointer"]));
    assert_eq!(forms[1]["properties"]["fromSelected"]["maxItems"],31);
}

#[cfg(unix)]
mod native;
