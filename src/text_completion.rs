//! Bounded, terminal-validated completions for tool-free auxiliary model calls.
//!
//! Deltas are previews, not a successful response. Only a clean terminal
//! message may become a side answer or an advisor verdict. The terminal
//! message is authoritative, including for providers that emit no deltas.

use std::future::Future;
use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::error::{Error, Result};
use crate::model::{ContentBlock, StopReason, StreamEvent};

/// Auxiliary calls request only a few hundred tokens. Bound host-side output
/// too: a provider or extension is not obliged to honor max_tokens.
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_STREAM_EVENTS: usize = 65_536;

/// Drain one tool-free completion without retaining cumulative previews.
///
/// Reject missing terminal events, transport/provider errors, incomplete stop
/// reasons, tool calls, empty answers, and oversized replies. A caller owns
/// the wall-clock deadline around both stream creation and this drain.
pub(crate) async fn collect_text<S>(mut stream: S, max_bytes: usize) -> Result<String>
where
    S: Stream<Item = Result<StreamEvent>> + Unpin,
{
    let mut delta_bytes = 0usize;
    let mut event_count = 0usize;
    while let Some(event) = stream.next().await {
        event_count += 1;
        if event_count > MAX_STREAM_EVENTS {
            return Err(Error::api("text completion exceeded stream event budget"));
        }
        match event? {
            StreamEvent::TextDelta { delta, .. } => {
                delta_bytes = delta_bytes
                    .checked_add(delta.len())
                    .filter(|bytes| *bytes <= max_bytes)
                    .ok_or_else(|| Error::api("text completion exceeded output byte budget"))?;
            }
            StreamEvent::TextEnd { content, .. } if content.len() > max_bytes => {
                return Err(Error::api("text completion exceeded output byte budget"));
            }
            StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallDelta { .. }
            | StreamEvent::ToolCallEnd { .. } => {
                return Err(Error::api("tool-free text completion requested a tool call"));
            }
            StreamEvent::Error { error, .. } => {
                return Err(Error::api(error.error_message.unwrap_or_else(|| {
                    "provider reported a failed text completion".to_string()
                })));
            }
            StreamEvent::Done { reason, message } => {
                if let Some(error) = message.error_message {
                    return Err(Error::api(error));
                }
                if reason != StopReason::Stop || message.stop_reason != StopReason::Stop {
                    return Err(Error::api(format!(
                        "text completion did not finish cleanly (event: {reason:?}, message: {:?})",
                        message.stop_reason
                    )));
                }
                let mut answer = String::new();
                for block in message.content {
                    match block {
                        ContentBlock::Text(text) => {
                            if text.text.len() > max_bytes.saturating_sub(answer.len()) {
                                return Err(Error::api(
                                    "text completion exceeded output byte budget",
                                ));
                            }
                            answer.push_str(&text.text);
                        }
                        ContentBlock::ToolCall(_) => {
                            return Err(Error::api(
                                "tool-free text completion requested a tool call",
                            ));
                        }
                        _ => {}
                    }
                }
                if answer.trim().is_empty() {
                    return Err(Error::api("text completion returned empty reply"));
                }
                return Ok(answer);
            }
            _ => {}
        }
    }
    Err(Error::api("text completion stream ended without Done event"))
}

/// Bound a complete auxiliary request without spawning a detached task or
/// self-waking while the provider is idle. Dropping the losing future also
/// drops its stream; setup and response draining share the same deadline.
pub(crate) async fn with_timeout<F>(timeout: Duration, future: F) -> Option<F::Output>
where
    F: Future,
{
    if timeout.is_zero() {
        return None;
    }
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    let now = cx
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let timer = asupersync::time::sleep(now, timeout);
    futures::pin_mut!(timer, future);
    match futures::future::select(timer, future).await {
        futures::future::Either::Left(((), _)) => None,
        futures::future::Either::Right((output, _)) => Some(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, TextContent, ToolCall};

    fn done(text: &str) -> StreamEvent {
        StreamEvent::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        }
    }

    fn delta(text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            content_index: 0,
            delta: text.to_string(),
        }
    }

    async fn read(events: Vec<StreamEvent>, max_bytes: usize) -> Result<String> {
        collect_text(futures::stream::iter(events.into_iter().map(Ok)), max_bytes).await
    }

    #[test]
    fn terminal_only_provider_returns_its_answer() {
        asupersync::test_utils::run_test(|| async {
            assert_eq!(read(vec![done("complete")], 8).await.unwrap(), "complete");
        });
    }

    #[test]
    fn terminal_text_is_authoritative_and_not_duplicated() {
        asupersync::test_utils::run_test(|| async {
            let text = read(vec![delta("preview"), done("final answer")], 64)
                .await
                .unwrap();
            assert_eq!(text, "final answer");
        });
    }

    #[test]
    fn clean_done_does_not_wait_for_connection_close() {
        asupersync::test_utils::run_test(|| async {
            let stream = futures::stream::iter([Ok(done("done"))])
                .chain(futures::stream::pending());
            assert_eq!(collect_text(stream, 16).await.unwrap(), "done");
        });
    }

    #[test]
    fn partial_text_without_done_is_not_a_success() {
        asupersync::test_utils::run_test(|| async {
            let error = read(vec![delta("BLOCKER\nunfinished rationale")], 64)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("without Done"));
            assert!(read(Vec::new(), 64).await.is_err());
        });
    }

    #[test]
    fn provider_error_after_text_is_not_a_success() {
        asupersync::test_utils::run_test(|| async {
            let error = StreamEvent::Error {
                reason: StopReason::Error,
                error: AssistantMessage {
                    error_message: Some("upstream unavailable".to_string()),
                    stop_reason: StopReason::Error,
                    ..Default::default()
                },
            };
            assert!(read(vec![delta("partial"), error], 64)
                .await
                .unwrap_err()
                .to_string()
                .contains("upstream unavailable"));
        });
    }

    #[test]
    fn transport_error_is_preserved() {
        asupersync::test_utils::run_test(|| async {
            let stream = futures::stream::iter([
                Ok(delta("partial")),
                Err(Error::api("connection reset")),
            ]);
            assert!(collect_text(stream, 64)
                .await
                .unwrap_err()
                .to_string()
                .contains("connection reset"));
        });
    }

    #[test]
    fn incomplete_and_inconsistent_stop_reasons_are_rejected() {
        asupersync::test_utils::run_test(|| async {
            for stop in [
                StopReason::Length,
                StopReason::Error,
                StopReason::Aborted,
                StopReason::ToolUse,
                StopReason::PauseTurn,
            ] {
                let mut event = done("not complete");
                if let StreamEvent::Done { reason, .. } = &mut event {
                    *reason = stop;
                }
                assert!(read(vec![event], 64).await.is_err());
                let mut event = done("not complete");
                if let StreamEvent::Done { message, .. } = &mut event {
                    message.stop_reason = stop;
                }
                assert!(read(vec![event], 64).await.is_err());
            }
        });
    }

    #[test]
    fn clean_stop_with_error_payload_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let mut event = done("partial");
            if let StreamEvent::Done { message, .. } = &mut event {
                message.error_message = Some("failed".to_string());
            }
            assert!(read(vec![event], 64).await.is_err());
        });
    }

    #[test]
    fn empty_terminal_does_not_fall_back_to_stale_preview() {
        asupersync::test_utils::run_test(|| async {
            assert!(read(vec![delta("stale preview"), done(" \n")], 64)
                .await
                .is_err());
        });
    }

    #[test]
    fn streaming_and_terminal_tool_calls_are_rejected() {
        asupersync::test_utils::run_test(|| async {
            let start = StreamEvent::ToolCallStart {
                content_index: 0,
                id: "call-1".to_string(),
                name: "bash".to_string(),
            };
            assert!(read(vec![start, done("answer")], 64).await.is_err());
            let mut event = done("answer");
            if let StreamEvent::Done { message, .. } = &mut event {
                message.content.push(ContentBlock::ToolCall(ToolCall {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"command": "echo unexpected"}),
                    thought_signature: None,
                }));
            }
            assert!(read(vec![event], 64).await.is_err());
        });
    }

    #[test]
    fn output_budget_is_bytes_and_applies_to_deltas_and_terminal() {
        asupersync::test_utils::run_test(|| async {
            assert!(read(vec![delta("abcd"), delta("e"), done("ok")], 4)
                .await
                .is_err());
            assert!(read(vec![done("abcde")], 4).await.is_err());
            assert!(read(vec![done("éé")], 3).await.is_err());
            assert_eq!(read(vec![done("éé")], 4).await.unwrap(), "éé");
        });
    }

    #[test]
    fn terminal_budget_is_shared_across_text_blocks() {
        asupersync::test_utils::run_test(|| async {
            let mut event = done("abc");
            if let StreamEvent::Done { message, .. } = &mut event {
                message.content.push(ContentBlock::Text(TextContent::new("def")));
            }
            assert!(read(vec![event.clone()], 5).await.is_err());
            assert_eq!(read(vec![event], 6).await.unwrap(), "abcdef");
        });
    }

    #[test]
    fn empty_delta_flood_is_bounded() {
        asupersync::test_utils::run_test(|| async {
            let stream = futures::stream::iter((0..=MAX_STREAM_EVENTS).map(|_| Ok(delta(""))));
            assert!(collect_text(stream, 64)
                .await
                .unwrap_err()
                .to_string()
                .contains("event budget"));
        });
    }

    #[test]
    fn zero_deadline_never_polls_request() {
        asupersync::test_utils::run_test(|| async {
            use std::sync::atomic::{AtomicBool, Ordering};
            let polled = AtomicBool::new(false);
            let result = with_timeout(Duration::ZERO, async {
                polled.store(true, Ordering::SeqCst);
                42
            })
            .await;
            assert!(result.is_none());
            assert!(!polled.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn deadline_drops_pending_request() {
        asupersync::test_utils::run_test(|| async {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            struct OnDrop(Arc<AtomicBool>);
            impl Drop for OnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = OnDrop(Arc::clone(&dropped));
            let request = async move {
                let _guard = guard;
                futures::future::pending::<()>().await;
            };
            assert!(with_timeout(Duration::from_millis(10), request).await.is_none());
            assert!(dropped.load(Ordering::SeqCst));
        });
    }
}
