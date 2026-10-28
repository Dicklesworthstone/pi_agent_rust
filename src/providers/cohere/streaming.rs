//! Incremental Cohere v2 message and tool-call state machine.
//!
//! Wire contract: https://docs.cohere.com/docs/tool-use-streaming
//! Completion requires `message-end`, never an OpenAI-style `[DONE]` marker.
//! A wire index is a key, not a vector allocation size. Tool and content
//! indices occupy separate namespaces, and completed indices cannot be reused.

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, BillingTokenCounts, BillingUsage, ContentBlock, StopReason, StreamEvent,
    TextContent, ThinkingContent, ToolCall, Usage,
};
use crate::sse::SseStream;
use futures::{Stream, StreamExt, stream};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::pin::Pin;

const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_BLOCKS: usize = 1024;
const MAX_ID_BYTES: usize = 1024;

fn protocol(message: &str) -> Error {
    Error::api(format!("Cohere stream protocol error: {message}"))
}

fn required_str<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(&format!("missing or invalid {pointer}")))
}

fn wire_index(value: &Value) -> Result<Option<u32>> {
    value
        .get("index")
        .map(|index| {
            index
                .as_u64()
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| protocol("invalid block index"))
        })
        .transpose()
}

fn required_index(value: &Value) -> Result<u32> {
    wire_index(value)?.ok_or_else(|| protocol("missing content index"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
}

#[derive(Clone, Copy)]
struct ContentSlot {
    content_index: usize,
    kind: Kind,
    closed: bool,
}

struct ToolSlot {
    content_index: usize,
    arguments: String,
}

pub(super) struct StreamState<S>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    pub(super) event_source: SseStream<S>,
    pub(super) pending_events: VecDeque<StreamEvent>,
    pub(super) finished: bool,
    partial: AssistantMessage,
    started: bool,
    contents: BTreeMap<u32, ContentSlot>,
    tools: BTreeMap<u32, ToolSlot>,
    seen_tool_indices: BTreeSet<u32>,
    seen_tool_ids: BTreeSet<String>,
    plan: Option<ContentSlot>,
    received_bytes: usize,
    usage_observer: Option<crate::provider::UsageObserver>,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    pub(super) fn new(
        event_source: SseStream<S>,
        model: String,
        api: String,
        provider: String,
    ) -> Self {
        Self {
            event_source,
            pending_events: VecDeque::new(),
            finished: false,
            partial: AssistantMessage {
                content: Vec::new(),
                api,
                provider,
                model,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            started: false,
            contents: BTreeMap::new(),
            tools: BTreeMap::new(),
            seen_tool_indices: BTreeSet::new(),
            seen_tool_ids: BTreeSet::new(),
            plan: None,
            received_bytes: 0,
            usage_observer: None,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        let total = self.received_bytes.saturating_add(bytes);
        if total > MAX_RESPONSE_BYTES {
            return Err(protocol("response exceeds the accumulated-content limit"));
        }
        self.received_bytes = total;
        Ok(())
    }

    fn check_block_capacity(&self) -> Result<()> {
        if self.partial.content.len() >= MAX_BLOCKS {
            return Err(protocol("response exceeds the content-block limit"));
        }
        Ok(())
    }

    fn open_content(&mut self, kind: Kind) -> Result<ContentSlot> {
        self.check_block_capacity()?;
        let content_index = self.partial.content.len();
        match kind {
            Kind::Text => {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
                self.pending_events
                    .push_back(StreamEvent::TextStart { content_index });
            }
            Kind::Thinking => {
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));
                self.pending_events
                    .push_back(StreamEvent::ThinkingStart { content_index });
            }
        }
        Ok(ContentSlot {
            content_index,
            kind,
            closed: false,
        })
    }

    fn append_content(&mut self, slot: ContentSlot, text: &str) -> Result<()> {
        if slot.closed {
            return Err(protocol("delta received after content-end"));
        }
        self.charge(text.len())?;
        if text.is_empty() {
            return Ok(());
        }
        let content_index = slot.content_index;
        match (slot.kind, self.partial.content.get_mut(content_index)) {
            (Kind::Text, Some(ContentBlock::Text(block))) => {
                block.text.push_str(text);
                self.pending_events.push_back(StreamEvent::TextDelta {
                    content_index,
                    delta: text.to_string(),
                });
            }
            (Kind::Thinking, Some(ContentBlock::Thinking(block))) => {
                block.thinking.push_str(text);
                self.pending_events.push_back(StreamEvent::ThinkingDelta {
                    content_index,
                    delta: text.to_string(),
                });
            }
            _ => return Err(protocol("content kind changed while streaming")),
        }
        Ok(())
    }

    fn close_content(&mut self, slot: ContentSlot) -> Result<()> {
        if slot.closed {
            return Err(protocol("duplicate content-end"));
        }
        let content_index = slot.content_index;
        let event = match self.partial.content.get(content_index) {
            Some(ContentBlock::Text(text)) => StreamEvent::TextEnd {
                content_index,
                content: text.text.clone(),
            },
            Some(ContentBlock::Thinking(thinking)) => StreamEvent::ThinkingEnd {
                content_index,
                content: thinking.thinking.clone(),
            },
            _ => return Err(protocol("missing content block")),
        };
        self.pending_events.push_back(event);
        Ok(())
    }

    fn close_plan(&mut self) -> Result<()> {
        if let Some(slot) = self.plan
            && !slot.closed
        {
            self.close_content(slot)?;
            self.plan = Some(ContentSlot {
                closed: true,
                ..slot
            });
        }
        Ok(())
    }

    fn active_tool_index(&self, index: Option<u32>) -> Result<u32> {
        if let Some(index) = index {
            return self
                .tools
                .contains_key(&index)
                .then_some(index)
                .ok_or_else(|| protocol("tool delta/end has no active start for its index"));
        }
        // Older gateways and existing recordings omit the index. Preserve
        // that shape only when it is unambiguous, never guess among calls.
        if self.tools.len() != 1 {
            return Err(protocol(
                "indexless tool delta/end is ambiguous or has no start",
            ));
        }
        self.tools
            .keys()
            .next()
            .copied()
            .ok_or_else(|| protocol("missing tool start"))
    }

    fn start_tool(&mut self, value: &Value) -> Result<()> {
        self.close_plan()?;
        self.check_block_capacity()?;
        let index = if let Some(index) = wire_index(value)? {
            index
        } else {
            if !self.tools.is_empty() {
                return Err(protocol(
                    "indexless tool start overlaps another active call",
                ));
            }
            match self.seen_tool_indices.last() {
                Some(last) => last
                    .checked_add(1)
                    .ok_or_else(|| protocol("tool index overflow"))?,
                None => 0,
            }
        };
        if self.seen_tool_indices.contains(&index) {
            return Err(protocol("duplicate tool-call index"));
        }
        let id = required_str(value, "/delta/message/tool_calls/id")?;
        let name = required_str(value, "/delta/message/tool_calls/function/name")?;
        if id.trim().is_empty()
            || name.trim().is_empty()
            || id.len() > MAX_ID_BYTES
            || name.len() > MAX_ID_BYTES
        {
            return Err(protocol("invalid tool-call identity"));
        }
        if self.seen_tool_ids.contains(id) {
            return Err(protocol("duplicate tool-call id"));
        }
        let arguments = match value.pointer("/delta/message/tool_calls/function/arguments") {
            None | Some(Value::Null) => "",
            Some(Value::String(arguments)) => arguments,
            _ => return Err(protocol("tool arguments must be JSON text")),
        };
        if arguments.len() > MAX_ARGUMENT_BYTES {
            return Err(protocol("tool arguments exceed the byte limit"));
        }
        self.charge(
            id.len()
                .saturating_add(name.len())
                .saturating_add(arguments.len()),
        )?;
        let content_index = self.partial.content.len();
        self.partial.content.push(ContentBlock::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: Value::Null,
            thought_signature: None,
        }));
        self.tools.insert(
            index,
            ToolSlot {
                content_index,
                arguments: arguments.to_string(),
            },
        );
        self.seen_tool_indices.insert(index);
        self.seen_tool_ids.insert(id.to_string());
        self.pending_events.push_back(StreamEvent::ToolCallStart {
            content_index,
            id: id.to_string(),
            name: name.to_string(),
        });
        if !arguments.is_empty() {
            self.pending_events.push_back(StreamEvent::ToolCallDelta {
                content_index,
                delta: arguments.to_string(),
            });
        }
        Ok(())
    }

    fn tool_delta(&mut self, value: &Value) -> Result<()> {
        let index = self.active_tool_index(wire_index(value)?)?;
        let delta = required_str(value, "/delta/message/tool_calls/function/arguments")?;
        self.charge(delta.len())?;
        let slot = self
            .tools
            .get_mut(&index)
            .ok_or_else(|| protocol("missing tool start"))?;
        if slot.arguments.len().saturating_add(delta.len()) > MAX_ARGUMENT_BYTES {
            return Err(protocol("tool arguments exceed the byte limit"));
        }
        slot.arguments.push_str(delta);
        if !delta.is_empty() {
            self.pending_events.push_back(StreamEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: delta.to_string(),
            });
        }
        Ok(())
    }

    fn end_tool(&mut self, value: &Value) -> Result<()> {
        let index = self.active_tool_index(wire_index(value)?)?;
        let slot = self
            .tools
            .remove(&index)
            .ok_or_else(|| protocol("missing tool start"))?;
        let arguments: Value = serde_json::from_str(&slot.arguments)
            .map_err(|_| protocol("tool arguments are not complete JSON"))?;
        if !arguments.is_object() {
            return Err(protocol("tool arguments must be a JSON object"));
        }
        let Some(ContentBlock::ToolCall(tool_call)) =
            self.partial.content.get_mut(slot.content_index)
        else {
            return Err(protocol("missing tool-call block"));
        };
        tool_call.arguments = arguments;
        self.pending_events.push_back(StreamEvent::ToolCallEnd {
            content_index: slot.content_index,
            tool_call: tool_call.clone(),
        });
        Ok(())
    }

    fn message_end(&mut self, value: &Value) -> Result<()> {
        // A received bill remains real even when another terminal field is
        // malformed. Publish it before lifecycle validation or queued events
        // can return control to a caller that cancels the stream.
        self.apply_usage(value)?;
        let finish = required_str(value, "/delta/finish_reason")?;
        let has_tools = !self.seen_tool_indices.is_empty();
        // A normal completion that emitted tool calls is a ToolUse stop, and a
        // TOOL_CALL completion that emitted none is a protocol violation rather
        // than an empty turn.
        let reason = match finish {
            "COMPLETE" | "STOP_SEQUENCE" | "TOOL_CALL" if has_tools => StopReason::ToolUse,
            "COMPLETE" | "STOP_SEQUENCE" => StopReason::Stop,
            "TOOL_CALL" => return Err(protocol("TOOL_CALL completion has no tool calls")),
            "MAX_TOKENS" => StopReason::Length,
            "ERROR" | "ERROR_TOXIC" => StopReason::Error,
            _ => return Err(protocol("unknown message finish reason")),
        };
        if !self.tools.is_empty() {
            return Err(protocol("message-end arrived with unfinished tool calls"));
        }
        if self.contents.values().any(|slot| !slot.closed) {
            return Err(protocol("message-end arrived before content-end"));
        }
        self.close_plan()?;
        self.partial.stop_reason = reason;
        if reason == StopReason::Error {
            self.partial.error_message =
                Some("Cohere reported an unsuccessful generation".to_string());
        }
        self.pending_events.push_back(StreamEvent::Done {
            reason,
            message: std::mem::take(&mut self.partial),
        });
        self.finished = true;
        Ok(())
    }

    fn apply_usage(&mut self, value: &Value) -> Result<()> {
        self.partial.usage.billing = billed_usage(value.pointer("/delta/usage/billed_units"));
        if self.partial.usage.billing.is_some()
            && let Some(observer) = &self.usage_observer
        {
            observer.observe_authoritative(&mut self.partial.usage)?;
        }
        if let Some(tokens) = value.pointer("/delta/usage/tokens")
            && !tokens.is_null()
        {
            if !tokens.is_object() {
                return Err(protocol("invalid usage token object"));
            }
            self.partial.usage.input = token_count(tokens.get("input_tokens"))?;
            self.partial.usage.output = token_count(tokens.get("output_tokens"))?;
            self.partial.usage.total_tokens = self
                .partial
                .usage
                .input
                .saturating_add(self.partial.usage.output);
        }
        if self.partial.usage.billing.is_none()
            && let Some(observer) = &self.usage_observer
        {
            observer.observe_authoritative(&mut self.partial.usage)?;
        }
        Ok(())
    }

    pub(super) fn process_event(&mut self, data: &str) -> Result<()> {
        let outcome = self.process_inner(data);
        if outcome.is_err() {
            self.finished = true;
            self.pending_events.clear();
        }
        outcome
    }

    fn process_inner(&mut self, data: &str) -> Result<()> {
        if self.finished {
            return Err(protocol("event received after terminal outcome"));
        }
        if data.len() > MAX_EVENT_BYTES {
            return Err(protocol("event exceeds the byte limit"));
        }
        if data.trim() == "[DONE]" {
            return Err(protocol("stream ended before message-end (unexpected EOF)"));
        }
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid event JSON"))?;
        let kind = required_str(&value, "/type")?;
        if kind == "error" || kind == "stream-error" {
            // Do not echo remote payloads: they may include prompt text or credentials.
            return Err(protocol("provider reported a stream error"));
        }
        if kind == "message-start" {
            if self.started {
                return Err(protocol("duplicate message-start"));
            }
            self.started = true;
            self.pending_events.push_back(StreamEvent::Start {
                partial: self.partial.clone(),
            });
            return Ok(());
        }
        match kind {
            "content-start" | "content-delta" | "content-end" | "tool-plan-delta"
            | "tool-call-start" | "tool-call-delta" | "tool-call-end" | "message-end" => {
                if !self.started {
                    return Err(protocol(
                        "content or completion received before message-start",
                    ));
                }
            }
            _ => return Ok(()), // Citations/debug/new advisory events are forward-compatible.
        }
        self.dispatch_started_event(kind, &value)
    }

    /// The per-kind handlers, once the event is known to belong to a started
    /// message. Split from `process_inner` so neither half outgrows the line
    /// budget as more event kinds arrive.
    fn dispatch_started_event(&mut self, kind: &str, value: &Value) -> Result<()> {
        let value = value.clone();
        match kind {
            "content-start" => {
                self.close_plan()?;
                let index = required_index(&value)?;
                if self.contents.contains_key(&index) {
                    return Err(protocol("duplicate content index"));
                }
                let content = value
                    .pointer("/delta/message/content")
                    .ok_or_else(|| protocol("missing content-start data"))?;
                let kind = match required_str(content, "/type")? {
                    "text" => Kind::Text,
                    "thinking" => Kind::Thinking,
                    _ => return Err(protocol("unsupported content kind")),
                };
                let text = required_str(
                    content,
                    if kind == Kind::Text {
                        "/text"
                    } else {
                        "/thinking"
                    },
                )?;
                let slot = self.open_content(kind)?;
                self.contents.insert(index, slot);
                self.append_content(slot, text)?;
            }
            "content-delta" => {
                let index = required_index(&value)?;
                let slot = self
                    .contents
                    .get(&index)
                    .copied()
                    .ok_or_else(|| protocol("content-delta has no matching start"))?;
                let content = value
                    .pointer("/delta/message/content")
                    .ok_or_else(|| protocol("missing content delta"))?;
                let (field, other) = match slot.kind {
                    Kind::Text => ("/text", "thinking"),
                    Kind::Thinking => ("/thinking", "text"),
                };
                if content.get(other).is_some() {
                    return Err(protocol("content kind changed while streaming"));
                }
                self.append_content(slot, required_str(content, field)?)?;
            }
            "content-end" => {
                let index = required_index(&value)?;
                let slot = self
                    .contents
                    .get(&index)
                    .copied()
                    .ok_or_else(|| protocol("content-end has no matching start"))?;
                self.close_content(slot)?;
                self.contents.insert(
                    index,
                    ContentSlot {
                        closed: true,
                        ..slot
                    },
                );
            }
            "tool-plan-delta" => {
                let text = required_str(&value, "/delta/message/tool_plan")?;
                let slot = match self.plan {
                    Some(slot) => slot,
                    None => self.open_content(Kind::Text)?,
                };
                self.plan = Some(slot);
                // Plans are public assistant prose, not signed model thinking.
                // Keep them in the transcript without inventing a signature.
                self.append_content(slot, text)?;
            }
            "tool-call-start" => self.start_tool(&value)?,
            "tool-call-delta" => self.tool_delta(&value)?,
            "tool-call-end" => self.end_tool(&value)?,
            "message-end" => self.message_end(&value)?,
            _ => unreachable!("known event kinds were checked above"),
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Option<Result<StreamEvent>> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Some(Ok(event));
            }
            if self.finished {
                return None;
            }
            match self.event_source.next().await {
                Some(Ok(event)) => {
                    if event.event == "ping" && event.data.is_empty() {
                        continue;
                    }
                    if let Err(error) = self.process_event(&event.data) {
                        return Some(Err(error));
                    }
                }
                Some(Err(_)) => {
                    self.finished = true;
                    return Some(Err(Error::api(
                        "Cohere stream transport failed (unexpected EOF)",
                    )));
                }
                None => {
                    self.finished = true;
                    return Some(Err(Error::api(
                        "Cohere stream ended before message-end (unexpected EOF)",
                    )));
                }
            }
        }
    }
}

fn token_count(value: Option<&Value>) -> Result<u64> {
    match value {
        None | Some(Value::Null) => Ok(0),
        Some(value) => {
            exact_token_count(value).ok_or_else(|| protocol("invalid usage token count"))
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn exact_token_count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        // Cohere's official schema uses numbers, including whole-valued
        // floats. Never round a fractional bill or saturate an overflow.
        let count = value.as_f64()?;
        (count.is_finite()
            && (0.0..18_446_744_073_709_551_616.0).contains(&count)
            && count.fract() == 0.0)
            .then_some(count as u64)
    })
}

fn billed_usage(value: Option<&Value>) -> Option<BillingUsage> {
    let value = value.filter(|value| !value.is_null())?;
    let unavailable = || {
        Some(BillingUsage::Unavailable {
        reason: "Cohere billed_units requires complete nonnegative integer token counts without unsupported billing units".to_string(),
    })
    };
    let Some(units) = value.as_object() else {
        return unavailable();
    };
    let (Some(input), Some(output)) = (
        units.get("input_tokens").and_then(exact_token_count),
        units.get("output_tokens").and_then(exact_token_count),
    ) else {
        return unavailable();
    };
    if units.iter().any(|(field, value)| {
        !matches!(field.as_str(), "input_tokens" | "output_tokens")
            && !value.is_null()
            && value.as_f64() != Some(0.0)
    }) {
        return unavailable();
    }
    // Cohere excludes model-added tokens from billed_units; usage.tokens
    // remains the actual context count. No dollar total or discounted cache
    // partition is documented here, so do not manufacture either one.
    // https://docs.cohere.com/docs/how-does-cohere-pricing-work
    Some(BillingUsage::Tokens {
        text: BillingTokenCounts {
            input,
            output,
            ..Default::default()
        },
        audio: BillingTokenCounts::default(),
        reported_total: None,
    })
}

pub(super) fn response_stream<S>(
    source: S,
    model: String,
    api: String,
    provider: String,
    usage_observer: Option<crate::provider::UsageObserver>,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let mut state = StreamState::new(SseStream::new(source), model, api, provider);
    state.usage_observer = usage_observer;
    Box::pin(
        stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .fuse(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> StreamState<futures::stream::Empty<std::io::Result<Vec<u8>>>> {
        StreamState::new(
            SseStream::new(stream::empty()),
            "model".into(),
            "cohere-chat".into(),
            "cohere".into(),
        )
    }

    fn feed(
        state: &mut StreamState<futures::stream::Empty<std::io::Result<Vec<u8>>>>,
        event: &Value,
    ) -> Result<()> {
        state.process_event(&event.to_string())
    }

    fn start(index: u32, id: &str, arguments: &str) -> Value {
        json!({"type":"tool-call-start","index":index,"delta":{"message":{"tool_calls":{
            "id":id,"type":"function","function":{"name":"read","arguments":arguments}
        }}}})
    }

    fn delta(index: u32, arguments: &str) -> Value {
        json!({"type":"tool-call-delta","index":index,"delta":{"message":{"tool_calls":{
            "function":{"arguments":arguments}
        }}}})
    }

    fn end(reason: &str) -> Value {
        json!({"type":"message-end","delta":{"finish_reason":reason,"usage":{
            "tokens":{"input_tokens":2,"output_tokens":3}
        }}})
    }

    fn started() -> StreamState<futures::stream::Empty<std::io::Result<Vec<u8>>>> {
        let mut state = state();
        feed(&mut state, &json!({"type":"message-start"})).unwrap();
        state
    }

    #[test]
    fn indexed_interleaved_calls_keep_identity_arguments_and_start_order() {
        let mut state = started();
        feed(&mut state, &start(7, "call-a", "{\"path\":")).unwrap();
        feed(&mut state, &start(100, "call-b", "{\"path\":")).unwrap();
        feed(&mut state, &delta(100, "\"b.txt\"}")).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end","index":100})).unwrap();
        feed(&mut state, &delta(7, "\"a.txt\"}")).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end","index":7})).unwrap();
        feed(&mut state, &end("TOOL_CALL")).unwrap();
        let Some(StreamEvent::Done { reason, message }) = state.pending_events.back() else {
            panic!("Done");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        let calls: Vec<_> = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(calls[1].arguments["path"], "b.txt");
        assert_eq!(message.usage.total_tokens, 5);
    }

    #[test]
    fn indexless_recordings_work_only_for_one_active_call() {
        let mut state = started();
        let mut event = start(0, "legacy", "{}");
        event.as_object_mut().unwrap().remove("index");
        feed(&mut state, &event).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end"})).unwrap();
        feed(&mut state, &end("COMPLETE")).unwrap();
        assert!(matches!(
            state.pending_events.back(),
            Some(StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            })
        ));

        let mut state = started();
        feed(&mut state, &start(0, "a", "{}")).unwrap();
        feed(&mut state, &start(1, "b", "{}")).unwrap();
        assert!(feed(&mut state, &json!({"type":"tool-call-end"})).is_err());
    }

    #[test]
    fn malformed_tool_arguments_never_become_executable_null_calls() {
        for arguments in ["", "{", "null", "[]", "42", "{\"secret\":"] {
            let mut state = started();
            feed(&mut state, &start(0, "a", arguments)).unwrap();
            let error = feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert!(state.finished);
            assert!(state.pending_events.is_empty());
        }
    }

    #[test]
    fn duplicate_ids_and_reused_indices_are_rejected() {
        for second in [start(0, "b", "{}"), start(1, "a", "{}")] {
            let mut state = started();
            feed(&mut state, &start(0, "a", "{}")).unwrap();
            feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap();
            assert!(feed(&mut state, &second).is_err());
        }
    }

    #[test]
    fn missing_wrong_and_finished_tool_indices_are_rejected() {
        for event in [delta(1, "{}"), json!({"type":"tool-call-end","index":1})] {
            let mut state = started();
            feed(&mut state, &start(0, "a", "{}")).unwrap();
            assert!(feed(&mut state, &event).is_err());
        }
        let mut state = started();
        feed(&mut state, &start(0, "a", "{}")).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap();
        assert!(feed(&mut state, &delta(0, "{}")).is_err());
    }

    #[test]
    fn unfinished_calls_and_bare_done_cannot_complete_a_turn() {
        for terminal in [end("TOOL_CALL").to_string(), "[DONE]".to_string()] {
            let mut state = started();
            feed(&mut state, &start(0, "a", "{")).unwrap();
            assert!(state.process_event(&terminal).is_err());
            assert!(
                !state
                    .pending_events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Done { .. }))
            );
        }
    }

    #[test]
    fn tool_plan_is_streamed_and_retained_as_public_assistant_text() {
        let mut state = started();
        for text in ["I will ", "read the file."] {
            feed(
                &mut state,
                &json!({"type":"tool-plan-delta","delta":{"message":{"tool_plan":text}}}),
            )
            .unwrap();
        }
        feed(&mut state, &start(0, "a", "{}")).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap();
        feed(&mut state, &end("TOOL_CALL")).unwrap();
        assert_eq!(
            state
                .pending_events
                .iter()
                .filter(|event| matches!(event, StreamEvent::TextEnd { .. }))
                .count(),
            1
        );
        let Some(StreamEvent::Done { message, .. }) = state.pending_events.back() else {
            panic!("Done");
        };
        let ContentBlock::Text(plan) = &message.content[0] else {
            panic!("plan");
        };
        assert_eq!(plan.text, "I will read the file.");
    }

    #[test]
    fn content_kind_and_lifecycle_are_checked() {
        let open = json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}});
        for bad in [
            open.clone(),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"thinking":"wrong"}}}}),
            json!({"type":"content-end","index":1}),
            end("COMPLETE"),
        ] {
            let mut state = started();
            feed(&mut state, &open).unwrap();
            assert!(feed(&mut state, &bad).is_err());
        }
        let mut state = started();
        feed(&mut state, &open).unwrap();
        feed(&mut state, &json!({"type":"content-end","index":0})).unwrap();
        assert!(feed(&mut state, &json!({"type":"content-end","index":0})).is_err());
    }

    #[test]
    fn thinking_and_text_indices_do_not_collide_with_tool_indices() {
        let mut state = started();
        feed(&mut state, &json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"thinking","thinking":"Think"}}}})).unwrap();
        feed(&mut state, &json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"thinking":" more"}}}})).unwrap();
        feed(&mut state, &json!({"type":"content-end","index":0})).unwrap();
        feed(&mut state, &start(0, "a", "{}")).unwrap();
        feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap();
        feed(&mut state, &end("TOOL_CALL")).unwrap();
        assert!(state.pending_events.iter().any(|event| matches!(event,
            StreamEvent::ThinkingEnd { content, content_index: 0 } if content == "Think more")));
        assert!(state.pending_events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallEnd {
                content_index: 1,
                ..
            }
        )));
    }

    #[test]
    fn usage_saturates_and_explicit_failures_are_not_tool_successes() {
        for (finish, expected) in [
            ("MAX_TOKENS", StopReason::Length),
            ("ERROR", StopReason::Error),
        ] {
            let mut state = started();
            feed(&mut state, &start(0, "a", "{}")).unwrap();
            feed(&mut state, &json!({"type":"tool-call-end","index":0})).unwrap();
            let mut terminal = end(finish);
            terminal["delta"]["usage"]["tokens"]["input_tokens"] = json!(u64::MAX);
            feed(&mut state, &terminal).unwrap();
            let Some(StreamEvent::Done { reason, message }) = state.pending_events.back() else {
                panic!("Done");
            };
            assert_eq!(*reason, expected);
            assert_eq!(message.usage.total_tokens, u64::MAX);
        }
    }

    #[test]
    fn billed_units_are_independent_of_context_tokens_and_preserve_explicit_zero() {
        for (input, output) in [(json!(5), json!(2)), (json!(0.0), json!(0.0))] {
            let mut state = started();
            let mut terminal = end("COMPLETE");
            terminal["delta"]["usage"]["tokens"] =
                json!({"input_tokens":71.0,"output_tokens":26.0});
            terminal["delta"]["usage"]["billed_units"] =
                json!({"input_tokens":input,"output_tokens":output});
            feed(&mut state, &terminal).unwrap();
            let Some(StreamEvent::Done { message, .. }) = state.pending_events.back() else {
                panic!("Done");
            };
            assert_eq!(
                (
                    message.usage.input,
                    message.usage.output,
                    message.usage.total_tokens
                ),
                (71, 26, 97)
            );
            assert!(matches!(
                message.usage.billing,
                Some(BillingUsage::Tokens { .. })
            ));
            let Some(BillingUsage::Tokens {
                text,
                audio,
                reported_total,
            }) = message.usage.billing.as_ref()
            else {
                unreachable!();
            };
            assert_eq!(text.input, exact_token_count(&input).unwrap());
            assert_eq!(text.output, exact_token_count(&output).unwrap());
            assert_eq!(*audio, BillingTokenCounts::default());
            assert_eq!(*reported_total, None);
        }
        let mut state = started();
        feed(&mut state, &end("COMPLETE")).unwrap();
        let Some(StreamEvent::Done { message, .. }) = state.pending_events.back() else {
            panic!("Done");
        };
        assert!(
            message.usage.billing.is_none(),
            "absent billed_units retains legacy text-only pricing"
        );
    }

    #[test]
    fn incomplete_or_unsupported_billed_units_are_explicitly_unavailable() {
        for units in [
            json!({}),
            json!({"input_tokens":5}),
            json!({"input_tokens":null,"output_tokens":2}),
            json!({"input_tokens":-1,"output_tokens":2}),
            json!({"input_tokens":1.5,"output_tokens":2}),
            json!({"input_tokens":1e30,"output_tokens":2}),
            json!({"input_tokens":"private-invalid-value","output_tokens":2}),
            json!({"input_tokens":5,"output_tokens":2,"search_units":1}),
            json!({"input_tokens":5,"output_tokens":2,"images":1}),
            json!({"input_tokens":5,"output_tokens":2,"future_units":1}),
            json!({"input_tokens":5,"output_tokens":2,"future_units":"private-invalid-value"}),
            json!([]),
        ] {
            let billing = billed_usage(Some(&units)).unwrap();
            assert!(
                matches!(billing, BillingUsage::Unavailable { .. }),
                "{units}"
            );
            let encoded = serde_json::to_string(&billing).unwrap();
            assert!(!encoded.contains("private-invalid-value"));
            assert!(!encoded.contains("NaN"));
        }
        assert_eq!(billed_usage(None), None);
        assert_eq!(billed_usage(Some(&Value::Null)), None);
        assert_eq!(exact_token_count(&json!(u64::MAX)), Some(u64::MAX));
        assert_eq!(
            exact_token_count(&json!(18_446_744_073_709_551_616.0)),
            None
        );
        assert!(matches!(
            billed_usage(Some(
                &json!({"input_tokens":0,"output_tokens":0,"search_units":0.0})
            )),
            Some(BillingUsage::Tokens { .. })
        ));
    }

    #[test]
    fn oversized_arguments_and_sparse_indices_have_bounded_state() {
        let mut state = started();
        feed(&mut state, &start(u32::MAX, "a", "{}")).unwrap();
        assert_eq!(state.tools.len(), 1);
        assert_eq!(state.partial.content.len(), 1);
        assert!(
            feed(
                &mut state,
                &delta(u32::MAX, &"x".repeat(MAX_ARGUMENT_BYTES))
            )
            .is_err()
        );
    }

    #[test]
    fn block_and_accumulated_byte_limits_are_enforced() {
        let mut state = started();
        state.received_bytes = MAX_RESPONSE_BYTES;
        assert!(feed(&mut state, &start(0, "a", "{}")).is_err());
        let mut state = started();
        for index in 0..MAX_BLOCKS {
            feed(
                &mut state,
                &start(u32::try_from(index).unwrap(), &format!("a{index}"), "{}"),
            )
            .unwrap();
            feed(&mut state, &json!({"type":"tool-call-end","index":index})).unwrap();
            state.pending_events.clear();
        }
        assert!(feed(&mut state, &start(2000, "overflow", "{}")).is_err());
    }

    #[test]
    fn malformed_payloads_and_remote_errors_do_not_echo_content() {
        for data in [
            "{secret",
            "[DONE]",
            r#"{"type":"error","message":"secret"}"#,
        ] {
            let mut state = started();
            let error = state.process_event(data).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn terminal_events_require_a_message_and_a_known_finish_reason() {
        assert!(feed(&mut state(), &end("COMPLETE")).is_err());
        assert!(feed(&mut started(), &end("FUTURE_UNKNOWN")).is_err());
        assert!(feed(&mut started(), &end("TOOL_CALL")).is_err());
        let mut state = started();
        assert!(feed(&mut state, &json!({"type":"message-start"})).is_err());
    }

    #[test]
    fn driver_emits_one_terminal_error_then_stays_exhausted() {
        futures::executor::block_on(async {
            for data in ["", "data: [DONE]\n\n", "data: {bad json}\n\n"] {
                let mut events = response_stream(
                    stream::iter(vec![Ok(data.as_bytes().to_vec())]),
                    "m".into(),
                    "cohere-chat".into(),
                    "cohere".into(),
                    None,
                );
                assert!(events.next().await.unwrap().is_err());
                assert!(events.next().await.is_none());
                assert!(events.next().await.is_none());
            }
            let source = stream::iter(vec![Err(std::io::Error::other("secret transport detail"))]);
            let mut events = response_stream(
                source,
                "m".into(),
                "cohere-chat".into(),
                "cohere".into(),
                None,
            );
            assert!(
                !events
                    .next()
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
            assert!(events.next().await.is_none());
        });
    }

    #[test]
    fn driver_handles_one_byte_chunks_and_never_polls_after_native_completion() {
        futures::executor::block_on(async {
            let wire = format!(
                "data: {{\"type\":\"message-start\"}}\n\ndata: {}\n\n",
                end("COMPLETE")
            );
            let source = stream::iter(wire.into_bytes().into_iter().map(|byte| Ok(vec![byte])))
                .chain(stream::pending());
            let mut events = response_stream(
                source,
                "m".into(),
                "cohere-chat".into(),
                "cohere".into(),
                None,
            );
            assert!(matches!(
                events.next().await.unwrap().unwrap(),
                StreamEvent::Start { .. }
            ));
            assert!(matches!(
                events.next().await.unwrap().unwrap(),
                StreamEvent::Done { .. }
            ));
            assert!(events.next().await.is_none());
        });
    }
}
