//! GitHub Copilot provider implementation.
//!
//! Copilot uses a two-step authentication flow:
//! 1. Exchange a GitHub OAuth/PAT token for a short-lived Copilot session token
//!    via `https://api.github.com/copilot_internal/v2/token`.
//! 2. Use the session token with the model's declared Chat Completions,
//!    Responses, or Anthropic Messages transport at the Copilot proxy endpoint.
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
use url::Url;

use super::anthropic::AnthropicProvider;
use super::openai::OpenAIProvider;
use super::openai_responses::OpenAIResponsesProvider;

// ── Constants ────────────────────────────────────────────────────

/// Default GitHub API base for token exchange.
const GITHUB_API_BASE: &str = "https://api.github.com";

/// Default Copilot inference API base when token metadata omits it.
const COPILOT_API_BASE: &str = "https://api.githubcopilot.com";

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

fn configured_github_api_base() -> String {
    std::env::var("PI_COPILOT_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("GITHUB_API_URL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| GITHUB_API_BASE.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopilotTransport {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl CopilotTransport {
    fn from_api(api: &str) -> Option<Self> {
        match api.trim().to_ascii_lowercase().as_str() {
            "openai-completions" => Some(Self::ChatCompletions),
            "openai-responses" => Some(Self::Responses),
            "anthropic-messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }
}

fn normalize_copilot_api_base(raw_endpoint: &str) -> Result<String> {
    let endpoint = if raw_endpoint.trim().is_empty() {
        COPILOT_API_BASE
    } else {
        raw_endpoint.trim()
    };
    let mut parsed = Url::parse(endpoint)
        .map_err(|err| Error::auth(format!("Invalid Copilot API endpoint: {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::auth(
            "Invalid Copilot API endpoint: expected an HTTP(S) URL with a host",
        ));
    }

    let mut path = parsed.path().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if path.ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
            break;
        }
    }
    parsed.set_path(if path.is_empty() { "/" } else { &path });
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn copilot_api_endpoint(api_base: &str, transport: CopilotTransport) -> Result<String> {
    let mut parsed = Url::parse(api_base)
        .map_err(|err| Error::auth(format!("Invalid cached Copilot API endpoint: {err}")))?;
    let base_path = parsed.path().trim_end_matches('/');
    let suffix = match transport {
        CopilotTransport::ChatCompletions => "chat/completions",
        CopilotTransport::Responses => "responses",
        CopilotTransport::AnthropicMessages if base_path.ends_with("/v1") => "messages",
        CopilotTransport::AnthropicMessages => "v1/messages",
    };
    let path = if base_path.is_empty() {
        format!("/{suffix}")
    } else {
        format!("{base_path}/{suffix}")
    };
    parsed.set_path(&path);
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
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
    /// The inference API base or a transport-specific endpoint.
    #[serde(default)]
    api: String,
}

/// Cached session token with expiry.
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: i64,
    api_base: String,
}

// ── Provider ─────────────────────────────────────────────────────

/// GitHub Copilot provider with transport-aware streaming.
pub struct CopilotProvider {
    /// HTTP client for token exchange and API requests.
    client: Client,
    /// The GitHub OAuth token or PAT used for token exchange.
    github_token: String,
    /// The model ID to request (e.g., "gpt-4o", "claude-3.5-sonnet").
    model: String,
    /// API transport selected from model metadata.
    api: String,
    /// Whether the selected model supports reasoning controls.
    reasoning: bool,
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
        let model = model.into();
        Self {
            client: Client::new(),
            github_token: github_token.into(),
            api: crate::models::github_copilot_api_for_model(&model).to_string(),
            model,
            reasoning: false,
            github_api_base: configured_github_api_base(),
            provider_name: "github-copilot".to_string(),
            compat: None,
            cached_token: Mutex::new(None),
        }
    }

    /// Set the API transport declared by the model registry.
    #[must_use]
    pub fn with_api_name(mut self, api: impl Into<String>) -> Self {
        self.api = api.into();
        self
    }

    /// Set whether the selected model supports reasoning controls.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
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

    fn transport(&self) -> Result<CopilotTransport> {
        CopilotTransport::from_api(&self.api).ok_or_else(|| {
            Error::provider(
                &self.provider_name,
                format!(
                    "Unsupported GitHub Copilot API transport {:?}; expected openai-completions, openai-responses, or anthropic-messages",
                    self.api
                ),
            )
        })
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

        let api_base = normalize_copilot_api_base(&token_response.endpoints.api)?;

        let cached = CachedToken {
            token: token_response.token,
            expires_at: token_response.expires_at,
            api_base,
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
        let transport = self.transport()?;
        let session = self.ensure_session_token().await?;
        let api_endpoint = copilot_api_endpoint(&session.api_base, transport)?;

        // Override the authorization: Copilot uses the session token,
        // not the GitHub OAuth token.
        let mut copilot_options = options.clone();
        copilot_options.api_key = None;

        // Add Copilot-specific headers.
        copilot_options.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", session.token),
        );
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

        match transport {
            CopilotTransport::ChatCompletions => {
                OpenAIProvider::new(&self.model)
                    .with_reasoning(self.reasoning)
                    .with_provider_name(&self.provider_name)
                    .with_base_url(api_endpoint)
                    .with_compat(self.compat.clone())
                    .with_client(self.client.clone())
                    .stream(context, &copilot_options)
                    .await
            }
            CopilotTransport::Responses => {
                OpenAIResponsesProvider::new(&self.model)
                    .with_reasoning(self.reasoning)
                    .with_provider_name(&self.provider_name)
                    .with_api_name(&self.api)
                    .with_base_url(api_endpoint)
                    .with_compat(self.compat.clone())
                    .with_client(self.client.clone())
                    .stream(context, &copilot_options)
                    .await
            }
            CopilotTransport::AnthropicMessages => {
                AnthropicProvider::new(&self.model)
                    .with_provider_name(&self.provider_name)
                    .with_base_url(api_endpoint)
                    .with_compat(self.compat.clone())
                    .with_client(self.client.clone())
                    .stream(context, &copilot_options)
                    .await
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, UserContent, UserMessage};
    use crate::vcr::{
        Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder,
    };
    use futures::StreamExt;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    struct CapturedRequest {
        request_line: String,
        headers: HashMap<String, String>,
        body: String,
    }

    fn read_test_request(socket: &mut TcpStream) -> CapturedRequest {
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut chunk).expect("read request headers");
            assert!(count > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..count]);
        }

        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("request header boundary");
        let header_text = String::from_utf8_lossy(&bytes[..header_end]);
        let mut lines = header_text.lines();
        let request_line = lines.next().expect("request line").to_string();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = bytes[header_end + 4..].to_vec();
        while body.len() < content_length {
            let count = socket.read(&mut chunk).expect("read request body");
            assert!(count > 0, "request ended before body");
            body.extend_from_slice(&chunk[..count]);
        }

        CapturedRequest {
            request_line,
            headers,
            body: String::from_utf8_lossy(&body[..content_length]).to_string(),
        }
    }

    fn write_test_response(socket: &mut TcpStream, content_type: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .expect("write response");
        socket.flush().expect("flush response");
    }

    fn spawn_copilot_responses_server() -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        let base_url = format!("http://{addr}");
        let endpoint = base_url.clone();
        let (request_tx, request_rx) = mpsc::channel();

        std::thread::spawn(move || {
            for request_index in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept request");
                let request = read_test_request(&mut socket);
                if request_index == 0 {
                    assert!(
                        request
                            .request_line
                            .starts_with("GET /copilot_internal/v2/token "),
                        "unexpected token request: {}",
                        request.request_line
                    );
                    let body = serde_json::json!({
                        "token": "ghu_session_test",
                        "expires_at": chrono::Utc::now().timestamp() + 3600,
                        "endpoints": { "api": endpoint }
                    })
                    .to_string();
                    write_test_response(&mut socket, "application/json", &body);
                } else {
                    request_tx.send(request).expect("capture inference request");
                    let body = [
                        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"ok"}"#,
                        "",
                        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
                        "",
                    ]
                    .join("\n");
                    write_test_response(&mut socket, "text/event-stream", &body);
                }
            }
        });

        (base_url, request_rx)
    }

    #[test]
    fn test_copilot_provider_defaults() {
        let p = CopilotProvider::new("gpt-4o", "ghp_test123");
        assert_eq!(p.name(), "github-copilot");
        assert_eq!(p.api(), "openai-completions");
        assert_eq!(p.model_id(), "gpt-4o");
    }

    #[test]
    fn test_copilot_provider_selects_transport_from_model() {
        assert_eq!(
            CopilotProvider::new("gpt-5.6-terra", "ghp_test").api(),
            "openai-responses"
        );
        assert_eq!(
            CopilotProvider::new("gemini-3.7-flash", "ghp_test").api(),
            "openai-completions"
        );
        assert_eq!(
            CopilotProvider::new("claude-opus-4.8", "ghp_test").api(),
            "anthropic-messages"
        );
    }

    #[test]
    fn test_copilot_responses_stream_uses_responses_endpoint_and_session_token() {
        let (github_api_base, request_rx) = spawn_copilot_responses_server();
        let provider = CopilotProvider::new("gpt-5.6-terra", "github-token")
            .with_reasoning(true)
            .with_github_api_base(github_api_base);
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut stream = provider
                .stream(&context, &StreamOptions::default())
                .await
                .expect("Copilot Responses stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured Responses request");
        assert!(
            request.request_line.starts_with("POST /responses "),
            "unexpected inference request: {}",
            request.request_line
        );
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer ghu_session_test")
        );
        let body: serde_json::Value =
            serde_json::from_str(&request.body).expect("Responses request body");
        assert_eq!(body["model"], "gpt-5.6-terra");
        assert_eq!(body["input"][0]["role"], "user");
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
        let p = CopilotProvider::new("gpt-4o", "ghp_test").with_github_api_base(GITHUB_API_BASE);
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
            api_base: "https://example.com/v1".to_string(),
        };
        assert_eq!(cloned.token, "session-tok");
        assert_eq!(cloned.expires_at, 99999);
    }

    #[test]
    fn test_copilot_endpoint_selection() {
        for (base, transport, expected) in [
            (
                "https://copilot.example.com",
                CopilotTransport::ChatCompletions,
                "https://copilot.example.com/chat/completions",
            ),
            (
                "https://copilot.example.com/v1",
                CopilotTransport::Responses,
                "https://copilot.example.com/v1/responses",
            ),
            (
                "https://copilot.example.com",
                CopilotTransport::AnthropicMessages,
                "https://copilot.example.com/v1/messages",
            ),
            (
                "https://copilot.example.com/v1",
                CopilotTransport::AnthropicMessages,
                "https://copilot.example.com/v1/messages",
            ),
        ] {
            assert_eq!(
                copilot_api_endpoint(base, transport).expect("endpoint"),
                expected
            );
        }
    }

    #[test]
    fn test_copilot_api_base_normalization_removes_transport_suffix() {
        for (endpoint, expected) in [
            (
                "https://copilot.example.com/v1/chat/completions",
                "https://copilot.example.com/v1",
            ),
            (
                "https://copilot.example.com/responses",
                "https://copilot.example.com",
            ),
            (
                "https://copilot.example.com/v1/messages",
                "https://copilot.example.com/v1",
            ),
        ] {
            assert_eq!(
                normalize_copilot_api_base(endpoint).expect("normalized base"),
                expected
            );
        }
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
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy_token")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
            let cached = provider
                .ensure_session_token()
                .await
                .expect("token exchange");
            assert_eq!(cached.token, "ghu_session_test");
            assert_eq!(cached.expires_at, far_future);
            assert_eq!(cached.api_base, "https://copilot-proxy.example.com/v1");
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
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
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
            let provider = CopilotProvider::new("gpt-4o", "ghp_bad_token")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
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
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
            let cached = provider.ensure_session_token().await.expect("fallback");
            assert_eq!(cached.api_base, "https://api.githubcopilot.com");
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
            let provider = CopilotProvider::new("gpt-4o", "ghp_dummy")
                .with_github_api_base(GITHUB_API_BASE)
                .with_client(client);
            let cached = provider
                .ensure_session_token()
                .await
                .expect("full endpoint");
            assert_eq!(cached.api_base, "https://custom.proxy.com");
        });
    }
}
