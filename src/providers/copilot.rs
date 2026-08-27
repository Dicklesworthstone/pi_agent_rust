//! GitHub Copilot provider implementation.
//!
//! Copilot uses a two-step authentication flow:
//! 1. Exchange a GitHub OAuth/PAT token for a short-lived Copilot session token
//!    via `https://api.github.com/copilot_internal/v2/token`.
//! 2. Use the session token to make OpenAI-compatible chat completion requests
//!    to the Copilot proxy endpoint.
//!
//! The session token is cached and automatically refreshed when it expires.
//! GitHub Enterprise Server is supported via a configurable base URL.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamEvent, StreamOptions};
use async_trait::async_trait;
use futures::Stream;
use serde::Deserialize;
use std::pin::Pin;
use std::sync::Mutex;

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

// ── Token exchange types ─────────────────────────────────────────

/// Response from the Copilot token exchange endpoint.
#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    /// The short-lived session token.
    token: String,
    /// Unix timestamp (seconds) when the token expires.
    expires_at: i64,
    /// Endpoints returned by the API.
    #[serde(default)]
    endpoints: CopilotEndpoints,
}

/// Endpoint URLs returned alongside the session token.
#[derive(Debug, Default, Deserialize)]
struct CopilotEndpoints {
    /// The API endpoint for chat completions.
    #[serde(default)]
    api: String,
}

/// Cached session token with expiry.
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: i64,
    api_endpoint: String,
}

// ── Provider ─────────────────────────────────────────────────────

/// GitHub Copilot provider that wraps OpenAI-compatible streaming.
pub struct CopilotProvider {
    /// HTTP client for token exchange and API requests.
    client: Client,
    /// The GitHub OAuth token or PAT used for token exchange.
    github_token: String,
    /// The model ID to request (e.g., "gpt-4o", "claude-3.5-sonnet").
    model: String,
    /// GitHub API base URL (supports Enterprise: `https://github.example.com/api/v3`).
    github_api_base: String,
    /// Provider name for event attribution.
    provider_name: String,
    /// Compatibility overrides passed to the underlying OpenAI provider.
    compat: Option<CompatConfig>,
    /// Cached session token (refreshed automatically).
    cached_token: Mutex<Option<CachedToken>>,
}

impl CopilotProvider {
    /// Create a new Copilot provider.
    pub fn new(model: impl Into<String>, github_token: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            github_token: github_token.into(),
            model: model.into(),
            github_api_base: GITHUB_API_BASE.to_string(),
            provider_name: "github-copilot".to_string(),
            compat: None,
            cached_token: Mutex::new(None),
        }
    }

    /// Set the GitHub API base URL (for Enterprise).
    #[must_use]
    pub fn with_github_api_base(mut self, base: impl Into<String>) -> Self {
        self.github_api_base = base.into();
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

    /// Get a valid session token, refreshing if necessary.
    async fn ensure_session_token(&self) -> Result<CachedToken> {
        // Check cache first.
        {
            let guard = self
                .cached_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = &*guard {
                let now = chrono::Utc::now().timestamp();
                if cached.expires_at > now + TOKEN_REFRESH_MARGIN_SECS {
                    return Ok(cached.clone());
                }
            }
        }

        // Exchange GitHub token for a Copilot session token.
        let token_url = format!(
            "{}/copilot_internal/v2/token",
            self.github_api_base.trim_end_matches('/')
        );

        let request = self
            .client
            .get(&token_url)
            .header("Authorization", format!("token {}", self.github_token))
            .header("Accept", "application/json")
            .header("Editor-Version", copilot_editor_version())
            .header("User-Agent", copilot_user_agent())
            .header("X-Github-Api-Version", github_api_version());

        let response = Box::pin(request.send())
            .await
            .map_err(|e| Error::auth(format!("Copilot token exchange failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_string());

        if !(200..300).contains(&status) {
            return Err(Error::auth(format!(
                "Copilot token exchange failed (HTTP {status}). \
                 Verify your GitHub token has Copilot access. Response: {text}"
            )));
        }

        let token_response: CopilotTokenResponse = serde_json::from_str(&text)
            .map_err(|e| Error::auth(format!("Invalid Copilot token response: {e}")))?;

        // Determine the API endpoint.
        let api_endpoint = if token_response.endpoints.api.is_empty() {
            // Fallback: use the standard Copilot proxy URL.
            "https://api.githubcopilot.com/chat/completions".to_string()
        } else {
            let base = token_response.endpoints.api.trim_end_matches('/');
            if base.ends_with("/chat/completions") {
                base.to_string()
            } else {
                format!("{base}/chat/completions")
            }
        };

        let cached = CachedToken {
            token: token_response.token,
            expires_at: token_response.expires_at,
            api_endpoint,
        };

        // Store in cache.
        {
            let mut guard = self
                .cached_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(cached.clone());
        }

        Ok(cached)
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

    #[allow(clippy::too_many_lines)]
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Get a valid session token.
        let session = self.ensure_session_token().await?;

        // Build an OpenAI provider pointed at the Copilot endpoint.
        let inner = OpenAIProvider::new(&self.model)
            .with_provider_name(&self.provider_name)
            .with_base_url(&session.api_endpoint)
            .with_compat(self.compat.clone())
            .with_client(self.client.clone());

        // Override the authorization: Copilot uses the session token,
        // not the GitHub OAuth token. The clone carries
        // `before_provider_request` through, so the inner OpenAI route's
        // rewrite hook wiring covers Copilot too (bd-dzddo).
        let mut copilot_options = options.clone();
        copilot_options.api_key = Some(session.token);

        // Add Copilot-specific headers.
        copilot_options
            .headers
            .insert("Editor-Version".to_string(), copilot_editor_version());
        copilot_options
            .headers
            .insert("User-Agent".to_string(), copilot_user_agent());
        copilot_options
            .headers
            .insert("X-Github-Api-Version".to_string(), github_api_version());
        copilot_options.headers.insert(
            "Copilot-Integration-Id".to_string(),
            "vscode-chat".to_string(),
        );

        inner.stream(context, &copilot_options).await
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcr::{
        Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder,
    };

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
        };
        assert_eq!(cloned.token, "session-tok");
        assert_eq!(cloned.expires_at, 99999);
    }

    /// Build a VCR client that returns a successful token exchange response.
    fn vcr_token_exchange_client(
        test_name: &str,
        token: &str,
        expires_at: i64,
        api_endpoint: &str,
    ) -> (Client, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let response_body = serde_json::json!({
            "token": token,
            "expires_at": expires_at,
            "endpoints": {
                "api": api_endpoint
            }
        })
        .to_string();
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
                    status: 200,
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
}
