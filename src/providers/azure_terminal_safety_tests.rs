use super::{
    AssistantMessage, ContentBlock, MAX_READY_EVENTS_PER_YIELD, SseStream, StopReason, StreamEvent,
    StreamState,
};
use crate::provider::{Context, ModelCost, Provider, StreamOptions};
use futures::{FutureExt, Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Wake, Waker};

type TestState = StreamState<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>;

fn state() -> TestState {
    StreamState::new(
        SseStream::new(futures::stream::empty()),
        "deployment".to_string(),
        "azure-openai".to_string(),
        "azure-openai".to_string(),
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
fn malformed_tool_identities_reject_the_whole_wire_batch() {
    for (id, name, diagnostic) in [
        (None, Some("write"), "ID is empty"),
        (Some(""), Some("write"), "ID is empty"),
        (Some(" \t"), Some("write"), "ID is empty"),
        (Some("other"), None, "name is empty"),
        (Some("other"), Some(""), "name is empty"),
        (Some("other"), Some(" \n"), "name is empty"),
        (Some("call-2"), Some("write"), "ID is duplicated"),
    ] {
        for finish_reason in [Some("tool_calls"), Some("stop"), None] {
            let mut state = state();
            feed(&mut state, json!({"content": "partial explanation"}), None);
            feed(&mut state, tool_delta(2, r#"{"path":"kept"}"#), None);
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
            assert!(!error.contains("call-2"));
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
    feed(
        &mut state,
        json!({"tool_calls": [{"index": 4, "id": "call-", "function": {"name": "re"}}]}),
        None,
    );
    feed(
        &mut state,
        json!({"tool_calls": [{
            "index": 4,
            "id": "four",
            "function": {"name": "ad", "arguments": r#""path":"complete"}"#}
        }]}),
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
        json!({"tool_calls": [{"index": 1, "id": "", "function": {"name": "read", "arguments": "{}"}}]}),
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
fn trailing_cached_usage_keeps_exclusive_input_buckets_and_context_total() {
    let mut state = state();
    feed(&mut state, json!({"content": "answer"}), Some("stop"));
    state
        .process_event(
            r#"{"choices":[],"usage":{"prompt_tokens":2000,"completion_tokens":100,"total_tokens":2100,"prompt_tokens_details":{"cached_tokens":1500}}}"#,
        )
        .unwrap();
    let message = done_message(&mut state);
    assert_eq!(message.usage.input, 500);
    assert_eq!(message.usage.cache_read, 1500);
    assert_eq!(message.usage.output, 100);
    assert_eq!(message.usage.total_tokens, 2100);
    assert_eq!(message.usage.input + message.usage.cache_read, 2000);
}

#[test]
fn missing_or_null_cache_details_do_not_reuse_an_earlier_snapshot() {
    for details in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"cached_tokens": null})),
        Some(json!({"cached_tokens": 0})),
    ] {
        let mut state = state();
        state
            .process_event(
                r#"{"choices":[],"usage":{"prompt_tokens":2000,"completion_tokens":100,"total_tokens":2100,"prompt_tokens_details":{"cached_tokens":1500}}}"#,
            )
            .unwrap();
        let mut usage = json!({"prompt_tokens":2000,"completion_tokens":100,"total_tokens":2100});
        if let Some(details) = details {
            usage["prompt_tokens_details"] = details;
        }
        state
            .process_event(&json!({"choices": [], "usage": usage}).to_string())
            .unwrap();
        let message = done_message(&mut state);
        assert_eq!(message.usage.input, 2000);
        assert_eq!(message.usage.cache_read, 0);
        assert_eq!(message.usage.total_tokens, 2100);
    }
}

#[test]
fn oversized_cached_counts_cannot_underflow_input() {
    let mut state = state();
    state
        .process_event(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":250}}}"#,
        )
        .unwrap();
    let message = done_message(&mut state);
    assert_eq!(message.usage.input, 0);
    assert_eq!(message.usage.cache_read, 250);
    assert_eq!(message.usage.total_tokens, 105);
}

#[test]
fn legacy_null_audio_and_optional_completion_counts_remain_compatible() {
    for completion in [None, Some(Value::Null), Some(json!(0))] {
        let mut usage = json!({
            "prompt_tokens": 100,
            "total_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": null, "audio_tokens": null},
            "completion_tokens_details": {"audio_tokens": null},
        });
        if let Some(completion) = completion {
            usage["completion_tokens"] = completion;
        }
        let mut state = state().with_model_cost(Some(usage_rates()));
        state
            .process_event(&json!({"choices": [], "usage": usage}).to_string())
            .unwrap();
        let mut message = done_message(&mut state);
        usage_rates().price_usage(&mut message.usage).unwrap();
        assert_eq!(message.usage.input, 100);
        assert_eq!(message.usage.output, 0);
        assert_eq!(message.usage.cache_read, 0);
        assert_eq!(message.usage.cache_write, 0);
        assert_eq!(message.usage.total_tokens, 100);
        assert_eq!(message.usage.cost.total, 100.0);
    }
}

struct UsageSource {
    chunks: VecDeque<std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
    dropped: Arc<AtomicBool>,
}

impl Stream for UsageSource {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(chunk) = self.chunks.pop_front() {
            Poll::Ready(Some(chunk))
        } else if self.pending_at_end {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

impl Drop for UsageSource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

type WireOutput = Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>;

fn wire_output(
    chunks: impl IntoIterator<Item = std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
) -> (WireOutput, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let source = UsageSource {
        chunks: chunks.into_iter().collect(),
        pending_at_end,
        dropped: Arc::clone(&dropped),
    };
    let output = StreamState::new(
        SseStream::new(source),
        "deployment".to_string(),
        "azure-openai".to_string(),
        "azure-openai".to_string(),
    )
    .into_stream();
    (Box::pin(output), dropped)
}

fn wire_bytes(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut bytes = String::new();
    for event in events {
        write!(bytes, "data: {event}\n\n").unwrap();
    }
    bytes.into_bytes()
}

fn assert_retired(output: &mut WireOutput, dropped: &AtomicBool) {
    // The terminal item, not a subsequent EOF poll or the caller's Drop,
    // releases transport ownership even when the wire would stay pending.
    assert!(dropped.load(Ordering::SeqCst), "terminal retained source");
    for _ in 0..3 {
        assert!(output.next().now_or_never().unwrap().is_none());
    }
}

#[test]
fn wire_terminal_retires_source_after_trailing_usage_and_block_ends() {
    for reason in ["stop", "error"] {
        let mut bytes = wire_bytes([
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":reason}]}),
            json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}),
        ]);
        bytes.extend_from_slice(b"data: [DONE]\n\ndata: {invalid unread tail}\n\n");
        let (mut output, dropped) = wire_output([Ok(bytes)], true);
        let mut text_ends = 0;
        let mut terminal = false;
        for _ in 0..8 {
            match output.next().now_or_never().unwrap().unwrap().unwrap() {
                StreamEvent::TextEnd { content, .. } => {
                    assert_eq!(content, "answer");
                    text_ends += 1;
                    assert!(!dropped.load(Ordering::SeqCst));
                }
                StreamEvent::Done {
                    reason: stop,
                    message,
                } => {
                    assert_eq!(
                        stop,
                        if reason == "stop" {
                            StopReason::Stop
                        } else {
                            StopReason::Error
                        }
                    );
                    assert_eq!(message.usage.input, 12);
                    assert_eq!(message.usage.output, 3);
                    assert_eq!(message.usage.total_tokens, 15);
                    terminal = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(terminal);
        assert_eq!(text_ends, 1);
        assert_retired(&mut output, &dropped);
    }
}

#[test]
fn wire_fatal_parse_and_transport_errors_retire_before_delivery() {
    for input in [
        Ok(b"data: {invalid json}\n\n".to_vec()),
        Ok(b"data: {\"choices\":\"invalid shape\"}\n\n".to_vec()),
        Ok(b"data: \xff\n\n".to_vec()),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid",
        )),
    ] {
        let (mut output, dropped) = wire_output([input, Ok(b"data: [DONE]\n\n".to_vec())], true);
        assert!(output.next().now_or_never().unwrap().unwrap().is_err());
        assert_retired(&mut output, &dropped);
    }
}

#[test]
fn wire_unexpected_eof_preserves_partial_text_and_fuses_after_one_error() {
    let (mut output, dropped) = wire_output(
        [Ok(wire_bytes([
            json!({"choices":[{"delta":{"content":"partial"}}]}),
        ]))],
        false,
    );
    let mut text = String::new();
    let mut failed = false;
    for _ in 0..8 {
        match output.next().now_or_never().unwrap().unwrap() {
            Ok(StreamEvent::TextDelta { delta, .. }) => text.push_str(&delta),
            Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. }) => {
                panic!("EOF must not invent a completed response");
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
    assert_eq!(text, "partial");
    assert_retired(&mut output, &dropped);
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
    let usage = json!({"choices":[],"usage":{
        "prompt_tokens":12,"completion_tokens":3,"total_tokens":15
    }});
    for _ in 0..MAX_READY_EVENTS_PER_YIELD * 2 {
        match kind {
            "heartbeat" => bytes.extend_from_slice(b"event: ping\ndata: {}\n\n"),
            "role" => bytes.extend(wire_bytes([
                json!({"choices":[{"delta":{"role":"assistant"}}]}),
            ])),
            "usage" => bytes.extend(wire_bytes([usage.clone()])),
            _ => panic!("unknown silent frame"),
        }
    }
    bytes.extend(wire_bytes([usage]));
    bytes.extend_from_slice(b"data: [DONE]\n\n");
    bytes
}

#[test]
fn ready_metadata_bursts_yield_and_wake_without_losing_usage() {
    for kind in ["heartbeat", "role", "usage"] {
        let (mut output, dropped) = wire_output([Ok(silent_burst(kind))], true);
        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut task = TaskContext::from_waker(&waker);
        assert!(matches!(
            output.as_mut().poll_next(&mut task),
            Poll::Ready(Some(Ok(StreamEvent::Start { .. })))
        ));
        assert!(output.as_mut().poll_next(&mut task).is_pending(), "{kind}");
        assert!(
            flag.0.load(Ordering::SeqCst),
            "{kind} must arrange a repoll"
        );
        assert!(!dropped.load(Ordering::SeqCst));
        let mut terminal = false;
        for _ in 0..6 {
            if let Poll::Ready(Some(Ok(StreamEvent::Done { message, .. }))) =
                output.as_mut().poll_next(&mut task)
            {
                assert_eq!(message.usage.total_tokens, 15);
                terminal = true;
                break;
            }
        }
        assert!(terminal, "{kind} must resume after yielding");
        assert_retired(&mut output, &dropped);
    }
}

#[test]
fn draining_ready_text_and_queued_tool_events_still_yields() {
    for tools in [false, true] {
        let count = MAX_READY_EVENTS_PER_YIELD * 2;
        let mut bytes = if tools {
            let calls = (0..count)
                .map(|index| {
                    json!({
                        "index":index,"id":format!("call-{index}"),
                        "function":{"name":"read","arguments":"{}"}
                    })
                })
                .collect::<Vec<_>>();
            wire_bytes([json!({"choices":[{
                "delta":{"tool_calls":calls},"finish_reason":"tool_calls"
            }]})])
        } else {
            wire_bytes((0..count).map(|_| json!({"choices":[{"delta":{"content":"x"}}]})))
        };
        bytes.extend_from_slice(b"data: [DONE]\n\n");
        let (mut output, dropped) = wire_output([Ok(bytes)], true);
        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut task = TaskContext::from_waker(&waker);
        let mut drain = Box::pin(output.by_ref().collect::<Vec<_>>());
        assert!(drain.as_mut().poll(&mut task).is_pending());
        assert!(flag.0.load(Ordering::SeqCst));
        let mut events = None;
        for _ in 0..16 {
            if let Poll::Ready(completed) = drain.as_mut().poll(&mut task) {
                events = Some(completed);
                break;
            }
        }
        drop(drain);
        let events = events.expect("bounded visible drain must complete");
        let events = events
            .into_iter()
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
        if tools {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                    .count(),
                count
            );
        } else {
            let text = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(text, "x".repeat(count));
        }
        assert_retired(&mut output, &dropped);
    }
}

#[test]
fn cancellation_can_win_while_a_ready_metadata_stream_is_drained() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let (mut output, dropped) = wire_output([Ok(silent_burst("usage"))], true);
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
            futures::future::Either::Left(_) => panic!("metadata stream starved cancellation"),
        }
    });
    drop(output);
    assert!(dropped.load(Ordering::SeqCst));
}

struct UsageWireProvider {
    response: Mutex<Option<Vec<std::io::Result<Vec<u8>>>>>,
    pending_at_end: bool,
    dropped: Arc<AtomicBool>,
    rates: Option<ModelCost>,
}

#[async_trait::async_trait]
impl Provider for UsageWireProvider {
    fn name(&self) -> &str {
        "azure"
    }

    fn api(&self) -> &str {
        "azure-openai"
    }

    fn model_id(&self) -> &str {
        "billing-fixture"
    }

    fn model_cost(&self) -> Option<ModelCost> {
        self.rates.clone()
    }

    async fn stream(
        &self,
        _: &Context<'_>,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>
    {
        let source = UsageSource {
            chunks: self
                .response
                .lock()
                .unwrap()
                .take()
                .expect("one wire request")
                .into(),
            pending_at_end: self.pending_at_end,
            dropped: Arc::clone(&self.dropped),
        };
        Ok(Box::pin(
            StreamState::new(
                SseStream::new(source),
                self.model_id().to_string(),
                self.api().to_string(),
                self.name().to_string(),
            )
            .with_model_cost(self.rates.clone())
            .with_usage_observer(options.usage_observer.clone())
            .into_stream(),
        ))
    }
}

fn guarded_usage_wire(
    chunks: Vec<std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
) -> (Arc<dyn Provider>, crate::cost::CostBudget, Arc<AtomicBool>) {
    guarded_usage_wire_with_rates(chunks, pending_at_end, Some(usage_rates()), 13.0)
}

fn usage_rates() -> ModelCost {
    ModelCost {
        input: 1_000_000.0,
        output: 2_000_000.0,
        cache_read: 500_000.0,
        cache_write: 3_000_000.0,
        ..ModelCost::default()
    }
}

fn guarded_usage_wire_with_rates(
    chunks: Vec<std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
    rates: Option<ModelCost>,
    cap: f64,
) -> (Arc<dyn Provider>, crate::cost::CostBudget, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(UsageWireProvider {
        response: Mutex::new(Some(chunks)),
        pending_at_end,
        dropped: Arc::clone(&dropped),
        rates,
    });
    let budget = crate::cost::CostBudget::new(None, Some(cap), 0.0).unwrap();
    (
        crate::provider::guard_with_cost_budget(provider, budget.clone()),
        budget,
        dropped,
    )
}

fn cached_usage_wire() -> Vec<u8> {
    b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n".to_vec()
}

fn usage_wire(usage: &str) -> Vec<u8> {
    // Do not round-trip through Value: duplicate fields are billing evidence.
    format!("data: {{\"choices\":[],\"usage\":{usage}}}\n\n").into_bytes()
}

#[test]
fn usage_only_pending_stream_accounts_before_any_event_and_survives_cancellation() {
    futures::executor::block_on(async {
        let (provider, budget, dropped) = guarded_usage_wire(vec![Ok(cached_usage_wire())], true);
        let mut events = provider
            .stream(&Context::default(), &StreamOptions::default())
            .await
            .unwrap();
        assert!(events.next().now_or_never().is_none());
        assert_eq!(budget.snapshot().pending, 14.0);
        assert_eq!(budget.spent(), 0.0);
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(matches!(
            provider
                .stream(&Context::default(), &StreamOptions::default())
                .await,
            Err(crate::error::Error::CostCap { .. })
        ));

        drop(events);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(budget.spent(), 14.0);
        assert_eq!(budget.snapshot().pending, 0.0);
    });
}

#[test]
fn duplicate_trailing_usage_survives_success_and_stream_failures_without_double_billing() {
    futures::executor::block_on(async {
        for outcome in ["done", "transport", "parser", "eof"] {
            let mut chunks = vec![
                Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n".to_vec()),
                Ok(cached_usage_wire()),
                Ok(cached_usage_wire()),
            ];
            match outcome {
                "done" => chunks.push(Ok(b"data: [DONE]\n\n".to_vec())),
                "transport" => chunks.push(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "fixture reset",
                ))),
                "parser" => chunks.push(Ok(b"data: {broken json}\n\n".to_vec())),
                _ => {}
            }
            let (provider, budget, dropped) = guarded_usage_wire(chunks, false);
            let mut events = provider
                .stream(&Context::default(), &StreamOptions::default())
                .await
                .unwrap();
            let mut done = 0;
            let mut errors = 0;
            for _ in 0..12 {
                let Some(event) = events.next().now_or_never().expect("finite ready source") else {
                    break;
                };
                match event {
                    Ok(StreamEvent::Done { message, .. }) => {
                        done += 1;
                        assert_eq!(message.usage.input, 4);
                        assert_eq!(message.usage.cache_read, 8);
                        assert_eq!(message.usage.output, 3);
                        assert_eq!(message.usage.total_tokens, 15);
                        assert_eq!(message.usage.cost.total, 14.0);
                    }
                    Err(_) => errors += 1,
                    _ => {}
                }
            }
            assert_eq!(done, usize::from(outcome == "done"), "{outcome}");
            assert_eq!(errors, usize::from(outcome != "done"), "{outcome}");
            drop(events);
            assert!(dropped.load(Ordering::SeqCst), "{outcome}");
            assert_eq!(budget.spent(), 14.0, "{outcome}");
            assert_eq!(budget.snapshot().pending, 0.0, "{outcome}");
        }
    });
}

#[test]
fn cache_writes_are_exclusive_and_acknowledged_before_cancellation() {
    futures::executor::block_on(async {
        for lifetimes in [
            None,
            Some(json!({"ttl_30m_tokens": 30, "ttl_12h_tokens": 0})),
        ] {
            let mut usage = json!({
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "total_tokens": 110,
                "prompt_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 30},
            });
            if let Some(lifetimes) = lifetimes {
                usage["prompt_tokens_details"]["cache_write_tokens_details"] = lifetimes;
            }
            for complete in [false, true] {
                let frame = usage_wire(&usage.to_string());
                let mut chunks = vec![Ok(frame.clone()), Ok(frame)];
                if complete {
                    chunks.push(Ok(b"data: [DONE]\n\n".to_vec()));
                }
                let (provider, budget, dropped) =
                    guarded_usage_wire_with_rates(chunks, !complete, Some(usage_rates()), 149.0);
                let mut events = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                if complete {
                    let mut done = 0;
                    while let Some(event) = events.next().await {
                        if let StreamEvent::Done { message, .. } = event.unwrap() {
                            done += 1;
                            assert_eq!(message.usage.input, 10);
                            assert_eq!(message.usage.cache_read, 60);
                            assert_eq!(message.usage.cache_write, 30);
                            assert_eq!(message.usage.output, 10);
                            assert_eq!(message.usage.total_tokens, 110);
                            assert_eq!(message.usage.cost.input, 10.0);
                            assert_eq!(message.usage.cost.cache_read, 30.0);
                            assert_eq!(message.usage.cost.cache_write, 90.0);
                            assert_eq!(message.usage.cost.output, 20.0);
                            assert_eq!(message.usage.cost.total, 150.0);
                        }
                    }
                    assert_eq!(done, 1);
                } else {
                    assert!(events.next().now_or_never().is_none());
                    assert_eq!(budget.snapshot().pending, 150.0);
                    assert_eq!(budget.spent(), 0.0);
                }
                assert!(matches!(
                    provider
                        .stream(&Context::default(), &StreamOptions::default())
                        .await,
                    Err(crate::error::Error::CostCap { .. })
                ));
                drop(events);
                assert!(dropped.load(Ordering::SeqCst));
                assert_eq!(budget.snapshot().pending, 0.0);
                assert_eq!(budget.spent(), 150.0);
            }
        }
    });
}

#[test]
fn invalid_or_unpriceable_usage_quarantines_the_budget_without_losing_prior_spend() {
    let invalid = [
        "[0]",
        "[7]",
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":[8]}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"completion_tokens_details":[0]}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":[1,0]}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":8,"cache_write_tokens":3}}"#,
        r#"{"prompt_tokens":18446744073709551615,"completion_tokens":0,"total_tokens":18446744073709551615,"prompt_tokens_details":{"cached_tokens":18446744073709551615,"cache_write_tokens":1}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":10}"#,
        r#"{"prompt_tokens":18446744073709551615,"completion_tokens":1,"total_tokens":0}"#,
        r#"{"prompt_tokens":10,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":null}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":-1}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1.5}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":"1"}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":0,"ttl_12h_tokens":0}}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":18446744073709551615,"ttl_12h_tokens":2}}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":1}}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":1,"ttl_12h_tokens":0,"ttl_24h_tokens":1}}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":0,"ttl_12h_tokens":1}}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"prompt_tokens_details":{"audio_tokens":1}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11,"completion_tokens_details":{"audio_tokens":1}}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":null}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":-1}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":"0"}"#,
        r#"{"prompt_tokens":10,"prompt_tokens":1,"completion_tokens":0,"total_tokens":10}"#,
        r#"{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":10,"cost":0}"#,
    ];
    futures::executor::block_on(async {
        for invalid in invalid {
            let acknowledged = b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n".to_vec();
            let (provider, budget, dropped) = guarded_usage_wire_with_rates(
                vec![
                    Ok(acknowledged),
                    Ok(usage_wire(invalid)),
                    Ok(b"data: [DONE]\n\n".to_vec()),
                ],
                false,
                Some(usage_rates()),
                100.0,
            );
            let mut events = provider
                .stream(&Context::default(), &StreamOptions::default())
                .await
                .unwrap();
            assert!(matches!(
                events.next().await,
                Some(Ok(StreamEvent::Start { .. }))
            ));
            assert_eq!(budget.snapshot().pending, 14.0);
            assert!(
                matches!(
                    events.next().await,
                    Some(Err(crate::error::Error::Session(_)))
                ),
                "accepted {invalid}"
            );
            assert!(events.next().await.is_none());
            drop(events);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(budget.spent(), 14.0, "lost prior bill for {invalid}");
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                budget.admit(),
                Err(crate::error::Error::Session(_))
            ));
            assert!(matches!(
                provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await,
                Err(crate::error::Error::Session(_))
            ));
        }
    });
}

#[test]
fn cache_writes_need_a_positive_configured_rate_or_an_explicit_bill() {
    futures::executor::block_on(async {
        for rates in [
            None,
            Some(ModelCost::default()),
            Some(ModelCost {
                cache_write: 0.0,
                ..usage_rates()
            }),
            Some(ModelCost {
                cache_write: -1.0,
                ..usage_rates()
            }),
            Some(ModelCost {
                cache_write: f64::INFINITY,
                ..usage_rates()
            }),
            Some(ModelCost {
                cache_write: f64::NAN,
                ..usage_rates()
            }),
        ] {
            for bill in [None, Some(0.0), Some(5.0)] {
                let mut usage = json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 10,
                    "total_tokens": 110,
                    "prompt_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 30},
                });
                if let Some(bill) = bill {
                    usage["cost"] = json!(bill);
                }
                let (provider, budget, dropped) = guarded_usage_wire_with_rates(
                    vec![
                        Ok(usage_wire(&usage.to_string())),
                        Ok(b"data: [DONE]\n\n".to_vec()),
                    ],
                    false,
                    rates.clone(),
                    100.0,
                );
                let mut events = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                let mut done = 0;
                let mut errors = 0;
                while let Some(event) = events.next().await {
                    match event {
                        Ok(StreamEvent::Done { message, .. }) => {
                            done += 1;
                            assert_eq!(message.usage.cost.total, bill.unwrap());
                            assert!(matches!(
                                message.usage.billing,
                                Some(crate::model::BillingUsage::ReportedTotal { .. })
                            ));
                        }
                        Ok(StreamEvent::Start { .. }) => {}
                        Err(crate::error::Error::Session(_)) => errors += 1,
                        event => panic!("unexpected event: {event:?}"),
                    }
                }
                assert_eq!(done, usize::from(bill.is_some()));
                assert_eq!(errors, usize::from(bill.is_none()));
                drop(events);
                assert!(dropped.load(Ordering::SeqCst));
                assert_eq!(budget.spent(), bill.unwrap_or(0.0));
                assert_eq!(budget.snapshot().pending, 0.0);
                assert_eq!(budget.admit().is_ok(), bill.is_some());
            }
        }
    });
}

#[test]
fn reported_bills_price_usage_when_counts_or_lifetimes_cannot() {
    futures::executor::block_on(async {
        for mut usage in [
            json!({"prompt_tokens": 10, "completion_tokens": 0, "total_tokens": 10,
                "prompt_tokens_details": {"cached_tokens": 11}}),
            json!({"prompt_tokens": 10, "completion_tokens": 0, "total_tokens": 10,
                "prompt_tokens_details": {"cache_write_tokens": 5,
                    "cache_write_tokens_details": {"ttl_30m_tokens": 2, "ttl_12h_tokens": 3}}}),
            json!({"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11,
                "prompt_tokens_details": {"audio_tokens": 2},
                "completion_tokens_details": {"audio_tokens": 1}}),
            json!({"prompt_tokens": "unknown"}),
        ] {
            for bill in [0.0, 7.0] {
                usage["cost"] = json!(bill);
                let (provider, budget, _) = guarded_usage_wire_with_rates(
                    vec![
                        Ok(usage_wire(&usage.to_string())),
                        Ok(b"data: [DONE]\n\n".to_vec()),
                    ],
                    false,
                    Some(usage_rates()),
                    6.0,
                );
                let mut events = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                let mut done = 0;
                while let Some(event) = events.next().await {
                    if let StreamEvent::Done { message, .. } = event.unwrap() {
                        done += 1;
                        assert_eq!(message.usage.cost.total, bill);
                        assert!(matches!(
                            message.usage.billing,
                            Some(crate::model::BillingUsage::ReportedTotal { total }) if total == bill
                        ));
                    }
                }
                assert_eq!(done, 1);
                assert_eq!(budget.spent(), bill);
                assert_eq!(budget.snapshot().pending, 0.0);
                assert_eq!(budget.admit().is_ok(), bill < 6.0);
            }
        }
    });
}

#[test]
fn explicit_zero_bill_refunds_pending_spend_but_unknown_price_counts_do_not() {
    futures::executor::block_on(async {
        for rates in [None, Some(usage_rates())] {
            for bill in [None, Some(0.0)] {
                for complete in [false, true] {
                    let first = b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0,\"total_tokens\":10,\"cost\":20}}\n\n".to_vec();
                    let mut usage = json!({
                        "prompt_tokens": 10,
                        "completion_tokens": 0,
                        "total_tokens": 10,
                    });
                    if let Some(bill) = bill {
                        usage["cost"] = json!(bill);
                    }
                    let mut chunks = vec![Ok(first), Ok(usage_wire(&usage.to_string()))];
                    if complete {
                        chunks.push(Ok(b"data: [DONE]\n\n".to_vec()));
                    }
                    let (provider, budget, dropped) =
                        guarded_usage_wire_with_rates(chunks, !complete, rates.clone(), 15.0);
                    let mut events = provider
                        .stream(&Context::default(), &StreamOptions::default())
                        .await
                        .unwrap();
                    assert!(matches!(
                        events.next().await,
                        Some(Ok(StreamEvent::Start { .. }))
                    ));
                    assert_eq!(budget.snapshot().pending, 20.0);
                    assert!(matches!(
                        budget.admit(),
                        Err(crate::error::Error::CostCap { .. })
                    ));
                    let expected = bill.unwrap_or(if rates.is_some() { 10.0 } else { 20.0 });
                    if complete {
                        assert!(matches!(
                            events.next().await,
                            Some(Ok(StreamEvent::Done { .. }))
                        ));
                        assert!(events.next().await.is_none());
                    } else {
                        assert!(events.next().now_or_never().is_none());
                        assert_eq!(budget.snapshot().pending, expected);
                    }
                    assert_eq!(budget.admit().is_ok(), expected < 15.0);
                    drop(events);
                    assert!(dropped.load(Ordering::SeqCst));
                    assert_eq!(budget.spent(), expected);
                    assert_eq!(budget.snapshot().pending, 0.0);
                }
            }
        }
    });
}

#[test]
fn factory_passes_configured_write_rates_to_native_usage_normalization() {
    const CHILD_ROOT: &str = "PI_AZURE_FACTORY_USAGE_CHILD";
    const COMPLETE: &str = "AZURE_FACTORY_USAGE_COMPLETE";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        check_factory_write_usage(&std::path::PathBuf::from(root));
        eprintln!("{COMPLETE}");
        return;
    }

    // Isolate VCR and Azure environment settings from concurrent unit tests.
    // Playback mode cannot fall through to an actual Azure network request.
    let dir = tempfile::tempdir().expect("factory usage fixture");
    write_factory_usage_fixture(dir.path());
    let log_path = dir.path().join("child.log");
    let log = std::fs::File::create(&log_path).expect("child diagnostics");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "providers::azure::terminal_safety_tests::factory_passes_configured_write_rates_to_native_usage_normalization",
            "--nocapture",
        ])
        .env(CHILD_ROOT, dir.path())
        .env("VCR_MODE", "playback")
        .env("VCR_CASSETTE_DIR", dir.path())
        .env("PI_VCR_TEST_NAME", "azure_factory_write_usage")
        .env("VCR_CHUNK_DELAY_MS", "0")
        .env("PI_CODING_AGENT_DIR", dir.path().join("global"))
        .env("AZURE_OPENAI_RESOURCE", "contoso")
        .env("AZURE_OPENAI_API_VERSION", "2024-12-01-preview")
        .env_remove("AZURE_OPENAI_DEPLOYMENT")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().expect("clone child diagnostics"))
        .stderr(log)
        .spawn()
        .expect("isolated Azure factory regression");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(format!("polling child failed: {error}")),
            Ok(None) if std::time::Instant::now() >= deadline => {
                break Err("child exceeded its 30-second deadline".to_string());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let trace = std::fs::read_to_string(&log_path).expect("read child diagnostics");
    let status = result.unwrap_or_else(|reason| panic!("{reason}\n{trace}"));
    assert!(
        status.success(),
        "Azure factory regression failed:\n{trace}"
    );
    assert!(
        trace.contains(COMPLETE),
        "child did not execute the test:\n{trace}"
    );
}

fn write_factory_usage_fixture(root: &std::path::Path) {
    let mut models = Vec::new();
    let mut interactions = Vec::new();
    for id in ["configured", "omitted", "zero-rate", "explicit-free"] {
        let mut cost = json!({"input": 1_000_000, "output": 2_000_000, "cacheRead": 500_000});
        if id == "configured" {
            cost["cacheWrite"] = json!(3_000_000);
        } else if id != "omitted" {
            cost["cacheWrite"] = json!(0);
        }
        models.push(json!({"id": id, "cost": cost}));
        let mut usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "total_tokens": 110,
            "prompt_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 30},
        });
        if id == "explicit-free" {
            usage["cost"] = json!(0);
        }
        interactions.push(json!({
            "request": {
                "method": "POST",
                "url": format!("https://contoso.openai.azure.com/openai/deployments/{id}/chat/completions?api-version=2024-12-01-preview"),
                "headers": [],
            },
            "response": {
                "status": 200,
                "headers": [["content-type", "text/event-stream"]],
                "body_chunks": [
                    String::from_utf8(usage_wire(&usage.to_string())).unwrap(),
                    "data: [DONE]\n\n",
                ],
            },
        }));
    }
    let config = json!({"providers": {"azure-openai": {
        "api": "azure-openai",
        "baseUrl": "https://contoso.openai.azure.com",
        "models": models,
    }}});
    std::fs::write(
        root.join("models.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .expect("write models fixture");
    let cassette = json!({
        "version": "1.0",
        "test_name": "azure_factory_write_usage",
        "recorded_at": "2026-11-08T00:00:00Z",
        "interactions": interactions,
    });
    std::fs::write(
        root.join("azure_factory_write_usage.json"),
        serde_json::to_vec(&cassette).unwrap(),
    )
    .expect("write playback fixture");
}

fn check_factory_write_usage(root: &std::path::Path) {
    let registry = crate::models::ModelRegistry::load_with_credential_resolver(
        Some(root.join("models.json")),
        |_| None,
    );
    assert!(registry.error().is_none(), "{:?}", registry.error());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (id, bill) in [
            ("configured", Some(150.0)),
            ("omitted", None),
            ("zero-rate", None),
            ("explicit-free", Some(0.0)),
        ] {
            let entry = registry.find("azure-openai", id).expect("fixture model");
            let transport = crate::providers::create_provider(&entry, None).expect("Azure factory");
            let budget = crate::cost::CostBudget::new(None, Some(149.0), 0.0).unwrap();
            let provider = crate::provider::guard_with_cost_budget(transport, budget.clone());
            let context = Context::default();
            let options = StreamOptions {
                api_key: Some("azure-fixture-key".to_string()),
                ..StreamOptions::default()
            };
            let mut events = provider.stream(&context, &options).await.unwrap();
            let mut done = 0;
            let mut errors = 0;
            while let Some(event) = events.next().await {
                match event {
                    Ok(StreamEvent::Done { message, .. }) => {
                        done += 1;
                        assert_eq!(message.usage.input, 10, "{id}");
                        assert_eq!(message.usage.cache_write, 30, "{id}");
                        assert_eq!(message.usage.cache_read, 60, "{id}");
                        assert_eq!(message.usage.total_tokens, 110, "{id}");
                        assert_eq!(message.usage.cost.total, bill.unwrap(), "{id}");
                    }
                    Ok(StreamEvent::Start { .. }) => {}
                    Err(crate::error::Error::Session(_)) => errors += 1,
                    event => panic!("unexpected {id} event: {event:?}"),
                }
            }
            assert_eq!(done, usize::from(bill.is_some()), "{id}");
            assert_eq!(errors, usize::from(bill.is_none()), "{id}");
            assert_eq!(budget.spent(), bill.unwrap_or(0.0), "{id}");
            assert_eq!(budget.snapshot().pending, 0.0, "{id}");
            match bill {
                None => assert!(matches!(
                    budget.admit(),
                    Err(crate::error::Error::Session(_))
                )),
                Some(total) if total >= 149.0 => {
                    assert!(matches!(
                        budget.admit(),
                        Err(crate::error::Error::CostCap { .. })
                    ));
                }
                Some(_) => assert!(budget.admit().is_ok()),
            }
        }
    });
}

#[test]
fn duplicate_outer_usage_fields_quarantine_spend_even_when_one_is_null() {
    let duplicate = [
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":10},"usage":{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":0}}"#,
        r#"{"choices":[],"usage":null,"usage":{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":0}}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":0,"total_tokens":10,"cost":0},"usage":null}"#,
        r#"{"choices":[],"usage":null,"usage":null}"#,
    ];
    futures::executor::block_on(async {
        for frame in duplicate {
            let acknowledged = b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n".to_vec();
            let (provider, budget, dropped) = guarded_usage_wire_with_rates(
                vec![
                    Ok(acknowledged),
                    Ok(format!("data: {frame}\n\n").into_bytes()),
                    Ok(b"data: [DONE]\n\n".to_vec()),
                ],
                false,
                Some(usage_rates()),
                100.0,
            );
            let mut events = provider
                .stream(&Context::default(), &StreamOptions::default())
                .await
                .unwrap();
            assert!(matches!(
                events.next().await,
                Some(Ok(StreamEvent::Start { .. }))
            ));
            assert_eq!(budget.snapshot().pending, 14.0);
            assert!(
                matches!(
                    events.next().await,
                    Some(Err(crate::error::Error::Session(_)))
                ),
                "accepted duplicate envelope {frame}"
            );
            assert!(events.next().await.is_none());
            drop(events);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(budget.spent(), 14.0);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                budget.admit(),
                Err(crate::error::Error::Session(_))
            ));
        }
    });
}

#[test]
fn valid_bills_survive_malformed_choices_or_error_siblings() {
    futures::executor::block_on(async {
        for sibling in [
            r#""choices":null"#,
            r#""choices":{}"#,
            r#""choices":[{"delta":"invalid"}]"#,
            r#""error":"invalid""#,
        ] {
            for bill in [0.0, 7.0] {
                let acknowledged = b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0,\"total_tokens\":10,\"cost\":20}}\n\n".to_vec();
                let usage = json!({
                    "prompt_tokens": 10,
                    "completion_tokens": 0,
                    "total_tokens": 10,
                    "cost": bill,
                });
                let frame = format!("data: {{{sibling},\"usage\":{usage}}}\n\n").into_bytes();
                let (provider, budget, dropped) = guarded_usage_wire_with_rates(
                    vec![Ok(acknowledged), Ok(frame)],
                    false,
                    Some(usage_rates()),
                    15.0,
                );
                let mut events = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                assert!(matches!(
                    events.next().await,
                    Some(Ok(StreamEvent::Start { .. }))
                ));
                assert_eq!(budget.snapshot().pending, 20.0);
                let error = events.next().await.unwrap().unwrap_err();
                assert!(error.to_string().contains("JSON parse error"), "{error}");
                assert!(events.next().await.is_none());
                drop(events);
                assert!(dropped.load(Ordering::SeqCst));
                assert_eq!(budget.spent(), bill, "{sibling}");
                assert_eq!(budget.snapshot().pending, 0.0);
                assert!(budget.admit().is_ok());
            }
        }
    });
}

#[test]
fn missing_or_null_usage_envelopes_preserve_acknowledged_spend() {
    futures::executor::block_on(async {
        let acknowledged = b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0,\"total_tokens\":10,\"cost\":20}}\n\n".to_vec();
        let (provider, budget, dropped) = guarded_usage_wire_with_rates(
            vec![
                Ok(acknowledged),
                Ok(b"data: {\"choices\":[]}\n\n".to_vec()),
                Ok(b"data: {\"choices\":[],\"usage\":null}\n\n".to_vec()),
            ],
            true,
            Some(usage_rates()),
            15.0,
        );
        let mut events = provider
            .stream(&Context::default(), &StreamOptions::default())
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(Ok(StreamEvent::Start { .. }))
        ));
        assert!(events.next().now_or_never().is_none());
        assert_eq!(budget.snapshot().pending, 20.0);
        drop(events);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(budget.spent(), 20.0);
        assert_eq!(budget.snapshot().pending, 0.0);
        assert!(matches!(
            budget.admit(),
            Err(crate::error::Error::CostCap { .. })
        ));
    });
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
