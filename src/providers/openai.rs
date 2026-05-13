//! OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI Chat Completions API,
//! supporting streaming responses and tool use. Compatible with:
//! - OpenAI direct API (api.openai.com)
//! - Azure OpenAI
//! - Any OpenAI-compatible API (Groq, Together, etc.)

use std::borrow::Cow;

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ToolCall, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const OPENROUTER_DEFAULT_HTTP_REFERER: &str = "https://github.com/Dicklesworthstone/pi_agent_rust";
const OPENROUTER_DEFAULT_X_TITLE: &str = "Pi Agent Rust";

/// Map a role string (which may come from compat config at runtime) to a `Cow<'_, str>`.
///
/// The OpenAI API uses a small, well-known set of role names.  When the value
/// matches one of these we return the corresponding string literal (zero
/// allocation).  For an unknown role name (extremely rare – only possible via
/// exotic compat overrides) we return an owned String.
fn to_cow_role(role: &str) -> Cow<'_, str> {
    match role {
        "system" => Cow::Borrowed("system"),
        "developer" => Cow::Borrowed("developer"),
        "user" => Cow::Borrowed("user"),
        "assistant" => Cow::Borrowed("assistant"),
        "tool" => Cow::Borrowed("tool"),
        "function" => Cow::Borrowed("function"),
        other => Cow::Owned(other.to_string()),
    }
}

fn map_has_any_header(headers: &std::collections::HashMap<String, String>, names: &[&str]) -> bool {
    headers
        .keys()
        .any(|key| names.iter().any(|name| key.eq_ignore_ascii_case(name)))
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

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn openrouter_default_http_referer() -> String {
    first_non_empty_env(&["OPENROUTER_HTTP_REFERER", "PI_OPENROUTER_HTTP_REFERER"])
        .unwrap_or_else(|| OPENROUTER_DEFAULT_HTTP_REFERER.to_string())
}

fn openrouter_default_x_title() -> String {
    first_non_empty_env(&["OPENROUTER_X_TITLE", "PI_OPENROUTER_X_TITLE"])
        .unwrap_or_else(|| OPENROUTER_DEFAULT_X_TITLE.to_string())
}

// ============================================================================
// OpenAI Provider
// ============================================================================

/// OpenAI Chat Completions API provider.
pub struct OpenAIProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
    compat: Option<CompatConfig>,
    http_timeout_secs: Option<u64>,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: OPENAI_API_URL.to_string(),
            provider: "openai".to_string(),
            compat: None,
            http_timeout_secs: None,
        }
    }

    /// Override the provider name reported in streamed events.
    ///
    /// This is useful for OpenAI-compatible backends (Groq, Together, etc.) that use this
    /// implementation but should still surface their own provider identifier in session logs.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Create with a custom base URL (for Azure, Groq, etc.).
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
    ///
    /// Overrides are applied during request building (field names, headers,
    /// capability flags) and response parsing (stop-reason mapping).
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Set HTTP request timeout in seconds for this provider.
    #[must_use]
    pub fn with_http_timeout_secs(mut self, timeout_secs: Option<u64>) -> Self {
        self.http_timeout_secs = timeout_secs;
        self
    }

    /// Build the request body for the OpenAI API.
    pub fn build_request<'a>(
        &'a self,
        context: &'a Context<'_>,
        options: &StreamOptions,
    ) -> OpenAIRequest<'a> {
        let system_role = self
            .compat
            .as_ref()
            .and_then(|c| c.system_role_name.as_deref())
            .unwrap_or("system");
        let messages = Self::build_messages_with_role(context, system_role);

        let tools_supported = self
            .compat
            .as_ref()
            .and_then(|c| c.supports_tools)
            .unwrap_or(true);

        let tools: Option<Vec<OpenAITool<'a>>> = if context.tools.is_empty() || !tools_supported {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_openai).collect())
        };

        // Determine which max-tokens field to populate based on compat config.
        let use_alt_field = self
            .compat
            .as_ref()
            .and_then(|c| c.max_tokens_field.as_deref())
            .is_some_and(|f| f == "max_completion_tokens");

        let token_limit = options.max_tokens.or(Some(DEFAULT_MAX_TOKENS));
        let (max_tokens, max_completion_tokens) = if use_alt_field {
            (None, token_limit)
        } else {
            (token_limit, None)
        };

        let include_usage = self
            .compat
            .as_ref()
            .and_then(|c| c.supports_usage_in_streaming)
            .unwrap_or(true);

        let stream_options = Some(OpenAIStreamOptions { include_usage });

        OpenAIRequest {
            model: &self.model,
            messages,
            max_tokens,
            max_completion_tokens,
            temperature: options.temperature,
            tools,
            stream: true,
            stream_options,
        }
    }

    fn build_request_json(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<serde_json::Value> {
        let request = self.build_request(context, options);
        let mut value = serde_json::to_value(request)
            .map_err(|e| Error::api(format!("Failed to serialize OpenAI request: {e}")))?;
        self.apply_openrouter_routing_overrides(&mut value)?;
        Ok(value)
    }

    fn apply_openrouter_routing_overrides(&self, request: &mut serde_json::Value) -> Result<()> {
        if !self.provider.eq_ignore_ascii_case("openrouter") {
            return Ok(());
        }

        let Some(routing) = self
            .compat
            .as_ref()
            .and_then(|compat| compat.open_router_routing.as_ref())
        else {
            return Ok(());
        };

        let Some(request_obj) = request.as_object_mut() else {
            return Err(Error::api(
                "OpenAI request body must serialize to a JSON object",
            ));
        };
        let Some(routing_obj) = routing.as_object() else {
            return Err(Error::config(
                "openRouterRouting must be a JSON object when configured",
            ));
        };

        for (key, value) in routing_obj {
            request_obj.insert(key.clone(), value.clone());
        }
        Ok(())
    }

    /// Build the messages array with system prompt prepended using the given role name.
    fn build_messages_with_role<'a>(
        context: &'a Context<'_>,
        system_role: &'a str,
    ) -> Vec<OpenAIMessage<'a>> {
        let mut messages = Vec::with_capacity(context.messages.len() + 1);

        // Add system prompt as first message
        if let Some(system) = &context.system_prompt {
            messages.push(OpenAIMessage {
                role: to_cow_role(system_role),
                content: Some(OpenAIContent::Text(Cow::Borrowed(system))),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Convert conversation messages
        for message in context.messages.iter() {
            messages.extend(convert_message_to_openai(message));
        }

        messages
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "openai-completions"
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
        let authorization_override = authorization_override(options, self.compat.as_ref());

        let auth_value = if authorization_override.is_some() {
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

        let request_body = self.build_request_json(context, options)?;

        // Note: Content-Type is set by .json() below; setting it here too
        // produces a duplicate header that OpenAI's server rejects.
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream");

        if let Some(auth_value) = auth_value {
            request = request.header("Authorization", format!("Bearer {auth_value}"));
        }

        if self.provider.eq_ignore_ascii_case("openrouter") {
            let compat_headers = self
                .compat
                .as_ref()
                .and_then(|compat| compat.custom_headers.as_ref());
            let has_referer = map_has_any_header(&options.headers, &["http-referer", "referer"])
                || compat_headers.is_some_and(|headers| {
                    map_has_any_header(headers, &["http-referer", "referer"])
                });
            if !has_referer {
                request = request.header("HTTP-Referer", openrouter_default_http_referer());
            }

            let has_title = map_has_any_header(&options.headers, &["x-title"])
                || compat_headers.is_some_and(|headers| map_has_any_header(headers, &["x-title"]));
            if !has_title {
                request = request.header("X-Title", openrouter_default_x_title());
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

        // Apply HTTP timeout from configuration hierarchy
        if let Some(timeout) = crate::http::effective_http_timeout(
            options.http_timeout_secs,
            self.http_timeout_secs,
            None, // Global timeout will be passed from provider factory
        ) {
            request = request.timeout(timeout);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(Error::provider(
                &self.provider,
                format!("OpenAI API error (HTTP {status}): {body}"),
            ));
        }

        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_ascii_lowercase());
        if !content_type
            .as_deref()
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            let message = content_type.map_or_else(
                || {
                    format!(
                        "OpenAI API protocol error (HTTP {status}): missing Content-Type (expected text/event-stream)"
                    )
                },
                |value| {
                    format!(
                        "OpenAI API protocol error (HTTP {status}): unexpected Content-Type {value} (expected text/event-stream)"
                    )
                },
            );
            return Err(Error::api(message));
        }

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                if state.done {
                    return None;
                }
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            // A successful chunk resets the consecutive error counter.
                            state.transient_error_count = 0;
                            // OpenAI sends "[DONE]" as final message
                            if msg.data == "[DONE]" {
                                state.done = true;
                                let reason = state.partial.stop_reason;
                                let message = std::mem::take(&mut state.partial);
                                return Some((Ok(StreamEvent::Done { reason, message }), state));
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                state.done = true;
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
                            state.done = true;
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        // Stream ended without [DONE] sentinel (e.g.
                        // premature server disconnect).  Emit a Done event
                        // so the agent loop receives the accumulated partial
                        // instead of silently losing it.
                        None => {
                            state.done = true;
                            let reason = state.partial.stop_reason;
                            let message = std::mem::take(&mut state.partial);
                            return Some((Ok(StreamEvent::Done { reason, message }), state));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

// ============================================================================
// Stream State
// ============================================================================

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    tool_calls: Vec<ToolCallState>,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    done: bool,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
}

struct ToolCallState {
    index: usize,
    content_index: usize,
    id: String,
    name: String,
    arguments: String,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    fn new(event_source: SseStream<S>, model: String, api: String, provider: String) -> Self {
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
            tool_calls: Vec::new(),
            pending_events: VecDeque::new(),
            started: false,
            done: false,
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

    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: OpenAIStreamChunk = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        // Handle usage in final chunk
        if let Some(usage) = chunk.usage {
            self.partial.usage.input = usage.prompt_tokens;
            self.partial.usage.output = usage.completion_tokens.unwrap_or(0);
            self.partial.usage.total_tokens = usage.total_tokens;
            if let Some(details) = usage.prompt_tokens_details {
                self.partial.usage.cache_read = details.cached_tokens.unwrap_or(0);
            }
        }

        if let Some(error) = chunk.error {
            self.partial.stop_reason = StopReason::Error;
            if let Some(message) = error.message {
                let message = message.trim();
                if !message.is_empty() {
                    self.partial.error_message = Some(message.to_string());
                }
            }
        }

        // Process choices
        if let Some(choice) = chunk.choices.into_iter().next() {
            if !self.started
                && choice.finish_reason.is_none()
                && choice.delta.content.is_none()
                && choice.delta.reasoning_content.is_none()
                && choice.delta.tool_calls.is_none()
            {
                self.ensure_started();
                return Ok(());
            }

            self.process_choice(choice);
        }

        Ok(())
    }

    fn finalize_tool_call_arguments(&mut self) {
        for tc in &self.tool_calls {
            let arguments: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                Ok(args) => args,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        raw = %tc.arguments,
                        "Failed to parse tool arguments as JSON"
                    );
                    serde_json::Value::Null
                }
            };

            if let Some(ContentBlock::ToolCall(block)) =
                self.partial.content.get_mut(tc.content_index)
            {
                block.arguments = arguments;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_choice(&mut self, choice: OpenAIChoice) {
        let delta = choice.delta;
        if delta.content.is_some()
            || delta.tool_calls.is_some()
            || delta.reasoning_content.is_some()
        {
            self.ensure_started();
        }

        // Handle finish reason - may arrive in empty delta without content/tool_calls
        // Ensure we emit Start before processing finish_reason
        if choice.finish_reason.is_some() {
            self.ensure_started();
        }

        // Handle reasoning content (e.g. DeepSeek R1)
        if let Some(reasoning) = delta.reasoning_content {
            // Update partial content
            let last_is_thinking =
                matches!(self.partial.content.last(), Some(ContentBlock::Thinking(_)));

            let content_index = if last_is_thinking {
                self.partial.content.len() - 1
            } else {
                let idx = self.partial.content.len();
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));

                self.pending_events
                    .push_back(StreamEvent::ThinkingStart { content_index: idx });

                idx
            };

            if let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(content_index) {
                t.thinking.push_str(&reasoning);
            }

            self.pending_events.push_back(StreamEvent::ThinkingDelta {
                content_index,
                delta: reasoning,
            });
        }

        // Handle text content

        if let Some(content) = delta.content {
            // Update partial content

            let last_is_text = matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));

            let content_index = if last_is_text {
                self.partial.content.len() - 1
            } else {
                let idx = self.partial.content.len();

                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));

                self.pending_events
                    .push_back(StreamEvent::TextStart { content_index: idx });

                idx
            };

            if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(content_index) {
                t.text.push_str(&content);
            }

            self.pending_events.push_back(StreamEvent::TextDelta {
                content_index,

                delta: content,
            });
        }

        // Handle tool calls

        if let Some(tool_calls) = delta.tool_calls {
            for tc_delta in tool_calls {
                let index = tc_delta.index as usize;

                // OpenAI may emit sparse tool-call indices. Match by logical index

                // instead of assuming contiguous 0..N ordering in arrival order.

                let tool_state_idx = if let Some(existing_idx) =
                    self.tool_calls.iter().position(|tc| tc.index == index)
                {
                    existing_idx
                } else {
                    let content_index = self.partial.content.len();

                    self.tool_calls.push(ToolCallState {
                        index,

                        content_index,

                        id: String::new(),

                        name: String::new(),

                        arguments: String::new(),
                    });

                    // Initialize the tool call block in partial content

                    self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: String::new(),

                        name: String::new(),

                        arguments: serde_json::Value::Null,

                        thought_signature: None,
                    }));

                    self.pending_events
                        .push_back(StreamEvent::ToolCallStart { content_index });

                    self.tool_calls.len() - 1
                };

                let tc = &mut self.tool_calls[tool_state_idx];

                let content_index = tc.content_index;

                // Update ID if present

                if let Some(id) = tc_delta.id {
                    tc.id.push_str(&id);

                    if let Some(ContentBlock::ToolCall(block)) =
                        self.partial.content.get_mut(content_index)
                    {
                        block.id.clone_from(&tc.id);
                    }
                }

                // Update function name if present

                if let Some(function) = tc_delta.function {
                    if let Some(name) = function.name {
                        tc.name.push_str(&name);

                        if let Some(ContentBlock::ToolCall(block)) =
                            self.partial.content.get_mut(content_index)
                        {
                            block.name.clone_from(&tc.name);
                        }
                    }

                    if let Some(args) = function.arguments {
                        tc.arguments.push_str(&args);

                        // Update arguments in partial (best effort parse, or just raw string if we supported it)

                        // Note: We don't update partial.arguments here because it requires valid JSON.

                        // We only update it at the end or if we switched to storing raw string args.

                        // But we MUST emit the delta.

                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,

                            delta: args,
                        });
                    }
                }
            }
        }

        // Handle finish reason (MUST happen after delta processing to capture final chunks)

        if let Some(reason) = choice.finish_reason {
            self.partial.stop_reason = match reason.as_str() {
                "length" => StopReason::Length,

                "tool_calls" => StopReason::ToolUse,

                "content_filter" | "error" => StopReason::Error,

                _ => StopReason::Stop,
            };

            // Emit TextEnd/ThinkingEnd for all open text/thinking blocks (not just the last one,
            // since text/thinking may precede tool calls).

            for (content_index, block) in self.partial.content.iter().enumerate() {
                if let ContentBlock::Text(t) = block {
                    self.pending_events.push_back(StreamEvent::TextEnd {
                        content_index,
                        content: t.text.clone(),
                    });
                } else if let ContentBlock::Thinking(t) = block {
                    self.pending_events.push_back(StreamEvent::ThinkingEnd {
                        content_index,
                        content: t.thinking.clone(),
                    });
                }
            }

            // Finalize tool call arguments

            self.finalize_tool_call_arguments();

            // Emit ToolCallEnd for each accumulated tool call

            for tc in &self.tool_calls {
                if let Some(ContentBlock::ToolCall(tool_call)) =
                    self.partial.content.get(tc.content_index)
                {
                    self.pending_events.push_back(StreamEvent::ToolCallEnd {
                        content_index: tc.content_index,

                        tool_call: tool_call.clone(),
                    });
                }
            }
        }
    }
}

// ============================================================================
// OpenAI API Types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct OpenAIRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// Some providers (e.g., o1-series) use `max_completion_tokens` instead of `max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool<'a>>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAIStreamOptions>,
}

#[derive(Debug, Serialize)]
struct OpenAIStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage<'a> {
    role: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAIContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCallRef<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIContent<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<OpenAIContentPart<'a>>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIContentPart<'a> {
    Text { text: Cow<'a, str> },
    ImageUrl { image_url: OpenAIImageUrl<'a> },
}

#[derive(Debug, Serialize)]
struct OpenAIImageUrl<'a> {
    url: String,
    #[serde(skip)]
    // Phantom data for lifetime if needed, but url is String here as constructed from format!
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCallRef<'a> {
    id: &'a str,
    r#type: &'static str,
    function: OpenAIFunctionRef<'a>,
}

#[derive(Debug, Serialize)]
struct OpenAIFunctionRef<'a> {
    name: &'a str,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAITool<'a> {
    r#type: &'static str,
    function: OpenAIFunction<'a>,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
    #[serde(default)]
    error: Option<OpenAIChunkError>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    delta: OpenAIDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAIFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct OpenAIUsage {
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: Option<u64>,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAIPromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct OpenAIPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChunkError {
    #[serde(default)]
    message: Option<String>,
}

// ============================================================================
// Conversion Functions
// ============================================================================

#[allow(clippy::too_many_lines)]
fn convert_message_to_openai(message: &Message) -> Vec<OpenAIMessage<'_>> {
    match message {
        Message::User(user) => vec![OpenAIMessage {
            role: Cow::Borrowed("user"),
            content: Some(convert_user_content(&user.content)),
            tool_calls: None,
            tool_call_id: None,
        }],
        Message::Custom(custom) => vec![OpenAIMessage {
            role: Cow::Borrowed("user"),
            content: Some(OpenAIContent::Text(Cow::Borrowed(&custom.content))),
            tool_calls: None,
            tool_call_id: None,
        }],
        Message::Assistant(assistant) => {
            let mut messages = Vec::new();

            // Collect text content
            let text: String = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");

            // Collect tool calls
            let tool_calls: Vec<OpenAIToolCallRef<'_>> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(tc) => Some(OpenAIToolCallRef {
                        id: &tc.id,
                        r#type: "function",
                        function: OpenAIFunctionRef {
                            name: &tc.name,
                            arguments: tc.arguments.to_string(),
                        },
                    }),
                    _ => None,
                })
                .collect();

            let content = if text.is_empty() {
                // Send empty string instead of omitting the field. Some
                // OpenAI-compatible providers (e.g. GLM via Ollama Cloud) reject
                // requests where assistant messages have no content field.
                // An empty string is valid per the OpenAI spec and accepted
                // by all known providers.
                Some(OpenAIContent::Text(Cow::Borrowed("")))
            } else {
                Some(OpenAIContent::Text(Cow::Owned(text)))
            };

            let tool_calls = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            messages.push(OpenAIMessage {
                role: Cow::Borrowed("assistant"),
                content,
                tool_calls,
                tool_call_id: None,
            });

            messages
        }
        Message::ToolResult(result) => {
            let mut text_parts = Vec::new();
            let mut image_parts = Vec::new();

            for block in &result.content {
                match block {
                    ContentBlock::Text(t) => text_parts.push(t.text.as_str()),
                    ContentBlock::Image(img) => {
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        image_parts.push(OpenAIContentPart::ImageUrl {
                            image_url: OpenAIImageUrl {
                                url,
                                _phantom: std::marker::PhantomData,
                            },
                        });
                    }
                    _ => {}
                }
            }

            let text_content = if text_parts.is_empty() {
                if image_parts.is_empty() {
                    Some(OpenAIContent::Text(Cow::Borrowed("")))
                } else {
                    Some(OpenAIContent::Text(Cow::Borrowed("(see attached image)")))
                }
            } else {
                Some(OpenAIContent::Text(Cow::Owned(text_parts.join("\n"))))
            };

            let mut messages = vec![OpenAIMessage {
                role: Cow::Borrowed("tool"),
                content: text_content,
                tool_calls: None,
                tool_call_id: Some(&result.tool_call_id),
            }];

            if !image_parts.is_empty() {
                let mut parts = vec![OpenAIContentPart::Text {
                    text: Cow::Borrowed("Attached image(s) from tool result:"),
                }];
                parts.extend(image_parts);
                messages.push(OpenAIMessage {
                    role: Cow::Borrowed("user"),
                    content: Some(OpenAIContent::Parts(parts)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }

            messages
        }
    }
}

fn convert_user_content(content: &UserContent) -> OpenAIContent<'_> {
    match content {
        UserContent::Text(text) => OpenAIContent::Text(Cow::Borrowed(text)),
        UserContent::Blocks(blocks) => {
            let parts: Vec<OpenAIContentPart<'_>> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(OpenAIContentPart::Text {
                        text: Cow::Borrowed(&t.text),
                    }),
                    ContentBlock::Image(img) => {
                        // Convert to data URL for OpenAI
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        Some(OpenAIContentPart::ImageUrl {
                            image_url: OpenAIImageUrl {
                                url,
                                _phantom: std::marker::PhantomData,
                            },
                        })
                    }
                    _ => None,
                })
                .collect();
            OpenAIContent::Parts(parts)
        }
    }
}

fn convert_tool_to_openai(tool: &ToolDef) -> OpenAITool<'_> {
    OpenAITool {
        r#type: "function",
        function: OpenAIFunction {
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
        },
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use futures::{StreamExt, stream};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn test_convert_user_text_message() {
        let message = Message::User(crate::model::UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        });

        let converted = convert_message_to_openai(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
    }

    #[test]
    fn test_tool_conversion() {
        let tool = ToolDef {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": {"type": "string"}
                }
            }),
        };

        let converted = convert_tool_to_openai(&tool);
        assert_eq!(converted.r#type, "function");
        assert_eq!(converted.function.name, "test_tool");
        assert_eq!(converted.function.description, "A test tool");
        assert_eq!(
            converted.function.parameters,
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": {"type": "string"}
                }
            })
        );
    }

    #[test]
    fn test_provider_info() {
        let provider = OpenAIProvider::new("gpt-4o");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.api(), "openai-completions");
    }

    #[test]
    fn test_build_request_includes_system_tools_and_stream_options() {
        let provider = OpenAIProvider::new("gpt-4o");
        let context = Context {
            system_prompt: Some("You are concise.".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: vec![ToolDef {
                name: "search".to_string(),
                description: "Search docs".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "q": { "type": "string" }
                    },
                    "required": ["q"]
                }),
            }]
            .into(),
        };
        let options = StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(123),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["model"], "gpt-4o");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "You are concise.");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "Ping");
        let temperature = value["temperature"]
            .as_f64()
            .expect("temperature should serialize as number");
        assert!((temperature - 0.2).abs() < 1e-6);
        assert_eq!(value["max_tokens"], 123);
        assert_eq!(value["stream"], true);
        assert_eq!(value["stream_options"]["include_usage"], true);
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "search");
        assert_eq!(value["tools"][0]["function"]["description"], "Search docs");
        assert_eq!(
            value["tools"][0]["function"]["parameters"],
            json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                },
                "required": ["q"]
            })
        );
    }

    #[test]
    fn test_stream_accumulates_tool_call_argument_deltas() {
        let events = vec![
            json!({ "choices": [{ "delta": {} }] }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "function": {
                                "name": "search",
                                "arguments": "{\"q\":\"ru"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "st\"}"
                            }
                        }]
                    }
                }]
            }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ToolCallDelta { delta, .. } if delta == "{\"q\":\"ru")
        ));
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ToolCallDelta { delta, .. } if delta == "st\"}"))
        );
        let done = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        let tool_call = done
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("assembled tool call content");
        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.name, "search");
        assert_eq!(tool_call.arguments, json!({ "q": "rust" }));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_handles_sparse_tool_call_index_without_panic() {
        let events = vec![
            json!({ "choices": [{ "delta": {} }] }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 2,
                            "id": "call_sparse",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"q\":\"sparse\"}"
                            }
                        }]
                    }
                }]
            }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        let done = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        let tool_calls: Vec<&ToolCall> = done
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_sparse");
        assert_eq!(tool_calls[0].name, "lookup");
        assert_eq!(tool_calls[0].arguments, json!({ "q": "sparse" }));
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ToolCallStart { .. })),
            "expected tool call start event"
        );
    }

    #[test]
    fn test_stream_maps_finish_reason_error_to_stop_reason_error() {
        let events = vec![
            json!({
                "choices": [{ "delta": {}, "finish_reason": "error" }],
                "error": { "message": "upstream provider timeout" }
            }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        let done = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { reason, message } => Some((reason, message)),
                _ => None,
            })
            .expect("done event");
        assert_eq!(*done.0, StopReason::Error);
        assert_eq!(
            done.1.error_message.as_deref(),
            Some("upstream provider timeout")
        );
    }

    #[test]
    fn test_finish_reason_without_prior_content_emits_start() {
        let events = vec![
            json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);

        // Should have: Start, Done
        // First event must be Start (bug would skip this)
        assert!(!out.is_empty(), "expected at least one event");
        assert!(
            matches!(out[0], StreamEvent::Start { .. }),
            "First event should be Start, got {:?}",
            out[0]
        );
    }

    #[test]
    fn test_stream_emits_all_events_in_correct_order() {
        let events = vec![
            json!({ "choices": [{ "delta": { "content": "Hello" } }] }),
            json!({ "choices": [{ "delta": { "content": " world" } }] }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);

        // Verify sequence: Start, TextStart, TextDelta, TextDelta, TextEnd, Done
        assert_eq!(out.len(), 6, "Expected 6 events, got {}", out.len());

        assert!(
            matches!(out[0], StreamEvent::Start { .. }),
            "Event 0 should be Start, got {:?}",
            out[0]
        );

        assert!(
            matches!(
                out[1],
                StreamEvent::TextStart {
                    content_index: 0,
                    ..
                }
            ),
            "Event 1 should be TextStart at index 0, got {:?}",
            out[1]
        );

        assert!(
            matches!(&out[2], StreamEvent::TextDelta { content_index: 0, delta, .. } if delta == "Hello"),
            "Event 2 should be TextDelta 'Hello' at index 0, got {:?}",
            out[2]
        );

        assert!(
            matches!(&out[3], StreamEvent::TextDelta { content_index: 0, delta, .. } if delta == " world"),
            "Event 3 should be TextDelta ' world' at index 0, got {:?}",
            out[3]
        );

        assert!(
            matches!(&out[4], StreamEvent::TextEnd { content_index: 0, content, .. } if content == "Hello world"),
            "Event 4 should be TextEnd 'Hello world' at index 0, got {:?}",
            out[4]
        );

        assert!(
            matches!(
                out[5],
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    ..
                }
            ),
            "Event 5 should be Done with Stop reason, got {:?}",
            out[5]
        );
    }

    #[test]
    fn test_build_request_applies_openrouter_routing_overrides() {
        let provider = OpenAIProvider::new("openai/gpt-4o-mini")
            .with_provider_name("openrouter")
            .with_compat(Some(CompatConfig {
                open_router_routing: Some(json!({
                    "models": ["openai/gpt-4o-mini", "anthropic/claude-3.5-sonnet"],
                    "provider": {
                        "order": ["openai", "anthropic"],
                        "allow_fallbacks": false
                    },
                    "route": "fallback"
                })),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions::default();

        let request = provider
            .build_request_json(&context, &options)
            .expect("request json");
        assert_eq!(request["model"], "openai/gpt-4o-mini");
        assert_eq!(request["route"], "fallback");
        assert_eq!(request["provider"]["allow_fallbacks"], false);
        assert_eq!(request["models"][0], "openai/gpt-4o-mini");
        assert_eq!(request["models"][1], "anthropic/claude-3.5-sonnet");
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
            Some("text/event-stream")
        );

        let body: Value = serde_json::from_str(&captured.body).expect("request body json");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn test_stream_openrouter_injects_default_attribution_headers() {
        let options = StreamOptions {
            api_key: Some("test-openrouter-key".to_string()),
            ..Default::default()
        };
        let captured = run_stream_and_capture_headers_with(
            OpenAIProvider::new("openai/gpt-4o-mini").with_provider_name("openrouter"),
            &options,
        )
        .expect("captured request");

        assert_eq!(
            captured.headers.get("http-referer").map(String::as_str),
            Some(OPENROUTER_DEFAULT_HTTP_REFERER)
        );
        assert_eq!(
            captured.headers.get("x-title").map(String::as_str),
            Some(OPENROUTER_DEFAULT_X_TITLE)
        );
    }

    #[test]
    fn test_stream_openrouter_respects_explicit_attribution_headers() {
        let options = StreamOptions {
            api_key: Some("test-openrouter-key".to_string()),
            headers: HashMap::from([
                (
                    "HTTP-Referer".to_string(),
                    "https://example.test/app".to_string(),
                ),
                (
                    "X-Title".to_string(),
                    "Custom OpenRouter Client".to_string(),
                ),
            ]),
            ..Default::default()
        };
        let captured = run_stream_and_capture_headers_with(
            OpenAIProvider::new("openai/gpt-4o-mini").with_provider_name("openrouter"),
            &options,
        )
        .expect("captured request");

        assert_eq!(
            captured.headers.get("http-referer").map(String::as_str),
            Some("https://example.test/app")
        );
        assert_eq!(
            captured.headers.get("x-title").map(String::as_str),
            Some("Custom OpenRouter Client")
        );
    }

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

    #[test]
    fn test_stream_fixtures() {
        let fixture = load_fixture("openai_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case {}", case.name);
        }
    }

    fn load_fixture(file_name: &str) -> ProviderFixture {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/provider_responses")
            .join(file_name);
        let raw = std::fs::read_to_string(path).expect("fixture read");
        serde_json::from_str(&raw).expect("fixture parse")
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    fn run_stream_and_capture_headers() -> Option<CapturedRequest> {
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };
        run_stream_and_capture_headers_with(OpenAIProvider::new("gpt-4o"), &options)
    }

    fn run_stream_and_capture_headers_with(
        provider: OpenAIProvider,
        options: &StreamOptions,
    ) -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = provider.with_base_url(base_url);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, options).await.expect("stream");
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
            r#"data: {"choices":[{"delta":{}}]}"#,
            "",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "",
            "data: [DONE]",
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
                    Err(err) => panic!("read error: {err}"),
                }
            }

            let header_end = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("request header boundary");
            let header_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let headers = parse_headers(&header_text);
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
                    Err(err) => panic!("read error: {err}"),
                }
            }

            let captured = CapturedRequest {
                headers,
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

        (format!("http://{addr}/chat/completions"), rx)
    }

    fn parse_headers(header_text: &str) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        for line in header_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        headers
    }

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| {
                        let data = match event {
                            Value::String(text) => text.clone(),
                            _ => serde_json::to_string(event).expect("serialize event"),
                        };
                        format!("data: {data}\n\n").into_bytes()
                    })
                    .map(Ok),
            );
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai".to_string(),
                "openai".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.data == "[DONE]" {
                    out.extend(state.pending_events.drain(..));
                    let reason = state.partial.stop_reason;
                    out.push(StreamEvent::Done {
                        reason,
                        message: std::mem::take(&mut state.partial),
                    });
                    break;
                }
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
            }

            out
        })
    }

    fn collect_thinking_text(events: &[StreamEvent]) -> String {
        let mut out = String::new();
        for event in events {
            if let StreamEvent::ThinkingDelta { delta, .. } = event {
                out.push_str(delta);
            }
        }
        out
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
            StreamEvent::TextStart { content_index, .. } => EventSummary {
                kind: "text_start".to_string(),
                content_index: Some(*content_index),
                delta: None,
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
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "tool_use",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
        }
        .to_string()
    }

    // ── bd-3uqg.2.4: compat override behavior ──────────────────────

    fn context_with_tools() -> Context<'static> {
        Context {
            system_prompt: Some("You are helpful.".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: vec![ToolDef {
                name: "search".to_string(),
                description: "Search".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
            .into(),
        }
    }

    fn default_stream_options() -> StreamOptions {
        StreamOptions {
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    #[test]
    fn compat_system_role_name_overrides_default() {
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            system_role_name: Some("developer".to_string()),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(
            value["messages"][0]["role"], "developer",
            "system message should use overridden role name"
        );
    }

    #[test]
    fn compat_none_uses_default_system_role() {
        let provider = OpenAIProvider::new("gpt-4o");
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(
            value["messages"][0]["role"], "system",
            "default system role should be 'system'"
        );
    }

    #[test]
    fn compat_supports_tools_false_omits_tools() {
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            supports_tools: Some(false),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert!(
            value["tools"].is_null(),
            "tools should be omitted when supports_tools=false"
        );
    }

    #[test]
    fn compat_supports_tools_true_includes_tools() {
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            supports_tools: Some(true),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert!(
            value["tools"].is_array(),
            "tools should be included when supports_tools=true"
        );
    }

    #[test]
    fn compat_max_tokens_field_routes_to_max_completion_tokens() {
        let provider = OpenAIProvider::new("o1").with_compat(Some(CompatConfig {
            max_tokens_field: Some("max_completion_tokens".to_string()),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert!(
            value["max_tokens"].is_null(),
            "max_tokens should be absent when routed to max_completion_tokens"
        );
        assert_eq!(
            value["max_completion_tokens"], 1024,
            "max_completion_tokens should carry the token limit"
        );
    }

    #[test]
    fn compat_default_routes_to_max_tokens() {
        let provider = OpenAIProvider::new("gpt-4o");
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(
            value["max_tokens"], 1024,
            "default should use max_tokens field"
        );
        assert!(
            value["max_completion_tokens"].is_null(),
            "max_completion_tokens should be absent by default"
        );
    }

    #[test]
    fn compat_supports_usage_in_streaming_false() {
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            supports_usage_in_streaming: Some(false),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(
            value["stream_options"]["include_usage"], false,
            "include_usage should be false when supports_usage_in_streaming=false"
        );
    }

    #[test]
    fn compat_combined_overrides() {
        let provider = OpenAIProvider::new("custom-model").with_compat(Some(CompatConfig {
            system_role_name: Some("developer".to_string()),
            max_tokens_field: Some("max_completion_tokens".to_string()),
            supports_tools: Some(false),
            supports_usage_in_streaming: Some(false),
            ..Default::default()
        }));
        let context = context_with_tools();
        let options = default_stream_options();
        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(value["messages"][0]["role"], "developer");
        assert!(value["max_tokens"].is_null());
        assert_eq!(value["max_completion_tokens"], 1024);
        assert!(value["tools"].is_null());
        assert_eq!(value["stream_options"]["include_usage"], false);
    }

    #[test]
    fn compat_custom_headers_injected_into_stream_request() {
        let mut custom = HashMap::new();
        custom.insert("X-Custom-Tag".to_string(), "test-123".to_string());
        custom.insert("X-Provider-Region".to_string(), "us-east-1".to_string());
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIProvider::new("gpt-4o")
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom),
                ..Default::default()
            }));

        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
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

        let captured = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured request");
        assert_eq!(
            captured.headers.get("x-custom-tag").map(String::as_str),
            Some("test-123"),
            "custom header should be present in request"
        );
        assert_eq!(
            captured
                .headers
                .get("x-provider-region")
                .map(String::as_str),
            Some("us-east-1"),
            "custom header should be present in request"
        );
    }

    #[test]
    fn compat_authorization_header_is_used_without_api_key() {
        let mut custom = HashMap::new();
        custom.insert(
            "Authorization".to_string(),
            "Bearer compat-openai-token".to_string(),
        );
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            custom_headers: Some(custom),
            ..Default::default()
        }));
        let options = StreamOptions::default();

        let captured =
            run_stream_and_capture_headers_with(provider, &options).expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer compat-openai-token")
        );
    }

    #[test]
    fn blank_compat_authorization_header_does_not_override_builtin_api_key() {
        let mut custom = HashMap::new();
        custom.insert("Authorization".to_string(), "   ".to_string());
        let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
            custom_headers: Some(custom),
            ..Default::default()
        }));
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
            ..Default::default()
        };

        let captured =
            run_stream_and_capture_headers_with(provider, &options).expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer test-openai-key")
        );
    }

    #[test]
    fn reasoning_only_delta_emits_thinking_events() {
        let events = vec![
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "plan"},
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ThinkingStart { .. })),
            "expected ThinkingStart for reasoning-only delta"
        );
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ThinkingDelta { .. })),
            "expected ThinkingDelta for reasoning-only delta"
        );
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ThinkingEnd { .. })),
            "expected ThinkingEnd after finish_reason"
        );
        assert_eq!(collect_thinking_text(&out), "plan");
    }

    #[test]
    fn reasoning_delta_segmentation_is_lossless() {
        let single = vec![
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "abc"},
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            Value::String("[DONE]".to_string()),
        ];

        let split = vec![
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "a"},
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "bc"},
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            Value::String("[DONE]".to_string()),
        ];

        let single_out = collect_events(&single);
        let split_out = collect_events(&split);

        assert_eq!(
            collect_thinking_text(&single_out),
            collect_thinking_text(&split_out),
            "segmenting reasoning deltas should not change final thinking text"
        );
    }

    // ========================================================================
    // Proptest — process_event() fuzz coverage (FUZZ-P1.3)
    // ========================================================================

    mod proptest_process_event {
        use super::*;
        use proptest::prelude::*;

        fn make_state()
        -> StreamState<impl Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin>
        {
            let empty = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
            let sse = crate::sse::SseStream::new(Box::pin(empty));
            StreamState::new(sse, "gpt-test".into(), "openai".into(), "openai".into())
        }

        fn small_string() -> impl Strategy<Value = String> {
            prop_oneof![Just(String::new()), "[a-zA-Z0-9_]{1,16}", "[ -~]{0,32}",]
        }

        fn optional_string() -> impl Strategy<Value = Option<String>> {
            prop_oneof![Just(None), small_string().prop_map(Some),]
        }

        fn token_count() -> impl Strategy<Value = u64> {
            prop_oneof![
                5 => 0u64..10_000u64,
                2 => Just(0u64),
                1 => Just(u64::MAX),
                1 => (u64::MAX - 100)..=u64::MAX,
            ]
        }

        fn finish_reason() -> impl Strategy<Value = Option<String>> {
            prop_oneof![
                3 => Just(None),
                1 => Just(Some("stop".to_string())),
                1 => Just(Some("length".to_string())),
                1 => Just(Some("tool_calls".to_string())),
                1 => Just(Some("content_filter".to_string())),
                1 => small_string().prop_map(Some),
            ]
        }

        fn tool_call_index() -> impl Strategy<Value = u32> {
            prop_oneof![
                5 => 0u32..3u32,
                1 => Just(u32::MAX),
                1 => 100u32..200u32,
            ]
        }

        /// Generate valid `OpenAIStreamChunk` JSON.
        fn openai_chunk_json() -> impl Strategy<Value = String> {
            prop_oneof![
                // Text content delta
                3 => (small_string(), finish_reason()).prop_map(|(text, fr)| {
                    let mut choice = serde_json::json!({
                        "delta": {"content": text}
                    });
                    if let Some(reason) = fr {
                        choice["finish_reason"] = serde_json::Value::String(reason);
                    }
                    serde_json::json!({"choices": [choice]}).to_string()
                }),
                // Empty delta (initial or heartbeat)
                2 => Just(r#"{"choices":[{"delta":{}}]}"#.to_string()),
                // Finish-only delta
                2 => finish_reason()
                    .prop_filter_map("some reason", |fr| fr)
                    .prop_map(|reason| {
                        serde_json::json!({
                            "choices": [{"delta": {}, "finish_reason": reason}]
                        })
                        .to_string()
                    }),
                // Tool call delta
                3 => (tool_call_index(), optional_string(), optional_string(), optional_string())
                    .prop_map(|(idx, id, name, args)| {
                        let mut tc = serde_json::json!({"index": idx});
                        if let Some(id) = id { tc["id"] = serde_json::Value::String(id); }
                        let mut func = serde_json::Map::new();
                        if let Some(n) = name { func.insert("name".into(), serde_json::Value::String(n)); }
                        if let Some(a) = args { func.insert("arguments".into(), serde_json::Value::String(a)); }
                        if !func.is_empty() { tc["function"] = serde_json::Value::Object(func); }
                        serde_json::json!({
                            "choices": [{"delta": {"tool_calls": [tc]}}]
                        })
                        .to_string()
                    }),
                // Usage-only chunk (no choices)
                2 => (token_count(), token_count(), token_count()).prop_map(|(prompt, compl, total)| {
                    serde_json::json!({
                        "choices": [],
                        "usage": {
                            "prompt_tokens": prompt,
                            "completion_tokens": compl,
                            "total_tokens": total
                        }
                    })
                    .to_string()
                }),
                // Error chunk
                1 => small_string().prop_map(|msg| {
                    serde_json::json!({
                        "choices": [],
                        "error": {"message": msg}
                    })
                    .to_string()
                }),
                // Empty choices
                1 => Just(r#"{"choices":[]}"#.to_string()),
            ]
        }

        /// Chaos — arbitrary JSON strings.
        fn chaos_json() -> impl Strategy<Value = String> {
            prop_oneof![
                Just(String::new()),
                Just("{}".to_string()),
                Just("[]".to_string()),
                Just("null".to_string()),
                Just("{".to_string()),
                Just(r#"{"choices":"not_array"}"#.to_string()),
                Just(r#"{"choices":[{"delta":null}]}"#.to_string()),
                "[a-z_]{1,20}".prop_map(|t| format!(r#"{{"type":"{t}"}}"#)),
                "[ -~]{0,64}",
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn process_event_valid_never_panics(data in openai_chunk_json()) {
                let mut state = make_state();
                let _ = state.process_event(&data);
            }

            #[test]
            fn process_event_chaos_never_panics(data in chaos_json()) {
                let mut state = make_state();
                let _ = state.process_event(&data);
            }

            #[test]
            fn process_event_sequence_never_panics(
                events in prop::collection::vec(openai_chunk_json(), 1..8)
            ) {
                let mut state = make_state();
                for event in &events {
                    let _ = state.process_event(event);
                }
            }

            #[test]
            fn process_event_mixed_sequence_never_panics(
                events in prop::collection::vec(
                    prop_oneof![openai_chunk_json(), chaos_json()],
                    1..12
                )
            ) {
                let mut state = make_state();
                for event in &events {
                    let _ = state.process_event(event);
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

    type FuzzStream =
        Pin<Box<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>>;

    /// Opaque wrapper around the OpenAI stream processor state.
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
            Self(StreamState::new(
                crate::sse::SseStream::new(Box::pin(empty)),
                "gpt-fuzz".into(),
                "openai".into(),
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
