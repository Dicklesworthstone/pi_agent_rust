use super::{AssistantMessage, ContentBlock, SseStream, StopReason, StreamEvent, StreamState};
use serde_json::{Value, json};

type TestState = StreamState<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>;

fn state() -> TestState {
    StreamState::new(
        SseStream::new(futures::stream::empty()),
        "deployment".to_string(),
        "azure-openai".to_string(),
        "azure-openai".to_string(),
    )
}

fn feed(state: &mut TestState, delta: Value, finish_reason: Option<&str>) {
    state
        .process_event(
            &json!({"choices": [{"delta": delta, "finish_reason": finish_reason}]}).to_string(),
        )
        .expect("valid wire JSON");
}

fn tool_delta(index: u32, arguments: &str) -> Value {
    json!({"tool_calls": [{
        "index": index,
        "id": format!("call-{index}"),
        "function": {"name": "read", "arguments": arguments}
    }]})
}

fn tool_end_count(state: &TestState) -> usize {
    state
        .pending_events
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
        .count()
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

#[test]
fn malformed_and_non_object_arguments_never_complete_a_tool() {
    for raw in ["{", r#"{"path":"unfinished"#, "null", "[]", "42", "true"] {
        let mut state = state();
        feed(&mut state, tool_delta(0, raw), Some("tool_calls"));
        let message = done_message(&mut state);
        assert_eq!(message.stop_reason, StopReason::Error, "accepted {raw:?}");
        assert_eq!(tool_end_count(&state), 0);
    }
}

#[test]
fn sentinel_without_finish_reason_parses_complete_tool_arguments() {
    let mut state = state();
    feed(&mut state, tool_delta(7, r#"{"path":"complete"}"#), None);
    let message = done_message(&mut state);
    assert_ne!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 1);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call)
            if call.id == "call-7" && call.arguments == json!({"path": "complete"})
    ));
}

#[test]
fn sentinel_without_finish_reason_rejects_truncated_arguments() {
    let mut state = state();
    feed(&mut state, tool_delta(7, r#"{"path":"unfinished"#), None);
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 0);
}

#[test]
fn invalid_parallel_call_rejects_the_entire_batch() {
    let mut state = state();
    feed(&mut state, tool_delta(8, r#"{"path":"valid"}"#), None);
    feed(&mut state, tool_delta(2, "{"), Some("tool_calls"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 0);
    assert!(message.error_message.unwrap().contains("index 2"));
}

#[test]
fn sparse_interleaved_calls_are_finalized_with_original_ids() {
    let mut state = state();
    feed(&mut state, tool_delta(8, r#"{"path":"a"#), None);
    feed(&mut state, tool_delta(2, r#"{"path":"b"}"#), None);
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
        ContentBlock::ToolCall(call) if call.id == "call-8" && call.arguments == json!({"path": "a"})
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::ToolCall(call) if call.id == "call-2" && call.arguments == json!({"path": "b"})
    ));
}

#[test]
fn no_argument_tool_is_normalized_to_an_object() {
    let mut state = state();
    feed(&mut state, tool_delta(0, ""), Some("tool_calls"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call) if call.arguments == json!({})
    ));
}

#[test]
fn error_envelope_cannot_be_erased_by_a_success_finish_reason() {
    let mut state = state();
    state
        .process_event(r#"{"error":{"code":"rate_limit","message":"quota exhausted"}}"#)
        .unwrap();
    feed(&mut state, json!({"content": "partial"}), Some("stop"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("quota exhausted"));
}

#[test]
fn blank_error_envelope_still_fails_the_response() {
    for error in [json!({}), json!({"message": " \t "})] {
        let mut state = state();
        state
            .process_event(&json!({"error": error}).to_string())
            .unwrap();
        let message = done_message(&mut state);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(message.error_message.is_some());
    }
}

#[test]
fn error_finish_reason_is_not_reported_as_a_normal_stop() {
    let mut state = state();
    feed(&mut state, json!({"content": "partial"}), Some("error"));
    assert_eq!(done_message(&mut state).stop_reason, StopReason::Error);
}

#[test]
fn late_content_is_rejected_without_rewriting_completed_content() {
    let mut state = state();
    feed(&mut state, json!({"content": "original"}), Some("stop"));
    feed(&mut state, json!({"content": "unexpected"}), None);
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text(text) if text.text == "original"
    ));
}

#[test]
fn duplicate_finish_frames_and_sentinel_emit_one_tool_end() {
    let mut state = state();
    feed(&mut state, tool_delta(0, "{}"), Some("tool_calls"));
    feed(&mut state, json!({}), Some("stop"));
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(tool_end_count(&state), 1);
    let queued = state.pending_events.len();
    state.finish_response();
    assert_eq!(state.pending_events.len(), queued);
}

#[test]
fn trailing_usage_and_terminal_event_order_are_preserved() {
    let mut state = state();
    feed(&mut state, json!({"content": "answer"}), None);
    state
        .process_event(r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#)
        .unwrap();
    state.finish_response();
    assert!(matches!(
        state.pending_events.front(),
        Some(StreamEvent::Start { .. })
    ));
    assert!(matches!(
        state.pending_events.back(),
        Some(StreamEvent::Done { .. })
    ));
    let ends = state
        .pending_events
        .iter()
        .filter(|event| matches!(event, StreamEvent::TextEnd { .. }))
        .count();
    assert_eq!(ends, 1);
    if let Some(StreamEvent::Done { message, .. }) = state.pending_events.back() {
        assert_eq!(message.usage.input, 12);
        assert_eq!(message.usage.output, 3);
        assert_eq!(message.usage.total_tokens, 15);
    }
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
