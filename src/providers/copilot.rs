//! GitHub Copilot provider implementation.
//!
//! Copilot uses a two-step authentication flow:
//! 1. Exchange a GitHub OAuth/PAT token for a short-lived Copilot session token
//!    via `https://api.github.com/copilot_internal/v2/token`.
//! 2. Use the session token to make OpenAI-compatible chat completion requests
//!    to the Copilot proxy endpoint.
//!
//! The session token is cached for its issuing GitHub credential and refreshed
//! when that credential changes, it expires, or the proxy explicitly rejects
//! it before a stream starts. Bound credential removal revokes the cache.
//! Established streams and ambiguous transport failures are never replayed here.
//! GitHub Enterprise Server is supported via a configurable base URL.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamEvent, StreamOptions};
use asupersync::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use async_trait::async_trait;
use futures::Stream;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::openai::OpenAIProvider;

// ── Constants ────────────────────────────────────────────────────

/// Default GitHub API base for token exchange.
const GITHUB_API_BASE: &str = "https://api.github.com";

/// Editor version header value (required by Copilot API).
/// Override via `PI_COPILOT_EDITOR_VERSION`.
const EDITOR_VERSION: &str = "vscode/1.96.2";

/// User-Agent header value (required by Copilot API).
/// Override via `PI_COPILOT_USER_AGENT`.
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";

/// GitHub API version header.
/// Override via `PI_GITHUB_API_VERSION`.
const GITHUB_API_VERSION: &str = "2025-04-01";

/// Safety margin: refresh the session token this many seconds before expiry.
const TOKEN_REFRESH_MARGIN_SECS: i64 = 60;

/// Token responses are small control-plane messages, not model output.
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

fn copilot_editor_version() -> String {
    std::env::var("PI_COPILOT_EDITOR_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| EDITOR_VERSION.to_string())
}

fn copilot_user_agent() -> String {
    std::env::var("PI_COPILOT_USER_AGENT")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| COPILOT_USER_AGENT.to_string())
}

fn github_api_version() -> String {
    std::env::var("PI_GITHUB_API_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| GITHUB_API_VERSION.to_string())
}

/// Resolve the GitHub REST API base used for the OAuth token exchange.
///
/// Defaults to `https://api.github.com`. GitHub Enterprise / data-residency
/// deployments can point the exchange elsewhere via
/// `PI_COPILOT_GITHUB_API_BASE` (e.g. `https://github.example.com/api/v3`).
/// This is deliberately separate from the model catalog's `base_url`, which
/// is a chat-completions endpoint hint and must never steer the token
/// exchange (gh #191).
fn github_api_base_from_env(value: Option<&str>) -> String {
    match value.map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => GITHUB_API_BASE.to_string(),
    }
}

/// The effective GitHub REST API base for Copilot OAuth/usage calls
/// (`PI_COPILOT_GITHUB_API_BASE` or the `api.github.com` default).
pub(crate) fn github_api_base() -> String {
    github_api_base_from_env(std::env::var("PI_COPILOT_GITHUB_API_BASE").ok().as_deref())
}

/// Ensure a chat-completions URL ends with `/chat/completions` exactly once.
fn ensure_chat_completions_path(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

// ── Token exchange types ─────────────────────────────────────────

/// Response from the Copilot token exchange endpoint.
#[derive(Deserialize)]
struct CopilotTokenResponse {
    /// The short-lived session token.
    token: String,
    /// Unix timestamp (seconds) when the token expires.
    expires_at: i64,
    /// Endpoints returned by the API.
    #[serde(default)]
    endpoints: CopilotEndpoints,
}

impl std::fmt::Debug for CopilotTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotTokenResponse")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Endpoint URLs returned alongside the session token.
#[derive(Debug, Default, Deserialize)]
struct CopilotEndpoints {
    /// The API endpoint for chat completions.
    #[serde(default)]
    api: String,
}

/// Cached session token with expiry.
#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: i64,
    api_endpoint: String,
    /// Cache identity only; never logged or used as an authentication token.
    github_credential: [u8; 32],
}

fn github_credential_identity(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Treat all response content as secret-bearing, including parser errors:
/// serde diagnostics can otherwise quote an unexpected credential value.
fn parse_session_token_response(text: &str, now: i64) -> Result<CopilotTokenResponse> {
    if text.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(Error::auth(
            "Copilot token response exceeded the size limit",
        ));
    }
    let response: CopilotTokenResponse =
        serde_json::from_str(text).map_err(|_| Error::auth("Invalid Copilot token response"))?;
    // Do not trim or otherwise rewrite an opaque credential. Reject values
    // that cannot safely form a single HTTP Authorization header instead.
    if response.token.is_empty() || !response.token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::auth("Copilot returned an unusable session token"));
    }
    if response.expires_at <= now {
        return Err(Error::auth("Copilot returned an expired session token"));
    }
    Ok(response)
}

// ── Provider ─────────────────────────────────────────────────────

/// GitHub Copilot provider that wraps OpenAI-compatible streaming.
pub struct CopilotProvider {
    /// HTTP client for token exchange and API requests.
    client: Client,
    /// The GitHub OAuth token or PAT used for token exchange.
    github_token: String,
    /// A positively identified models.json fallback, used only when the bound
    /// source is unconfigured. Never inferred from a captured login token.
    catalog_api_key_fallback: Option<String>,
    /// The model ID to request (e.g., "gpt-4o", "claude-3.5-sonnet").
    model: String,
    /// GitHub API base URL for the OAuth token exchange only (supports
    /// Enterprise: `https://github.example.com/api/v3`). Never used for
    /// chat completions.
    github_api_base: String,
    /// Optional chat-completions endpoint override. When set, it takes
    /// precedence over the endpoint the token-exchange response supplies.
    chat_completions_override: Option<String>,
    /// Provider name for event attribution.
    provider_name: String,
    /// Compatibility overrides passed to the underlying OpenAI provider.
    compat: Option<CompatConfig>,
    /// Whether the target Copilot model supports reasoning effort controls.
    reasoning: bool,
    /// Declared input modalities (`text`, `image`, ...) for the target model.
    input_types: Vec<crate::provider::InputType>,
    /// Catalog pricing for the model when known.
    model_cost: Option<crate::provider::ModelCost>,
    /// Optional authoritative source for live login/logout and key rotation.
    auth_path_override: Option<std::path::PathBuf>,
    auth_command_working_directory: Option<std::path::PathBuf>,
    /// Cached session token (refreshed automatically).
    cached_token: Mutex<Option<CachedToken>>,
    /// Serialize exchanges without holding a synchronous mutex across I/O.
    /// A cancelled owner releases the permit; waiters resolve auth afresh.
    token_refresh: Arc<AsyncMutex<()>>,
}

impl CopilotProvider {
    /// Create a new Copilot provider.
    pub fn new(model: impl Into<String>, github_token: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            github_token: github_token.into(),
            catalog_api_key_fallback: None,
            model: model.into(),
            github_api_base: github_api_base(),
            chat_completions_override: None,
            provider_name: "github-copilot".to_string(),
            compat: None,
            reasoning: false,
            input_types: Vec::new(),
            model_cost: None,
            auth_path_override: None,
            auth_command_working_directory: None,
            cached_token: Mutex::new(None),
            token_refresh: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Set the GitHub API base URL used for the OAuth token exchange (for
    /// Enterprise / data-residency deployments). This must be a GitHub REST
    /// API host — never a chat-completions endpoint; for that, use
    /// [`Self::with_chat_completions_endpoint`] (gh #191).
    #[must_use]
    pub fn with_github_api_base(mut self, base: impl Into<String>) -> Self {
        self.github_api_base = base.into();
        self
    }

    /// Pin the chat-completions endpoint, overriding whatever endpoint the
    /// token-exchange response supplies. This is what a configured catalog
    /// `base_url` means for Copilot (a chat endpoint hint, like every other
    /// provider); it never affects where the OAuth token exchange goes
    /// (gh #191).
    #[must_use]
    pub fn with_chat_completions_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.chat_completions_override = Some(endpoint.into());
        self
    }

    /// Set the provider name for event attribution.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Attach compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Inject a custom HTTP client (for testing / VCR).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Enable or disable OpenAI reasoning effort parameters on Chat Completions.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Declare supported input modalities (`text`, `image`, ...) for the model.
    #[must_use]
    pub fn with_input_types(mut self, input_types: &[crate::provider::InputType]) -> Self {
        self.input_types = input_types.to_vec();
        self
    }

    /// Attach catalog pricing for this Copilot model.
    #[must_use]
    pub const fn with_model_cost(mut self, cost: crate::provider::ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    /// Bind the authoritative credential source for every subsequent request.
    /// Removing credentials from this source also revokes unexpired cached
    /// session bearers; the construction-time token is not a fallback.
    pub fn with_auth_storage(mut self, auth: &crate::auth::AuthStorage) -> Result<Self> {
        self.auth_path_override = Some(if auth.path().is_absolute() {
            auth.path().to_path_buf()
        } else {
            std::env::current_dir()?.join(auth.path())
        });
        self.auth_command_working_directory = auth
            .command_working_directory()
            .map(std::path::Path::to_path_buf);
        Ok(self)
    }

    /// The factory supplies this only after verifying model-key provenance.
    /// Stored credentials and pool exhaustion retain their normal precedence.
    pub(super) fn with_catalog_api_key_fallback(mut self, key: Option<&str>) -> Self {
        self.catalog_api_key_fallback = key.map(str::to_string);
        self
    }

    async fn effective_github_token(&self) -> Result<String> {
        let token = if let Some(path) = &self.auth_path_override {
            let path = path.clone();
            let cwd = self.auth_command_working_directory.clone();
            let provider = self.provider_name.clone();
            let fallback = self.catalog_api_key_fallback.clone();
            // Guarded file reads and configured credential commands may block.
            // Keep them off the async executor, and never expose their diagnostics
            // (which may contain credential material) in a provider error.
            asupersync::runtime::spawn_blocking(move || -> Result<String> {
                let mut storage = crate::auth::AuthStorage::load(path).map_err(|_| {
                    Error::auth("Unable to read the bound GitHub Copilot credential source")
                })?;
                if let Some(cwd) = cwd {
                    storage.set_command_working_directory(&cwd).map_err(|_| {
                        Error::auth("Unable to resolve the GitHub Copilot credential workspace")
                    })?;
                }
                let mut resolution = storage.resolve_api_key_status(&provider, None);
                if matches!(resolution, crate::auth::CredentialResolution::NotConfigured)
                    && provider != "github-copilot"
                {
                    resolution = storage.resolve_api_key_status("github-copilot", None);
                }
                resolution.into_result(&provider)?.or(fallback).ok_or_else(|| {
                    Error::auth(
                        "GitHub Copilot credentials are no longer available in the bound auth source; run /login github-copilot",
                    )
                })
            })
            .await?
        } else {
            self.github_token.clone()
        };
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Error::auth(
                "GitHub Copilot requires a usable GitHub credential",
            ));
        }
        Ok(token)
    }

    fn clear_session_token(&self) {
        *self
            .cached_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Get a valid session token, refreshing if necessary.
    async fn ensure_session_token(&self) -> Result<CachedToken> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let _refresh = OwnedMutexGuard::lock(Arc::clone(&self.token_refresh), owner.cx())
            .await
            .map_err(|_| Error::Aborted)?;
        let github_token = self
            .effective_github_token()
            .await
            .inspect_err(|_| self.clear_session_token())?;
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        let github_credential = github_credential_identity(&github_token);
        // Expiry alone is insufficient: logout and account switches must take
        // effect before a cached session can authorize another completion.
        {
            let mut guard = self
                .cached_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = &*guard {
                let now = chrono::Utc::now().timestamp();
                // ubs:ignore[rust.security.constant-time-compare] Cache identity matching, not authentication verification.
                if cached.github_credential == github_credential
                    && cached.expires_at > now + TOKEN_REFRESH_MARGIN_SECS
                {
                    return Ok(cached.clone());
                }
            }
            *guard = None;
        }

        // Exchange GitHub token for a Copilot session token.
        let token_url = format!(
            "{}/copilot_internal/v2/token",
            self.github_api_base.trim_end_matches('/')
        );

        let request = self
            .client
            .get(&token_url)
            .header("Authorization", format!("token {github_token}"))
            .header("Accept", "application/json")
            .header("Editor-Version", copilot_editor_version())
            .header("User-Agent", copilot_user_agent())
            .header("X-Github-Api-Version", github_api_version());

        let response = Box::pin(request.send()).await.map_err(|_| {
            // Transport diagnostics can contain a request URL or headers.
            Error::auth("Copilot token exchange failed before a response was received")
        })?;

        let status = response.status();
        if !(200..300).contains(&status) {
            // Never copy an authentication response into user-visible
            // errors. Do not drain a potentially unbounded error body.
            return Err(Error::auth(format!(
                "Copilot token exchange failed (HTTP {status}). \
                 Verify your GitHub token has Copilot access."
            )));
        }

        let text = response
            .text_limited(MAX_TOKEN_RESPONSE_BYTES)
            .await
            .map_err(|_| Error::auth("Failed to read Copilot token response"))?;
        let token_response = parse_session_token_response(&text, chrono::Utc::now().timestamp())?;

        // The exchange may have been in flight while the user logged out or
        // changed accounts. Never publish or use a bearer issued for that
        // abandoned credential. The refresh permit also prevents an older
        // exchange from overwriting a newer account's cached bearer.
        let current_token = self
            .effective_github_token()
            .await
            .inspect_err(|_| self.clear_session_token())?;
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        // ubs:ignore[rust.security.constant-time-compare] Credential-source change detection, not authentication verification.
        if github_credential_identity(&current_token) != github_credential {
            return Err(Error::auth(
                "GitHub Copilot credentials changed during session refresh; retry the request",
            ));
        }

        // Determine the chat-completions endpoint. A configured override
        // (catalog `base_url`) wins over the endpoint the token-exchange
        // response supplied; with neither, fall back to the standard
        // Copilot proxy URL.
        let api_endpoint = if let Some(configured) = &self.chat_completions_override {
            ensure_chat_completions_path(configured)
        } else if token_response.endpoints.api.is_empty() {
            "https://api.githubcopilot.com/chat/completions".to_string()
        } else {
            ensure_chat_completions_path(&token_response.endpoints.api)
        };

        let cached = CachedToken {
            token: token_response.token,
            expires_at: token_response.expires_at,
            api_endpoint,
            github_credential,
        };

        // Store only a fully read, parsed, and validated credential.
        {
            let mut guard = self
                .cached_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(cached.clone());
        }

        Ok(cached)
    }

    /// A late rejection must not evict a different request's refreshed token.
    fn invalidate_session_token(&self, rejected: &CachedToken) {
        let mut cached = self
            .cached_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.as_ref().is_some_and(|token| {
            // ubs:ignore[rust.security.constant-time-compare] Rejected request identity, not authentication verification.
            token.token == rejected.token && token.github_credential == rejected.github_credential
        }) {
            *cached = None;
        }
    }

    /// Only the inner adapter's explicit HTTP rejection authorizes a retry.
    /// Permission failures, response bodies mentioning 401, and I/O failures
    /// do not prove that it is safe to submit the completion again.
    fn session_token_rejected(&self, error: &Error) -> bool {
        matches!(error, Error::Provider { provider, message }
            if provider == &self.provider_name
                && message.starts_with("OpenAI API error (HTTP 401):"))
    }

    fn session_options(session: &CachedToken, options: &StreamOptions) -> StreamOptions {
        // Preserve request hooks and generation settings, but never let a
        // generic GitHub credential override the exchanged session bearer.
        let mut options = options.clone();
        options.api_key = Some(session.token.clone());
        options
            .headers
            .retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
        options.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", session.token),
        );
        options
            .headers
            .insert("Editor-Version".to_string(), copilot_editor_version());
        options
            .headers
            .insert("User-Agent".to_string(), copilot_user_agent());
        options
            .headers
            .insert("X-Github-Api-Version".to_string(), github_api_version());
        options.headers.insert(
            "Copilot-Integration-Id".to_string(),
            "vscode-chat".to_string(),
        );
        options
    }

    async fn stream_with_session(
        &self,
        session: &CachedToken,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let mut compat = self.compat.clone();
        if let Some(headers) = compat
            .as_mut()
            .and_then(|compat| compat.custom_headers.as_mut())
        {
            headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
        }
        let mut inner = OpenAIProvider::new(&self.model)
            .with_provider_name(&self.provider_name)
            .with_base_url(&session.api_endpoint)
            .with_compat(compat)
            .with_reasoning(self.reasoning)
            .with_client(self.client.clone());
        if let Some(cost) = self.model_cost.clone() {
            inner = inner.with_model_cost(cost);
        }
        if !self.input_types.is_empty() {
            inner = inner.with_input_types(&self.input_types);
        }
        inner
            .stream(context, &Self::session_options(session, options))
            .await
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn api(&self) -> &'static str {
        "openai-completions"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn model_cost(&self) -> Option<crate::provider::ModelCost> {
        self.model_cost.clone()
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            vision: self.input_types.is_empty()
                || self
                    .input_types
                    .iter()
                    .any(|kind| matches!(kind, crate::provider::InputType::Image)),
            ..crate::provider::ProviderCapabilities::default()
        }
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let session = self.ensure_session_token().await?;
        match self.stream_with_session(&session, context, options).await {
            Err(error) if self.session_token_rejected(&error) => {
                self.invalidate_session_token(&session);
                let refreshed = self.ensure_session_token().await?;
                // Exactly one recovery attempt. Do not wrap the returned
                // stream: once accepted, even a later auth-shaped stream
                // error must not replay a potentially billable completion.
                let result = self.stream_with_session(&refreshed, context, options).await;
                if let Err(error) = &result
                    && self.session_token_rejected(error)
                {
                    self.invalidate_session_token(&refreshed);
                }
                result
            }
            result => result,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcr::{
        Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder,
    };
    use futures::StreamExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_copilot_provider_defaults() {
        let p = CopilotProvider::new("gpt-4o", "ghp_test123");
        assert_eq!(p.name(), "github-copilot");
        assert_eq!(p.api(), "openai-completions");
        assert_eq!(p.model_id(), "gpt-4o");
        assert_eq!(p.github_api_base, GITHUB_API_BASE);
    }

    #[test]
    fn test_copilot_provider_builder() {
        let p = CopilotProvider::new("gpt-4o", "ghp_test")
            .with_provider_name("copilot-enterprise")
            .with_github_api_base("https://github.example.com/api/v3");

        assert_eq!(p.name(), "copilot-enterprise");
        assert_eq!(p.github_api_base, "https://github.example.com/api/v3");
    }

    #[test]
    fn test_chat_completions_endpoint_does_not_touch_github_api_base() {
        // A catalog `base_url` (e.g. https://api.individual.githubcopilot.com)
        // is a chat-completions hint; the OAuth exchange host must stay put.
        let p = CopilotProvider::new("gpt-4o", "ghp_test")
            .with_chat_completions_endpoint("https://api.individual.githubcopilot.com");
        assert_eq!(p.github_api_base, GITHUB_API_BASE);
        assert_eq!(
            p.chat_completions_override.as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
    }

    #[test]
    fn test_github_api_base_from_env_resolution() {
        // Default: api.github.com.
        assert_eq!(github_api_base_from_env(None), GITHUB_API_BASE);
        assert_eq!(github_api_base_from_env(Some("")), GITHUB_API_BASE);
        assert_eq!(github_api_base_from_env(Some("  ")), GITHUB_API_BASE);
        // GHE escape hatch.
        assert_eq!(
            github_api_base_from_env(Some("https://github.example.com/api/v3")),
            "https://github.example.com/api/v3"
        );
    }

    #[test]
    fn test_ensure_chat_completions_path() {
        assert_eq!(
            ensure_chat_completions_path("https://proxy.example.com/v1"),
            "https://proxy.example.com/v1/chat/completions"
        );
        assert_eq!(
            ensure_chat_completions_path("https://proxy.example.com/v1/"),
            "https://proxy.example.com/v1/chat/completions"
        );
        assert_eq!(
            ensure_chat_completions_path("https://proxy.example.com/chat/completions"),
            "https://proxy.example.com/chat/completions"
        );
    }

    #[test]
    fn test_copilot_token_response_deserialization() {
        let json = r#"{
            "token": "ghu_session_abc123",
            "expires_at": 1700000000,
            "endpoints": {
                "api": "https://copilot-proxy.githubusercontent.com/v1",
                "proxy": "https://copilot-proxy.githubusercontent.com"
            }
        }"#;

        let resp: CopilotTokenResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.token, "ghu_session_abc123");
        assert_eq!(resp.expires_at, 1_700_000_000);
        assert_eq!(
            resp.endpoints.api,
            "https://copilot-proxy.githubusercontent.com/v1"
        );
    }

    #[test]
    fn test_copilot_token_response_missing_endpoints() {
        let json = r#"{"token": "ghu_abc", "expires_at": 1700000000}"#;

        let resp: CopilotTokenResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.token, "ghu_abc");
        assert!(resp.endpoints.api.is_empty());
    }

    #[test]
    fn test_copilot_token_exchange_url_construction() {
        // Standard GitHub
        let p = CopilotProvider::new("gpt-4o", "ghp_test");
        let expected = "https://api.github.com/copilot_internal/v2/token";
        let actual = format!(
            "{}/copilot_internal/v2/token",
            p.github_api_base.trim_end_matches('/')
        );
        assert_eq!(actual, expected);

        // Enterprise with trailing slash
        let p = CopilotProvider::new("gpt-4o", "ghp_test")
            .with_github_api_base("https://github.example.com/api/v3/");
        let actual = format!(
            "{}/copilot_internal/v2/token",
            p.github_api_base.trim_end_matches('/')
        );
        assert_eq!(
            actual,
            "https://github.example.com/api/v3/copilot_internal/v2/token"
        );
    }

    #[test]
    fn test_cached_token_clone() {
        let cloned = CachedToken {
            token: "session-tok".to_string(),
            expires_at: 99999,
            api_endpoint: "https://example.com/chat/completions".to_string(),
            github_credential: github_credential_identity("github-secret"),
        };
        assert_eq!(cloned.token, "session-tok");
        assert_eq!(cloned.expires_at, 99999);
    }

    fn vcr_raw_token_client(
        test_name: &str,
        status: u16,
        response_body: String,
    ) -> (Client, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: test_name.to_string(),
            recorded_at: "2025-01-01T00:00:00Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "GET".to_string(),
                    url: "https://api.github.com/copilot_internal/v2/token".to_string(),
                    headers: vec![],
                    body: None,
                    body_text: None,
                },
                response: RecordedResponse {
                    status,
                    headers: vec![],
                    body_chunks: vec![response_body],
                    body_chunks_base64: None,
                },
            }],
        };
        let serialized = serde_json::to_string_pretty(&cassette).expect("serialize");
        std::fs::write(temp.path().join(format!("{test_name}.json")), serialized)
            .expect("write cassette");
        let recorder = VcrRecorder::new_with(test_name, VcrMode::Playback, temp.path());
        let client = Client::new().with_vcr(recorder);
        (client, temp)
    }

    /// Build a VCR client that returns a successful token exchange response.
    fn vcr_token_exchange_client(
        test_name: &str,
        token: &str,
        expires_at: i64,
        api_endpoint: &str,
    ) -> (Client, tempfile::TempDir) {
        let response_body = serde_json::json!({
            "token": token,
            "expires_at": expires_at,
            "endpoints": {
                "api": api_endpoint
            }
        })
        .to_string();
        vcr_raw_token_client(test_name, 200, response_body)
    }

    #[test]
    fn test_token_exchange_success_via_vcr() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let far_future = chrono::Utc::now().timestamp() + 3600;
            let (client, _temp) = vcr_token_exchange_client(
                "copilot_token_success",
                "ghu_session_test",
                far_future,
                "https://copilot-proxy.example.com/v1",
            );
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy_token").with_client(client);
            let cached = provider
                .ensure_session_token()
                .await
                .expect("token exchange");
            assert_eq!(cached.token, "ghu_session_test");
            assert_eq!(cached.expires_at, far_future);
            assert_eq!(
                cached.api_endpoint,
                "https://copilot-proxy.example.com/v1/chat/completions"
            );
        });
    }

    #[test]
    fn test_token_exchange_caches_on_second_call() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let far_future = chrono::Utc::now().timestamp() + 3600;
            let (client, _temp) =
                vcr_token_exchange_client("copilot_token_cache", "ghu_cached", far_future, "");
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy").with_client(client);
            // First call populates the cache.
            let first = provider.ensure_session_token().await.expect("first call");
            assert_eq!(first.token, "ghu_cached");
            // Second call should use the cache (no VCR interaction needed).
            let second = provider.ensure_session_token().await.expect("second call");
            assert_eq!(second.token, "ghu_cached");
        });
    }

    #[test]
    fn test_token_exchange_error_returns_auth_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let test_name = "copilot_token_error";
        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: test_name.to_string(),
            recorded_at: "2025-01-01T00:00:00Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "GET".to_string(),
                    url: "https://api.github.com/copilot_internal/v2/token".to_string(),
                    headers: vec![],
                    body: None,
                    body_text: None,
                },
                response: RecordedResponse {
                    status: 401,
                    headers: vec![],
                    body_chunks: vec![r#"{"message":"Bad credentials"}"#.to_string()],
                    body_chunks_base64: None,
                },
            }],
        };
        let serialized = serde_json::to_string_pretty(&cassette).expect("serialize");
        std::fs::write(temp.path().join(format!("{test_name}.json")), serialized)
            .expect("write cassette");
        let recorder = VcrRecorder::new_with(test_name, VcrMode::Playback, temp.path());
        let client = Client::new().with_vcr(recorder);

        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let provider = CopilotProvider::new("gpt-4o", "ghp_bad_token").with_client(client);
            let result = provider.ensure_session_token().await;
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("401") || msg.contains("Bad credentials"),
                "expected auth error, got: {msg}"
            );
        });
    }

    #[test]
    fn test_token_exchange_fallback_endpoint() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let far_future = chrono::Utc::now().timestamp() + 3600;
            // Empty api endpoint → should fall back to default.
            let (client, _temp) =
                vcr_token_exchange_client("copilot_token_fallback", "ghu_fallback", far_future, "");
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy").with_client(client);
            let cached = provider.ensure_session_token().await.expect("fallback");
            assert_eq!(
                cached.api_endpoint,
                "https://api.githubcopilot.com/chat/completions"
            );
        });
    }

    #[test]
    fn test_configured_endpoint_never_routes_token_exchange() {
        // Tripwire: the cassette only answers a GET to
        // https://api.github.com/copilot_internal/v2/token. If a configured
        // chat endpoint ever leaked into the token-exchange URL again
        // (gh #191), VCR playback would find no matching interaction and
        // this test would fail. The override must still win for the
        // chat-completions endpoint over the server-supplied one.
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let far_future = chrono::Utc::now().timestamp() + 3600;
            let (client, _temp) = vcr_token_exchange_client(
                "copilot_token_chat_override",
                "ghu_override",
                far_future,
                "https://server-supplied.example.com/v1",
            );
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy")
                .with_client(client)
                .with_chat_completions_endpoint("https://api.individual.githubcopilot.com");
            let cached = provider
                .ensure_session_token()
                .await
                .expect("token exchange must target api.github.com");
            assert_eq!(
                cached.api_endpoint,
                "https://api.individual.githubcopilot.com/chat/completions"
            );
        });
    }

    #[test]
    fn test_token_exchange_endpoint_already_has_path() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let far_future = chrono::Utc::now().timestamp() + 3600;
            let (client, _temp) = vcr_token_exchange_client(
                "copilot_token_full_endpoint",
                "ghu_full",
                far_future,
                "https://custom.proxy.com/chat/completions",
            );
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy").with_client(client);
            let cached = provider
                .ensure_session_token()
                .await
                .expect("full endpoint");
            // Endpoint already includes /chat/completions; should not be duplicated.
            assert_eq!(
                cached.api_endpoint,
                "https://custom.proxy.com/chat/completions"
            );
        });
    }

    fn cached_session(token: &str) -> CachedToken {
        CachedToken {
            token: token.to_string(),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            api_endpoint: "https://copilot-proxy.example.com/v1/chat/completions".to_string(),
            github_credential: github_credential_identity("github-secret"),
        }
    }

    #[test]
    fn session_rejection_requires_exact_proxy_status() {
        let provider = CopilotProvider::new("gpt-4o", "github-secret")
            .with_provider_name("copilot-enterprise");
        assert!(provider.session_token_rejected(&Error::provider(
            "copilot-enterprise",
            "OpenAI API error (HTTP 401): rejected"
        )));
        for error in [
            Error::provider("other", "OpenAI API error (HTTP 401): rejected"),
            Error::provider("copilot-enterprise", "OpenAI API error (HTTP 403): 401"),
            Error::provider("copilot-enterprise", "OpenAI API error (HTTP 429): 401"),
            Error::provider("copilot-enterprise", "OpenAI API error (HTTP 500): 401"),
            Error::provider("copilot-enterprise", "SSE error: HTTP 401"),
            Error::api("OpenAI API error (HTTP 401): rejected"),
            Error::Aborted,
        ] {
            assert!(!provider.session_token_rejected(&error), "{error}");
        }
    }

    #[test]
    fn rejection_does_not_evict_a_concurrent_replacement() {
        let provider = CopilotProvider::new("gpt-4o", "github-secret");
        *provider.cached_token.lock().expect("cache") = Some(cached_session("new-token"));
        provider.invalidate_session_token(&cached_session("old-token"));
        assert_eq!(
            provider
                .cached_token
                .lock()
                .expect("cache")
                .as_ref()
                .expect("retained")
                .token,
            "new-token"
        );
        let mut other_account = cached_session("new-token");
        other_account.github_credential = github_credential_identity("other-account");
        provider.invalidate_session_token(&other_account);
        assert!(provider.cached_token.lock().expect("cache").is_some());
        provider.invalidate_session_token(&cached_session("new-token"));
        assert!(provider.cached_token.lock().expect("cache").is_none());
        provider.invalidate_session_token(&cached_session("new-token"));
        assert!(provider.cached_token.lock().expect("cache").is_none());
    }

    #[test]
    fn session_bearer_replaces_all_authorization_spellings() {
        let original = StreamOptions {
            api_key: Some("github-secret".to_string()),
            max_tokens: Some(123),
            headers: [
                (
                    "authorization".to_string(),
                    "Bearer github-secret".to_string(),
                ),
                ("AUTHORIZATION".to_string(), "stale-session".to_string()),
                ("X-Request-Id".to_string(), "request-1".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let options =
            CopilotProvider::session_options(&cached_session("session-secret"), &original);
        let auth: Vec<_> = options
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1, "Bearer session-secret");
        assert_eq!(options.api_key.as_deref(), Some("session-secret"));
        assert_eq!(options.max_tokens, Some(123));
        assert_eq!(
            options.headers.get("X-Request-Id").map(String::as_str),
            Some("request-1")
        );
        assert_eq!(original.api_key.as_deref(), Some("github-secret"));
        assert_eq!(original.headers.len(), 3);
    }

    fn recovery_request_body() -> serde_json::Value {
        serde_json::json!({"model": "gpt-4o", "messages": [], "stream": true})
    }

    /// Start with a cached token. A second proxy response requires exactly
    /// one intervening GitHub exchange; no live HTTP fallback is permitted.
    fn recovery_fixture(
        test_name: &str,
        responses: &[(u16, &str)],
    ) -> (
        CopilotProvider,
        StreamOptions,
        Arc<AtomicUsize>,
        tempfile::TempDir,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut interactions = Vec::new();
        for (index, &(status, body)) in responses.iter().enumerate() {
            if index == 1 {
                interactions.push(Interaction {
                    request: RecordedRequest {
                        method: "GET".to_string(),
                        url: "https://api.github.com/copilot_internal/v2/token".to_string(),
                        headers: vec![],
                        body: None,
                        body_text: None,
                    },
                    response: RecordedResponse {
                        status: 200,
                        headers: vec![],
                        body_chunks: vec![
                            serde_json::json!({
                                "token": "fresh-session",
                                "expires_at": chrono::Utc::now().timestamp() + 3600,
                                "endpoints": {"api": "https://copilot-proxy.example.com/v1"}
                            })
                            .to_string(),
                        ],
                        body_chunks_base64: None,
                    },
                });
            }
            interactions.push(Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://copilot-proxy.example.com/v1/chat/completions".to_string(),
                    headers: vec![],
                    body: Some(recovery_request_body()),
                    body_text: None,
                },
                response: RecordedResponse {
                    status,
                    headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                    body_chunks: vec![body.to_string()],
                    body_chunks_base64: None,
                },
            });
        }
        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: test_name.to_string(),
            recorded_at: "2025-01-01T00:00:00Z".to_string(),
            interactions,
        };
        std::fs::write(
            temp.path().join(format!("{test_name}.json")),
            serde_json::to_string_pretty(&cassette).expect("serialize"),
        )
        .expect("write cassette");
        let client = Client::new().with_vcr(VcrRecorder::new_with(
            test_name,
            VcrMode::Playback,
            temp.path(),
        ));
        let provider = CopilotProvider::new("gpt-4o", "github-secret")
            .with_github_api_base(GITHUB_API_BASE)
            .with_client(client);
        *provider.cached_token.lock().expect("cache") = Some(cached_session("old-session"));
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let options = StreamOptions {
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(
                move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Some(recovery_request_body()) })
                },
            )),
            ..Default::default()
        };
        (provider, options, attempts, temp)
    }

    #[test]
    fn rejected_cached_session_refreshes_once_and_streams() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let (provider, options, attempts, _temp) = recovery_fixture(
                "copilot_recover_401",
                &[
                    (401, "expired"),
                    (200, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"),
                ],
            );
            let mut stream = provider.stream(&Context::default(), &options).await.expect("recovered");
            let mut done = false;
            while let Some(event) = stream.next().await {
                done |= matches!(event.expect("stream event"), StreamEvent::Done { .. });
            }
            assert!(done, "the recovered response must reach a terminal completion");
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert_eq!(provider.ensure_session_token().await.expect("cached").token, "fresh-session");
        });
    }

    #[test]
    fn repeated_rejection_is_bounded_and_evicts_rejected_refresh() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let (provider, options, attempts, _temp) = recovery_fixture(
                "copilot_repeated_401",
                &[(401, "expired"), (401, "still rejected")],
            );
            let result = provider.stream(&Context::default(), &options).await;
            let error = result.err().expect("second rejection must be returned");
            assert!(provider.session_token_rejected(&error));
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert!(provider.cached_token.lock().expect("cache").is_none());
        });
    }

    #[test]
    fn non_auth_proxy_errors_are_not_replayed() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            for status in [400, 403, 429, 500, 503] {
                let (provider, options, attempts, _temp) = recovery_fixture(
                    &format!("copilot_no_replay_{status}"),
                    &[(status, "upstream body mentions HTTP 401")],
                );
                let error = provider
                    .stream(&Context::default(), &options)
                    .await
                    .err()
                    .expect("error");
                assert!(!provider.session_token_rejected(&error));
                assert_eq!(attempts.load(Ordering::SeqCst), 1);
                assert_eq!(
                    provider.ensure_session_token().await.expect("cached").token,
                    "old-session"
                );
            }
        });
    }

    #[test]
    fn established_stream_errors_never_trigger_reauthentication() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let (provider, options, attempts, _temp) = recovery_fixture(
                "copilot_no_stream_replay",
                &[(200, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\nevent: error\ndata: {\"error\":{\"message\":\"HTTP 401\"}}\n\n")],
            );
            let mut stream = provider.stream(&Context::default(), &options).await.expect("accepted");
            let mut completed = false;
            while let Some(event) = stream.next().await {
                completed |= matches!(event, Ok(StreamEvent::Done { .. }));
            }
            assert!(!completed, "a broken stream must not masquerade as completion");
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert_eq!(provider.ensure_session_token().await.expect("cached").token, "old-session");
        });
    }

    #[test]
    fn session_response_requires_header_safe_unexpired_credentials() {
        let now = 1_700_000_000;
        for token in [
            "",
            " ",
            " secret",
            "secret ",
            "secret\tvalue",
            "secret\r\nInjected: yes",
            "secret\u{7f}",
            "sëcret",
        ] {
            let body = serde_json::json!({"token": token, "expires_at": now + 3600}).to_string();
            let error = parse_session_token_response(&body, now).expect_err("unsafe credential");
            assert!(error.to_string().contains("unusable session token"));
        }
        for expires_at in [now - 1, now, i64::MIN] {
            let body =
                serde_json::json!({"token": "secret-marker", "expires_at": expires_at}).to_string();
            let error = parse_session_token_response(&body, now).expect_err("expired credential");
            assert!(error.to_string().contains("expired session token"));
            assert!(!error.to_string().contains("secret-marker"));
        }
        // Opaque Copilot tokens may contain separators. A live token inside
        // the refresh margin can serve this request; it is not yet expired.
        let body = serde_json::json!({
            "token": "tid=test;exp=1700000001;sku=copilot;sig=a+b/c=",
            "expires_at": now + 1
        })
        .to_string();
        let parsed = parse_session_token_response(&body, now).expect("valid opaque token");
        assert_eq!(parsed.expires_at, now + 1);
    }

    #[test]
    fn malformed_session_responses_do_not_echo_secret_values() {
        let now = 1_700_000_000;
        for body in [
            r#"{"token":"secret-marker","expires_at":"secret-marker"}"#,
            r#"{"token":["secret-marker"],"expires_at":1700003600}"#,
            r#"{"token":"secret-marker","expires_at":1700003600,"endpoints":{"api":42}}"#,
            "secret-marker is not JSON",
        ] {
            let error = parse_session_token_response(body, now).expect_err("malformed response");
            assert!(error.to_string().contains("Invalid Copilot token response"));
            assert!(!error.to_string().contains("secret-marker"));
        }
    }

    #[test]
    fn oversized_token_text_is_rejected_without_parsing_or_echoing_it() {
        let now = 1_700_000_000;
        // Well-formed and otherwise acceptable: without the size guard this
        // body parses into a usable token, so reaching `expect_err` is the
        // guard doing the work and nothing else.
        let body = serde_json::json!({
            "token": "secret-marker",
            "expires_at": now + 3600,
            "padding": "x".repeat(MAX_TOKEN_RESPONSE_BYTES)
        })
        .to_string();
        assert!(body.len() > MAX_TOKEN_RESPONSE_BYTES);
        let error = parse_session_token_response(&body, now).expect_err("oversized response");
        assert!(error.to_string().contains("size limit"));
        assert!(!error.to_string().contains("secret-marker"));
    }

    #[test]
    fn secret_bearing_debug_values_are_redacted() {
        let response = parse_session_token_response(
            r#"{"token":"secret-marker","expires_at":1700003600,"endpoints":{"api":"https://example.com/?key=url-secret"}}"#,
            1_700_000_000,
        ).expect("response");
        let cached = CachedToken {
            token: response.token.clone(),
            expires_at: response.expires_at,
            api_endpoint: response.endpoints.api.clone(),
            github_credential: github_credential_identity("github-secret"),
        };
        for debug in [format!("{response:?}"), format!("{cached:?}")] {
            assert!(debug.contains("[REDACTED]"));
            assert!(!debug.contains("secret-marker"));
            assert!(!debug.contains("url-secret"));
        }
    }

    #[test]
    fn invalid_exchange_credentials_never_populate_the_cache() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let now = chrono::Utc::now().timestamp();
            for (index, (token, expires_at)) in [
                ("", now + 3600),
                ("secret\r\nInjected: yes", now + 3600),
                ("expired-secret", now - 1),
            ]
            .into_iter()
            .enumerate()
            {
                let (client, _temp) = vcr_token_exchange_client(
                    &format!("copilot_invalid_token_{index}"),
                    token,
                    expires_at,
                    "",
                );
                let provider = CopilotProvider::new("gpt-4o", "github-secret")
                    .with_github_api_base(GITHUB_API_BASE)
                    .with_client(client);
                assert!(provider.ensure_session_token().await.is_err());
                assert!(provider.cached_token.lock().expect("cache").is_none());
            }
        });
    }

    #[test]
    fn oversized_exchange_response_is_rejected_before_caching() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let body = serde_json::json!({
                "token": "secret-marker",
                "expires_at": chrono::Utc::now().timestamp() + 3600,
                "padding": "x".repeat(MAX_TOKEN_RESPONSE_BYTES)
            })
            .to_string();
            let (client, _temp) = vcr_raw_token_client("copilot_oversized_token", 200, body);
            let provider = CopilotProvider::new("gpt-4o", "github-secret")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
            let error = provider
                .ensure_session_token()
                .await
                .expect_err("oversized response");
            // The read is bounded before the parser ever sees a byte, so an
            // oversized body is refused by `text_limited`, not by the size
            // guard in `parse_session_token_response`. This assertion used to
            // name the guard's message, which is why it has failed since the
            // day it was written. What the test is actually for holds either
            // way and is asserted below: refused before caching, and the body
            // never copied into the error.
            // `oversized_token_text_is_rejected_without_parsing_or_echoing_it`
            // covers the guard itself.
            assert!(
                error
                    .to_string()
                    .contains("Failed to read Copilot token response"),
                "unexpected error: {error}"
            );
            assert!(!error.to_string().contains("secret-marker"));
            assert!(provider.cached_token.lock().expect("cache").is_none());
        });
    }

    #[test]
    fn failed_exchange_responses_do_not_disclose_response_bodies() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            for status in [401, 403, 429, 500] {
                let (client, _temp) = vcr_raw_token_client(
                    &format!("copilot_secret_error_{status}"),
                    status,
                    "github-secret session-secret".repeat(MAX_TOKEN_RESPONSE_BYTES / 16),
                );
                let provider = CopilotProvider::new("gpt-4o", "github-secret")
                    .with_github_api_base(GITHUB_API_BASE)
                    .with_client(client);
                let error = provider
                    .ensure_session_token()
                    .await
                    .expect_err("exchange rejected");
                let message = error.to_string();
                assert!(message.contains(&format!("HTTP {status}")));
                assert!(!message.contains("github-secret"));
                assert!(!message.contains("session-secret"));
                assert!(provider.cached_token.lock().expect("cache").is_none());
            }
        });
    }

    #[test]
    fn failed_reauthentication_never_reuses_the_rejected_session() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            // No GET interaction is available: refresh fails in playback,
            // rather than falling through to a live authentication call.
            let (provider, options, attempts, _temp) =
                recovery_fixture("copilot_failed_refresh", &[(401, "expired")]);
            let error = provider
                .stream(&Context::default(), &options)
                .await
                .err()
                .expect("refresh failure");
            assert!(error.to_string().contains("Copilot token exchange failed"));
            assert!(!error.to_string().contains("github-secret"));
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert!(provider.cached_token.lock().expect("cache").is_none());
        });
    }

    #[test]
    fn copilot_forwards_model_metadata_and_reloads_rotated_auth_storage_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"github-copilot":{"type":"api_key","key":"ghu_rotated_live_token"}}"#,
        )
        .expect("write auth.json");
        let auth = crate::auth::AuthStorage::load(auth_path).expect("load auth");

        let cost = crate::provider::ModelCost {
            input: 2.5,
            output: 10.0,
            cache_read: 1.25,
            cache_write: 0.0,
            audio: None,
        };
        let provider = CopilotProvider::new("o3-mini", "ghu_stale_startup_token")
            .with_reasoning(true)
            .with_input_types(&[crate::provider::InputType::Text])
            .with_model_cost(cost.clone())
            .with_auth_storage(&auth)
            .expect("bind auth storage");

        assert!(provider.reasoning);
        assert_eq!(provider.model_cost(), Some(cost));
        assert!(
            !provider.capabilities().vision,
            "text-only input_types must report vision=false"
        );
        assert_eq!(
            asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime")
                .block_on(provider.effective_github_token())
                .expect("live credential"),
            "ghu_rotated_live_token",
            "live auth.json credential must override stale startup token on session refresh"
        );
    }

    fn isolated_bound_auth_test(name: &str, pool: Option<&str>) -> bool {
        const CHILD: &str = "PI_COPILOT_BOUND_AUTH_TEST_CHILD";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        // Keep ambient credentials, proxies and other tests' rotation rings out
        // of these cases without mutating the shared libtest environment.
        let _guard = crate::jobs::process_test_guard();
        let directory = tempfile::tempdir().expect("auth fixture logs");
        let log_path = directory.path().join("output.log");
        let log = std::fs::File::create(&log_path).expect("auth fixture log");
        let owner = crate::agent_cx::AgentCx::for_request();
        let mut command = owner
            .process()
            .command(std::env::current_exe().expect("test executable"));
        command
            .current_dir(directory.path())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(
                log.try_clone().expect("clone log"),
            ))
            .stderr(std::process::Stdio::from(log));
        for variable in [
            "GITHUB_COPILOT_API_KEY",
            "GITHUB_COPILOT_API_KEYS",
            "GITHUB_TOKEN",
            "GITHUB_TOKENS",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(variable);
        }
        if let Some(pool) = pool {
            command.env("GITHUB_COPILOT_API_KEYS", pool);
        }
        let mut child = command.spawn().expect("isolated auth fixture");
        drop(command);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().expect("observe auth fixture") {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("terminate timed-out auth fixture");
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(&log_path).expect("read auth fixture log");
        assert!(
            status.is_some_and(|status| status.success()),
            "Copilot auth fixture failed or timed out: {status:?}\n{output}"
        );
        assert!(output.contains("1 passed"), "fixture did not run: {output}");
        true
    }

    fn save_bound_auth(path: &std::path::Path, token: Option<&str>) {
        let mut auth = crate::auth::AuthStorage::empty_at(path.to_path_buf());
        if let Some(token) = token {
            auth.set(
                "github-copilot",
                crate::auth::AuthCredential::BearerToken {
                    token: token.to_string(),
                },
            );
        }
        auth.save().expect("save bound auth");
    }

    fn copilot_entry(key: &str) -> crate::models::ModelEntry {
        let mut entry = crate::models::ad_hoc_model_entry("github-copilot", "gpt-4o")
            .expect("native Copilot model");
        entry.api_key = Some(key.to_string());
        entry
    }

    fn configured_copilot_entry(directory: &std::path::Path) -> crate::models::ModelEntry {
        let path = directory.join("models.json");
        std::fs::write(
            &path,
            r#"{"providers":{"github-copilot":{"apiKey":"catalog-fallback","models":[{"id":"gpt-4o"}]}}}"#,
        )
        .expect("configured catalog fallback");
        crate::models::ModelRegistry::load_with_credential_resolver(Some(path), |_| None)
            .find("github-copilot", "gpt-4o")
            .expect("configured Copilot model")
    }

    /// A real loopback exchange verifies both the selected GitHub credential
    /// and the number of requests. The callback changes auth while the first
    /// exchange is in flight, before its response can authorize a completion.
    fn bound_auth_exchange_server(
        exchanges: Vec<(&'static str, &'static str)>,
        after_first_request: impl FnOnce() + Send + 'static,
    ) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind exchange fixture");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut after_first_request = Some(after_first_request);
            for (expected_github, session) in exchanges {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "exchange accept deadline");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("exchange accept failed: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .expect("fixture read poll");
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("fixture write timeout");
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    assert!(Instant::now() < deadline, "exchange request deadline");
                    assert!(request.len() < 64 * 1024, "exchange headers exceed bound");
                    let mut chunk = [0u8; 2048];
                    match socket.read(&mut chunk) {
                        Ok(0) => panic!("exchange request closed before headers"),
                        Ok(count) => request.extend_from_slice(&chunk[..count]),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => panic!("exchange request failed: {error}"),
                    }
                }
                let request = String::from_utf8(request).expect("HTTP headers");
                assert!(request.starts_with("GET /copilot_internal/v2/token HTTP/1.1\r\n"));
                assert!(
                    request.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("authorization")
                                && value.trim() == format!("token {expected_github}")
                        })
                    }),
                    "exchange must use the current GitHub credential"
                );
                counter.fetch_add(1, Ordering::SeqCst);
                if let Some(change_auth) = after_first_request.take() {
                    change_auth();
                }
                let body = serde_json::json!({
                    "token": session,
                    "expires_at": chrono::Utc::now().timestamp() + 3600,
                    "endpoints": {"api": "https://copilot-proxy.example.com/v1"}
                })
                .to_string();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("exchange response");
            }
        });
        (endpoint, requests, server)
    }

    #[test]
    fn bound_auth_updates_revoke_cached_bearers() {
        if isolated_bound_auth_test(
            "providers::copilot::tests::bound_auth_updates_revoke_cached_bearers",
            None,
        ) {
            return;
        }
        let directory = tempfile::tempdir().expect("auth source");
        let path = directory.path().join("auth.json");
        save_bound_auth(&path, Some("github-old"));
        let auth = crate::auth::AuthStorage::load(path.clone()).expect("auth");
        let (endpoint, requests, server) = bound_auth_exchange_server(
            vec![("github-old", "old-session"), ("github-new", "new-session")],
            || {},
        );
        let provider = super::super::create_copilot_provider(
            &copilot_entry("github-old"),
            Some(&auth),
            Client::new(),
        )
        .expect("bind auth")
        .with_github_api_base(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            assert_eq!(
                provider.ensure_session_token().await.expect("first").token,
                "old-session"
            );
            assert_eq!(
                provider.ensure_session_token().await.expect("cached").token,
                "old-session"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 1);

            save_bound_auth(&path, Some("github-new"));
            assert_eq!(
                provider
                    .ensure_session_token()
                    .await
                    .expect("rotated")
                    .token,
                "new-session"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 2);

            save_bound_auth(&path, None);
            let error = provider
                .ensure_session_token()
                .await
                .expect_err("logout revokes bearer");
            assert!(error.to_string().contains("no longer available"));
            assert!(!error.to_string().contains("github-old"));
            assert!(provider.cached_token.lock().expect("cache").is_none());
            assert_eq!(requests.load(Ordering::SeqCst), 2);

            let invalid_source = crate::auth::AuthStorage::empty_at(directory.path().to_path_buf());
            let invalid_provider = CopilotProvider::new("gpt-4o", "stale-startup-secret")
                .with_auth_storage(&invalid_source)
                .expect("bind unreadable source");
            *invalid_provider.cached_token.lock().expect("cache") =
                Some(cached_session("cached-secret"));
            let error = invalid_provider
                .ensure_session_token()
                .await
                .expect_err("unreadable source revokes bearer");
            assert!(error.to_string().contains("Unable to read"));
            assert!(!error.to_string().contains("secret"));
            assert!(
                invalid_provider
                    .cached_token
                    .lock()
                    .expect("cache")
                    .is_none()
            );
        });
        server.join().expect("exchange fixture");
    }

    #[test]
    fn factory_keeps_stale_and_source_only_credentials_bound() {
        if isolated_bound_auth_test(
            "providers::copilot::tests::factory_keeps_stale_and_source_only_credentials_bound",
            None,
        ) {
            return;
        }
        let directory = tempfile::tempdir().expect("auth source");
        let path = directory.path().join("auth.json");
        let entry = copilot_entry("github-old");
        let (endpoint, requests, server) = bound_auth_exchange_server(
            vec![("github-new", "new-session"), ("github-new", "new-session")],
            || {},
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            for source_only in [false, true] {
                // The catalog still holds A; the source changed to B before
                // provider construction. Neither a new snapshot nor an empty
                // source-only binding may turn captured A into an explicit pin.
                save_bound_auth(&path, Some("github-new"));
                let auth = if source_only {
                    crate::auth::AuthStorage::empty_at(path.clone())
                } else {
                    crate::auth::AuthStorage::load(path.clone()).expect("current snapshot")
                };
                let provider =
                    super::super::create_copilot_provider(&entry, Some(&auth), Client::new())
                        .expect("retain source authority")
                        .with_github_api_base(&endpoint);
                assert_eq!(
                    provider
                        .ensure_session_token()
                        .await
                        .expect("new account")
                        .token,
                    "new-session"
                );

                save_bound_auth(&path, None);
                let removed = crate::auth::AuthStorage::load(path.clone()).expect("removed source");
                let rebuilt =
                    super::super::create_copilot_provider(&entry, Some(&removed), Client::new())
                        .expect("retain removed source authority")
                        .with_github_api_base(&endpoint);
                for stale in [&provider, &rebuilt] {
                    let error = stale
                        .ensure_session_token()
                        .await
                        .expect_err("logout fences snapshots");
                    assert!(error.to_string().contains("no longer available"));
                    assert!(stale.cached_token.lock().expect("cache").is_none());
                }
            }
        });
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.join().expect("exchange fixture");
    }

    #[test]
    fn catalog_fallback_preserves_managed_precedence() {
        if isolated_bound_auth_test(
            "providers::copilot::tests::catalog_fallback_preserves_managed_precedence",
            None,
        ) {
            return;
        }
        let directory = tempfile::tempdir().expect("auth source");
        let path = directory.path().join("auth.json");
        let entry = configured_copilot_entry(directory.path());
        assert_eq!(
            entry.configured_api_key_fallback(),
            Some("catalog-fallback")
        );
        save_bound_auth(&path, Some("github-managed"));
        // Interactive model switching supplies this source identity without
        // requiring a readable credential snapshot during construction.
        let auth = crate::auth::AuthStorage::empty_at(path.clone());
        let (endpoint, requests, server) = bound_auth_exchange_server(
            vec![
                ("github-managed", "managed-session"),
                ("catalog-fallback", "fallback-session"),
            ],
            || {},
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let provider =
                super::super::create_copilot_provider(&entry, Some(&auth), Client::new())
                    .expect("bind configured fallback")
                    .with_github_api_base(endpoint);
            assert_eq!(
                provider
                    .ensure_session_token()
                    .await
                    .expect("stored credential wins")
                    .token,
                "managed-session"
            );
            save_bound_auth(&path, None);
            assert_eq!(
                provider
                    .ensure_session_token()
                    .await
                    .expect("explicitly configured fallback")
                    .token,
                "fallback-session"
            );
            assert_eq!(
                provider
                    .ensure_session_token()
                    .await
                    .expect("cached configured fallback")
                    .token,
                "fallback-session"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 2);

            let mut replaced = entry.clone();
            replaced.api_key = Some("later-managed-snapshot".to_string());
            let replaced =
                super::super::create_copilot_provider(&replaced, Some(&auth), Client::new())
                    .expect("bind replaced model key");
            let error = replaced
                .ensure_session_token()
                .await
                .expect_err("stale marker cannot authorize a replacement");
            assert!(error.to_string().contains("no longer available"));

            let unreadable = crate::auth::AuthStorage::empty_at(directory.path().to_path_buf());
            let provider =
                super::super::create_copilot_provider(&entry, Some(&unreadable), Client::new())
                    .expect("retain unreadable source binding");
            let error = provider
                .ensure_session_token()
                .await
                .expect_err("read failure");
            assert!(error.to_string().contains("Unable to read"));
            assert!(!error.to_string().contains("catalog-fallback"));
        });
        server.join().expect("exchange fixture");
    }

    #[test]
    fn bound_auth_pool_exhaustion_rejects_cached_bearer() {
        if isolated_bound_auth_test(
            "providers::copilot::tests::bound_auth_pool_exhaustion_rejects_cached_bearer",
            Some("pool-first,pool-second"),
        ) {
            return;
        }
        let directory = tempfile::tempdir().expect("auth source");
        let path = directory.path().join("auth.json");
        save_bound_auth(&path, None);
        let auth = crate::auth::AuthStorage::load(path).expect("auth");
        let captured = auth
            .resolve_api_key_status("github-copilot", None)
            .into_result("github-copilot")
            .expect("healthy pool")
            .expect("configured pool");
        // Keep the cooldown beyond the fixture deadline, including on loaded
        // runners. The key selected during catalog loading is now unavailable.
        for _ in 0..6 {
            crate::auth::report_provider_rate_limit("github-copilot", &captured);
        }
        let healthy = match captured.as_str() {
            "pool-first" => "pool-second",
            "pool-second" => "pool-first",
            _ => panic!("unexpected fixture credential"),
        };
        let (endpoint, requests, server) = bound_auth_exchange_server(
            vec![
                (healthy, "pooled-session"),
                ("configured-pin", "pinned-session"),
            ],
            || {},
        );
        let entry = copilot_entry(&captured);
        let provider = super::super::create_copilot_provider(&entry, Some(&auth), Client::new())
            .expect("bind cooled catalog snapshot")
            .with_github_api_base(&endpoint);
        let configured_entry = configured_copilot_entry(directory.path());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            assert_eq!(
                provider
                    .ensure_session_token()
                    .await
                    .expect("healthy sibling")
                    .token,
                "pooled-session"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            for _ in 0..6 {
                crate::auth::report_provider_rate_limit("github-copilot", healthy);
            }
            let rebuilt = super::super::create_copilot_provider(&entry, Some(&auth), Client::new())
                .expect("bind captured member of exhausted pool")
                .with_github_api_base(&endpoint);
            let catalog_fallback = super::super::create_copilot_provider(
                &configured_entry,
                Some(&auth),
                Client::new(),
            )
            .expect("bind catalog fallback below managed pool");
            for managed in [&provider, &rebuilt, &catalog_fallback] {
                let error = managed
                    .ensure_session_token()
                    .await
                    .expect_err("exhaustion must refuse cached or captured pool credentials");
                assert!(
                    error
                        .to_string()
                        .contains(crate::auth::CREDENTIAL_POOL_EXHAUSTED)
                );
                assert!(!error.to_string().contains(&captured));
                assert!(managed.cached_token.lock().expect("cache").is_none());
            }
            assert_eq!(requests.load(Ordering::SeqCst), 1);

            // A direct constructor without a bound source is an explicit pin.
            // Unlike a catalog fallback, it has no managed pool to consult.
            let pinned =
                CopilotProvider::new("gpt-4o", "configured-pin").with_github_api_base(endpoint);
            assert_eq!(
                pinned
                    .ensure_session_token()
                    .await
                    .expect("explicit pin")
                    .token,
                "pinned-session"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 2);
        });
        server.join().expect("exchange fixture");
    }

    #[test]
    fn inflight_auth_changes_cannot_publish_stale_bearers() {
        if isolated_bound_auth_test(
            "providers::copilot::tests::inflight_auth_changes_cannot_publish_stale_bearers",
            None,
        ) {
            return;
        }
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        for next_credential in [Some("github-new"), None] {
            let directory = tempfile::tempdir().expect("auth source");
            let path = directory.path().join("auth.json");
            save_bound_auth(&path, Some("github-old"));
            let auth = crate::auth::AuthStorage::load(path.clone()).expect("auth");
            let mut exchanges = vec![("github-old", "abandoned-session")];
            if next_credential.is_some() {
                exchanges.push(("github-new", "new-session"));
            }
            let (endpoint, requests, server) = bound_auth_exchange_server(exchanges, move || {
                save_bound_auth(&path, next_credential);
            });
            let provider = CopilotProvider::new("gpt-4o", "stale-startup-secret")
                .with_auth_storage(&auth)
                .expect("bind auth")
                .with_github_api_base(endpoint);
            runtime.block_on(async {
                let (first, waiter) = futures::join!(
                    provider.ensure_session_token(),
                    provider.ensure_session_token(),
                );
                assert!(
                    first.is_err(),
                    "an abandoned exchange cannot authorize a completion"
                );
                if next_credential.is_some() {
                    assert_eq!(
                        waiter.expect("waiter uses new account").token,
                        "new-session"
                    );
                    assert_eq!(
                        provider
                            .ensure_session_token()
                            .await
                            .expect("new cached account")
                            .token,
                        "new-session"
                    );
                    assert_eq!(requests.load(Ordering::SeqCst), 2);
                } else {
                    assert!(waiter.is_err(), "logout also fences queued refreshes");
                    assert!(provider.cached_token.lock().expect("cache").is_none());
                    assert_eq!(requests.load(Ordering::SeqCst), 1);
                }
            });
            server.join().expect("exchange fixture");
        }
    }
}
