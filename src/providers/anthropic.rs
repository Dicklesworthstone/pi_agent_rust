//! Anthropic Messages API provider implementation.
//!
//! This module implements the Provider trait for the Anthropic Messages API,
//! supporting streaming responses, tool use, and extended thinking.

use crate::auth::unmark_anthropic_oauth_bearer_token;
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, RedactedThinkingContent, StopDetails, StopReason,
    StreamEvent, TextContent, ThinkingContent, ThinkingLevel, ToolCall, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{CacheRetention, Context, Provider, StreamOptions, ToolDef};
use crate::provider_metadata::canonical_provider_id;
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const ANTHROPIC_OAUTH_TOKEN_PREFIX: &str = "sk-ant-oat";
/// Beta flags added when using Anthropic OAuth bearer tokens.
/// Override via `PI_ANTHROPIC_BETA_FLAGS`.
const ANTHROPIC_OAUTH_BETA_FLAGS: &str = "claude-code-20250219,oauth-2025-04-20";
/// Beta flag for Anthropic prompt caching.
/// Override via `PI_ANTHROPIC_CACHE_BETA_FLAG`.
const ANTHROPIC_CACHE_BETA_FLAG: &str = "prompt-caching-2024-07-31";
const KIMI_SHARE_DIR_ENV_KEY: &str = "KIMI_SHARE_DIR";

fn anthropic_oauth_beta_flags() -> String {
    std::env::var("PI_ANTHROPIC_BETA_FLAGS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ANTHROPIC_OAUTH_BETA_FLAGS.to_string())
}

fn anthropic_cache_beta_flag() -> String {
    std::env::var("PI_ANTHROPIC_CACHE_BETA_FLAG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ANTHROPIC_CACHE_BETA_FLAG.to_string())
}

#[inline]
fn is_anthropic_oauth_token(token: &str) -> bool {
    token.contains(ANTHROPIC_OAUTH_TOKEN_PREFIX)
}

#[inline]
fn is_anthropic_provider(provider: &str) -> bool {
    canonical_provider_id(provider).unwrap_or(provider) == "anthropic"
}

#[inline]
fn is_anthropic_bearer_token(provider: &str, token: &str) -> bool {
    if !is_anthropic_provider(provider) {
        return false;
    }
    let token = token.trim();
    if token.is_empty() {
        return false;
    }

    // OAuth tokens use the Bearer lane.
    if is_anthropic_oauth_token(token) {
        return true;
    }

    // Legacy/external Claude credentials are bearer tokens and do not start with sk-ant.
    !token.starts_with("sk-ant-")
}

#[inline]
fn is_kimi_coding_provider(provider: &str) -> bool {
    canonical_provider_id(provider).unwrap_or(provider) == "kimi-for-coding"
}

#[inline]
fn is_kimi_oauth_token(provider: &str, token: &str) -> bool {
    is_kimi_coding_provider(provider) && !token.starts_with("sk-")
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

fn x_api_key_override(options: &StreamOptions, compat: Option<&CompatConfig>) -> Option<String> {
    super::first_non_empty_header_value_case_insensitive(&options.headers, &["x-api-key"]).or_else(
        || {
            compat
                .and_then(|compat| compat.custom_headers.as_ref())
                .and_then(|headers| {
                    super::first_non_empty_header_value_case_insensitive(headers, &["x-api-key"])
                })
        },
    )
}

fn sanitize_ascii_header_value(value: &str, fallback: &str) -> String {
    if value.is_ascii() && !value.trim().is_empty() {
        return value.to_string();
    }
    let sanitized = value
        .chars()
        .filter(char::is_ascii)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn home_dir_with_env_lookup<F>(env_lookup: F) -> Option<std::path::PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("HOME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env_lookup("USERPROFILE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
        })
        .or_else(|| {
            let drive = env_lookup("HOMEDRIVE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            let path = env_lookup("HOMEPATH")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            if path.starts_with('\\') || path.starts_with('/') {
                Some(std::path::PathBuf::from(format!("{drive}{path}")))
            } else {
                let mut combined = std::path::PathBuf::from(drive);
                combined.push(path);
                Some(combined)
            }
        })
}

fn home_dir() -> Option<std::path::PathBuf> {
    home_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_share_dir_with_env_lookup<F>(env_lookup: F) -> Option<std::path::PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup(KIMI_SHARE_DIR_ENV_KEY)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| home_dir_with_env_lookup(env_lookup).map(|home| home.join(".kimi")))
}

fn kimi_share_dir() -> Option<std::path::PathBuf> {
    kimi_share_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_device_id_paths() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let primary = kimi_share_dir()?.join("device_id");
    let legacy = home_dir().map_or_else(
        || primary.clone(),
        |home| home.join(".pi").join("agent").join("kimi-device-id"),
    );
    Some((primary, legacy))
}

fn kimi_device_id() -> String {
    static DEVICE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DEVICE_ID
        .get_or_init(|| {
            let generated = uuid::Uuid::new_v4().simple().to_string();
            let Some((primary, legacy)) = kimi_device_id_paths() else {
                return generated;
            };

            for path in [&primary, &legacy] {
                if let Ok(existing) = fs::read_to_string(path) {
                    let existing = existing.trim();
                    if !existing.is_empty() {
                        return existing.to_string();
                    }
                }
            }

            if let Some(parent) = primary.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);

            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            if let Ok(mut file) = options.open(&primary) {
                use std::io::Write;
                let _ = file.write_all(generated.as_bytes());
            }

            generated
        })
        .clone()
}

fn kimi_common_headers() -> Vec<(String, String)> {
    let device_name = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let device_model = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let os_version = std::env::consts::OS.to_string();

    vec![
        (
            "X-Msh-Platform".to_string(),
            sanitize_ascii_header_value("kimi_cli", "unknown"),
        ),
        (
            "X-Msh-Version".to_string(),
            sanitize_ascii_header_value(env!("CARGO_PKG_VERSION"), "unknown"),
        ),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize_ascii_header_value(&device_name, "unknown"),
        ),
        (
            "X-Msh-Device-Model".to_string(),
            sanitize_ascii_header_value(&device_model, "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize_ascii_header_value(&os_version, "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize_ascii_header_value(&kimi_device_id(), "unknown"),
        ),
    ]
}

// ============================================================================
// Anthropic Provider
// ============================================================================

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
    compat: Option<CompatConfig>,
}

/// Whether an Anthropic(-compatible) model on the `anthropic-messages`
/// transport uses the modern adaptive-thinking API (`thinking: {type:"adaptive"}`
/// plus `output_config.effort`) rather than the deprecated fixed-budget
/// extended-thinking API (`thinking: {type:"enabled", budget_tokens:N}`).
///
/// Returns `None` for non-Claude ids (e.g. MiniMax over the Anthropic-compatible
/// transport) so the caller keeps the legacy budget path. This is a *fallback*
/// heuristic only: `compat.forceAdaptiveThinking` from the catalog is
/// authoritative (gh #117), so newer models can be onboarded as data without a
/// code change here.
///
/// Adaptive thinking is GA on Claude Opus 4.6/4.7/4.8, Sonnet 4.6, and the
/// Claude Fable/Mythos (5.x) families; `budget_tokens` is rejected (400) on
/// Opus 4.7/4.8/Fable/Mythos and deprecated on Opus 4.6 / Sonnet 4.6. Older
/// models (Opus/Sonnet 4.5 and earlier, Claude 3.x, all Haiku) require
/// `budget_tokens`.
/// Ref: https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking
fn anthropic_model_uses_adaptive_thinking(model_id: &str) -> Option<bool> {
    const ADAPTIVE_PREFIXES: [&str; 8] = [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-fable-",
        "claude-mythos-",
    ];
    let lower = model_id.to_ascii_lowercase();
    let pos = lower.find("claude-")?;
    let id = &lower[pos..];
    Some(
        ADAPTIVE_PREFIXES
            .iter()
            .any(|prefix| id.starts_with(prefix)),
    )
}

/// Map pi's thinking level onto the Anthropic `output_config.effort` value for
/// adaptive-thinking models. A per-model `thinkingLevelMap` from the catalog
/// (gh #117) overrides the default mapping (e.g. mapping `xhigh -> max`),
/// keyed by the lowercase `ThinkingLevel` name.
///
/// Default mapping: `minimal`/`low -> "low"`, `medium -> "medium"`,
/// `high -> "high"`, `xhigh -> "xhigh"`, `max -> "max"` (Anthropic has no
/// `minimal` effort — it collapses to the nearest Anthropic tier). `off`
/// yields `None` (no effort emitted; thinking is not enabled for `off`).
fn anthropic_effort_for_level(
    level: ThinkingLevel,
    compat: Option<&CompatConfig>,
) -> Option<String> {
    if let Some(map) = compat.and_then(|c| c.thinking_level_map.as_ref())
        && let Some(mapped) = map.get(level.to_string().as_str())
    {
        return Some(mapped.clone());
    }
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low".to_string()),
        ThinkingLevel::Medium => Some("medium".to_string()),
        ThinkingLevel::High => Some("high".to_string()),
        ThinkingLevel::XHigh => Some("xhigh".to_string()),
        ThinkingLevel::Max => Some("max".to_string()),
    }
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: ANTHROPIC_API_URL.to_string(),
            provider: "anthropic".to_string(),
            compat: None,
        }
    }

    /// Override the provider name reported in streamed events.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
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
    ///
    /// Overrides are applied during request building (custom headers)
    /// and can be extended for Anthropic-specific quirks.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Build the request body for the Anthropic API.
    pub fn build_request<'a>(
        &'a self,
        context: &'a Context<'_>,
        options: &StreamOptions,
    ) -> AnthropicRequest<'a> {
        let messages = context
            .messages
            .iter()
            .map(convert_message_to_anthropic)
            .collect();

        let tools: Option<Vec<AnthropicTool<'_>>> = if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(convert_tool_to_anthropic)
                    .collect(),
            )
        };

        // Decide between the modern adaptive-thinking API and the legacy
        // fixed-budget extended-thinking API. Catalog metadata
        // (`compat.forceAdaptiveThinking`) is authoritative; the model-id
        // heuristic is consulted only when the catalog is silent (gh #116/#117).
        let adaptive = self
            .compat
            .as_ref()
            .and_then(|compat| compat.force_adaptive_thinking)
            .or_else(|| anthropic_model_uses_adaptive_thinking(&self.model))
            .unwrap_or(false);

        // Build thinking configuration if enabled.
        let level = options
            .thinking_level
            .filter(|level| *level != ThinkingLevel::Off);
        let (thinking, output_config) = match level {
            None => (None, None),
            Some(level) if adaptive => {
                // Modern path: adaptive thinking + effort. `budget_tokens` is
                // rejected (400) on Opus 4.7/4.8/Fable/Mythos and deprecated on
                // Opus 4.6 / Sonnet 4.6. `display: "summarized"` keeps pi's
                // thinking-stream visible (the default is "omitted" on the
                // newest models).
                let thinking = AnthropicThinking {
                    r#type: "adaptive",
                    budget_tokens: None,
                    display: Some("summarized"),
                };
                let output_config = AnthropicOutputConfig {
                    effort: anthropic_effort_for_level(level, self.compat.as_ref()),
                };
                (Some(thinking), Some(output_config))
            }
            Some(level) => {
                // Legacy path: fixed-budget extended thinking.
                let budget = options.thinking_budgets.as_ref().map_or_else(
                    || level.default_budget(),
                    |b| match level {
                        ThinkingLevel::Off => 0,
                        ThinkingLevel::Minimal => b.minimal,
                        ThinkingLevel::Low => b.low,
                        ThinkingLevel::Medium => b.medium,
                        ThinkingLevel::High => b.high,
                        ThinkingLevel::XHigh => b.xhigh,
                        ThinkingLevel::Max => b.max,
                    },
                );
                let thinking = AnthropicThinking {
                    r#type: "enabled",
                    budget_tokens: Some(budget),
                    display: None,
                };
                (Some(thinking), None)
            }
        };

        let mut max_tokens = options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        if let Some(budget) = thinking.as_ref().and_then(|t| t.budget_tokens)
            && max_tokens <= budget
        {
            max_tokens = budget + 4096;
        }

        // Adaptive-thinking models reject sampling params (`temperature`,
        // `top_p`, `top_k`) with a 400; legacy extended thinking requires
        // `temperature = 1.0`.
        let temperature = if adaptive {
            None
        } else if thinking.is_some() {
            Some(1.0)
        } else {
            options.temperature
        };

        AnthropicRequest {
            model: &self.model,
            messages,
            system: context.system_prompt.as_deref(),
            max_tokens,
            temperature,
            tools,
            stream: true,
            thinking,
            output_config,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "anthropic-messages"
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
        let request_body = self.build_request(context, options);
        let authorization_override = authorization_override(options, self.compat.as_ref());
        let x_api_key_override = x_api_key_override(options, self.compat.as_ref());
        let mut anthropic_bearer_token = false;
        let mut kimi_oauth_token = false;

        // Build request with headers (Content-Type set by .json() below)
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream")
            .header("anthropic-version", ANTHROPIC_API_VERSION);

        if let Some(authorization_override) = authorization_override {
            if let Some(bearer_token) =
                bearer_token_from_authorization_header(&authorization_override)
            {
                anthropic_bearer_token = is_anthropic_bearer_token(&self.provider, &bearer_token);
                kimi_oauth_token = is_kimi_oauth_token(&self.provider, &bearer_token);
            }
        } else if x_api_key_override.is_none() {
            let raw_auth_value = options
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                    .ok_or_else(|| {
                        Error::provider(
                            self.name(),
                            "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                        )
                    })?;
            let forced_bearer_token = if is_anthropic_provider(&self.provider) {
                unmark_anthropic_oauth_bearer_token(&raw_auth_value).map(ToString::to_string)
            } else {
                None
            };
            let force_bearer = forced_bearer_token.is_some();
            let auth_value = forced_bearer_token.unwrap_or(raw_auth_value);

            anthropic_bearer_token =
                force_bearer || is_anthropic_bearer_token(&self.provider, &auth_value);
            kimi_oauth_token = is_kimi_oauth_token(&self.provider, &auth_value);

            if anthropic_bearer_token || kimi_oauth_token {
                request = request.header("Authorization", format!("Bearer {auth_value}"));
            } else {
                request = request.header("X-API-Key", &auth_value);
            }
        }

        if anthropic_bearer_token {
            request = request
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("x-app", "cli")
                .header(
                    "user-agent",
                    format!(
                        "pi_agent_rust/{} (external, cli)",
                        env!("CARGO_PKG_VERSION")
                    ),
                );
        } else if kimi_oauth_token {
            request = request.header(
                "user-agent",
                format!(
                    "pi_agent_rust/{} (kimi-oauth, cli)",
                    env!("CARGO_PKG_VERSION")
                ),
            );
            for (name, value) in kimi_common_headers() {
                request = request.header(name, value);
            }
        }

        let mut beta_flags: Vec<String> = Vec::new();
        if anthropic_bearer_token {
            beta_flags.push(anthropic_oauth_beta_flags());
        }
        if options.cache_retention != CacheRetention::None {
            beta_flags.push(anthropic_cache_beta_flag());
        }
        if !beta_flags.is_empty() {
            request = request.header("anthropic-beta", beta_flags.join(","));
        }

        // Apply provider-specific custom headers from compat config.
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization", "x-api-key"],
            );
        }

        // Per-request headers from StreamOptions (highest priority).
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization", "x-api-key"],
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
                format!("Anthropic API error (HTTP {status}): {body}"),
            ));
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
                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            state.transient_error_count = 0;
                            if msg.event == "ping" {
                                // Skip ping events
                            } else {
                                match state.process_event(&msg.data) {
                                    Ok(Some(event)) => {
                                        if matches!(
                                            &event,
                                            StreamEvent::Done { .. } | StreamEvent::Error { .. }
                                        ) {
                                            state.done = true;
                                        }
                                        return Some((Ok(event), state));
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        state.done = true;
                                        return Some((Err(e), state));
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            // WriteZero, WouldBlock, and TimedOut errors are transient (e.g. empty SSE
                            // frames when TLS buffers are full). Skip them and
                            // keep reading, but cap consecutive occurrences to
                            // avoid infinite loops.
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
                        // Stream ended before message_stop (e.g.
                        // network disconnect).  Emit Done so the
                        // agent loop receives the partial message.
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

struct ToolAccum {
    id: String,
    name: String,
    json: String,
}

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    tool_accums: HashMap<u32, ToolAccum>,
    done: bool,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    const fn recompute_total_tokens(&mut self) {
        self.partial.usage.total_tokens = self
            .partial
            .usage
            .input
            .saturating_add(self.partial.usage.output)
            .saturating_add(self.partial.usage.cache_read)
            .saturating_add(self.partial.usage.cache_write);
    }

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
            tool_accums: HashMap::new(),
            done: false,
            transient_error_count: 0,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<Option<StreamEvent>> {
        let event: AnthropicStreamEvent = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                Ok(Some(self.handle_message_start(message)))
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => Ok(Some(self.handle_content_block_start(index, content_block))),
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                Ok(self.handle_content_block_delta(index, delta))
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                Ok(self.handle_content_block_stop(index))
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                self.handle_message_delta(&delta, usage)?;
                Ok(None)
            }
            AnthropicStreamEvent::MessageStop => {
                let reason = self.partial.stop_reason;
                Ok(Some(StreamEvent::Done {
                    reason,
                    message: std::mem::take(&mut self.partial),
                }))
            }
            AnthropicStreamEvent::Error { error } => {
                self.partial.stop_reason = StopReason::Error;
                self.partial.error_message = Some(error.message);
                Ok(Some(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: std::mem::take(&mut self.partial),
                }))
            }
            AnthropicStreamEvent::Ping => Ok(None),
        }
    }

    fn handle_message_start(&mut self, message: AnthropicMessageStart) -> StreamEvent {
        if let Some(usage) = message.usage {
            self.partial.usage.input = usage.input;
            self.partial.usage.cache_read = usage.cache_read.unwrap_or(0);
            self.partial.usage.cache_write = usage.cache_write.unwrap_or(0);
            self.recompute_total_tokens();
        }
        StreamEvent::Start {
            partial: self.partial.clone(),
        }
    }

    fn handle_content_block_start(
        &mut self,
        index: u32,
        content_block: AnthropicContentBlock,
    ) -> StreamEvent {
        let content_index = index as usize;

        match content_block {
            AnthropicContentBlock::Text => {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
                StreamEvent::TextStart { content_index }
            }
            AnthropicContentBlock::Thinking => {
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));
                StreamEvent::ThinkingStart { content_index }
            }
            AnthropicContentBlock::ToolUse { id, name } => {
                let id = id.unwrap_or_default();
                let name = name.unwrap_or_default();
                self.tool_accums.insert(
                    index,
                    ToolAccum {
                        id: id.clone(),
                        name: name.clone(),
                        json: String::new(),
                    },
                );
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                }));
                StreamEvent::ToolCallStart {
                    content_index,
                    id,
                    name,
                }
            }
            AnthropicContentBlock::RedactedThinking { data } => {
                // Redacted thinking arrives in a single `content_block_start` —
                // there are no follow-up `content_block_delta` events for the
                // opaque `data` payload. Push the block immediately and bracket
                // the stream with ThinkingStart so downstream UIs can render a
                // placeholder; ThinkingEnd with empty content fires from
                // `handle_content_block_stop`.
                self.partial.content.push(ContentBlock::RedactedThinking(
                    RedactedThinkingContent { data },
                ));
                StreamEvent::ThinkingStart { content_index }
            }
        }
    }

    fn handle_content_block_delta(
        &mut self,
        index: u32,
        delta: AnthropicDelta,
    ) -> Option<StreamEvent> {
        let idx = index as usize;

        match delta {
            AnthropicDelta::TextDelta { text } => {
                if let Some(text) = text {
                    if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(idx) {
                        t.text.push_str(&text);
                    }
                    Some(StreamEvent::TextDelta {
                        content_index: idx,
                        delta: text,
                    })
                } else {
                    None
                }
            }
            AnthropicDelta::ThinkingDelta { thinking } => {
                if let Some(thinking) = thinking {
                    if let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(idx) {
                        t.thinking.push_str(&thinking);
                    }
                    Some(StreamEvent::ThinkingDelta {
                        content_index: idx,
                        delta: thinking,
                    })
                } else {
                    None
                }
            }
            AnthropicDelta::InputJsonDelta { partial_json } => {
                if let Some(partial_json) = partial_json {
                    if let Some(accum) = self.tool_accums.get_mut(&index) {
                        accum.json.push_str(&partial_json);
                    }
                    Some(StreamEvent::ToolCallDelta {
                        content_index: idx,
                        delta: partial_json,
                    })
                } else {
                    None
                }
            }
            AnthropicDelta::SignatureDelta { signature } => {
                // The Anthropic API sends signature_delta for thinking blocks
                // to deliver the thinking_signature required for multi-turn
                // extended thinking conversations.
                if let Some(sig) = signature
                    && let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(idx)
                {
                    t.thinking_signature = Some(sig);
                }
                None
            }
        }
    }

    fn handle_content_block_stop(&mut self, index: u32) -> Option<StreamEvent> {
        let idx = index as usize;

        match self.partial.content.get_mut(idx) {
            Some(ContentBlock::Text(t)) => {
                // Clone the accumulated text from the partial for the TextEnd event.
                // The partial keeps its text intact for the Done message
                // (finalize_assistant_message replaces the agent's accumulated message).
                let content = t.text.clone();
                Some(StreamEvent::TextEnd {
                    content_index: idx,
                    content,
                })
            }
            Some(ContentBlock::Thinking(t)) => {
                // Clone the accumulated thinking from the partial for the ThinkingEnd event.
                let content = t.thinking.clone();
                Some(StreamEvent::ThinkingEnd {
                    content_index: idx,
                    content,
                })
            }
            Some(ContentBlock::ToolCall(tc)) => {
                if let Some(accum) = self.tool_accums.remove(&index) {
                    let arguments: serde_json::Value = match serde_json::from_str(&accum.json) {
                        Ok(args) => args,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                raw = %accum.json,
                                "Failed to parse tool arguments as JSON"
                            );
                            serde_json::Value::Null
                        }
                    };
                    let tool_call = ToolCall {
                        id: accum.id,
                        name: accum.name,
                        arguments: arguments.clone(),
                        thought_signature: None,
                    };
                    tc.arguments = arguments;

                    Some(StreamEvent::ToolCallEnd {
                        content_index: idx,
                        tool_call,
                    })
                } else {
                    None
                }
            }
            Some(ContentBlock::RedactedThinking(_)) => Some(StreamEvent::ThinkingEnd {
                content_index: idx,
                content: String::new(),
            }),
            _ => None,
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    fn handle_message_delta(
        &mut self,
        delta: &AnthropicMessageDelta,
        usage: Option<AnthropicDeltaUsage>,
    ) -> Result<()> {
        if let Some(stop_reason) = delta.stop_reason.as_deref() {
            self.partial.stop_reason = match stop_reason {
                "end_turn" | "stop_sequence" => StopReason::Stop,
                // Our normalized model has one truncation outcome; Anthropic's
                // context-window stop is therefore intentionally represented as
                // `Length`, alongside ordinary output-token exhaustion.
                "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                "tool_use" => StopReason::ToolUse,
                "pause_turn" => StopReason::PauseTurn,
                "refusal" => StopReason::Refusal,
                unknown => {
                    return Err(Error::provider(
                        "anthropic",
                        format!("unsupported Anthropic stop_reason `{unknown}`"),
                    ));
                }
            };
            self.partial.stop_details.clone_from(&delta.stop_details);
        }

        if let Some(u) = usage {
            self.partial.usage.output = u.output_tokens;
            self.recompute_total_tokens();
        }

        Ok(())
    }
}

// ============================================================================
// Anthropic API Types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct AnthropicRequest<'a> {
    model: &'a str,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool<'a>>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

/// Thinking configuration. Two shapes share this struct:
/// - Legacy fixed-budget extended thinking: `{"type":"enabled","budget_tokens":N}`.
/// - Modern adaptive thinking: `{"type":"adaptive","display":"summarized"}`.
///
/// `budget_tokens` is rejected (400) on Opus 4.7/4.8/Fable/Mythos and deprecated
/// on Opus 4.6 / Sonnet 4.6; adaptive thinking + `output_config.effort` is the
/// modern path (gh #116).
#[derive(Debug, Serialize)]
struct AnthropicThinking {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<&'static str>,
}

/// `output_config` carries the modern `effort` control for adaptive-thinking
/// models (`low`/`medium`/`high`/`xhigh`/`max`). `high` is the API default.
#[derive(Debug, Serialize)]
struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage<'a> {
    role: &'static str,
    content: Vec<AnthropicContent<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    Image {
        source: AnthropicImageSource<'a>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: Vec<AnthropicToolResultContent<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicImageSource<'a> {
    r#type: &'static str,
    media_type: &'a str,
    data: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolResultContent<'a> {
    Text { text: &'a str },
    Image { source: AnthropicImageSource<'a> },
}

#[derive(Debug, Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicDeltaUsage>,
    },
    MessageStop,
    Error {
        error: AnthropicError,
    },
    Ping,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageStart {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

/// Usage statistics from Anthropic API.
/// Field names match the API response format.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct AnthropicUsage {
    #[serde(rename = "input_tokens")]
    input: u64,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: Option<u64>,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_write: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDeltaUsage {
    output_tokens: u64,
}

/// Content block type from `content_block_start`.
///
/// Using a tagged enum avoids allocating a `String` for the type field.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text,
    Thinking,
    /// Anthropic's safety pipeline emits this when upstream reasoning is
    /// hidden; the `data` field is opaque and must be preserved on the partial
    /// so a faithful round-trip is possible. OpenRouter's Anthropic-compatible
    /// relay forwards it verbatim, so any pi run that fails over to OpenRouter
    /// will see this variant.
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    ToolUse {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
}

/// Per-token delta from the Anthropic streaming API.
///
/// Using a tagged enum instead of a flat struct with `r#type: String` avoids
/// allocating a `String` for the type discriminant on every content_block_delta
/// event (the hottest path — one allocation per streamed token).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Variant names mirror Anthropic API type discriminants
enum AnthropicDelta {
    TextDelta {
        #[serde(default)]
        text: Option<String>,
    },
    ThinkingDelta {
        #[serde(default)]
        thinking: Option<String>,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: Option<String>,
    },
    SignatureDelta {
        #[serde(default)]
        signature: Option<String>,
    },
}

/// Stop reason from `message_delta`.
///
#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<StopDetails>,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    message: String,
}

// ============================================================================
// Conversion Functions
// ============================================================================

fn convert_message_to_anthropic(message: &Message) -> AnthropicMessage<'_> {
    match message {
        Message::User(user) => AnthropicMessage {
            role: "user",
            content: convert_user_content(&user.content),
        },
        Message::Custom(custom) => AnthropicMessage {
            role: "user",
            content: vec![AnthropicContent::Text {
                text: &custom.content,
            }],
        },
        Message::Assistant(assistant) => AnthropicMessage {
            role: "assistant",
            content: assistant
                .content
                .iter()
                .filter_map(convert_content_block_to_anthropic)
                .collect(),
        },
        Message::ToolResult(result) => AnthropicMessage {
            role: "user",
            content: vec![AnthropicContent::ToolResult {
                tool_use_id: &result.tool_call_id,
                content: result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(t) => {
                            Some(AnthropicToolResultContent::Text { text: &t.text })
                        }
                        ContentBlock::Image(img) => Some(AnthropicToolResultContent::Image {
                            source: AnthropicImageSource {
                                r#type: "base64",
                                media_type: &img.mime_type,
                                data: &img.data,
                            },
                        }),
                        _ => None,
                    })
                    .collect(),
                is_error: if result.is_error { Some(true) } else { None },
            }],
        },
    }
}

fn convert_user_content(content: &UserContent) -> Vec<AnthropicContent<'_>> {
    match content {
        UserContent::Text(text) => vec![AnthropicContent::Text { text }],
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(AnthropicContent::Text { text: &t.text }),
                ContentBlock::Image(img) => Some(AnthropicContent::Image {
                    source: AnthropicImageSource {
                        r#type: "base64",
                        media_type: &img.mime_type,
                        data: &img.data,
                    },
                }),
                _ => None,
            })
            .collect(),
    }
}

fn convert_content_block_to_anthropic(block: &ContentBlock) -> Option<AnthropicContent<'_>> {
    match block {
        ContentBlock::Text(t) => Some(AnthropicContent::Text { text: &t.text }),
        ContentBlock::ToolCall(tc) => Some(AnthropicContent::ToolUse {
            id: &tc.id,
            name: &tc.name,
            input: &tc.arguments,
        }),
        // Thinking blocks must be echoed back with their signature for
        // multi-turn extended thinking.  Skip blocks without a signature
        // (the API would reject them).
        ContentBlock::Thinking(t) => {
            t.thinking_signature
                .as_ref()
                .map(|sig| AnthropicContent::Thinking {
                    thinking: &t.thinking,
                    signature: sig,
                })
        }
        // Images and redacted-thinking markers don't round-trip back to the
        // Anthropic input surface — Anthropic discards redacted-thinking on
        // subsequent turns, so we do the same.
        ContentBlock::Image(_) | ContentBlock::RedactedThinking(_) => None,
    }
}

fn convert_tool_to_anthropic(tool: &ToolDef) -> AnthropicTool<'_> {
    AnthropicTool {
        name: &tool.name,
        description: &tool.description,
        input_schema: &tool.parameters,
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
    use serde_json::Value;
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn home_dir_lookup_falls_back_to_userprofile() {
        let home = home_dir_with_env_lookup(|key| match key {
            "USERPROFILE" => Some("C:\\Users\\Ada".to_string()),
            _ => None,
        });

        assert_eq!(home, Some(PathBuf::from("C:\\Users\\Ada")));
    }

    #[test]
    fn home_dir_lookup_falls_back_to_homedrive_homepath() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOMEDRIVE" => Some("D:".to_string()),
            "HOMEPATH" => Some("\\Users\\Grace".to_string()),
            _ => None,
        });

        assert_eq!(home, Some(PathBuf::from("D:\\Users\\Grace")));
    }

    #[test]
    fn test_convert_user_text_message() {
        let message = Message::User(crate::model::UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        });

        let converted = convert_message_to_anthropic(&message);
        assert_eq!(converted.role, "user");
        assert_eq!(converted.content.len(), 1);
    }

    #[test]
    fn test_thinking_budget() {
        assert_eq!(ThinkingLevel::Minimal.default_budget(), 1024);
        assert_eq!(ThinkingLevel::Low.default_budget(), 2048);
        assert_eq!(ThinkingLevel::Medium.default_budget(), 8192);
        assert_eq!(ThinkingLevel::High.default_budget(), 16384);
    }

    #[test]
    fn test_build_request_includes_system_tools_and_thinking() {
        let provider = AnthropicProvider::new("claude-test");
        let context = Context {
            system_prompt: Some("System prompt".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: vec![ToolDef {
                name: "echo".to_string(),
                description: "Echo a string.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    },
                    "required": ["text"]
                }),
            }]
            .into(),
        };
        let options = StreamOptions {
            max_tokens: Some(128),
            temperature: Some(0.2),
            thinking_level: Some(ThinkingLevel::Medium),
            thinking_budgets: Some(crate::provider::ThinkingBudgets {
                minimal: 1024,
                low: 2048,
                medium: 9000,
                high: 16384,
                xhigh: 32768,
                max: 65536,
            }),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        assert_eq!(request.model, "claude-test");
        assert_eq!(request.system, Some("System prompt"));
        assert_eq!(request.temperature, Some(1.0)); // thinking forces temperature to 1.0
        assert!(request.stream);
        assert_eq!(request.max_tokens, 13_096);

        let thinking = request.thinking.expect("thinking config");
        assert_eq!(thinking.r#type, "enabled");
        assert_eq!(thinking.budget_tokens, Some(9000));
        assert!(thinking.display.is_none());
        // Legacy (non-adaptive) model: no modern effort knob.
        assert!(request.output_config.is_none());

        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content.len(), 1);
        match &request.messages[0].content[0] {
            AnthropicContent::Text { text } => assert_eq!(*text, "Ping"),
            other => panic!(),
        }

        let tools = request.tools.expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo a string.");
        assert_eq!(
            *tools[0].input_schema,
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            })
        );
    }

    #[test]
    fn test_build_request_omits_optional_fields_by_default() {
        let provider = AnthropicProvider::new("claude-test");
        let context = Context::default();
        let options = StreamOptions::default();

        let request = provider.build_request(&context, &options);
        assert_eq!(request.model, "claude-test");
        assert_eq!(request.system, None);
        assert!(request.tools.is_none());
        assert!(request.thinking.is_none());
        assert!(request.output_config.is_none());
        assert_eq!(request.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(request.stream);
    }

    /// gh #116: an adaptive-thinking Anthropic model must emit the modern
    /// `thinking: {type:"adaptive", display:"summarized"}` + `output_config.effort`
    /// wire shape at every thinking level — never the deprecated `budget_tokens`
    /// — and must omit `temperature` (sampling params are rejected on these
    /// models). This asserts the exact serialized JSON (the proof).
    #[test]
    fn test_build_request_adaptive_thinking_effort_wire_format() {
        let provider = AnthropicProvider::new("claude-opus-4-8");

        let cases = [
            (ThinkingLevel::Minimal, "low"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::XHigh, "xhigh"),
        ];
        for (level, effort) in cases {
            let options = StreamOptions {
                max_tokens: Some(16_000),
                temperature: Some(0.7),
                thinking_level: Some(level),
                ..Default::default()
            };
            let body = serde_json::to_value(provider.build_request(&Context::default(), &options))
                .expect("serialize request");
            assert_eq!(
                body["thinking"],
                json!({ "type": "adaptive", "display": "summarized" }),
                "level {level:?}: adaptive thinking wire shape"
            );
            assert_eq!(
                body["output_config"],
                json!({ "effort": effort }),
                "level {level:?}: output_config.effort"
            );
            assert!(
                body["thinking"].get("budget_tokens").is_none(),
                "level {level:?}: adaptive must not emit deprecated budget_tokens"
            );
            assert!(
                body.get("temperature").is_none(),
                "level {level:?}: adaptive models reject temperature"
            );
        }

        // `off`: no thinking, no effort, still no temperature on adaptive models.
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::Off),
            temperature: Some(0.7),
            ..Default::default()
        };
        let body = serde_json::to_value(provider.build_request(&Context::default(), &options))
            .expect("serialize request");
        assert!(body.get("thinking").is_none(), "off: no thinking");
        assert!(body.get("output_config").is_none(), "off: no output_config");
        assert!(body.get("temperature").is_none(), "off: no temperature");
    }

    /// Pre-adaptive Anthropic(-compatible) models keep the legacy fixed-budget
    /// extended-thinking path (`thinking: {type:"enabled", budget_tokens}`) with
    /// `temperature = 1.0` and no `output_config`. MiniMax (Anthropic-compatible,
    /// non-Claude id) must also stay on the budget path.
    #[test]
    fn test_build_request_legacy_model_keeps_budget_tokens() {
        for model in [
            "claude-opus-4-5",
            "claude-3-5-sonnet-20241022",
            "claude-sonnet-4-5",
            "MiniMax-M3",
        ] {
            let provider = AnthropicProvider::new(model);
            let options = StreamOptions {
                max_tokens: Some(16_000),
                thinking_level: Some(ThinkingLevel::High),
                ..Default::default()
            };
            let context = Context::default();
            let request = provider.build_request(&context, &options);
            let thinking = request.thinking.as_ref().expect("thinking config");
            assert_eq!(thinking.r#type, "enabled", "{model}: legacy budget path");
            assert!(
                thinking.budget_tokens.is_some(),
                "{model}: budget_tokens set"
            );
            assert!(thinking.display.is_none(), "{model}: no display field");
            assert!(request.output_config.is_none(), "{model}: no output_config");
            assert_eq!(request.temperature, Some(1.0), "{model}: temperature 1.0");
        }
    }

    /// gh #117: catalog metadata is authoritative. `forceAdaptiveThinking`
    /// overrides the built-in model-id heuristic in both directions.
    #[test]
    fn test_build_request_force_adaptive_thinking_overrides_heuristic() {
        // true: an id the heuristic does NOT recognize still goes adaptive.
        let compat = CompatConfig {
            force_adaptive_thinking: Some(true),
            ..CompatConfig::default()
        };
        let provider = AnthropicProvider::new("some-new-anthropic-model").with_compat(Some(compat));
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let context = Context::default();
        let request = provider.build_request(&context, &options);
        let thinking = request.thinking.as_ref().expect("thinking config");
        assert_eq!(thinking.r#type, "adaptive");
        assert_eq!(thinking.display, Some("summarized"));
        assert!(thinking.budget_tokens.is_none());
        assert_eq!(
            request
                .output_config
                .as_ref()
                .and_then(|c| c.effort.as_deref()),
            Some("high")
        );
        assert!(request.temperature.is_none());

        // false: an id the heuristic WOULD treat as adaptive is forced legacy.
        let compat = CompatConfig {
            force_adaptive_thinking: Some(false),
            ..CompatConfig::default()
        };
        let provider = AnthropicProvider::new("claude-opus-4-8").with_compat(Some(compat));
        let context = Context::default();
        let request = provider.build_request(&context, &options);
        assert_eq!(
            request.thinking.as_ref().expect("thinking config").r#type,
            "enabled"
        );
        assert!(request.output_config.is_none());
    }

    /// gh #117: a per-model `thinkingLevelMap` remaps the effort value
    /// (e.g. `xhigh -> max`), overriding the transport's default mapping.
    #[test]
    fn test_build_request_thinking_level_map_overrides_effort() {
        let mut map = HashMap::new();
        map.insert("xhigh".to_string(), "max".to_string());
        let compat = CompatConfig {
            thinking_level_map: Some(map),
            ..CompatConfig::default()
        };
        let provider = AnthropicProvider::new("claude-opus-4-8").with_compat(Some(compat));
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::XHigh),
            ..Default::default()
        };
        let context = Context::default();
        let request = provider.build_request(&context, &options);
        assert_eq!(
            request
                .output_config
                .as_ref()
                .and_then(|c| c.effort.as_deref()),
            Some("max"),
            "thinkingLevelMap xhigh->max must win over the default xhigh"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_stream_parses_thinking_and_tool_call_events() {
        let events = vec![
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 3 } }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "thinking" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "thinking_delta", "thinking": "step 1" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 0
            }),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": { "type": "tool_use", "id": "tool_123", "name": "search" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"ru" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "st\"}" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 1
            }),
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": { "type": "text" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": { "type": "text_delta", "text": "done" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 2
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 5 }
            }),
            json!({
                "type": "message_stop"
            }),
        ];

        let out = collect_events(&events);
        assert_eq!(out.len(), 12, "expected full stream event sequence");

        assert!(matches!(&out[0], StreamEvent::Start { .. }));
        assert!(matches!(
            &out[1],
            StreamEvent::ThinkingStart {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            &out[2],
            StreamEvent::ThinkingDelta {
                content_index: 0,
                delta,
                ..
            } if delta == "step 1"
        ));
        assert!(matches!(
            &out[3],
            StreamEvent::ThinkingEnd {
                content_index: 0,
                content,
                ..
            } if content == "step 1"
        ));
        assert!(matches!(
            &out[4],
            StreamEvent::ToolCallStart {
                content_index: 1,
                ..
            }
        ));
        assert!(matches!(
            &out[5],
            StreamEvent::ToolCallDelta {
                content_index: 1,
                delta,
                ..
            } if delta == "{\"q\":\"ru"
        ));
        assert!(matches!(
            &out[6],
            StreamEvent::ToolCallDelta {
                content_index: 1,
                delta,
                ..
            } if delta == "st\"}"
        ));
        if let StreamEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } = &out[7]
        {
            assert_eq!(*content_index, 1);
            assert_eq!(tool_call.id, "tool_123");
            assert_eq!(tool_call.name, "search");
            assert_eq!(tool_call.arguments, json!({ "q": "rust" }));
        } else {
            panic!();
        }
        assert!(matches!(
            &out[8],
            StreamEvent::TextStart {
                content_index: 2,
                ..
            }
        ));
        assert!(matches!(
            &out[9],
            StreamEvent::TextDelta {
                content_index: 2,
                delta,
                ..
            } if delta == "done"
        ));
        assert!(matches!(
            &out[10],
            StreamEvent::TextEnd {
                content_index: 2,
                content,
                ..
            } if content == "done"
        ));
        if let StreamEvent::Done { reason, message } = &out[11] {
            assert_eq!(*reason, StopReason::ToolUse);
            assert_eq!(message.stop_reason, StopReason::ToolUse);
        } else {
            panic!();
        }
    }

    #[test]
    fn test_message_delta_sets_length_stop_reason_and_usage() {
        let events = vec![
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 5 } }
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "max_tokens" },
                "usage": { "output_tokens": 7 }
            }),
            json!({
                "type": "message_stop"
            }),
        ];

        let out = collect_events(&events);
        assert_eq!(out.len(), 2);
        if let StreamEvent::Done { reason, message } = &out[1] {
            assert_eq!(*reason, StopReason::Length);
            assert_eq!(message.stop_reason, StopReason::Length);
            assert_eq!(message.usage.input, 5);
            assert_eq!(message.usage.output, 7);
            assert_eq!(message.usage.total_tokens, 12);
        } else {
            panic!();
        }
    }

    #[test]
    fn all_stable_anthropic_stop_reasons_are_handled() {
        let cases = [
            ("end_turn", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("stop_sequence", StopReason::Stop),
            ("tool_use", StopReason::ToolUse),
            ("pause_turn", StopReason::PauseTurn),
            ("refusal", StopReason::Refusal),
            ("model_context_window_exceeded", StopReason::Length),
        ];

        for (wire_reason, expected_reason) in cases {
            let mut delta = json!({ "stop_reason": wire_reason });
            if wire_reason == "refusal" {
                delta["stop_details"] = json!({
                    "type": "refusal",
                    "category": "cyber",
                    "explanation": "The request could enable cyber harm."
                });
            }
            let events = vec![
                json!({
                    "type": "message_start",
                    "message": { "usage": { "input_tokens": 5 } }
                }),
                json!({
                    "type": "message_delta",
                    "delta": delta,
                    "usage": { "output_tokens": 7 }
                }),
                json!({ "type": "message_stop" }),
            ];

            let out = collect_events(&events);
            assert_eq!(out.len(), 2, "{wire_reason}");
            let StreamEvent::Done { reason, message } = &out[1] else {
                panic!("expected terminal Done event for {wire_reason}");
            };
            assert_eq!(*reason, expected_reason, "{wire_reason}");
            assert_eq!(message.stop_reason, expected_reason, "{wire_reason}");
            assert_eq!(message.usage.total_tokens, 12, "{wire_reason}");

            if wire_reason == "refusal" {
                assert_eq!(
                    message.stop_details.as_ref().map(|details| (
                        details.kind.as_str(),
                        details.category.as_deref(),
                        details.explanation.as_deref(),
                    )),
                    Some((
                        "refusal",
                        Some("cyber"),
                        Some("The request could enable cyber harm."),
                    )),
                );
            } else {
                assert!(message.stop_details.is_none(), "{wire_reason}");
            }
        }
    }

    #[test]
    fn unknown_anthropic_stop_reason_is_an_explicit_protocol_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let byte_stream = stream::iter(vec![Ok::<_, std::io::Error>(
                b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"future_reason\"}}\n\n"
                    .to_vec(),
            )]);
            let mut state = StreamState::new(
                crate::sse::SseStream::new(Box::pin(byte_stream)),
                "claude-test".to_string(),
                "anthropic-messages".to_string(),
                "anthropic".to_string(),
            );
            let event = state
                .event_source
                .next()
                .await
                .expect("event")
                .expect("SSE event");
            let error = state
                .process_event(&event.data)
                .expect_err("unknown stop reason must fail the protocol");
            assert!(
                error
                    .to_string()
                    .contains("unsupported Anthropic stop_reason `future_reason`"),
                "unexpected error: {error}"
            );
        });
    }

    #[test]
    fn test_usage_total_tokens_saturates_on_large_values() {
        let events = vec![
            json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": u64::MAX,
                        "cache_read_input_tokens": 1,
                        "cache_creation_input_tokens": 1
                    }
                }
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 1 }
            }),
            json!({
                "type": "message_stop"
            }),
        ];

        let out = collect_events(&events);
        assert_eq!(out.len(), 2);
        if let StreamEvent::Done { message, .. } = &out[1] {
            assert_eq!(message.usage.total_tokens, u64::MAX);
        } else {
            panic!();
        }
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
        let fixture = load_fixture("anthropic_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case {}", case.name);
        }
    }

    #[test]
    fn test_stream_error_event_maps_to_stop_reason_error() {
        let events = vec![json!({
            "type": "error",
            "error": { "message": "nope" }
        })];

        let out = collect_events(&events);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], StreamEvent::Error { .. }),
            "expected StreamEvent::Error, got {:?}",
            out[0]
        );
        if let StreamEvent::Error { reason, error } = &out[0] {
            assert_eq!(*reason, StopReason::Error);
            assert_eq!(error.stop_reason, StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("nope"));
        }
    }

    #[test]
    fn test_stream_emits_single_done_when_transport_ends_after_message_stop() {
        let out = collect_stream_items_from_body(&success_sse_body());
        let done_count = out
            .iter()
            .filter(|item| matches!(item, Ok(StreamEvent::Done { .. })))
            .count();
        assert_eq!(done_count, 1, "expected exactly one terminal Done event");
    }

    #[test]
    fn test_stream_error_event_is_terminal() {
        let body = [
            r#"data: {"type":"error","error":{"message":"boom"}}"#,
            "",
            // If the stream keeps running after Error, this would produce Done.
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert_eq!(out.len(), 1, "Error should terminate the stream");
        assert!(matches!(out[0], Ok(StreamEvent::Error { .. })));
    }

    #[test]
    fn test_stream_parse_error_is_terminal() {
        let body = [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            "",
            r"data: {invalid-json}",
            "",
            // This should not be emitted after parse error.
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert_eq!(out.len(), 2, "parse error should stop further events");
        assert!(matches!(out[0], Ok(StreamEvent::Start { .. })));
        match &out[1] {
            Ok(event) => panic!(),
            Err(err) => assert!(err.to_string().contains("JSON parse error")),
        }
    }

    #[test]
    fn test_stream_fragmented_sse_transport_preserves_text_delta_order() {
        let response_parts = vec![
            "seg-00|".to_string(),
            "seg-01|".to_string(),
            "seg-02|".to_string(),
            "seg-03|".to_string(),
            "seg-04|".to_string(),
            "seg-05|".to_string(),
            "seg-06|".to_string(),
            "seg-07|".to_string(),
            "seg-08|".to_string(),
            "seg-09|".to_string(),
            "seg-10|".to_string(),
            "seg-11|".to_string(),
        ];
        let expected_text = response_parts.concat();
        let part_refs = response_parts
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let frames = build_text_stream_sse_frames(&part_refs);
        let chunks = split_ascii_stream_bytes(&frames, &[1, 2, 5, 3, 8, 13, 21]);
        let out = collect_events_from_byte_chunks(chunks);

        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(
            out.get(1),
            Some(StreamEvent::TextStart {
                content_index: 0,
                ..
            })
        ));

        let deltas = collect_text_deltas(&out);
        assert_eq!(deltas, response_parts);
        assert_eq!(deltas.concat(), expected_text);

        let final_text = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::TextEnd { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("text_end event");
        assert_eq!(final_text, expected_text);

        let done_count = out
            .iter()
            .filter(|event| matches!(event, StreamEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1, "expected exactly one Done event");

        match out.last() {
            Some(StreamEvent::Done { reason, message }) => {
                assert_eq!(*reason, StopReason::Stop);
                assert_eq!(message.stop_reason, StopReason::Stop);
            }
            other => panic!("expected final Done event, got {other:?}"),
        }
    }

    #[test]
    fn test_stream_high_volume_fragmented_sse_preserves_delta_count_and_content() {
        let response_parts = (0..128)
            .map(|idx| format!("chunk-{idx:03}|"))
            .collect::<Vec<_>>();
        let expected_text = response_parts.concat();
        let part_refs = response_parts
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let frames = build_text_stream_sse_frames(&part_refs);
        let chunks = split_ascii_stream_bytes(&frames, &[1, 1, 2, 3, 5, 8, 13, 21, 34]);
        let out = collect_events_from_byte_chunks(chunks);
        let deltas = collect_text_deltas(&out);

        assert_eq!(
            deltas.len(),
            response_parts.len(),
            "expected one TextDelta per text fragment"
        );
        assert_eq!(deltas, response_parts);
        assert_eq!(deltas.concat(), expected_text);

        let final_text = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::TextEnd { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("text_end event");
        assert_eq!(final_text, expected_text);
    }

    #[test]
    fn test_stream_sets_required_headers() {
        let captured = run_stream_and_capture_headers(CacheRetention::None)
            .expect("captured request for required headers");
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("sk-ant-test-key")
        );
        assert_eq!(
            captured
                .headers
                .get("anthropic-version")
                .map(String::as_str),
            Some(ANTHROPIC_API_VERSION)
        );
        assert!(!captured.headers.contains_key("anthropic-beta"));
        assert!(captured.body.contains("\"stream\":true"));
    }

    #[test]
    fn test_stream_adds_prompt_caching_beta_header_when_enabled() {
        let captured = run_stream_and_capture_headers(CacheRetention::Short)
            .expect("captured request for beta header");
        assert_eq!(
            captured.headers.get("anthropic-beta").map(String::as_str),
            Some("prompt-caching-2024-07-31")
        );
    }

    #[test]
    fn test_stream_uses_oauth_bearer_auth_headers() {
        let captured =
            run_stream_and_capture_headers_with_api_key(CacheRetention::None, "sk-ant-oat-test")
                .expect("captured request for oauth headers");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer sk-ant-oat-test")
        );
        assert!(!captured.headers.contains_key("x-api-key"));
        assert_eq!(
            captured
                .headers
                .get("anthropic-dangerous-direct-browser-access")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            captured.headers.get("x-app").map(String::as_str),
            Some("cli")
        );
        assert!(
            captured
                .headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("oauth-2025-04-20"))
        );
        assert!(
            captured
                .headers
                .get("user-agent")
                .is_some_and(|value| value.contains("pi_agent_rust/"))
        );
    }

    #[test]
    fn test_stream_uses_bearer_headers_for_marked_anthropic_oauth_token() {
        let marked = "__pi_anthropic_oauth_bearer__:sk-ant-api-like-token";
        let captured = run_stream_and_capture_headers_with_api_key(CacheRetention::None, marked)
            .expect("captured request for marked oauth headers");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer sk-ant-api-like-token")
        );
        assert!(!captured.headers.contains_key("x-api-key"));
        assert!(
            captured
                .headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("oauth-2025-04-20"))
        );
    }

    #[test]
    fn test_stream_claude_style_non_sk_token_uses_bearer_auth_headers() {
        let captured =
            run_stream_and_capture_headers_with_api_key(CacheRetention::None, "claude-oauth-token")
                .expect("captured request for claude bearer headers");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer claude-oauth-token")
        );
        assert!(!captured.headers.contains_key("x-api-key"));
    }

    #[test]
    fn test_stream_kimi_oauth_uses_bearer_and_kimi_headers() {
        let captured = run_stream_and_capture_headers_for_provider_with_api_key(
            CacheRetention::None,
            "kimi-for-coding",
            "kimi-oauth-token",
        )
        .expect("captured request for kimi oauth headers");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer kimi-oauth-token")
        );
        assert!(!captured.headers.contains_key("x-api-key"));
        assert!(
            !captured
                .headers
                .contains_key("anthropic-dangerous-direct-browser-access")
        );
        assert!(!captured.headers.contains_key("anthropic-beta"));
        assert_eq!(
            captured.headers.get("x-msh-platform").map(String::as_str),
            Some("kimi_cli")
        );
        assert!(captured.headers.contains_key("x-msh-version"));
        assert!(captured.headers.contains_key("x-msh-device-name"));
        assert!(captured.headers.contains_key("x-msh-device-model"));
        assert!(captured.headers.contains_key("x-msh-os-version"));
        assert!(captured.headers.contains_key("x-msh-device-id"));
    }

    #[test]
    fn test_stream_kimi_api_key_uses_x_api_key_header() {
        let captured = run_stream_and_capture_headers_for_provider_with_api_key(
            CacheRetention::None,
            "kimi-for-coding",
            "sk-kimi-api-key",
        )
        .expect("captured request for kimi api-key headers");
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("sk-kimi-api-key")
        );
        assert!(!captured.headers.contains_key("authorization"));
        assert!(!captured.headers.contains_key("x-msh-platform"));
    }

    #[test]
    fn test_stream_oauth_beta_header_includes_prompt_caching_when_enabled() {
        let captured =
            run_stream_and_capture_headers_with_api_key(CacheRetention::Short, "sk-ant-oat-test")
                .expect("captured request for oauth + cache beta header");
        let beta = captured
            .headers
            .get("anthropic-beta")
            .expect("anthropic-beta header");
        assert!(beta.contains("oauth-2025-04-20"));
        assert!(beta.contains("prompt-caching-2024-07-31"));
    }

    #[test]
    fn test_stream_http_error_includes_status_and_body_message() {
        let (base_url, _rx) = spawn_test_server(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#,
        );
        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
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
        let result = runtime.block_on(async { provider.stream(&context, &options).await });
        let Err(err) = result else {
            panic!();
        };
        let message = err.to_string();
        assert!(message.contains("Anthropic API error (HTTP 401)"));
        assert!(message.contains("Invalid API key"));
    }

    #[test]
    fn test_provider_name_reflects_override() {
        let provider = AnthropicProvider::new("claude-test").with_provider_name("kimi-for-coding");
        assert_eq!(provider.name(), "kimi-for-coding");
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    fn run_stream_and_capture_headers(cache_retention: CacheRetention) -> Option<CapturedRequest> {
        run_stream_and_capture_headers_with_api_key(cache_retention, "sk-ant-test-key")
    }

    fn run_stream_and_capture_headers_with_api_key(
        cache_retention: CacheRetention,
        api_key: &str,
    ) -> Option<CapturedRequest> {
        run_stream_and_capture_headers_for_provider_with_api_key(
            cache_retention,
            "anthropic",
            api_key,
        )
    }

    fn run_stream_and_capture_headers_for_provider_with_api_key(
        cache_retention: CacheRetention,
        provider_name: &str,
        api_key: &str,
    ) -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = AnthropicProvider::new("claude-test")
            .with_provider_name(provider_name)
            .with_base_url(base_url);
        let context = Context {
            system_prompt: Some("test system".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.to_string()),
            cache_retention,
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

    fn collect_stream_items_from_body(body: &str) -> Vec<Result<StreamEvent>> {
        let (base_url, _rx) = spawn_test_server(200, "text/event-stream", body);
        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
        let context = Context {
            system_prompt: Some("test system".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("sk-ant-test-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut items = Vec::new();
            while let Some(item) = stream.next().await {
                items.push(item);
            }
            items
        })
    }

    fn success_sse_body() -> String {
        [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            "",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            "",
            r#"data: {"type":"message_stop"}"#,
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
                    Err(err) => panic!(),
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
                    Err(err) => panic!(),
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

        (format!("http://{addr}/messages"), rx)
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

    fn load_fixture(file_name: &str) -> ProviderFixture {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/provider_responses")
            .join(file_name);
        let raw = std::fs::read_to_string(path).expect("fixture read");
        serde_json::from_str(&raw).expect("fixture parse")
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
                "claude-test".to_string(),
                "anthropic-messages".to_string(),
                "anthropic".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.event == "ping" {
                    continue;
                }
                if let Some(event) = state.process_event(&msg.data).expect("process_event") {
                    out.push(event);
                }
            }

            out
        })
    }

    fn collect_events_from_byte_chunks(chunks: Vec<Vec<u8>>) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(chunks.into_iter().map(Ok));
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "claude-test".to_string(),
                "anthropic-messages".to_string(),
                "anthropic".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.event == "ping" {
                    continue;
                }
                if let Some(event) = state.process_event(&msg.data).expect("process_event") {
                    out.push(event);
                }
            }

            out
        })
    }

    fn build_text_stream_sse_frames(text_parts: &[&str]) -> Vec<String> {
        let message_start = json!({
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 1
                }
            }
        });
        let content_start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text" }
        });
        let content_stop = json!({
            "type": "content_block_stop",
            "index": 0
        });
        let message_delta = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": text_parts.len().max(1) }
        });

        let mut frames = vec![
            format!("event: message_start\ndata: {message_start}\n\n"),
            format!("event: content_block_start\ndata: {content_start}\n\n"),
        ];

        for (idx, text) in text_parts.iter().enumerate() {
            if idx % 4 == 1 {
                frames.push("event: ping\ndata: {\"type\":\"ping\"}\n\n".to_string());
            }
            let content_delta = json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": text }
            });
            frames.push(format!(
                "event: content_block_delta\ndata: {content_delta}\n\n"
            ));
        }

        frames.push(format!(
            "event: content_block_stop\ndata: {content_stop}\n\n"
        ));
        frames.push(format!("event: message_delta\ndata: {message_delta}\n\n"));
        frames.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string());
        frames
    }

    fn split_ascii_stream_bytes(frames: &[String], fragment_sizes: &[usize]) -> Vec<Vec<u8>> {
        assert!(
            !fragment_sizes.is_empty(),
            "fragment_sizes must contain at least one size"
        );
        assert!(
            fragment_sizes.iter().all(|size| *size > 0),
            "fragment_sizes must be positive"
        );

        let joined = frames.concat();
        assert!(
            joined.is_ascii(),
            "test-only chunk fragmentation expects ASCII SSE fixtures"
        );

        let bytes = joined.as_bytes();
        let mut offset = 0usize;
        let mut idx = 0usize;
        let mut chunks = Vec::new();
        while offset < bytes.len() {
            let size = fragment_sizes[idx % fragment_sizes.len()];
            let end = (offset + size).min(bytes.len());
            chunks.push(bytes[offset..end].to_vec());
            offset = end;
            idx += 1;
        }
        chunks
    }

    fn collect_text_deltas(events: &[StreamEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
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

    // ── bd-3uqg.2.4: Compat custom headers injection ─────────────────

    #[test]
    fn test_compat_custom_headers_injected_into_request() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());

        let mut custom = HashMap::new();
        custom.insert("X-Custom-Tag".to_string(), "anthropic-override".to_string());
        custom.insert("X-Routing-Hint".to_string(), "us-east-1".to_string());
        let compat = crate::models::CompatConfig {
            custom_headers: Some(custom),
            ..Default::default()
        };

        let provider = AnthropicProvider::new("claude-test")
            .with_base_url(base_url)
            .with_compat(Some(compat));

        let context = Context {
            system_prompt: Some("test".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("sk-ant-test-key".to_string()),
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
            Some("anthropic-override"),
            "compat custom header X-Custom-Tag missing"
        );
        assert_eq!(
            captured.headers.get("x-routing-hint").map(String::as_str),
            Some("us-east-1"),
            "compat custom header X-Routing-Hint missing"
        );
        // Standard headers should still be present
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("sk-ant-test-key"),
        );
    }

    #[test]
    fn test_compat_authorization_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());

        let mut custom = HashMap::new();
        custom.insert(
            "Authorization".to_string(),
            "Bearer sk-ant-oat-compat".to_string(),
        );
        let provider = AnthropicProvider::new("claude-test")
            .with_base_url(base_url)
            .with_compat(Some(crate::models::CompatConfig {
                custom_headers: Some(custom),
                ..Default::default()
            }));

        let context = Context {
            system_prompt: Some("test".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
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

        let captured = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer sk-ant-oat-compat")
        );
        assert!(!captured.headers.contains_key("x-api-key"));
        assert_eq!(
            captured
                .headers
                .get("anthropic-dangerous-direct-browser-access")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            captured.headers.get("x-app").map(String::as_str),
            Some("cli")
        );
        assert!(
            captured
                .headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("oauth-2025-04-20"))
        );
    }

    #[test]
    fn test_authorization_override_wins_side_effects_over_x_api_key_override() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());

        let mut custom = HashMap::new();
        custom.insert(
            "Authorization".to_string(),
            "Bearer sk-ant-oat-compat".to_string(),
        );
        let provider = AnthropicProvider::new("claude-test")
            .with_base_url(base_url)
            .with_compat(Some(crate::models::CompatConfig {
                custom_headers: Some(custom),
                ..Default::default()
            }));

        let context = Context {
            system_prompt: Some("test".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let mut headers = HashMap::new();
        headers.insert("X-API-Key".to_string(), "header-ant-key".to_string());

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider
                .stream(
                    &context,
                    &StreamOptions {
                        headers,
                        ..Default::default()
                    },
                )
                .await
                .expect("stream");
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
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer sk-ant-oat-compat")
        );
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("header-ant-key")
        );
        assert_eq!(
            captured
                .headers
                .get("anthropic-dangerous-direct-browser-access")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            captured.headers.get("x-app").map(String::as_str),
            Some("cli")
        );
        assert!(
            captured
                .headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("oauth-2025-04-20"))
        );
    }

    #[test]
    fn test_stream_option_x_api_key_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());

        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
        let context = Context {
            system_prompt: Some("test".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let mut headers = HashMap::new();
        headers.insert("X-API-Key".to_string(), "header-ant-key".to_string());

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider
                .stream(
                    &context,
                    &StreamOptions {
                        headers,
                        ..Default::default()
                    },
                )
                .await
                .expect("stream");
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
            captured.headers.get("x-api-key").map(String::as_str),
            Some("header-ant-key")
        );
        assert!(!captured.headers.contains_key("authorization"));
    }

    #[test]
    fn test_compat_none_does_not_affect_headers() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());

        let provider = AnthropicProvider::new("claude-test")
            .with_base_url(base_url)
            .with_compat(None);

        let context = Context {
            system_prompt: Some("test".to_string().into()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("sk-ant-test-key".to_string()),
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
        // Standard Anthropic headers present, no custom headers
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("sk-ant-test-key"),
        );
        assert!(
            !captured.headers.contains_key("x-custom-tag"),
            "No custom headers should be present with compat=None"
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
            StreamState::new(
                sse,
                "claude-test".into(),
                "anthropic-messages".into(),
                "anthropic".into(),
            )
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

        fn block_type() -> impl Strategy<Value = String> {
            prop_oneof![
                Just("text".to_string()),
                Just("thinking".to_string()),
                Just("tool_use".to_string()),
                Just("unknown_block_type".to_string()),
                "[a-z_]{1,12}",
            ]
        }

        fn delta_type() -> impl Strategy<Value = String> {
            prop_oneof![
                Just("text_delta".to_string()),
                Just("thinking_delta".to_string()),
                Just("input_json_delta".to_string()),
                Just("signature_delta".to_string()),
                Just("unknown_delta".to_string()),
                "[a-z_]{1,16}",
            ]
        }

        fn content_index() -> impl Strategy<Value = u32> {
            prop_oneof![
                5 => 0u32..5u32,
                2 => Just(0u32),
                1 => Just(u32::MAX),
                1 => 1000u32..2000u32,
            ]
        }

        fn stop_reason_str() -> impl Strategy<Value = String> {
            prop_oneof![
                Just("end_turn".to_string()),
                Just("max_tokens".to_string()),
                Just("tool_use".to_string()),
                Just("stop_sequence".to_string()),
                Just("unknown_reason".to_string()),
                "[a-z_]{1,12}",
            ]
        }

        /// Strategy that generates valid `AnthropicStreamEvent` JSON strings
        /// covering all event type variants and edge cases.
        fn anthropic_event_json() -> impl Strategy<Value = String> {
            prop_oneof![
                // message_start
                3 => token_count().prop_flat_map(|input| {
                    (Just(input), token_count(), token_count()).prop_map(
                        move |(cache_read, cache_write, _)| {
                            serde_json::json!({
                                "type": "message_start",
                                "message": {
                                    "usage": {
                                        "input_tokens": input,
                                        "cache_read_input_tokens": cache_read,
                                        "cache_creation_input_tokens": cache_write
                                    }
                                }
                            })
                            .to_string()
                        },
                    )
                }),
                // message_start without usage
                1 => Just(r#"{"type":"message_start","message":{}}"#.to_string()),
                // content_block_start
                3 => (content_index(), block_type(), optional_string(), optional_string())
                    .prop_map(|(idx, bt, id, name)| {
                        let mut block = serde_json::json!({"type": bt});
                        if let Some(id) = id {
                            block["id"] = serde_json::Value::String(id);
                        }
                        if let Some(name) = name {
                            block["name"] = serde_json::Value::String(name);
                        }
                        serde_json::json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": block
                        })
                        .to_string()
                    }),
                // content_block_delta
                3 => (content_index(), delta_type(), optional_string(), optional_string(), optional_string(), optional_string())
                    .prop_map(|(idx, dt, text, thinking, partial_json, sig)| {
                        let mut delta = serde_json::json!({"type": dt});
                        if let Some(t) = text { delta["text"] = serde_json::Value::String(t); }
                        if let Some(t) = thinking { delta["thinking"] = serde_json::Value::String(t); }
                        if let Some(p) = partial_json { delta["partial_json"] = serde_json::Value::String(p); }
                        if let Some(s) = sig { delta["signature"] = serde_json::Value::String(s); }
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": delta
                        })
                        .to_string()
                    }),
                // content_block_stop
                2 => content_index().prop_map(|idx| {
                    serde_json::json!({"type": "content_block_stop", "index": idx}).to_string()
                }),
                // message_delta
                2 => (stop_reason_str(), token_count()).prop_map(|(sr, out)| {
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": sr},
                        "usage": {"output_tokens": out}
                    })
                    .to_string()
                }),
                // message_delta without usage
                1 => stop_reason_str().prop_map(|sr| {
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": sr}
                    })
                    .to_string()
                }),
                // message_stop
                2 => Just(r#"{"type":"message_stop"}"#.to_string()),
                // error
                2 => small_string().prop_map(|msg| {
                    serde_json::json!({"type": "error", "error": {"message": msg}}).to_string()
                }),
                // ping
                2 => Just(r#"{"type":"ping"}"#.to_string()),
            ]
        }

        /// Strategy that generates arbitrary JSON — chaos testing.
        fn chaos_json() -> impl Strategy<Value = String> {
            prop_oneof![
                // Empty / whitespace
                Just(String::new()),
                Just("{}".to_string()),
                Just("[]".to_string()),
                Just("null".to_string()),
                Just("true".to_string()),
                Just("42".to_string()),
                // Broken JSON
                Just("{".to_string()),
                Just(r#"{"type":}"#.to_string()),
                Just(r#"{"type":null}"#.to_string()),
                // Unknown type tag
                "[a-z_]{1,20}".prop_map(|t| format!(r#"{{"type":"{t}"}}"#)),
                // Completely random printable ASCII
                "[ -~]{0,64}",
                // Valid JSON with wrong schema
                Just(r#"{"type":"message_start"}"#.to_string()),
                Just(r#"{"type":"content_block_delta"}"#.to_string()),
                Just(r#"{"type":"error"}"#.to_string()),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn process_event_valid_never_panics(data in anthropic_event_json()) {
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
                events in prop::collection::vec(anthropic_event_json(), 1..8)
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

    /// Opaque wrapper around the Anthropic stream processor state.
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
                "claude-fuzz".into(),
                "anthropic-messages".into(),
                "anthropic".into(),
            ))
        }

        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            Ok(self
                .0
                .process_event(data)?
                .map_or_else(Vec::new, |event| vec![event]))
        }
    }
}
