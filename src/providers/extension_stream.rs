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
