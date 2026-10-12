//! Per-block admission for structured extension streams. Keep only bounded
//! lifecycle metadata, never copies of cumulative text or tool arguments.
//!
//! A terminal-only reply and sparse previews are valid: a missing start/end
//! is not itself an error. Once a block emits an end, however, another indexed
//! event cannot reopen it. End payloads must agree with the partial that the
//! extension supplied in that same event, before either reaches consumers.
//! Successful terminal messages must also preserve every observed block type
//! and every completed block payload. A terminal is not permission to replace
//! a tool call after its completed preview has already been displayed.
//! Tool IDs and names are bound at their first nonempty indexed preview, not
//! just at the end: arguments may accumulate, but a preview cannot change the
//! identity of the operation that stream consumers have already observed.
//! Nonblank IDs cannot identify two different preview indices. Text/thinking
//! deltas must agree with their snapshots, and later snapshots and successful
//! terminals may extend, but never rewrite, an already observed text prefix.

use super::{MAX_BLOCKS, limit, protocol};
use crate::error::Result;
use crate::model::{AssistantMessageEvent, ContentBlock, ToolCall};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
    Tool,
}

impl Kind {
    fn matches(self, block: &ContentBlock) -> bool {
        matches!(
            (self, block),
            (Self::Text, ContentBlock::Text(_))
                | (Self::Thinking, ContentBlock::Thinking(_))
                | (Self::Tool, ContentBlock::ToolCall(_))
        )
    }
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
    // A fixed-size commitment, not a retained copy of potentially huge tool
    // arguments or cumulative text. None denotes an unfinished preview.
    fingerprint: Option<[u8; 32]>,
}

// Keep identity commitments separate from completed-payload commitments. An
// unfinished tool may legitimately gain arguments, a signature, or an ID/name
// that was initially absent. Once a nonblank ID/name is observed, it may not
// change or disappear. Retain no copies of untrusted identity strings.
#[derive(Clone, Copy, Default)]
struct ToolIdentity {
    id: Option<[u8; 32]>,
    name: Option<[u8; 32]>,
}

impl ToolIdentity {
    fn observe(self, call: &ToolCall) -> Result<Self> {
        let observed = Self {
            id: identity_fingerprint(&call.id),
            name: identity_fingerprint(&call.name),
        };
        if self.id.is_some() && self.id != observed.id {
            return Err(protocol(
                "tool-call ID changed after its first nonempty preview",
            ));
        }
        if self.name.is_some() && self.name != observed.name {
            return Err(protocol(
                "tool name changed after its first nonempty preview",
            ));
        }
        Ok(observed)
    }
}

fn identity_fingerprint(value: &str) -> Option<[u8; 32]> {
    // Whitespace-only identities cannot identify a completed tool call. Do
    // not normalize nonblank values: whitespace and case remain significant.
    (!value.trim().is_empty()).then(|| Sha256::digest(value.as_bytes()).into())
}

// Retain only a length and a commitment to a delta's cumulative snapshot.
// Signatures may arrive later; completed-block fingerprints bind those at End.
#[derive(Clone, Copy)]
struct TextPrefix {
    bytes: usize,
    fingerprint: [u8; 32],
}

impl TextPrefix {
    fn new(text: &str) -> Self {
        Self {
            bytes: text.len(),
            fingerprint: Sha256::digest(text.as_bytes()).into(),
        }
    }

    fn check(self, text: &str) -> Result<()> {
        // get() also rejects a replacement that puts the old byte boundary
        // inside a new Unicode character, rather than panicking on a slice.
        let prefix = text
            .get(..self.bytes)
            .ok_or_else(|| protocol("text or thinking content retracted an observed prefix"))?;
        let fingerprint: [u8; 32] = Sha256::digest(prefix.as_bytes()).into();
        if fingerprint != self.fingerprint {
            return Err(protocol(
                "text or thinking content changed an observed prefix",
            ));
        }
        Ok(())
    }
}

fn text_payload(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(text) => Some(&text.text),
        ContentBlock::Thinking(thinking) => Some(&thinking.thinking),
        _ => None,
    }
}

pub(super) struct Ledger {
    entries: Vec<Option<State>>,
    tool_identities: Vec<Option<ToolIdentity>>,
    text_prefixes: Vec<Option<TextPrefix>>,
}

impl Ledger {
    pub(super) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            tool_identities: Vec::new(),
            text_prefixes: Vec::new(),
        }
    }

    fn observe_tool(&self, index: usize, call: &ToolCall) -> Result<ToolIdentity> {
        let previous = self
            .tool_identities
            .get(index)
            .copied()
            .flatten()
            .unwrap_or_default();
        let observed = previous.observe(call)?;
        // Scan only when an ID first becomes bound, not on every arguments
        // delta. There can be at most MAX_BLOCKS such bindings per stream;
        // reuse their fixed-size commitments instead of storing another map.
        if previous.id.is_none()
            && let Some(id) = observed.id
            && self
                .tool_identities
                .iter()
                .flatten()
                .any(|identity| identity.id == Some(id))
        {
            return Err(protocol(
                "tool-call ID is already assigned to another content index",
            ));
        }
        Ok(observed)
    }

    fn observe_text(
        &self,
        index: usize,
        event: &AssistantMessageEvent,
        block: &ContentBlock,
    ) -> Result<Option<TextPrefix>> {
        let Some(text) = text_payload(block) else {
            return Ok(None);
        };
        let delta = match event {
            AssistantMessageEvent::TextDelta { delta, .. }
            | AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(delta.as_str()),
            AssistantMessageEvent::TextEnd { .. } | AssistantMessageEvent::ThinkingEnd { .. } => {
                None
            }
            // Indexed Start events deliver only the index, not their text.
            // They must not consume a prefix before the first actual delta.
            _ => return Ok(None),
        };
        let before_delta = if let Some(delta) = delta {
            text.strip_suffix(delta).ok_or_else(|| {
                protocol("text or thinking delta disagrees with its partial block")
            })?
        } else {
            text
        };
        if let Some(Some(previous)) = self.text_prefixes.get(index) {
            // A sparse stream can skip intervening updates, but the new
            // delta cannot replay/overlap text that was already observed.
            previous.check(before_delta)?;
        }
        Ok(delta.map(|_| TextPrefix::new(text)))
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
            E::Done { message, .. } => return self.admit_terminal(&message.content),
            E::Start { .. } | E::Error { .. } => return Ok(()),
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
        if !kind.matches(block) {
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
            if previous.fingerprint.is_some() {
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
        let tool_identity = if let ContentBlock::ToolCall(call) = block {
            Some(self.observe_tool(index, call)?)
        } else {
            None
        };
        let text_prefix = self.observe_text(index, event, block)?;
        let fingerprint = if phase == Phase::End {
            Some(block_fingerprint(block)?)
        } else {
            None
        };
        if self.entries.len() <= index {
            self.entries.resize(index + 1, None);
        }
        if let Some(identity) = tool_identity {
            if self.tool_identities.len() <= index {
                self.tool_identities.resize(index + 1, None);
            }
            self.tool_identities[index] = Some(identity);
        }
        if let Some(prefix) = text_prefix {
            if self.text_prefixes.len() <= index {
                self.text_prefixes.resize(index + 1, None);
            }
            self.text_prefixes[index] = Some(prefix);
        }
        self.entries[index] = Some(State { kind, fingerprint });
        Ok(())
    }

    fn admit_terminal(&self, content: &[ContentBlock]) -> Result<()> {
        if content.len() > MAX_BLOCKS {
            return Err(limit());
        }
        for (index, state) in self.entries.iter().enumerate() {
            let Some(state) = state else {
                continue;
            };
            let block = content
                .get(index)
                .ok_or_else(|| protocol("terminal message omitted an observed content block"))?;
            if !state.kind.matches(block) {
                return Err(protocol("terminal message changed an observed block type"));
            }
            if let ContentBlock::ToolCall(call) = block {
                self.observe_tool(index, call)?;
            }
            if let Some(text) = text_payload(block)
                && let Some(Some(prefix)) = self.text_prefixes.get(index)
            {
                prefix.check(text)?;
            }
            if let Some(expected) = state.fingerprint
                && block_fingerprint(block)? != expected
            {
                return Err(protocol(
                    "terminal message changed a completed content block",
                ));
            }
        }
        Ok(())
    }
}

// Serialize directly into the digest: retaining each completed payload would
// otherwise let many individually bounded previews accumulate unbounded data.
struct FingerprintWriter(Sha256);

impl Write for FingerprintWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Object key order is not part of a tool call's meaning, including when a
// downstream dependency enables serde_json's preserve_order feature. Sort
// borrowed entries recursively rather than cloning the arguments into a second
// JSON tree. Array order and scalar types remain significant.
struct CanonicalValue<'a>(&'a serde_json::Value);

impl Serialize for CanonicalValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            serde_json::Value::Array(items) => {
                serializer.collect_seq(items.iter().map(CanonicalValue))
            }
            serde_json::Value::Object(items) => {
                let mut entries: Vec<_> = items.iter().collect();
                entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
                serializer.collect_map(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key, CanonicalValue(value))),
                )
            }
            value => value.serialize(serializer),
        }
    }
}

fn block_fingerprint(block: &ContentBlock) -> Result<[u8; 32]> {
    let mut writer = FingerprintWriter(Sha256::new());
    writer.0.update(b"pi:extension:completed-block:v1\0");
    let result = match block {
        ContentBlock::ToolCall(call) => serde_json::to_writer(
            &mut writer,
            &(
                &call.id,
                &call.name,
                CanonicalValue(&call.arguments),
                &call.thought_signature,
            ),
        ),
        _ => serde_json::to_writer(&mut writer, block),
    };
    result.map_err(|_| protocol("completed block could not be fingerprinted"))?;
    Ok(writer.0.finalize().into())
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

    fn terminal(reason: StopReason, content: Vec<ContentBlock>) -> AssistantMessageEvent {
        let mut complete = (*message(content)).clone();
        complete.stop_reason = reason;
        if matches!(reason, StopReason::Error | StopReason::Aborted) {
            AssistantMessageEvent::Error {
                reason,
                error: Arc::new(complete),
            }
        } else {
            AssistantMessageEvent::Done {
                reason,
                message: Arc::new(complete),
            }
        }
    }

    #[test]
    fn terminal_cannot_replace_a_completed_tool_preview() {
        for reason in [StopReason::Stop, StopReason::ToolUse, StopReason::PauseTurn] {
            for changed in 0..4 {
                let mut decoder = decoder();
                push(&mut decoder, block_event(Kind::Tool, Phase::End)).unwrap();
                let mut supplied = call();
                match changed {
                    0 => supplied.id = "PRIVATE-CANARY".into(),
                    1 => supplied.name = "PRIVATE-CANARY".into(),
                    2 => supplied.arguments = json!({"content": "PRIVATE-CANARY"}),
                    _ => supplied.thought_signature = Some("PRIVATE-CANARY".into()),
                }
                reject(
                    &mut decoder,
                    terminal(reason, vec![ContentBlock::ToolCall(supplied)]),
                );
            }
        }
    }

    fn tool_preview(phase: Phase, tool_call: ToolCall) -> AssistantMessageEvent {
        indexed_tool_preview(0, phase, tool_call)
    }

    fn indexed_tool_preview(
        content_index: usize,
        phase: Phase,
        tool_call: ToolCall,
    ) -> AssistantMessageEvent {
        let mut content = vec![text(""); content_index];
        content.push(ContentBlock::ToolCall(tool_call.clone()));
        let partial = message(content);
        match phase {
            Phase::Start => AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            },
            Phase::Delta => AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta: "{}".into(),
                partial,
            },
            Phase::End => AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            },
        }
    }

    #[test]
    fn duplicate_tool_ids_are_rejected_before_any_second_preview_is_forwarded() {
        for (first_index, next_index) in [(0, 2), (2, 0)] {
            for first in [Phase::Start, Phase::Delta, Phase::End] {
                for next in [Phase::Start, Phase::Delta, Phase::End] {
                    let mut decoder = decoder();
                    let mut supplied = call();
                    supplied.id = "PRIVATE-CANARY".into();
                    push(
                        &mut decoder,
                        indexed_tool_preview(first_index, first, supplied.clone()),
                    )
                    .unwrap();
                    supplied.arguments = json!({"path": "other.txt"});
                    reject(
                        &mut decoder,
                        indexed_tool_preview(next_index, next, supplied),
                    );
                }
            }
        }
    }

    #[test]
    fn delayed_tool_ids_cannot_collide_when_they_become_known() {
        for next in [Phase::Delta, Phase::End] {
            let mut decoder = decoder();
            let mut unknown = call();
            unknown.id.clear();
            for index in 0..2 {
                push(
                    &mut decoder,
                    indexed_tool_preview(index, Phase::Start, unknown.clone()),
                )
                .unwrap();
            }
            push(&mut decoder, tool_preview(Phase::Delta, call())).unwrap();
            reject(&mut decoder, indexed_tool_preview(1, next, call()));
        }
    }

    #[test]
    fn distinct_ids_allow_same_tool_name_and_repeated_interleaved_argument_updates() {
        let calls = [
            call(),
            ToolCall {
                id: "call-2".into(),
                ..call()
            },
        ];
        let mut decoder = decoder();
        for phase in [Phase::Start, Phase::Delta, Phase::Delta, Phase::End] {
            for index in [1, 0] {
                assert_eq!(
                    push(
                        &mut decoder,
                        indexed_tool_preview(index, phase, calls[index].clone()),
                    )
                    .unwrap()
                    .len(),
                    1
                );
            }
        }
        let output = push(
            &mut decoder,
            terminal(
                StopReason::Stop,
                calls.into_iter().map(ContentBlock::ToolCall).collect(),
            ),
        )
        .unwrap();
        assert!(matches!(
            &output[0],
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                message,
            } if message.content.len() == 2
        ));
    }

    #[test]
    fn rejected_duplicate_does_not_claim_an_index_or_mutate_the_existing_call() {
        let mut ledger = Ledger::new();
        ledger.admit(&tool_preview(Phase::Start, call())).unwrap();
        assert!(
            ledger
                .admit(&indexed_tool_preview(1, Phase::End, call()))
                .is_err()
        );
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.tool_identities.len(), 1);
        let other = ToolCall {
            id: "call-2".into(),
            ..call()
        };
        ledger
            .admit(&indexed_tool_preview(1, Phase::End, other.clone()))
            .unwrap();
        ledger
            .admit(&terminal(
                StopReason::ToolUse,
                vec![
                    ContentBlock::ToolCall(call()),
                    ContentBlock::ToolCall(other),
                ],
            ))
            .unwrap();
    }

    fn text_preview(
        kind: Kind,
        content_index: usize,
        phase: Phase,
        delta: &str,
        content: &str,
    ) -> AssistantMessageEvent {
        use AssistantMessageEvent as E;
        let mut blocks = vec![text(""); content_index];
        blocks.push(match kind {
            Kind::Text => text(content),
            Kind::Thinking => thinking(content),
            Kind::Tool => panic!("expected text or thinking"),
        });
        let partial = message(blocks);
        match (kind, phase) {
            (Kind::Text, Phase::Start) => E::TextStart {
                content_index,
                partial,
            },
            (Kind::Text, Phase::Delta) => E::TextDelta {
                content_index,
                delta: delta.into(),
                partial,
            },
            (Kind::Text, Phase::End) => E::TextEnd {
                content_index,
                content: content.into(),
                partial,
            },
            (Kind::Thinking, Phase::Start) => E::ThinkingStart {
                content_index,
                partial,
            },
            (Kind::Thinking, Phase::Delta) => E::ThinkingDelta {
                content_index,
                delta: delta.into(),
                partial,
            },
            (Kind::Thinking, Phase::End) => E::ThinkingEnd {
                content_index,
                content: content.into(),
                partial,
            },
            (Kind::Tool, _) => unreachable!(),
        }
    }

    #[test]
    fn text_and_thinking_deltas_must_match_their_same_event_snapshots() {
        for kind in [Kind::Text, Kind::Thinking] {
            for (delta, content) in [("PRIVATE-CANARY", "real"), ("long", "x"), ("x", "xy")] {
                reject(
                    &mut decoder(),
                    text_preview(kind, 0, Phase::Delta, delta, content),
                );
            }
        }
    }

    #[test]
    fn later_deltas_cannot_rewrite_retract_overlap_or_replay_observed_text() {
        for kind in [Kind::Text, Kind::Thinking] {
            for (delta, content) in [
                (" suffix", "PRIVATE-CANARY suffix"),
                ("", "pre"),
                ("prefix", "prefix"),
                ("fix", "prefix"),
                ("", "different"),
            ] {
                let mut decoder = decoder();
                push(
                    &mut decoder,
                    text_preview(kind, 0, Phase::Delta, "prefix", "prefix"),
                )
                .unwrap();
                reject(
                    &mut decoder,
                    text_preview(kind, 0, Phase::Delta, delta, content),
                );
            }
        }
    }

    #[test]
    fn ends_and_successful_terminals_cannot_replace_open_text_prefixes() {
        for kind in [Kind::Text, Kind::Thinking] {
            for replacement in ["", "pre", "PRIVATE-CANARY"] {
                let mut stream_decoder = decoder();
                push(
                    &mut stream_decoder,
                    text_preview(kind, 0, Phase::Delta, "prefix", "prefix"),
                )
                .unwrap();
                reject(
                    &mut stream_decoder,
                    text_preview(kind, 0, Phase::End, "", replacement),
                );
                for reason in [
                    StopReason::Stop,
                    StopReason::Length,
                    StopReason::Refusal,
                    StopReason::PauseTurn,
                ] {
                    let mut decoder = decoder();
                    push(
                        &mut decoder,
                        text_preview(kind, 0, Phase::Delta, "prefix", "prefix"),
                    )
                    .unwrap();
                    let block = if kind == Kind::Text {
                        text(replacement)
                    } else {
                        thinking(replacement)
                    };
                    reject(&mut decoder, terminal(reason, vec![block]));
                }
            }
        }
    }

    #[test]
    fn sparse_unicode_updates_and_optional_end_events_can_extend_open_prefixes() {
        for kind in [Kind::Text, Kind::Thinking] {
            for emit_end in [false, true] {
                let mut decoder = decoder();
                for (delta, content) in [
                    ("🦀", "unseen 🦀"),
                    (" world", "unseen 🦀 middle world"),
                    ("", "unseen 🦀 middle world"),
                ] {
                    assert_eq!(
                        push(
                            &mut decoder,
                            text_preview(kind, 0, Phase::Delta, delta, content),
                        )
                        .unwrap()
                        .len(),
                        1
                    );
                }
                let complete = "unseen 🦀 middle world!";
                if emit_end {
                    push(
                        &mut decoder,
                        text_preview(kind, 0, Phase::End, "", complete),
                    )
                    .unwrap();
                }
                let block = if kind == Kind::Text {
                    text(complete)
                } else {
                    thinking(complete)
                };
                let output = push(&mut decoder, terminal(StopReason::Stop, vec![block])).unwrap();
                assert!(matches!(
                    &output[0],
                    StreamEvent::Done { message, .. }
                        if text_payload(&message.content[0]) == Some(complete)
                ));
            }
        }
    }

    #[test]
    fn indexed_starts_do_not_consume_text_and_interleaved_prefixes_are_independent() {
        let mut decoder = decoder();
        for (kind, index, content) in [(Kind::Text, 0, "answer"), (Kind::Thinking, 1, "reason")] {
            push(
                &mut decoder,
                text_preview(kind, index, Phase::Start, "", content),
            )
            .unwrap();
            push(
                &mut decoder,
                text_preview(kind, index, Phase::Delta, content, content),
            )
            .unwrap();
        }
        for (kind, index, content) in [(Kind::Thinking, 1, "reason!"), (Kind::Text, 0, "answer!")] {
            push(
                &mut decoder,
                text_preview(kind, index, Phase::Delta, "!", content),
            )
            .unwrap();
        }
        let output = push(
            &mut decoder,
            terminal(StopReason::Stop, vec![text("answer!"), thinking("reason!")]),
        )
        .unwrap();
        assert!(matches!(&output[0], StreamEvent::Done { .. }));
    }

    #[test]
    fn unicode_boundary_rewrites_and_rejected_deltas_do_not_mutate_prefix_state() {
        for kind in [Kind::Text, Kind::Thinking] {
            let mut ledger = Ledger::new();
            ledger
                .admit(&text_preview(kind, 0, Phase::Delta, "a", "a"))
                .unwrap();
            // The previous one-byte boundary lies inside the new 'é'.
            assert!(
                ledger
                    .admit(&text_preview(kind, 0, Phase::Delta, "!", "é!"))
                    .is_err()
            );
            ledger
                .admit(&text_preview(kind, 0, Phase::Delta, "🦀", "a🦀"))
                .unwrap();
            ledger
                .admit(&text_preview(kind, 0, Phase::End, "", "a🦀"))
                .unwrap();
        }
    }

    #[test]
    fn text_prefix_tracking_is_bounded_and_not_allocated_for_end_only_previews() {
        let mut ledger = Ledger::new();
        for content_index in [MAX_BLOCKS, usize::MAX] {
            let event = AssistantMessageEvent::TextDelta {
                content_index,
                delta: "".into(),
                partial: message(vec![text("")]),
            };
            assert!(ledger.admit(&event).is_err());
            assert!(ledger.entries.is_empty());
            assert!(ledger.text_prefixes.is_empty());
        }
        ledger
            .admit(&block_event(Kind::Tool, Phase::Start))
            .unwrap();
        ledger
            .admit(&text_preview(Kind::Text, 1, Phase::End, "", "complete"))
            .unwrap();
        assert!(ledger.text_prefixes.is_empty());
        ledger
            .admit(&text_preview(
                Kind::Thinking,
                MAX_BLOCKS - 1,
                Phase::Delta,
                "🦀",
                "🦀",
            ))
            .unwrap();
        assert_eq!(ledger.text_prefixes.len(), MAX_BLOCKS);
        assert!(
            ledger.text_prefixes[..MAX_BLOCKS - 1]
                .iter()
                .all(Option::is_none)
        );
        assert!(std::mem::size_of::<TextPrefix>() <= 40);
    }

    #[test]
    fn failure_and_cancellation_can_discard_open_text_without_fabricating_success() {
        for kind in [Kind::Text, Kind::Thinking] {
            for reason in [StopReason::Error, StopReason::Aborted] {
                let mut decoder = decoder();
                push(
                    &mut decoder,
                    text_preview(kind, 0, Phase::Delta, "prefix", "prefix"),
                )
                .unwrap();
                let output = push(&mut decoder, terminal(reason, vec![])).unwrap();
                assert!(matches!(&output[0], StreamEvent::Error { .. }));
                assert!(decoder.finished());
                assert!(decoder.finish().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn open_tool_previews_cannot_change_or_erase_observed_identity() {
        for first in [Phase::Start, Phase::Delta] {
            for next in [Phase::Delta, Phase::End] {
                for field in ["id", "name"] {
                    for value in ["PRIVATE-CANARY", "", "  "] {
                        let mut decoder = decoder();
                        push(&mut decoder, tool_preview(first, call())).unwrap();
                        let mut changed = call();
                        if field == "id" {
                            changed.id = value.into();
                        } else {
                            changed.name = value.into();
                        }
                        reject(&mut decoder, tool_preview(next, changed));
                    }
                }
            }
        }
    }

    #[test]
    fn successful_terminals_bind_identity_even_without_tool_end_events() {
        for first in [Phase::Start, Phase::Delta] {
            for reason in [StopReason::Stop, StopReason::ToolUse, StopReason::PauseTurn] {
                for field in ["id", "name"] {
                    let mut decoder = decoder();
                    push(&mut decoder, tool_preview(first, call())).unwrap();
                    let mut changed = call();
                    if field == "id" {
                        changed.id = "PRIVATE-CANARY".into();
                    } else {
                        changed.name = "PRIVATE-CANARY".into();
                    }
                    reject(
                        &mut decoder,
                        terminal(reason, vec![ContentBlock::ToolCall(changed)]),
                    );
                }
            }
        }
    }

    #[test]
    fn missing_tool_identity_can_arrive_incrementally_while_arguments_grow() {
        let mut decoder = decoder();
        let mut supplied = ToolCall {
            id: String::new(),
            name: "  ".into(),
            arguments: json!({}),
            thought_signature: None,
        };
        push(&mut decoder, tool_preview(Phase::Start, supplied.clone())).unwrap();
        supplied.id = call().id;
        push(&mut decoder, tool_preview(Phase::Delta, supplied.clone())).unwrap();
        supplied.name = call().name;
        supplied.arguments = json!({"path": "output.txt"});
        push(&mut decoder, tool_preview(Phase::Delta, supplied)).unwrap();
        push(&mut decoder, tool_preview(Phase::End, call())).unwrap();
        let output = push(
            &mut decoder,
            terminal(StopReason::Stop, vec![ContentBlock::ToolCall(call())]),
        )
        .unwrap();
        assert!(matches!(
            &output[0],
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        ));
    }

    #[test]
    fn filling_one_identity_field_does_not_allow_replacing_the_other() {
        for missing_id in [false, true] {
            let mut decoder = decoder();
            let mut initial = call();
            if missing_id {
                initial.id.clear();
            } else {
                initial.name.clear();
            }
            push(&mut decoder, tool_preview(Phase::Start, initial)).unwrap();
            let mut changed = call();
            if missing_id {
                changed.name = "PRIVATE-CANARY".into();
            } else {
                changed.id = "PRIVATE-CANARY".into();
            }
            reject(&mut decoder, tool_preview(Phase::Delta, changed));
        }
    }

    #[test]
    fn tool_identity_tracking_is_bounded_and_only_allocated_for_tools() {
        let mut ledger = Ledger::new();
        for content_index in [MAX_BLOCKS, usize::MAX] {
            assert!(
                ledger
                    .admit(&AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: message(vec![ContentBlock::ToolCall(call())]),
                    })
                    .is_err()
            );
            assert!(ledger.entries.is_empty());
            assert!(ledger.tool_identities.is_empty());
        }
        ledger
            .admit(&block_event(Kind::Text, Phase::Start))
            .unwrap();
        assert!(ledger.tool_identities.is_empty());
        let mut content = vec![text(""); MAX_BLOCKS];
        content[MAX_BLOCKS - 1] = ContentBlock::ToolCall(call());
        ledger
            .admit(&AssistantMessageEvent::ToolCallStart {
                content_index: MAX_BLOCKS - 1,
                partial: message(content),
            })
            .unwrap();
        assert_eq!(ledger.tool_identities.len(), MAX_BLOCKS);
        assert!(
            ledger.tool_identities[..MAX_BLOCKS - 1]
                .iter()
                .all(Option::is_none)
        );
        assert!(std::mem::size_of::<ToolIdentity>() <= 72);
    }

    #[test]
    fn identity_matching_preserves_exact_spelling_and_cancellation() {
        for (id, name) in [
            ("CALL-1", "write"),
            ("call-1", "Write"),
            ("call-1 ", "write"),
        ] {
            let mut decoder = decoder();
            push(&mut decoder, tool_preview(Phase::Start, call())).unwrap();
            reject(
                &mut decoder,
                tool_preview(
                    Phase::Delta,
                    ToolCall {
                        id: id.into(),
                        name: name.into(),
                        ..call()
                    },
                ),
            );
        }
        for reason in [StopReason::Error, StopReason::Aborted] {
            let mut decoder = decoder();
            push(&mut decoder, tool_preview(Phase::Start, call())).unwrap();
            let output = push(&mut decoder, terminal(reason, vec![])).unwrap();
            assert!(matches!(&output[0], StreamEvent::Error { .. }));
            assert!(decoder.finished());
        }
    }

    #[test]
    fn terminal_preserves_completed_text_thinking_and_signatures() {
        for kind in [Kind::Text, Kind::Thinking] {
            let mut decoder = decoder();
            push(&mut decoder, block_event(kind, Phase::End)).unwrap();
            let changed = if kind == Kind::Text {
                text("PRIVATE-CANARY")
            } else {
                thinking("PRIVATE-CANARY")
            };
            reject(&mut decoder, terminal(StopReason::Stop, vec![changed]));
        }
        let mut decoder = decoder();
        push(&mut decoder, block_event(Kind::Thinking, Phase::End)).unwrap();
        reject(
            &mut decoder,
            terminal(
                StopReason::Stop,
                vec![ContentBlock::Thinking(ThinkingContent {
                    thinking: "complete".into(),
                    thinking_signature: Some("PRIVATE-CANARY".into()),
                })],
            ),
        );
    }

    #[test]
    fn terminal_cannot_drop_or_retype_observed_blocks() {
        for kind in [Kind::Text, Kind::Thinking, Kind::Tool] {
            for phase in [Phase::Start, Phase::Delta, Phase::End] {
                for replacement in [vec![], vec![text("complete"), thinking("complete")]] {
                    let mut decoder = decoder();
                    // Track index 2 so an unrelated earlier block cannot stand
                    // in for an omitted observed block, even of the same kind.
                    let mut event = serde_json::to_value(block_event(kind, phase)).unwrap();
                    let block = event["partial"]["content"][0].clone();
                    event["contentIndex"] = json!(2);
                    event["partial"]["content"] = json!([text(""), text(""), block]);
                    decoder.push(event).unwrap();
                    reject(&mut decoder, terminal(StopReason::Stop, replacement));
                }
                let mut decoder = decoder();
                push(&mut decoder, block_event(kind, phase)).unwrap();
                let replacement = if kind == Kind::Text {
                    thinking("complete")
                } else {
                    text("complete")
                };
                reject(&mut decoder, terminal(StopReason::Stop, vec![replacement]));
            }
        }
    }

    #[test]
    fn terminal_can_complete_open_previews_and_unobserved_blocks() {
        for kind in [Kind::Text, Kind::Thinking, Kind::Tool] {
            let mut decoder = decoder();
            push(&mut decoder, block_event(kind, Phase::Delta)).unwrap();
            let block = match kind {
                Kind::Text => text("complete with additional text"),
                Kind::Thinking => thinking("complete with additional thinking"),
                Kind::Tool => ContentBlock::ToolCall(ToolCall {
                    arguments: json!({"path": "output.txt", "content": "final arguments"}),
                    ..call()
                }),
            };
            let result = push(
                &mut decoder,
                terminal(
                    StopReason::Stop,
                    vec![block, text("unobserved final block")],
                ),
            )
            .unwrap();
            assert!(matches!(&result[0], StreamEvent::Done { .. }));
            assert!(decoder.finished());
        }
    }

    #[test]
    fn unchanged_completed_tool_preserves_success_normalization() {
        for reason in [StopReason::Stop, StopReason::ToolUse, StopReason::PauseTurn] {
            let mut decoder = decoder();
            push(&mut decoder, block_event(Kind::Tool, Phase::End)).unwrap();
            let output = push(
                &mut decoder,
                terminal(reason, vec![ContentBlock::ToolCall(call())]),
            )
            .unwrap();
            let expected = if reason == StopReason::Stop {
                StopReason::ToolUse
            } else {
                reason
            };
            assert!(matches!(
                &output[0],
                StreamEvent::Done { reason, message }
                    if *reason == expected && message.stop_reason == expected
            ));
        }
    }

    #[test]
    fn failed_streams_may_discard_their_partial_blocks() {
        for reason in [StopReason::Error, StopReason::Aborted] {
            let mut decoder = decoder();
            push(&mut decoder, block_event(Kind::Tool, Phase::End)).unwrap();
            let output = push(&mut decoder, terminal(reason, vec![])).unwrap();
            assert!(matches!(&output[0], StreamEvent::Error { .. }));
            assert!(decoder.finished());
        }
    }

    #[test]
    fn tool_fingerprints_ignore_object_order_but_bind_array_order_and_types() {
        let original = ContentBlock::ToolCall(ToolCall {
            arguments: serde_json::from_str(r#"{"z":1,"a":{"y":[{"b":2,"a":1},true],"x":"text"}}"#)
                .unwrap(),
            ..call()
        });
        let reordered = ContentBlock::ToolCall(ToolCall {
            arguments: serde_json::from_str(r#"{"a":{"x":"text","y":[{"a":1,"b":2},true]},"z":1}"#)
                .unwrap(),
            ..call()
        });
        assert_eq!(
            block_fingerprint(&original).unwrap(),
            block_fingerprint(&reordered).unwrap()
        );
        for arguments in [
            json!({"z": 1, "a": {"y": [true, {"a": 1, "b": 2}], "x": "text"}}),
            json!({"z": "1", "a": {"y": [{"a": 1, "b": 2}, true], "x": "text"}}),
        ] {
            let changed = ContentBlock::ToolCall(ToolCall {
                arguments,
                ..call()
            });
            assert_ne!(
                block_fingerprint(&original).unwrap(),
                block_fingerprint(&changed).unwrap()
            );
        }
        // Payload size cannot change the per-block retained memory budget.
        assert!(std::mem::size_of::<State>() <= 40);
    }
}
