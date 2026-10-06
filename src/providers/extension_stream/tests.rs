use super::*;
use crate::model::{AssistantMessage, TextContent, ThinkingContent, ToolCall};
use serde_json::json;
use std::sync::Arc;

fn decoder() -> Decoder {
    Decoder::new("model".into(), "provider".into(), "api".into())
}

fn text_message(text: &str) -> Arc<AssistantMessage> {
    Arc::new(ExtensionStreamSimpleProvider::make_partial(
        "model", "provider", "api", text,
    ))
}

fn wire(event: AssistantMessageEvent) -> Value {
    serde_json::to_value(event).unwrap()
}

fn done(message: Arc<AssistantMessage>, reason: StopReason) -> Value {
    wire(AssistantMessageEvent::Done { reason, message })
}

fn tool_message(reason: StopReason) -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            arguments: json!({"path":"output.txt", "content":"complete"}),
            thought_signature: None,
        })],
        stop_reason: reason,
        ..(*text_message("")).clone()
    })
}

#[test]
fn structured_text_eof_is_not_promoted_to_success() {
    let mut decoder = decoder();
    decoder
        .push(wire(AssistantMessageEvent::Start {
            partial: text_message(""),
        }))
        .unwrap();
    let preview = decoder
        .push(wire(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "preview".into(),
            partial: text_message("preview"),
        }))
        .unwrap();
    assert!(matches!(&preview[0], StreamEvent::TextDelta { delta, .. } if delta == "preview"));
    assert!(
        decoder
            .finish()
            .unwrap_err()
            .to_string()
            .contains("PI_EXTENSION_STREAM_INCOMPLETE")
    );
    assert!(decoder.finished());
    assert!(decoder.finish().unwrap().is_empty());
}

#[test]
fn tool_call_end_is_still_only_a_preview_until_an_explicit_terminal() {
    let mut decoder = decoder();
    let partial = tool_message(StopReason::ToolUse);
    let ContentBlock::ToolCall(call) = &partial.content[0] else {
        unreachable!()
    };
    decoder
        .push(wire(AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: call.clone(),
            partial: Arc::clone(&partial),
        }))
        .unwrap();
    let error = decoder.finish().unwrap_err();
    assert!(error.to_string().contains("INCOMPLETE"));
    assert!(!error.to_string().contains("output.txt"));
}

#[test]
fn raw_text_iterator_has_one_empty_start_and_one_complete_terminal() {
    let mut decoder = decoder();
    let initial = decoder.push(json!("Hello ")).unwrap();
    let StreamEvent::Start { partial } = &initial[0] else {
        panic!("missing start")
    };
    assert!(matches!(&partial.content[0], ContentBlock::Text(text) if text.text.is_empty()));
    assert!(matches!(
        initial[1],
        StreamEvent::TextStart { content_index: 0 }
    ));
    assert!(matches!(&initial[2], StreamEvent::TextDelta { delta, .. } if delta == "Hello "));
    let next = decoder.push(json!("🦀")).unwrap();
    assert_eq!(next.len(), 1);
    let terminal = decoder.finish().unwrap();
    assert_eq!(terminal.len(), 2);
    assert!(matches!(&terminal[0], StreamEvent::TextEnd { content, .. } if content == "Hello 🦀"));
    let StreamEvent::Done { reason, message } = &terminal[1] else {
        panic!("missing done")
    };
    assert_eq!(*reason, StopReason::Stop);
    assert!(matches!(&message.content[0], ContentBlock::Text(text) if text.text == "Hello 🦀"));
    assert!(decoder.finish().unwrap().is_empty());
    assert!(decoder.push(json!("late")).is_err());
}

#[test]
fn empty_iterator_is_distinct_from_an_explicit_empty_text_chunk() {
    assert!(
        decoder()
            .finish()
            .unwrap_err()
            .to_string()
            .contains("INCOMPLETE")
    );
    let mut decoder = decoder();
    decoder.push(json!("")).unwrap();
    assert!(matches!(
        decoder.finish().unwrap()[1],
        StreamEvent::Done { .. }
    ));
}

#[test]
fn stream_modes_cannot_be_mixed_in_either_order() {
    for text_first in [false, true] {
        let mut decoder = decoder();
        let structured = wire(AssistantMessageEvent::Start {
            partial: text_message(""),
        });
        let (first, second) = if text_first {
            (json!("text"), structured)
        } else {
            (structured, json!("text"))
        };
        decoder.push(first).unwrap();
        assert!(
            decoder
                .push(second)
                .unwrap_err()
                .to_string()
                .contains("PROTOCOL")
        );
        assert!(decoder.finished());
        assert!(decoder.finish().unwrap().is_empty());
    }
}

#[test]
fn terminal_only_text_preserves_finish_reason_usage_and_signature() {
    for reason in [StopReason::Stop, StopReason::Length, StopReason::PauseTurn] {
        let mut message = (*text_message("authoritative")).clone();
        message.stop_reason = reason;
        message.usage.output = 17;
        if let ContentBlock::Text(text) = &mut message.content[0] {
            text.text_signature = Some("signature".into());
        }
        let before = serde_json::to_value(&message).unwrap();
        let mut decoder = decoder();
        let events = decoder.push(done(Arc::new(message), reason)).unwrap();
        let StreamEvent::Done {
            message,
            reason: actual,
        } = &events[0]
        else {
            panic!("missing done")
        };
        assert_eq!(*actual, reason);
        assert_eq!(serde_json::to_value(message).unwrap(), before);
        assert!(decoder.finished());
    }
}

#[test]
fn tool_execution_requires_a_valid_successful_terminal() {
    let mut stream_decoder = decoder();
    let terminal = stream_decoder
        .push(done(tool_message(StopReason::ToolUse), StopReason::ToolUse))
        .unwrap();
    assert!(
        matches!(&terminal[0], StreamEvent::Done { reason: StopReason::ToolUse, message }
        if matches!(&message.content[0], ContentBlock::ToolCall(call) if call.arguments["content"] == "complete"))
    );
    for reason in [
        StopReason::Length,
        StopReason::Refusal,
        StopReason::Error,
        StopReason::Aborted,
    ] {
        assert!(
            decoder().push(done(tool_message(reason), reason)).is_err(),
            "{reason:?}"
        );
    }
    let mut empty = (*text_message("not a tool call")).clone();
    empty.stop_reason = StopReason::ToolUse;
    assert!(
        decoder()
            .push(done(Arc::new(empty), StopReason::ToolUse))
            .is_err()
    );
}

#[test]
fn successful_stop_with_complete_tool_calls_normalizes_both_terminal_reasons() {
    for preview_first in [false, true] {
        let mut message = (*tool_message(StopReason::Stop)).clone();
        message.usage.output = 17;
        if let ContentBlock::ToolCall(call) = &mut message.content[0] {
            call.thought_signature = Some("tool-signature".into());
        }
        message
            .content
            .insert(0, ContentBlock::Text(TextContent::new("Writing the output")));
        message.content.push(ContentBlock::ToolCall(ToolCall {
            id: "call-2".into(),
            name: "read".into(),
            arguments: json!({"path": "input.txt"}),
            thought_signature: None,
        }));
        let mut expected = message.clone();
        expected.stop_reason = StopReason::ToolUse;
        let message = Arc::new(message);
        let mut decoder = decoder();
        if preview_first {
            let ContentBlock::ToolCall(call) = &message.content[1] else {
                unreachable!()
            };
            decoder
                .push(wire(AssistantMessageEvent::ToolCallEnd {
                    content_index: 1,
                    tool_call: call.clone(),
                    partial: Arc::clone(&message),
                }))
                .unwrap();
            assert!(!decoder.finished());
        }
        let terminal = decoder.push(done(message, StopReason::Stop)).unwrap();
        let [StreamEvent::Done { reason, message }] = terminal.as_slice() else {
            panic!("expected exactly one terminal event")
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(
            serde_json::to_value(message).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert!(decoder.finished());
        assert!(decoder.finish().unwrap().is_empty());
        assert!(decoder.push(json!("late")).is_err());
    }
}

#[test]
fn paused_server_tool_payload_is_preserved_for_replay_not_local_dispatch() {
    let message = tool_message(StopReason::PauseTurn);
    let before = serde_json::to_value(&message).unwrap();
    let events = decoder()
        .push(done(message, StopReason::PauseTurn))
        .unwrap();
    let StreamEvent::Done { reason, message } = &events[0] else {
        panic!("missing done")
    };
    assert_eq!(*reason, StopReason::PauseTurn);
    assert_eq!(serde_json::to_value(message).unwrap(), before);
}

#[test]
fn ambiguous_terminal_tool_arguments_and_duplicate_ids_are_rejected() {
    for reason in [StopReason::Stop, StopReason::ToolUse] {
        for problem in 0..5 {
            let mut message = (*tool_message(reason)).clone();
            if problem == 4 {
                message.content.push(message.content[0].clone());
            } else {
                let ContentBlock::ToolCall(call) = &mut message.content[0] else {
                    unreachable!()
                };
                match problem {
                    0 => call.id.clear(),
                    1 => call.name = "  ".into(),
                    2 => call.arguments = json!("not object arguments"),
                    _ => call.arguments = Value::Null,
                }
            }
            let mut decoder = decoder();
            assert!(decoder.push(done(Arc::new(message), reason)).is_err());
            assert!(decoder.finished());
            assert!(decoder.finish().unwrap().is_empty());
        }
    }
}

#[test]
fn failed_and_mismatched_done_payloads_never_become_success() {
    let mut failed = (*text_message("not successful")).clone();
    failed.error_message = Some("PRIVATE-CANARY".into());
    let error = decoder()
        .push(done(Arc::new(failed), StopReason::Stop))
        .unwrap_err();
    assert!(error.to_string().contains("PROTOCOL"));
    assert!(!error.to_string().contains("PRIVATE-CANARY"));
    assert!(
        decoder()
            .push(done(text_message("text"), StopReason::Length))
            .is_err()
    );
    for message_reason in [StopReason::ToolUse, StopReason::Error, StopReason::Aborted] {
        assert!(
            decoder()
                .push(done(tool_message(message_reason), StopReason::Stop))
                .is_err()
        );
    }
    let mut failed_tool = (*tool_message(StopReason::Stop)).clone();
    failed_tool.error_message = Some("PRIVATE-CANARY".into());
    let error = decoder()
        .push(done(Arc::new(failed_tool), StopReason::Stop))
        .unwrap_err();
    assert!(error.to_string().contains("PROTOCOL"));
    assert!(!error.to_string().contains("PRIVATE-CANARY"));
}

#[test]
fn explicit_error_and_abort_events_keep_their_failure_semantics() {
    for reason in [StopReason::Error, StopReason::Aborted] {
        let mut message = (*text_message("partial")).clone();
        message.stop_reason = reason;
        message.error_message = Some("provider failed".into());
        let mut decoder = decoder();
        let events = decoder
            .push(wire(AssistantMessageEvent::Error {
                reason,
                error: Arc::new(message),
            }))
            .unwrap();
        assert!(
            matches!(&events[0], StreamEvent::Error { reason: actual, .. } if *actual == reason)
        );
        assert!(decoder.finished());
        assert!(decoder.finish().unwrap().is_empty());
    }
    assert!(
        decoder()
            .push(wire(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error: text_message("inconsistent"),
            }))
            .is_err()
    );
}

#[test]
fn indexed_events_cannot_select_missing_or_wrong_type_blocks() {
    for index in [1, usize::MAX] {
        assert!(
            decoder()
                .push(wire(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: "x".into(),
                    partial: text_message("x"),
                }))
                .is_err()
        );
    }
    assert!(
        decoder()
            .push(wire(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: text_message("wrong type"),
            }))
            .is_err()
    );
    let thinking = Arc::new(AssistantMessage {
        content: vec![ContentBlock::Thinking(ThinkingContent {
            thinking: "reason".into(),
            thinking_signature: None,
        })],
        ..(*text_message("")).clone()
    });
    assert!(
        decoder()
            .push(wire(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "reason".into(),
                partial: thinking,
            }))
            .is_ok()
    );
}

#[test]
fn malformed_wire_values_are_not_echoed_and_duplicate_start_is_rejected() {
    for value in [
        Value::Null,
        json!(1),
        json!(["PRIVATE-CANARY"]),
        json!({"type":"PRIVATE-CANARY"}),
    ] {
        let error = decoder().push(value).unwrap_err();
        assert!(error.to_string().contains("PROTOCOL"));
        assert!(!error.to_string().contains("PRIVATE-CANARY"));
    }
    let mut decoder = decoder();
    let start = wire(AssistantMessageEvent::Start {
        partial: text_message(""),
    });
    decoder.push(start.clone()).unwrap();
    assert!(decoder.push(start).is_err());
}

#[test]
fn event_delta_depth_and_block_budgets_fail_before_forwarding() {
    let mut event_limited = decoder();
    event_limited.events = MAX_EVENTS;
    assert!(
        event_limited
            .push(json!(""))
            .unwrap_err()
            .to_string()
            .contains("LIMIT")
    );
    let mut byte_limited = decoder();
    byte_limited.delta_bytes = MAX_DELTA_BYTES - 3;
    assert!(
        byte_limited
            .push(json!("🦀"))
            .unwrap_err()
            .to_string()
            .contains("LIMIT")
    );
    assert!(byte_limited.text.is_empty());
    let mut nested = json!(null);
    for _ in 0..MAX_EVENT_DEPTH {
        nested = json!({"nested": nested});
    }
    assert!(
        decoder()
            .push(nested)
            .unwrap_err()
            .to_string()
            .contains("LIMIT")
    );
    let too_many = Arc::new(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new("")); MAX_BLOCKS + 1],
        ..(*text_message("")).clone()
    });
    assert!(
        decoder()
            .push(done(too_many, StopReason::Stop))
            .unwrap_err()
            .to_string()
            .contains("LIMIT")
    );
    let mut nodes = 2;
    assert!(admit_shape(&json!([null, null]), MAX_EVENT_DEPTH, &mut nodes).is_err());
    assert!(serde_json::to_writer(&mut ByteBudget(2), &json!("x")).is_err());
}
