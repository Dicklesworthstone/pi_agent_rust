use super::*;
use crate::model::{AssistantMessage, ImageContent, Message, TextContent};
use crate::tools::ToolOutput;
use serde_json::json;

fn start(id: &str, name: &str, args: Value) -> AgentEvent {
    AgentEvent::ToolExecutionStart {
        tool_call_id: id.into(),
        tool_name: name.into(),
        args,
    }
}

fn end(id: &str, name: &str, output: &str) -> AgentEvent {
    AgentEvent::ToolExecutionEnd {
        tool_call_id: id.into(),
        tool_name: name.into(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details: None,
            is_error: false,
        },
        is_error: false,
    }
}

fn operation(tracker: &mut Tracker, id: &str, path: &str, output: &str) {
    tracker.observe(&start(id, "read", json!({"path":path})));
    tracker.observe(&end(id, "read", output));
}

fn paused(decision: GoalDecision, code: &str) {
    match decision {
        GoalDecision::Pause { reason } => assert!(reason.contains(code), "{reason}"),
        _ => panic!("expected a pause, never completion"),
    }
}

fn continues(decision: GoalDecision) {
    assert!(matches!(decision, GoalDecision::Continue { .. }));
}

#[test]
fn text_only_success_pauses_instead_of_treating_prose_as_progress() {
    let mut tracker = Tracker::new(4);
    tracker.observe(&AgentEvent::MessageEnd {
        message: Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Made progress; goal complete!"))],
            ..AssistantMessage::default()
        }),
    });
    paused(tracker.decide(), "GOAL_NO_TOOL_PROGRESS");
}

#[test]
fn a_new_successful_operation_permits_but_does_not_prove_progress() {
    let mut tracker = Tracker::new(4);
    operation(&mut tracker, "first", "a", "A");
    continues(tracker.decide());
    operation(&mut tracker, "second", "b", "B");
    continues(tracker.decide());
    operation(&mut tracker, "third", "b", "changed B");
    continues(tracker.decide());
    operation(&mut tracker, "fourth", "c", "C");
    paused(tracker.decide(), "GOAL_STEP_LIMIT");
}

#[test]
fn new_call_ids_and_different_diagnostics_do_not_hide_repeated_work() {
    let mut tracker = Tracker::new(4);
    operation(&mut tracker, "one", "a", "same");
    continues(tracker.decide());
    tracker.observe(&start("completely-new-id", "read", json!({"path":"a"})));
    let mut event = end("completely-new-id", "read", "same");
    if let AgentEvent::ToolExecutionEnd { result, .. } = &mut event {
        result.details = Some(json!({"elapsed":999, "artifact":"unique-temp-path"}));
        if let ContentBlock::Text(text) = &mut result.content[0] {
            text.text_signature = Some("new-provider-signature".into());
        }
    }
    tracker.observe(&event);
    paused(tracker.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
}

#[test]
fn canonical_json_ignores_object_insertion_order_at_every_depth() {
    let left: Value = serde_json::from_str(r#"{"z":2,"a":{"y":1,"x":[true,null]}}"#).unwrap();
    let right: Value = serde_json::from_str(r#"{"a":{"x":[true,null],"y":1},"z":2}"#).unwrap();
    let mut first = Sha256::new();
    let mut second = Sha256::new();
    json(&mut first, &mut Budget::default(), &left, 0).unwrap();
    json(&mut second, &mut Budget::default(), &right, 0).unwrap();
    assert_eq!(first.finalize(), second.finalize());
}

#[test]
fn parallel_completion_order_and_redundant_copies_do_not_count_as_new_work() {
    let mut tracker = Tracker::new(5);
    tracker.observe(&start("a", "read", json!({"path":"a"})));
    tracker.observe(&start("b", "read", json!({"path":"b"})));
    tracker.observe(&end("b", "read", "B"));
    tracker.observe(&end("a", "read", "A"));
    continues(tracker.decide());
    operation(&mut tracker, "c", "a", "A");
    operation(&mut tracker, "d", "b", "B");
    operation(&mut tracker, "e", "a", "A");
    paused(tracker.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
}

#[test]
fn multi_prompt_cycles_and_subsets_are_detected_across_the_whole_run() {
    let mut tracker = Tracker::new(8);
    operation(&mut tracker, "1", "a", "A");
    continues(tracker.decide());
    operation(&mut tracker, "2", "b", "B");
    continues(tracker.decide());
    operation(&mut tracker, "3", "c", "C");
    continues(tracker.decide());
    operation(&mut tracker, "4", "a", "A");
    operation(&mut tracker, "5", "c", "C");
    paused(tracker.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
}

#[test]
fn either_error_flag_excludes_an_operation_from_successful_progress() {
    for event_flag in [false, true] {
        let mut tracker = Tracker::new(3);
        tracker.observe(&start("a", "read", json!({"path":"missing"})));
        let mut event = end("a", "read", "different error each time");
        if let AgentEvent::ToolExecutionEnd { result, is_error, .. } = &mut event {
            *is_error = event_flag;
            result.is_error = !event_flag;
        }
        tracker.observe(&event);
        paused(tracker.decide(), "GOAL_NO_TOOL_PROGRESS");
    }
}

#[test]
fn new_failures_cannot_rearm_a_run_whose_only_success_was_already_seen() {
    let mut tracker = Tracker::new(3);
    operation(&mut tracker, "a", "readable", "A");
    continues(tracker.decide());
    operation(&mut tracker, "b", "readable", "A");
    tracker.observe(&start("c", "read", json!({"path":"different-failure"})));
    let mut event = end("c", "read", "new error");
    if let AgentEvent::ToolExecutionEnd { is_error, .. } = &mut event {
        *is_error = true;
    }
    tracker.observe(&event);
    paused(tracker.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
}

#[test]
fn changed_image_bytes_are_new_output_but_stable_bytes_repeat() {
    let mut tracker = Tracker::new(5);
    for (id, data) in [("one", "YQ=="), ("two", "Yg=="), ("three", "Yg==")] {
        tracker.observe(&start(id, "read", json!({"path":"image.png"})));
        let mut event = end(id, "read", "");
        if let AgentEvent::ToolExecutionEnd { result, .. } = &mut event {
            result.content = vec![ContentBlock::Image(ImageContent {
                data: data.into(), mime_type: "image/png".into(),
            })];
        }
        tracker.observe(&event);
        if id == "three" {
            paused(tracker.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
        } else {
            continues(tracker.decide());
        }
    }
}

#[test]
fn missing_mismatched_duplicate_or_unfinished_events_fail_closed() {
    let cases = [
        vec![end("unknown", "read", "A")],
        vec![start("a", "read", json!({})), end("a", "write", "A")],
        vec![start("a", "read", json!({})), start("a", "read", json!({}))],
        vec![start("a", "read", json!({})), end("a", "read", "A"), end("a", "read", "A")],
        vec![start("a", "read", json!({}))],
    ];
    for events in cases {
        let mut tracker = Tracker::new(3);
        operation(&mut tracker, "valid", "a", "A");
        for event in events { tracker.observe(&event); }
        paused(tracker.decide(), "GOAL_PROGRESS_UNCERTAIN");
    }
}

#[test]
fn exact_call_limit_fits_and_one_more_invalidates_the_entire_observation() {
    for extra in [0, 1] {
        let mut tracker = Tracker::new(3);
        for i in 0..MAX_CALLS + extra {
            operation(&mut tracker, &i.to_string(), "a", "A");
        }
        if extra == 0 {
            continues(tracker.decide());
        } else {
            paused(tracker.decide(), "GOAL_PROGRESS_UNCERTAIN");
        }
    }
}

#[test]
fn byte_budget_is_aggregate_not_reset_for_each_operation() {
    let mut tracker = Tracker::new(3);
    let output = "x".repeat(MAX_BYTES / 2);
    operation(&mut tracker, "a", "a", &output);
    assert!(!tracker.current.invalid);
    operation(&mut tracker, "b", "b", &output);
    assert!(tracker.current.invalid);
    paused(tracker.decide(), "GOAL_PROGRESS_UNCERTAIN");
}

#[test]
fn byte_and_json_node_admission_boundaries_are_inclusive() {
    let mut digest = Sha256::new();
    let mut budget = Budget { bytes: 9, nodes: 1 };
    part(&mut digest, &mut budget, b"a").unwrap();
    assert_eq!(budget.bytes, 0);
    assert!(part(&mut digest, &mut budget, b"").is_err());
    let mut budget = Budget { bytes: 100, nodes: 1 };
    json(&mut digest, &mut budget, &Value::Null, 0).unwrap();
    assert!(json(&mut digest, &mut budget, &Value::Null, 0).is_err());
}

#[test]
fn nested_and_wide_arguments_stop_at_fixed_depth_and_node_limits() {
    let mut value = Value::Null;
    for _ in 0..MAX_DEPTH { value = Value::Array(vec![value]); }
    let mut digest = Sha256::new();
    json(&mut digest, &mut Budget::default(), &value, 0).unwrap();
    let too_deep = Value::Array(vec![value]);
    assert!(json(&mut digest, &mut Budget::default(), &too_deep, 0).is_err());
    let wide = Value::Array(vec![Value::Null; MAX_NODES]);
    assert!(json(&mut digest, &mut Budget::default(), &wide, 0).is_err());
}

#[test]
fn a_partial_fingerprint_cannot_authorize_continuation() {
    let mut tracker = Tracker::new(3);
    operation(&mut tracker, "good", "a", "A");
    tracker.observe(&start("large", "read", json!({"path":"x".repeat(MAX_BYTES)})));
    tracker.observe(&end("large", "read", "later valid-looking output"));
    paused(tracker.decide(), "GOAL_PROGRESS_UNCERTAIN");
}

#[test]
fn step_boundary_and_latched_pause_can_never_complete_or_rearm_a_goal() {
    let mut tracker = Tracker::new(1);
    operation(&mut tracker, "a", "a", "A");
    paused(tracker.decide(), "GOAL_STEP_LIMIT");
    operation(&mut tracker, "b", "b", "B");
    paused(tracker.decide(), "GOAL_STEP_LIMIT");
    assert_eq!(tracker.steps, 1);
    assert!(tracker.seen.is_empty());
}

#[test]
fn explicit_new_run_rearms_observation_but_never_reuses_old_state() {
    let mut first = Tracker::new(3);
    operation(&mut first, "a", "a", "A");
    continues(first.decide());
    operation(&mut first, "b", "a", "A");
    paused(first.decide(), "GOAL_REPEATED_TOOL_PROGRESS");
    let mut second = Tracker::new(3);
    operation(&mut second, "c", "a", "A");
    continues(second.decide());
}
