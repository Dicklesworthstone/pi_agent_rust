use super::{
    AssistantMessage, ContentBlock, EventStream, MAX_READY_EVENTS_PER_YIELD, SseStream, StopReason,
    StreamEvent, StreamState, parse_final_tool_arguments, wire_stream,
};
use futures::{FutureExt as _, Stream, StreamExt as _};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, Wake, Waker};

type TestState = StreamState<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>;

fn state() -> TestState {
    StreamState::new(
        SseStream::new(futures::stream::empty()),
        "test-model".to_string(),
        "openai-completions".to_string(),
        "openai".to_string(),
    )
}

#[allow(clippy::needless_pass_by_value)]
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
fn terminal_parser_rejects_duplicate_decoded_keys_recursively() {
    for raw in [
        r#"{"path":"first","path":"second"}"#,
        r#"{"path":"first","pa\u0074h":"second"}"#,
        r#"{"nested":{"x":1,"x":2}}"#,
        r#"{"items":[{"safe":1},{"x":1,"x":2}]}"#,
    ] {
        assert_eq!(
            parse_final_tool_arguments(raw).unwrap_err(),
            "arguments contain duplicate JSON object keys"
        );
    }
}

#[test]
fn terminal_parser_allows_same_key_in_distinct_objects() {
    let raw = r#"{"left":{"path":"a"},"right":{"path":"b"},"items":[{"x":1},{"x":2}]}"#;
    assert_eq!(
        parse_final_tool_arguments(raw).unwrap(),
        json!({"left":{"path":"a"},"right":{"path":"b"},"items":[{"x":1},{"x":2}]})
    );
}

#[test]
fn duplicate_argument_rejects_the_entire_parallel_tool_batch() {
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "good", "read", r#"{"path":"kept"}"#),
        None,
    );
    feed(
        &mut state,
        tool_delta(1, "bad", "write", r#"{"path":"one","path":"two"}"#),
        Some("tool_calls"),
    );
    assert_eq!(tool_end_count(&state), 0);
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("duplicate JSON object keys"))
    );
    for block in message.content {
        if let ContentBlock::ToolCall(call) = block {
            assert!(call.arguments.is_null());
        }
    }
}

#[test]
fn duplicate_argument_diagnostic_does_not_echo_private_payload() {
    let raw = r#"{"private-key":"private-first","private-key":"private-second"}"#;
    let mut state = state();
    feed(
        &mut state,
        tool_delta(0, "call", "write", raw),
        Some("tool_calls"),
    );
    let error = done_message(&mut state).error_message.unwrap();
    assert!(error.contains("duplicate JSON object keys"));
    for secret in ["private-key", "private-first", "private-second"] {
        assert!(!error.contains(secret));
    }
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
fn malformed_tool_identities_reject_the_whole_wire_batch() {
    for (id, name, diagnostic) in [
        (None, Some("write"), "ID is empty"),
        (Some(""), Some("write"), "ID is empty"),
        (Some(" \t"), Some("write"), "ID is empty"),
        (Some("other"), None, "name is empty"),
        (Some("other"), Some(""), "name is empty"),
        (Some("other"), Some(" \n"), "name is empty"),
        (Some("private-call-id"), Some("write"), "ID is duplicated"),
    ] {
        for finish_reason in [Some("tool_calls"), Some("stop"), None] {
            let mut state = state();
            feed(&mut state, json!({"content": "partial explanation"}), None);
            feed(
                &mut state,
                tool_delta(2, "private-call-id", "read", r#"{"path":"kept"}"#),
                None,
            );
            feed(
                &mut state,
                json!({"tool_calls": [{
                    "index": 9,
                    "id": id,
                    "function": {"name": name, "arguments": r#"{"token":"private-argument"}"#}
                }]}),
                finish_reason,
            );
            state
                .process_event(
                    r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#,
                )
                .unwrap();
            let message = done_message(&mut state);
            assert_eq!(message.stop_reason, StopReason::Error);
            assert_eq!(tool_end_count(&state), 0);
            let error = message.error_message.as_deref().expect("identity error");
            assert!(error.contains("index 9"));
            assert!(error.contains(diagnostic), "{error}");
            assert!(!error.contains("private-call-id"));
            assert!(!error.contains("private-argument"));
            assert_eq!(message.usage.input, 12);
            assert_eq!(message.usage.output, 3);
            assert_eq!(message.usage.total_tokens, 15);
            assert!(matches!(
                &message.content[0],
                ContentBlock::Text(text) if text.text == "partial explanation"
            ));
            assert_eq!(message.content.len(), 3);
            for block in &message.content[1..] {
                assert!(matches!(
                    block,
                    ContentBlock::ToolCall(call) if call.arguments.is_null()
                ));
            }
        }
    }
}

#[test]
fn tool_identities_may_finish_after_the_opening_delta() {
    let mut state = state();
    feed(
        &mut state,
        json!({"tool_calls": [{"index": 4, "function": {"arguments": "{"}}]}),
        None,
    );
    feed(&mut state, tool_delta(4, "call-", "re", ""), None);
    feed(
        &mut state,
        tool_delta(4, "four", "ad", r#""path":"complete"}"#),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(tool_end_count(&state), 1);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall(call)
            if call.id == "call-four" && call.name == "read"
                && call.arguments == json!({"path": "complete"})
    ));
}

#[test]
fn identity_validation_does_not_replace_the_provider_error() {
    let mut state = state();
    state
        .process_event(r#"{"error":{"message":"original provider failure"}}"#)
        .unwrap();
    feed(
        &mut state,
        tool_delta(1, "", "read", "{}"),
        Some("tool_calls"),
    );
    let message = done_message(&mut state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(tool_end_count(&state), 0);
    assert_eq!(
        message.error_message.as_deref(),
        Some("original provider failure")
    );
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

#[derive(Default)]
struct SourceObservation {
    dropped: AtomicBool,
    polls: AtomicUsize,
}

struct ObservedSource {
    chunks: VecDeque<std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
    observation: Arc<SourceObservation>,
}

impl Stream for ObservedSource {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.observation.polls.fetch_add(1, Ordering::SeqCst);
        if let Some(chunk) = this.chunks.pop_front() {
            Poll::Ready(Some(chunk))
        } else if this.pending_at_end {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

impl Drop for ObservedSource {
    fn drop(&mut self) {
        self.observation.dropped.store(true, Ordering::SeqCst);
    }
}

fn observed_wire(
    chunks: impl IntoIterator<Item = std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
) -> (EventStream, Arc<SourceObservation>) {
    let observation = Arc::new(SourceObservation::default());
    let source = ObservedSource {
        chunks: chunks.into_iter().collect(),
        pending_at_end,
        observation: Arc::clone(&observation),
    };
    (
        wire_stream(
            source,
            "test-model".to_string(),
            "openai-completions".to_string(),
            "openai".to_string(),
            false,
            None,
        ),
        observation,
    )
}

fn wire_bytes(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut bytes = String::new();
    for event in events {
        write!(bytes, "data: {event}\n\n").unwrap();
    }
    bytes.into_bytes()
}

fn usage_chunk() -> Value {
    json!({"choices": [], "usage": {
        "prompt_tokens": 12,
        "completion_tokens": 3,
        "total_tokens": 15,
        "prompt_tokens_details": {"cached_tokens": 5},
        "cost": 0.125
    }})
}

fn assert_retired(output: &mut EventStream, observation: &SourceObservation) {
    // Check BEFORE requesting EOF. The terminal stream can remain in an SDK
    // caller's state indefinitely without ever receiving another poll.
    assert!(
        observation.dropped.load(Ordering::SeqCst),
        "the terminal event retained its source"
    );
    let polls = observation.polls.load(Ordering::SeqCst);
    for _ in 0..3 {
        assert!(
            output
                .next()
                .now_or_never()
                .expect("fused EOF must be ready")
                .is_none()
        );
    }
    assert_eq!(observation.polls.load(Ordering::SeqCst), polls);
}

#[test]
fn wire_terminal_releases_source_without_losing_trailing_usage_or_block_ends() {
    let mut bytes = wire_bytes([
        json!({"choices":[{"delta":{"reasoning_content":"reasoning"}}]}),
        json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}),
        usage_chunk(),
    ]);
    bytes.extend_from_slice(b"data: [DONE]\n\ndata: {unread invalid tail}\n\n");
    let (mut output, observation) = observed_wire([Ok(bytes)], true);
    let mut text_ends = 0;
    let mut thinking_ends = 0;
    let mut terminal_seen = false;
    for _ in 0..16 {
        let event = output.next().now_or_never().unwrap().unwrap().unwrap();
        match event {
            StreamEvent::TextEnd { content, .. } => {
                assert_eq!(content, "answer");
                text_ends += 1;
                assert!(!observation.dropped.load(Ordering::SeqCst));
            }
            StreamEvent::ThinkingEnd { content, .. } => {
                assert_eq!(content, "reasoning");
                thinking_ends += 1;
            }
            StreamEvent::Done { reason, message } => {
                assert_eq!(reason, StopReason::Stop);
                assert_eq!(message.usage.input, 7);
                assert_eq!(message.usage.cache_read, 5);
                assert_eq!(message.usage.output, 3);
                assert_eq!(message.usage.total_tokens, 15);
                assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
                terminal_seen = true;
                break;
            }
            _ => {}
        }
    }
    assert!(terminal_seen);
    assert_eq!((text_ends, thinking_ends), (1, 1));
    assert_eq!(observation.polls.load(Ordering::SeqCst), 1);
    assert_retired(&mut output, &observation);
}

#[test]
fn wire_error_terminal_retains_failure_and_usage_then_releases_source() {
    for failure in [
        json!({"error":{"message":"provider overloaded"},"choices":[]}),
        json!({"choices":[{"delta":tool_delta(0, "call", "write", "{"),"finish_reason":"tool_calls"}]}),
        json!({"choices":[{"delta":{},"finish_reason":"content_filter"}]}),
    ] {
        let mut bytes = wire_bytes([failure, usage_chunk()]);
        bytes.extend_from_slice(b"data: [DONE]\n\ndata: [DONE]\n\n");
        let (mut output, observation) = observed_wire([Ok(bytes)], true);
        let mut terminal_seen = false;
        for _ in 0..12 {
            let event = output.next().now_or_never().unwrap().unwrap().unwrap();
            assert!(!matches!(event, StreamEvent::ToolCallEnd { .. }));
            if let StreamEvent::Done { reason, message } = event {
                assert_eq!(reason, StopReason::Error);
                assert!(message.error_message.is_some());
                assert_eq!(message.usage.total_tokens, 15);
                terminal_seen = true;
                break;
            }
        }
        assert!(terminal_seen);
        assert_retired(&mut output, &observation);
    }
}

#[test]
fn wire_parse_and_transport_failures_release_source_before_error_delivery() {
    for input in [
        Ok(b"data: {invalid json}\n\n".to_vec()),
        Ok(b"data: {\"choices\":\"invalid shape\"}\n\n".to_vec()),
        Ok(b"data: \xff\n\n".to_vec()),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fixture reset",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fixture corruption",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "fixture interrupted",
        )),
    ] {
        let (mut output, observation) = observed_wire([input], true);
        assert!(output.next().now_or_never().unwrap().unwrap().is_err());
        assert_retired(&mut output, &observation);
    }
}

#[test]
fn wire_unexpected_eof_preserves_partial_events_and_fuses_after_one_error() {
    for partial in [false, true] {
        let chunks = if partial {
            vec![Ok(wire_bytes([
                json!({"choices":[{"delta":{"content":"partial"}}]}),
            ]))]
        } else {
            Vec::new()
        };
        let (mut output, observation) = observed_wire(chunks, false);
        let mut text = String::new();
        let mut failed = false;
        for _ in 0..8 {
            match output.next().now_or_never().unwrap().unwrap() {
                Ok(StreamEvent::TextDelta { delta, .. }) => text.push_str(&delta),
                Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. }) => {
                    panic!("an unexpected EOF must not invent a completed response");
                }
                Ok(_) => {}
                Err(error) => {
                    assert!(error.to_string().contains("before [DONE] sentinel"));
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed);
        assert_eq!(text, if partial { "partial" } else { "" });
        assert_retired(&mut output, &observation);
    }
}

#[test]
fn dropping_an_unpolled_or_pending_wire_stream_releases_its_source() {
    for poll_first in [false, true] {
        let (mut output, observation) = observed_wire([], true);
        if poll_first {
            assert!(output.next().now_or_never().is_none());
        }
        assert!(!observation.dropped.load(Ordering::SeqCst));
        drop(output);
        assert!(observation.dropped.load(Ordering::SeqCst));
    }
}

#[derive(Clone, Copy, Debug)]
enum AudioSourceEnd {
    Error,
    Eof,
    Pending,
}

#[derive(Clone, Copy, Debug)]
enum AudioEvidence {
    Missing,
    Partial,
    Complete,
    Zero,
}

struct AudioSourceProvider {
    ending: AudioSourceEnd,
    evidence: AudioEvidence,
    observation: Arc<SourceObservation>,
}

#[async_trait::async_trait]
impl crate::provider::Provider for AudioSourceProvider {
    fn name(&self) -> &str {
        "openai"
    }
    fn api(&self) -> &'static str {
        "openai-completions"
    }
    fn model_id(&self) -> &str {
        "gpt-audio-1.5"
    }

    fn model_cost(&self) -> Option<crate::provider::ModelCost> {
        Some(crate::provider::ModelCost {
            input: 2.5,
            output: 10.0,
            audio: Some(crate::provider::AudioModelCost {
                input: Some(32.0),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    async fn stream(
        &self,
        _: &crate::provider::Context<'_>,
        options: &crate::provider::StreamOptions,
    ) -> crate::error::Result<EventStream> {
        let complete = matches!(self.evidence, AudioEvidence::Complete | AudioEvidence::Zero);
        let mut frame = json!({"choices":[{"delta":{"content":"heard"},"finish_reason": if complete { Some("stop") } else { None }}]});
        if !matches!(self.evidence, AudioEvidence::Missing) {
            frame["usage"] = json!({"prompt_tokens":1000,"completion_tokens":0,"total_tokens":1000,"prompt_tokens_details":{"audio_tokens":1000}});
            if matches!(self.evidence, AudioEvidence::Zero) {
                frame["usage"]["cost"] = json!(0);
            }
        }
        let mut chunks = VecDeque::from([Ok(wire_bytes([frame]))]);
        if matches!(self.ending, AudioSourceEnd::Error) {
            chunks.push_back(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "audio fixture reset",
            )));
        }
        Ok(wire_stream(
            ObservedSource {
                chunks,
                pending_at_end: matches!(self.ending, AudioSourceEnd::Pending),
                observation: Arc::clone(&self.observation),
            },
            self.model_id().to_string(),
            self.api().to_string(),
            self.name().to_string(),
            true,
            options.usage_observer.clone(),
        ))
    }
}

#[test]
fn interrupted_audio_sources_quarantine_missing_final_billing_and_retain_known_spend() {
    futures::executor::block_on(async {
        for ending in [
            AudioSourceEnd::Error,
            AudioSourceEnd::Eof,
            AudioSourceEnd::Pending,
        ] {
            for evidence in [AudioEvidence::Missing, AudioEvidence::Partial] {
                for unpolled in [false, true] {
                    let observation = Arc::new(SourceObservation::default());
                    let budget = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
                    let provider = crate::provider::guard_with_cost_budget(
                        Arc::new(AudioSourceProvider {
                            ending,
                            evidence,
                            observation: Arc::clone(&observation),
                        }),
                        budget.clone(),
                    );
                    let mut output = provider
                        .stream(
                            &crate::provider::Context::default(),
                            &crate::provider::StreamOptions::default(),
                        )
                        .await
                        .unwrap();
                    let mut stopped = unpolled;
                    if !unpolled {
                        for _ in 0..12 {
                            match output.next().now_or_never() {
                                Some(Some(Ok(_))) => {}
                                Some(Some(Err(_))) => {
                                    assert!(!matches!(ending, AudioSourceEnd::Pending));
                                    stopped = true;
                                    break;
                                }
                                None => {
                                    assert!(matches!(ending, AudioSourceEnd::Pending));
                                    stopped = true;
                                    break;
                                }
                                Some(None) => panic!("incomplete audio source silently ended"),
                            }
                        }
                    }
                    assert!(stopped, "{ending:?} {evidence:?}");
                    drop(output);
                    assert!(observation.dropped.load(Ordering::SeqCst));
                    let expected = if !unpolled && matches!(evidence, AudioEvidence::Partial) {
                        0.032
                    } else {
                        0.0
                    };
                    assert!(
                        (budget.spent() - expected).abs() < 1e-12,
                        "{ending:?} {evidence:?} unpolled={unpolled}"
                    );
                    assert_eq!(budget.snapshot().pending, 0.0);
                    assert!(matches!(
                        budget.admit(),
                        Err(crate::error::Error::Session(_))
                    ));
                }
            }
        }
    });
}

#[test]
fn acknowledged_audio_bills_survive_dropping_before_the_terminator() {
    futures::executor::block_on(async {
        for ending in [
            AudioSourceEnd::Error,
            AudioSourceEnd::Eof,
            AudioSourceEnd::Pending,
        ] {
            for evidence in [AudioEvidence::Complete, AudioEvidence::Zero] {
                let observation = Arc::new(SourceObservation::default());
                let budget = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
                let provider = crate::provider::guard_with_cost_budget(
                    Arc::new(AudioSourceProvider {
                        ending,
                        evidence,
                        observation: Arc::clone(&observation),
                    }),
                    budget.clone(),
                );
                let mut output = provider
                    .stream(
                        &crate::provider::Context::default(),
                        &crate::provider::StreamOptions::default(),
                    )
                    .await
                    .unwrap();
                let mut saw_end = false;
                for _ in 0..8 {
                    if matches!(
                        output.next().now_or_never().unwrap().unwrap().unwrap(),
                        StreamEvent::TextEnd { .. }
                    ) {
                        saw_end = true;
                        break;
                    }
                }
                assert!(saw_end);
                drop(output);
                assert!(observation.dropped.load(Ordering::SeqCst));
                let expected = if matches!(evidence, AudioEvidence::Zero) {
                    0.0
                } else {
                    0.032
                };
                assert!((budget.spent() - expected).abs() < 1e-12);
                assert_eq!(budget.snapshot().pending, 0.0);
                budget.admit().unwrap();
            }
        }
    });
}

struct WakeFlag(AtomicBool);

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn silent_burst(kind: &str) -> Vec<u8> {
    let mut bytes = wire_bytes([json!({"choices":[{"delta":{"role":"assistant"}}]})]);
    for _ in 0..MAX_READY_EVENTS_PER_YIELD * 2 {
        match kind {
            "heartbeat" => bytes.extend_from_slice(b"event: ping\ndata: {}\n\n"),
            "role" => bytes.extend(wire_bytes([
                json!({"choices":[{"delta":{"role":"assistant"}}]}),
            ])),
            "usage" => bytes.extend(wire_bytes([usage_chunk()])),
            _ => panic!("unknown silent frame"),
        }
    }
    bytes.extend(wire_bytes([usage_chunk()]));
    bytes.extend_from_slice(b"data: [DONE]\n\n");
    bytes
}

#[test]
fn ready_heartbeat_role_and_usage_bursts_yield_and_wake_without_losing_usage() {
    for kind in ["heartbeat", "role", "usage"] {
        let (mut output, observation) = observed_wire([Ok(silent_burst(kind))], true);
        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut task = TaskContext::from_waker(&waker);
        assert!(matches!(
            output.as_mut().poll_next(&mut task),
            Poll::Ready(Some(Ok(StreamEvent::Start { .. })))
        ));
        assert!(
            output.as_mut().poll_next(&mut task).is_pending(),
            "{kind} burst must yield"
        );
        assert!(
            flag.0.load(Ordering::SeqCst),
            "{kind} must schedule its next poll"
        );
        assert!(!observation.dropped.load(Ordering::SeqCst));
        let mut done = false;
        for _ in 0..4 {
            if let Poll::Ready(Some(Ok(StreamEvent::Done { message, .. }))) =
                output.as_mut().poll_next(&mut task)
            {
                assert_eq!(message.usage.input, 7);
                assert_eq!(message.usage.cache_read, 5);
                assert_eq!(message.usage.total_tokens, 15);
                done = true;
                break;
            }
        }
        assert!(done, "{kind} burst did not resume after yielding");
        assert_retired(&mut output, &observation);
    }
}

#[test]
fn abort_can_win_when_a_ready_openai_metadata_burst_would_otherwise_starve_it() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let (mut output, observation) = observed_wire([Ok(silent_burst("usage"))], true);
    runtime.block_on(async {
        assert!(matches!(
            output.next().await,
            Some(Ok(StreamEvent::Start { .. }))
        ));
        let (abort, signal) = crate::agent::AbortHandle::new();
        let next = Box::pin(output.next());
        let cancel = Box::pin(async move {
            abort.abort();
            signal.wait().await;
        });
        match futures::future::select(next, cancel).await {
            futures::future::Either::Right(((), pending)) => drop(pending),
            futures::future::Either::Left(_) => panic!("metadata burst starved cancellation"),
        }
    });
    drop(output);
    assert!(observation.dropped.load(Ordering::SeqCst));
}
