//! Native Cursor CLI provider (`agent.v1.AgentService`).
//!
//! Talks to Cursor's `api2.cursor.sh` backend using the **Connect** streaming
//! protocol (`connectrpc.com`): a POST to `/agent.v1.AgentService/Run` whose
//! request and response bodies are sequences of *enveloped* Protocol Buffers
//! frames — `[flag: u8][length: u32 big-endian][payload]` — under
//! `content-type: application/connect+proto`.
//!
//! The message shapes and field numbers were extracted from the generated
//! `agent.proto` descriptor shipped by the two open-source reference proxies
//! (`ephraimduncan/opencode-cursor` and `ndraiman/pi-cursor-provider`), which
//! reimplement the same protocol the official Cursor CLI speaks.
//!
//! ## Transport caveat (important)
//!
//! Cursor's official CLI drives this endpoint over **HTTP/2** (it even sends
//! `te: trailers`). Pi's HTTP client is HTTP/1.1-only. The Connect *streaming*
//! protocol is deliberately specified to work over HTTP/1.1 as well — unlike
//! gRPC, it does not depend on HTTP trailers, because the terminal metadata
//! rides in a final end-of-stream frame (flag bit `0x02`) instead. This
//! provider therefore issues the Connect request over the available HTTP/1.1
//! transport. Whether Cursor's servers accept an HTTP/1.1 Connect request is
//! **not verified here** (it requires live Cursor credentials); a rejection
//! surfaces as an ordinary HTTP error from [`Provider::stream`].
//!
//! ## Scope
//!
//! The single-turn text/thinking streaming path (system prompt + latest user
//! message → streamed assistant text, thinking and token usage) is fully
//! implemented and unit-tested. Cursor manages multi-turn history and tool
//! execution through a stateful checkpoint/blob (`KvClientMessage`) channel and
//! a separate `ExecServerMessage` tool round-trip; those are intentionally *not*
//! reimplemented here (they cannot be reconstructed correctly without live
//! protocol capture) and are documented as follow-up work rather than guessed.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingContent, Usage,
    UserContent,
};
use crate::provider::{Context, Provider, StreamEvent, StreamOptions};
use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{self, Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use uuid::Uuid;

/// Default Cursor Connect endpoint (full RPC path).
pub const CURSOR_API_URL: &str = "https://api2.cursor.sh/agent.v1.AgentService/Run";

/// Client-version marker the Cursor backend expects. Mirrors the value used by
/// the reference proxies; Cursor gates some behaviour on the `x-cursor-client-*`
/// markers rather than requiring an obfuscated checksum header.
const CURSOR_CLIENT_VERSION: &str = "cli-2026.01.09-231024f";

/// The `api` identifier reported for streamed events / session logs.
const CURSOR_API_NAME: &str = "cursor-agent";

// ============================================================================
// Minimal protobuf wire codec (proto3 subset)
// ============================================================================

/// A tiny hand-rolled Protocol Buffers reader/writer covering exactly the wire
/// features the Cursor `agent.v1` messages use: varints, length-delimited
/// fields (strings, bytes, nested messages) and the two fixed-width wire types
/// (so unknown fields can be skipped without corrupting the stream).
mod pb {
    /// Wire type: base-128 varint.
    const WIRE_VARINT: u64 = 0;
    /// Wire type: 64-bit fixed.
    const WIRE_I64: u64 = 1;
    /// Wire type: length-delimited.
    const WIRE_LEN: u64 = 2;
    /// Wire type: 32-bit fixed.
    const WIRE_I32: u64 = 5;

    /// Append a base-128 varint to `out`.
    #[allow(clippy::cast_possible_truncation)] // masked to 7 bits before the cast
    pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn write_tag(out: &mut Vec<u8>, field: u32, wire: u64) {
        write_varint(out, (u64::from(field) << 3) | wire);
    }

    /// Write a length-delimited field (used for strings, bytes and nested
    /// messages).
    pub fn write_len_delim(out: &mut Vec<u8>, field: u32, data: &[u8]) {
        write_tag(out, field, WIRE_LEN);
        write_varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    /// Write a UTF-8 string field.
    pub fn write_string(out: &mut Vec<u8>, field: u32, value: &str) {
        write_len_delim(out, field, value.as_bytes());
    }

    /// A single decoded field value. Length-delimited payloads borrow from the
    /// message buffer.
    #[derive(Clone, Copy)]
    pub enum Field<'a> {
        Varint(u64),
        Len(&'a [u8]),
        I64(u64),
        I32(u32),
    }

    /// A forward-only reader over one protobuf message body.
    pub struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        pub const fn new(buf: &'a [u8]) -> Self {
            Self { buf, pos: 0 }
        }

        fn read_varint(&mut self) -> Option<u64> {
            let mut result: u64 = 0;
            let mut shift: u32 = 0;
            loop {
                let byte = *self.buf.get(self.pos)?;
                self.pos += 1;
                result |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Some(result);
                }
                shift += 7;
                if shift >= 64 {
                    return None; // malformed varint
                }
            }
        }

        /// Read the next `(field_number, value)` pair, or `None` at end of
        /// buffer / on malformed input. Unknown wire types (groups) stop
        /// iteration rather than mis-parse.
        pub fn next_field(&mut self) -> Option<(u32, Field<'a>)> {
            if self.pos >= self.buf.len() {
                return None;
            }
            let tag = self.read_varint()?;
            let field = u32::try_from(tag >> 3).ok()?;
            match tag & 0x7 {
                WIRE_VARINT => Some((field, Field::Varint(self.read_varint()?))),
                WIRE_LEN => {
                    let len = usize::try_from(self.read_varint()?).ok()?;
                    let end = self.pos.checked_add(len)?;
                    let slice = self.buf.get(self.pos..end)?;
                    self.pos = end;
                    Some((field, Field::Len(slice)))
                }
                WIRE_I64 => {
                    let end = self.pos.checked_add(8)?;
                    let bytes = self.buf.get(self.pos..end)?;
                    self.pos = end;
                    Some((
                        field,
                        Field::I64(u64::from_le_bytes(bytes.try_into().ok()?)),
                    ))
                }
                WIRE_I32 => {
                    let end = self.pos.checked_add(4)?;
                    let bytes = self.buf.get(self.pos..end)?;
                    self.pos = end;
                    Some((
                        field,
                        Field::I32(u32::from_le_bytes(bytes.try_into().ok()?)),
                    ))
                }
                _ => None, // start/end group: not used by agent.v1
            }
        }
    }
}

// ============================================================================
// Connect envelope framing
// ============================================================================

/// Connect end-of-stream flag (bit 1): the final frame carries JSON trailers.
const CONNECT_END_STREAM_FLAG: u8 = 0b0000_0010;
/// Connect compression flag (bit 0).
const CONNECT_COMPRESSED_FLAG: u8 = 0b0000_0001;
/// Upper bound on a single Connect frame's declared payload length. The length
/// prefix is server-controlled, so this caps how much we will buffer for one
/// frame; agent stream frames are small (per-delta), and 64 MiB is far above any
/// legitimate frame while still bounding memory against a hostile length prefix.
const MAX_CONNECT_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Wrap a payload in a Connect envelope: `[flag][len: u32 BE][payload]`.
#[allow(clippy::cast_possible_truncation)] // request payloads are far below u32::MAX
fn encode_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(flags);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// A fully-decoded Connect frame.
#[derive(Debug)]
struct ConnectFrame {
    end_stream: bool,
    compressed: bool,
    payload: Vec<u8>,
}

/// Incremental parser that reassembles complete Connect frames from arbitrarily
/// chunked byte input.
#[derive(Default)]
struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Pop the next complete frame if one is fully buffered.
    ///
    /// Returns `None` while the next frame is still incomplete, `Some(Ok(frame))`
    /// once a full frame is buffered, and `Some(Err(..))` if the frame's declared
    /// length exceeds [`MAX_CONNECT_FRAME_LEN`]. The length is attacker-influenced
    /// (a 4-byte prefix up to `u32::MAX`), so without the cap a single hostile or
    /// buggy prefix would make the buffer grow without bound waiting for bytes
    /// that never arrive — an out-of-memory vector. We can't resync past a frame
    /// whose length we don't trust, so an over-cap length is a fatal stream error.
    fn next_frame(&mut self) -> Option<Result<ConnectFrame>> {
        if self.buf.len() < 5 {
            return None;
        }
        let flags = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_CONNECT_FRAME_LEN {
            return Some(Err(Error::api(format!(
                "Cursor stream frame length {len} exceeds the maximum of {MAX_CONNECT_FRAME_LEN} bytes"
            ))));
        }
        let total = 5usize.checked_add(len)?;
        if self.buf.len() < total {
            return None;
        }
        let payload = self.buf[5..total].to_vec();
        self.buf.drain(0..total);
        Some(Ok(ConnectFrame {
            end_stream: flags & CONNECT_END_STREAM_FLAG != 0,
            compressed: flags & CONNECT_COMPRESSED_FLAG != 0,
            payload,
        }))
    }
}

// ============================================================================
// Request encoding: pi Context -> AgentClientMessage bytes
// ============================================================================

// Field numbers (verified against agent.proto, package agent.v1):
//   AgentClientMessage.run_request              = 1
//   AgentRunRequest.action                      = 2
//   AgentRunRequest.model_details               = 3
//   AgentRunRequest.conversation_id             = 5
//   AgentRunRequest.custom_system_prompt        = 8
//   ConversationAction.user_message_action      = 1
//   UserMessageAction.user_message              = 1
//   UserMessage.text                            = 1
//   UserMessage.message_id                      = 2
//   ModelDetails.model_id                        = 1

/// Serialize a single-turn `AgentClientMessage { run_request: AgentRunRequest }`.
fn build_run_request(
    model_id: &str,
    system_prompt: Option<&str>,
    user_text: &str,
    conversation_id: &str,
    message_id: &str,
) -> Vec<u8> {
    // UserMessage { text = 1, message_id = 2 }
    let mut user_message = Vec::new();
    pb::write_string(&mut user_message, 1, user_text);
    pb::write_string(&mut user_message, 2, message_id);

    // UserMessageAction { user_message = 1 }
    let mut user_message_action = Vec::new();
    pb::write_len_delim(&mut user_message_action, 1, &user_message);

    // ConversationAction { user_message_action = 1 }
    let mut action = Vec::new();
    pb::write_len_delim(&mut action, 1, &user_message_action);

    // ModelDetails { model_id = 1 }
    let mut model_details = Vec::new();
    pb::write_string(&mut model_details, 1, model_id);

    // AgentRunRequest { action = 2, model_details = 3, conversation_id = 5,
    //                   custom_system_prompt = 8 }
    let mut run_request = Vec::new();
    pb::write_len_delim(&mut run_request, 2, &action);
    pb::write_len_delim(&mut run_request, 3, &model_details);
    pb::write_string(&mut run_request, 5, conversation_id);
    if let Some(system) = system_prompt
        && !system.is_empty()
    {
        pb::write_string(&mut run_request, 8, system);
    }

    // AgentClientMessage { run_request = 1 }
    let mut message = Vec::new();
    pb::write_len_delim(&mut message, 1, &run_request);
    message
}

/// Extract the text of the most recent user message from the conversation.
///
/// Cursor's `Run` request carries a single `UserMessage`; the agent loop resends
/// the full history each turn, so the latest user turn is what drives this
/// request. Prior turns (assistant/tool history) are managed server-side by
/// Cursor via its checkpoint channel, which is not reimplemented here.
fn latest_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some(render_user_content(&user.content)),
        _ => None,
    })
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let ContentBlock::Text(text) = block {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text.text);
                }
            }
            out
        }
    }
}

// ============================================================================
// Response decoding: AgentServerMessage bytes -> semantic events
// ============================================================================

// Field numbers (verified against agent.proto, package agent.v1):
//   AgentServerMessage.interaction_update             = 1
//   AgentServerMessage.conversation_checkpoint_update = 3 (ConversationStateStructure)
//   InteractionUpdate.text_delta                      = 1  (TextDeltaUpdate.text = 1)
//   InteractionUpdate.thinking_delta                  = 4  (ThinkingDeltaUpdate.text = 1)
//   InteractionUpdate.token_delta                     = 8  (TokenDeltaUpdate.tokens = 1)
//   InteractionUpdate.turn_ended                      = 14
//   ConversationStateStructure.token_details          = 5  (ConversationTokenDetails.used_tokens = 1)

/// A semantic event decoded from one `AgentServerMessage` frame.
#[derive(Debug, PartialEq, Eq)]
enum ServerEvent {
    TextDelta(String),
    ThinkingDelta(String),
    TokenDelta(i64),
    TurnEnded,
    UsedTokens(u64),
}

/// Decode one `AgentServerMessage` protobuf body into semantic events.
fn decode_server_message(bytes: &[u8]) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    let mut reader = pb::Reader::new(bytes);
    while let Some((field, value)) = reader.next_field() {
        match (field, value) {
            (1, pb::Field::Len(inner)) => decode_interaction_update(inner, &mut events),
            (3, pb::Field::Len(inner)) => {
                if let Some(used) = decode_checkpoint_used_tokens(inner) {
                    events.push(ServerEvent::UsedTokens(used));
                }
            }
            _ => {}
        }
    }
    events
}

fn decode_interaction_update(bytes: &[u8], events: &mut Vec<ServerEvent>) {
    let mut reader = pb::Reader::new(bytes);
    while let Some((field, value)) = reader.next_field() {
        match (field, value) {
            (1, pb::Field::Len(inner)) => {
                if let Some(text) = decode_delta_text(inner) {
                    events.push(ServerEvent::TextDelta(text));
                }
            }
            (4, pb::Field::Len(inner)) => {
                if let Some(text) = decode_delta_text(inner) {
                    events.push(ServerEvent::ThinkingDelta(text));
                }
            }
            (8, pb::Field::Len(inner)) => {
                if let Some(tokens) = decode_token_delta(inner) {
                    events.push(ServerEvent::TokenDelta(tokens));
                }
            }
            (14, _) => events.push(ServerEvent::TurnEnded),
            _ => {}
        }
    }
}

/// Read field 1 (a string) of a `TextDeltaUpdate` / `ThinkingDeltaUpdate`.
fn decode_delta_text(bytes: &[u8]) -> Option<String> {
    let mut reader = pb::Reader::new(bytes);
    while let Some((field, value)) = reader.next_field() {
        if let (1, pb::Field::Len(raw)) = (field, value) {
            return Some(String::from_utf8_lossy(raw).into_owned());
        }
    }
    None
}

/// Read `TokenDeltaUpdate.tokens` (field 1, int32 encoded as a varint).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn decode_token_delta(bytes: &[u8]) -> Option<i64> {
    let mut reader = pb::Reader::new(bytes);
    while let Some((field, value)) = reader.next_field() {
        if let (1, pb::Field::Varint(raw)) = (field, value) {
            // proto3 `int32` is sign-extended to a 64-bit varint on the wire;
            // truncating to i32 then widening recovers the signed value.
            return Some(i64::from(raw as i32));
        }
    }
    None
}

/// Read `ConversationStateStructure.token_details.used_tokens` (5 -> 1).
fn decode_checkpoint_used_tokens(bytes: &[u8]) -> Option<u64> {
    let mut reader = pb::Reader::new(bytes);
    while let Some((field, value)) = reader.next_field() {
        if let (5, pb::Field::Len(token_details)) = (field, value) {
            let mut inner = pb::Reader::new(token_details);
            while let Some((f, v)) = inner.next_field() {
                if let (1, pb::Field::Varint(used)) = (f, v) {
                    return Some(used);
                }
            }
        }
    }
    None
}

// ============================================================================
// Streaming state machine: semantic events -> StreamEvent
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

/// Per-request streaming state: buffers Connect frames from the HTTP body and
/// turns decoded `AgentServerMessage`s into pi [`StreamEvent`]s.
struct StreamState {
    source: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>,
    frames: FrameBuffer,
    partial: AssistantMessage,
    pending: VecDeque<StreamEvent>,
    /// The currently-open content block (index + kind), if any.
    open_block: Option<(usize, BlockKind)>,
    output_tokens: u64,
    used_tokens: Option<u64>,
    error_message: Option<String>,
    started: bool,
    finalized: bool,
    done: bool,
    transient_error_count: usize,
}

impl StreamState {
    fn new(
        source: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>,
        partial: AssistantMessage,
    ) -> Self {
        Self {
            source,
            frames: FrameBuffer::default(),
            partial,
            pending: VecDeque::new(),
            open_block: None,
            output_tokens: 0,
            used_tokens: None,
            error_message: None,
            started: false,
            finalized: false,
            done: false,
            transient_error_count: 0,
        }
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            self.pending.push_back(StreamEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    /// Close the currently-open content block, emitting its `*End` event.
    fn close_open_block(&mut self) {
        let Some((index, kind)) = self.open_block.take() else {
            return;
        };
        match kind {
            BlockKind::Text => {
                let content = match self.partial.content.get(index) {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                self.pending.push_back(StreamEvent::TextEnd {
                    content_index: index,
                    content,
                });
            }
            BlockKind::Thinking => {
                let content = match self.partial.content.get(index) {
                    Some(ContentBlock::Thinking(t)) => t.thinking.clone(),
                    _ => String::new(),
                };
                self.pending.push_back(StreamEvent::ThinkingEnd {
                    content_index: index,
                    content,
                });
            }
        }
    }

    /// Ensure a content block of `kind` is open, returning its index. Switching
    /// kinds closes the previous block first.
    fn ensure_block(&mut self, kind: BlockKind) -> usize {
        if let Some((index, open_kind)) = self.open_block {
            if open_kind == kind {
                return index;
            }
            self.close_open_block();
        }
        let index = self.partial.content.len();
        match kind {
            BlockKind::Text => {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
                self.pending.push_back(StreamEvent::TextStart {
                    content_index: index,
                });
            }
            BlockKind::Thinking => {
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));
                self.pending.push_back(StreamEvent::ThinkingStart {
                    content_index: index,
                });
            }
        }
        self.open_block = Some((index, kind));
        index
    }

    fn apply(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::TextDelta(delta) => {
                if delta.is_empty() {
                    return;
                }
                self.ensure_started();
                let index = self.ensure_block(BlockKind::Text);
                if let Some(ContentBlock::Text(text)) = self.partial.content.get_mut(index) {
                    text.text.push_str(&delta);
                }
                self.pending.push_back(StreamEvent::TextDelta {
                    content_index: index,
                    delta,
                });
            }
            ServerEvent::ThinkingDelta(delta) => {
                if delta.is_empty() {
                    return;
                }
                self.ensure_started();
                let index = self.ensure_block(BlockKind::Thinking);
                if let Some(ContentBlock::Thinking(thinking)) = self.partial.content.get_mut(index)
                {
                    thinking.thinking.push_str(&delta);
                }
                self.pending.push_back(StreamEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                });
            }
            ServerEvent::TokenDelta(tokens) => {
                if tokens > 0 {
                    self.output_tokens = self
                        .output_tokens
                        .saturating_add(u64::try_from(tokens).unwrap_or(0));
                }
            }
            ServerEvent::UsedTokens(used) => {
                self.used_tokens = Some(used);
            }
            ServerEvent::TurnEnded => {
                // Turn completion is confirmed by the Connect end-of-stream
                // frame (or the body closing); nothing to emit here.
            }
        }
    }

    /// Process one Connect frame, buffering the resulting pi events.
    fn process_frame(&mut self, frame: &ConnectFrame) -> Result<()> {
        if frame.compressed {
            return Err(Error::api(
                "Cursor stream sent a compressed Connect frame; compression is not supported",
            ));
        }
        if frame.end_stream {
            // Terminal frame: JSON trailers, optionally carrying an error.
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&frame.payload)
                && let Some(error) = value.get("error")
            {
                let message = error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Cursor stream returned an error")
                    .to_string();
                self.error_message = Some(message);
            }
            self.finalize();
            // The end-of-stream frame is the protocol terminator: stop reading so
            // we neither process data frames that arrive after `Done` nor block on
            // `source.next()` if the server holds the HTTP body open. `finalize`
            // has already enqueued the terminal `Done`/`Error` into `pending`,
            // which `next_event` drains before it observes `done`.
            self.done = true;
            return Ok(());
        }
        for event in decode_server_message(&frame.payload) {
            self.apply(event);
        }
        Ok(())
    }

    /// Emit the closing events (open-block `*End`, then `Done`/`Error`).
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.ensure_started();
        self.close_open_block();

        let output = self.output_tokens;
        let total = self.used_tokens.unwrap_or(output);
        // Cursor reports only an aggregate token count and has no prompt-cache
        // tiers, so `input` (per pi's #121 convention, cache reads excluded)
        // is the non-output remainder.
        let input = total.saturating_sub(output);
        self.partial.usage = Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            total_tokens: total,
            ..Usage::default()
        };

        if let Some(message) = self.error_message.take() {
            self.partial.stop_reason = StopReason::Error;
            self.partial.error_message = Some(message);
            self.pending.push_back(StreamEvent::Error {
                reason: StopReason::Error,
                error: self.partial.clone(),
            });
        } else {
            self.partial.stop_reason = StopReason::Stop;
            self.pending.push_back(StreamEvent::Done {
                reason: StopReason::Stop,
                message: self.partial.clone(),
            });
        }
    }

    /// Produce the next [`StreamEvent`], reading and framing more of the HTTP
    /// body as needed. Returns `None` once the stream is fully drained.
    async fn next_event(&mut self) -> Option<Result<StreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if self.done {
                return None;
            }

            match self.frames.next_frame() {
                Some(Ok(frame)) => {
                    if let Err(err) = self.process_frame(&frame) {
                        self.done = true;
                        return Some(Err(err));
                    }
                    continue;
                }
                Some(Err(err)) => {
                    // Malformed framing (e.g. an over-cap length prefix): the
                    // stream can't be resynced, so surface it and stop reading.
                    self.done = true;
                    return Some(Err(err));
                }
                None => {}
            }

            match self.source.next().await {
                Some(Ok(chunk)) => {
                    self.transient_error_count = 0;
                    self.frames.push(&chunk);
                }
                Some(Err(err)) => {
                    // Treat the same transient I/O kinds as the SSE providers do.
                    const MAX_CONSECUTIVE_TRANSIENT_ERRORS: usize = 5;
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WriteZero
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) {
                        self.transient_error_count += 1;
                        if self.transient_error_count <= MAX_CONSECUTIVE_TRANSIENT_ERRORS {
                            continue;
                        }
                    }
                    self.done = true;
                    return Some(Err(Error::sse(&err)));
                }
                None => {
                    // Body closed: finalize with whatever we accumulated.
                    self.finalize();
                    self.done = true;
                }
            }
        }
    }
}

// ============================================================================
// Provider
// ============================================================================

/// Native Cursor CLI provider.
pub struct CursorProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
}

impl CursorProvider {
    /// Create a new Cursor provider for `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: CURSOR_API_URL.to_string(),
            provider: "cursor".to_string(),
        }
    }

    /// Override the provider name reported in streamed events.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Override the Connect endpoint URL.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Use a custom HTTP client (VCR / test harness).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
}

/// Resolve the Cursor access token from the request options / environment.
///
/// Cursor's own CLI keeps a browser-issued JWT; pi feeds provider credentials
/// through `StreamOptions::api_key` (populated by `/login` or the auth store),
/// falling back to the `CURSOR_API_KEY` / `CURSOR_ACCESS_TOKEN` env vars.
fn pick_token(inline: Option<&str>, env_token: Option<&str>) -> Option<String> {
    inline
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            env_token
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
}

#[async_trait]
impl Provider for CursorProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &str {
        CURSOR_API_NAME
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let env_token = std::env::var("CURSOR_API_KEY")
            .ok()
            .or_else(|| std::env::var("CURSOR_ACCESS_TOKEN").ok());
        let token =
            pick_token(options.api_key.as_deref(), env_token.as_deref()).ok_or_else(|| {
                Error::provider(
                    &self.provider,
                    "Missing Cursor credentials. Run /login cursor or set CURSOR_API_KEY.",
                )
            })?;

        let user_text = latest_user_text(&context.messages)
            .ok_or_else(|| Error::provider(&self.provider, "No user message to send to Cursor"))?;
        let system_prompt = context.system_prompt.as_deref();
        let conversation_id = Uuid::new_v4().to_string();
        let message_id = Uuid::new_v4().to_string();
        let request_id = Uuid::new_v4().to_string();

        let request_body = build_run_request(
            &self.model,
            system_prompt,
            &user_text,
            &conversation_id,
            &message_id,
        );
        let framed = encode_frame(0, &request_body);

        let request = self
            .client
            .post(&self.base_url)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("authorization", format!("Bearer {token}"))
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
            .header("x-request-id", request_id)
            .body(framed);

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|err| format!("<failed to read body: {err}>"));
            return Err(Error::provider(
                &self.provider,
                format!("Cursor API error (HTTP {status}): {body}"),
            ));
        }

        let partial = AssistantMessage {
            api: CURSOR_API_NAME.to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            timestamp: Utc::now().timestamp_millis(),
            stop_details: None,
            ..AssistantMessage::default()
        };

        let state = StreamState::new(response.bytes_stream(), partial);
        let stream = stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── protobuf codec ──────────────────────────────────────────────────

    #[test]
    fn varint_round_trip() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            pb::write_varint(&mut buf, value);
            let mut reader = pb::Reader::new(&buf);
            // A bare varint is not a tagged field, so read it via a synthetic tag.
            let mut tagged = Vec::new();
            pb::write_varint(&mut tagged, 1u64 << 3); // field 1, varint
            tagged.extend_from_slice(&buf);
            let mut r2 = pb::Reader::new(&tagged);
            match r2.next_field() {
                Some((1, pb::Field::Varint(got))) => assert_eq!(got, value),
                other => panic!("expected varint {value}, got {:?}", other.map(|(f, _)| f)),
            }
            let _ = reader.next_field();
        }
    }

    #[test]
    fn reader_skips_unknown_fields() {
        // Encode field 2 (string) then field 1 (string); reader should surface both.
        let mut buf = Vec::new();
        pb::write_string(&mut buf, 2, "second");
        pb::write_string(&mut buf, 1, "first");
        let mut reader = pb::Reader::new(&buf);
        let mut seen = Vec::new();
        while let Some((field, value)) = reader.next_field() {
            if let pb::Field::Len(raw) = value {
                seen.push((field, String::from_utf8_lossy(raw).into_owned()));
            }
        }
        assert_eq!(
            seen,
            vec![(2, "second".to_string()), (1, "first".to_string())]
        );
    }

    // ── Connect framing ─────────────────────────────────────────────────

    #[test]
    fn frame_encode_then_parse_across_chunks() {
        let mut a = encode_frame(0, b"hello");
        let b = encode_frame(CONNECT_END_STREAM_FLAG, b"{}");
        a.extend_from_slice(&b);

        let mut fb = FrameBuffer::default();
        // Feed one byte at a time to exercise reassembly.
        let mut frames = Vec::new();
        for byte in &a {
            fb.push(&[*byte]);
            while let Some(frame) = fb.next_frame() {
                frames.push(frame.expect("valid frame"));
            }
        }
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].end_stream);
        assert_eq!(frames[0].payload, b"hello");
        assert!(frames[1].end_stream);
        assert_eq!(frames[1].payload, b"{}");
    }

    #[test]
    fn frame_length_prefix_is_big_endian() {
        let frame = encode_frame(0, &[0xAB, 0xCD]);
        assert_eq!(&frame[0..5], &[0x00, 0x00, 0x00, 0x00, 0x02]);
    }

    #[test]
    fn frame_over_max_length_is_error_not_unbounded_buffering() {
        // A hostile/buggy length prefix declaring more than MAX_CONNECT_FRAME_LEN
        // must be reported as an error rather than making FrameBuffer grow without
        // bound waiting for bytes that will never arrive.
        let mut header = vec![0u8]; // flags
        let over = u32::try_from(MAX_CONNECT_FRAME_LEN).expect("cap fits in u32") + 1;
        header.extend_from_slice(&over.to_be_bytes());
        let mut fb = FrameBuffer::default();
        fb.push(&header);
        match fb.next_frame() {
            Some(Err(_)) => {}
            other => panic!("expected Some(Err(..)) for over-cap frame length, got {other:?}"),
        }
    }

    #[test]
    fn frame_at_max_length_boundary_is_accepted() {
        // A length exactly at the cap is allowed (only strictly-greater is rejected).
        let payload = vec![0x7u8; 3];
        let mut fb = FrameBuffer::default();
        fb.push(&encode_frame(0, &payload));
        let frame = fb
            .next_frame()
            .expect("frame buffered")
            .expect("within cap");
        assert_eq!(frame.payload, payload);
    }

    // ── request encoding ────────────────────────────────────────────────

    /// Walk into a nested length-delimited field by number.
    fn descend(bytes: &[u8], field: u32) -> Option<&[u8]> {
        let mut reader = pb::Reader::new(bytes);
        while let Some((f, value)) = reader.next_field() {
            if f == field {
                if let pb::Field::Len(inner) = value {
                    return Some(inner);
                }
            }
        }
        None
    }

    fn read_string(bytes: &[u8], field: u32) -> Option<String> {
        let mut reader = pb::Reader::new(bytes);
        while let Some((f, value)) = reader.next_field() {
            if f == field {
                if let pb::Field::Len(raw) = value {
                    return Some(String::from_utf8_lossy(raw).into_owned());
                }
            }
        }
        None
    }

    #[test]
    fn build_run_request_round_trips_all_fields() {
        let bytes = build_run_request(
            "claude-4.5-sonnet",
            Some("You are Pi."),
            "hello cursor",
            "conv-123",
            "msg-456",
        );
        // AgentClientMessage.run_request (1) -> AgentRunRequest
        let run_request = descend(&bytes, 1).expect("run_request");
        // model_details (3) -> model_id (1)
        let model_details = descend(run_request, 3).expect("model_details");
        assert_eq!(
            read_string(model_details, 1).as_deref(),
            Some("claude-4.5-sonnet")
        );
        // conversation_id (5) + custom_system_prompt (8)
        assert_eq!(read_string(run_request, 5).as_deref(), Some("conv-123"));
        assert_eq!(read_string(run_request, 8).as_deref(), Some("You are Pi."));
        // action (2) -> user_message_action (1) -> user_message (1) -> text (1)
        let action = descend(run_request, 2).expect("action");
        let uma = descend(action, 1).expect("user_message_action");
        let user_message = descend(uma, 1).expect("user_message");
        assert_eq!(
            read_string(user_message, 1).as_deref(),
            Some("hello cursor")
        );
        assert_eq!(read_string(user_message, 2).as_deref(), Some("msg-456"));
    }

    #[test]
    fn build_run_request_omits_empty_system_prompt() {
        let bytes = build_run_request("m", Some(""), "hi", "c", "msg");
        let run_request = descend(&bytes, 1).expect("run_request");
        assert!(read_string(run_request, 8).is_none());
        let bytes_none = build_run_request("m", None, "hi", "c", "msg");
        let rr = descend(&bytes_none, 1).expect("run_request");
        assert!(read_string(rr, 8).is_none());
    }

    #[test]
    fn latest_user_text_prefers_last_user_message() {
        use crate::model::{AssistantMessage, UserMessage};
        use std::sync::Arc;
        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("first".into()),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage::default())),
            Message::User(UserMessage {
                content: UserContent::Text("second".into()),
                timestamp: 0,
            }),
        ];
        assert_eq!(latest_user_text(&messages).as_deref(), Some("second"));
    }

    #[test]
    fn latest_user_text_joins_text_blocks() {
        use crate::model::UserMessage;
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new("line one")),
                ContentBlock::Text(TextContent::new("line two")),
            ]),
            timestamp: 0,
        })];
        assert_eq!(
            latest_user_text(&messages).as_deref(),
            Some("line one\nline two")
        );
    }

    // ── response decoding ───────────────────────────────────────────────

    /// Build an `InteractionUpdate` frame with a single `*DeltaUpdate` child.
    fn interaction_update(field: u32, text: &str) -> Vec<u8> {
        let mut delta = Vec::new();
        pb::write_string(&mut delta, 1, text); // TextDeltaUpdate.text = 1
        let mut update = Vec::new();
        pb::write_len_delim(&mut update, field, &delta);
        let mut server = Vec::new();
        pb::write_len_delim(&mut server, 1, &update); // interaction_update = 1
        server
    }

    fn token_delta_message(tokens: u64) -> Vec<u8> {
        let mut td = Vec::new();
        pb::write_varint(&mut td, 1u64 << 3); // TokenDeltaUpdate.tokens = 1, varint
        pb::write_varint(&mut td, tokens);
        let mut update = Vec::new();
        pb::write_len_delim(&mut update, 8, &td); // token_delta = 8
        let mut server = Vec::new();
        pb::write_len_delim(&mut server, 1, &update);
        server
    }

    fn checkpoint_message(used: u64) -> Vec<u8> {
        let mut details = Vec::new();
        pb::write_varint(&mut details, 1u64 << 3); // used_tokens = 1
        pb::write_varint(&mut details, used);
        let mut state = Vec::new();
        pb::write_len_delim(&mut state, 5, &details); // token_details = 5
        let mut server = Vec::new();
        pb::write_len_delim(&mut server, 3, &state); // conversation_checkpoint_update = 3
        server
    }

    #[test]
    fn decode_text_and_thinking_deltas() {
        assert_eq!(
            decode_server_message(&interaction_update(1, "hi")),
            vec![ServerEvent::TextDelta("hi".into())]
        );
        assert_eq!(
            decode_server_message(&interaction_update(4, "pondering")),
            vec![ServerEvent::ThinkingDelta("pondering".into())]
        );
    }

    #[test]
    fn decode_token_and_checkpoint() {
        assert_eq!(
            decode_server_message(&token_delta_message(7)),
            vec![ServerEvent::TokenDelta(7)]
        );
        assert_eq!(
            decode_server_message(&checkpoint_message(1234)),
            vec![ServerEvent::UsedTokens(1234)]
        );
    }

    // ── streaming state machine ─────────────────────────────────────────

    /// Drive `StreamState` synchronously over a set of Connect data frames plus
    /// a terminal end-stream frame, collecting the emitted pi events.
    fn drive(frames: &[Vec<u8>], end_stream_json: &[u8]) -> Vec<StreamEvent> {
        let empty: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>> =
            Box::pin(stream::empty());
        let mut state = StreamState::new(empty, AssistantMessage::default());
        for frame in frames {
            state
                .process_frame(&ConnectFrame {
                    end_stream: false,
                    compressed: false,
                    payload: frame.clone(),
                })
                .expect("data frame");
        }
        state
            .process_frame(&ConnectFrame {
                end_stream: true,
                compressed: false,
                payload: end_stream_json.to_vec(),
            })
            .expect("end frame");
        state.pending.drain(..).collect()
    }

    #[test]
    fn stream_state_emits_ordered_text_events() {
        let events = drive(
            &[
                interaction_update(1, "Hello"),
                interaction_update(1, ", world"),
                token_delta_message(5),
                checkpoint_message(42),
            ],
            b"{}",
        );

        let mut it = events.iter();
        assert!(matches!(it.next(), Some(StreamEvent::Start { .. })));
        assert!(matches!(
            it.next(),
            Some(StreamEvent::TextStart { content_index: 0 })
        ));
        assert!(matches!(
            it.next(),
            Some(StreamEvent::TextDelta { delta, .. }) if delta == "Hello"
        ));
        assert!(matches!(
            it.next(),
            Some(StreamEvent::TextDelta { delta, .. }) if delta == ", world"
        ));
        assert!(matches!(
            it.next(),
            Some(StreamEvent::TextEnd { content, .. }) if content == "Hello, world"
        ));
        match it.next() {
            Some(StreamEvent::Done { reason, message }) => {
                assert_eq!(*reason, StopReason::Stop);
                assert_eq!(message.usage.output, 5);
                assert_eq!(message.usage.total_tokens, 42);
                assert_eq!(message.usage.input, 37);
                assert_eq!(message.usage.cache_read, 0);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(it.next().is_none());
    }

    #[test]
    fn stream_state_switches_thinking_to_text_block() {
        let events = drive(
            &[
                interaction_update(4, "let me think"),
                interaction_update(1, "answer"),
            ],
            b"{}",
        );
        // Start, ThinkingStart(0), ThinkingDelta, ThinkingEnd(0),
        // TextStart(1), TextDelta, TextEnd(1), Done
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::ThinkingStart { content_index: 0 }
        ));
        assert!(matches!(events[2], StreamEvent::ThinkingDelta { .. }));
        assert!(matches!(
            events[3],
            StreamEvent::ThinkingEnd {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            events[4],
            StreamEvent::TextStart { content_index: 1 }
        ));
        assert!(matches!(events[5], StreamEvent::TextDelta { .. }));
        assert!(matches!(
            events[6],
            StreamEvent::TextEnd {
                content_index: 1,
                ..
            }
        ));
        assert!(matches!(events[7], StreamEvent::Done { .. }));
    }

    #[test]
    fn stream_state_surfaces_end_stream_error() {
        let events = drive(
            &[interaction_update(1, "partial")],
            br#"{"error":{"message":"boom"}}"#,
        );
        match events.last() {
            Some(StreamEvent::Error { reason, error }) => {
                assert_eq!(*reason, StopReason::Error);
                assert_eq!(error.error_message.as_deref(), Some("boom"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ── token resolution ────────────────────────────────────────────────

    #[test]
    fn pick_token_precedence() {
        assert_eq!(
            pick_token(Some("inline"), Some("env")).as_deref(),
            Some("inline")
        );
        assert_eq!(pick_token(Some("  "), Some("env")).as_deref(), Some("env"));
        assert_eq!(pick_token(None, Some("env")).as_deref(), Some("env"));
        assert_eq!(pick_token(None, None), None);
        assert_eq!(pick_token(Some(""), Some("")), None);
    }

    #[test]
    fn provider_reports_identity() {
        let provider = CursorProvider::new("claude-4.5-sonnet");
        assert_eq!(provider.name(), "cursor");
        assert_eq!(provider.api(), CURSOR_API_NAME);
        assert_eq!(provider.model_id(), "claude-4.5-sonnet");
    }
}
