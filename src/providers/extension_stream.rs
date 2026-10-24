//! Admission and terminal semantics for extension-authored provider streams.
//!
//! String iterators have an explicit text-only contract: iterator exhaustion
//! finishes that text. Structured event iterators have a different contract:
//! only a validated terminal event can complete them. Never promote a partial
//! assistant (especially a partial tool call) to a successful terminal reply.

use std::collections::HashSet;
use std::io::{self, Write};

use serde_json::Value;

use super::ExtensionStreamSimpleProvider;
use crate::error::{Error, Result};
use crate::model::{AssistantMessageEvent, ContentBlock, StopReason, StreamEvent};

mod blocks;

const MAX_EVENTS: usize = 262_144;
const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_EVENT_NODES: usize = 262_144;
const MAX_EVENT_DEPTH: usize = 128;
const MAX_DELTA_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOCKS: usize = 1024;
// Sparse providers may emit only completed previews, with no deltas at all.
// Bound their aggregate payload before delivery, not just each input event.
const MAX_PREVIEW_BYTES: usize = MAX_EVENT_BYTES;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Undetermined,
    Text,
    Events,
    Finished,
}

/// Own only text-mode accumulation, not every cumulative structured partial.
/// One decoder is created per provider invocation and is never shared across
/// streams. The outer stream owns iterator cancellation and queued delivery.
pub(super) struct Decoder {
    model: String,
    provider: String,
    api: String,
    mode: Mode,
    text: String,
    events: usize,
    delta_bytes: usize,
    preview_bytes_remaining: usize,
    blocks: blocks::Ledger,
}

fn protocol(message: &'static str) -> Error {
    Error::extension(format!("PI_EXTENSION_STREAM_PROTOCOL: {message}"))
}

fn limit() -> Error {
    Error::extension(
        "PI_EXTENSION_STREAM_LIMIT: extension provider stream exceeds its event, text, or message-shape limit",
    )
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "extension event byte limit")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admit_shape(value: &Value, depth: usize, remaining: &mut usize) -> Result<()> {
    if depth == 0 || *remaining == 0 {
        return Err(limit());
    }
    *remaining -= 1;
    match value {
        Value::Array(items) => {
            for item in items {
                admit_shape(item, depth - 1, remaining)?;
            }
        }
        Value::Object(items) => {
            for item in items.values() {
                admit_shape(item, depth - 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl Decoder {
    pub(super) const fn new(model: String, provider: String, api: String) -> Self {
        Self {
            model,
            provider,
            api,
            mode: Mode::Undetermined,
            text: String::new(),
            events: 0,
            delta_bytes: 0,
            preview_bytes_remaining: MAX_PREVIEW_BYTES,
            blocks: blocks::Ledger::new(),
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.mode == Mode::Finished
    }

    /// Rejecting an event ends the decoder; no buffered text can later be
    /// converted into a success by an accidental finish call.
    pub(super) fn push(&mut self, value: Value) -> Result<Vec<StreamEvent>> {
        let result = self.push_inner(value);
        if result.is_err() {
            self.mode = Mode::Finished;
            self.text.clear();
        }
        result
    }

    fn charge_delta(&mut self, bytes: usize) -> Result<()> {
        self.delta_bytes = self.delta_bytes.checked_add(bytes).ok_or_else(limit)?;
        if self.delta_bytes > MAX_DELTA_BYTES {
            return Err(limit());
        }
        Ok(())
    }

    /// Charge only the non-delta payload that will reach stream consumers.
    /// Do not repeatedly charge cumulative partials or the final message: those
    /// already have a per-event cap. Serialize into a counter, not another
    /// allocation, and include tool IDs, names, arguments and signatures.
    fn charge_preview(&mut self, event: &AssistantMessageEvent) -> Result<()> {
        let mut budget = ByteBudget(self.preview_bytes_remaining);
        match event {
            AssistantMessageEvent::TextEnd { content, .. }
            | AssistantMessageEvent::ThinkingEnd { content, .. } => {
                serde_json::to_writer(&mut budget, content)
            }
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                serde_json::to_writer(&mut budget, tool_call)
            }
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                let Some(ContentBlock::ToolCall(call)) = partial.content.get(*content_index) else {
                    return Err(protocol(
                        "content index does not identify the event's block type",
                    ));
                };
                serde_json::to_writer(&mut budget, &(&call.id, &call.name))
            }
            _ => return Ok(()),
        }
        .map_err(|_| limit())?;
        self.preview_bytes_remaining = budget.0;
        Ok(())
    }

    fn push_inner(&mut self, value: Value) -> Result<Vec<StreamEvent>> {
        if self.finished() {
            return Err(protocol("event received after stream termination"));
        }
        self.events = self.events.checked_add(1).ok_or_else(limit)?;
        if self.events > MAX_EVENTS {
            return Err(limit());
        }
        if let Value::String(chunk) = value {
            if self.mode == Mode::Events {
                return Err(protocol(
                    "structured events and raw text chunks cannot be mixed",
                ));
            }
            self.charge_delta(chunk.len())?;
            let first = self.mode == Mode::Undetermined;
            self.mode = Mode::Text;
            self.text.push_str(&chunk);
            let mut events = Vec::with_capacity(if first { 3 } else { 1 });
            if first {
                events.push(StreamEvent::Start {
                    partial: ExtensionStreamSimpleProvider::make_partial(
                        &self.model,
                        &self.provider,
                        &self.api,
                        "",
                    ),
                });
                events.push(StreamEvent::TextStart { content_index: 0 });
            }
            events.push(StreamEvent::TextDelta {
                content_index: 0,
                delta: chunk,
            });
            return Ok(events);
        }
        if self.mode == Mode::Text {
            return Err(protocol(
                "raw text chunks and structured events cannot be mixed",
            ));
        }
        if !value.is_object() {
            return Err(protocol(
                "expected a text chunk or structured assistant event",
            ));
        }
        let mut remaining = MAX_EVENT_NODES;
        admit_shape(&value, MAX_EVENT_DEPTH, &mut remaining)?;
        serde_json::to_writer(&mut ByteBudget(MAX_EVENT_BYTES), &value).map_err(|_| limit())?;
        // Never forward deserializer diagnostics: unknown field values can
        // themselves contain credentials, private source or terminal controls.
        let event: AssistantMessageEvent = serde_json::from_value(value)
            .map_err(|_| protocol("invalid structured assistant event"))?;
        if matches!(event, AssistantMessageEvent::Start { .. }) && self.events != 1 {
            return Err(protocol("start must be the first structured event"));
        }
        self.mode = Mode::Events;
        validate_event(&event)?;
        self.charge_preview(&event)?;
        self.blocks.admit(&event)?;
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. }
            | AssistantMessageEvent::ThinkingDelta { delta, .. }
            | AssistantMessageEvent::ToolCallDelta { delta, .. } => {
                self.charge_delta(delta.len())?;
            }
            _ => {}
        }
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        let mut output = ExtensionStreamSimpleProvider::assistant_event_to_stream_event(event);
        // Some OpenAI-compatible backends finish complete tool calls with
        // `stop`. Only normalize after validating the terminal message, so a
        // failed, inconsistent or malformed call cannot become executable.
        if let StreamEvent::Done { reason, message } = &mut output
            && *reason == StopReason::Stop
            && message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            *reason = StopReason::ToolUse;
            message.stop_reason = StopReason::ToolUse;
        }
        if terminal {
            self.mode = Mode::Finished;
        }
        Ok(vec![output])
    }

    pub(super) fn finish(&mut self) -> Result<Vec<StreamEvent>> {
        let mode = std::mem::replace(&mut self.mode, Mode::Finished);
        if mode == Mode::Finished {
            return Ok(Vec::new());
        }
        if mode != Mode::Text {
            return Err(Error::extension(
                "PI_EXTENSION_STREAM_INCOMPLETE: structured or empty provider stream ended without a terminal event; partial output was not completed",
            ));
        }
        let text = std::mem::take(&mut self.text);
        let message = ExtensionStreamSimpleProvider::make_partial(
            &self.model,
            &self.provider,
            &self.api,
            &text,
        );
        Ok(vec![
            StreamEvent::TextEnd {
                content_index: 0,
                content: text,
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        ])
    }
}

#[allow(clippy::too_many_lines)] // one match over every event shape
fn validate_event(event: &AssistantMessageEvent) -> Result<()> {
    let (message, indexed) = match event {
        AssistantMessageEvent::Start { partial } => (partial, None),
        AssistantMessageEvent::TextStart {
            content_index,
            partial,
        }
        | AssistantMessageEvent::TextDelta {
            content_index,
            partial,
            ..
        }
        | AssistantMessageEvent::TextEnd {
            content_index,
            partial,
            ..
        } => (partial, Some((*content_index, "text"))),
        AssistantMessageEvent::ThinkingStart {
            content_index,
            partial,
        }
        | AssistantMessageEvent::ThinkingDelta {
            content_index,
            partial,
            ..
        }
        | AssistantMessageEvent::ThinkingEnd {
            content_index,
            partial,
            ..
        } => (partial, Some((*content_index, "thinking"))),
        AssistantMessageEvent::ToolCallStart {
            content_index,
            partial,
        }
        | AssistantMessageEvent::ToolCallDelta {
            content_index,
            partial,
            ..
        }
        | AssistantMessageEvent::ToolCallEnd {
            content_index,
            partial,
            ..
        } => (partial, Some((*content_index, "toolCall"))),
        AssistantMessageEvent::Done { reason, message } => {
            if *reason != message.stop_reason
                || matches!(reason, StopReason::Error | StopReason::Aborted)
                || message.error_message.is_some()
            {
                return Err(protocol(
                    "done event carries a failed or inconsistent terminal message",
                ));
            }
            let mut ids = HashSet::new();
            for block in &message.content {
                if let ContentBlock::ToolCall(call) = block {
                    // PauseTurn is a server-tool continuation, not local tool
                    // authorization. Preserve its payload for verbatim replay.
                    if !matches!(
                        reason,
                        StopReason::Stop | StopReason::ToolUse | StopReason::PauseTurn
                    ) {
                        return Err(protocol(
                            "tool calls require a successful stop, tool-use or paused-turn terminal reason",
                        ));
                    }
                    if call.id.trim().is_empty()
                        || call.name.trim().is_empty()
                        || !call.arguments.is_object()
                        || !ids.insert(call.id.as_str())
                    {
                        return Err(protocol(
                            "terminal tool calls need unique nonempty IDs, names and object arguments",
                        ));
                    }
                }
            }
            if *reason == StopReason::ToolUse && ids.is_empty() {
                return Err(protocol("tool-use terminal message contains no tool calls"));
            }
            (message, None)
        }
        AssistantMessageEvent::Error { reason, error } => {
            if *reason != error.stop_reason
                || !matches!(reason, StopReason::Error | StopReason::Aborted)
            {
                return Err(protocol(
                    "error event carries an inconsistent terminal reason",
                ));
            }
            (error, None)
        }
    };
    if message.content.len() > MAX_BLOCKS {
        return Err(limit());
    }
    if let Some((index, kind)) = indexed {
        let valid = matches!(
            (kind, message.content.get(index)),
            ("text", Some(ContentBlock::Text(_)))
                | ("thinking", Some(ContentBlock::Thinking(_)))
                | ("toolCall", Some(ContentBlock::ToolCall(_)))
        );
        if !valid {
            return Err(protocol(
                "content index does not identify the event's block type",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod preview_budget_tests {
    use super::*;
    use crate::model::{AssistantMessage, TextContent, ThinkingContent, ToolCall};
    use serde_json::json;
    use std::sync::Arc;

    fn decoder(budget: usize) -> Decoder {
        let mut decoder = Decoder::new("model".into(), "provider".into(), "api".into());
        decoder.preview_bytes_remaining = budget;
        decoder
    }

    fn message(content: Vec<ContentBlock>) -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage {
            content,
            ..ExtensionStreamSimpleProvider::make_partial("model", "provider", "api", "")
        })
    }

    fn partial(index: usize, block: ContentBlock) -> Arc<AssistantMessage> {
        let mut content = vec![ContentBlock::Text(TextContent::new("")); index];
        content.push(block);
        message(content)
    }

    fn text_end(index: usize, content: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextEnd {
            content_index: index,
            content: content.into(),
            partial: partial(index, ContentBlock::Text(TextContent::new(content))),
        }
    }

    fn thinking_end(index: usize, content: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::ThinkingEnd {
            content_index: index,
            content: content.into(),
            partial: partial(
                index,
                ContentBlock::Thinking(ThinkingContent {
                    thinking: content.into(),
                    thinking_signature: None,
                }),
            ),
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            arguments: json!({"path": "output.txt", "content": "PRIVATE-CANARY"}),
            thought_signature: Some("PRIVATE-SIGNATURE".into()),
        }
    }

    fn tool_end(index: usize, call: ToolCall) -> AssistantMessageEvent {
        AssistantMessageEvent::ToolCallEnd {
            content_index: index,
            partial: partial(index, ContentBlock::ToolCall(call.clone())),
            tool_call: call,
        }
    }

    fn push(decoder: &mut Decoder, event: AssistantMessageEvent) -> Result<Vec<StreamEvent>> {
        decoder.push(serde_json::to_value(event).unwrap())
    }

    fn reject_limit(decoder: &mut Decoder, event: AssistantMessageEvent) {
        let error = push(decoder, event).unwrap_err().to_string();
        assert!(error.contains("PI_EXTENSION_STREAM_LIMIT"), "{error}");
        assert!(!error.contains("PRIVATE-CANARY"));
        assert!(!error.contains("PRIVATE-SIGNATURE"));
        assert!(decoder.finished());
        assert!(decoder.finish().unwrap().is_empty());
        assert!(
            push(
                decoder,
                AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: message(vec![]),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn completed_text_and_thinking_share_a_budget_without_deltas() {
        let first = "first 🦀";
        let second = "PRIVATE-CANARY";
        let first_bytes = serde_json::to_vec(first).unwrap().len();
        let second_bytes = serde_json::to_vec(second).unwrap().len();
        let mut decoder = decoder(first_bytes + second_bytes - 1);
        let events = push(&mut decoder, text_end(0, first)).unwrap();
        assert!(matches!(&events[0], StreamEvent::TextEnd { .. }));
        assert_eq!(decoder.preview_bytes_remaining, second_bytes - 1);
        reject_limit(&mut decoder, thinking_end(1, second));
        assert_eq!(decoder.delta_bytes, 0);
    }

    #[test]
    fn completed_tools_charge_arguments_and_signatures_before_delivery() {
        let call = call();
        let bytes = serde_json::to_vec(&call).unwrap().len();
        let mut too_small = decoder(bytes - 1);
        reject_limit(&mut too_small, tool_end(0, call.clone()));
        let mut exact = decoder(bytes);
        let events = push(&mut exact, tool_end(0, call)).unwrap();
        assert!(matches!(&events[0], StreamEvent::ToolCallEnd { .. }));
        assert_eq!(exact.preview_bytes_remaining, 0);
        assert_eq!(exact.delta_bytes, 0);
    }

    #[test]
    fn tool_start_identities_share_the_completed_preview_budget() {
        for change_id in [false, true] {
            let mut call = call();
            if change_id {
                call.id = "PRIVATE-CANARY 🦀".into();
            } else {
                call.name = "PRIVATE-CANARY 🦀".into();
            }
            let bytes = serde_json::to_vec(&(&call.id, &call.name)).unwrap().len();
            let start = AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial(0, ContentBlock::ToolCall(call.clone())),
            };
            reject_limit(&mut decoder(bytes - 1), start.clone());
            let mut exact = decoder(bytes);
            let events = push(&mut exact, start).unwrap();
            assert!(matches!(&events[0], StreamEvent::ToolCallStart { .. }));
            assert_eq!(exact.preview_bytes_remaining, 0);
            reject_limit(&mut exact, tool_end(0, call));
        }
    }

    #[test]
    fn preview_budget_counts_utf8_and_json_escaping_at_the_exact_boundary() {
        let content = "🦀\n\t\"\\";
        let bytes = serde_json::to_vec(content).unwrap().len();
        assert!(bytes > content.len());
        reject_limit(&mut decoder(bytes - 1), text_end(0, content));
        let mut exact = decoder(bytes);
        push(&mut exact, text_end(0, content)).unwrap();
        assert_eq!(exact.preview_bytes_remaining, 0);
        // Even an empty end has a serialized payload; it cannot wrap the
        // counter or restore budget after the last byte was consumed.
        reject_limit(&mut exact, thinking_end(1, ""));
    }

    #[test]
    fn terminal_does_not_charge_completed_content_twice() {
        let text = "answer";
        let call = call();
        let bytes =
            serde_json::to_vec(text).unwrap().len() + serde_json::to_vec(&call).unwrap().len();
        let mut decoder = decoder(bytes);
        push(&mut decoder, text_end(0, text)).unwrap();
        push(&mut decoder, tool_end(1, call.clone())).unwrap();
        assert_eq!(decoder.preview_bytes_remaining, 0);
        let output = push(
            &mut decoder,
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message(vec![
                    ContentBlock::Text(TextContent::new(text)),
                    ContentBlock::ToolCall(call),
                ]),
            },
        )
        .unwrap();
        assert!(matches!(
            &output[0],
            StreamEvent::Done { reason, message }
                if *reason == StopReason::ToolUse && message.stop_reason == StopReason::ToolUse
        ));
        assert!(decoder.finished());
    }

    #[test]
    fn preview_budget_is_per_stream_and_independent_of_delta_accounting() {
        let mut first = decoder(0);
        push(
            &mut first,
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "delta".into(),
                partial: partial(0, ContentBlock::Text(TextContent::new("delta"))),
            },
        )
        .unwrap();
        assert_eq!(first.delta_bytes, 5);
        assert_eq!(first.preview_bytes_remaining, 0);
        push(
            &mut first,
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message(vec![ContentBlock::Text(TextContent::new("delta"))]),
            },
        )
        .unwrap();
        let mut second = Decoder::new("model".into(), "provider".into(), "api".into());
        assert_eq!(second.preview_bytes_remaining, MAX_PREVIEW_BYTES);
        push(&mut second, text_end(0, "independent")).unwrap();
        assert_eq!(second.delta_bytes, 0);
    }

    #[test]
    fn individually_valid_tool_starts_cannot_accumulate_unbounded_metadata() {
        let mut call = call();
        let bytes = serde_json::to_vec(&(&call.id, &call.name)).unwrap().len();
        let mut decoder = decoder(3 * bytes);
        for index in 0..3 {
            // Distinct calls need distinct IDs. Equal-length identities keep
            // this test focused on the exact aggregate metadata-byte limit.
            call.id = format!("call-{index}");
            assert_eq!(
                serde_json::to_vec(&(&call.id, &call.name)).unwrap().len(),
                bytes
            );
            let events = push(
                &mut decoder,
                AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: partial(index, ContentBlock::ToolCall(call.clone())),
                },
            )
            .unwrap();
            assert_eq!(events.len(), 1);
        }
        assert_eq!(decoder.preview_bytes_remaining, 0);
        call.id = "call-3".into();
        reject_limit(
            &mut decoder,
            AssistantMessageEvent::ToolCallStart {
                content_index: 3,
                partial: partial(3, ContentBlock::ToolCall(call)),
            },
        );
    }
}
