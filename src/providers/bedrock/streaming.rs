//! Incremental Bedrock ConverseStream decoding.
//!
//! AWS event-stream frames are binary, not SSE. Validate both IEEE CRC32s
//! before interpreting headers or JSON, and never turn a truncated response
//! into a successful assistant turn. All work belongs to the returned stream;
//! dropping it drops the HTTP body and any partially assembled tool input.

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, RedactedThinkingContent, StopReason, StreamEvent, TextContent,
    ThinkingContent, ToolCall, Usage,
};
use base64::Engine as _;
use futures::{Stream, StreamExt, stream};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::pin::Pin;

pub(super) const CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";
// Local resource budgets, not validation of the service's protocol maxima.
const MAX_BUFFER_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BLOCKS: usize = 1024;
type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>;

fn invalid(message: &str) -> Error {
    Error::provider(
        "amazon-bedrock",
        format!("Invalid Bedrock event stream: {message}"),
    )
}

// The table index is bounded to 0..256 before conversion.
#[allow(clippy::cast_possible_truncation)]
const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    !bytes.iter().fold(u32::MAX, |crc, byte| {
        (crc >> 8) ^ TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize]
    })
}

fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    if count > bytes.len() {
        return Err(invalid("truncated frame header"));
    }
    let (value, rest) = bytes.split_at(count);
    *bytes = rest;
    Ok(value)
}

const fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[derive(Debug)]
struct Frame {
    headers: BTreeMap<String, Option<String>>,
    payload: Vec<u8>,
}

impl Frame {
    fn header(&self, name: &str) -> Result<&str> {
        self.headers
            .get(name)
            .and_then(Option::as_deref)
            .ok_or_else(|| invalid("missing or non-string event control header"))
    }
}

fn decode_headers(mut bytes: &[u8]) -> Result<BTreeMap<String, Option<String>>> {
    let mut headers = BTreeMap::new();
    while !bytes.is_empty() {
        let name_len = usize::from(take(&mut bytes, 1)?[0]);
        if name_len == 0 {
            return Err(invalid("empty event header name"));
        }
        let name = std::str::from_utf8(take(&mut bytes, name_len)?)
            .map_err(|_| invalid("event header name is not UTF-8"))?
            .to_string();
        let kind = take(&mut bytes, 1)?[0];
        let value = match kind {
            0 | 1 => None,
            2 => {
                take(&mut bytes, 1)?;
                None
            }
            3 => {
                take(&mut bytes, 2)?;
                None
            }
            4 => {
                take(&mut bytes, 4)?;
                None
            }
            5 | 8 => {
                take(&mut bytes, 8)?;
                None
            }
            9 => {
                take(&mut bytes, 16)?;
                None
            }
            6 | 7 => {
                let length = take(&mut bytes, 2)?;
                let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                let value = take(&mut bytes, length)?;
                if kind == 7 {
                    Some(
                        std::str::from_utf8(value)
                            .map_err(|_| invalid("event header value is not UTF-8"))?
                            .to_string(),
                    )
                } else {
                    None
                }
            }
            _ => return Err(invalid("unknown event header type")),
        };
        if headers.insert(name, value).is_some() {
            return Err(invalid("duplicate event header"));
        }
    }
    Ok(headers)
}

#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
    consumed: usize,
}

impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let remaining = self.buffer.len() - self.consumed;
        if bytes.len() > MAX_BUFFER_BYTES.saturating_sub(remaining) {
            return Err(invalid("local event-stream buffer budget exceeded"));
        }
        // Compact only when receiving another transport chunk, not once per
        // frame. A coalesced chunk can contain hundreds of tiny delta frames.
        if self.consumed != 0 {
            self.buffer.copy_within(self.consumed.., 0);
            self.buffer.truncate(remaining);
            self.consumed = 0;
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Frame>> {
        let bytes = &self.buffer[self.consumed..];
        if bytes.len() < 12 {
            return Ok(None);
        }
        // Verify the prelude before trusting either attacker-controlled length.
        if crc32(&bytes[..8]) != read_u32(&bytes[8..12]) {
            return Err(invalid("prelude CRC32 mismatch"));
        }
        let total = usize::try_from(read_u32(&bytes[..4]))
            .map_err(|_| invalid("frame length is not representable"))?;
        let header_len = usize::try_from(read_u32(&bytes[4..8]))
            .map_err(|_| invalid("header length is not representable"))?;
        if total < 16 || header_len > total - 16 {
            return Err(invalid("invalid frame lengths"));
        }
        if total > MAX_BUFFER_BYTES {
            return Err(invalid("local event-stream frame budget exceeded"));
        }
        if bytes.len() < total {
            return Ok(None);
        }
        if crc32(&bytes[..total - 4]) != read_u32(&bytes[total - 4..total]) {
            return Err(invalid("message CRC32 mismatch"));
        }
        let headers = decode_headers(&bytes[12..12 + header_len])?;
        let payload = bytes[12 + header_len..total - 4].to_vec();
        self.consumed += total;
        Ok(Some(Frame { headers, payload }))
    }

    fn finish(&self) -> Result<()> {
        if self.consumed != self.buffer.len() {
            return Err(invalid("unexpected EOF inside an event-stream frame"));
        }
        Ok(())
    }
}

struct OpenBlock {
    index: usize,
    tool_input: String,
    // Absence of input permits a zero-argument call; an explicitly supplied
    // empty stream is malformed JSON, not permission to invent arguments.
    tool_input_seen: bool,
    redacted: Vec<u8>,
}

struct MessageState {
    message: AssistantMessage,
    pending: VecDeque<StreamEvent>,
    open: BTreeMap<u64, OpenBlock>,
    seen: BTreeSet<u64>,
    // Tool IDs correlate subsequent results and cannot be reused even after
    // their content block closes. Wire block indices are a separate namespace.
    tool_ids: BTreeSet<String>,
    started: bool,
    stopped: bool,
    metadata_seen: bool,
    content_bytes: usize,
}

impl MessageState {
    fn new(model: String, provider: String) -> Self {
        Self {
            message: AssistantMessage {
                api: "bedrock-converse-stream".to_string(),
                model,
                provider,
                timestamp: chrono::Utc::now().timestamp_millis(),
                ..AssistantMessage::default()
            },
            pending: VecDeque::new(),
            open: BTreeMap::new(),
            seen: BTreeSet::new(),
            tool_ids: BTreeSet::new(),
            started: false,
            stopped: false,
            metadata_seen: false,
            content_bytes: 0,
        }
    }

    fn reserve_content(&mut self, length: usize) -> Result<()> {
        self.content_bytes = self.content_bytes.saturating_add(length);
        if self.content_bytes > MAX_CONTENT_BYTES {
            return Err(invalid("local assistant-content budget exceeded"));
        }
        Ok(())
    }

    fn add_block(&mut self, id: u64, content: ContentBlock) -> Result<usize> {
        if self.seen.len() >= MAX_BLOCKS || !self.seen.insert(id) {
            return Err(invalid(
                "duplicate content block or local block budget exceeded",
            ));
        }
        let index = self.message.content.len();
        self.message.content.push(content);
        self.open.insert(
            id,
            OpenBlock {
                index,
                tool_input: String::new(),
                tool_input_seen: false,
                redacted: Vec::new(),
            },
        );
        Ok(index)
    }

    /// The only content-block kind Bedrock opens explicitly is a tool call;
    /// text and thinking blocks are created by their first delta. Split out of
    /// `event` so that dispatcher stays under the line budget.
    fn start_block(&mut self, value: &Value) -> Result<()> {
        let id = block_id(value)?;
        let start = value
            .get("start")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("missing or invalid block start"))?;
        if start.len() != 1 {
            return Err(invalid("ambiguous content block start"));
        }
        let tool = start
            .get("toolUse")
            .ok_or_else(|| invalid("unsupported content block start"))?;
        // A server_tool_use is performed remotely. Mapping it to ToolCall
        // would authorize a second, local execution with a different trust
        // boundary. Fail closed until the model has a remote-tool vocabulary.
        if tool.get("type").is_some() {
            return Err(invalid(
                "server-managed or unknown tool type is not a local tool request",
            ));
        }
        let tool_id = string(tool, "toolUseId")?;
        let name = string(tool, "name")?;
        if !valid_tool_identity(tool_id, true) || !valid_tool_identity(name, false) {
            return Err(invalid("invalid tool-call identity"));
        }
        if self.tool_ids.contains(tool_id) {
            return Err(invalid("duplicate tool-call id"));
        }
        self.reserve_content(tool_id.len().saturating_add(name.len()))?;
        let index = self.add_block(
            id,
            ContentBlock::ToolCall(ToolCall {
                id: tool_id.to_string(),
                name: name.to_string(),
                arguments: Value::Null,
                thought_signature: None,
            }),
        )?;
        self.tool_ids.insert(tool_id.to_string());
        self.pending.push_back(StreamEvent::ToolCallStart {
            content_index: index,
            id: tool_id.to_string(),
            name: name.to_string(),
        });
        Ok(())
    }

    /// A complete block is evidence of content, not permission to execute it.
    /// The shared agent admits tools on Stop/Length too, so a Bedrock response
    /// containing local calls must explicitly finish with tool_use. Refusals
    /// stay on its fail-closed Error path; they are never retried as success.
    fn stop_message(&mut self, value: &Value) -> Result<()> {
        if !self.open.is_empty() {
            return Err(invalid(
                "messageStop arrived with unfinished content blocks",
            ));
        }
        let wire_reason = string(value, "stopReason")?;
        if wire_reason == "tool_use" && self.tool_ids.is_empty() {
            return Err(invalid("tool_use stop reason has no local tool calls"));
        }
        let (reason, failure) = match wire_reason {
            "end_turn" | "stop_sequence" => (StopReason::Stop, None),
            "tool_use" => (StopReason::ToolUse, None),
            "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
            "guardrail_intervened" => (
                StopReason::Error,
                Some("Bedrock guardrail_intervened: response blocked; local tool calls withheld"),
            ),
            "content_filtered" => (
                StopReason::Error,
                Some("Bedrock content_filtered: response refused; local tool calls withheld"),
            ),
            "malformed_tool_use" => (
                StopReason::Error,
                Some("Bedrock malformed_tool_use: invalid tool request; local tool calls withheld"),
            ),
            "malformed_model_output" => (
                StopReason::Error,
                Some(
                    "Bedrock malformed_model_output: invalid model response; local tool calls withheld",
                ),
            ),
            // Unknown provider strings can contain credentials or terminal
            // controls. Keep a fixed diagnostic rather than echoing the wire.
            _ => (
                StopReason::Error,
                Some("Bedrock returned an unsupported stop reason; local tool calls withheld"),
            ),
        };
        let failure = failure.or_else(|| {
            if self.tool_ids.is_empty() || reason == StopReason::ToolUse {
                None
            } else if reason == StopReason::Length {
                Some("Bedrock response was truncated before tool_use authorization; local tool calls withheld")
            } else {
                Some("Bedrock ended without tool_use authorization; local tool calls withheld")
            }
        });
        self.message.stop_reason = if failure.is_some() {
            StopReason::Error
        } else {
            reason
        };
        self.message.error_message = failure.map(str::to_string);
        self.stopped = true;
        Ok(())
    }

    fn event(&mut self, kind: &str, value: &Value) -> Result<()> {
        if kind == "messageStart" {
            if self.started || string(value, "role")? != "assistant" {
                return Err(invalid("duplicate messageStart or non-assistant role"));
            }
            self.started = true;
            self.pending.push_back(StreamEvent::Start {
                partial: self.message.clone(),
            });
            return Ok(());
        }
        if !self.started {
            return Err(invalid("response event arrived before messageStart"));
        }
        if kind == "metadata" {
            if !self.stopped || self.metadata_seen {
                return Err(invalid(
                    "metadata arrived before messageStop or more than once",
                ));
            }
            let usage = value
                .get("usage")
                .ok_or_else(|| invalid("metadata has no usage"))?;
            let input = token_count(usage, "inputTokens")?;
            let output = token_count(usage, "outputTokens")?;
            let cache_read = token_count(usage, "cacheReadInputTokens")?;
            let cache_write = token_count(usage, "cacheWriteInputTokens")?;
            let total = match usage.get("totalTokens") {
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid totalTokens"))?,
                None => input
                    .saturating_add(output)
                    .saturating_add(cache_read)
                    .saturating_add(cache_write),
            };
            self.message.usage = Usage {
                input,
                output,
                cache_read,
                cache_write,
                total_tokens: total,
                ..Usage::default()
            };
            self.metadata_seen = true;
            return Ok(());
        }
        if self.stopped {
            return Err(invalid("content arrived after messageStop"));
        }
        match kind {
            "contentBlockStart" => self.start_block(value)?,
            "contentBlockDelta" => self.delta(value)?,
            "contentBlockStop" => self.end_block(block_id(value)?)?,
            "messageStop" => self.stop_message(value)?,
            _ => return Err(invalid("unsupported ConverseStream event type")),
        }
        Ok(())
    }

    fn delta(&mut self, value: &Value) -> Result<()> {
        let id = block_id(value)?;
        let delta = value
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("missing content delta"))?;
        if delta.len() != 1 {
            return Err(invalid("ambiguous content delta"));
        }
        if let Some(text) = delta.get("text") {
            let text = text
                .as_str()
                .ok_or_else(|| invalid("non-string text delta"))?;
            self.reserve_content(text.len())?;
            let index = if let Some(block) = self.open.get(&id) {
                block.index
            } else {
                let index = self.add_block(id, ContentBlock::Text(TextContent::new("")))?;
                self.pending.push_back(StreamEvent::TextStart {
                    content_index: index,
                });
                index
            };
            let ContentBlock::Text(block) = &mut self.message.content[index] else {
                return Err(invalid("text delta changed the content block type"));
            };
            block.text.push_str(text);
            self.pending.push_back(StreamEvent::TextDelta {
                content_index: index,
                delta: text.to_string(),
            });
        } else if let Some(tool) = delta.get("toolUse") {
            let input = string(tool, "input")?;
            self.reserve_content(input.len())?;
            let block = self
                .open
                .get_mut(&id)
                .ok_or_else(|| invalid("tool delta arrived before tool start"))?;
            if !matches!(self.message.content[block.index], ContentBlock::ToolCall(_)) {
                return Err(invalid("tool delta changed the content block type"));
            }
            block.tool_input_seen = true;
            block.tool_input.push_str(input);
            self.pending.push_back(StreamEvent::ToolCallDelta {
                content_index: block.index,
                delta: input.to_string(),
            });
        } else if let Some(reasoning) = delta.get("reasoningContent") {
            self.reasoning(id, reasoning)?;
        } else {
            return Err(invalid("unsupported content delta"));
        }
        Ok(())
    }

    fn reasoning(&mut self, id: u64, value: &Value) -> Result<()> {
        let fields = value
            .as_object()
            .ok_or_else(|| invalid("invalid reasoning delta"))?;
        if fields.len() != 1 {
            return Err(invalid("ambiguous reasoning delta"));
        }
        if let Some(encoded) = fields.get("redactedContent") {
            let encoded = encoded
                .as_str()
                .ok_or_else(|| invalid("invalid redacted reasoning"))?;
            self.reserve_content(encoded.len())?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| invalid("invalid redacted reasoning base64"))?;
            let index = if let Some(block) = self.open.get(&id) {
                block.index
            } else {
                self.add_block(
                    id,
                    ContentBlock::RedactedThinking(RedactedThinkingContent {
                        data: String::new(),
                    }),
                )?
            };
            if !matches!(
                self.message.content[index],
                ContentBlock::RedactedThinking(_)
            ) {
                return Err(invalid("redacted reasoning changed the content block type"));
            }
            // Each JSON blob is independently base64 encoded. Concatenating
            // the encoded strings would corrupt padding and replay bytes.
            self.open
                .get_mut(&id)
                .ok_or_else(|| invalid("missing reasoning block"))?
                .redacted
                .extend_from_slice(&bytes);
            return Ok(());
        }
        let (signature, text) = if fields.contains_key("signature") {
            (true, string(value, "signature")?)
        } else {
            (false, string(value, "text")?)
        };
        self.reserve_content(text.len())?;
        let index = if let Some(block) = self.open.get(&id) {
            block.index
        } else {
            let index = self.add_block(
                id,
                ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                }),
            )?;
            self.pending.push_back(StreamEvent::ThinkingStart {
                content_index: index,
            });
            index
        };
        let ContentBlock::Thinking(block) = &mut self.message.content[index] else {
            return Err(invalid("reasoning delta changed the content block type"));
        };
        if signature {
            block
                .thinking_signature
                .get_or_insert_with(String::new)
                .push_str(text);
        } else {
            block.thinking.push_str(text);
            self.pending.push_back(StreamEvent::ThinkingDelta {
                content_index: index,
                delta: text.to_string(),
            });
        }
        Ok(())
    }

    fn end_block(&mut self, id: u64) -> Result<()> {
        let block = self
            .open
            .remove(&id)
            .ok_or_else(|| invalid("stop for an unopened or closed block"))?;
        let content_index = block.index;
        match &mut self.message.content[content_index] {
            ContentBlock::Text(text) => self.pending.push_back(StreamEvent::TextEnd {
                content_index,
                content: text.text.clone(),
            }),
            ContentBlock::Thinking(thinking) => self.pending.push_back(StreamEvent::ThinkingEnd {
                content_index,
                content: thinking.thinking.clone(),
            }),
            ContentBlock::RedactedThinking(redacted) => {
                redacted.data = base64::engine::general_purpose::STANDARD.encode(block.redacted);
            }
            ContentBlock::ToolCall(tool) => {
                let arguments = if block.tool_input_seen {
                    serde_json::from_str::<Value>(&block.tool_input)
                        .map_err(|_| invalid("malformed tool input JSON"))?
                } else {
                    serde_json::json!({})
                };
                if !arguments.is_object() {
                    return Err(invalid("tool input must be a complete JSON object"));
                }
                tool.arguments = arguments;
                self.pending.push_back(StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call: tool.clone(),
                });
            }
            _ => return Err(invalid("unsupported completed content block")),
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<StreamEvent> {
        if !self.started || !self.stopped || !self.open.is_empty() {
            return Err(invalid("unexpected EOF before a complete messageStop"));
        }
        let message = std::mem::take(&mut self.message);
        if message.stop_reason == StopReason::Error {
            Ok(StreamEvent::Error {
                reason: StopReason::Error,
                error: message,
            })
        } else {
            Ok(StreamEvent::Done {
                reason: message.stop_reason,
                message,
            })
        }
    }
}

// Bedrock's service identity grammar, checked before allocation or publication:
// ToolUseBlockStart.name is [a-zA-Z0-9_-]+ and toolUseId additionally allows
// '.', ':'; both are at most 64 bytes. IDs are preserved, never normalized.
fn valid_tool_identity(value: &str, is_id: bool) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-')
                || (is_id && matches!(byte, b'.' | b':'))
        })
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing or invalid event field"))
}

fn block_id(value: &Value) -> Result<u64> {
    value
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("missing or invalid contentBlockIndex"))
}

fn token_count(value: &Value, field: &str) -> Result<u64> {
    value.get(field).map_or(Ok(0), |value| {
        value
            .as_u64()
            .ok_or_else(|| invalid("invalid token usage counter"))
    })
}

fn dispatch(frame: &Frame, state: &mut MessageState, secrets: &[String]) -> Result<()> {
    let message_type = frame.header(":message-type")?;
    if message_type == "error" || message_type == "exception" {
        let (code, message) = if message_type == "error" {
            (
                frame.header(":error-code")?,
                frame.header(":error-message")?.to_string(),
            )
        } else {
            let code = frame.header(":exception-type")?;
            let value: Value = serde_json::from_slice(&frame.payload)
                .map_err(|_| invalid("invalid exception JSON"))?;
            (
                code,
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream failed")
                    .to_string(),
            )
        };
        let status = match code {
            "throttlingException" => " (HTTP 429)",
            "internalServerException" => " (HTTP 500)",
            "serviceUnavailableException" => " (HTTP 503)",
            "modelStreamErrorException" => " (HTTP 424)",
            "validationException" => " (HTTP 400)",
            _ => "",
        };
        let details = super::bedrock_error_snippet(&format!("{code}{status}: {message}"), secrets);
        return Err(Error::provider(
            &state.message.provider,
            format!("Bedrock stream exception: {details}"),
        ));
    }
    if message_type != "event" {
        return Err(invalid("unknown event message type"));
    }
    if frame.headers.contains_key(":content-type") {
        let content_type = frame.header(":content-type")?;
        if !content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("application/json")
        {
            return Err(invalid("unexpected event payload content type"));
        }
    }
    let kind = frame.header(":event-type")?;
    let value =
        serde_json::from_slice(&frame.payload).map_err(|_| invalid("invalid event JSON"))?;
    state.event(kind, &value)
}

pub(super) fn from_bytes(
    source: ByteStream,
    model: String,
    provider: String,
    secrets: Vec<String>,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
    struct State {
        source: Option<ByteStream>,
        decoder: Decoder,
        message: MessageState,
        secrets: Vec<String>,
        finished: bool,
    }
    let state = State {
        source: Some(source),
        decoder: Decoder::default(),
        message: MessageState::new(model, provider),
        secrets,
        finished: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        loop {
            if let Some(event) = state.message.pending.pop_front() {
                return Some((Ok(event), state));
            }
            let outcome = match state.decoder.next() {
                Ok(Some(frame)) => dispatch(&frame, &mut state.message, &state.secrets),
                Err(error) => Err(error),
                Ok(None) => {
                    let chunk = match state.source.as_mut() {
                        Some(source) => source.next().await,
                        None => None,
                    };
                    match chunk {
                        Some(Ok(bytes)) => state.decoder.push(&bytes),
                        Some(Err(error)) => {
                            let details =
                                super::bedrock_error_snippet(&error.to_string(), &state.secrets);
                            Err(Error::provider(
                                &state.message.message.provider,
                                format!("Bedrock stream transport error: {details}"),
                            ))
                        }
                        None => {
                            state.finished = true;
                            state.source = None;
                            let outcome =
                                state.decoder.finish().and_then(|()| state.message.finish());
                            return Some((outcome, state));
                        }
                    }
                }
            };
            if let Err(error) = outcome {
                // Drop the socket before yielding the terminal error. A caller
                // is not obliged to poll again or immediately drop our stream.
                state.source = None;
                state.finished = true;
                state.message.pending.clear();
                return Some((Err(error), state));
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll};

    fn header(name: &str, value: &str, target: &mut Vec<u8>) {
        target.push(u8::try_from(name.len()).unwrap());
        target.extend_from_slice(name.as_bytes());
        target.push(7);
        target.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        target.extend_from_slice(value.as_bytes());
    }

    fn raw_frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(
            &u32::try_from(16 + headers.len() + payload.len())
                .unwrap()
                .to_be_bytes(),
        );
        frame.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend_from_slice(headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame
    }

    fn event(kind: &str, payload: &Value) -> Vec<u8> {
        let mut headers = Vec::new();
        header(":message-type", "event", &mut headers);
        header(":event-type", kind, &mut headers);
        header(":content-type", "application/json", &mut headers);
        raw_frame(&headers, &serde_json::to_vec(&payload).unwrap())
    }

    fn start() -> Vec<u8> {
        event("messageStart", &json!({"role": "assistant"}))
    }

    fn stop() -> Vec<u8> {
        event("messageStop", &json!({"stopReason": "end_turn"}))
    }

    fn text() -> Vec<u8> {
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "héllo"}}),
        )
    }

    fn block_stop(index: u64) -> Vec<u8> {
        event("contentBlockStop", &json!({"contentBlockIndex": index}))
    }

    fn collect(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamEvent>> {
        futures::executor::block_on(
            from_bytes(
                Box::pin(stream::iter(chunks.into_iter().map(Ok))),
                "model-a".to_string(),
                "provider-a".to_string(),
                Vec::new(),
            )
            .collect(),
        )
    }

    #[test]
    fn crc_uses_ieee_not_castagnoli() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn frames_decode_across_every_transport_split() {
        let frame = text();
        for split in 0..frame.len() {
            let mut decoder = Decoder::default();
            decoder.push(&frame[..split]).unwrap();
            assert!(decoder.next().unwrap().is_none());
            decoder.push(&frame[split..]).unwrap();
            let parsed = decoder.next().unwrap().unwrap();
            assert_eq!(parsed.header(":event-type").unwrap(), "contentBlockDelta");
            let payload: Value = serde_json::from_slice(&parsed.payload).unwrap();
            assert_eq!(payload["delta"]["text"], "héllo");
            assert!(decoder.next().unwrap().is_none());
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn coalesced_frames_and_single_byte_chunks_produce_identical_messages() {
        let bytes = [start(), text(), block_stop(0), stop()].concat();
        for chunks in [
            vec![bytes.clone()],
            bytes.iter().map(|byte| vec![*byte]).collect(),
        ] {
            let result = collect(chunks);
            assert_eq!(result.len(), 5);
            assert!(
                matches!(&result[0], Ok(StreamEvent::Start { partial }) if partial.content.is_empty())
            );
            assert!(
                matches!(&result[2], Ok(StreamEvent::TextDelta { delta, .. }) if delta == "héllo")
            );
            let Ok(StreamEvent::Done { message, .. }) = result.last().unwrap() else {
                panic!("{result:?}")
            };
            assert_eq!(message.provider, "provider-a");
            assert_eq!(message.model, "model-a");
            assert_eq!(message.api, "bedrock-converse-stream");
        }
    }

    #[test]
    fn text_delta_arrives_while_the_response_is_still_open() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        tx.unbounded_send(Ok([start(), text()].concat())).unwrap();
        let mut output = from_bytes(Box::pin(rx), "m".into(), "p".into(), Vec::new());
        for expected in ["start", "text_start", "delta"] {
            let item = output
                .next()
                .now_or_never()
                .expect("must not wait for the tail")
                .unwrap()
                .unwrap();
            assert!(matches!(
                (expected, item),
                ("start", StreamEvent::Start { .. })
                    | ("text_start", StreamEvent::TextStart { .. })
                    | ("delta", StreamEvent::TextDelta { .. })
            ));
        }
        assert!(output.next().now_or_never().is_none());
        tx.unbounded_send(Ok([block_stop(0), stop()].concat()))
            .unwrap();
        drop(tx);
        let rest = futures::executor::block_on(output.collect::<Vec<_>>());
        assert!(matches!(rest.last(), Some(Ok(StreamEvent::Done { .. }))));
    }

    #[test]
    fn truncated_or_corrupt_frames_never_emit_done() {
        let frame = text();
        for end in 1..frame.len() {
            let result = collect(vec![start(), frame[..end].to_vec()]);
            assert!(result.last().unwrap().is_err(), "truncation at {end}");
            assert!(
                !result
                    .iter()
                    .any(|item| matches!(item, Ok(StreamEvent::Done { .. })))
            );
        }
        for offset in [0, 8, 15, frame.len() - 1] {
            let mut damaged = frame.clone();
            damaged[offset] ^= 1;
            let result = collect(vec![start(), damaged, stop()]);
            assert!(result.last().unwrap().is_err());
        }
        let result = collect(vec![start(), text(), block_stop(0)]);
        assert!(
            result
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("messageStop")
        );
        let result = collect(vec![start(), stop(), vec![0]]);
        assert!(
            result.last().unwrap().is_err(),
            "partial trailing frame is still corruption"
        );
    }

    #[test]
    fn invalid_lengths_are_rejected_before_waiting_for_the_payload() {
        for (total, header_len) in [(15_u32, 0_u32), (16, 1), (u32::MAX, 0)] {
            let mut prelude = Vec::new();
            prelude.extend_from_slice(&total.to_be_bytes());
            prelude.extend_from_slice(&header_len.to_be_bytes());
            prelude.extend_from_slice(&crc32(&prelude).to_be_bytes());
            let mut decoder = Decoder::default();
            decoder.push(&prelude).unwrap();
            assert!(decoder.next().is_err());
        }
    }

    #[test]
    fn typed_headers_are_skipped_but_duplicates_and_bad_encodings_are_rejected() {
        let mut headers = Vec::new();
        for (kind, length) in [
            (0, 0),
            (1, 0),
            (2, 1),
            (3, 2),
            (4, 4),
            (5, 8),
            (8, 8),
            (9, 16),
        ] {
            headers.extend_from_slice(&[1, b'a' + kind, kind]);
            headers.extend(std::iter::repeat_n(0, length));
        }
        headers.extend_from_slice(&[1, b'z', 6, 0, 2, 0, 255]);
        header(":message-type", "event", &mut headers);
        let parsed = decode_headers(&headers).unwrap();
        assert_eq!(parsed[":message-type"].as_deref(), Some("event"));
        header(":message-type", "exception", &mut headers);
        assert!(decode_headers(&headers).is_err());
        for bad in [
            vec![0],
            vec![1, 255, 0],
            vec![1, b'x', 7, 0, 1, 255],
            vec![1, b'x', 255],
        ] {
            assert!(decode_headers(&bad).is_err());
        }
    }

    #[test]
    fn tool_arguments_are_assembled_and_validated_before_tool_end() {
        let result = collect(vec![
            start(),
            event(
                "contentBlockStart",
                &json!({"contentBlockIndex": 7, "start": {"toolUse": {"toolUseId": "call-a", "name": "read"}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 7, "delta": {"toolUse": {"input": "{\"path\":"}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 7, "delta": {"toolUse": {"input": "\"a.txt\"}"}}}),
            ),
            block_stop(7),
            event("messageStop", &json!({"stopReason": "tool_use"})),
        ]);
        assert!(result.iter().all(Result::is_ok), "{result:?}");
        let tool = result
            .iter()
            .find_map(|item| match item {
                Ok(StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                }) => {
                    assert_eq!(*content_index, 0);
                    Some(tool_call)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(tool.id, "call-a");
        assert_eq!(tool.arguments, json!({"path": "a.txt"}));
        assert!(matches!(
            result.last(),
            Some(Ok(StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }))
        ));

        let result = collect(vec![
            start(),
            event(
                "contentBlockStart",
                &json!({"contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "a", "name": "read"}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": "{"}}}),
            ),
            block_stop(0),
            stop(),
        ]);
        assert!(result.last().unwrap().is_err());
        assert!(
            !result
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::ToolCallEnd { .. })))
        );
    }

    #[test]
    fn reasoning_and_redacted_bytes_survive_without_becoming_visible_text() {
        let result = collect(vec![
            start(),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "Think."}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig-"}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "part2"}}}),
            ),
            block_stop(0),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 1, "delta": {"reasoningContent": {"redactedContent": "AA=="}}}),
            ),
            event(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 1, "delta": {"reasoningContent": {"redactedContent": "AQ=="}}}),
            ),
            block_stop(1),
            stop(),
        ]);
        assert!(result.iter().all(Result::is_ok), "{result:?}");
        assert!(
            !result
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::TextDelta { .. })))
        );
        let Ok(StreamEvent::Done { message, .. }) = result.last().unwrap() else {
            panic!("{result:?}")
        };
        let ContentBlock::Thinking(thinking) = &message.content[0] else {
            panic!()
        };
        assert_eq!(thinking.thinking, "Think.");
        assert_eq!(thinking.thinking_signature.as_deref(), Some("sig-part2"));
        let ContentBlock::RedactedThinking(redacted) = &message.content[1] else {
            panic!()
        };
        assert_eq!(redacted.data, "AAE=");
    }

    #[test]
    fn metadata_after_message_stop_is_included_in_done() {
        let result = collect(vec![
            start(),
            stop(),
            event(
                "metadata",
                &json!({"usage": {
                    "inputTokens": 7, "outputTokens": 3, "cacheReadInputTokens": 20,
                    "cacheWriteInputTokens": 5, "totalTokens": 35
                }}),
            ),
        ]);
        let Ok(StreamEvent::Done { message, .. }) = result.last().unwrap() else {
            panic!("{result:?}")
        };
        assert_eq!(message.usage.input, 7);
        assert_eq!(message.usage.output, 3);
        assert_eq!(message.usage.cache_read, 20);
        assert_eq!(message.usage.cache_write, 5);
        assert_eq!(message.usage.total_tokens, 35);
    }

    #[test]
    fn invalid_lifecycle_cannot_be_committed_as_success() {
        for frames in [
            vec![text(), stop()],
            vec![start(), start()],
            vec![start(), text(), stop()],
            vec![start(), block_stop(0)],
            vec![start(), text(), block_stop(0), text()],
            vec![start(), stop(), text()],
            vec![start(), event("metadata", &json!({"usage": {}}))],
        ] {
            let result = collect(frames);
            assert!(result.last().unwrap().is_err(), "{result:?}");
        }
    }

    #[test]
    fn streamed_exceptions_are_redacted_and_terminal() {
        let mut headers = Vec::new();
        header(":message-type", "exception", &mut headers);
        header(":exception-type", "throttlingException", &mut headers);
        let frame = raw_frame(&headers, br#"{"message":"slow down secret-key-canary"}"#);
        let result: Vec<_> = futures::executor::block_on(
            from_bytes(
                Box::pin(stream::iter(vec![Ok(start()), Ok(frame), Ok(stop())])),
                "m".into(),
                "custom-bedrock".into(),
                vec!["secret-key-canary".into()],
            )
            .collect(),
        );
        assert_eq!(result.len(), 2);
        let error = result[1].as_ref().unwrap_err().to_string();
        assert!(error.contains("HTTP 429"), "{error}");
        assert!(!error.contains("secret-key-canary"), "{error}");
        assert!(error.contains("custom-bedrock"), "{error}");
    }

    struct DropProbe {
        frame: Option<Vec<u8>>,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for DropProbe {
        type Item = std::io::Result<Vec<u8>>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.frame
                .take()
                .map_or(Poll::Pending, |frame| Poll::Ready(Some(Ok(frame))))
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn malformed_stream_releases_transport_before_yielding_error() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut output = from_bytes(
            Box::pin(DropProbe {
                frame: Some(vec![0; 12]),
                dropped: Arc::clone(&dropped),
            }),
            "m".into(),
            "p".into(),
            Vec::new(),
        );
        assert!(output.next().now_or_never().unwrap().unwrap().is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(output.next().now_or_never().unwrap().is_none());
    }

    #[test]
    fn dropping_a_pending_stream_releases_transport() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut output = from_bytes(
            Box::pin(DropProbe {
                frame: None,
                dropped: Arc::clone(&dropped),
            }),
            "m".into(),
            "p".into(),
            Vec::new(),
        );
        assert!(output.next().now_or_never().is_none());
        drop(output);
        assert!(dropped.load(Ordering::SeqCst));
    }

    fn tool_start(index: u64, id: &str, name: &str) -> Vec<u8> {
        event(
            "contentBlockStart",
            &json!({"contentBlockIndex": index, "start": {
                "toolUse": {"toolUseId": id, "name": name}
            }}),
        )
    }

    fn tool_input(index: u64, input: &str) -> Vec<u8> {
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": index, "delta": {"toolUse": {"input": input}}}),
        )
    }

    fn tool_stop() -> Vec<u8> {
        event("messageStop", &json!({"stopReason": "tool_use"}))
    }

    fn assert_one_terminal_error(result: &[Result<StreamEvent>]) {
        assert!(result.last().is_some_and(Result::is_err), "{result:?}");
        assert_eq!(result.iter().filter(|item| item.is_err()).count(), 1);
        assert!(
            !result
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::Done { .. }))),
            "{result:?}"
        );
    }

    #[test]
    fn duplicate_tool_ids_are_rejected_even_with_distinct_or_closed_wire_indices() {
        for close_first in [false, true] {
            let mut frames = vec![start(), tool_start(7, "call-a", "read")];
            if close_first {
                frames.push(block_stop(7));
            }
            frames.extend([
                tool_start(u64::MAX, "call-a", "write"),
                block_stop(u64::MAX),
                tool_stop(),
            ]);
            let result = collect(frames);
            assert_one_terminal_error(&result);
            assert_eq!(
                result
                    .iter()
                    .filter(|item| matches!(item, Ok(StreamEvent::ToolCallStart { .. })))
                    .count(),
                1
            );
            assert!(
                result
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate tool-call id")
            );
        }
    }

    #[test]
    fn tool_identity_grammar_is_checked_before_publishing_the_call() {
        for invalid_identity in ["", " ", " leading", "a/b", "a\nb", "é", &"x".repeat(65)] {
            for invalid_id in [false, true] {
                let (id, name) = if invalid_id {
                    (invalid_identity, "read")
                } else {
                    ("call-a", invalid_identity)
                };
                let result = collect(vec![
                    start(),
                    tool_start(0, id, name),
                    block_stop(0),
                    tool_stop(),
                ]);
                assert_one_terminal_error(&result);
                assert!(!result.iter().any(|item| matches!(
                    item,
                    Ok(StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. })
                )));
            }
        }
        assert!(valid_tool_identity("provider:call.1_a-b", true));
        assert!(!valid_tool_identity("read.file", false));
        assert!(!valid_tool_identity("read:file", false));
        assert!(valid_tool_identity(&"x".repeat(64), true));
        assert!(valid_tool_identity(&"x".repeat(64), false));
    }

    #[test]
    fn server_managed_and_unknown_tool_types_never_enter_local_execution() {
        for kind in [
            json!("server_tool_use"),
            json!("future_type"),
            Value::Null,
            json!(false),
        ] {
            let result = collect(vec![
                start(),
                event(
                    "contentBlockStart",
                    &json!({"contentBlockIndex": 0, "start": {
                        "toolUse": {"toolUseId": "call-a", "name": "bash", "type": kind}
                    }}),
                ),
                tool_input(0, "{\"command\":\"must-not-run\"}"),
                block_stop(0),
                tool_stop(),
            ]);
            assert_one_terminal_error(&result);
            assert!(!result.iter().any(|item| matches!(
                item,
                Ok(StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. })
            )));
            assert!(
                result
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("not a local tool request")
            );
        }
    }

    #[test]
    fn ambiguous_start_unions_cannot_hide_a_second_content_kind() {
        for extra in ["toolResult", "text", "futureBlock"] {
            let mut payload = json!({"contentBlockIndex": 0, "start": {
                "toolUse": {"toolUseId": "call-a", "name": "read"}
            }});
            payload["start"]
                .as_object_mut()
                .unwrap()
                .insert(extra.to_string(), json!({}));
            let result = collect(vec![
                start(),
                event("contentBlockStart", &payload),
                tool_stop(),
            ]);
            assert_one_terminal_error(&result);
            assert!(
                !result
                    .iter()
                    .any(|item| matches!(item, Ok(StreamEvent::ToolCallStart { .. })))
            );
        }
    }

    #[test]
    fn supplied_tool_arguments_must_finish_as_a_json_object() {
        for input in ["", " ", "null", "[]", "42", "true", "\"text\"", "{", "{}{}"] {
            let result = collect(vec![
                start(),
                tool_start(0, "call-a", "read"),
                tool_input(0, input),
                block_stop(0),
                tool_stop(),
            ]);
            assert_one_terminal_error(&result);
            assert!(
                !result
                    .iter()
                    .any(|item| matches!(item, Ok(StreamEvent::ToolCallEnd { .. }))),
                "{input}: {result:?}"
            );
        }
    }

    #[test]
    fn absent_arguments_and_explicit_empty_objects_are_valid_zero_argument_calls() {
        for fragments in [vec![], vec!["{}"], vec!["", "{", "}"]] {
            let mut frames = vec![start(), tool_start(0, "call-a", "read")];
            frames.extend(fragments.into_iter().map(|input| tool_input(0, input)));
            frames.extend([block_stop(0), tool_stop()]);
            let result = collect(frames);
            assert!(result.iter().all(Result::is_ok), "{result:?}");
            let Some(Ok(StreamEvent::Done { reason, message })) = result.last() else {
                panic!("expected completed tool turn");
            };
            assert_eq!(*reason, StopReason::ToolUse);
            let ContentBlock::ToolCall(call) = &message.content[0] else {
                panic!("expected tool call");
            };
            assert_eq!(call.arguments, json!({}));
        }
    }

    #[test]
    fn parallel_calls_keep_dense_content_indices_and_independent_argument_buffers() {
        let bytes = [
            start(),
            text(),
            block_stop(0),
            tool_start(u64::MAX, "provider:call.1", "read"),
            tool_start(7, "provider:call.2", "read"),
            tool_input(7, "{\"path\":"),
            tool_input(u64::MAX, "{\"path\":\"first.txt\"}"),
            block_stop(u64::MAX),
            tool_input(7, "\"second.txt\"}"),
            block_stop(7),
            tool_stop(),
        ]
        .concat();
        for chunks in [
            vec![bytes.clone()],
            bytes.chunks(3).map(<[u8]>::to_vec).collect(),
        ] {
            let result = collect(chunks);
            assert!(result.iter().all(Result::is_ok), "{result:?}");
            let completed: Vec<_> = result
                .iter()
                .filter_map(|item| match item {
                    Ok(StreamEvent::ToolCallEnd {
                        content_index,
                        tool_call,
                    }) => Some((*content_index, tool_call)),
                    _ => None,
                })
                .collect();
            assert_eq!(completed.len(), 2);
            assert_eq!(completed[0].0, 1);
            assert_eq!(completed[0].1.id, "provider:call.1");
            assert_eq!(completed[0].1.arguments, json!({"path": "first.txt"}));
            assert_eq!(completed[1].0, 2);
            assert_eq!(completed[1].1.id, "provider:call.2");
            assert_eq!(completed[1].1.arguments, json!({"path": "second.txt"}));
            let Some(Ok(StreamEvent::Done { message, .. })) = result.last() else {
                panic!("expected Done");
            };
            for (index, call) in completed {
                let ContentBlock::ToolCall(stored) = &message.content[index] else {
                    panic!("expected stored call");
                };
                assert_eq!(stored.id, call.id);
                assert_eq!(stored.arguments, call.arguments);
            }
        }
    }

    #[test]
    fn tool_use_without_a_tool_is_not_a_completed_agent_step() {
        for frames in [
            vec![start(), tool_stop()],
            vec![start(), text(), block_stop(0), tool_stop()],
        ] {
            let result = collect(frames);
            assert_one_terminal_error(&result);
            assert!(
                result
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("no local tool calls")
            );
        }
    }

    #[test]
    fn rejected_tool_start_retires_a_still_open_transport_before_the_error() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut output = from_bytes(
            Box::pin(DropProbe {
                frame: Some(
                    [
                        start(),
                        tool_start(0, "call-a", "read"),
                        tool_start(1, "call-a", "write"),
                    ]
                    .concat(),
                ),
                dropped: Arc::clone(&dropped),
            }),
            "m".into(),
            "p".into(),
            Vec::new(),
        );
        for _ in 0..2 {
            assert!(output.next().now_or_never().unwrap().unwrap().is_ok());
        }
        assert!(output.next().now_or_never().unwrap().unwrap().is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(output.next().now_or_never().unwrap().is_none());
    }

    fn usage_metadata() -> Vec<u8> {
        event(
            "metadata",
            &json!({"usage": {
                "inputTokens": 7,
                "outputTokens": 3,
                "cacheReadInputTokens": 20,
                "cacheWriteInputTokens": 5,
                "totalTokens": 35
            }}),
        )
    }

    fn terminal_failure(result: &[Result<StreamEvent>]) -> &AssistantMessage {
        assert!(result.iter().all(Result::is_ok), "{result:?}");
        assert!(
            !result
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::Done { .. }))),
            "{result:?}"
        );
        assert_eq!(
            result
                .iter()
                .filter(|item| matches!(item, Ok(StreamEvent::Error { .. })))
                .count(),
            1,
            "{result:?}"
        );
        let Some(Ok(StreamEvent::Error { reason, error })) = result.last() else {
            panic!("expected one terminal Error: {result:?}");
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        error
    }

    #[test]
    fn completed_local_calls_require_terminal_tool_use_authorization() {
        for reason in [
            "end_turn",
            "stop_sequence",
            "max_tokens",
            "model_context_window_exceeded",
            "guardrail_intervened",
            "content_filtered",
            "malformed_tool_use",
            "malformed_model_output",
            "future_outcome",
        ] {
            let result = collect(vec![
                start(),
                tool_start(7, "call-a", "write"),
                tool_input(7, "{\"path\":\"retained.txt\"}"),
                block_stop(7),
                event("messageStop", &json!({"stopReason": reason})),
                usage_metadata(),
            ]);
            // Even a validated ToolCallEnd cannot make the final outcome a
            // success. The call remains available for diagnostics, not execution.
            assert_eq!(
                result
                    .iter()
                    .filter(|item| matches!(item, Ok(StreamEvent::ToolCallEnd { .. })))
                    .count(),
                1,
                "{reason}: {result:?}"
            );
            let message = terminal_failure(&result);
            assert!(
                message
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("local tool calls withheld")
            );
            assert_eq!(message.usage.total_tokens, 35);
            let ContentBlock::ToolCall(call) = &message.content[0] else {
                panic!("expected retained diagnostic call");
            };
            assert_eq!(call.id, "call-a");
            assert_eq!(call.arguments, json!({"path": "retained.txt"}));
        }
    }

    #[test]
    fn text_only_stops_preserve_normal_and_truncation_outcomes() {
        for (wire_reason, expected) in [
            ("end_turn", StopReason::Stop),
            ("stop_sequence", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("model_context_window_exceeded", StopReason::Length),
        ] {
            let result = collect(vec![
                start(),
                text(),
                block_stop(0),
                event("messageStop", &json!({"stopReason": wire_reason})),
                usage_metadata(),
            ]);
            assert!(result.iter().all(Result::is_ok), "{result:?}");
            assert!(
                !result
                    .iter()
                    .any(|item| matches!(item, Ok(StreamEvent::Error { .. })))
            );
            let Some(Ok(StreamEvent::Done { reason, message })) = result.last() else {
                panic!("expected Done for {wire_reason}: {result:?}");
            };
            assert_eq!(*reason, expected);
            assert_eq!(message.stop_reason, expected);
            assert!(message.error_message.is_none());
            assert_eq!(message.usage.total_tokens, 35);
            let ContentBlock::Text(content) = &message.content[0] else {
                panic!("expected retained text");
            };
            assert_eq!(content.text, "héllo");
        }
    }

    #[test]
    fn service_failure_outcomes_preserve_text_identity_and_usage() {
        for wire_reason in [
            "guardrail_intervened",
            "content_filtered",
            "malformed_tool_use",
            "malformed_model_output",
        ] {
            let result = collect(vec![
                start(),
                text(),
                block_stop(0),
                event("messageStop", &json!({"stopReason": wire_reason})),
                usage_metadata(),
            ]);
            let message = terminal_failure(&result);
            assert!(
                message
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains(wire_reason)
            );
            assert_eq!(message.provider, "provider-a");
            assert_eq!(message.model, "model-a");
            assert_eq!(message.api, "bedrock-converse-stream");
            assert_eq!(message.usage.input, 7);
            assert_eq!(message.usage.output, 3);
            assert_eq!(message.usage.cache_read, 20);
            assert_eq!(message.usage.cache_write, 5);
            assert_eq!(message.usage.total_tokens, 35);
            let ContentBlock::Text(content) = &message.content[0] else {
                panic!("expected retained text");
            };
            assert_eq!(content.text, "héllo");
        }
    }

    #[test]
    fn unknown_stop_outcomes_do_not_echo_untrusted_wire_strings() {
        for wire_reason in ["", "future_status secret-canary\u{1b}[31m\n", "TOOL_USE"] {
            let result = collect(vec![
                start(),
                event("messageStop", &json!({"stopReason": wire_reason})),
            ]);
            let message = terminal_failure(&result);
            assert_eq!(
                message.error_message.as_deref(),
                Some("Bedrock returned an unsupported stop reason; local tool calls withheld")
            );
        }
    }

    #[test]
    fn terminal_failures_wait_for_metadata_then_finish_exactly_once() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        tx.unbounded_send(Ok([
            start(),
            text(),
            block_stop(0),
            event("messageStop", &json!({"stopReason": "content_filtered"})),
        ]
        .concat()))
            .unwrap();
        let mut output = from_bytes(Box::pin(rx), "m".into(), "p".into(), Vec::new());
        for _ in 0..4 {
            let item = output.next().now_or_never().unwrap().unwrap().unwrap();
            assert!(!matches!(
                item,
                StreamEvent::Done { .. } | StreamEvent::Error { .. }
            ));
        }
        assert!(output.next().now_or_never().is_none());
        tx.unbounded_send(Ok(usage_metadata())).unwrap();
        drop(tx);
        let result = output.next().now_or_never().unwrap().unwrap().unwrap();
        let StreamEvent::Error { reason, error } = result else {
            panic!("expected terminal Error");
        };
        assert_eq!(reason, StopReason::Error);
        assert_eq!(error.usage.total_tokens, 35);
        assert_eq!(error.content.len(), 1);
        assert!(output.next().now_or_never().unwrap().is_none());
    }

    #[test]
    fn failed_stop_does_not_hide_a_corrupt_or_exception_tail() {
        let mut exception_headers = Vec::new();
        header(":message-type", "exception", &mut exception_headers);
        header(
            ":exception-type",
            "internalServerException",
            &mut exception_headers,
        );
        let exception = raw_frame(&exception_headers, br#"{"message":"tail failed"}"#);
        for tail in [vec![0], vec![0; 12], exception] {
            let result = collect(vec![
                start(),
                text(),
                block_stop(0),
                event(
                    "messageStop",
                    &json!({"stopReason": "malformed_model_output"}),
                ),
                tail,
            ]);
            assert_one_terminal_error(&result);
            assert!(
                !result
                    .iter()
                    .any(|item| matches!(item, Ok(StreamEvent::Error { .. })))
            );
        }
    }

    #[test]
    fn absent_or_non_string_stop_reasons_cannot_default_to_success() {
        for payload in [
            json!({}),
            json!({"stopReason": null}),
            json!({"stopReason": 1}),
            json!({"stopReason": false}),
            json!({"stopReason": []}),
            json!({"stopReason": {}}),
        ] {
            let result = collect(vec![start(), event("messageStop", &payload)]);
            assert_one_terminal_error(&result);
        }
    }
}
