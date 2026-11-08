use super::{AssistantMessage, ContentBlock, SseStream, StopReason, StreamEvent, StreamState};
use crate::provider::{Context, ModelCost, Provider, StreamOptions};
use futures::{FutureExt, Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

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

struct UsageWireProvider {
    response: Mutex<Option<Vec<std::io::Result<Vec<u8>>>>>,
    pending_at_end: bool,
    dropped: Arc<AtomicBool>,
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
        Some(ModelCost {
            input: 1_000_000.0,
            output: 2_000_000.0,
            cache_read: 500_000.0,
            ..ModelCost::default()
        })
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
            .with_usage_observer(options.usage_observer.clone())
            .into_stream(),
        ))
    }
}

fn guarded_usage_wire(
    chunks: Vec<std::io::Result<Vec<u8>>>,
    pending_at_end: bool,
) -> (Arc<dyn Provider>, crate::cost::CostBudget, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(UsageWireProvider {
        response: Mutex::new(Some(chunks)),
        pending_at_end,
        dropped: Arc::clone(&dropped),
    });
    let budget = crate::cost::CostBudget::new(None, Some(13.0), 0.0).unwrap();
    (
        crate::provider::guard_with_cost_budget(provider, budget.clone()),
        budget,
        dropped,
    )
}

fn cached_usage_wire() -> Vec<u8> {
    b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n".to_vec()
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
fn late_error_finish_reason_is_not_ignored_as_a_duplicate() {
    for reason in ["content_filter", "error"] {
        let mut state = state();
        feed(&mut state, json!({"content": "partial"}), Some("stop"));
        feed(&mut state, json!({}), Some(reason));
        assert_eq!(done_message(&mut state).stop_reason, StopReason::Error);
    }
}
