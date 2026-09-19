use super::{
    AssistantMessage, ContentBlock, SseStream, StopReason, StreamEvent, StreamState,
    parse_final_tool_arguments,
};
use serde_json::{Value, json};

type TestState =
    StreamState<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>;

fn state() -> TestState {
    StreamState::new(
        SseStream::new(futures::stream::empty()),
        "test-model".to_string(),
        "openai-completions".to_string(),
        "openai".to_string(),
    )
}

fn feed(state: &mut TestState, delta: Value, finish_reason: Option<&str>) {
    state
        .process_event(
            &json!({"choices": [{"delta": delta, "finish_reason": finish_reason}]}).to_string(),
        )
        .expect("valid wire JSON");
}

fn tool_delta(index: u32, id: &str, name: &str, arguments: &str) -> Value {
    json!({"tool_calls": [{
        "index": index,
        "id": id,
        "function": {"name": name, "arguments": arguments}
    }]})
}

fn done_message(state: &mut TestState) -> AssistantMessage {
    state.finish_response();
    match state.pending_events.pop_back().expect("terminal event") {
        StreamEvent::Done { reason, message } => {
            assert_eq!(reason, message.stop_reason);
            message
        }
        event => panic!("expected Done, got {event:?}"),
    }
}

fn tool_end_count(state: &TestState) -> usize {
    state
        .pending_events
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
        .count()
}

#[test]
fn terminal_parser_rejects_incomplete_or_non_object_arguments() {
    for raw in [
        "{",
        r#"{"command":"unfinished"#,
        r#"{"path":}"#,
        r#"{"x":1,}"#,
        r#"{"x":1} trailing"#,
        "null",
        "true",
        "42",
        "[]",
        "[{}]",
        r#""not an object""#,
    ] {
        assert!(parse_final_tool_arguments(raw).is_err(), "accepted {raw:?}");
    }
}

#[test]
fn terminal_parser_accepts_complete_objects_and_no_argument_tools() {
    for raw in ["", " \r\n\t", "{}", " { } \n"] {
        assert_eq!(parse_final_tool_arguments(raw).unwrap(), json!({}));
    }
    let arguments = json!({"command": "echo λ", "nested": [1, {"ok": true}]});
    assert_eq!(
        parse_final_tool_arguments(&arguments.to_string()).unwrap(),
        arguments
    );
}

#[test]
fn malformed_arguments_fail_instead_of_emitting_a_completed_tool() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call-1", "bash", r#"{"command":"unfinished"#),
        Some("tool_calls"),
    );
    assert_eq!(state.partial.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 0);
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.error_message.unwrap().contains("index 0"));
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.arguments.is_null()
    ));
}

#[test]
fn sentinel_only_response_does_not_execute_repaired_preview_arguments() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call-1", "write", r#"{"path":"unfinished"#),
        None,
    );
    // Preview completion is useful for display, but is not final wire JSON.
    assert!(matches!(
        &state.partial.content[0],
        ContentBlock::ToolCall(call) if call.arguments.is_object()
    ));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 0);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.arguments.is_null()
    ));
}

#[test]
fn sentinel_only_valid_tool_call_is_strictly_finalized() {
    let mut state = state();
    let arguments = json!({"command": "printf 'complete'"});
    feed(
        &mut state,
        tool_delta(7, "call-7", "bash", &arguments.to_string()),
        None,
    );
    let message = done_message(&mut state);
    assert_ne!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 1);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call)
            if call.id == "call-7" && call.name == "bash" && call.arguments == arguments
    ));
}

#[test]
fn invalid_parallel_call_prevents_all_tool_end_events() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(2, "good", "read", r#"{"path":"ok"}"#),
        None,
    );
    feed(
        &mut state,
        tool_delta(9, "bad", "write", "{"),
        Some("tool_calls"),
    );
    assert_eq!(tool_end_count(&state), 0);
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    for block in message.content {
        if let ContentBlock::ToolCall(call) = block {
            assert!(call.arguments.is_null());
        }
    }
}

#[test]
fn sparse_interleaved_calls_keep_their_ids_and_arguments() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(8, "eight", "read", r#"{"path":"a"#),
        None,
    );
    feed(
        &mut state,
        tool_delta(2, "two", "write", r#"{"path":"b"}"#),
        None,
    );
    feed(
        &mut state,
        json!({"tool_calls": [{"index": 8, "function": {"arguments": "\"}"}}]}),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(tool_end_count(&state), 2);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.id == "eight" && call.arguments == json!({"path": "a"})
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::ToolCall(call) if call.id == "two" && call.arguments == json!({"path": "b"})
    ));
}

#[test]
fn empty_arguments_are_normalized_for_a_no_argument_tool() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "clock", "clock", ""),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.arguments == json!({})
    ));
}

#[test]
fn provider_error_cannot_be_overwritten_by_finish_reason() {
    let mut state = state();
    state
        .process_event(r#"{"error":{"message":"upstream failed"}}"#)
        .unwrap();
    feed(&mut state, json!({"content": "partial"}), Some("stop"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("upstream failed"));
}

#[test]
fn first_provider_error_survives_secondary_argument_failure() {
    let mut state = state();
    state
        .process_event(r#"{"error":{"message":"original failure"}}"#)
        .unwrap();
    feed(
        &mut state,
        tool_delta(0, "call", "read", "{"),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    assert_eq!(message.error_message.as_deref(), Some("original failure"));
    assert_eq!(tool_end_count(&state), 0);
}

#[test]
fn content_after_finish_reason_is_rejected_without_mutating_the_tool() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call", "read", r#"{"path":"original"}"#),
        Some("tool_calls"),
    );
    feed(
        &mut state,
        json!({"tool_calls": [{"index": 0, "function": {"arguments": "unexpected"}}]}),
        None,
    );
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.arguments == json!({"path": "original"})
    ));
}

#[test]
fn duplicate_terminal_frames_do_not_change_reason_or_repeat_ends() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call", "clock", "{}"),
        Some("tool_calls"),
    );
    feed(&mut state, json!({}), Some("stop"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(tool_end_count(&state), 1);
    let queued = state.pending_events.len();
    state.finish_response();
    assert_eq!(state.pending_events.len(), queued);
}

#[test]
fn trailing_usage_is_retained_after_content_finalization() {
    let mut state = state();
    feed(&mut state, json!({"content": "answer"}), Some("stop"));
    state
        .process_event(
            &json!({"choices": [], "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3,
                "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 5},
                "cost": 0.125
            }})
            .to_string(),
        )
        .unwrap();
    let message = done_message(&mut state);
    assert_eq!(message.usage.input, 7);
    assert_eq!(message.usage.cache_read, 5);
    assert_eq!(message.usage.output, 3);
    assert_eq!(message.usage.total_tokens, 15);
    assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
}

#[test]
fn sentinel_closes_thinking_and_text_before_done_exactly_once() {
    let mut state = state();
    feed(&mut state, json!({"reasoning_content": "reasoning"}), None);
    feed(&mut state, json!({"content": "answer"}), None);
    state.finish_response();
    assert!(matches!(
        state.pending_events.front(),
        Some(StreamEvent::Start { .. })
    ));
    assert!(matches!(
        state.pending_events.back(),
        Some(StreamEvent::Done { .. })
    ));
    assert_eq!(
        state
            .pending_events
            .iter()
            .filter(|event| matches!(event, StreamEvent::TextEnd { .. }))
            .count(),
        1
    );
    assert_eq!(
        state
            .pending_events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ThinkingEnd { .. }))
            .count(),
        1
    );
    let queued = state.pending_events.len();
    state.finish_response();
    assert_eq!(state.pending_events.len(), queued);
}

#[test]
fn terminal_argument_errors_do_not_echo_sensitive_payloads() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call", "write", r#"{"token":"private-token-value"#),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    let error = message.error_message.expect("argument error");
    assert!(!error.contains("private-token-value"));
    assert!(error.contains("complete JSON"));
}

#[test]
fn late_error_finish_reason_is_not_ignored_as_a_duplicate() {
    for reason in ["content_filter", "error"] {
        let mut state = state();
        feed(&mut state, json!({"content": "partial"}), Some("stop"));
        feed(&mut state, json!({}), Some(reason));
        assert_eq!(done_message(&mut state).stop_reason, StopReason::Error);
    }
}
