//! OpenAI Responses API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI `responses` endpoint,
//! supporting streaming output text and function tool calls.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ToolCall, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::{SseEvent, SseStream};
use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

// ============================================================================
// Constants
// ============================================================================

const OPENAI_RESPONSES_API_URL: &str = "https://api.openai.com/v1/responses";
pub(crate) const CODEX_RESPONSES_API_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

// ============================================================================
// OpenAI Responses Provider
// ============================================================================

/// OpenAI Responses API provider.
pub struct OpenAIResponsesProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
    api: String,
    codex_mode: bool,
    compat: Option<CompatConfig>,
}

impl OpenAIResponsesProvider {
    /// Create a new OpenAI Responses provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: OPENAI_RESPONSES_API_URL.to_string(),
            provider: "openai".to_string(),
            api: "openai-responses".to_string(),
            codex_mode: false,
            compat: None,
        }
    }

    /// Override the provider name reported in streamed events.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Override API identifier reported in streamed events.
    #[must_use]
    pub fn with_api_name(mut self, api: impl Into<String>) -> Self {
        self.api = api.into();
        self
    }

    /// Enable OpenAI Codex Responses mode (ChatGPT OAuth endpoint + headers).
    #[must_use]
    pub const fn with_codex_mode(mut self, enabled: bool) -> Self {
        self.codex_mode = enabled;
        self
    }

    /// Create with a custom base URL.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Create with a custom HTTP client (VCR, test harness, etc.).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Attach provider-specific compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    pub fn build_request(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> OpenAIResponsesRequest {
        let input = build_openai_responses_input(context);
        let tools: Option<Vec<OpenAIResponsesTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(convert_tool_to_openai_responses)
                    .collect(),
            )
        };

        let instructions = context.system_prompt.as_deref().map(ToString::to_string);

        // Codex mode requires additional fields per the TS reference implementation.
        // tool_choice and parallel_tool_calls are always sent (not conditional on tools).
        let (tool_choice, parallel_tool_calls, text, include, reasoning) = if self.codex_mode {
            let effort = options
                .thinking_level
                .as_ref()
                .map_or_else(|| "high".to_string(), ToString::to_string);
            (
                Some("auto"),
                Some(true),
                Some(OpenAIResponsesTextConfig {
                    verbosity: "medium",
                }),
                Some(vec!["reasoning.encrypted_content"]),
                Some(OpenAIResponsesReasoning {
                    effort,
                    summary: Some("auto"),
                }),
            )
        } else {
            (None, None, None, None, None)
        };

        OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            temperature: options.temperature,
            max_output_tokens: if self.codex_mode {
                None
            } else {
                options.max_tokens.or(Some(DEFAULT_MAX_OUTPUT_TOKENS))
            },
            tools,
            stream: true,
            store: false,
            tool_choice,
            parallel_tool_calls,
            text,
            include,
            reasoning,
        }
    }
}

fn bearer_token_from_authorization_header(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let bearer_value = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if scheme.eq_ignore_ascii_case("bearer") && !bearer_value.trim().is_empty() {
        Some(bearer_value.trim().to_string())
    } else {
        None
    }
}

fn authorization_override(
    options: &StreamOptions,
    compat: Option<&CompatConfig>,
) -> Option<String> {
    super::first_non_empty_header_value_case_insensitive(&options.headers, &["authorization"])
        .or_else(|| {
            compat
                .and_then(|compat| compat.custom_headers.as_ref())
                .and_then(|headers| {
                    super::first_non_empty_header_value_case_insensitive(
                        headers,
                        &["authorization"],
                    )
                })
        })
}

#[async_trait]
impl Provider for OpenAIResponsesProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &str {
        &self.api
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    #[allow(clippy::too_many_lines)]
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let authorization_header_value = authorization_override(options, self.compat.as_ref());

        let auth_value = if authorization_header_value.is_some() {
            None
        } else {
            Some(
                options
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .ok_or_else(|| {
                        Error::provider(
                            self.name(),
                            "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                        )
                    })?,
            )
        };

        let request_body = self.build_request(context, options);

        // Note: Content-Type is set by .json() below; setting it here too
        // produces a duplicate header that OpenAI's server rejects.
        let mut request = self.client.post(&self.base_url).header(
            "Accept",
            "text/event-stream, application/x-ndjson, application/ndjson",
        );

        if let Some(ref auth_value) = auth_value {
            request = request.header("Authorization", format!("Bearer {auth_value}"));
        }

        if self.codex_mode {
            let codex_bearer = authorization_header_value
                .as_deref()
                .and_then(bearer_token_from_authorization_header)
                .or_else(|| auth_value.clone())
                .ok_or_else(|| {
                    Error::provider(
                        self.name(),
                        "OpenAI Codex mode requires a Bearer token. Provide one via /login openai-codex or an Authorization: Bearer <token> header.",
                    )
                })?;
            let account_id = extract_chatgpt_account_id(&codex_bearer).ok_or_else(|| {
                Error::provider(
                    self.name(),
                    "Invalid OpenAI Codex OAuth token (missing chatgpt_account_id claim). Run /login openai-codex again.",
                )
            })?;
            request = request
                .header("chatgpt-account-id", account_id)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "pi")
                .header("User-Agent", "pi_agent_rust");
            if let Some(session_id) = &options.session_id {
                request = request.header("session_id", session_id);
            }
        }

        // Apply provider-specific custom headers from compat config.
        if let Some(compat) = &self.compat {
            if let Some(custom_headers) = &compat.custom_headers {
                request = super::apply_headers_ignoring_blank_auth_overrides(
                    request,
                    custom_headers,
                    &["authorization"],
                );
            }
        }

        // Per-request headers from StreamOptions (highest priority).
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(Error::provider(
                self.name(),
                format!("OpenAI API error (HTTP {status}): {body}"),
            ));
        }

        // Validate Content-Type when present. For standard OpenAI Responses
        // endpoints we fail closed if the header is missing; for Codex we allow
        // it because some endpoints omit the header while still streaming SSE.
        // If the header IS present and indicates a non-SSE type, reject early
        // so we fail closed instead of silently dropping events in the SSE parser.
        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_ascii_lowercase());
        if content_type.is_none() && !self.codex_mode {
            return Err(Error::api(format!(
                "OpenAI API protocol error (HTTP {status}): missing content-type header (expected text/event-stream or application/x-ndjson)"
            )));
        }
        let mut use_ndjson = false;
        if let Some(ref ct) = content_type {
            if ct.contains("application/x-ndjson") || ct.contains("application/ndjson") {
                use_ndjson = true;
            } else if !ct.contains("text/event-stream") {
                return Err(Error::api(format!(
                    "OpenAI API protocol error (HTTP {status}): unexpected Content-Type {ct} (expected text/event-stream or application/x-ndjson)"
                )));
            }
        }

        let byte_stream = response.bytes_stream();
        let event_source = if use_ndjson {
            ResponsesEventStream::Ndjson(NdjsonStream::new(byte_stream))
        } else {
            ResponsesEventStream::Sse(SseStream::new(byte_stream))
        };

        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    // We may have queued terminal events (ToolCallEnd, Done, etc) after
                    // consuming the final wire chunk. Only stop once the queue is empty.
                    if state.finished {
                        return None;
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            // A successful chunk resets the consecutive error counter.
                            state.transient_error_count = 0;
                            if msg.data == "[DONE]" {
                                // Response terminal metadata can arrive before trailing item
                                // events, so only emit Done once the wire stream itself ends.
                                if !state.finish_terminal_response() {
                                    // Best-effort fallback: if we didn't see a completed or
                                    // incomplete chunk, emit Done using current state.
                                    state.finish(None);
                                }
                                continue;
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                return Some((Err(e), state));
                            }
                        }
                        Some(Err(e)) => {
                            // WriteZero, WouldBlock, and TimedOut errors are treated as transient.
                            // Skip them and keep reading the stream, but cap
                            // consecutive occurrences to avoid infinite loops.
                            const MAX_CONSECUTIVE_TRANSIENT_ERRORS: usize = 5;
                            if e.kind() == std::io::ErrorKind::WriteZero
                                || e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut
                            {
                                state.transient_error_count += 1;
                                if state.transient_error_count <= MAX_CONSECUTIVE_TRANSIENT_ERRORS {
                                    tracing::warn!(
                                        kind = ?e.kind(),
                                        count = state.transient_error_count,
                                        "Transient error in SSE stream, continuing"
                                    );
                                    continue;
                                }
                                tracing::warn!(
                                    kind = ?e.kind(),
                                    "Error persisted after {MAX_CONSECUTIVE_TRANSIENT_ERRORS} \
                                     consecutive attempts, treating as fatal"
                                );
                            }
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        None => {
                            if state.finish_terminal_response() {
                                continue;
                            }

                            // If the stream ends unexpectedly, surface an error. This matches the
                            // agent loop expectation that providers emit Done/Error explicitly.
                            return Some((
                                Err(Error::api("Stream ended without Done event")),
                                state,
                            ));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

// ============================================================================
// NDJSON Stream
// ============================================================================

const NDJSON_MAX_EVENT_BYTES: usize = 100 * 1024 * 1024;

struct NdjsonStream<S> {
    inner: S,
    buffer: Vec<u8>,
    pending: VecDeque<String>,
    finished: bool,
    max_event_bytes: usize,
}

impl<S> NdjsonStream<S> {
    const fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            finished: false,
            max_event_bytes: NDJSON_MAX_EVENT_BYTES,
        }
    }

    #[cfg(test)]
    fn with_max_event_bytes(inner: S, max_event_bytes: usize) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            finished: false,
            max_event_bytes,
        }
    }

    fn invalid_utf8_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid UTF-8 in NDJSON stream",
        )
    }

    fn event_too_large_error(&self) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("NDJSON event exceeds {} bytes", self.max_event_bytes),
        )
    }

    fn push_line(&mut self, line_bytes: &[u8]) -> std::io::Result<()> {
        if line_bytes.len() > self.max_event_bytes {
            return Err(self.event_too_large_error());
        }
        let line = std::str::from_utf8(line_bytes).map_err(|_| Self::invalid_utf8_error())?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            return Ok(());
        }
        self.pending.push_back(line.to_string());
        Ok(())
    }

    fn drain_buffer(&mut self) -> std::io::Result<()> {
        if self.buffer.len() > self.max_event_bytes && memchr::memchr(b'\n', &self.buffer).is_none()
        {
            return Err(self.event_too_large_error());
        }
        while let Some(pos) = memchr::memchr(b'\n', &self.buffer) {
            if pos > self.max_event_bytes {
                return Err(self.event_too_large_error());
            }
            let mut line = self.buffer.drain(..=pos).collect::<Vec<u8>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if !line.is_empty() {
                self.push_line(&line)?;
            }
        }
        Ok(())
    }

    fn finish_buffer(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.buffer);
        self.push_line(&line)?;
        Ok(())
    }

    fn event_from_line(line: String) -> SseEvent {
        SseEvent {
            data: line,
            ..SseEvent::default()
        }
    }
}

impl<S> Stream for NdjsonStream<S>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    type Item = std::io::Result<SseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(line) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(Self::event_from_line(line))));
        }

        if self.finished {
            return Poll::Ready(None);
        }

        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    self.buffer.extend_from_slice(&chunk);
                    if let Err(err) = self.drain_buffer() {
                        return Poll::Ready(Some(Err(err)));
                    }
                    if let Some(line) = self.pending.pop_front() {
                        return Poll::Ready(Some(Ok(Self::event_from_line(line))));
                    }
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(None) => {
                    self.finished = true;
                    if let Err(err) = self.finish_buffer() {
                        return Poll::Ready(Some(Err(err)));
                    }
                    if let Some(line) = self.pending.pop_front() {
                        return Poll::Ready(Some(Ok(Self::event_from_line(line))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

enum ResponsesEventStream<S>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    Ndjson(NdjsonStream<S>),
    Sse(SseStream<S>),
}

impl<S> Stream for ResponsesEventStream<S>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    type Item = std::io::Result<SseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().get_mut() {
            Self::Ndjson(inner) => Pin::new(inner).poll_next(cx),
            Self::Sse(inner) => Pin::new(inner).poll_next(cx),
        }
    }
}

// ============================================================================
// Stream State
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextKey {
    item_id: String,
    content_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReasoningKey {
    item_id: String,
    kind: ReasoningKind,
    index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReasoningKind {
    Summary,
    Text,
}

struct ToolCallState {
    content_index: usize,
    call_id: String,
    name: String,
    arguments: String,
}

enum TerminalContentSnapshot {
    Text {
        content_index: usize,
        content: String,
    },
    Thinking {
        content_index: usize,
        content: String,
    },
    ToolCall {
        content_index: usize,
    },
}

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<SseEvent, std::io::Error>> + Unpin,
{
    event_source: S,
    partial: AssistantMessage,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    finished: bool,
    text_blocks: HashMap<TextKey, usize>,
    reasoning_blocks: HashMap<ReasoningKey, usize>,
    tool_calls_by_item_id: HashMap<String, ToolCallState>,
    orphan_tool_call_arguments: HashMap<String, String>,
    terminal_response_seen: bool,
    terminal_incomplete_reason: Option<String>,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::result::Result<SseEvent, std::io::Error>> + Unpin,
{
    fn new(event_source: S, model: String, api: String, provider: String) -> Self {
        Self {
            event_source,
            partial: AssistantMessage {
                content: Vec::new(),
                api,
                provider,
                model,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            pending_events: VecDeque::new(),
            started: false,
            finished: false,
            text_blocks: HashMap::new(),
            reasoning_blocks: HashMap::new(),
            tool_calls_by_item_id: HashMap::new(),
            orphan_tool_call_arguments: HashMap::new(),
            terminal_response_seen: false,
            terminal_incomplete_reason: None,
            transient_error_count: 0,
        }
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            self.pending_events.push_back(StreamEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    fn record_terminal_response(&mut self, incomplete_reason: Option<String>) {
        self.terminal_response_seen = true;
        self.terminal_incomplete_reason = incomplete_reason;
    }

    fn finish_terminal_response(&mut self) -> bool {
        if !self.terminal_response_seen {
            return false;
        }

        let incomplete_reason = self.terminal_incomplete_reason.take();
        self.terminal_response_seen = false;
        self.finish(incomplete_reason);
        true
    }

    fn text_block_for(&mut self, item_id: String, content_index: u32) -> usize {
        let key = TextKey {
            item_id,
            content_index,
        };
        if let Some(idx) = self.text_blocks.get(&key) {
            return *idx;
        }

        let idx = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Text(TextContent::new("")));
        self.text_blocks.insert(key, idx);
        self.pending_events
            .push_back(StreamEvent::TextStart { content_index: idx });
        idx
    }

    fn reasoning_block_for(&mut self, kind: ReasoningKind, item_id: String, index: u32) -> usize {
        let key = ReasoningKey {
            item_id,
            kind,
            index,
        };
        if let Some(idx) = self.reasoning_blocks.get(&key) {
            return *idx;
        }

        let idx = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
            }));
        self.reasoning_blocks.insert(key, idx);
        self.pending_events
            .push_back(StreamEvent::ThinkingStart { content_index: idx });
        idx
    }

    fn append_reasoning_delta(
        &mut self,
        kind: ReasoningKind,
        item_id: String,
        index: u32,
        delta: String,
    ) {
        self.ensure_started();
        let idx = self.reasoning_block_for(kind, item_id, index);
        if let Some(ContentBlock::Thinking(block)) = self.partial.content.get_mut(idx) {
            block.thinking.push_str(&delta);
        }
        self.pending_events.push_back(StreamEvent::ThinkingDelta {
            content_index: idx,
            delta,
        });
    }

    fn apply_text_snapshot(&mut self, item_id: String, content_index: u32, text: String) {
        self.ensure_started();
        let idx = self.text_block_for(item_id, content_index);
        let Some(ContentBlock::Text(block)) = self.partial.content.get_mut(idx) else {
            return;
        };

        if block.text == text {
            return;
        }

        if let Some(suffix) = text.strip_prefix(&block.text) {
            if suffix.is_empty() {
                return;
            }
            block.text.push_str(suffix);
            self.pending_events.push_back(StreamEvent::TextDelta {
                content_index: idx,
                delta: suffix.to_string(),
            });
            return;
        }

        tracing::warn!(
            content_index = idx,
            old_len = block.text.len(),
            new_len = text.len(),
            "text snapshot does not extend accumulated text; replacing"
        );
        block.text = text;
    }

    fn apply_reasoning_snapshot(
        &mut self,
        kind: ReasoningKind,
        item_id: String,
        index: u32,
        text: String,
    ) {
        self.ensure_started();
        let idx = self.reasoning_block_for(kind, item_id, index);
        let Some(ContentBlock::Thinking(block)) = self.partial.content.get_mut(idx) else {
            return;
        };

        if block.thinking == text {
            return;
        }

        if let Some(suffix) = text.strip_prefix(&block.thinking) {
            if suffix.is_empty() {
                return;
            }
            block.thinking.push_str(suffix);
            self.pending_events.push_back(StreamEvent::ThinkingDelta {
                content_index: idx,
                delta: suffix.to_string(),
            });
            return;
        }

        tracing::warn!(
            content_index = idx,
            old_len = block.thinking.len(),
            new_len = text.len(),
            "reasoning snapshot does not extend accumulated thinking; replacing"
        );
        block.thinking = text;
    }

    #[allow(clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: OpenAIResponsesChunk = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        match chunk {
            OpenAIResponsesChunk::OutputTextDelta {
                item_id,
                content_index,
                delta,
            } => {
                self.ensure_started();
                let idx = self.text_block_for(item_id, content_index);
                if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(idx) {
                    t.text.push_str(&delta);
                }
                self.pending_events.push_back(StreamEvent::TextDelta {
                    content_index: idx,
                    delta,
                });
            }
            OpenAIResponsesChunk::ReasoningSummaryTextDelta {
                item_id,
                summary_index,
                delta,
            } => {
                self.append_reasoning_delta(ReasoningKind::Summary, item_id, summary_index, delta);
            }
            OpenAIResponsesChunk::ReasoningTextDelta {
                item_id,
                content_index,
                delta,
            } => {
                self.append_reasoning_delta(ReasoningKind::Text, item_id, content_index, delta);
            }
            OpenAIResponsesChunk::OutputTextDone {
                item_id,
                content_index,
                text,
            } => {
                self.apply_text_snapshot(item_id, content_index, text);
            }
            OpenAIResponsesChunk::ContentPartDone {
                item_id,
                content_index,
                part,
            } => match part {
                OpenAIResponsesContentPartDone::OutputText { text } => {
                    self.apply_text_snapshot(item_id, content_index, text);
                }
                OpenAIResponsesContentPartDone::ReasoningText { text } => {
                    self.apply_reasoning_snapshot(
                        ReasoningKind::Text,
                        item_id,
                        content_index,
                        text,
                    );
                }
                OpenAIResponsesContentPartDone::Unknown => {}
            },
            OpenAIResponsesChunk::ReasoningTextDone {
                item_id,
                content_index,
                text,
            } => {
                self.apply_reasoning_snapshot(ReasoningKind::Text, item_id, content_index, text);
            }
            OpenAIResponsesChunk::ReasoningSummaryTextDone {
                item_id,
                summary_index,
                text,
            } => {
                self.apply_reasoning_snapshot(ReasoningKind::Summary, item_id, summary_index, text);
            }
            OpenAIResponsesChunk::ReasoningSummaryPartDone {
                item_id,
                summary_index,
                part,
            } => match part {
                OpenAIResponsesReasoningSummaryPartDone::SummaryText { text } => {
                    self.apply_reasoning_snapshot(
                        ReasoningKind::Summary,
                        item_id,
                        summary_index,
                        text,
                    );
                }
                OpenAIResponsesReasoningSummaryPartDone::Unknown => {}
            },
            OpenAIResponsesChunk::OutputItemAdded { item } => {
                if let OpenAIResponsesOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    self.ensure_started();

                    let mut buffered_arguments = self
                        .orphan_tool_call_arguments
                        .remove(&id)
                        .unwrap_or_default();
                    buffered_arguments.insert_str(0, &arguments);

                    let content_index = self.partial.content.len();
                    self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: serde_json::Value::Null,
                        thought_signature: None,
                    }));

                    self.tool_calls_by_item_id.insert(
                        id,
                        ToolCallState {
                            content_index,
                            call_id,
                            name,
                            arguments: buffered_arguments.clone(),
                        },
                    );

                    self.pending_events
                        .push_back(StreamEvent::ToolCallStart { content_index });

                    if !buffered_arguments.is_empty() {
                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,
                            delta: buffered_arguments,
                        });
                    }
                }
            }
            OpenAIResponsesChunk::FunctionCallArgumentsDelta { item_id, delta } => {
                self.ensure_started();
                if let Some(tc) = self.tool_calls_by_item_id.get_mut(&item_id) {
                    tc.arguments.push_str(&delta);
                    self.pending_events.push_back(StreamEvent::ToolCallDelta {
                        content_index: tc.content_index,
                        delta,
                    });
                } else {
                    self.orphan_tool_call_arguments
                        .entry(item_id)
                        .or_default()
                        .push_str(&delta);
                }
            }
            OpenAIResponsesChunk::OutputItemDone { item } => {
                if let OpenAIResponsesOutputItemDone::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    self.ensure_started();
                    self.end_tool_call(&id, &call_id, &name, &arguments);
                }
            }
            OpenAIResponsesChunk::ResponseCompleted { response }
            | OpenAIResponsesChunk::ResponseDone { response }
            | OpenAIResponsesChunk::ResponseIncomplete { response } => {
                self.ensure_started();
                self.partial.usage.input = response.usage.input_tokens;
                self.partial.usage.output = response.usage.output_tokens;
                self.partial.usage.total_tokens = response
                    .usage
                    .total_tokens
                    .unwrap_or(response.usage.input_tokens + response.usage.output_tokens);
                self.record_terminal_response(response.incomplete_reason());
            }
            OpenAIResponsesChunk::ResponseFailed { response } => {
                self.ensure_started();
                self.partial.stop_reason = StopReason::Error;
                self.partial.error_message = Some(
                    response
                        .error
                        .and_then(|error| error.message)
                        .unwrap_or_else(|| "Codex response failed".to_string()),
                );
                self.pending_events.push_back(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: std::mem::take(&mut self.partial),
                });
                self.finished = true;
            }
            OpenAIResponsesChunk::Error { message } => {
                self.ensure_started();
                self.partial.stop_reason = StopReason::Error;
                self.partial.error_message = Some(message);
                self.pending_events.push_back(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: std::mem::take(&mut self.partial),
                });
                self.finished = true;
            }
            OpenAIResponsesChunk::Unknown => {}
        }

        Ok(())
    }

    fn partial_has_tool_call(&self) -> bool {
        self.partial
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall(_)))
    }

    fn end_tool_call(&mut self, item_id: &str, call_id: &str, name: &str, arguments: &str) {
        let (mut tc, synthesized_start) = self.tool_calls_by_item_id.remove(item_id).map_or_else(
            || {
                // If we missed the added event, synthesize the full tool-call block now
                // so downstream consumers still see a valid Start → Delta? → End sequence.
                let content_index = self.partial.content.len();
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                }));
                (
                    ToolCallState {
                        content_index,
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        arguments: self
                            .orphan_tool_call_arguments
                            .remove(item_id)
                            .unwrap_or_default(),
                    },
                    true,
                )
            },
            |state| (state, false),
        );

        if synthesized_start {
            self.pending_events.push_back(StreamEvent::ToolCallStart {
                content_index: tc.content_index,
            });
        }

        // Prefer the final arguments field when present.
        if !arguments.is_empty() {
            tc.arguments = arguments.to_string();
        }

        if synthesized_start && !tc.arguments.is_empty() {
            self.pending_events.push_back(StreamEvent::ToolCallDelta {
                content_index: tc.content_index,
                delta: tc.arguments.clone(),
            });
        }

        let parsed_args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    raw = %tc.arguments,
                    "Failed to parse tool arguments as JSON"
                );
                serde_json::Value::Null
            });

        self.partial.stop_reason = StopReason::ToolUse;
        self.pending_events.push_back(StreamEvent::ToolCallEnd {
            content_index: tc.content_index,
            tool_call: ToolCall {
                id: tc.call_id.clone(),
                name: tc.name.clone(),
                arguments: parsed_args.clone(),
                thought_signature: None,
            },
        });

        if let Some(ContentBlock::ToolCall(block)) = self.partial.content.get_mut(tc.content_index)
        {
            block.id = tc.call_id;
            block.name = tc.name;
            block.arguments = parsed_args;
        }
    }

    fn terminal_content_snapshots(&self) -> Vec<TerminalContentSnapshot> {
        self.partial
            .content
            .iter()
            .enumerate()
            .filter_map(|(content_index, block)| match block {
                ContentBlock::Text(t) => Some(TerminalContentSnapshot::Text {
                    content_index,
                    content: t.text.clone(),
                }),
                ContentBlock::Thinking(t) => Some(TerminalContentSnapshot::Thinking {
                    content_index,
                    content: t.thinking.clone(),
                }),
                ContentBlock::ToolCall(_) => {
                    Some(TerminalContentSnapshot::ToolCall { content_index })
                }
                ContentBlock::Image(_) | ContentBlock::RedactedThinking(_) => None,
            })
            .collect()
    }

    fn open_tool_call_snapshots_in_content_order(
        &self,
    ) -> Vec<(usize, String, String, String, String)> {
        let mut open_tool_calls: Vec<(usize, String, String, String, String)> = self
            .tool_calls_by_item_id
            .iter()
            .map(|(item_id, tc)| {
                (
                    tc.content_index,
                    item_id.clone(),
                    tc.call_id.clone(),
                    tc.name.clone(),
                    tc.arguments.clone(),
                )
            })
            .collect();
        open_tool_calls.sort_by_key(|(content_index, ..)| *content_index);
        open_tool_calls
    }

    fn finish(&mut self, incomplete_reason: Option<String>) {
        if self.finished {
            return;
        }

        let mut open_tool_calls = self
            .open_tool_call_snapshots_in_content_order()
            .into_iter()
            .peekable();

        for block in self.terminal_content_snapshots() {
            match block {
                TerminalContentSnapshot::Text {
                    content_index,
                    content,
                } => self.pending_events.push_back(StreamEvent::TextEnd {
                    content_index,
                    content,
                }),
                TerminalContentSnapshot::Thinking {
                    content_index,
                    content,
                } => self.pending_events.push_back(StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                }),
                TerminalContentSnapshot::ToolCall { content_index } => {
                    while let Some((_, item_id, call_id, name, arguments)) = open_tool_calls
                        .next_if(|(tool_content_index, ..)| *tool_content_index == content_index)
                    {
                        self.end_tool_call(&item_id, &call_id, &name, &arguments);
                    }
                }
            }
        }

        // Best-effort: close any tool calls whose content block disappeared unexpectedly.
        for (_, item_id, call_id, name, arguments) in open_tool_calls {
            self.end_tool_call(&item_id, &call_id, &name, &arguments);
        }

        // Infer stop reason.
        if let Some(reason) = incomplete_reason {
            let reason_lower = reason.to_ascii_lowercase();
            if reason_lower.contains("max_output") || reason_lower.contains("length") {
                self.partial.stop_reason = StopReason::Length;
            } else if reason_lower.contains("tool") {
                self.partial.stop_reason = StopReason::ToolUse;
            } else if reason_lower.contains("content_filter") || reason_lower.contains("error") {
                self.partial.stop_reason = StopReason::Error;
            }
        } else if self.partial_has_tool_call() {
            self.partial.stop_reason = StopReason::ToolUse;
        }

        let reason = self.partial.stop_reason;
        self.pending_events.push_back(StreamEvent::Done {
            reason,
            message: self.partial.clone(),
        });
        self.finished = true;
    }
}

fn extract_chatgpt_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let payload_json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    payload_json
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

// ============================================================================
// OpenAI Responses API Types (minimal)
// ============================================================================

#[derive(Debug, Serialize)]
pub struct OpenAIResponsesRequest {
    model: String,
    input: Vec<OpenAIResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAIResponsesTool>>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<OpenAIResponsesTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAIResponsesReasoning>,
}

#[derive(Debug, Serialize)]
struct OpenAIResponsesTextConfig {
    verbosity: &'static str,
}

#[derive(Debug, Serialize)]
struct OpenAIResponsesReasoning {
    effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIResponsesInputItem {
    System {
        role: &'static str,
        content: String,
    },
    User {
        role: &'static str,
        content: Vec<OpenAIResponsesUserContentPart>,
    },
    Assistant {
        role: &'static str,
        content: Vec<OpenAIResponsesAssistantContentPart>,
    },
    FunctionCall {
        #[serde(rename = "type")]
        r#type: &'static str,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        r#type: &'static str,
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIResponsesUserContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIResponsesAssistantContentPart {
    #[serde(rename = "output_text")]
    OutputText { text: String },
}

#[derive(Debug, Serialize)]
struct OpenAIResponsesTool {
    #[serde(rename = "type")]
    r#type: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

fn convert_tool_to_openai_responses(tool: &ToolDef) -> OpenAIResponsesTool {
    OpenAIResponsesTool {
        r#type: "function",
        name: tool.name.clone(),
        description: if tool.description.trim().is_empty() {
            None
        } else {
            Some(tool.description.clone())
        },
        parameters: tool.parameters.clone(),
    }
}

fn build_openai_responses_input(context: &Context<'_>) -> Vec<OpenAIResponsesInputItem> {
    let mut input = Vec::with_capacity(context.messages.len());

    // System prompt is sent as top-level `instructions` field, not in input array.
    // Some providers (e.g. OpenAI Codex) require `instructions` and reject system
    // messages in the input array.

    for message in context.messages.iter() {
        match message {
            Message::User(user) => input.push(convert_user_message_to_responses(&user.content)),
            Message::Custom(custom) => input.push(OpenAIResponsesInputItem::User {
                role: "user",
                content: vec![OpenAIResponsesUserContentPart::InputText {
                    text: custom.content.clone(),
                }],
            }),
            Message::Assistant(assistant) => {
                // Preserve ordering between text and tool calls.
                let mut pending_text = String::new();

                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(t) => pending_text.push_str(&t.text),
                        ContentBlock::ToolCall(tc) => {
                            if !pending_text.is_empty() {
                                input.push(OpenAIResponsesInputItem::Assistant {
                                    role: "assistant",
                                    content: vec![
                                        OpenAIResponsesAssistantContentPart::OutputText {
                                            text: std::mem::take(&mut pending_text),
                                        },
                                    ],
                                });
                            }
                            input.push(OpenAIResponsesInputItem::FunctionCall {
                                r#type: "function_call",
                                call_id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            });
                        }
                        _ => {}
                    }
                }

                if !pending_text.is_empty() {
                    input.push(OpenAIResponsesInputItem::Assistant {
                        role: "assistant",
                        content: vec![OpenAIResponsesAssistantContentPart::OutputText {
                            text: pending_text,
                        }],
                    });
                }
            }
            Message::ToolResult(result) => {
                let mut out = String::new();
                for (i, block) in result.content.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    if let ContentBlock::Text(t) = block {
                        out.push_str(&t.text);
                    }
                }
                input.push(OpenAIResponsesInputItem::FunctionCallOutput {
                    r#type: "function_call_output",
                    call_id: result.tool_call_id.clone(),
                    output: out,
                });
            }
        }
    }

    input
}

fn convert_user_message_to_responses(content: &UserContent) -> OpenAIResponsesInputItem {
    match content {
        UserContent::Text(text) => OpenAIResponsesInputItem::User {
            role: "user",
            content: vec![OpenAIResponsesUserContentPart::InputText { text: text.clone() }],
        },
        UserContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text(t) => {
                        parts.push(OpenAIResponsesUserContentPart::InputText {
                            text: t.text.clone(),
                        });
                    }
                    ContentBlock::Image(img) => {
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        parts.push(OpenAIResponsesUserContentPart::InputImage { image_url: url });
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                parts.push(OpenAIResponsesUserContentPart::InputText {
                    text: String::new(),
                });
            }
            OpenAIResponsesInputItem::User {
                role: "user",
                content: parts,
            }
        }
    }
}

// ============================================================================
// Streaming Chunk Types (minimal, forward-compatible)
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesChunk {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        item_id: String,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        item_id: String,
        content_index: u32,
        text: String,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OpenAIResponsesOutputItem },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OpenAIResponsesOutputItemDone },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { item_id: String, delta: String },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        item_id: String,
        content_index: u32,
        part: OpenAIResponsesContentPartDone,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        item_id: String,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        item_id: String,
        content_index: u32,
        text: String,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        item_id: String,
        summary_index: u32,
        delta: String,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        item_id: String,
        summary_index: u32,
        text: String,
    },
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        item_id: String,
        summary_index: u32,
        part: OpenAIResponsesReasoningSummaryPartDone,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: OpenAIResponsesDonePayload,
    },
    #[serde(rename = "response.done")]
    ResponseDone {
        response: OpenAIResponsesDonePayload,
    },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete {
        response: OpenAIResponsesDonePayload,
    },
    #[serde(rename = "response.failed")]
    ResponseFailed {
        response: OpenAIResponsesFailedPayload,
    },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesContentPartDone {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "reasoning_text")]
    ReasoningText { text: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesReasoningSummaryPartDone {
    #[serde(rename = "summary_text")]
    SummaryText { text: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesOutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesOutputItemDone {
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesDonePayload {
    #[serde(default)]
    incomplete_details: Option<OpenAIResponsesIncompleteDetails>,
    usage: OpenAIResponsesUsage,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesFailedPayload {
    #[serde(default)]
    error: Option<OpenAIResponsesFailedError>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesFailedError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesIncompleteDetails {
    reason: String,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct OpenAIResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    total_tokens: Option<u64>,
}

impl OpenAIResponsesDonePayload {
    fn incomplete_reason(&self) -> Option<String> {
        self.incomplete_details.as_ref().map(|d| d.reason.clone())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;
    use proptest::prelude::*;
    use proptest::string::string_regex;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn test_provider_info() {
        let provider = OpenAIResponsesProvider::new("gpt-4o");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.api(), "openai-responses");
    }

    #[test]
    fn test_build_request_includes_system_tools_and_defaults() {
        let provider = OpenAIResponsesProvider::new("gpt-4o");
        let context = Context::owned(
            Some("System guidance".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            vec![
                ToolDef {
                    name: "search".to_string(),
                    description: "Search docs".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": { "q": { "type": "string" } },
                        "required": ["q"]
                    }),
                },
                ToolDef {
                    name: "blank_desc".to_string(),
                    description: "   ".to_string(),
                    parameters: json!({ "type": "object" }),
                },
            ],
        );
        let options = StreamOptions {
            temperature: Some(0.3),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["model"], "gpt-4o");
        let temperature = value["temperature"]
            .as_f64()
            .expect("temperature should serialize as number");
        assert!((temperature - 0.3).abs() < 1e-6);
        assert_eq!(value["max_output_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(value["stream"], true);
        assert_eq!(value["instructions"], "System guidance");
        assert_eq!(value["input"][0]["role"], "user");
        assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(value["input"][0]["content"][0]["text"], "Ping");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["name"], "search");
        assert_eq!(value["tools"][0]["description"], "Search docs");
        assert_eq!(
            value["tools"][0]["parameters"],
            json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"]
            })
        );
        assert!(value["tools"][1].get("description").is_none());
    }

    #[test]
    fn test_stream_parses_text_and_tool_call() {
        let events = vec![
            json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "content_index": 0,
                "delta": "Hello"
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "echo",
                    "arguments": ""
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "output_index": 1,
                "delta": "{\"text\":\"hi\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "echo",
                    "arguments": "{\"text\":\"hi\"}",
                    "status": "completed"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 2,
                        "total_tokens": 3
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta == "Hello"))
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "echo")
        ));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_accumulates_function_call_arguments_deltas() {
        let events = vec![
            json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "fc_2",
                    "call_id": "call_2",
                    "name": "search",
                    "arguments": "{\"q\":\"ru"
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_2",
                "delta": "st\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_2",
                    "call_id": "call_2",
                    "name": "search",
                    "arguments": ""
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        let tool_end = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("tool call end");
        assert_eq!(tool_end.id, "call_2");
        assert_eq!(tool_end.name, "search");
        assert_eq!(tool_end.arguments, json!({ "q": "rust" }));
    }

    #[test]
    fn test_stream_synthesizes_tool_call_start_when_done_arrives_first() {
        let events = vec![
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_3",
                    "call_id": "call_3",
                    "name": "echo",
                    "arguments": "{\"text\":\"late start\"}",
                    "status": "completed"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        let start_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
            .expect("tool call start");
        let delta_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallDelta { delta, .. } if delta == "{\"text\":\"late start\"}"))
            .expect("tool call delta");
        let end_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
            .expect("tool call end");

        assert!(start_idx < delta_idx);
        assert!(delta_idx < end_idx);

        let tool_end = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("tool call end");
        assert_eq!(tool_end.id, "call_3");
        assert_eq!(tool_end.name, "echo");
        assert_eq!(tool_end.arguments, json!({ "text": "late start" }));
        assert!(matches!(
            out.last(),
            Some(StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn test_stream_preserves_orphan_tool_call_deltas_until_done() {
        let events = vec![
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_4",
                "delta": "{\"text\":\"buffered\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_4",
                    "call_id": "call_4",
                    "name": "echo",
                    "arguments": "",
                    "status": "completed"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
        );
        assert!(out.iter().any(
            |event| matches!(event, StreamEvent::ToolCallDelta { delta, .. } if delta == "{\"text\":\"buffered\"}")
        ));

        let tool_end = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("tool call end");
        assert_eq!(tool_end.id, "call_4");
        assert_eq!(tool_end.arguments, json!({ "text": "buffered" }));
    }

    #[test]
    fn test_stream_reads_late_tool_call_events_after_response_completed() {
        let body = [
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_5","delta":"{\"text\":\"late tool\"}"}"#,
            "",
            r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
            "",
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_5","call_id":"call_5","name":"echo","arguments":"","status":"completed"}}"#,
            "",
            "data: [DONE]",
            "",
        ]
        .join("\n");

        let out = collect_stream_events_from_body(&body);
        let start_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
            .expect("tool call start");
        let delta_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallDelta { delta, .. } if delta == "{\"text\":\"late tool\"}"))
            .expect("tool call delta");
        let end_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
            .expect("tool call end");
        let done_idx = out
            .iter()
            .position(|event| matches!(event, StreamEvent::Done { .. }))
            .expect("done event");

        assert!(start_idx < delta_idx);
        assert!(delta_idx < end_idx);
        assert!(end_idx < done_idx);

        let tool_end = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("tool call end");
        assert_eq!(tool_end.id, "call_5");
        assert_eq!(tool_end.arguments, json!({ "text": "late tool" }));
    }

    #[test]
    fn test_stream_materializes_output_text_done_without_deltas() {
        let events = vec![
            json!({
                "type": "response.output_text.done",
                "item_id": "msg_done",
                "content_index": 0,
                "text": "done-only text"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(
            out.get(1),
            Some(StreamEvent::TextStart { content_index: 0 })
        ));
        assert!(matches!(
            out.get(2),
            Some(StreamEvent::TextDelta {
                content_index: 0,
                delta,
            }) if delta == "done-only text"
        ));
        assert!(matches!(
            out.iter().find(|event| matches!(event, StreamEvent::TextEnd { .. })),
            Some(StreamEvent::TextEnd {
                content_index: 0,
                content,
            }) if content == "done-only text"
        ));
        assert!(matches!(
            out.last(),
            Some(StreamEvent::Done {
                reason: StopReason::Stop,
                ..
            })
        ));
    }

    #[test]
    fn test_stream_backfills_missing_output_text_suffix_from_done_event() {
        let events = vec![
            json!({
                "type": "response.output_text.delta",
                "item_id": "msg_suffix",
                "content_index": 0,
                "delta": "Hello"
            }),
            json!({
                "type": "response.output_text.done",
                "item_id": "msg_suffix",
                "content_index": 0,
                "text": "Hello world"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 2,
                        "total_tokens": 3
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        let text_deltas: Vec<(usize, String)> = out
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                } => Some((*content_index, delta.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_deltas,
            vec![(0, "Hello".to_string()), (0, " world".to_string())]
        );
        assert!(matches!(
            out.iter().find(|event| matches!(event, StreamEvent::TextEnd { .. })),
            Some(StreamEvent::TextEnd {
                content_index: 0,
                content,
            }) if content == "Hello world"
        ));
    }

    #[test]
    fn test_stream_materializes_reasoning_summary_part_done_without_deltas() {
        let events = vec![
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": "rs_done",
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": "Plan first, answer second."
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(
            out.get(1),
            Some(StreamEvent::ThinkingStart { content_index: 0 })
        ));
        assert!(matches!(
            out.get(2),
            Some(StreamEvent::ThinkingDelta {
                content_index: 0,
                delta,
            }) if delta == "Plan first, answer second."
        ));
        assert!(matches!(
            out.iter()
                .find(|event| matches!(event, StreamEvent::ThinkingEnd { .. })),
            Some(StreamEvent::ThinkingEnd {
                content_index: 0,
                content,
            }) if content == "Plan first, answer second."
        ));
    }

    #[test]
    fn test_stream_materializes_reasoning_text_done_without_deltas() {
        let events = vec![
            json!({
                "type": "response.reasoning_text.done",
                "item_id": "rt_done",
                "content_index": 0,
                "text": "Private chain of thought."
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(
            out.get(1),
            Some(StreamEvent::ThinkingStart { content_index: 0 })
        ));
        assert!(matches!(
            out.get(2),
            Some(StreamEvent::ThinkingDelta {
                content_index: 0,
                delta,
            }) if delta == "Private chain of thought."
        ));
        assert!(matches!(
            out.iter()
                .find(|event| matches!(event, StreamEvent::ThinkingEnd { .. })),
            Some(StreamEvent::ThinkingEnd {
                content_index: 0,
                content,
            }) if content == "Private chain of thought."
        ));
    }

    #[test]
    fn test_stream_separates_reasoning_summary_and_reasoning_text_for_same_item() {
        let events = vec![
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "rs_same",
                "summary_index": 0,
                "delta": "summary"
            }),
            json!({
                "type": "response.reasoning_text.delta",
                "item_id": "rs_same",
                "content_index": 0,
                "delta": "full reasoning"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2
                    }
                }
            }),
        ];

        let out = collect_events(&events);
        let thinking_starts: Vec<usize> = out
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingStart { content_index } => Some(*content_index),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_starts, vec![0, 1]);

        let thinking_deltas: Vec<(usize, String)> = out
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                } => Some((*content_index, delta.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking_deltas,
            vec![
                (0, "summary".to_string()),
                (1, "full reasoning".to_string())
            ]
        );

        let thinking_ends: Vec<(usize, String)> = out
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                } => Some((*content_index, content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking_ends,
            vec![
                (0, "summary".to_string()),
                (1, "full reasoning".to_string())
            ]
        );
    }

    #[test]
    fn test_finish_emits_terminal_block_end_events_in_content_order() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(vec![Ok(Vec::new())]);
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            state
                .process_event(
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_1",
                        "content_index": 0,
                        "delta": "hello",
                    })
                    .to_string(),
                )
                .expect("text delta");
            state
                .process_event(
                    &json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": "rs_1",
                        "summary_index": 0,
                        "delta": "think",
                    })
                    .to_string(),
                )
                .expect("reasoning delta");
            state
                .process_event(
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_2",
                        "content_index": 0,
                        "delta": "world",
                    })
                    .to_string(),
                )
                .expect("second text delta");

            state.finish(None);

            let terminal_end_kinds: Vec<(&'static str, usize)> = state
                .pending_events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::TextEnd { content_index, .. } => Some(("text", *content_index)),
                    StreamEvent::ThinkingEnd { content_index, .. } => {
                        Some(("thinking", *content_index))
                    }
                    _ => None,
                })
                .collect();

            assert_eq!(
                terminal_end_kinds,
                vec![("text", 0), ("thinking", 1), ("text", 2)]
            );
        });
    }

    #[test]
    fn test_open_tool_call_snapshots_sort_by_content_index() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(vec![Ok(Vec::new())]);
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            state.tool_calls_by_item_id.insert(
                "late".to_string(),
                ToolCallState {
                    content_index: 4,
                    call_id: "call-late".to_string(),
                    name: "later".to_string(),
                    arguments: "{\"b\":2}".to_string(),
                },
            );
            state.tool_calls_by_item_id.insert(
                "early".to_string(),
                ToolCallState {
                    content_index: 1,
                    call_id: "call-early".to_string(),
                    name: "earlier".to_string(),
                    arguments: "{\"a\":1}".to_string(),
                },
            );

            let ordered_ids: Vec<String> = state
                .open_tool_call_snapshots_in_content_order()
                .into_iter()
                .map(|(_, item_id, ..)| item_id)
                .collect();

            assert_eq!(ordered_ids, vec!["early".to_string(), "late".to_string()]);
        });
    }

    #[test]
    fn test_finish_keeps_open_tool_call_end_in_content_order() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(vec![Ok(Vec::new())]);
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            state
                .process_event(
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_1",
                        "content_index": 0,
                        "delta": "before",
                    })
                    .to_string(),
                )
                .expect("first text delta");
            state
                .process_event(
                    &json!({
                        "type": "response.output_item.added",
                        "item": {
                            "type": "function_call",
                            "id": "fc_7",
                            "call_id": "call_7",
                            "name": "echo",
                            "arguments": "{\"text\":\"mid\"}"
                        }
                    })
                    .to_string(),
                )
                .expect("tool call added");
            state
                .process_event(
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_2",
                        "content_index": 0,
                        "delta": "after",
                    })
                    .to_string(),
                )
                .expect("second text delta");

            state.finish(None);

            let terminal_event_order: Vec<(&'static str, usize)> = state
                .pending_events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::TextEnd { content_index, .. } => Some(("text", *content_index)),
                    StreamEvent::ToolCallEnd { content_index, .. } => {
                        Some(("tool", *content_index))
                    }
                    _ => None,
                })
                .collect();

            assert_eq!(
                terminal_event_order,
                vec![("text", 0), ("tool", 1), ("text", 2)]
            );
        });
    }

    #[test]
    fn test_stream_sets_bearer_auth_header() {
        let captured = run_stream_and_capture_headers().expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer test-openai-key")
        );
        assert_eq!(
            captured.headers.get("accept").map(String::as_str),
            Some("text/event-stream, application/x-ndjson, application/ndjson")
        );

        let body: Value = serde_json::from_str(&captured.body).expect("request body json");
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    fn build_test_jwt(account_id: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id
                }
            }))
            .expect("payload json"),
        );
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn test_bearer_token_parser_accepts_case_insensitive_scheme() {
        let parsed_bearer = super::bearer_token_from_authorization_header("bEaReR abc.def.ghi");
        assert_eq!(parsed_bearer.as_deref(), Some("abc.def.ghi"));
        assert!(super::bearer_token_from_authorization_header("Basic abc").is_none());
        assert!(super::bearer_token_from_authorization_header("Bearer").is_none());
    }

    #[test]
    fn test_codex_mode_adds_required_headers_with_authorization_override() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIResponsesProvider::new("gpt-4o")
            .with_provider_name("openai-codex")
            .with_api_name("openai-codex-responses")
            .with_codex_mode(true)
            .with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let test_jwt = build_test_jwt("acct_test_123");
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {test_jwt}"));
        let options = StreamOptions {
            headers,
            session_id: Some("session-abc".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        let expected_auth = format!("Bearer {test_jwt}");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some(expected_auth.as_str())
        );
        assert_eq!(captured.header_count("authorization"), 1);
        assert_eq!(
            captured.headers.get("user-agent").map(String::as_str),
            Some("pi_agent_rust")
        );
        assert_eq!(captured.header_count("user-agent"), 1);
        assert_eq!(
            captured
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acct_test_123")
        );
        assert_eq!(
            captured.headers.get("openai-beta").map(String::as_str),
            Some("responses=experimental")
        );
        assert_eq!(
            captured.headers.get("session_id").map(String::as_str),
            Some("session-abc")
        );
    }

    #[test]
    fn test_codex_mode_accepts_compat_authorization_header_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let test_jwt = build_test_jwt("acct_compat_456");
        let mut custom_headers = HashMap::new();
        custom_headers.insert("Authorization".to_string(), format!("Bearer {test_jwt}"));
        let provider = OpenAIResponsesProvider::new("gpt-4o")
            .with_provider_name("openai-codex")
            .with_api_name("openai-codex-responses")
            .with_codex_mode(true)
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..Default::default()
            }));
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider
                .stream(&context, &StreamOptions::default())
                .await
                .expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        assert_eq!(captured.header_count("authorization"), 1);
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {test_jwt}").as_str())
        );
        assert_eq!(
            captured
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acct_compat_456")
        );
    }

    #[test]
    fn test_stream_accepts_ndjson_content_type() {
        let body = concat!(
            "{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"ok\"}\n",
            "{\"type\":\"response.completed\",\"response\":{\"incomplete_details\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n"
        );
        let (base_url, _rx) = spawn_test_server(200, "application/x-ndjson", body);
        let provider = OpenAIResponsesProvider::new("gpt-4o").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut saw_delta = false;
            let mut done_usage: Option<Usage> = None;

            while let Some(event) = stream.next().await {
                let event = event.expect("stream event");
                match event {
                    StreamEvent::TextDelta { delta, .. } if delta == "ok" => {
                        saw_delta = true;
                    }
                    StreamEvent::Done { message, .. } => {
                        let usage = message.usage;
                        done_usage = Some(usage);
                        break;
                    }
                    StreamEvent::Error { .. } => {
                        panic!("unexpected error while reading NDJSON stream");
                    }
                    _ => {}
                }
            }

            assert!(saw_delta, "expected NDJSON text delta");
            let usage = done_usage.expect("missing Done usage");
            assert_eq!(usage.input, 1);
            assert_eq!(usage.output, 1);
            assert_eq!(usage.total_tokens, 2);
        });
    }

    fn ndjson_body_from_events(events: &[Value]) -> String {
        let mut lines = Vec::with_capacity(events.len());
        for event in events {
            let data = serde_json::to_string(event).expect("serialize event");
            lines.push(data);
        }
        let mut body = lines.join("\n");
        body.push('\n');
        body
    }

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(events.iter().map(|event| {
                let data = serde_json::to_string(event).expect("serialize event");
                Ok(format!("data: {data}\n\n").into_bytes())
            }));

            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            let mut out = Vec::new();
            loop {
                if let Some(event) = state.pending_events.pop_front() {
                    out.push(event);
                    continue;
                }

                if state.finished {
                    break;
                }

                match state.event_source.next().await {
                    Some(item) => {
                        let msg = item.expect("SSE event");
                        if msg.data == "[DONE]" {
                            if !state.finish_terminal_response() {
                                state.finish(None);
                            }
                            continue;
                        }
                        state.process_event(&msg.data).expect("process_event");
                    }
                    None => {
                        assert!(
                            state.finish_terminal_response(),
                            "stream ended without Done event"
                        );
                    }
                }
            }

            out
        })
    }

    fn collect_stream_events_from_body(body: &str) -> Vec<StreamEvent> {
        let (base_url, _rx) = spawn_test_server(200, "text/event-stream", body);
        let provider = OpenAIResponsesProvider::new("gpt-4o").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut out = Vec::new();
            while let Some(event) = stream.next().await {
                let event = event.expect("stream event");
                let is_terminal =
                    matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                out.push(event);
                if is_terminal {
                    break;
                }
            }
            out
        })
    }

    fn collect_stream_events_from_ndjson_body(body: &str) -> Vec<StreamEvent> {
        let (base_url, _rx) = spawn_test_server(200, "application/x-ndjson", body);
        let provider = OpenAIResponsesProvider::new("gpt-4o").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut out = Vec::new();
            while let Some(event) = stream.next().await {
                let event = event.expect("stream event");
                let is_terminal =
                    matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                out.push(event);
                if is_terminal {
                    break;
                }
            }
            out
        })
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        header_lines: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn header_count(&self, name: &str) -> usize {
            self.header_lines
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .count()
        }
    }

    fn run_stream_and_capture_headers() -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIResponsesProvider::new("gpt-4o").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    fn success_sse_body() -> String {
        [
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"ok"}"#,
            "",
            r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
            "",
        ]
        .join("\n")
    }

    fn spawn_test_server(
        status_code: u16,
        content_type: &str,
        body: &str,
    ) -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = mpsc::channel();
        let body = body.to_string();
        let content_type = content_type.to_string();

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match socket.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(err) => assert!(false, "request header read failed: {err}"),
                }
            }

            let header_end = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("request header boundary");
            let header_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let (headers, header_lines) = parse_headers(&header_text);
            let mut request_body = bytes[header_end + 4..].to_vec();

            let content_length = headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            while request_body.len() < content_length {
                match socket.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => request_body.extend_from_slice(&chunk[..n]),
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(err) => assert!(false, "request body read failed: {err}"),
                }
            }

            let captured = CapturedRequest {
                headers,
                header_lines,
                body: String::from_utf8_lossy(&request_body).to_string(),
            };
            tx.send(captured).expect("send captured request");

            let reason = match status_code {
                401 => "Unauthorized",
                500 => "Internal Server Error",
                _ => "OK",
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write response");
            socket.flush().expect("flush response");
        });

        (format!("http://{addr}/responses"), rx)
    }

    fn parse_headers(header_text: &str) -> (HashMap<String, String>, Vec<(String, String)>) {
        let mut headers = HashMap::new();
        let mut header_lines = Vec::new();
        for line in header_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                let normalized_name = name.trim().to_ascii_lowercase();
                let normalized_value = value.trim().to_string();
                header_lines.push((normalized_name.clone(), normalized_value.clone()));
                headers.insert(normalized_name, normalized_value);
            }
        }
        (headers, header_lines)
    }

    // ========================================================================
    // Fixture-based stream parsing tests
    // ========================================================================

    #[derive(Debug, Deserialize)]
    struct ProviderFixture {
        cases: Vec<ProviderCase>,
    }

    #[derive(Debug, Deserialize)]
    struct ProviderCase {
        name: String,
        events: Vec<Value>,
        expected: Vec<EventSummary>,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct EventSummary {
        kind: String,
        #[serde(default)]
        content_index: Option<usize>,
        #[serde(default)]
        delta: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    fn load_fixture(file_name: &str) -> ProviderFixture {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/provider_responses")
            .join(file_name);
        let data = std::fs::read_to_string(&path).expect("read fixture file");
        serde_json::from_str(&data).expect("parse fixture JSON")
    }

    fn summarize_event(event: &StreamEvent) -> EventSummary {
        match event {
            StreamEvent::Start { .. } => EventSummary {
                kind: "start".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: None,
            },
            StreamEvent::TextStart { content_index, .. } => EventSummary {
                kind: "text_start".to_string(),
                content_index: Some(*content_index),
                delta: None,
                content: None,
                reason: None,
            },
            StreamEvent::TextDelta {
                content_index,
                delta,
                ..
            } => EventSummary {
                kind: "text_delta".to_string(),
                content_index: Some(*content_index),
                delta: Some(delta.clone()),
                content: None,
                reason: None,
            },
            StreamEvent::TextEnd {
                content_index,
                content,
                ..
            } => EventSummary {
                kind: "text_end".to_string(),
                content_index: Some(*content_index),
                delta: None,
                content: Some(content.clone()),
                reason: None,
            },
            StreamEvent::Done { reason, .. } => EventSummary {
                kind: "done".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(*reason)),
            },
            StreamEvent::Error { reason, .. } => EventSummary {
                kind: "error".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(*reason)),
            },
            _ => EventSummary {
                kind: "other".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: None,
            },
        }
    }

    fn reason_to_string(reason: StopReason) -> String {
        match reason {
            StopReason::Stop => "stop".to_string(),
            StopReason::ToolUse => "tool_use".to_string(),
            StopReason::Length => "length".to_string(),
            StopReason::Error => "error".to_string(),
            StopReason::Aborted => "aborted".to_string(),
        }
    }

    #[test]
    fn test_stream_fixtures() {
        let fixture = load_fixture("openai_responses_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn test_stream_ndjson_fixtures() {
        let fixture = load_fixture("openai_responses_stream.json");
        for case in fixture.cases {
            let body = ndjson_body_from_events(&case.events);
            let events = collect_stream_events_from_ndjson_body(&body);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn ndjson_rejects_oversized_event() {
        let delta = "x".repeat(128);
        let event = json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "content_index": 0,
            "delta": delta,
        });
        let line = serde_json::to_string(&event).expect("serialize event");
        let max = line.len().saturating_sub(1).max(1);
        let body = format!("{line}\n");

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let err = runtime.block_on(async move {
            let byte_stream = stream::iter(vec![Ok(body.into_bytes())]);
            let mut event_source = NdjsonStream::with_max_event_bytes(byte_stream, max);
            match event_source.next().await {
                Some(Err(e)) => Some(e),
                _ => None,
            }
        });

        let err = err.expect("expected oversized NDJSON error");
        assert!(
            err.to_string().contains("NDJSON event exceeds"),
            "unexpected error: {err}"
        );
    }

    fn ndjson_body_for_deltas(deltas: &[String]) -> String {
        let mut lines = Vec::with_capacity(deltas.len() + 1);
        for delta in deltas {
            let event = json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "content_index": 0,
                "delta": delta,
            });
            lines.push(serde_json::to_string(&event).expect("serialize event"));
        }
        let done = json!({
            "type": "response.completed",
            "response": {
                "incomplete_details": null,
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });
        lines.push(serde_json::to_string(&done).expect("serialize event"));
        let mut body = lines.join("\n");
        body.push('\n');
        body
    }

    fn chunk_bytes(input: &[u8], chunk_sizes: &[usize]) -> Vec<Vec<u8>> {
        if chunk_sizes.is_empty() {
            return vec![input.to_vec()];
        }
        let mut chunks = Vec::new();
        let mut offset = 0usize;
        let mut idx = 0usize;
        while offset < input.len() {
            let size = chunk_sizes[idx % chunk_sizes.len()].max(1);
            let end = (offset + size).min(input.len());
            chunks.push(input[offset..end].to_vec());
            offset = end;
            idx += 1;
        }
        chunks
    }

    fn collect_ndjson_events_from_chunks(body: &str, chunk_sizes: &[usize]) -> Vec<StreamEvent> {
        let bytes = body.as_bytes();
        let chunks = chunk_bytes(bytes, chunk_sizes);
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(chunks.into_iter().map(Ok));
            let event_source = NdjsonStream::new(byte_stream);
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            let mut out = Vec::new();
            loop {
                if let Some(event) = state.pending_events.pop_front() {
                    out.push(event);
                    continue;
                }

                if state.finished {
                    break;
                }

                match state.event_source.next().await {
                    Some(item) => {
                        let msg = item.expect("NDJSON event");
                        state.process_event(&msg.data).expect("process_event");
                    }
                    None => {
                        assert!(
                            state.finish_terminal_response(),
                            "stream ended without Done event"
                        );
                    }
                }
            }

            out
        })
    }

    fn summarize_events(events: &[StreamEvent]) -> Vec<EventSummary> {
        events.iter().map(summarize_event).collect()
    }

    fn delta_strategy() -> impl Strategy<Value = String> {
        string_regex("[a-z0-9 ]{1,8}").expect("delta regex")
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            max_shrink_iters: 64,
            .. ProptestConfig::default()
        })]

        #[test]
        fn ndjson_chunking_invariant(
            deltas in prop::collection::vec(delta_strategy(), 1..6),
            chunk_sizes in prop::collection::vec(1usize..16, 0..8),
        ) {
            let body = ndjson_body_for_deltas(&deltas);
            let expected = summarize_events(&collect_ndjson_events_from_chunks(&body, &[]));
            let actual = summarize_events(&collect_ndjson_events_from_chunks(&body, &chunk_sizes));
            prop_assert_eq!(actual, expected);
        }
    }
}

// ============================================================================
// Fuzzing support
// ============================================================================

#[cfg(feature = "fuzzing")]
pub mod fuzz {
    use super::*;
    use futures::stream;
    use std::pin::Pin;

    type FuzzByteStream =
        Pin<Box<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>>;
    type FuzzStream = crate::sse::SseStream<FuzzByteStream>;

    /// Opaque wrapper around the OpenAI Responses stream processor state.
    pub struct Processor(StreamState<FuzzStream>);

    impl Default for Processor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Processor {
        /// Create a fresh processor with default state.
        pub fn new() -> Self {
            let empty = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
            let event_source = crate::sse::SseStream::new(Box::pin(empty));
            Self(StreamState::new(
                event_source,
                "gpt-responses-fuzz".into(),
                "openai-responses".into(),
                "openai".into(),
            ))
        }

        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            self.0.process_event(data)?;
            Ok(self.0.pending_events.drain(..).collect())
        }
    }
}
