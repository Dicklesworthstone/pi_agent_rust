//! Per-block admission for structured extension streams. Keep only bounded
//! lifecycle metadata, never copies of cumulative text or tool arguments.
//!
//! A terminal-only reply and sparse previews are valid: a missing start/end
//! is not itself an error. Once a block emits an end, however, another indexed
//! event cannot reopen it. End payloads must agree with the partial that the
//! extension supplied in that same event, before either reaches consumers.

use super::{MAX_BLOCKS, limit, protocol};
use crate::error::Result;
use crate::model::{AssistantMessageEvent, ContentBlock};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
    Tool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    Delta,
    End,
}

#[derive(Clone, Copy)]
struct State {
    kind: Kind,
    closed: bool,
}

pub(super) struct Ledger {
    entries: Vec<Option<State>>,
}

impl Ledger {
    pub(super) const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    #[allow(clippy::too_many_lines)] // one match over the block lifecycle events
    pub(super) fn admit(&mut self, event: &AssistantMessageEvent) -> Result<()> {
        use AssistantMessageEvent as E;
        let (index, kind, phase, partial) = match event {
            E::TextStart {
                content_index,
                partial,
            } => (*content_index, Kind::Text, Phase::Start, partial),
            E::TextDelta {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Text, Phase::Delta, partial),
            E::TextEnd {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Text, Phase::End, partial),
            E::ThinkingStart {
                content_index,
                partial,
            } => (*content_index, Kind::Thinking, Phase::Start, partial),
            E::ThinkingDelta {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Thinking, Phase::Delta, partial),
            E::ThinkingEnd {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Thinking, Phase::End, partial),
            E::ToolCallStart {
                content_index,
                partial,
            } => (*content_index, Kind::Tool, Phase::Start, partial),
            E::ToolCallDelta {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Tool, Phase::Delta, partial),
            E::ToolCallEnd {
                content_index,
                partial,
                ..
            } => (*content_index, Kind::Tool, Phase::End, partial),
            E::Start { .. } | E::Done { .. } | E::Error { .. } => return Ok(()),
        };
        // Check before index + 1 or any allocation, even when called without
        // the outer decoder's message-shape validation.
        if index >= MAX_BLOCKS || partial.content.len() > MAX_BLOCKS {
            return Err(limit());
        }
        let block = partial
            .content
            .get(index)
            .ok_or_else(|| protocol("content index does not identify the event's block type"))?;
        if !matches!(
            (kind, block),
            (Kind::Text, ContentBlock::Text(_))
                | (Kind::Thinking, ContentBlock::Thinking(_))
                | (Kind::Tool, ContentBlock::ToolCall(_))
        ) {
            return Err(protocol(
                "content index does not identify the event's block type",
            ));
        }
        match (event, block) {
            (E::TextEnd { content, .. }, ContentBlock::Text(text)) if content != &text.text => {
                return Err(protocol("text end disagrees with its partial block"));
            }
            (E::ThinkingEnd { content, .. }, ContentBlock::Thinking(thinking))
                if content != &thinking.thinking =>
            {
                return Err(protocol("thinking end disagrees with its partial block"));
            }
            (E::ToolCallEnd { tool_call, .. }, ContentBlock::ToolCall(call)) => {
                if tool_call.id != call.id
                    || tool_call.name != call.name
                    || tool_call.arguments != call.arguments
                    || tool_call.thought_signature != call.thought_signature
                {
                    return Err(protocol("tool-call end disagrees with its partial block"));
                }
                if call.id.trim().is_empty()
                    || call.name.trim().is_empty()
                    || !call.arguments.is_object()
                {
                    return Err(protocol(
                        "completed tool previews need nonempty IDs, names and object arguments",
                    ));
                }
            }
            _ => {}
        }
        if let Some(Some(previous)) = self.entries.get(index) {
            if previous.closed {
                return Err(protocol("content event received after its block ended"));
            }
            if previous.kind != kind {
                return Err(protocol(
                    "content index changed block type during the stream",
                ));
            }
            if phase == Phase::Start {
                return Err(protocol(
                    "block start must precede other events for that index",
                ));
            }
        }
        if self.entries.len() <= index {
            self.entries.resize(index + 1, None);
        }
        self.entries[index] = Some(State {
            kind,
            closed: phase == Phase::End,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder, ExtensionStreamSimpleProvider};
    use super::*;
    use crate::model::{
        AssistantMessage, StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall,
    };
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn decoder() -> Decoder {
        Decoder::new("model".into(), "provider".into(), "api".into())
    }

    fn message(content: Vec<ContentBlock>) -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage {
            content,
            ..ExtensionStreamSimpleProvider::make_partial("model", "provider", "api", "")
        })
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(value))
    }

    fn thinking(value: &str) -> ContentBlock {
        ContentBlock::Thinking(ThinkingContent {
            thinking: value.into(),
            thinking_signature: Some("thinking-signature".into()),
        })
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            arguments: json!({"path": "output.txt", "content": "complete"}),
            thought_signature: Some("tool-signature".into()),
        }
    }

    fn push(decoder: &mut Decoder, event: AssistantMessageEvent) -> Result<Vec<StreamEvent>> {
        decoder.push(serde_json::to_value(event).unwrap())
    }

    fn reject(decoder: &mut Decoder, event: AssistantMessageEvent) {
        let error = push(decoder, event).unwrap_err().to_string();
        assert!(error.contains("PI_EXTENSION_STREAM_PROTOCOL"), "{error}");
        assert!(!error.contains("PRIVATE-CANARY"));
        assert!(decoder.finished());
        assert!(decoder.finish().unwrap().is_empty());
        assert!(decoder.push(json!("later text")).is_err());
    }

    #[test]
    fn text_and_thinking_end_payloads_must_match_their_partial() {
        reject(
            &mut decoder(),
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "PRIVATE-CANARY".into(),
                partial: message(vec![text("actual")]),
            },
        );
        reject(
            &mut decoder(),
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "PRIVATE-CANARY".into(),
                partial: message(vec![thinking("actual")]),
            },
        );
        for content in ["", "complete 🦀\ntext"] {
            let result = push(
                &mut decoder(),
                AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: content.into(),
                    partial: message(vec![text(content)]),
                },
            )
            .unwrap();
            assert!(
                matches!(&result[0], StreamEvent::TextEnd { content: actual, .. } if actual == content)
            );
            assert!(
                push(
                    &mut decoder(),
                    AssistantMessageEvent::ThinkingEnd {
                        content_index: 0,
                        content: content.into(),
                        partial: message(vec![thinking(content)]),
                    }
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn tool_end_checks_id_name_arguments_and_signature_before_forwarding() {
        for changed in 0..4 {
            let expected = call();
            let mut supplied = expected.clone();
            match changed {
                0 => supplied.id = "PRIVATE-CANARY".into(),
                1 => supplied.name = "PRIVATE-CANARY".into(),
                2 => supplied.arguments = json!({"content": "PRIVATE-CANARY"}),
                _ => supplied.thought_signature = Some("PRIVATE-CANARY".into()),
            }
            reject(
                &mut decoder(),
                AssistantMessageEvent::ToolCallEnd {
                    content_index: 0,
                    tool_call: supplied,
                    partial: message(vec![ContentBlock::ToolCall(expected)]),
                },
            );
        }
        let expected = call();
        let result = push(
            &mut decoder(),
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: expected.clone(),
                partial: message(vec![ContentBlock::ToolCall(expected.clone())]),
            },
        )
        .unwrap();
        let StreamEvent::ToolCallEnd { tool_call, .. } = &result[0] else {
            panic!("expected tool preview");
        };
        assert_eq!(
            serde_json::to_value(tool_call).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn matching_but_incomplete_tool_end_payloads_are_rejected() {
        for changed in 0..4 {
            let mut incomplete = call();
            match changed {
                0 => incomplete.id = "  ".into(),
                1 => incomplete.name.clear(),
                2 => incomplete.arguments = Value::Null,
                _ => incomplete.arguments = json!("PRIVATE-CANARY"),
            }
            reject(
                &mut decoder(),
                AssistantMessageEvent::ToolCallEnd {
                    content_index: 0,
                    tool_call: incomplete.clone(),
                    partial: message(vec![ContentBlock::ToolCall(incomplete)]),
                },
            );
        }
    }

    fn block_event(kind: Kind, phase: Phase) -> AssistantMessageEvent {
        use AssistantMessageEvent as E;
        let content_index = 0;
        let partial = message(vec![match kind {
            Kind::Text => text("complete"),
            Kind::Thinking => thinking("complete"),
            Kind::Tool => ContentBlock::ToolCall(call()),
        }]);
        match (kind, phase) {
            (Kind::Text, Phase::Start) => E::TextStart {
                content_index,
                partial,
            },
            (Kind::Text, Phase::Delta) => E::TextDelta {
                content_index,
                delta: "complete".into(),
                partial,
            },
            (Kind::Text, Phase::End) => E::TextEnd {
                content_index,
                content: "complete".into(),
                partial,
            },
            (Kind::Thinking, Phase::Start) => E::ThinkingStart {
                content_index,
                partial,
            },
            (Kind::Thinking, Phase::Delta) => E::ThinkingDelta {
                content_index,
                delta: "complete".into(),
                partial,
            },
            (Kind::Thinking, Phase::End) => E::ThinkingEnd {
                content_index,
                content: "complete".into(),
                partial,
            },
            (Kind::Tool, Phase::Start) => E::ToolCallStart {
                content_index,
                partial,
            },
            (Kind::Tool, Phase::Delta) => E::ToolCallDelta {
                content_index,
                delta: "{}".into(),
                partial,
            },
            (Kind::Tool, Phase::End) => E::ToolCallEnd {
                content_index,
                tool_call: call(),
                partial,
            },
        }
    }

    #[test]
    fn completed_blocks_cannot_be_reopened_changed_or_ended_twice() {
        for kind in [Kind::Text, Kind::Thinking, Kind::Tool] {
            for phase in [Phase::Start, Phase::Delta, Phase::End] {
                let mut decoder = decoder();
                push(&mut decoder, block_event(kind, Phase::End)).unwrap();
                reject(&mut decoder, block_event(kind, phase));
            }
        }
    }

    #[test]
    fn open_blocks_cannot_change_type_or_restart() {
        for first in [Kind::Text, Kind::Thinking, Kind::Tool] {
            for next in [Kind::Text, Kind::Thinking, Kind::Tool] {
                let mut decoder = decoder();
                push(&mut decoder, block_event(first, Phase::Delta)).unwrap();
                if first == next {
                    reject(&mut decoder, block_event(next, Phase::Start));
                } else {
                    reject(&mut decoder, block_event(next, Phase::Delta));
                }
            }
            let mut decoder = decoder();
            push(&mut decoder, block_event(first, Phase::Start)).unwrap();
            reject(&mut decoder, block_event(first, Phase::Start));
        }
    }

    #[test]
    fn interleaved_blocks_finish_independently_then_require_a_terminal() {
        use AssistantMessageEvent as E;
        let partial = message(vec![
            text("answer"),
            thinking("reason"),
            ContentBlock::ToolCall(call()),
        ]);
        let mut decoder = decoder();
        for event in [
            E::TextStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
            E::ToolCallStart {
                content_index: 2,
                partial: Arc::clone(&partial),
            },
            E::ThinkingDelta {
                content_index: 1,
                delta: "reason".into(),
                partial: Arc::clone(&partial),
            },
            E::TextEnd {
                content_index: 0,
                content: "answer".into(),
                partial: Arc::clone(&partial),
            },
            E::ToolCallDelta {
                content_index: 2,
                delta: "{}".into(),
                partial: Arc::clone(&partial),
            },
            E::ThinkingEnd {
                content_index: 1,
                content: "reason".into(),
                partial: Arc::clone(&partial),
            },
            E::ToolCallEnd {
                content_index: 2,
                tool_call: call(),
                partial: Arc::clone(&partial),
            },
        ] {
            assert_eq!(push(&mut decoder, event).unwrap().len(), 1);
            assert!(!decoder.finished());
        }
        let mut complete = (*partial).clone();
        complete.stop_reason = StopReason::ToolUse;
        complete.usage.output = 17;
        let expected = serde_json::to_value(&complete).unwrap();
        let output = push(
            &mut decoder,
            E::Done {
                reason: StopReason::ToolUse,
                message: Arc::new(complete),
            },
        )
        .unwrap();
        let StreamEvent::Done { reason, message } = &output[0] else {
            panic!("missing terminal")
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(serde_json::to_value(message).unwrap(), expected);
        assert!(decoder.finished());
    }

    #[test]
    fn sparse_previews_do_not_require_start_events_or_fabricate_completion() {
        for kind in [Kind::Text, Kind::Thinking, Kind::Tool] {
            let mut decoder = decoder();
            push(&mut decoder, block_event(kind, Phase::End)).unwrap();
            assert!(!decoder.finished());
            assert!(
                decoder
                    .finish()
                    .unwrap_err()
                    .to_string()
                    .contains("INCOMPLETE")
            );
        }
        let output = push(
            &mut decoder(),
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message(vec![text("terminal only")]),
            },
        )
        .unwrap();
        assert!(matches!(
            &output[0],
            StreamEvent::Done {
                reason: StopReason::Stop,
                ..
            }
        ));
    }

    #[test]
    fn block_tracking_is_bounded_before_allocation() {
        let mut ledger = Ledger::new();
        for content_index in [MAX_BLOCKS, usize::MAX] {
            let error = ledger
                .admit(&AssistantMessageEvent::TextStart {
                    content_index,
                    partial: message(vec![text("")]),
                })
                .unwrap_err();
            assert!(error.to_string().contains("LIMIT"));
            assert!(ledger.entries.is_empty());
        }
        ledger
            .admit(&AssistantMessageEvent::TextStart {
                content_index: MAX_BLOCKS - 1,
                partial: message(vec![text(""); MAX_BLOCKS]),
            })
            .unwrap();
        assert_eq!(ledger.entries.len(), MAX_BLOCKS);
        assert!(ledger.entries[..MAX_BLOCKS - 1].iter().all(Option::is_none));
    }
}
