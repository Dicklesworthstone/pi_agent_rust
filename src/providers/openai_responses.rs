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
const MAX_READY_EVENTS_PER_YIELD: usize = 64;

type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

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
    auth_header: bool,
    /// Whether the catalog entry declares this a reasoning model. When true,
    /// non-codex requests emit `reasoning: { effort, summary }` (plus the
    /// encrypted-content include) for a non-`off` thinking level, mirroring the
    /// TS `openai-responses` builder (gh #165). Defaults to `false`; the
    /// registry sets it from `ModelEntry::model.reasoning` via
    /// [`with_reasoning`](Self::with_reasoning).
    reasoning: bool,
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
            auth_header: true,
            reasoning: false,
        }
    }

    /// Declare whether the underlying catalog model is a reasoning model.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
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

    /// Control whether this route generates a Bearer Authorization header.
    /// Custom Authorization headers remain authoritative regardless of this flag.
    #[must_use]
    pub const fn with_auth_header(mut self, enabled: bool) -> Self {
        self.auth_header = enabled;
        self
    }

    pub fn build_request(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> OpenAIResponsesRequest {
        let input = build_openai_responses_input(context, &self.model, &self.provider, &self.api);
        let tools: Option<Vec<OpenAIResponsesTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(|tool| convert_tool_to_openai_responses(tool, self.codex_mode))
                    .collect(),
            )
        };

        let instructions = context.system_prompt.as_deref().map(ToString::to_string);

        // Codex mode requires additional fields per the TS reference implementation.
        // tool_choice and parallel_tool_calls are always sent (not conditional on tools).
        let (tool_choice, parallel_tool_calls, text, include, reasoning) = if self.codex_mode {
            let effort = options.thinking_level.as_ref().map_or_else(
                || Some("high".to_string()),
                |level| responses_effort_for_level(*level, self.compat.as_ref()),
            );
            (
                Some("auto"),
                Some(true),
                Some(OpenAIResponsesTextConfig {
                    verbosity: "medium",
                }),
                Some(vec!["reasoning.encrypted_content"]),
                effort.map(|effort| OpenAIResponsesReasoning {
                    effort,
                    summary: Some("auto"),
                }),
            )
        } else if let Some(effort) = options
            .thinking_level
            .filter(|level| self.reasoning && *level != crate::model::ThinkingLevel::Off)
            .and_then(|level| responses_effort_for_level(level, self.compat.as_ref()))
        {
            // Non-codex reasoning models mirror the TS `openai-responses`
            // builder (gh #165): when the catalog marks the model as reasoning
            // and a thinking level is requested, send `reasoning: { effort,
            // summary: "auto" }` plus the encrypted-content include so
            // reasoning items round-trip across turns.
            (
                None,
                None,
                None,
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
            prompt_cache_key: options.prompt_cache_key.clone(),
            service_tier: crate::provider::openai_service_tier(
                if self.codex_mode {
                    "openai-codex"
                } else {
                    &self.provider
                },
                options.service_tier.as_deref(),
            ),
        }
    }
}

/// Map pi's thinking level onto the Responses API `reasoning.effort` value.
///
/// A per-model `thinkingLevelMap` from the catalog (gh #117/#165) overrides the
/// default mapping (e.g. `{"xhigh": "high"}` for gateways with different
/// vocabularies), keyed by the lowercase `ThinkingLevel` name. Without an
/// override the level name itself is the effort value, matching the TS
/// implementation which passes thinking levels straight through.
fn responses_effort_for_level(
    level: crate::model::ThinkingLevel,
    compat: Option<&CompatConfig>,
) -> Option<String> {
    let name = level.to_string();
    if let Some(mapped) = compat
        .and_then(|c| c.thinking_level_map.as_ref())
        .and_then(|map| map.get(name.as_str()))
    {
        return mapped.clone();
    }
    Some(name)
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

/// Derive the sibling native-compact endpoint from a Responses base URL
/// (gh #167): `…/responses` → `…/responses/compact`. Returns `None` when the
/// base URL does not end in `/responses` (a custom shape we cannot safely
/// extend), so callers fail open instead of POSTing to a guessed route.
fn derive_compact_url(base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        Some(format!("{base}/compact"))
    } else {
        None
    }
}

impl OpenAIResponsesProvider {
    /// POST a sanitized, extension-composed compact request to the native
    /// `…/responses/compact` endpoint and return the raw response JSON
    /// (gh #167). Auth mirrors [`Provider::stream`]: `options.api_key` /
    /// `OPENAI_API_KEY` (keyless-local providers skip auth entirely), plus
    /// codex-mode and compat headers. Credentials never appear in the
    /// returned value.
    async fn compact_native_impl(
        &self,
        request_body: &serde_json::Value,
        options: &StreamOptions,
    ) -> Result<serde_json::Value> {
        let Some(url) = derive_compact_url(&self.base_url) else {
            return Err(Error::provider(
                self.name(),
                format!(
                    "cannot derive a /responses/compact endpoint from base URL {} — native compaction unavailable",
                    self.base_url
                ),
            ));
        };

        // Auth resolution: identical policy to `stream()` above.
        let authorization_header_value = authorization_override(options, self.compat.as_ref());
        let auth_value = if authorization_header_value.is_some() || !self.auth_header {
            None
        } else {
            let resolved = options
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            match resolved {
                Some(key) => Some(key),
                None if crate::provider_metadata::provider_is_keyless_local(self.name()) => None,
                None => {
                    return Err(Error::provider(
                        self.name(),
                        "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                    ));
                }
            }
        };

        let mut request = self.client.post(&url).header("Accept", "application/json");
        if let Some(ref auth) = auth_value {
            request = request.header("Authorization", format!("Bearer {auth}"));
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
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization"],
            );
        }
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );

        let request = request.json(request_body)?;
        let response = Box::pin(request.send()).await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
        if !(200..300).contains(&status) {
            return Err(Error::provider(
                self.name(),
                format!("OpenAI compact endpoint error (HTTP {status}): {body}"),
            ));
        }
        serde_json::from_str(&body).map_err(|err| {
            Error::api(format!(
                "OpenAI compact endpoint returned non-JSON (HTTP {status}): {err}"
            ))
        })
    }
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

    async fn compact_native(
        &self,
        request_body: &serde_json::Value,
        options: &StreamOptions,
    ) -> Result<serde_json::Value> {
        self.compact_native_impl(request_body, options).await
    }

    #[allow(clippy::too_many_lines)]
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let authorization_header_value = authorization_override(options, self.compat.as_ref());

        let auth_value = if authorization_header_value.is_some() || !self.auth_header {
            None
        } else {
            let resolved = options
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            match resolved {
                Some(key) => Some(key),
                // Local / self-hosted providers (ollama, llamacpp, mistralrs, …)
                // require NO API key; proceed without an Authorization header
                // rather than failing. (#104)
                None if crate::provider_metadata::provider_is_keyless_local(self.name()) => None,
                None => {
                    return Err(Error::provider(
                        self.name(),
                        "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                    ));
                }
            }
        };

        let request_body = self.build_request(context, options);

        // before_provider_request rewrite hook (gh #167 / bd-1q31s): offer
        // the fully-built body to extensions, validate any rewrite, and fail
        // open to the original body on rejection. The rewritten JSON is sent
        // verbatim (after validation) so extension-added provider fields
        // survive; auth headers are never part of the exchange.
        let mut rewritten_body: Option<serde_json::Value> = None;
        if let Some(hook) = &options.before_provider_request {
            match serde_json::to_value(&request_body) {
                Ok(original) => {
                    let event = crate::provider::BeforeProviderRequestEvent {
                        provider: self.provider.clone(),
                        api: self.api.clone(),
                        model: self.model.clone(),
                        base_url: self.base_url.clone(),
                        payload: original,
                    };
                    if let Some(mutated) = hook.rewrite(event).await {
                        match validate_rewritten_responses_request(mutated) {
                            Ok(validated) => rewritten_body = Some(validated),
                            Err(reason) => {
                                tracing::warn!(
                                    "before_provider_request rewrite rejected (fail-open, original request kept): {reason}"
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "failed to serialize request body for before_provider_request (fail-open): {err}"
                    );
                }
            }
        }

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
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization"],
            );
        }

        // Per-request headers from StreamOptions (highest priority).
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );

        let request = match &rewritten_body {
            Some(body) => request.json(body)?,
            None => request.json(&request_body)?,
        };

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

        Ok(wire_stream(
            response.bytes_stream(),
            use_ndjson,
            self.model.clone(),
            self.api().to_string(),
            self.name().to_string(),
        ))
    }
}

async fn yield_stream_turn() {
    let mut yielded = false;
    std::future::poll_fn(|task| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            task.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

fn wire_stream<S>(
    source: S,
    use_ndjson: bool,
    model: String,
    api: String,
    provider: String,
) -> EventStream
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin + Send + 'static,
{
    let event_source = if use_ndjson {
        ResponsesEventStream::Ndjson(NdjsonStream::new(source))
    } else {
        ResponsesEventStream::Sse(SseStream::new(source))
    };
    let state = StreamState::new(event_source, model, api, provider);
    let output = stream::unfold(Some(state), |active| async move {
        let mut state = active?;
        let mut ready_events = 0;
        loop {
            if let Some(event) = state.pending_events.pop_front() {
                let terminal =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                // Release the source, buffered wire data, and partial tool state
                // before the caller observes a terminal event and stops polling.
                let active = (!terminal).then_some(state);
                return Some((Ok(event), active));
            }

            // Drain block endings queued at the wire boundary before stopping.
            if state.finished {
                return None;
            }

            // Metadata-only events can always be ready, even when the underlying
            // SSE/NDJSON parser yields fairly within each individual event.
            if ready_events == MAX_READY_EVENTS_PER_YIELD {
                yield_stream_turn().await;
                ready_events = 0;
            }
            ready_events += 1;

            match state.event_source.next().await {
                Some(Ok(msg)) => {
                    state.transient_error_count = 0;
                    if msg.data == "[DONE]" {
                        // Completion metadata can precede trailing item events
                        // and usage. Finalize only at the wire boundary.
                        if !state.finish_terminal_response() {
                            state.finish(None);
                        }
                        continue;
                    }

                    if let Err(error) = state.process_event(&msg.data) {
                        // A fatal frame must not leave a live source that can
                        // authorize trailing tools when a caller repolls.
                        return Some((Err(error), None));
                    }
                }
                Some(Err(error)) => {
                    // Preserve the bounded retry policy for transient wire errors.
                    const MAX_CONSECUTIVE_TRANSIENT_ERRORS: usize = 5;
                    if error.kind() == std::io::ErrorKind::WriteZero
                        || error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut
                    {
                        state.transient_error_count += 1;
                        if state.transient_error_count <= MAX_CONSECUTIVE_TRANSIENT_ERRORS {
                            tracing::warn!(
                                kind = ?error.kind(),
                                count = state.transient_error_count,
                                "Transient error in SSE stream, continuing"
                            );
                            continue;
                        }
                        tracing::warn!(
                            kind = ?error.kind(),
                            "Error persisted after {MAX_CONSECUTIVE_TRANSIENT_ERRORS} \
                             consecutive attempts, treating as fatal"
                        );
                    }
                    return Some((Err(Error::sse(&error)), None));
                }
                None => {
                    if state.finish_terminal_response() {
                        continue;
                    }

                    return Some((Err(Error::api("Stream ended without Done event")), None));
                }
            }
        }
    });

    // A terminal item retires the state; fusion also makes repeated EOF polls safe.
    Box::pin(output.fuse())
}

// ============================================================================
// NDJSON Stream
// ============================================================================

const NDJSON_MAX_EVENT_BYTES: usize = 100 * 1024 * 1024;
const MAX_NDJSON_CHUNKS_PER_POLL: usize = 64;
const MAX_NDJSON_LINES_PER_POLL: usize = 64;

struct NdjsonStream<S> {
    inner: S,
    buffer: Vec<u8>,
    buffer_offset: usize,
    finished: bool,
    max_event_bytes: usize,
}

impl<S> NdjsonStream<S> {
    const fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            buffer_offset: 0,
            finished: false,
            max_event_bytes: NDJSON_MAX_EVENT_BYTES,
        }
    }

    #[cfg(test)]
    fn with_max_event_bytes(inner: S, max_event_bytes: usize) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            buffer_offset: 0,
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

    fn parse_line(&self, line_bytes: &[u8]) -> std::io::Result<Option<SseEvent>> {
        if line_bytes.len() > self.max_event_bytes {
            return Err(self.event_too_large_error());
        }
        let line = std::str::from_utf8(line_bytes).map_err(|_| Self::invalid_utf8_error())?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::event_from_line(line.to_string())))
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
        if self.finished {
            return Poll::Ready(None);
        }

        let mut chunks = 0;
        let mut lines = 0;
        loop {
            // Empty chunks and coalesced blank lines produce no provider event,
            // so both require their own cooperative polling budget here.
            if lines == MAX_NDJSON_LINES_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let unread = &self.buffer[self.buffer_offset..];
            if let Some(pos) = memchr::memchr(b'\n', unread) {
                let event = self.parse_line(&unread[..pos]);
                self.buffer_offset += pos + 1;
                lines += 1;
                match event {
                    Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                    Ok(None) => continue,
                    Err(error) => {
                        self.finished = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                }
            }
            if unread.len() > self.max_event_bytes {
                self.finished = true;
                return Poll::Ready(Some(Err(self.event_too_large_error())));
            }
            if chunks == MAX_NDJSON_CHUNKS_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            // Compact only before reading more bytes. Parsing a coalesced batch
            // does not repeatedly shift its entire unread tail for every line.
            let consumed = self.buffer_offset;
            if consumed > 0 {
                drop(self.buffer.drain(..consumed));
                self.buffer_offset = 0;
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    chunks += 1;
                    self.buffer.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(None) => {
                    self.finished = true;
                    let line = std::mem::take(&mut self.buffer);
                    match self.parse_line(&line) {
                        Ok(event) => return Poll::Ready(event.map(Ok)),
                        Err(error) => return Poll::Ready(Some(Err(error))),
                    }
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

// Match the 1024-block admission bound used by other streaming providers.
// Completed and orphan identities count too: releasing them early would let a
// later event reuse a call or grow a second, unbounded tracking lane.
const MAX_STREAMED_TOOL_ITEMS: usize = 1024;
const MAX_STREAMED_TOOL_IDENTITY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum ToolItemEvent {
    Added,
    ArgumentsDelta,
    ArgumentsDone,
    Done,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct ToolItemIdentity {
    output_index: Option<u32>,
    function: Option<(String, String)>,
    added: bool,
    arguments_done: bool,
    has_arguments_snapshot: bool,
    completed: bool,
}

impl ToolItemIdentity {
    fn check(
        &self,
        function: Option<(&str, &str)>,
        output_index: Option<u32>,
        event: ToolItemEvent,
    ) -> std::result::Result<(), &'static str> {
        if self.completed {
            return Err("tool item received an event after completion");
        }
        if self.added && matches!(event, ToolItemEvent::Added) {
            return Err("tool item was added more than once");
        }
        if self.arguments_done
            && matches!(
                event,
                ToolItemEvent::ArgumentsDelta | ToolItemEvent::ArgumentsDone
            )
        {
            return Err("tool arguments received an event after finalization");
        }
        if let (Some(previous), Some(index)) = (self.output_index, output_index)
            && previous != index
        {
            return Err("tool item changed output index");
        }
        if let (Some((previous_call, previous_name)), Some((call_id, name))) =
            (self.function.as_ref(), function)
        {
            if previous_call != call_id {
                return Err("tool item changed call ID");
            }
            if previous_name != name {
                return Err("tool item changed function name");
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct ToolIdentities {
    items: HashMap<String, ToolItemIdentity>,
    item_by_call_id: HashMap<String, String>,
    item_by_output_index: HashMap<u32, String>,
    bytes: usize,
}

impl ToolIdentities {
    fn record(
        &mut self,
        item_id: &str,
        function: Option<(&str, &str)>,
        output_index: Option<u32>,
        event: ToolItemEvent,
    ) -> std::result::Result<(), &'static str> {
        if item_id.trim().is_empty() {
            return Err("tool item ID is empty");
        }
        let existing = self.items.get(item_id);
        if let Some(identity) = existing {
            identity.check(function, output_index, event)?;
        } else if self.items.len() >= MAX_STREAMED_TOOL_ITEMS {
            return Err("tool identity count limit exceeded");
        }
        if let Some((call_id, name)) = function {
            if call_id.trim().is_empty() {
                return Err("tool call ID is empty");
            }
            if name.trim().is_empty() {
                return Err("tool function name is empty");
            }
            if self
                .item_by_call_id
                .get(call_id)
                .is_some_and(|previous| previous != item_id)
            {
                return Err("tool call ID belongs to another item");
            }
        }
        if let Some(index) = output_index
            && self
                .item_by_output_index
                .get(&index)
                .is_some_and(|previous| previous != item_id)
        {
            return Err("tool output index belongs to another item");
        }

        let new_item_bytes = if existing.is_none() { item_id.len() } else { 0 };
        let new_function_bytes = if existing.is_none_or(|identity| identity.function.is_none()) {
            function.map_or(Some(0), |(call_id, name)| {
                call_id.len().checked_add(name.len())
            })
        } else {
            Some(0)
        };
        let bytes = new_function_bytes
            .and_then(|bytes| bytes.checked_add(new_item_bytes))
            .and_then(|bytes| bytes.checked_add(self.bytes))
            .filter(|bytes| *bytes <= MAX_STREAMED_TOOL_IDENTITY_BYTES)
            .ok_or("tool identity byte limit exceeded")?;

        // All checks precede mutation, including the first anonymous delta.
        let identity = self.items.entry(item_id.to_string()).or_default();
        if identity.output_index.is_none()
            && let Some(index) = output_index
        {
            identity.output_index = Some(index);
            self.item_by_output_index.insert(index, item_id.to_string());
        }
        if identity.function.is_none()
            && let Some((call_id, name)) = function
        {
            identity.function = Some((call_id.to_string(), name.to_string()));
            self.item_by_call_id
                .insert(call_id.to_string(), item_id.to_string());
        }
        match event {
            ToolItemEvent::Added => identity.added = true,
            ToolItemEvent::ArgumentsDelta => {}
            ToolItemEvent::ArgumentsDone => identity.arguments_done = true,
            ToolItemEvent::Done => identity.completed = true,
        }
        self.bytes = bytes;
        Ok(())
    }
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
    tool_identities: ToolIdentities,
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
                stop_details: None,
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
            tool_identities: ToolIdentities::default(),
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
        if self.finished {
            return Ok(());
        }
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
            OpenAIResponsesChunk::OutputItemAdded { item, output_index } => {
                if matches!(&item, OpenAIResponsesOutputItem::Unknown)
                    && self.reject_tool_kind_change(None, output_index)
                {
                    return Ok(());
                }
                if let OpenAIResponsesOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    self.ensure_started();
                    if !self.admit_tool_event(
                        &id,
                        Some((&call_id, &name)),
                        output_index,
                        ToolItemEvent::Added,
                    ) {
                        return Ok(());
                    }

                    let mut buffered_arguments = self
                        .orphan_tool_call_arguments
                        .remove(&id)
                        .unwrap_or_default();
                    let has_arguments_snapshot = self
                        .tool_identities
                        .items
                        .get(&id)
                        .is_some_and(|identity| identity.has_arguments_snapshot);
                    if buffered_arguments.is_empty() || !has_arguments_snapshot {
                        buffered_arguments.insert_str(0, &arguments);
                    }

                    let content_index = self.partial.content.len();
                    self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: serde_json::Value::Null,
                        // Preserve the original `fc_...` output-item id so
                        // subsequent requests can replay it (OpenAI validates
                        // reasoning/function-call pairing by item id).
                        thought_signature: Some(id.clone()),
                    }));

                    self.pending_events.push_back(StreamEvent::ToolCallStart {
                        content_index,
                        id: call_id.clone(),
                        name: name.clone(),
                    });

                    self.tool_calls_by_item_id.insert(
                        id,
                        ToolCallState {
                            content_index,
                            call_id,
                            name,
                            arguments: buffered_arguments.clone(),
                        },
                    );

                    if !buffered_arguments.is_empty() {
                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,
                            delta: buffered_arguments,
                        });
                    }
                }
            }
            OpenAIResponsesChunk::FunctionCallArgumentsDelta {
                item_id,
                output_index,
                delta,
            } => {
                self.ensure_started();
                if !self.admit_tool_event(
                    &item_id,
                    None,
                    output_index,
                    ToolItemEvent::ArgumentsDelta,
                ) {
                    return Ok(());
                }
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
            OpenAIResponsesChunk::FunctionCallArgumentsDone {
                item_id,
                output_index,
                arguments,
            } => {
                self.ensure_started();
                if !self.admit_tool_event(
                    &item_id,
                    None,
                    output_index,
                    ToolItemEvent::ArgumentsDone,
                ) {
                    return Ok(());
                }
                // A final snapshot may correct earlier deltas. Preserve the
                // established empty-field fallback; item completion still owns
                // ToolCallEnd and strict parsing of the final execution input.
                if !arguments.is_empty()
                    && let Some(identity) = self.tool_identities.items.get_mut(&item_id)
                {
                    identity.has_arguments_snapshot = true;
                }
                let buffered = self.tool_calls_by_item_id.get_mut(&item_id).map_or_else(
                    || self.orphan_tool_call_arguments.entry(item_id).or_default(),
                    |call| &mut call.arguments,
                );
                if !arguments.is_empty() {
                    *buffered = arguments;
                }
            }
            OpenAIResponsesChunk::OutputItemDone { item, output_index } => {
                if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call")
                    && self.reject_tool_kind_change(
                        item.get("id").and_then(serde_json::Value::as_str),
                        output_index,
                    )
                {
                    return Ok(());
                }
                // Legacy parity: before the raw-item capture lane, only
                // `function_call` was a known done-item shape — anything else
                // (including malformed `reasoning`/`message` payloads) fell
                // into the `#[serde(other)]` arm and was ignored. Keep the
                // hard error for malformed function calls, and keep the new
                // capture paths opportunistic so a partial item never aborts
                // the stream.
                let typed =
                    match serde_json::from_value::<OpenAIResponsesOutputItemDone>(item.clone()) {
                        Ok(typed) => typed,
                        Err(e) => {
                            if item.get("type").and_then(serde_json::Value::as_str)
                                == Some("function_call")
                            {
                                return Err(Error::api(format!(
                                    "JSON parse error: {e}\nData: {data}"
                                )));
                            }
                            OpenAIResponsesOutputItemDone::Unknown
                        }
                    };
                match typed {
                    OpenAIResponsesOutputItemDone::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                    } => {
                        self.ensure_started();
                        if !self.admit_tool_event(
                            &id,
                            Some((&call_id, &name)),
                            output_index,
                            ToolItemEvent::Done,
                        ) {
                            return Ok(());
                        }
                        self.end_tool_call(&id, &call_id, &name, &arguments);
                    }
                    OpenAIResponsesOutputItemDone::Reasoning {
                        id,
                        summary,
                        encrypted_content,
                    } => {
                        let summary_text = summary
                            .iter()
                            .map(|part| part.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.capture_reasoning_item(
                            &id,
                            encrypted_content.as_deref(),
                            &summary_text,
                            &item,
                        );
                    }
                    OpenAIResponsesOutputItemDone::Message { id } => {
                        self.capture_message_item_id(&id);
                    }
                    OpenAIResponsesOutputItemDone::Unknown => {}
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

    fn reject_tool_kind_change(
        &mut self,
        item_id: Option<&str>,
        output_index: Option<u32>,
    ) -> bool {
        let collision = item_id.is_some_and(|id| self.tool_identities.items.contains_key(id))
            || output_index.is_some_and(|index| {
                self.tool_identities
                    .item_by_output_index
                    .contains_key(&index)
            });
        if collision {
            self.reject_tool_response(
                "Invalid OpenAI Responses tool identity: tool item changed output kind".to_string(),
            );
        }
        collision
    }

    fn admit_tool_event(
        &mut self,
        item_id: &str,
        function: Option<(&str, &str)>,
        output_index: Option<u32>,
        event: ToolItemEvent,
    ) -> bool {
        if let Err(message) = self
            .tool_identities
            .record(item_id, function, output_index, event)
        {
            self.reject_tool_response(format!("Invalid OpenAI Responses tool identity: {message}"));
            false
        } else {
            true
        }
    }

    /// Preserve a completed `reasoning` output item (TS parity: the raw item
    /// JSON — id, summary, `encrypted_content`, status — is stored in the
    /// thinking block's `thinking_signature` so subsequent requests can replay
    /// it verbatim).
    ///
    /// The signature lands on the first thinking block streamed for the item.
    /// A reasoning item that produced no summary/text deltas gets a fresh
    /// (possibly empty) thinking block, but only when it carries
    /// `encrypted_content` — otherwise there is nothing replayable and the
    /// legacy behavior (ignore) is preserved.
    fn capture_reasoning_item(
        &mut self,
        item_id: &str,
        encrypted_content: Option<&str>,
        summary_text: &str,
        raw_item: &serde_json::Value,
    ) {
        let existing_idx = self
            .reasoning_blocks
            .iter()
            .filter(|(key, _)| key.item_id == item_id)
            .map(|(_, idx)| *idx)
            .min();

        let idx = if let Some(idx) = existing_idx {
            idx
        } else {
            if encrypted_content.is_none() {
                return;
            }
            self.ensure_started();
            let idx = self.reasoning_block_for(ReasoningKind::Summary, item_id.to_string(), 0);
            if !summary_text.is_empty() {
                self.apply_reasoning_snapshot(
                    ReasoningKind::Summary,
                    item_id.to_string(),
                    0,
                    summary_text.to_string(),
                );
            }
            idx
        };

        if let Some(ContentBlock::Thinking(block)) = self.partial.content.get_mut(idx) {
            block.thinking_signature = serde_json::to_string(raw_item).ok();
        }
    }

    /// Record the original `msg_...` item id on the text blocks streamed for a
    /// completed `message` output item (TS parity: `textSignature = item.id`),
    /// enabling faithful replay of the item id on subsequent requests.
    fn capture_message_item_id(&mut self, item_id: &str) {
        let indexes: Vec<usize> = self
            .text_blocks
            .iter()
            .filter(|(key, _)| key.item_id == item_id)
            .map(|(_, idx)| *idx)
            .collect();
        for idx in indexes {
            if let Some(ContentBlock::Text(block)) = self.partial.content.get_mut(idx) {
                block.text_signature = Some(item_id.to_string());
            }
        }
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
                    thought_signature: Some(item_id.to_string()),
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
                id: tc.call_id.clone(),
                name: tc.name.clone(),
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

        let parsed_args = match super::openai::parse_final_tool_arguments(&tc.arguments) {
            Ok(arguments) => arguments,
            Err(message) => {
                self.reject_tool_arguments(tc.content_index, message);
                return;
            }
        };

        self.partial.stop_reason = StopReason::ToolUse;
        self.pending_events.push_back(StreamEvent::ToolCallEnd {
            content_index: tc.content_index,
            tool_call: ToolCall {
                id: tc.call_id.clone(),
                name: tc.name.clone(),
                arguments: parsed_args.clone(),
                thought_signature: Some(item_id.to_string()),
            },
        });

        if let Some(ContentBlock::ToolCall(block)) = self.partial.content.get_mut(tc.content_index)
        {
            block.id = tc.call_id;
            block.name = tc.name;
            block.arguments = parsed_args;
            block.thought_signature = Some(item_id.to_string());
        }
    }

    fn reject_tool_arguments(&mut self, content_index: usize, message: &str) {
        // A completed item is an execution boundary, unlike the incremental
        // argument deltas. Do not promote malformed/truncated JSON (or a JSON
        // scalar) into an executable tool call with invented null arguments.
        self.reject_tool_response(format!(
            "Invalid OpenAI Responses tool arguments at content index {content_index}: {message}"
        ));
    }

    fn reject_tool_response(&mut self, message: String) {
        self.ensure_started();
        self.partial.stop_reason = StopReason::Error;
        self.partial.error_message = Some(message);
        // `finish` may have queued other calls from the same batch before it
        // reached this one. None of those buffered completions can authorize
        // execution after the batch has failed validation.
        self.pending_events
            .retain(|event| !matches!(event, StreamEvent::ToolCallEnd { .. }));
        self.pending_events.push_back(StreamEvent::Error {
            reason: StopReason::Error,
            error: std::mem::take(&mut self.partial),
        });
        self.tool_calls_by_item_id.clear();
        self.orphan_tool_call_arguments.clear();
        self.tool_identities = ToolIdentities::default();
        self.terminal_response_seen = false;
        self.terminal_incomplete_reason = None;
        self.finished = true;
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
                ContentBlock::Image(_)
                | ContentBlock::Media(_)
                | ContentBlock::RedactedThinking(_) => None,
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
        if !self.orphan_tool_call_arguments.is_empty() {
            self.reject_tool_response(
                "Invalid OpenAI Responses tool identity: argument stream ended without a function-call identity"
                    .to_string(),
            );
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
                        if self.finished {
                            return;
                        }
                    }
                }
            }
        }

        // Best-effort: close any tool calls whose content block disappeared unexpectedly.
        for (_, item_id, call_id, name, arguments) in open_tool_calls {
            self.end_tool_call(&item_id, &call_id, &name, &arguments);
            if self.finished {
                return;
            }
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

/// Validated re-ingest of a `before_provider_request` rewrite (bd-1q31s).
///
/// The typed request struct cannot round-trip through `Deserialize` (it holds
/// `&'static str` fields), so validation is structural: the rewrite must be a
/// JSON object that still names a model and carries an `input` array, and it
/// must not disable streaming — the SSE reader depends on `stream: true`, so
/// that invariant is re-imposed on the accepted value.
fn validate_rewritten_responses_request(
    mut value: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let Some(object) = value.as_object_mut() else {
        return Err("rewrite is not a JSON object".to_string());
    };
    match object.get("model") {
        Some(serde_json::Value::String(model)) if !model.is_empty() => {}
        _ => return Err("rewrite is missing a non-empty string `model`".to_string()),
    }
    if !object.get("input").is_some_and(serde_json::Value::is_array) {
        return Err("rewrite is missing an `input` array".to_string());
    }
    object.insert("stream".to_string(), serde_json::Value::Bool(true));
    Ok(value)
}

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
    /// Cache-affinity key (`prompt_cache_key`, gh #188). Parity with the TS
    /// `openai-responses` and `openai-codex-responses` providers, which both
    /// send it. Omitted when unset so the wire format is unchanged. See
    /// `StreamOptions::prompt_cache_key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Processing tier (`/fast` → `priority`); OpenAI and Codex only. See
    /// `provider::openai_service_tier`.
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
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
    /// Assistant `message` output item replayed with its original item id and
    /// `status`, mirroring the TS reference (`ResponseOutputMessage`). Only
    /// emitted when the source text block carries a `text_signature` captured
    /// from a previous Responses stream; signature-less text keeps the legacy
    /// role/content shape above.
    AssistantMessageItem {
        #[serde(rename = "type")]
        r#type: &'static str,
        role: &'static str,
        id: String,
        status: &'static str,
        content: Vec<OpenAIResponsesAssistantContentPart>,
    },
    FunctionCall {
        #[serde(rename = "type")]
        r#type: &'static str,
        /// Original Responses output-item id (`fc_...`). Replayed so OpenAI's
        /// reasoning/function-call pairing validation recognizes the item.
        /// Absent for legacy sessions with no captured metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        r#type: &'static str,
        call_id: String,
        output: OpenAIResponsesFunctionOutput,
    },
    /// Raw provider item replayed verbatim (currently: `reasoning` items with
    /// their `encrypted_content`, captured on parse into
    /// `ThinkingContent::thinking_signature`).
    RawItem(serde_json::Value),
}

/// Responses and Codex accept the same text/image content parts in function
/// outputs as in user input. Keep ordinary tool output as a string; an image
/// requires the structured form so its bytes stay attached to its call ID.
/// <https://developers.openai.com/api/docs/guides/function-calling#formatting-results>
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIResponsesFunctionOutput {
    Text(String),
    Content(Vec<OpenAIResponsesUserContentPart>),
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
    OutputText {
        text: String,
        /// Present (as `[]`) only on replayed `message` items, matching the TS
        /// reference; omitted on legacy role/content assistant items so their
        /// serialization stays byte-identical.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<serde_json::Value>>,
    },
}

#[derive(Debug, Serialize)]
struct OpenAIResponsesTool {
    #[serde(rename = "type")]
    r#type: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

fn convert_tool_to_openai_responses(tool: &ToolDef, codex_mode: bool) -> OpenAIResponsesTool {
    let parameters = if codex_mode {
        super::tool_schema::codex_tool_schema(&tool.parameters)
    } else {
        std::borrow::Cow::Borrowed(&tool.parameters)
    };
    // An open item schema is intentional. Do not let Responses' implicit
    // strict adaptation close it or make optional arguments mandatory.
    let strict = matches!(parameters, std::borrow::Cow::Owned(_)).then_some(false);
    OpenAIResponsesTool {
        r#type: "function",
        name: tool.name.clone(),
        description: if tool.description.trim().is_empty() {
            None
        } else {
            Some(tool.description.clone())
        },
        parameters: parameters.into_owned(),
        strict,
    }
}

#[allow(clippy::too_many_lines)]
fn build_openai_responses_input(
    context: &Context<'_>,
    model: &str,
    provider: &str,
    api: &str,
) -> Vec<OpenAIResponsesInputItem> {
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
                // Raw output items belong to one provider, API and model.
                // Another endpoint can use the same item-id prefixes without
                // being able to decrypt its reasoning or resolve its IDs.
                // Gate here as well as in Agent screening: direct Provider
                // callers and embedded `call_id|fc_id` metadata also reach us.
                let same_origin = assistant.model == model
                    && assistant.provider == provider
                    && assistant.api == api;
                // Preserve ordering between text and tool calls.
                let mut pending_text = String::new();
                let flush_pending =
                    |pending: &mut String, input: &mut Vec<OpenAIResponsesInputItem>| {
                        if !pending.is_empty() {
                            input.push(OpenAIResponsesInputItem::Assistant {
                                role: "assistant",
                                content: vec![OpenAIResponsesAssistantContentPart::OutputText {
                                    text: std::mem::take(pending),
                                    annotations: None,
                                }],
                            });
                        }
                    };

                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(t) => {
                            let replay_id = if same_origin {
                                replayable_message_item_id(t)
                            } else {
                                None
                            };
                            if let Some(item_id) = replay_id {
                                // Replay the original `message` output item with
                                // its id + status so OpenAI's reasoning pairing
                                // validation recognizes it (TS parity).
                                flush_pending(&mut pending_text, &mut input);
                                let part = OpenAIResponsesAssistantContentPart::OutputText {
                                    text: t.text.clone(),
                                    annotations: Some(Vec::new()),
                                };
                                // A multi-part message item streams one text
                                // block per content_index, each stamped with
                                // the same `msg_...` id. Fold them back into a
                                // single item — OpenAI rejects duplicate input
                                // item ids.
                                if let Some(OpenAIResponsesInputItem::AssistantMessageItem {
                                    id,
                                    content,
                                    ..
                                }) = input.last_mut()
                                    && *id == item_id
                                {
                                    content.push(part);
                                } else {
                                    input.push(OpenAIResponsesInputItem::AssistantMessageItem {
                                        r#type: "message",
                                        role: "assistant",
                                        id: item_id,
                                        status: "completed",
                                        content: vec![part],
                                    });
                                }
                            } else {
                                pending_text.push_str(&t.text);
                            }
                        }
                        ContentBlock::Thinking(thinking) => {
                            // Replay raw reasoning items (with encrypted_content)
                            // captured from a previous Responses stream only
                            // to its original provider, API and model. Thinking
                            // blocks without a replayable raw item keep the
                            // legacy behavior of being dropped without
                            // flushing text.
                            if same_origin
                                && let Some(raw_item) = replayable_reasoning_item(thinking)
                            {
                                flush_pending(&mut pending_text, &mut input);
                                input.push(OpenAIResponsesInputItem::RawItem(raw_item));
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            flush_pending(&mut pending_text, &mut input);
                            let (call_id, embedded_item_id) = split_responses_tool_call_id(&tc.id);
                            input.push(OpenAIResponsesInputItem::FunctionCall {
                                r#type: "function_call",
                                id: if same_origin {
                                    replayable_function_call_item_id(tc, embedded_item_id)
                                } else {
                                    None
                                },
                                call_id: call_id.to_string(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            });
                        }
                        _ => {}
                    }
                }

                flush_pending(&mut pending_text, &mut input);
            }
            Message::ToolResult(result) => {
                let (call_id, _) = split_responses_tool_call_id(&result.tool_call_id);
                input.push(OpenAIResponsesInputItem::FunctionCallOutput {
                    r#type: "function_call_output",
                    call_id: call_id.to_string(),
                    output: convert_tool_result_output(&result.content),
                });
            }
        }
    }

    input
}

fn convert_tool_result_output(content: &[ContentBlock]) -> OpenAIResponsesFunctionOutput {
    if content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        let parts = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(OpenAIResponsesUserContentPart::InputText {
                    text: text.text.clone(),
                }),
                ContentBlock::Image(image) => Some(OpenAIResponsesUserContentPart::InputImage {
                    image_url: format!("data:{};base64,{}", image.mime_type, image.data),
                }),
                ContentBlock::Media(media) => Some(OpenAIResponsesUserContentPart::InputText {
                    text: media.placeholder(),
                }),
                _ => None,
            })
            .collect();
        return OpenAIResponsesFunctionOutput::Content(parts);
    }

    // Preserve the existing text-only wire representation, including empty
    // blocks and their separators. Audio/video get the same explicit omission
    // explanation as user attachments instead of silently disappearing.
    let mut output = String::new();
    for (index, block) in content.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        match block {
            ContentBlock::Text(text) => output.push_str(&text.text),
            ContentBlock::Media(media) => output.push_str(&media.placeholder()),
            _ => {}
        }
    }
    OpenAIResponsesFunctionOutput::Text(output)
}

/// Maximum item-id length OpenAI accepts on replayed input items.
const MAX_RESPONSES_ITEM_ID_LEN: usize = 64;

/// Split a pi tool-call id into `(call_id, embedded_item_id)`.
///
/// Legacy pi-rust sessions store the bare `call_...` id (no `|`), so the split
/// is the identity and requests stay byte-identical. Sessions written by TS
/// pi-mono store `"<call_id>|<item_id>"`; the suffix is the original `fc_...`
/// output-item id.
fn split_responses_tool_call_id(id: &str) -> (&str, Option<&str>) {
    id.split_once('|')
        .map_or((id, None), |(call_id, item_id)| (call_id, Some(item_id)))
}

/// Original `fc_...` output-item id to replay on a `function_call` input item.
///
/// Prefers the id captured on parse (stored in `thought_signature`), falling
/// back to a TS-session embedded id. Only ids that look like Responses
/// function-call item ids (`fc_` prefix, within OpenAI's 64-char limit) are
/// replayed; anything else (e.g. a Gemini thought signature imported from a
/// TS session) is ignored so the request keeps the legacy shape.
fn replayable_function_call_item_id(
    tool_call: &ToolCall,
    embedded_item_id: Option<&str>,
) -> Option<String> {
    tool_call
        .thought_signature
        .as_deref()
        .or(embedded_item_id)
        .filter(|id| id.starts_with("fc_") && id.len() <= MAX_RESPONSES_ITEM_ID_LEN)
        .map(ToString::to_string)
}

/// Original `msg_...` output-item id to replay on an assistant `message` item.
///
/// Present only when the text block carries a `text_signature` captured from a
/// previous Responses stream. Foreign or oversized signatures fall back to the
/// legacy signature-less shape.
fn replayable_message_item_id(text: &TextContent) -> Option<String> {
    text.text_signature
        .as_deref()
        .filter(|id| id.starts_with("msg_") && id.len() <= MAX_RESPONSES_ITEM_ID_LEN)
        .map(ToString::to_string)
}

/// Raw `reasoning` output item to replay verbatim, parsed from the
/// `thinking_signature` captured on stream parse.
///
/// The signature must be a JSON object with `"type": "reasoning"`; opaque
/// signatures from other providers (Anthropic base64 blobs, etc.) fail the
/// parse or the tag check and are skipped, preserving legacy behavior.
fn replayable_reasoning_item(thinking: &ThinkingContent) -> Option<serde_json::Value> {
    let signature = thinking.thinking_signature.as_deref()?;
    let value: serde_json::Value = serde_json::from_str(signature).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) == Some("reasoning") {
        Some(value)
    } else {
        None
    }
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
                    // No `input_video`/`input_audio` part on this transport;
                    // degrade to the text placeholder (gh #212).
                    ContentBlock::Media(media) => {
                        parts.push(OpenAIResponsesUserContentPart::InputText {
                            text: media.placeholder(),
                        });
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
    OutputItemAdded {
        item: OpenAIResponsesOutputItem,
        #[serde(default)]
        output_index: Option<u32>,
    },
    /// Raw item payload kept as JSON so `reasoning` items (encrypted content,
    /// ids, status) can be preserved verbatim; typed interpretation happens in
    /// `process_event` via [`OpenAIResponsesOutputItemDone`].
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item: serde_json::Value,
        #[serde(default)]
        output_index: Option<u32>,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        delta: String,
        #[serde(default)]
        output_index: Option<u32>,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        item_id: String,
        #[serde(default)]
        arguments: String,
        #[serde(default)]
        output_index: Option<u32>,
    },
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
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<OpenAIResponsesReasoningSummaryPart>,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(rename = "message")]
    Message { id: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesReasoningSummaryPart {
    #[serde(default)]
    text: String,
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

    /// bd-1q31s: a `before_provider_request` rewrite must survive validated
    /// re-ingest — objects keeping `model` + `input` pass (with `stream: true`
    /// re-imposed and extension-added fields preserved), everything else is
    /// rejected so the provider falls back to its original body.
    #[test]
    fn test_validate_rewritten_responses_request() {
        let valid = serde_json::json!({
            "model": "gpt-5.2-codex",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "stream": false,
            "custom_extension_field": {"kept": true},
        });
        let accepted = validate_rewritten_responses_request(valid).expect("valid rewrite accepted");
        assert_eq!(
            accepted["stream"],
            serde_json::Value::Bool(true),
            "stream: true must be re-imposed on accepted rewrites"
        );
        assert_eq!(accepted["custom_extension_field"]["kept"], true);

        let missing_stream = serde_json::json!({
            "model": "gpt-5.2-codex",
            "input": [],
        });
        let accepted =
            validate_rewritten_responses_request(missing_stream).expect("missing stream accepted");
        assert_eq!(accepted["stream"], serde_json::Value::Bool(true));

        for (label, invalid) in [
            ("non-object", serde_json::json!("payload")),
            ("missing model", serde_json::json!({"input": []})),
            ("empty model", serde_json::json!({"model": "", "input": []})),
            (
                "missing input",
                serde_json::json!({"model": "gpt-5.2-codex"}),
            ),
            (
                "non-array input",
                serde_json::json!({"model": "gpt-5.2-codex", "input": {}}),
            ),
        ] {
            assert!(
                validate_rewritten_responses_request(invalid).is_err(),
                "rewrite must be rejected: {label}"
            );
        }
    }

    #[test]
    fn test_build_request_prompt_cache_key_serialize_and_omit() {
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );

        // Set → serialized verbatim, on both the plain Responses path and the
        // codex path (TS parity: both providers send prompt_cache_key,
        // gh #188).
        let options = StreamOptions {
            prompt_cache_key: Some("sess-123".to_string()),
            ..Default::default()
        };
        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("gpt-5.2").with_codex_mode(codex_mode);
            let value = serde_json::to_value(provider.build_request(&context, &options))
                .expect("serialize request");
            assert_eq!(
                value["prompt_cache_key"], "sess-123",
                "codex_mode={codex_mode}"
            );

            // Unset → field absent, wire format byte-identical to before.
            let value =
                serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                    .expect("serialize request");
            assert!(
                value.get("prompt_cache_key").is_none(),
                "codex_mode={codex_mode}"
            );
        }
    }

    #[test]
    fn test_build_request_service_tier_for_openai_and_codex_only() {
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let fast = StreamOptions {
            service_tier: Some("priority".to_string()),
            ..Default::default()
        };
        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("gpt-5.2").with_codex_mode(codex_mode);
            let value = serde_json::to_value(provider.build_request(&context, &fast))
                .expect("serialize request");
            assert_eq!(value["service_tier"], "priority", "codex_mode={codex_mode}");
            let value =
                serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                    .expect("serialize request");
            assert!(
                value.get("service_tier").is_none(),
                "codex_mode={codex_mode}"
            );
        }
        // A third-party Responses backend never sees the field.
        let provider = OpenAIResponsesProvider::new("m").with_provider_name("my-proxy");
        let value = serde_json::to_value(provider.build_request(&context, &fast))
            .expect("serialize request");
        assert!(value.get("service_tier").is_none());
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
        assert!(
            value.get("reasoning").is_none(),
            "non-reasoning model must not send a reasoning field"
        );
        assert!(value.get("include").is_none());
    }

    #[test]
    fn build_request_repairs_itemless_arrays_only_for_codex() {
        let parameters = json!({
            "type":"object",
            "properties":{
                "listeners":{"type":"array"},
                "row":{"type":"array","prefixItems":[{"type":"string"}]},
                "example":{"const":{"type":"array"}}
            },
            "required":["listeners"]
        });
        let context = Context::owned(
            None,
            vec![],
            vec![ToolDef {
                name: "yield_result".to_string(),
                description: "Return structured results.".to_string(),
                parameters: parameters.clone(),
            }],
        );
        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("gpt-test").with_codex_mode(codex_mode);
            let body =
                serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                    .expect("serialize request");
            let tool = &body["tools"][0];
            if codex_mode {
                assert_eq!(tool["parameters"]["properties"]["listeners"]["items"], true);
                assert_eq!(tool["parameters"]["properties"]["row"]["items"], true);
                assert_eq!(tool["strict"], false);
            } else {
                assert_eq!(tool["parameters"], parameters);
                assert!(tool.get("strict").is_none());
            }
            assert_eq!(
                tool["parameters"]["properties"]["example"],
                parameters["properties"]["example"]
            );
            assert_eq!(context.tools[0].parameters, parameters);
        }
    }

    #[test]
    fn codex_request_keeps_unevaluated_item_constraints_unchanged() {
        let parameters = json!({
            "type":"object",
            "properties":{
                "row":{"type":"array","prefixItems":[{"type":"string"}],"unevaluatedItems":false},
                "other":{"type":"array"}
            }
        });
        let context = Context::owned(
            None,
            vec![],
            vec![ToolDef {
                name: "tuple".to_string(),
                description: "Return a tuple.".to_string(),
                parameters: parameters.clone(),
            }],
        );
        let provider = OpenAIResponsesProvider::new("gpt-test").with_codex_mode(true);
        let body =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .expect("serialize request");
        assert_eq!(body["tools"][0]["parameters"], parameters);
        assert!(body["tools"][0].get("strict").is_none());
    }

    fn reasoning_test_context() -> Context<'static> {
        Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            vec![],
        )
    }

    /// gh #165: a non-codex reasoning model with a thinking level set emits
    /// `reasoning: { effort, summary }` plus the encrypted-content include,
    /// mirroring the TS `openai-responses` builder.
    #[test]
    fn test_build_request_reasoning_model_emits_effort() {
        let provider = OpenAIResponsesProvider::new("custom-reasoner").with_reasoning(true);
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::High),
            ..Default::default()
        };

        let request = provider.build_request(&reasoning_test_context(), &options);
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["reasoning"]["effort"], "high");
        assert_eq!(value["reasoning"]["summary"], "auto");
        assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
        // Codex-only fields must stay absent on the plain responses route.
        assert!(value.get("tool_choice").is_none());
        assert!(value.get("parallel_tool_calls").is_none());
        assert!(value.get("text").is_none());
        // Non-codex requests keep sending max_output_tokens.
        assert_eq!(value["max_output_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    }

    /// gh #165: a catalog `thinkingLevelMap` re-maps pi's thinking level onto
    /// the provider's effort vocabulary for non-codex requests.
    #[test]
    fn test_build_request_thinking_level_map_overrides_effort() {
        let compat = CompatConfig {
            thinking_level_map: Some(HashMap::from([
                ("xhigh".to_string(), Some("max".to_string())),
                ("high".to_string(), Some("high".to_string())),
            ])),
            ..CompatConfig::default()
        };
        let provider = OpenAIResponsesProvider::new("custom-reasoner")
            .with_reasoning(true)
            .with_compat(Some(compat));
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::XHigh),
            ..Default::default()
        };

        let request = provider.build_request(&reasoning_test_context(), &options);
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["reasoning"]["effort"], "max");

        // Unmapped levels fall back to the level name itself.
        let medium = provider.build_request(
            &reasoning_test_context(),
            &StreamOptions {
                thinking_level: Some(crate::model::ThinkingLevel::Medium),
                ..Default::default()
            },
        );
        let medium = serde_json::to_value(&medium).expect("serialize request");
        assert_eq!(medium["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_build_request_nullable_effort_does_not_use_default() {
        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("freeflow-responses")
                .with_reasoning(true)
                .with_codex_mode(codex_mode)
                .with_compat(Some(CompatConfig {
                    thinking_level_map: Some(HashMap::from([("max".to_string(), None)])),
                    ..CompatConfig::default()
                }));
            let body = provider.build_request(
                &reasoning_test_context(),
                &StreamOptions {
                    thinking_level: Some(crate::model::ThinkingLevel::Max),
                    ..Default::default()
                },
            );
            let body = serde_json::to_value(body).expect("request JSON");
            assert!(body.get("reasoning").is_none(), "{body}");
        }
    }

    /// gh #165: no reasoning field when the model is non-reasoning, when the
    /// thinking level is `off`, or when no thinking level is requested.
    #[test]
    fn test_build_request_omits_reasoning_when_disabled_or_off() {
        let cases = [
            (false, Some(crate::model::ThinkingLevel::High)),
            (true, Some(crate::model::ThinkingLevel::Off)),
            (true, None),
        ];
        for (reasoning, thinking_level) in cases {
            let provider = OpenAIResponsesProvider::new("custom-model").with_reasoning(reasoning);
            let options = StreamOptions {
                thinking_level,
                ..Default::default()
            };
            let request = provider.build_request(&reasoning_test_context(), &options);
            let value = serde_json::to_value(&request).expect("serialize request");
            assert!(
                value.get("reasoning").is_none(),
                "reasoning={reasoning} thinking={thinking_level:?} must omit reasoning"
            );
            assert!(
                value.get("include").is_none(),
                "reasoning={reasoning} thinking={thinking_level:?} must omit include"
            );
        }
    }

    /// Codex mode keeps its existing defaults (effort falls back to "high")
    /// while honoring a catalog `thinkingLevelMap` override.
    #[test]
    fn test_build_request_codex_mode_effort_defaults_and_map() {
        let provider = OpenAIResponsesProvider::new("gpt-5.2-codex").with_codex_mode(true);
        let request = provider.build_request(&reasoning_test_context(), &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["reasoning"]["effort"], "high");
        assert_eq!(value["reasoning"]["summary"], "auto");
        assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(value["tool_choice"], "auto");

        let mapped = OpenAIResponsesProvider::new("gpt-5.2-codex")
            .with_codex_mode(true)
            .with_compat(Some(CompatConfig {
                thinking_level_map: Some(HashMap::from([(
                    "xhigh".to_string(),
                    Some("max".to_string()),
                )])),
                ..CompatConfig::default()
            }));
        let request = mapped.build_request(
            &reasoning_test_context(),
            &StreamOptions {
                thinking_level: Some(crate::model::ThinkingLevel::XHigh),
                ..Default::default()
            },
        );
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["reasoning"]["effort"], "max");
    }

    // ========================================================================
    // Raw-item preservation and replay (gh #167)
    // ========================================================================

    fn assistant_message_with_blocks(content: Vec<ContentBlock>) -> Message {
        Message::Assistant(std::sync::Arc::new(AssistantMessage {
            content,
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            model: "gpt-test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }))
    }

    fn tool_result_message(tool_call_id: &str, text: &str) -> Message {
        Message::ToolResult(std::sync::Arc::new(crate::model::ToolResultMessage {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "echo".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
            timestamp: 0,
        }))
    }

    fn multimodal_tool_result(id: &str, content: Vec<ContentBlock>, is_error: bool) -> Message {
        Message::tool_result(crate::model::ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "read".to_string(),
            content,
            details: None,
            is_error,
            timestamp: 0,
        })
    }

    fn tool_image(data: &str, mime_type: &str) -> ContentBlock {
        ContentBlock::Image(crate::model::ImageContent {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
        })
    }

    #[test]
    fn tool_output_images_keep_order_error_content_and_parallel_call_ids() {
        let calls = ["call_first|fc_first", "call_second"]
            .into_iter()
            .map(|id| {
                ContentBlock::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: "read".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })
            })
            .collect();
        let context = Context::owned(
            None,
            vec![
                assistant_message_with_blocks(calls),
                multimodal_tool_result(
                    "call_first|fc_first",
                    vec![tool_image("Zmlyc3Q=", "image/png")],
                    false,
                ),
                multimodal_tool_result(
                    "call_second",
                    vec![
                        ContentBlock::Text(TextContent::new("Capture partly failed")),
                        tool_image("c2Vjb25k", "image/jpeg"),
                        ContentBlock::Media(crate::model::MediaContent {
                            data: "YWJj".to_string(),
                            mime_type: "video/mp4".to_string(),
                            name: Some("clip.mp4".to_string()),
                        }),
                        ContentBlock::Text(TextContent::new("Inspect the remaining frame")),
                        tool_image("dGhpcmQ=", "image/webp"),
                    ],
                    true,
                ),
            ],
            Vec::new(),
        );
        let original = serde_json::to_value(&context.messages).unwrap();

        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("gpt-test").with_codex_mode(codex_mode);
            let request = provider.build_request(&context, &StreamOptions::default());
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(
                value["input"],
                json!([
                    {"type":"function_call", "id":"fc_first", "call_id":"call_first", "name":"read", "arguments":"{}"},
                    {"type":"function_call", "call_id":"call_second", "name":"read", "arguments":"{}"},
                    {"type":"function_call_output", "call_id":"call_first", "output":[
                        {"type":"input_image", "image_url":"data:image/png;base64,Zmlyc3Q="}
                    ]},
                    {"type":"function_call_output", "call_id":"call_second", "output":[
                        {"type":"input_text", "text":"Capture partly failed"},
                        {"type":"input_image", "image_url":"data:image/jpeg;base64,c2Vjb25k"},
                        {"type":"input_text", "text":"[media omitted: clip.mp4, video/mp4, 3 B]"},
                        {"type":"input_text", "text":"Inspect the remaining frame"},
                        {"type":"input_image", "image_url":"data:image/webp;base64,dGhpcmQ="}
                    ]}
                ]),
                "codex_mode={codex_mode}",
            );
        }
        assert_eq!(serde_json::to_value(&context.messages).unwrap(), original);
        assert!(matches!(&context.messages[2], Message::ToolResult(result) if result.is_error));
    }

    #[test]
    fn tool_output_text_stays_a_string_and_media_has_an_omission_explanation() {
        let context = Context::owned(
            None,
            vec![
                multimodal_tool_result("empty", Vec::new(), false),
                multimodal_tool_result(
                    "text",
                    vec![
                        ContentBlock::Text(TextContent::new("first\n")),
                        ContentBlock::Text(TextContent::new("")),
                        ContentBlock::Text(TextContent::new("last")),
                    ],
                    true,
                ),
                multimodal_tool_result(
                    "media",
                    vec![ContentBlock::Media(crate::model::MediaContent {
                        data: "YWJj".to_string(),
                        mime_type: "audio/wav".to_string(),
                        name: None,
                    })],
                    false,
                ),
            ],
            Vec::new(),
        );
        for codex_mode in [false, true] {
            let provider = OpenAIResponsesProvider::new("gpt-test").with_codex_mode(codex_mode);
            let request = provider.build_request(&context, &StreamOptions::default());
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(
                value["input"],
                json!([
                    {"type":"function_call_output", "call_id":"empty", "output":""},
                    {"type":"function_call_output", "call_id":"text", "output":"first\n\n\nlast"},
                    {"type":"function_call_output", "call_id":"media", "output":"[media omitted: unnamed, audio/wav, 3 B]"}
                ]),
                "codex_mode={codex_mode}",
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn native_image_read_reaches_responses_and_codex_under_agent_image_policy() {
        use crate::tools::Tool as _;
        use std::sync::Arc;

        // A real PNG goes through the native read tool before its persisted
        // result is replayed through Agent and the HTTP request converter.
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEklEQVR4nGP4z8DAAMIM/4EAAB/uBfsL2WiLAAAAAElFTkSuQmCC";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("frame.png"),
            base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap(),
        )
        .unwrap();
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let read = crate::tools::ReadTool::with_settings(dir.path(), false, false);
        let output = runtime
            .block_on(read.execute("call_image", json!({"path":"frame.png"}), None))
            .unwrap();
        assert!(!output.is_error);
        let original_content = serde_json::to_value(&output.content).unwrap();

        for codex_mode in [false, true] {
            for (block_images, model_accepts_images, omission) in [
                (false, true, None),
                (true, true, Some("Image reading is disabled.")),
                (
                    false,
                    false,
                    Some("[Image omitted: the active model does not support image input.]"),
                ),
            ] {
                let (base_url, rx) =
                    spawn_test_server(200, "text/event-stream", &success_sse_body());
                let provider = OpenAIResponsesProvider::new("gpt-test")
                    .with_codex_mode(codex_mode)
                    .with_base_url(base_url);
                let mut agent = crate::agent::Agent::new(
                    Arc::new(provider),
                    crate::tools::ToolRegistry::from_tools(Vec::new()),
                    crate::agent::AgentConfig {
                        block_images,
                        model_accepts_images,
                        stream_options: StreamOptions {
                            api_key: Some(build_test_jwt("acct_images")),
                            ..Default::default()
                        },
                        ..crate::agent::AgentConfig::default()
                    },
                );
                agent.replace_messages(vec![
                    assistant_message_with_blocks(vec![ContentBlock::ToolCall(ToolCall {
                        id: "call_image".to_string(),
                        name: "read".to_string(),
                        arguments: json!({"path":"frame.png"}),
                        thought_signature: None,
                    })]),
                    multimodal_tool_result("call_image", output.content.clone(), false),
                ]);
                let message = runtime
                    .block_on(agent.run("Describe the frame", |_| {}))
                    .unwrap();
                assert_eq!(message.stop_reason, StopReason::Stop);
                let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let body: Value = serde_json::from_str(&captured.body).unwrap();
                let result = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .expect("original tool output remains attributed");
                assert_eq!(result["call_id"], "call_image");
                if let Some(omission) = omission {
                    assert!(result["output"].as_str().unwrap().contains(omission));
                    assert!(
                        !captured.body.contains(PNG),
                        "filtered image bytes cannot be sent",
                    );
                } else {
                    assert_eq!(
                        result["output"],
                        json!([
                            {"type":"input_text", "text":"Read image file [image/png]"},
                            {"type":"input_image", "image_url":format!("data:image/png;base64,{PNG}")}
                        ]),
                    );
                }
                let Message::ToolResult(stored) = &agent.messages()[1] else {
                    panic!("native tool result must remain in history");
                };
                assert_eq!(
                    serde_json::to_value(&stored.content).unwrap(),
                    original_content,
                );
            }
        }
    }

    fn raw_reasoning_item() -> Value {
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{ "type": "summary_text", "text": "Plan the echo." }],
            "encrypted_content": "gAAAAA-encrypted",
            "status": "completed"
        })
    }

    /// End-to-end round trip: a stream carrying reasoning + message +
    /// function_call items is captured on the assistant message, and the next
    /// request replays the preserved items with their original ids,
    /// `encrypted_content`, and status fields (TS parity).
    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_stream_captures_raw_items_and_build_request_replays_them() {
        let events = vec![
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "rs_1",
                "summary_index": 0,
                "delta": "Plan the echo."
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": raw_reasoning_item()
            }),
            json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "content_index": 0,
                "delta": "Calling echo."
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [
                        { "type": "output_text", "text": "Calling echo.", "annotations": [] }
                    ]
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 2,
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
                "delta": "{\"text\":\"hi\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 2,
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
                    "usage": { "input_tokens": 1, "output_tokens": 2, "total_tokens": 3 }
                }
            }),
        ];

        let out = collect_events(&events);
        let done_message = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("done message");

        // Parse-side capture on the assistant message.
        let ContentBlock::Thinking(thinking) = &done_message.content[0] else {
            panic!("expected thinking block first");
        };
        assert_eq!(thinking.thinking, "Plan the echo.");
        let signature_value: Value = serde_json::from_str(
            thinking
                .thinking_signature
                .as_deref()
                .expect("thinking signature"),
        )
        .expect("signature parses as JSON");
        assert_eq!(signature_value, raw_reasoning_item());

        let ContentBlock::Text(text) = &done_message.content[1] else {
            panic!("expected text block second");
        };
        assert_eq!(text.text_signature.as_deref(), Some("msg_1"));

        let ContentBlock::ToolCall(tool_call) = &done_message.content[2] else {
            panic!("expected tool call block third");
        };
        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.thought_signature.as_deref(), Some("fc_1"));

        // The captured message survives a session JSONL round trip.
        let serialized = serde_json::to_string(&Message::Assistant(done_message.clone().into()))
            .expect("serialize");
        let Message::Assistant(roundtripped) =
            serde_json::from_str::<Message>(&serialized).expect("deserialize")
        else {
            panic!("expected assistant message");
        };

        // Request-side replay.
        let provider = OpenAIResponsesProvider::new("gpt-test");
        let context = Context::owned(
            None,
            vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Ping".to_string()),
                    timestamp: 0,
                }),
                Message::Assistant(roundtripped),
                tool_result_message("call_1", "ok"),
            ],
            vec![],
        );
        let request = provider.build_request(&context, &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(
            value["input"],
            json!([
                { "role": "user", "content": [{ "type": "input_text", "text": "Ping" }] },
                raw_reasoning_item(),
                {
                    "type": "message",
                    "role": "assistant",
                    "id": "msg_1",
                    "status": "completed",
                    "content": [
                        { "type": "output_text", "text": "Calling echo.", "annotations": [] }
                    ]
                },
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "echo",
                    "arguments": "{\"text\":\"hi\"}"
                },
                { "type": "function_call_output", "call_id": "call_1", "output": "ok" }
            ])
        );
    }

    /// A reasoning item that produced no summary deltas but carries
    /// `encrypted_content` still yields a thinking block holding the raw item,
    /// so the encrypted reasoning is not lost.
    #[test]
    fn test_stream_creates_thinking_block_for_summaryless_encrypted_reasoning() {
        let raw_item = json!({
            "type": "reasoning",
            "id": "rs_2",
            "summary": [],
            "encrypted_content": "gAAAAA-opaque"
        });
        let events = vec![
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": raw_item
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ThinkingStart { content_index: 0 }))
        );
        let done_message = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("done message");
        let ContentBlock::Thinking(thinking) = &done_message.content[0] else {
            panic!("expected thinking block");
        };
        assert!(thinking.thinking.is_empty());
        let signature_value: Value = serde_json::from_str(
            thinking
                .thinking_signature
                .as_deref()
                .expect("thinking signature"),
        )
        .expect("signature parses");
        assert_eq!(
            signature_value,
            json!({
                "type": "reasoning",
                "id": "rs_2",
                "summary": [],
                "encrypted_content": "gAAAAA-opaque"
            })
        );
    }

    /// A reasoning item with neither prior blocks nor `encrypted_content` has
    /// nothing replayable and keeps the legacy behavior of being ignored.
    #[test]
    fn test_stream_ignores_reasoning_done_without_encrypted_content() {
        let events = vec![
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": { "type": "reasoning", "id": "rs_3", "summary": [] }
            }),
            json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "content_index": 0,
                "delta": "hi"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            }),
        ];

        let out = collect_events(&events);
        let done_message = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("done message");
        assert_eq!(done_message.content.len(), 1);
        assert!(matches!(&done_message.content[0], ContentBlock::Text(_)));
    }

    /// Legacy sessions (no captured metadata) must build byte-identical
    /// requests to the pre-preservation implementation: merged assistant text
    /// with no id/status/annotations, function_call without an `id` field, and
    /// untouched call ids.
    #[test]
    fn test_build_request_without_raw_metadata_keeps_legacy_shape() {
        let provider = OpenAIResponsesProvider::new("gpt-test");
        let context = Context::owned(
            None,
            vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Ping".to_string()),
                    timestamp: 0,
                }),
                assistant_message_with_blocks(vec![
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: "legacy thinking".to_string(),
                        thinking_signature: None,
                    }),
                    ContentBlock::Text(TextContent::new("Part one. ")),
                    ContentBlock::Text(TextContent::new("Part two.")),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_legacy".to_string(),
                        name: "echo".to_string(),
                        arguments: json!({ "text": "hi" }),
                        thought_signature: None,
                    }),
                ]),
                tool_result_message("call_legacy", "ok"),
            ],
            vec![],
        );

        let request = provider.build_request(&context, &StreamOptions::default());
        let serialized = serde_json::to_string(&request).expect("serialize request");
        let value: Value = serde_json::from_str(&serialized).expect("parse request");
        assert_eq!(
            value["input"],
            json!([
                { "role": "user", "content": [{ "type": "input_text", "text": "Ping" }] },
                {
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Part one. Part two." }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_legacy",
                    "name": "echo",
                    "arguments": "{\"text\":\"hi\"}"
                },
                { "type": "function_call_output", "call_id": "call_legacy", "output": "ok" }
            ])
        );
        // Explicit absence checks for the new optional fields.
        assert!(!serialized.contains("\"annotations\""));
        assert!(!serialized.contains("\"status\""));
        assert!(!serialized.contains("\"id\""));
    }

    /// Combined `call_id|item_id` tool-call ids written by TS pi-mono sessions
    /// are split on replay: call_id keeps the prefix, and the embedded
    /// `fc_...` suffix is replayed as the function_call item id.
    #[test]
    fn test_build_request_splits_ts_session_combined_tool_call_ids() {
        let provider = OpenAIResponsesProvider::new("gpt-test");
        let context = Context::owned(
            None,
            vec![
                assistant_message_with_blocks(vec![ContentBlock::ToolCall(ToolCall {
                    id: "call_9|fc_9".to_string(),
                    name: "echo".to_string(),
                    arguments: json!({ "text": "hi" }),
                    thought_signature: None,
                })]),
                tool_result_message("call_9|fc_9", "ok"),
            ],
            vec![],
        );

        let request = provider.build_request(&context, &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(
            value["input"],
            json!([
                {
                    "type": "function_call",
                    "id": "fc_9",
                    "call_id": "call_9",
                    "name": "echo",
                    "arguments": "{\"text\":\"hi\"}"
                },
                { "type": "function_call_output", "call_id": "call_9", "output": "ok" }
            ])
        );
    }

    /// Foreign signatures (other providers' opaque blobs) never leak into
    /// Responses requests: only JSON reasoning items, `msg_`-prefixed text
    /// signatures, and `fc_`-prefixed item ids are replayed.
    #[test]
    fn test_build_request_ignores_foreign_signatures() {
        let provider = OpenAIResponsesProvider::new("gpt-test");
        let context = Context::owned(
            None,
            vec![assistant_message_with_blocks(vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "anthropic thinking".to_string(),
                    thinking_signature: Some("EqQBCkgIBBAB".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "Hello".to_string(),
                    text_signature: Some("sig123".to_string()),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_a".to_string(),
                    name: "echo".to_string(),
                    arguments: json!({}),
                    thought_signature: Some("gemini-opaque-blob".to_string()),
                }),
            ])],
            vec![],
        );

        let request = provider.build_request(&context, &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(
            value["input"],
            json!([
                {
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Hello" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "echo",
                    "arguments": "{}"
                }
            ])
        );
    }

    /// Malformed `reasoning`/`message` done items (and non-object items) are
    /// ignored, exactly as the pre-capture implementation ignored every
    /// non-`function_call` done item via `#[serde(other)]`. The stream must
    /// not abort on an unparseable opportunistic-capture payload.
    #[test]
    fn test_stream_tolerates_malformed_reasoning_and_message_done_items() {
        let events = vec![
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": { "type": "message" } // missing id
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": { "type": "reasoning", "id": 123 } // non-string id
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": "not-an-object"
            }),
            json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "content_index": 0,
                "delta": "still streaming"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "incomplete_details": null,
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            }),
        ];

        let out = collect_events(&events);
        let done_message = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("done message");
        assert_eq!(done_message.content.len(), 1);
        let ContentBlock::Text(text) = &done_message.content[0] else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "still streaming");
        assert!(text.text_signature.is_none());
    }

    /// A malformed `function_call` done item keeps the legacy hard error: the
    /// pre-capture code failed the whole-chunk parse for these, and silently
    /// dropping a tool call would corrupt the conversation.
    #[test]
    fn test_stream_errors_on_malformed_function_call_done_item() {
        let empty = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
        let event_source = crate::sse::SseStream::new(Box::pin(empty));
        let mut state = StreamState::new(
            event_source,
            "gpt-test".to_string(),
            "openai-responses".to_string(),
            "openai".to_string(),
        );
        let data = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "function_call", "id": "fc_1" } // missing call_id/name
        })
        .to_string();
        let err = state
            .process_event(&data)
            .expect_err("malformed function_call done item must error");
        assert!(err.to_string().contains("JSON parse error"));
    }

    /// Text blocks stamped with the same `msg_...` id (a multi-part message
    /// item) fold back into a single replayed `message` item — OpenAI rejects
    /// duplicate input item ids. Distinct ids stay distinct items.
    #[test]
    fn test_build_request_merges_multipart_message_item() {
        let provider = OpenAIResponsesProvider::new("gpt-test");
        let context = Context::owned(
            None,
            vec![assistant_message_with_blocks(vec![
                ContentBlock::Text(TextContent {
                    text: "Part one. ".to_string(),
                    text_signature: Some("msg_1".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "Part two.".to_string(),
                    text_signature: Some("msg_1".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "New item.".to_string(),
                    text_signature: Some("msg_2".to_string()),
                }),
            ])],
            vec![],
        );

        let request = provider.build_request(&context, &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(
            value["input"],
            json!([
                {
                    "type": "message",
                    "role": "assistant",
                    "id": "msg_1",
                    "status": "completed",
                    "content": [
                        { "type": "output_text", "text": "Part one. ", "annotations": [] },
                        { "type": "output_text", "text": "Part two.", "annotations": [] }
                    ]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "id": "msg_2",
                    "status": "completed",
                    "content": [
                        { "type": "output_text", "text": "New item.", "annotations": [] }
                    ]
                }
            ])
        );
    }

    /// Foreign origins must not replay raw reasoning or output-item IDs,
    /// including IDs embedded in tool-call correlation strings. Ordinary
    /// assistant text, tool arguments and call/result pairing survive.
    #[test]
    fn test_build_request_omits_raw_item_replay_for_foreign_origin() {
        let provider = OpenAIResponsesProvider::new("gpt-test");
        for (source_api, source_provider, source_model) in [
            ("openai-responses", "openai", "gpt-other"),
            ("openai-responses", "other-provider", "gpt-test"),
            ("other-api", "openai", "gpt-test"),
            ("", "", ""),
        ] {
            let foreign = Message::Assistant(std::sync::Arc::new(AssistantMessage {
                content: vec![
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: "Plan the echo.".to_string(),
                        thinking_signature: Some(raw_reasoning_item().to_string()),
                    }),
                    ContentBlock::Text(TextContent {
                        text: "Calling echo.".to_string(),
                        text_signature: Some("msg_1".to_string()),
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        arguments: json!({ "text": "hi" }),
                        thought_signature: Some("fc_1".to_string()),
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_2|fc_2".to_string(),
                        name: "echo".to_string(),
                        arguments: json!({ "text": "again" }),
                        thought_signature: None,
                    }),
                ],
                api: source_api.to_string(),
                provider: source_provider.to_string(),
                model: source_model.to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }));
            let context = Context::owned(
                None,
                vec![
                    foreign,
                    tool_result_message("call_1", "ok"),
                    tool_result_message("call_2|fc_2", "again ok"),
                ],
                vec![],
            );

            let request = provider.build_request(&context, &StreamOptions::default());
            let value = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(
                value["input"],
                json!([
                    {
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Calling echo." }]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "echo",
                        "arguments": "{\"text\":\"hi\"}"
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_2",
                        "name": "echo",
                        "arguments": "{\"text\":\"again\"}"
                    },
                    { "type": "function_call_output", "call_id": "call_1", "output": "ok" },
                    { "type": "function_call_output", "call_id": "call_2", "output": "again ok" }
                ]),
                "origin: {source_provider}/{source_api}/{source_model}"
            );
        }
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

    fn tool_item_event(kind: &str, id: &str, name: &str, arguments: &str) -> Value {
        json!({
            "type": kind,
            "item": {
                "type": "function_call",
                "id": format!("fc_{id}"),
                "call_id": format!("call_{id}"),
                "name": name,
                "arguments": arguments
            }
        })
    }

    fn completed_response_event() -> Value {
        json!({
            "type": "response.completed",
            "response": {
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        })
    }

    #[allow(clippy::too_many_lines)]
    fn conflicting_tool_identity_events(arguments: &str) -> Vec<(&'static str, Vec<Value>)> {
        let added = tool_item_event("response.output_item.added", "identity", "write", arguments);
        let done = tool_item_event("response.output_item.done", "identity", "write", arguments);
        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_identity", "delta": " "
        });
        let arguments_done = json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_identity", "arguments": arguments
        });
        let mut changed_call = done.clone();
        changed_call["item"]["call_id"] = json!("private-rewritten-call");
        let mut changed_name = done.clone();
        changed_name["item"]["name"] = json!("private-rewritten-name");
        let mut changed_item = done.clone();
        changed_item["item"]["id"] = json!("private-rewritten-item");
        let mut indexed_added = added.clone();
        indexed_added["output_index"] = json!(0);
        let mut indexed_other =
            tool_item_event("response.output_item.done", "other", "write", arguments);
        indexed_other["output_index"] = json!(0);
        let mut moved_done = done.clone();
        moved_done["output_index"] = json!(1);
        let mut moved_arguments = arguments_done.clone();
        moved_arguments["output_index"] = json!(1);
        let mut empty_item = done.clone();
        empty_item["item"]["id"] = json!("");
        let mut empty_call = done.clone();
        empty_call["item"]["call_id"] = json!(" ");
        let mut empty_name = done.clone();
        empty_name["item"]["name"] = json!("\t");
        vec![
            ("duplicate added", vec![added.clone(), added.clone()]),
            ("duplicate done", vec![done.clone(), done.clone()]),
            ("added after done", vec![done.clone(), added.clone()]),
            ("changed call", vec![added.clone(), changed_call]),
            ("changed name", vec![added.clone(), changed_name]),
            ("changed item", vec![added.clone(), changed_item.clone()]),
            ("completed call reused", vec![done.clone(), changed_item]),
            (
                "output slot reused",
                vec![indexed_added.clone(), indexed_other],
            ),
            (
                "item moved at completion",
                vec![indexed_added.clone(), moved_done],
            ),
            (
                "item moved at arguments done",
                vec![indexed_added.clone(), moved_arguments],
            ),
            (
                "slot reused by orphan delta",
                vec![
                    indexed_added,
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": "fc_other", "output_index": 0, "delta": "{}"
                    }),
                ],
            ),
            (
                "item became a message",
                vec![
                    added.clone(),
                    json!({
                        "type":"response.output_item.done", "item":{"type":"message", "id":"fc_identity"}
                    }),
                ],
            ),
            (
                "item became an unknown kind",
                vec![
                    added.clone(),
                    json!({
                        "type":"response.output_item.done", "item":{"id":"fc_identity"}
                    }),
                ],
            ),
            (
                "tool slot became reasoning",
                vec![
                    json!({
                        "type":"response.output_item.added", "output_index":0, "item":added["item"]
                    }),
                    json!({
                        "type":"response.output_item.done", "output_index":0,
                        "item":{"type":"reasoning", "id":"rs_other"}
                    }),
                ],
            ),
            (
                "non-function added reused a tool slot",
                vec![
                    json!({
                        "type":"response.output_item.added", "output_index":0, "item":added["item"]
                    }),
                    json!({
                        "type":"response.output_item.added", "output_index":0,
                        "item":{"type":"message", "id":"msg_other"}
                    }),
                ],
            ),
            ("delta after item done", vec![done.clone(), delta.clone()]),
            (
                "arguments after item done",
                vec![done, arguments_done.clone()],
            ),
            (
                "duplicate arguments done",
                vec![
                    added.clone(),
                    arguments_done.clone(),
                    arguments_done.clone(),
                ],
            ),
            (
                "delta after arguments done",
                vec![added, arguments_done, delta],
            ),
            ("empty item", vec![empty_item]),
            ("empty call", vec![empty_call]),
            ("empty name", vec![empty_name]),
        ]
    }

    #[test]
    fn contradictory_tool_identities_end_the_response_once_without_rewriting_calls() {
        for (case, mut events) in conflicting_tool_identity_events("{}") {
            events.push(tool_item_event(
                "response.output_item.done",
                "after_failure",
                "write",
                "{}",
            ));
            events.push(completed_response_event());
            let out = collect_events(&events);
            assert_eq!(
                out.iter()
                    .filter(|event| matches!(event, StreamEvent::Error { .. }))
                    .count(),
                1,
                "{case}"
            );
            assert!(
                !out.iter()
                    .any(|event| matches!(event, StreamEvent::Done { .. })),
                "{case}"
            );
            assert!(
                !out.iter().any(|event| matches!(
                    event, StreamEvent::ToolCallStart { id, .. } if id == "call_after_failure"
                )),
                "{case}"
            );
            let Some(StreamEvent::Error { reason, error }) = out.last() else {
                panic!("{case}: missing terminal error");
            };
            assert_eq!(*reason, StopReason::Error);
            assert_eq!(error.stop_reason, StopReason::Error);
            let detail = error.error_message.as_deref().unwrap();
            assert!(
                detail.contains("Invalid OpenAI Responses tool identity"),
                "{case}"
            );
            assert!(!detail.contains("private-rewritten"), "{case}");
            assert!(!detail.contains("fc_identity"), "{case}");
            assert!(!detail.contains("call_identity"), "{case}");
        }
    }

    #[test]
    fn parallel_tool_lifecycles_preserve_optional_fields_and_authoritative_snapshots() {
        let mut done_without_arguments =
            tool_item_event("response.output_item.done", "first", "write", "");
        done_without_arguments["item"]
            .as_object_mut()
            .unwrap()
            .remove("arguments");
        let events = [
            json!({"type":"response.function_call_arguments.delta", "item_id":"fc_first", "delta":"{\"old\":"}),
            tool_item_event("response.output_item.added", "second", "write", "{}"),
            tool_item_event("response.output_item.added", "first", "write", ""),
            json!({"type":"response.function_call_arguments.done", "item_id":"fc_first", "output_index":0, "arguments":"{\"final\":true}"}),
            tool_item_event("response.output_item.done", "second", "write", "{}"),
            done_without_arguments,
            tool_item_event("response.output_item.done", "done_only", "write", ""),
            completed_response_event(),
        ];
        let out = collect_events(&events);
        assert!(
            !out.iter()
                .any(|event| matches!(event, StreamEvent::Error { .. }))
        );
        let Some(StreamEvent::Done {
            reason: StopReason::ToolUse,
            message,
        }) = out.last()
        else {
            panic!("valid parallel calls must complete");
        };
        let calls = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].id, "call_second");
        assert_eq!(calls[1].id, "call_first");
        assert_eq!(calls[1].arguments, json!({"final":true}));
        assert_eq!(calls[1].thought_signature.as_deref(), Some("fc_first"));
        assert_eq!(calls[2].id, "call_done_only");
        assert_eq!(calls[2].arguments, json!({}));
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                .count(),
            3
        );

        for output_done in [false, true] {
            let mut events = vec![
                tool_item_event("response.output_item.added", "snapshot", "write", "{}"),
                json!({"type":"response.function_call_arguments.done", "item_id":"fc_snapshot", "arguments":"{\"final\":1}"}),
            ];
            if output_done {
                events.push(tool_item_event(
                    "response.output_item.done",
                    "snapshot",
                    "write",
                    r#"{"final":2}"#,
                ));
            }
            events.push(completed_response_event());
            let out = collect_events(&events);
            let expected = if output_done { 2 } else { 1 };
            assert!(out.iter().any(|event| matches!(
                event, StreamEvent::ToolCallEnd { tool_call, .. }
                    if tool_call.arguments == json!({"final": expected})
            )));
            assert!(
                !out.iter()
                    .any(|event| matches!(event, StreamEvent::Error { .. }))
            );
        }
    }

    #[test]
    fn argument_done_before_added_preserves_snapshots_and_empty_fallback() {
        for final_arguments in [None, Some(""), Some(r#"{"snapshot":true}"#)] {
            let mut arguments_done = json!({
                "type":"response.function_call_arguments.done", "item_id":"fc_ordered"
            });
            if let Some(arguments) = final_arguments {
                arguments_done["arguments"] = json!(arguments);
            }
            let out = collect_events(&[
                json!({"type":"response.function_call_arguments.delta", "item_id":"fc_ordered", "delta":"1}"}),
                arguments_done,
                tool_item_event("response.output_item.added", "ordered", "write", r#"{"a":"#),
                tool_item_event("response.output_item.done", "ordered", "write", ""),
                completed_response_event(),
            ]);
            let expected = if final_arguments.is_some_and(|arguments| !arguments.is_empty()) {
                json!({"snapshot":true})
            } else {
                json!({"a":1})
            };
            assert!(out.iter().any(|event| matches!(
                event, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.arguments == expected
            )));
            assert!(
                !out.iter()
                    .any(|event| matches!(event, StreamEvent::Error { .. }))
            );
        }
    }

    #[test]
    fn tool_identity_limits_include_orphans_and_completed_items() {
        let mut events = (0..MAX_STREAMED_TOOL_ITEMS)
            .map(|index| {
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": format!("fc_{index}"), "delta": ""
                })
            })
            .collect::<Vec<_>>();
        // Completing an orphan neither charges the same item twice nor frees
        // its identity for reuse before the actual end of the wire stream.
        events.push(tool_item_event(
            "response.output_item.added",
            "0",
            "write",
            "{}",
        ));
        events.push(tool_item_event(
            "response.output_item.done",
            "0",
            "write",
            "{}",
        ));
        events.push(json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_over_limit", "delta": ""
        }));
        events.push(completed_response_event());
        let out = collect_events(&events);
        let Some(StreamEvent::Error { error, .. }) = out.last() else {
            panic!("distinct orphan identities must be bounded");
        };
        assert!(
            error
                .error_message
                .as_deref()
                .unwrap()
                .contains("count limit")
        );
        assert!(out.iter().any(|event| matches!(
            event, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.id == "call_0"
        )));

        let oversized_item = "x".repeat(MAX_STREAMED_TOOL_IDENTITY_BYTES + 1);
        let almost_full_item = "x".repeat(MAX_STREAMED_TOOL_IDENTITY_BYTES - 1);
        for mut events in [
            vec![json!({
                "type": "response.function_call_arguments.delta",
                "item_id": oversized_item, "delta": ""
            })],
            vec![
                json!({"type":"response.function_call_arguments.delta", "item_id":almost_full_item, "delta":""}),
                json!({"type":"response.output_item.added", "item":{
                    "type":"function_call", "id":almost_full_item,
                    "call_id":"call_private", "name":"write", "arguments":"{}"
                }}),
            ],
        ] {
            events.push(completed_response_event());
            let out = collect_events(&events);
            let Some(StreamEvent::Error { error, .. }) = out.last() else {
                panic!("identity bytes must be bounded before retaining function fields");
            };
            assert!(
                error
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("byte limit")
            );
            assert!(!out.iter().any(|event| matches!(
                event,
                StreamEvent::ToolCallStart { .. } | StreamEvent::Done { .. }
            )));
        }
    }

    #[test]
    fn orphan_arguments_fail_closed_without_replacing_provider_failure_details() {
        let orphan = json!({
            "type":"response.function_call_arguments.done", "item_id":"fc_orphan",
            "arguments":"{}"
        });
        let out = collect_events(&[orphan.clone(), completed_response_event()]);
        assert!(matches!(out.last(), Some(StreamEvent::Error { error, .. })
            if error.error_message.as_deref().unwrap().contains("without a function-call identity")));
        for failure in [
            json!({"type":"response.failed", "response":{"error":{"message":"original provider failure"}}}),
            json!({"type":"error", "message":"original provider failure"}),
        ] {
            let out = collect_events(&[orphan.clone(), completed_response_event(), failure]);
            let Some(StreamEvent::Error { reason, error }) = out.last() else {
                panic!("provider failure must stay terminal");
            };
            assert_eq!(*reason, StopReason::Error);
            assert_eq!(
                error.error_message.as_deref(),
                Some("original provider failure")
            );
            assert_eq!(
                out.iter()
                    .filter(|event| matches!(event, StreamEvent::Error { .. }))
                    .count(),
                1
            );
        }
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn contradictory_streamed_identities_never_execute_a_valid_sibling() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("identity-must-not-be-written.txt");
        let arguments = json!({"path":marker, "content":"must not execute"}).to_string();
        for (case, conflict) in conflicting_tool_identity_events(&arguments) {
            let mut events = vec![tool_item_event(
                "response.output_item.done",
                "valid_peer",
                "write",
                &arguments,
            )];
            events.extend(conflict);
            events.push(completed_response_event());
            let body = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let (base_url, _requests) = spawn_test_server(200, "text/event-stream", &body);
            let provider = OpenAIResponsesProvider::new("gpt-test").with_base_url(base_url);
            let tools = crate::tools::ToolRegistry::from_tools(vec![Box::new(
                crate::tools::WriteTool::new(dir.path()),
            )]);
            let mut agent = crate::agent::Agent::new(
                Arc::new(provider),
                tools,
                crate::agent::AgentConfig {
                    max_tool_iterations: 1,
                    stream_options: StreamOptions {
                        api_key: Some("test-key".to_string()),
                        ..Default::default()
                    },
                    ..crate::agent::AgentConfig::default()
                },
            );
            let executions = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&executions);
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            let message = runtime
                .block_on(agent.run("Apply both edits", move |event| {
                    if matches!(event, crate::agent::AgentEvent::ToolExecutionStart { .. }) {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                }))
                .unwrap();
            assert_eq!(message.stop_reason, StopReason::Error, "{case}");
            assert!(
                message
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("Invalid OpenAI Responses tool identity"),
                "{case}"
            );
            assert_eq!(executions.load(Ordering::SeqCst), 0, "{case}");
            assert!(!marker.exists(), "{case}: a peer must not execute");
            assert!(
                !agent
                    .messages()
                    .iter()
                    .any(|message| matches!(message, Message::ToolResult(_))),
                "{case}"
            );
        }
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn valid_parallel_response_calls_execute_once_and_replay_matching_results() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for codex in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let first = dir.path().join("first.txt");
            let second = dir.path().join("second.txt");
            let first_args = json!({"path":first, "content":"one"}).to_string();
            let second_args = json!({"path":second, "content":"two"}).to_string();
            let events = [
                json!({"type":"response.function_call_arguments.delta", "item_id":"fc_first", "delta":"{"}),
                tool_item_event("response.output_item.added", "first", "write", ""),
                tool_item_event("response.output_item.done", "second", "write", &second_args),
                json!({"type":"response.function_call_arguments.done", "item_id":"fc_first", "arguments":first_args}),
                tool_item_event("response.output_item.done", "first", "write", ""),
                completed_response_event(),
            ];
            let content_type = if codex {
                "application/x-ndjson"
            } else {
                "text/event-stream"
            };
            let body = if codex {
                ndjson_body_from_events(&events)
            } else {
                events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>()
            };
            let follow_up = if codex {
                ndjson_body_from_events(&[completed_response_event()])
            } else {
                success_sse_body()
            };
            let (base_url, requests) =
                spawn_test_server_with_bodies(200, content_type, vec![body, follow_up]);
            let provider = OpenAIResponsesProvider::new("gpt-test")
                .with_codex_mode(codex)
                .with_base_url(base_url);
            let tools = crate::tools::ToolRegistry::from_tools(vec![Box::new(
                crate::tools::WriteTool::new(dir.path()),
            )]);
            let mut agent = crate::agent::Agent::new(
                Arc::new(provider),
                tools,
                crate::agent::AgentConfig {
                    max_tool_iterations: 1,
                    stream_options: StreamOptions {
                        api_key: Some(if codex {
                            build_test_jwt("acct-test")
                        } else {
                            "test-key".to_string()
                        }),
                        ..Default::default()
                    },
                    ..crate::agent::AgentConfig::default()
                },
            );
            let executions = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&executions);
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            let result = runtime
                .block_on(agent.run("Apply both edits", move |event| {
                    if matches!(event, crate::agent::AgentEvent::ToolExecutionStart { .. }) {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                }))
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(executions.load(Ordering::SeqCst), 2);
            assert_eq!(std::fs::read_to_string(first).unwrap(), "one");
            assert_eq!(std::fs::read_to_string(second).unwrap(), "two");
            requests.recv_timeout(Duration::from_secs(2)).unwrap();
            let follow_up = requests.recv_timeout(Duration::from_secs(2)).unwrap();
            let body: Value = serde_json::from_str(&follow_up.body).unwrap();
            let mut result_ids = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .map(|item| item["call_id"].as_str().unwrap())
                .collect::<Vec<_>>();
            result_ids.sort_unstable();
            assert_eq!(result_ids, ["call_first", "call_second"]);
        }
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn fatal_tool_payload_parse_error_cannot_resume_after_repoll() {
        for event_type in ["response.output_item.added", "response.output_item.done"] {
            let events = [
                tool_item_event("response.output_item.added", "before_error", "write", "{}"),
                json!({
                    "type": event_type,
                    "item": {
                        "type": "function_call",
                        "id": "fc_malformed",
                        "name": "write",
                        "arguments": "{}"
                    }
                }),
                tool_item_event("response.output_item.done", "after_error", "write", "{}"),
                completed_response_event(),
            ];
            let body = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let (base_url, _requests) = spawn_test_server(200, "text/event-stream", &body);
            let provider = OpenAIResponsesProvider::new("gpt-test").with_base_url(base_url);
            let context = Context::owned(
                None,
                vec![Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Apply the edit".to_string()),
                    timestamp: 0,
                })],
                Vec::new(),
            );
            let options = StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            };
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let mut stream = provider.stream(&context, &options).await.unwrap();
                let mut saw_error = false;
                while let Some(event) = stream.next().await {
                    match event {
                        Err(error) => {
                            assert!(error.to_string().contains("JSON parse error"));
                            saw_error = true;
                            break;
                        }
                        Ok(StreamEvent::Done { .. } | StreamEvent::ToolCallEnd { .. }) => {
                            panic!("{event_type}: malformed payload cannot complete a call");
                        }
                        Ok(_) => {}
                    }
                }
                assert!(
                    saw_error,
                    "{event_type}: expected the known-item parse error"
                );
                assert!(
                    stream.next().await.is_none(),
                    "{event_type}: resumed after error"
                );
            });
        }
    }

    #[test]
    fn malformed_completed_tool_arguments_cannot_become_executable_calls() {
        for raw in [
            r#"{"command":"sensitive-argument""#,
            r#"{"command":"sensitive-argument"} trailing"#,
            "null",
            "false",
            "42",
            "[]",
            r#""sensitive-argument""#,
        ] {
            // The same strict boundary applies to done-only items, accumulated
            // deltas, authoritative final snapshots and terminal fallback.
            for route in 0..4 {
                let mut events = Vec::new();
                if route != 0 {
                    events.push(tool_item_event(
                        "response.output_item.added",
                        "bad",
                        "write",
                        "",
                    ));
                    events.push(json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": "fc_bad",
                        "delta": if route == 2 { "{}" } else { raw }
                    }));
                }
                if route != 3 {
                    events.push(tool_item_event(
                        "response.output_item.done",
                        "bad",
                        "write",
                        if route == 1 { "" } else { raw },
                    ));
                }
                events.push(completed_response_event());
                let out = collect_events(&events);
                assert_eq!(
                    out.iter()
                        .filter(|event| matches!(event, StreamEvent::Error { .. }))
                        .count(),
                    1,
                    "one error terminal for route {route}, raw {raw:?}",
                );
                assert!(!out.iter().any(|event| matches!(
                    event,
                    StreamEvent::ToolCallEnd { .. } | StreamEvent::Done { .. }
                )));
                let Some(StreamEvent::Error { reason, error }) = out.last() else {
                    panic!("expected terminal error");
                };
                assert_eq!(*reason, StopReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                let detail = error.error_message.as_deref().unwrap();
                assert!(detail.contains("Invalid OpenAI Responses tool arguments"));
                assert!(!detail.contains("sensitive-argument"));
                assert!(matches!(
                    error.content.last(),
                    Some(ContentBlock::ToolCall(call)) if call.arguments.is_null()
                ));
            }
        }
    }

    #[test]
    fn completed_tool_arguments_accept_objects_and_complete_final_snapshots() {
        for raw in ["", " \n\t ", "{}", r#"{"items":[1,{"ok":true}],"n":null}"#] {
            let out = collect_events(&[
                tool_item_event("response.output_item.done", "valid", "echo", raw),
                completed_response_event(),
            ]);
            let calls = out
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 1);
            let expected = if raw.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(raw).unwrap()
            };
            assert_eq!(calls[0].arguments, expected);
            assert!(matches!(
                out.last(),
                Some(StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    ..
                })
            ));
        }

        let out = collect_events(&[
            tool_item_event(
                "response.output_item.added",
                "snapshot",
                "echo",
                "{\"text\":",
            ),
            tool_item_event(
                "response.output_item.done",
                "snapshot",
                "echo",
                r#"{"text":"complete final snapshot"}"#,
            ),
            completed_response_event(),
        ]);
        assert!(out.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallEnd { tool_call, .. }
                if tool_call.arguments == json!({"text": "complete final snapshot"})
        )));
        assert!(
            !out.iter()
                .any(|event| matches!(event, StreamEvent::Error { .. }))
        );
    }

    #[test]
    fn fallback_finalization_discards_buffered_tool_ends_when_one_call_is_invalid() {
        let out = collect_events(&[
            tool_item_event("response.output_item.added", "first", "write", "{}"),
            tool_item_event("response.output_item.added", "bad", "write", "[1]"),
            tool_item_event("response.output_item.added", "last", "write", "{}"),
            completed_response_event(),
        ]);
        assert!(!out.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallEnd { .. } | StreamEvent::Done { .. }
        )));
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, StreamEvent::Error { .. }))
                .count(),
            1,
        );
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn invalid_tool_completion_is_terminal_on_sse_and_ndjson_transports() {
        let events = [
            tool_item_event("response.output_item.done", "bad", "write", "null"),
            tool_item_event("response.output_item.done", "later", "write", "{}"),
            completed_response_event(),
        ];
        for ndjson in [false, true] {
            let body = if ndjson {
                ndjson_body_from_events(&events)
            } else {
                events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>()
                    + "data: [DONE]\n\n"
            };
            let content_type = if ndjson {
                "application/x-ndjson"
            } else {
                "text/event-stream"
            };
            let (base_url, _requests) = spawn_test_server(200, content_type, &body);
            let provider = OpenAIResponsesProvider::new("gpt-test").with_base_url(base_url);
            let context = Context::owned(None, Vec::new(), Vec::new());
            let options = StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            };
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let mut stream = provider.stream(&context, &options).await.unwrap();
                let mut errors = 0;
                while let Some(event) = stream.next().await {
                    match event.unwrap() {
                        StreamEvent::Error { reason, .. } => {
                            assert_eq!(reason, StopReason::Error);
                            errors += 1;
                        }
                        StreamEvent::ToolCallStart { id, .. } => assert_eq!(id, "call_bad"),
                        StreamEvent::ToolCallEnd { .. } | StreamEvent::Done { .. } => {
                            panic!("failed tool completion must not authorize execution");
                        }
                        _ => {}
                    }
                }
                assert_eq!(errors, 1);
                assert!(stream.next().await.is_none(), "terminal stream stays fused");
            });
        }
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn malformed_parallel_tool_batch_never_executes_the_valid_sibling() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-be-written.txt");
        let events = [
            tool_item_event(
                "response.output_item.done",
                "valid",
                "write",
                &json!({"path": marker, "content": "must not execute"}).to_string(),
            ),
            tool_item_event("response.output_item.done", "bad", "write", "null"),
            completed_response_event(),
        ];
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let (base_url, _requests) = spawn_test_server(200, "text/event-stream", &body);
        let provider = OpenAIResponsesProvider::new("gpt-test").with_base_url(base_url);
        let tools = crate::tools::ToolRegistry::from_tools(vec![Box::new(
            crate::tools::WriteTool::new(dir.path()),
        )]);
        let mut agent = crate::agent::Agent::new(
            Arc::new(provider),
            tools,
            crate::agent::AgentConfig {
                max_tool_iterations: 1,
                stream_options: StreamOptions {
                    api_key: Some("test-key".to_string()),
                    ..Default::default()
                },
                ..crate::agent::AgentConfig::default()
            },
        );
        let executions = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&executions);
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let message = runtime
            .block_on(agent.run("Read the files", move |event| {
                if matches!(event, crate::agent::AgentEvent::ToolExecutionStart { .. }) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }))
            .unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert!(
            !marker.exists(),
            "valid sibling of a malformed batch must not run"
        );
        assert!(
            !agent
                .messages()
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)))
        );
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

    /// gh #167: the compact endpoint is derived only from a `…/responses`
    /// base URL; anything else refuses rather than guessing a route.
    #[test]
    fn derive_compact_url_requires_responses_suffix() {
        assert_eq!(
            derive_compact_url("https://api.openai.com/v1/responses"),
            Some("https://api.openai.com/v1/responses/compact".to_string())
        );
        assert_eq!(
            derive_compact_url("http://localhost:8080/v1/responses/"),
            Some("http://localhost:8080/v1/responses/compact".to_string())
        );
        assert_eq!(derive_compact_url("https://api.openai.com/v1"), None);
        assert_eq!(
            derive_compact_url("https://chatgpt.com/backend-api/codex"),
            None
        );
    }

    /// gh #167: `compact_native` POSTs the sanitized body verbatim to the
    /// sibling compact endpoint with the session's Bearer auth, and returns
    /// the raw response JSON.
    #[test]
    fn compact_native_posts_body_with_session_auth() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let response_body = r#"{"id":"resp_1","created_at":123,"output":[{"type":"message","role":"user","content":[{"type":"input_text","text":"window"}]}]}"#;
            let (base_url, rx) = spawn_test_server(200, "application/json", response_body);
            let provider = OpenAIResponsesProvider::new("gpt-test")
                .with_base_url(format!("{base_url}/v1/responses"));
            let options = StreamOptions {
                api_key: Some("sk-compact-test".to_string()),
                ..Default::default()
            };
            let body = serde_json::json!({
                "model": "gpt-test",
                "instructions": "compact this",
                "input": [{ "type": "message", "role": "user", "content": [] }],
            });

            let result = provider
                .compact_native(&body, &options)
                .await
                .expect("compact POST succeeds");
            assert_eq!(result["id"], "resp_1");
            assert_eq!(result["output"][0]["content"][0]["text"], "window");

            let captured = rx.recv().expect("captured request");
            assert_eq!(
                captured.headers.get("authorization").map(String::as_str),
                Some("Bearer sk-compact-test")
            );
            assert_eq!(
                captured.headers.get("accept").map(String::as_str),
                Some("application/json")
            );
            let sent: Value = serde_json::from_str(&captured.body).expect("body json");
            assert_eq!(sent, body, "body must be sent verbatim");
        });
    }

    /// gh #167: an underivable base URL refuses before any network activity,
    /// and a non-2xx compact response surfaces as a provider error.
    #[test]
    fn compact_native_refuses_bad_base_url_and_surfaces_http_errors() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let provider = OpenAIResponsesProvider::new("gpt-test")
                .with_base_url("http://127.0.0.1:9/custom-route");
            let err = provider
                .compact_native(
                    &serde_json::json!({ "model": "gpt-test", "input": [] }),
                    &StreamOptions {
                        api_key: Some("sk-x".to_string()),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("underivable base URL must refuse");
            assert!(
                err.to_string().contains("cannot derive"),
                "unexpected error: {err}"
            );

            let (base_url, _rx) = spawn_test_server(500, "application/json", r#"{"error":"boom"}"#);
            let provider = OpenAIResponsesProvider::new("gpt-test")
                .with_base_url(format!("{base_url}/v1/responses"));
            let err = provider
                .compact_native(
                    &serde_json::json!({ "model": "gpt-test", "input": [] }),
                    &StreamOptions {
                        api_key: Some("sk-x".to_string()),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("HTTP 500 must surface");
            let message = err.to_string();
            assert!(message.contains("HTTP 500"), "unexpected error: {message}");
            assert!(message.contains("boom"), "unexpected error: {message}");
        });
    }

    #[test]
    fn auth_header_false_sends_custom_responses_request_without_authorization() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIResponsesProvider::new("custom-model")
            .with_provider_name("custom-openai-responses")
            .with_auth_header(false)
            .with_base_url(base_url);
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
                .expect("keyless stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let captured = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured keyless custom request");
        assert!(
            !captured.headers.contains_key("authorization"),
            "authHeader:false must not synthesize Authorization"
        );
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
        spawn_test_server_with_bodies(status_code, content_type, vec![body.to_string()])
    }

    fn spawn_test_server_with_bodies(
        status_code: u16,
        content_type: &str,
        bodies: Vec<String>,
    ) -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = mpsc::channel();
        let content_type = content_type.to_string();

        let serve = move |body: String| {
            let (mut socket, _) = listener.accept().expect("accept");
            // 250ms is the POLLING interval; the deadline below is the budget.
            // Treating a timed-out read as end-of-request truncated the buffer
            // and the header scan then failed as a malformed request rather
            // than a slow one (bd-eg6ng).
            socket
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("set read timeout");
            let deadline = std::time::Instant::now() + Duration::from_secs(30);

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
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture timed out waiting for the request"
                        );
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
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture timed out waiting for the request"
                        );
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
        };
        std::thread::spawn(move || {
            for body in bodies {
                serve(body);
            }
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
            StopReason::PauseTurn => "pause_turn".to_string(),
            StopReason::Refusal => "refusal".to_string(),
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

    mod stream_lifecycle {
        use super::*;
        use futures::FutureExt as _;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::task::{Wake, Waker};

        #[derive(Clone, Copy, Debug)]
        struct Mode {
            ndjson: bool,
            codex: bool,
        }

        const MODES: [Mode; 4] = [
            Mode {
                ndjson: false,
                codex: false,
            },
            Mode {
                ndjson: false,
                codex: true,
            },
            Mode {
                ndjson: true,
                codex: false,
            },
            Mode {
                ndjson: true,
                codex: true,
            },
        ];

        impl Mode {
            fn frame(self, data: &str) -> Vec<u8> {
                if self.ndjson {
                    format!("{data}\n").into_bytes()
                } else {
                    format!("data: {data}\n\n").into_bytes()
                }
            }

            fn events(self, values: &[Value]) -> Vec<u8> {
                values
                    .iter()
                    .flat_map(|value| self.frame(&value.to_string()))
                    .collect()
            }

            const fn identity(self) -> (&'static str, &'static str, &'static str) {
                if self.codex {
                    ("gpt-codex-test", "openai-codex-responses", "openai-codex")
                } else {
                    ("gpt-responses-test", "openai-responses", "openai")
                }
            }

            fn assert_identity(self, message: &AssistantMessage) {
                let (model, api, provider) = self.identity();
                assert_eq!(message.model, model);
                assert_eq!(message.api, api);
                assert_eq!(message.provider, provider);
            }
        }

        #[derive(Default)]
        struct SourceObservation {
            dropped: AtomicBool,
            polls: AtomicUsize,
        }

        enum Tail {
            Eof,
            Pending,
            Repeat(Vec<u8>),
        }

        struct ObservedSource {
            chunks: VecDeque<std::io::Result<Vec<u8>>>,
            tail: Tail,
            observation: Arc<SourceObservation>,
        }

        impl Stream for ObservedSource {
            type Item = std::io::Result<Vec<u8>>;

            fn poll_next(
                self: Pin<&mut Self>,
                _: &mut TaskContext<'_>,
            ) -> Poll<Option<Self::Item>> {
                let this = self.get_mut();
                let polls = this.observation.polls.fetch_add(1, Ordering::SeqCst) + 1;
                if let Some(chunk) = this.chunks.pop_front() {
                    return Poll::Ready(Some(chunk));
                }
                match &this.tail {
                    Tail::Eof => Poll::Ready(None),
                    Tail::Pending => Poll::Pending,
                    Tail::Repeat(chunk) => {
                        // A regressed driver fails the test instead of hanging
                        // forever while reading an always-ready source.
                        assert!(
                            polls <= MAX_READY_EVENTS_PER_YIELD * 4,
                            "ready source never yielded"
                        );
                        Poll::Ready(Some(Ok(chunk.clone())))
                    }
                }
            }
        }

        impl Drop for ObservedSource {
            fn drop(&mut self) {
                self.observation.dropped.store(true, Ordering::SeqCst);
            }
        }

        fn observed_stream(
            mode: Mode,
            chunks: Vec<std::io::Result<Vec<u8>>>,
            tail: Tail,
        ) -> (EventStream, Arc<SourceObservation>) {
            let observation = Arc::new(SourceObservation::default());
            let source = ObservedSource {
                chunks: chunks.into(),
                tail,
                observation: Arc::clone(&observation),
            };
            let (model, api, provider) = mode.identity();
            (
                wire_stream(
                    source,
                    mode.ndjson,
                    model.into(),
                    api.into(),
                    provider.into(),
                ),
                observation,
            )
        }

        fn assert_retired(output: &mut EventStream, observation: &SourceObservation) {
            assert!(
                observation.dropped.load(Ordering::SeqCst),
                "source survived terminal delivery"
            );
            let polls = observation.polls.load(Ordering::SeqCst);
            for _ in 0..3 {
                assert!(
                    matches!(output.next().now_or_never(), Some(None)),
                    "terminal stream did not stay at EOF"
                );
            }
            assert_eq!(
                observation.polls.load(Ordering::SeqCst),
                polls,
                "retired source was polled again"
            );
        }

        #[derive(Default)]
        struct WakeFlag(AtomicBool);

        impl Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        fn completion(input: u64, output: u64) -> Value {
            json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": input, "output_tokens": output, "total_tokens": input + output}}
            })
        }

        fn text_delta(text: &str) -> Value {
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": text})
        }

        fn tool_done(arguments: &str) -> Value {
            json!({
                "type": "response.output_item.done",
                "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "echo", "arguments": arguments}
            })
        }

        async fn terminal_events(output: &mut EventStream) -> Vec<StreamEvent> {
            let mut events = Vec::new();
            for _ in 0..32 {
                let event = output
                    .next()
                    .await
                    .expect("terminal event missing")
                    .expect("wire event");
                let terminal =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                events.push(event);
                if terminal {
                    return events;
                }
            }
            panic!("terminal event missing after 32 events");
        }

        #[test]
        fn terminal_releases_source_after_late_items_usage_and_block_endings() {
            for mode in MODES {
                for sentinel in [false, true] {
                    let mut body = mode.events(&[
                        json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": 0, "delta": "Plan"}),
                        text_delta("Ready"),
                        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"text\":\"late\"}"}),
                        completion(1, 1),
                        json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "summary": [{"text": "Plan"}], "encrypted_content": "opaque-signature"}}),
                        json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1"}}),
                        tool_done(""),
                        completion(17, 9),
                    ]);
                    let tail = if sentinel {
                        body.extend(mode.frame("[DONE]"));
                        Tail::Pending
                    } else {
                        Tail::Eof
                    };
                    let (mut output, observation) = observed_stream(mode, vec![Ok(body)], tail);
                    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                    let events = runtime.block_on(terminal_events(&mut output));
                    assert_retired(&mut output, &observation);
                    let Some(StreamEvent::Done { reason, message }) = events.last() else {
                        panic!("expected successful terminal for {mode:?}");
                    };
                    assert_eq!(*reason, StopReason::ToolUse);
                    mode.assert_identity(message);
                    assert_eq!(
                        (
                            message.usage.input,
                            message.usage.output,
                            message.usage.total_tokens
                        ),
                        (17, 9, 26)
                    );
                    assert!(events.iter().any(|event| matches!(event, StreamEvent::TextEnd { content, .. } if content == "Ready")));
                    assert!(events.iter().any(|event| matches!(event, StreamEvent::ThinkingEnd { content, .. } if content == "Plan")));
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                            .count(),
                        1
                    );
                    // ubs:ignore[rust.security.constant-time-compare] Public fixture equality, not authentication.
                    assert!(message.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text == "Ready" && text.text_signature.as_deref() == Some("msg_1"))));
                    let thinking = message
                        .content
                        .iter()
                        .find_map(|block| match block {
                            ContentBlock::Thinking(thinking) => Some(thinking),
                            _ => None,
                        })
                        .expect("thinking block");
                    let signature: Value = serde_json::from_str(
                        thinking
                            .thinking_signature
                            .as_deref()
                            .expect("late signature"),
                    )
                    .expect("signature JSON");
                    assert_eq!(signature["encrypted_content"], "opaque-signature");
                    // ubs:ignore[rust.security.constant-time-compare] Public fixture equality, not authentication.
                    assert!(message.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(call) if call.id == "call_1" && call.arguments == json!({"text": "late"}) && call.thought_signature.as_deref() == Some("fc_1"))));
                }
            }
        }

        #[test]
        fn protocol_errors_release_source_and_never_replay_trailing_tools() {
            for mode in MODES {
                for failure in [
                    json!({"type": "response.failed", "response": {"error": {"message": "provider failed"}}}),
                    json!({"type": "error", "message": "provider failed"}),
                    tool_done("{"),
                ] {
                    let mut body = mode.events(&[text_delta("partial"), completion(7, 3), failure]);
                    body.extend(mode.events(&[tool_done("{}"), completion(99, 99)]));
                    body.extend(mode.frame("[DONE]"));
                    let (mut output, observation) =
                        observed_stream(mode, vec![Ok(body)], Tail::Pending);
                    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                    let events = runtime.block_on(terminal_events(&mut output));
                    assert_retired(&mut output, &observation);
                    let Some(StreamEvent::Error { reason, error }) = events.last() else {
                        panic!("expected failed terminal for {mode:?}");
                    };
                    assert_eq!(*reason, StopReason::Error);
                    mode.assert_identity(error);
                    assert_eq!((error.usage.input, error.usage.output), (7, 3));
                    assert!(error.error_message.is_some());
                    assert!(error.content.iter().any(
                        |block| matches!(block, ContentBlock::Text(text) if text.text == "partial")
                    ));
                    assert!(!events.iter().any(|event| matches!(
                        event,
                        StreamEvent::ToolCallEnd { .. } | StreamEvent::Done { .. }
                    )));
                }
            }
        }

        #[test]
        fn parser_transport_and_eof_failures_retire_before_one_error() {
            for mode in MODES {
                for failure in [
                    "json",
                    "json-coalesced",
                    "known-event",
                    "utf8",
                    "utf8-coalesced",
                    "utf8-at-eof",
                    "transport",
                    "eof",
                    "transport-after-completed",
                ] {
                    let mut chunks = vec![Ok(mode.events(&[text_delta("partial")]))];
                    if failure == "transport-after-completed" {
                        chunks.push(Ok(mode.events(&[completion(7, 3)])));
                    }
                    let expected = match failure {
                        "json" | "json-coalesced" => {
                            chunks.push(Ok(mode.frame("{")));
                            "JSON parse error"
                        }
                        "known-event" => {
                            chunks.push(Ok(mode.frame(r#"{"type":"response.output_text.delta","item_id":"msg_1","content_index":0}"#)));
                            "JSON parse error"
                        }
                        "utf8" | "utf8-coalesced" => {
                            chunks.push(Ok(vec![0xff, b'\n']));
                            "UTF-8"
                        }
                        "utf8-at-eof" => {
                            chunks.push(Ok(vec![0xc3]));
                            "UTF-8"
                        }
                        "transport" | "transport-after-completed" => {
                            chunks.push(Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "connection lost",
                            )));
                            "connection lost"
                        }
                        "eof" => "Stream ended without Done event",
                        _ => unreachable!(),
                    };
                    let tail = if failure == "eof" || failure == "utf8-at-eof" {
                        Tail::Eof
                    } else {
                        chunks.push(Ok(mode.events(&[tool_done("{}"), completion(99, 99)])));
                        Tail::Pending
                    };
                    if failure.ends_with("-coalesced") {
                        let mut body = Vec::new();
                        for chunk in chunks {
                            body.extend(chunk.expect("coalesced wire bytes"));
                        }
                        chunks = vec![Ok(body)];
                    }
                    let (mut output, observation) = observed_stream(mode, chunks, tail);
                    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                    runtime.block_on(async {
                        let mut saw_partial = false;
                        for _ in 0..8 {
                            match output.next().await.expect("expected exactly one error") {
                                Ok(StreamEvent::TextDelta { delta, .. }) => {
                                    saw_partial |= delta == "partial"
                                }
                                Ok(StreamEvent::Start { partial }) => {
                                    mode.assert_identity(&partial)
                                }
                                Ok(StreamEvent::TextStart { .. }) => {}
                                Ok(event) => {
                                    panic!("unexpected event after failure {failure}: {event:?}")
                                }
                                Err(error) => {
                                    assert!(saw_partial);
                                    assert!(
                                        error.to_string().contains(expected),
                                        "{mode:?} {failure}: {error}"
                                    );
                                    assert_retired(&mut output, &observation);
                                    return;
                                }
                            }
                        }
                        panic!("wire failure was not reported");
                    });
                }
            }
        }

        #[test]
        fn ndjson_transient_errors_remain_bounded_and_reset_after_a_valid_frame() {
            let mode = Mode {
                ndjson: true,
                codex: true,
            };
            for recover in [false, true] {
                let mut chunks: Vec<_> = (0..5)
                    .map(|_| Err(std::io::Error::from(std::io::ErrorKind::TimedOut)))
                    .collect();
                if recover {
                    chunks.push(Ok(mode.events(&[completion(7, 3)])));
                    chunks.extend(
                        (0..5).map(|_| Err(std::io::Error::from(std::io::ErrorKind::TimedOut))),
                    );
                    chunks.push(Ok(mode.frame("[DONE]")));
                } else {
                    chunks.push(Err(std::io::Error::from(std::io::ErrorKind::TimedOut)));
                    chunks.push(Ok(mode.events(&[tool_done("{}"), completion(99, 99)])));
                }
                let (mut output, observation) = observed_stream(mode, chunks, Tail::Pending);
                let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                runtime.block_on(async {
                    if recover {
                        let events = terminal_events(&mut output).await;
                        let Some(StreamEvent::Done { message, .. }) = events.last() else {
                            panic!("valid frame did not reset transient allowance");
                        };
                        assert_eq!((message.usage.input, message.usage.output), (7, 3));
                    } else {
                        assert!(output.next().await.expect("fatal sixth error").is_err());
                        assert_eq!(observation.polls.load(Ordering::SeqCst), 6);
                    }
                    assert_retired(&mut output, &observation);
                });
            }
        }

        #[test]
        fn ready_metadata_bursts_yield_wake_and_preserve_late_usage() {
            for mode in MODES {
                let mut body = mode.events(&[completion(1, 1)]);
                for _ in 0..MAX_READY_EVENTS_PER_YIELD * 2 {
                    body.extend(mode.frame(r#"{"type":"response.in_progress"}"#));
                }
                body.extend(mode.events(&[completion(17, 9)]));
                body.extend(mode.frame("[DONE]"));
                let (mut output, observation) =
                    observed_stream(mode, vec![Ok(body)], Tail::Pending);
                assert!(matches!(
                    output.next().now_or_never(),
                    Some(Some(Ok(StreamEvent::Start { .. })))
                ));
                let flag = Arc::new(WakeFlag::default());
                let waker = Waker::from(Arc::clone(&flag));
                let mut task = TaskContext::from_waker(&waker);
                assert!(
                    output.as_mut().poll_next(&mut task).is_pending(),
                    "{mode:?} consumed an entire metadata burst"
                );
                assert!(
                    flag.0.load(Ordering::SeqCst),
                    "cooperative yield did not wake its consumer"
                );
                assert!(!observation.dropped.load(Ordering::SeqCst));
                let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                let events = runtime.block_on(terminal_events(&mut output));
                let Some(StreamEvent::Done { message, .. }) = events.last() else {
                    panic!("missing metadata terminal");
                };
                assert_eq!((message.usage.input, message.usage.output), (17, 9));
                assert_retired(&mut output, &observation);
            }
        }

        #[test]
        fn cancellation_wins_against_an_always_ready_metadata_source() {
            for mode in MODES {
                let mut repeats = vec![mode.frame(r#"{"type":"response.in_progress"}"#)];
                if mode.ndjson {
                    repeats.extend([Vec::new(), b" \t".to_vec(), b" \t\r\n".to_vec()]);
                }
                for repeat in repeats {
                    let (mut output, observation) =
                        observed_stream(mode, Vec::new(), Tail::Repeat(repeat));
                    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
                    runtime.block_on(async {
                        let (abort, signal) = crate::agent::AbortHandle::new();
                        let next = Box::pin(output.next());
                        let cancel = Box::pin(async move {
                            abort.abort();
                            signal.wait().await;
                        });
                        match futures::future::select(next, cancel).await {
                            futures::future::Either::Right(((), pending)) => drop(pending),
                            futures::future::Either::Left(_) => {
                                panic!("ready source starved cancellation")
                            }
                        }
                    });
                    assert!(observation.polls.load(Ordering::SeqCst) <= MAX_READY_EVENTS_PER_YIELD);
                    drop(output);
                    assert!(observation.dropped.load(Ordering::SeqCst));
                }
            }
        }

        #[test]
        fn ndjson_limits_each_line_and_preserves_valid_prefix_before_a_parse_error() {
            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            runtime.block_on(async {
                let line = r#"{"type":"response.in_progress"}"#;
                let count = MAX_READY_EVENTS_PER_YIELD * 2;
                for suffix in [vec![0xff, b'\n'], vec![0xc3], vec![b'x'; line.len() + 1]] {
                    let mut body = format!("{line}\n").repeat(count).into_bytes();
                    body.extend(suffix);
                    let mut parser = NdjsonStream::with_max_event_bytes(
                        stream::iter(vec![Ok(body)]),
                        line.len(),
                    );
                    for _ in 0..count {
                        let event = parser
                            .next()
                            .await
                            .expect("valid prefix line")
                            .expect("per-line limit, not aggregate limit");
                        assert_eq!(event.data, line);
                    }
                    assert!(parser.next().await.expect("invalid suffix").is_err());
                    for _ in 0..3 {
                        assert!(parser.next().await.is_none());
                    }
                }
            });
        }

        #[test]
        fn ndjson_empty_chunks_and_buffered_blank_lines_yield_at_the_wire_boundary() {
            let mode = Mode {
                ndjson: true,
                codex: true,
            };
            for chunk in [Vec::new(), b" \t".to_vec(), b" \t\r\n".to_vec()] {
                let (mut output, observation) =
                    observed_stream(mode, Vec::new(), Tail::Repeat(chunk));
                let flag = Arc::new(WakeFlag::default());
                let waker = Waker::from(Arc::clone(&flag));
                let mut task = TaskContext::from_waker(&waker);
                assert!(output.as_mut().poll_next(&mut task).is_pending());
                assert!(flag.0.load(Ordering::SeqCst));
                assert!(observation.polls.load(Ordering::SeqCst) <= MAX_NDJSON_CHUNKS_PER_POLL);
                drop(output);
                assert!(observation.dropped.load(Ordering::SeqCst));
            }

            let mut body = b" \t\r\n".repeat(MAX_NDJSON_LINES_PER_POLL * 2);
            body.extend(mode.events(&[completion(17, 9)]));
            body.extend(mode.frame("[DONE]"));
            let (mut output, observation) = observed_stream(mode, vec![Ok(body)], Tail::Pending);
            let flag = Arc::new(WakeFlag::default());
            let waker = Waker::from(Arc::clone(&flag));
            let mut task = TaskContext::from_waker(&waker);
            assert!(output.as_mut().poll_next(&mut task).is_pending());
            assert!(flag.0.load(Ordering::SeqCst));
            assert_eq!(
                observation.polls.load(Ordering::SeqCst),
                1,
                "buffered whitespace budget was not enforced"
            );
            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            let events = runtime.block_on(terminal_events(&mut output));
            let Some(StreamEvent::Done { message, .. }) = events.last() else {
                panic!("buffered whitespace discarded the final response");
            };
            assert_eq!((message.usage.input, message.usage.output), (17, 9));
            assert_retired(&mut output, &observation);
        }

        #[test]
        fn ndjson_budget_preserves_split_utf8_crlf_and_unterminated_final_line() {
            let mode = Mode {
                ndjson: true,
                codex: false,
            };
            let line = format!("{}\r\n", text_delta("café ☕")).into_bytes();
            let split = line
                .iter()
                .position(|byte| *byte == 0xc3)
                .expect("UTF-8 lead byte")
                + 1;
            let mut chunks = vec![Ok(line[..split].to_vec())];
            chunks.extend((0..MAX_NDJSON_CHUNKS_PER_POLL).map(|_| Ok(Vec::new())));
            chunks.push(Ok(line[split..].to_vec()));
            chunks.push(Ok(completion(7, 3).to_string().into_bytes()));
            let (mut output, observation) = observed_stream(mode, chunks, Tail::Eof);
            assert!(output.next().now_or_never().is_none());
            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            let events = runtime.block_on(terminal_events(&mut output));
            let Some(StreamEvent::Done { message, .. }) = events.last() else {
                panic!("split UTF-8 response did not complete");
            };
            assert!(
                message.content.iter().any(
                    |block| matches!(block, ContentBlock::Text(text) if text.text == "café ☕")
                )
            );
            assert_eq!((message.usage.input, message.usage.output), (7, 3));
            assert_retired(&mut output, &observation);
        }

        #[test]
        fn dropping_unpolled_or_pending_streams_releases_the_source() {
            for mode in MODES {
                for poll_once in [false, true] {
                    let (mut output, observation) =
                        observed_stream(mode, Vec::new(), Tail::Pending);
                    if poll_once {
                        assert!(output.next().now_or_never().is_none());
                    }
                    assert!(!observation.dropped.load(Ordering::SeqCst));
                    drop(output);
                    assert!(observation.dropped.load(Ordering::SeqCst));
                }
            }
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
