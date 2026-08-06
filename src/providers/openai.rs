//! OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI Chat Completions API,
//! supporting streaming responses and tool use. Compatible with:
//! - OpenAI direct API (api.openai.com)
//! - Azure OpenAI
//! - Any OpenAI-compatible API (Groq, Together, etc.)

use std::borrow::Cow;

use crate::auth::{
    AUTH_RESOLUTION_LOCK_TIMEOUT, AuthStorage, AuthStorageLoadFailure,
    resolve_ambient_sap_auth_token_with_client, resolve_sap_auth_candidate_with_client,
    resolve_sap_auth_token_with_client,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::provider_metadata::canonical_provider_id;
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_API_ERROR_BODY_BYTES: usize = 64 * 1024;
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

fn redacted_api_error_body(body: &str, secrets: &[&str]) -> String {
    crate::auth::redact_known_secrets_bounded(body, secrets, MAX_API_ERROR_BODY_BYTES)
}

fn push_api_error_secret(secrets: &mut Vec<String>, value: &str, authorization: bool) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    secrets.push(value.to_string());
    if authorization && let Some(separator) = value.find(char::is_whitespace) {
        let credential = value[separator..].trim();
        if !credential.is_empty() {
            secrets.push(credential.to_string());
        }
    }
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
    auth_path_override: Option<PathBuf>,
    auth_header: bool,
    /// Whether the model is a reasoning model. Gates the DeepSeek thinking
    /// dialect so non-reasoning DeepSeek models (e.g. `deepseek-chat`) never
    /// emit `thinking`/`reasoning_effort` (gh #114). Defaults to `false`; the
    /// registry sets it from `ModelEntry::model.reasoning` via `with_reasoning`.
    reasoning: bool,
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
            auth_path_override: None,
            auth_header: true,
            reasoning: false,
        }
    }

    /// Set whether the underlying model is a reasoning model.
    ///
    /// Only consulted by the DeepSeek thinking dialect (`reasoning_style`): a
    /// non-reasoning DeepSeek model serializes with no `thinking`/`reasoning_effort`
    /// (byte-for-byte as before #113), while a reasoning one forwards the level.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
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

    #[cfg(test)]
    #[must_use]
    fn with_auth_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.auth_path_override = Some(path.into());
        self
    }

    fn auth_path(&self) -> PathBuf {
        self.auth_path_override
            .clone()
            .unwrap_or_else(Config::auth_path)
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

    /// Control whether this route generates a Bearer Authorization header.
    /// Custom Authorization headers remain authoritative regardless of this flag.
    #[must_use]
    pub const fn with_auth_header(mut self, enabled: bool) -> Self {
        self.auth_header = enabled;
        self
    }

    /// Detect a provider-specific reasoning dialect for this transport.
    ///
    /// DeepSeek is identified the same way `ModelEntry::is_deepseek_reasoning_model`
    /// does it — by the canonical provider id (so the `deep-seek` alias also
    /// matches) or a `deepseek.com` base URL — AND only for reasoning models, so a
    /// non-reasoning DeepSeek model (e.g. `deepseek-chat`) emits no
    /// `thinking`/`reasoning_effort` (byte-for-byte as before #113, gh #114).
    /// Every other OpenAI-compatible provider is left untouched.
    fn reasoning_style(&self) -> Option<ReasoningStyle> {
        if !self.reasoning {
            return None;
        }
        let provider_is_deepseek = canonical_provider_id(&self.provider)
            .is_some_and(|canonical| canonical == "deepseek")
            || self.provider.eq_ignore_ascii_case("deepseek");
        let base_is_deepseek = self.base_url.to_ascii_lowercase().contains("deepseek.com");
        if provider_is_deepseek || base_is_deepseek {
            Some(ReasoningStyle::DeepSeek)
        } else {
            None
        }
    }

    fn is_sap_ai_core(&self) -> bool {
        canonical_provider_id(&self.provider).is_some_and(|canonical| canonical == "sap-ai-core")
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

        // Forward the reasoning level for providers with a request-side reasoning
        // dialect. Only DeepSeek today; all other transports get `(None, None)`,
        // so their serialized body is unchanged. DeepSeek collapses `low`/`medium`
        // into `high` itself, so we only emit the values it documents and let
        // `off` request the explicit non-thinking path. Both `xhigh` and `max`
        // map to DeepSeek's top `"max"` tier (xhigh kept its historical mapping
        // when the first-class `max` level was added; gh #139).
        let (thinking, reasoning_effort) = match self.reasoning_style() {
            Some(ReasoningStyle::DeepSeek) => match options.thinking_level.unwrap_or_default() {
                ThinkingLevel::Off => (Some(OpenAIThinking { kind: "disabled" }), None),
                ThinkingLevel::High => (Some(OpenAIThinking { kind: "enabled" }), Some("high")),
                ThinkingLevel::XHigh | ThinkingLevel::Max => {
                    (Some(OpenAIThinking { kind: "enabled" }), Some("max"))
                }
                ThinkingLevel::Minimal | ThinkingLevel::Low | ThinkingLevel::Medium => {
                    (Some(OpenAIThinking { kind: "enabled" }), None)
                }
            },
            None => (None, None),
        };

        OpenAIRequest {
            model: &self.model,
            messages,
            max_tokens,
            max_completion_tokens,
            temperature: options.temperature,
            tools,
            stream: true,
            stream_options,
            thinking,
            reasoning_effort,
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

        let auth_value = if authorization_override.is_some() || !self.auth_header {
            None
        } else {
            // SAP supports either a direct bearer or a service-key JSON candidate. Classify every
            // candidate before constructing the Authorization header so legacy ApiKey entries can
            // never leak a service key verbatim. The explicit lane remains highest precedence and
            // bypasses auth-file loading when it contains a direct bearer.
            let resolved = if self.is_sap_ai_core() {
                if let Some(candidate) = options
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                {
                    resolve_sap_auth_candidate_with_client(&self.client, candidate).await?
                } else {
                    match AuthStorage::load_with_lock_timeout_classified(
                        self.auth_path(),
                        AUTH_RESOLUTION_LOCK_TIMEOUT,
                    ) {
                        Ok(auth) => {
                            resolve_sap_auth_token_with_client(&self.client, &auth, None).await?
                        }
                        Err(failure @ AuthStorageLoadFailure::LockTimeout(_)) => {
                            return Err(failure.into_error());
                        }
                        Err(AuthStorageLoadFailure::Other(load_error)) => {
                            let ambient =
                                resolve_ambient_sap_auth_token_with_client(&self.client).await?;
                            if ambient.is_some() {
                                tracing::warn!(
                                    error = %load_error,
                                    "stored SAP credentials are unavailable; using complete ambient credentials"
                                );
                                ambient
                            } else {
                                return Err(Error::auth(format!(
                                    "Failed to load SAP AI Core credentials: {load_error}"
                                )));
                            }
                        }
                    }
                }
            } else if let Some(key) = options
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
            {
                Some(key.to_string())
            } else {
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty())
            };
            match resolved {
                Some(key) => Some(key),
                // Local / self-hosted providers (ollama, llamacpp, mistralrs, …)
                // expose an OpenAI-compatible server on localhost and require NO
                // API key. For these we proceed without an Authorization header
                // instead of failing, matching how ollama already works. (#104)
                None if crate::provider_metadata::provider_is_keyless_local(self.name()) => None,
                None if self.is_sap_ai_core() => {
                    return Err(Error::auth(
                        "SAP AI Core requires AICORE_SERVICE_KEY, the SAP_AI_CORE_* split credential variables, a stored service key, or an explicit bearer token.",
                    ));
                }
                None => {
                    return Err(Error::provider(
                        self.name(),
                        "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                    ));
                }
            }
        };

        let request_body = self.build_request_json(context, options)?;

        // Note: Content-Type is set by .json() below; setting it here too
        // produces a duplicate header that OpenAI's server rejects.
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream");

        if let Some(auth_value) = auth_value.as_deref() {
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

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text_limited(MAX_API_ERROR_BODY_BYTES)
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            let mut secrets = Vec::new();
            for value in [auth_value.as_deref(), authorization_override.as_deref()]
                .into_iter()
                .flatten()
            {
                push_api_error_secret(&mut secrets, value, true);
            }
            if let Some(headers) = self
                .compat
                .as_ref()
                .and_then(|compat| compat.custom_headers.as_ref())
            {
                for (name, value) in headers {
                    push_api_error_secret(
                        &mut secrets,
                        value,
                        name.eq_ignore_ascii_case("authorization"),
                    );
                }
            }
            for (name, value) in &options.headers {
                push_api_error_secret(
                    &mut secrets,
                    value,
                    name.eq_ignore_ascii_case("authorization"),
                );
            }
            if let Ok(url) = url::Url::parse(&self.base_url) {
                secrets.extend(
                    url.query_pairs()
                        .map(|(_, value)| value.into_owned())
                        .filter(|value| !value.is_empty()),
                );
                push_api_error_secret(&mut secrets, url.username(), false);
                if let Some(password) = url.password() {
                    push_api_error_secret(&mut secrets, password, false);
                }
            }
            let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let body = redacted_api_error_body(&body, &secret_refs);
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
                            let err = Error::sse(&e);
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

/// Best-effort completion of a partial JSON document into the most-complete
/// valid `Value` it can represent (#124).
///
/// Streaming tool-call `arguments` arrive as a growing prefix of a JSON object
/// (e.g. `{"path": "src/li`). Snapshot-based clients render the partial
/// message's `arguments`, so leaving it `Null` until the terminal event makes a
/// large tool call pop in all at once instead of streaming like text. This
/// closes an open string and any open objects/arrays, dropping a dangling
/// trailing comma or `"key":` (a key with no value yet) so the prefix parses.
///
/// Safety: it only ever CLOSES structure that is already open — it never
/// fabricates content — and returns `None` when the prefix still can't be
/// parsed, so the caller keeps the last good value. The result is therefore
/// always either a valid `Value` that is a faithful completion of the prefix,
/// or `None`; it can never surface wrong data.
///
/// Shared with the agent loop (#126): the agent rebuilds the partial message
/// that RPC/ACP clients actually receive from `StreamEvent`s, so the same
/// completion must be applied there (for every provider), not only inside this
/// provider's internal partial.
pub(crate) fn complete_partial_json(input: &str) -> Option<serde_json::Value> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // Fast path: the accumulated prefix is already valid JSON.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
        return Some(value);
    }

    // Structural scan to learn what is still open.
    let mut closers: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for byte in s.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => closers.push('}'),
            b'[' => closers.push(']'),
            b'}' | b']' => {
                closers.pop();
            }
            _ => {}
        }
    }

    let mut out = String::from(s);
    if in_string {
        if escaped {
            // Dangling escape backslash (`..."ab\`) — drop it before closing.
            out.pop();
        }
        out.push('"');
    }

    // Close each open container, trimming a dangling tail before its closer so
    // the result parses.
    while let Some(closer) = closers.pop() {
        trim_dangling_json_tail(&mut out, closer == '}');
        out.push(closer);
    }

    serde_json::from_str::<serde_json::Value>(&out).ok()
}

/// Before appending a container's closer, drop a trailing comma, and (for an
/// object) a dangling `"key":` or bare `"key"` (a key with no value yet), so the
/// closed container is valid JSON. A complete `"key": "value"` member is left
/// intact — a trailing string preceded by `:` is a value, not a dangling key.
fn trim_dangling_json_tail(out: &mut String, is_object: bool) {
    loop {
        let before = out.len();
        while out.ends_with(char::is_whitespace) {
            out.pop();
        }
        if out.ends_with(',') {
            out.pop();
            continue;
        }
        if is_object {
            // `"key":` with no value yet → drop the colon and the key string.
            if out.ends_with(':') {
                out.pop();
                while out.ends_with(char::is_whitespace) {
                    out.pop();
                }
                remove_trailing_json_string(out);
                continue;
            }
            // A trailing string: a value (preceded by `:`) means the member is
            // complete — stop and close. Otherwise (preceded by `,`/`{`) it's a
            // dangling key with no colon yet → drop it.
            if let Some(start) = trailing_json_string_start(out) {
                let preceded_by_colon = out[..start].trim_end().ends_with(':');
                if preceded_by_colon {
                    break;
                }
                out.truncate(start);
                continue;
            }
        }
        if out.len() == before {
            break;
        }
    }
}

/// Byte index of the opening quote of the JSON string literal that `out` ends
/// with (honoring backslash escapes), or `None` if `out` does not end with a
/// closing quote.
fn trailing_json_string_start(out: &str) -> Option<usize> {
    let bytes = out.as_bytes();
    if bytes.last() != Some(&b'"') {
        return None;
    }
    let mut i = bytes.len() - 1; // the closing quote
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' {
            // Count preceding backslashes to tell an escaped quote from the open.
            let mut backslashes = 0usize;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes.is_multiple_of(2) {
                return Some(i);
            }
        }
    }
    None
}

/// Remove a complete trailing JSON string literal (`"..."`, honoring escapes)
/// from `out`. No-op if `out` does not end with a closing quote.
fn remove_trailing_json_string(out: &mut String) {
    if let Some(start) = trailing_json_string_start(out) {
        out.truncate(start);
    }
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
                stop_details: None,
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
            let cached = usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0);
            self.partial.usage.cache_read = cached;
            // Anthropic convention (used across all transports): `usage.input`
            // EXCLUDES cache reads so that `input + cache_read` reconstructs the
            // full prompt. OpenAI-style APIs report `prompt_tokens` INCLUDING
            // cached tokens, so subtract the cached count (saturating to avoid
            // underflow if a provider ever reports cached > prompt). DeepSeek
            // exposes the miss count directly via `prompt_cache_miss_tokens`;
            // prefer it when present as a more robust source.
            self.partial.usage.input = usage
                .prompt_cache_miss_tokens
                .unwrap_or_else(|| usage.prompt_tokens.saturating_sub(cached));
            self.partial.usage.output = usage.completion_tokens.unwrap_or(0);
            self.partial.usage.total_tokens = usage.total_tokens;
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

                    // #129: the opening chunk carries the tool-call id and
                    // (usually) the function name — surface them on the start
                    // event so agent-side partials are correlatable from the
                    // first delta. The accumulation below remains the source
                    // of truth for the provider-side partial.
                    self.pending_events.push_back(StreamEvent::ToolCallStart {
                        content_index,
                        id: tc_delta.id.clone().unwrap_or_default(),
                        name: tc_delta
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default(),
                    });

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

                        // #124: keep the partial block's `arguments` growing as
                        // deltas arrive, so snapshot-based clients (RPC/ACP IDE
                        // frontends) render a large tool call as it streams
                        // instead of a pause then a pop-in. `complete_partial_json`
                        // best-effort closes the accumulated prefix into valid
                        // JSON; on an un-completable fragment it returns None and
                        // we keep the last good value (never wrong data). The
                        // terminal event still sets the fully-parsed arguments.
                        if let Some(partial_args) = complete_partial_json(&tc.arguments)
                            && let Some(ContentBlock::ToolCall(block)) =
                                self.partial.content.get_mut(content_index)
                        {
                            block.arguments = partial_args;
                        }

                        // The delta is still emitted for streaming consumers.
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
    /// DeepSeek-only thinking toggle (`{"type": "enabled" | "disabled"}`). Other
    /// OpenAI-compatible providers never set this, so it serializes away.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<OpenAIThinking>,
    /// DeepSeek-only reasoning effort (`"high"` | `"max"`). DeepSeek maps
    /// `low`/`medium` to `high` and `xhigh` to `max` itself, so we only emit the
    /// two values it documents.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct OpenAIStreamOptions {
    include_usage: bool,
}

/// DeepSeek's `thinking` request object on the chat-completions transport.
/// `{"type": "enabled"}` turns on thinking mode; `{"type": "disabled"}` forces
/// the non-thinking path. Serialized only for DeepSeek (see `ReasoningStyle`).
#[derive(Debug, Serialize)]
struct OpenAIThinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Request-side reasoning dialect for OpenAI-compatible providers that take
/// non-standard reasoning controls. The plain Chat Completions transport has no
/// reasoning toggle, so this is `None` for OpenAI/Groq/OpenRouter/etc. and the
/// emitted body is byte-for-byte unchanged for them. Kept as an enum so other
/// dialects (zai/qwen/openrouter "reasoning") can be added without touching the
/// `build_request` call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningStyle {
    /// DeepSeek: `thinking: {type: enabled|disabled}` + `reasoning_effort`
    /// (`high`|`max`). Mirrors the legacy `@earendil-works/pi-ai` `thinkingFormat`.
    DeepSeek,
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
    /// DeepSeek reports the cache-miss (uncached) prompt token count directly.
    /// When present it is the authoritative source for `usage.input`.
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
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
    use crate::auth::AuthCredential;
    use asupersync::runtime::RuntimeBuilder;
    use futures::{StreamExt, stream};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{Read, Write};

    /// #124: the partial-JSON completer gives snapshot clients live tool-call
    /// arguments. Each accumulated prefix must complete to a valid `Value` that
    /// faithfully reflects the content so far (closing open structure only),
    /// and an un-completable fragment yields `None` (caller keeps last value).
    #[test]
    fn complete_partial_json_streams_tool_call_arguments() {
        // Growing prefixes of `{"path": "src/lib.rs", "content": "hello"}`.
        assert_eq!(complete_partial_json(""), None);
        assert_eq!(complete_partial_json("{"), Some(json!({})));
        assert_eq!(
            complete_partial_json(r#"{"path": "src/li"#),
            Some(json!({"path": "src/li"}))
        );
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs""#),
            Some(json!({"path": "src/lib.rs"}))
        );
        // Complete member — must be kept, not stripped.
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs""#),
            Some(json!({"path": "src/lib.rs"}))
        );
        // Trailing comma dropped.
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs", "#),
            Some(json!({"path": "src/lib.rs"}))
        );
        // Dangling `"key":` (no value yet) dropped; complete member kept.
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs", "content":"#),
            Some(json!({"path": "src/lib.rs"}))
        );
        // Dangling bare key (no colon yet) dropped.
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs", "content"#),
            Some(json!({"path": "src/lib.rs"}))
        );
        // Complete document round-trips.
        assert_eq!(
            complete_partial_json(r#"{"path": "src/lib.rs", "content": "hello"}"#),
            Some(json!({"path": "src/lib.rs", "content": "hello"}))
        );
    }

    #[test]
    fn complete_partial_json_handles_arrays_escapes_and_unparseable() {
        // Open array + trailing comma.
        assert_eq!(
            complete_partial_json(r#"{"items": [1, 2, "#),
            Some(json!({"items": [1, 2]}))
        );
        assert_eq!(
            complete_partial_json(r#"{"items": [1, 2"#),
            Some(json!({"items": [1, 2]}))
        );
        // Escaped quote inside an open string is preserved.
        assert_eq!(
            complete_partial_json(r#"{"a": "b\"c"#),
            Some(json!({"a": "b\"c"}))
        );
        // Dangling escape backslash is dropped before closing.
        assert_eq!(
            complete_partial_json(r#"{"a": "bc\"#),
            Some(json!({"a": "bc"}))
        );
        // A partially-typed bare literal can't be closed safely → None.
        assert_eq!(complete_partial_json(r#"{"a": tr"#), None);
    }
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
    fn test_build_request_deepseek_forwards_thinking_and_reasoning_effort() {
        // Builds the serialized request body for a given thinking level.
        let body = |provider: &OpenAIProvider, level: crate::model::ThinkingLevel| {
            let context = Context {
                system_prompt: None,
                messages: vec![Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Solve it".to_string()),
                    timestamp: 0,
                })]
                .into(),
                tools: Vec::<ToolDef>::new().into(),
            };
            let options = StreamOptions {
                thinking_level: Some(level),
                ..Default::default()
            };
            serde_json::to_value(provider.build_request(&context, &options))
                .expect("serialize request")
        };

        // Detected via the provider id. `with_reasoning(true)` mirrors the
        // registry wiring for a DeepSeek reasoning model (deepseek-v4-pro).
        let ds = OpenAIProvider::new("deepseek-v4-pro")
            .with_provider_name("deepseek")
            .with_reasoning(true);

        let off = body(&ds, crate::model::ThinkingLevel::Off);
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(
            off.get("reasoning_effort").is_none(),
            "off must not send reasoning_effort"
        );

        let high = body(&ds, crate::model::ThinkingLevel::High);
        assert_eq!(high["thinking"]["type"], "enabled");
        assert_eq!(high["reasoning_effort"], "high");

        let xhigh = body(&ds, crate::model::ThinkingLevel::XHigh);
        assert_eq!(xhigh["thinking"]["type"], "enabled");
        assert_eq!(xhigh["reasoning_effort"], "max");

        // medium/low/minimal enable thinking but let DeepSeek pick the effort.
        let medium = body(&ds, crate::model::ThinkingLevel::Medium);
        assert_eq!(medium["thinking"]["type"], "enabled");
        assert!(medium.get("reasoning_effort").is_none());

        // Detected via the base URL even when the provider id is generic.
        let ds_by_url = OpenAIProvider::new("deepseek-v4-pro")
            .with_base_url("https://api.deepseek.com/v1/chat/completions".to_string())
            .with_reasoning(true);
        let high_url = body(&ds_by_url, crate::model::ThinkingLevel::High);
        assert_eq!(high_url["thinking"]["type"], "enabled");
        assert_eq!(high_url["reasoning_effort"], "high");
    }

    #[test]
    fn test_build_request_non_reasoning_deepseek_omits_thinking() {
        // A non-reasoning DeepSeek model (e.g. deepseek-chat) must NOT emit the
        // `thinking` toggle or `reasoning_effort`, even though the provider is
        // deepseek — pre-#113 wire behavior is preserved (gh #114, finding 2).
        let provider = OpenAIProvider::new("deepseek-chat")
            .with_provider_name("deepseek")
            .with_reasoning(false);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<ToolDef>::new().into(),
        };
        // Even at Off (the default/clamped level) there must be no thinking field.
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::Off),
            ..Default::default()
        };
        let value = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        assert!(value.get("thinking").is_none());
        assert!(value.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_build_request_deepseek_provider_alias_detected() {
        // The `deep-seek` provider alias canonicalizes to `deepseek`, so the
        // thinking dialect must fire for it too (gh #114, finding 3) — matching
        // `ModelEntry::is_deepseek_reasoning_model`'s `canonical_provider_id` use.
        let provider = OpenAIProvider::new("deepseek-v4-pro")
            .with_provider_name("deep-seek")
            .with_reasoning(true);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("solve it".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<ToolDef>::new().into(),
        };
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::XHigh),
            ..Default::default()
        };
        let value = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        assert_eq!(value["thinking"]["type"], "enabled");
        assert_eq!(value["reasoning_effort"], "max");
    }

    #[test]
    fn test_build_request_non_deepseek_omits_reasoning_controls() {
        let provider = OpenAIProvider::new("gpt-4o");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<ToolDef>::new().into(),
        };
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::High),
            ..Default::default()
        };
        let value = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        // A non-DeepSeek openai-completions provider serializes exactly as before:
        // no thinking toggle and no reasoning_effort regardless of thinking level.
        assert!(value.get("thinking").is_none());
        assert!(value.get("reasoning_effort").is_none());
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
    fn auth_header_false_sends_custom_openai_request_without_authorization() {
        let options = StreamOptions::default();
        let captured = run_stream_and_capture_headers_with(
            OpenAIProvider::new("custom-model")
                .with_provider_name("custom-openai")
                .with_auth_header(false),
            &options,
        )
        .expect("captured keyless custom request");

        assert!(
            !captured.headers.contains_key("authorization"),
            "authHeader:false must not synthesize Authorization"
        );
    }

    #[test]
    fn stream_error_redacts_transmitted_bearer_credential() {
        let secret = "provider-\"secret\\with&reserved=characters";
        let header_secret = "custom-\"header\\credential";
        let query_secret = "query-secret-value";
        let response_json = serde_json::json!({
            "authorizationEcho": secret,
            "customHeaderEcho": header_secret,
            "queryEcho": query_secret,
        })
        .to_string();
        let response_body = format!("upstream error: {response_json}");
        let (base_url, request_rx) = spawn_test_server(401, "application/json", &response_body);
        let provider = OpenAIProvider::new("gpt-4o")
            .with_base_url(format!("{base_url}?api_key={query_secret}"));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let mut options = StreamOptions {
            api_key: Some(secret.to_string()),
            ..Default::default()
        };
        options
            .headers
            .insert("x-api-key".to_string(), header_secret.to_string());
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let error = runtime.block_on(async {
            match provider.stream(&context, &options).await {
                Ok(_) => panic!("HTTP error must not produce a stream"),
                Err(error) => error,
            }
        });
        let message = error.to_string();
        assert!(message.contains("HTTP 401"), "{message}");
        assert!(message.contains("[REDACTED]"), "{message}");
        assert!(!message.contains(secret), "credential leaked: {message}");
        assert!(
            !message.contains("provider-"),
            "escaped credential leaked: {message}"
        );
        assert!(
            !message.contains("custom-"),
            "header credential leaked: {message}"
        );
        assert!(
            !message.contains(query_secret),
            "query credential leaked: {message}"
        );
        request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured error request");

        let expanding = redacted_api_error_body(&"x".repeat(MAX_API_ERROR_BODY_BYTES), &["x"]);
        assert!(
            expanding.len() <= MAX_API_ERROR_BODY_BYTES,
            "redaction expansion exceeded the final API error bound"
        );
    }

    #[test]
    fn sap_stream_exchanges_service_key_before_sending_bearer_header() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let auth_path = temp_dir.path().join("auth.json");
        let (token_url, token_request_rx) = spawn_test_server(
            200,
            "application/json",
            r#"{"access_token":"sap-access-token"}"#,
        );
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        let service_key_json = json!({
            "clientid": "sap-client",
            // ubs:ignore test fixture credential, not live secret.
            "clientsecret": "sap-secret",
            "url": token_url,
            "serviceurls": {
                "AI_API_URL": "https://api.ai.sap.example.com"
            }
        })
        .to_string();
        auth.set(
            "sap-ai-core",
            // Legacy auth files stored the complete service-key JSON in the generic ApiKey lane.
            // It must be exchanged, never forwarded as the bearer value.
            AuthCredential::ApiKey {
                key: service_key_json.clone(),
            },
        );
        auth.save().expect("save auth");

        let (inference_url, inference_request_rx) =
            spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIProvider::new("deployment-a")
            .with_provider_name("sap-ai-core")
            .with_base_url(inference_url)
            .with_auth_path(auth_path);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions::default();
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

        let token_request = token_request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured token request");
        assert!(token_request.body.contains("grant_type=client_credentials"));
        assert!(token_request.body.contains("client_id=sap-client"));

        let inference_request = inference_request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured inference request");
        let authorization = inference_request
            .headers
            .get("authorization")
            .expect("SAP inference authorization header");
        assert_eq!(
            authorization, "Bearer sap-access-token",
            "legacy service-key JSON must be exchanged before inference"
        );
        assert!(!authorization.contains(&service_key_json));
        assert!(!authorization.contains("sap-secret"));
        assert!(!inference_request.body.contains(&service_key_json));
        assert!(!inference_request.body.contains("sap-secret"));
    }

    #[test]
    fn sap_stream_uses_stored_direct_bearer_without_requiring_service_key() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let auth_path = temp_dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "sap-ai-core",
            AuthCredential::BearerToken {
                token: "stored-sap-bearer".to_string(),
            },
        );
        auth.save().expect("save auth");

        let (inference_url, inference_request_rx) =
            spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIProvider::new("deployment-a")
            .with_provider_name("sap-ai-core")
            .with_base_url(inference_url)
            .with_auth_path(auth_path);
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

        let inference_request = inference_request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured inference request");
        assert_eq!(
            inference_request
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer stored-sap-bearer")
        );
    }

    /// Drive `provider.stream()` once against `base_url` with no API key and
    /// return the `Result`. Keeps the request path deterministic and fast by
    /// pointing at an unroutable address so we observe the *auth decision*
    /// (key required vs. not) without depending on a full network round-trip.
    fn stream_result_without_key(
        provider_name: &str,
        base_url: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let provider = OpenAIProvider::new("local-model")
            .with_provider_name(provider_name)
            .with_base_url(base_url.to_string());
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
            api_key: None,
            ..Default::default()
        };
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async { provider.stream(&context, &options).await })
    }

    #[test]
    fn test_stream_keyless_local_provider_does_not_require_key() {
        // #104: local OpenAI-compatible providers (ollama, llamacpp, mistralrs)
        // need NO API key. With no api_key configured the request path must NOT
        // raise the "Missing API key for provider" error. We point at an
        // unroutable address so the call resolves quickly: it either starts the
        // stream or fails with a *connection* error — never the missing-key
        // error. (Skipped when OPENAI_API_KEY is ambient, which would satisfy
        // the key check for every provider and make the assertion vacuous.)
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return;
        }
        for provider in ["llamacpp", "mistralrs", "ollama"] {
            // 127.0.0.1:1 is reserved/unroutable, so connect fails fast.
            let result = stream_result_without_key(provider, "http://127.0.0.1:1/v1");
            if let Err(err) = result {
                assert!(
                    !err.to_string().contains("Missing API key"),
                    "{provider}: keyless local provider must not raise missing-key error, got: {err}"
                );
            }
        }
    }

    #[test]
    fn test_stream_unknown_provider_without_key_still_errors() {
        // Guard: the keyless bypass is scoped to known local providers. An
        // unknown provider with no key (and no ambient OPENAI_API_KEY) must
        // still fail with the missing-key error — and does so synchronously,
        // before any network I/O.
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return; // ambient key would satisfy the gate; skip in that env
        }
        let result =
            stream_result_without_key("totally-unknown-cloud-provider", "http://127.0.0.1:1/v1");
        let err = result.err().expect("missing key should error");
        assert!(
            err.to_string().contains("Missing API key"),
            "expected missing-key error, got: {err}"
        );
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
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
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

        /// Regression for #121: cached prompt tokens must not be double-counted.
        /// `prompt_tokens` includes cached tokens, so `usage.input` must exclude
        /// them (`prompt_tokens - cached_tokens`) while `cache_read` keeps the
        /// cached count, matching the Anthropic convention where
        /// `input + cache_read` reconstructs the full prompt.
        #[test]
        fn cache_heavy_usage_excludes_cache_reads_from_input() {
            let mut state = make_state();
            let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":172405,"completion_tokens":40,"total_tokens":172445,"prompt_tokens_details":{"cached_tokens":172288}}}"#;
            state.process_event(chunk).expect("process usage chunk");

            assert_eq!(state.partial.usage.input, 117);
            assert_eq!(state.partial.usage.cache_read, 172_288);
            assert_eq!(state.partial.usage.output, 40);
            assert_eq!(state.partial.usage.total_tokens, 172_445);
            // input + cache_read reconstructs the full prompt token count.
            assert_eq!(
                state.partial.usage.input + state.partial.usage.cache_read,
                172_405
            );
        }

        /// DeepSeek reports the cache-miss count directly; prefer it over
        /// subtraction as the authoritative source for `usage.input`.
        #[test]
        fn deepseek_prompt_cache_miss_tokens_is_authoritative_for_input() {
            let mut state = make_state();
            let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":20,"total_tokens":1020,"prompt_cache_miss_tokens":128,"prompt_tokens_details":{"cached_tokens":872}}}"#;
            state.process_event(chunk).expect("process usage chunk");

            assert_eq!(state.partial.usage.input, 128);
            assert_eq!(state.partial.usage.cache_read, 872);
        }

        /// Guard against underflow: if a provider ever reports more cached
        /// tokens than prompt tokens, `input` saturates to 0 rather than
        /// wrapping around.
        #[test]
        fn cached_greater_than_prompt_saturates_input_to_zero() {
            let mut state = make_state();
            let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":250}}}"#;
            state.process_event(chunk).expect("process usage chunk");

            assert_eq!(state.partial.usage.input, 0);
            assert_eq!(state.partial.usage.cache_read, 250);
        }

        /// No cache details: `input` equals `prompt_tokens` and `cache_read`
        /// stays zero (no regression for the common uncached case).
        #[test]
        fn usage_without_cache_details_maps_input_directly() {
            let mut state = make_state();
            let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":500,"completion_tokens":10,"total_tokens":510}}"#;
            state.process_event(chunk).expect("process usage chunk");

            assert_eq!(state.partial.usage.input, 500);
            assert_eq!(state.partial.usage.cache_read, 0);
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
