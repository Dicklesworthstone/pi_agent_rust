//! HTTP/network connector with policy-gated access.
//!
//! Provides basic fetch (GET/POST) with:
//! - Host allowlist/denylist
//! - TLS required by default
//! - Request timeouts and size limits
//! - Structured logging for audit trail

use super::{
    Connector, HostCallErrorCode, HostCallPayload, HostResultPayload, host_result_err,
    host_result_err_with_details, host_result_ok,
};
use crate::error::Result;
use crate::http::client::Client;
use asupersync::time::{timeout, wall_now};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Validation error with error code and message.
type ValidationError = (HostCallErrorCode, String);

/// Configuration for the HTTP connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConnectorConfig {
    /// Host patterns to allow (glob-style: "*.example.com", "api.github.com")
    #[serde(default)]
    pub allowlist: Vec<String>,

    /// Host patterns to deny (takes precedence over allowlist)
    #[serde(default)]
    pub denylist: Vec<String>,

    /// Require TLS for all requests (default: true)
    #[serde(default = "default_require_tls")]
    pub require_tls: bool,

    /// Maximum request body size in bytes (default: 10MB)
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,

    /// Maximum response body size in bytes (default: 50MB)
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,

    /// Default timeout in milliseconds (default: 30000)
    #[serde(default = "default_timeout_ms")]
    pub default_timeout_ms: u64,
}

const fn default_require_tls() -> bool {
    true
}

const fn default_max_request_bytes() -> usize {
    10 * 1024 * 1024 // 10MB
}

const fn default_max_response_bytes() -> usize {
    50 * 1024 * 1024 // 50MB
}

const fn default_timeout_ms() -> u64 {
    30_000 // 30 seconds
}

impl Default for HttpConnectorConfig {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            denylist: Vec::new(),
            require_tls: default_require_tls(),
            max_request_bytes: default_max_request_bytes(),
            max_response_bytes: default_max_response_bytes(),
            default_timeout_ms: default_timeout_ms(),
        }
    }
}

/// HTTP request parameters from hostcall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    /// The URL to fetch
    pub url: String,

    /// HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS)
    #[serde(default = "default_method")]
    pub method: String,

    /// Request headers
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Request body (for POST, PUT, PATCH)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    /// Request body as bytes (base64-encoded)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<String>,

    /// Override timeout in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// HTTP response returned to extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    /// HTTP status code
    pub status: u16,

    /// Response headers
    pub headers: HashMap<String, String>,

    /// Response body as string (if text)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    /// Response body as bytes (base64-encoded, if binary)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<String>,

    /// Response body size in bytes
    pub size_bytes: usize,

    /// Request duration in milliseconds
    pub duration_ms: u64,
}

/// HTTP connector for extension hostcalls.
pub struct HttpConnector {
    config: HttpConnectorConfig,
    client: Client,
}

impl HttpConnector {
    /// Create a new HTTP connector with the given configuration.
    #[must_use]
    pub fn new(config: HttpConnectorConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    /// Create a new HTTP connector with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(HttpConnectorConfig::default())
    }

    /// Validate a URL against the policy.
    fn validate_url(&self, url: &str) -> std::result::Result<(), ValidationError> {
        // Parse URL to extract host
        let parsed = url::Url::parse(url).map_err(|e| {
            (
                HostCallErrorCode::InvalidRequest,
                format!("Invalid URL: {e}"),
            )
        })?;

        // Check scheme (TLS requirement)
        let scheme = parsed.scheme();
        if self.config.require_tls && scheme != "https" {
            return Err((
                HostCallErrorCode::Denied,
                format!("TLS required: URL scheme must be 'https', got '{scheme}'"),
            ));
        }

        // HTTP/HTTPS only
        if scheme != "http" && scheme != "https" {
            return Err((
                HostCallErrorCode::InvalidRequest,
                format!("Unsupported URL scheme: '{scheme}'"),
            ));
        }

        // Extract host
        let host = parsed.host_str().ok_or_else(|| {
            (
                HostCallErrorCode::InvalidRequest,
                "URL missing host".to_string(),
            )
        })?;

        // Check denylist first (takes precedence)
        if Self::matches_pattern_list(host, &self.config.denylist) {
            return Err((
                HostCallErrorCode::Denied,
                format!("Host '{host}' is in denylist"),
            ));
        }

        // Check allowlist (if non-empty, host must match)
        if !self.config.allowlist.is_empty()
            && !Self::matches_pattern_list(host, &self.config.allowlist)
        {
            return Err((
                HostCallErrorCode::Denied,
                format!("Host '{host}' is not in allowlist"),
            ));
        }

        Ok(())
    }

    /// Check if a host matches any pattern in the list.
    fn matches_pattern_list(host: &str, patterns: &[String]) -> bool {
        let host_lower = host.to_ascii_lowercase();
        patterns.iter().any(|pattern| {
            let pattern_lower = pattern.to_ascii_lowercase();
            pattern_lower.strip_prefix("*.").map_or_else(
                || host_lower == pattern_lower,
                |domain| {
                    // Wildcard subdomain match: "*.example.com" matches "api.example.com"
                    let suffix = pattern_lower.strip_prefix('*').unwrap_or(""); // ".example.com"
                    host_lower.ends_with(suffix) || host_lower == domain
                },
            )
        })
    }

    /// Parse and validate the HTTP request from hostcall params.
    fn parse_request(&self, params: &Value) -> std::result::Result<HttpRequest, ValidationError> {
        let request: HttpRequest = serde_json::from_value(params.clone()).map_err(|e| {
            (
                HostCallErrorCode::InvalidRequest,
                format!("Invalid HTTP request params: {e}"),
            )
        })?;

        // Validate method (connector supports GET/POST only)
        let method_upper = request.method.to_ascii_uppercase();
        if !matches!(method_upper.as_str(), "GET" | "POST") {
            return Err((
                HostCallErrorCode::InvalidRequest,
                format!(
                    "Invalid HTTP method: '{}'. Supported methods: GET, POST.",
                    request.method
                ),
            ));
        }

        // Validate body size
        let body_size = request
            .body
            .as_ref()
            .map(String::len)
            .or_else(|| {
                request.body_bytes.as_ref().map(|b| b.len() * 3 / 4) // base64 decode estimate
            })
            .unwrap_or(0);

        if body_size > self.config.max_request_bytes {
            return Err((
                HostCallErrorCode::InvalidRequest,
                format!(
                    "Request body too large: {} bytes (max: {} bytes)",
                    body_size, self.config.max_request_bytes
                ),
            ));
        }

        if method_upper == "GET" && (request.body.is_some() || request.body_bytes.is_some()) {
            return Err((
                HostCallErrorCode::InvalidRequest,
                "GET requests cannot include a body".to_string(),
            ));
        }

        Ok(request)
    }

    /// Execute the HTTP request.
    async fn execute_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let start = std::time::Instant::now();

        // Build request
        let method_upper = request.method.to_ascii_uppercase();
        let mut builder = match method_upper.as_str() {
            "GET" | "HEAD" | "OPTIONS" => self.client.get(&request.url),
            _ => self.client.post(&request.url),
        };

        // Add headers
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }

        // Add body if present
        if let Some(body) = &request.body {
            builder = builder.body(body.as_bytes().to_vec());
        } else if let Some(body_bytes) = &request.body_bytes {
            use base64::Engine;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(body_bytes)
                .map_err(|e| {
                    crate::error::Error::validation(format!("Invalid base64 body: {e}"))
                })?;
            builder = builder.body(decoded);
        }

        // Send request
        let response = builder
            .send()
            .await
            .map_err(|e| crate::error::Error::extension(format!("HTTP request failed: {e}")))?;

        // Read response body with size limit
        let status = response.status();
        let response_headers: Vec<(String, String)> = response.headers().to_vec();

        let mut body_bytes_vec = Vec::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            let chunk: Vec<u8> = chunk_result
                .map_err(|e| crate::error::Error::extension(format!("Read error: {e}")))?;
            if body_bytes_vec.len() + chunk.len() > self.config.max_response_bytes {
                return Err(crate::error::Error::extension(format!(
                    "Response body too large (max: {} bytes)",
                    self.config.max_response_bytes
                )));
            }
            body_bytes_vec.extend_from_slice(&chunk);
        }

        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let size_bytes = body_bytes_vec.len();

        // Convert headers to HashMap
        let mut headers_map = HashMap::new();
        for (key, value) in response_headers {
            headers_map.insert(key, value);
        }

        // Try to decode body as UTF-8, fall back to base64.
        let (body, body_bytes_b64) = String::from_utf8(body_bytes_vec).map_or_else(
            |err| {
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD.encode(err.into_bytes());
                (None, Some(encoded))
            },
            |s| (Some(s), None),
        );

        Ok(HttpResponse {
            status,
            headers: headers_map,
            body,
            body_bytes: body_bytes_b64,
            size_bytes,
            duration_ms,
        })
    }

    fn request_details(request: &HttpRequest, timeout_ms: u64) -> Value {
        json!({
            "url": request.url,
            "method": request.method,
            "timeout_ms": timeout_ms,
        })
    }

    fn redact_url_for_log(url: &str) -> String {
        url::Url::parse(url).map_or_else(
            |_| url.split(['?', '#']).next().unwrap_or(url).to_string(),
            |mut parsed| {
                parsed.set_query(None);
                parsed.set_fragment(None);
                let _ = parsed.set_username("");
                let _ = parsed.set_password(None);
                parsed.to_string()
            },
        )
    }

    async fn dispatch_request(&self, call_id: &str, request: HttpRequest) -> HostResultPayload {
        let log_url = Self::redact_url_for_log(&request.url);
        // Validate URL against policy
        if let Err((code, message)) = self.validate_url(&request.url) {
            info!(
                call_id = %call_id,
                url = %log_url,
                error = %message,
                "HTTP connector: policy denied"
            );
            return host_result_err(call_id, code, message, None);
        }

        // Log request
        debug!(
            call_id = %call_id,
            url = %log_url,
            method = %request.method,
            "HTTP connector: executing request"
        );

        // Execute request with timeout for the full request/response read.
        let timeout_ms = request.timeout_ms.unwrap_or(self.config.default_timeout_ms);
        let start = Instant::now();
        match timeout(
            wall_now(),
            Duration::from_millis(timeout_ms),
            Box::pin(self.execute_request(&request)),
        )
        .await
        {
            Ok(Ok(response)) => {
                info!(
                    call_id = %call_id,
                    url = %log_url,
                    status = %response.status,
                    size_bytes = %response.size_bytes,
                    duration_ms = %response.duration_ms,
                    "HTTP connector: request completed"
                );

                let output = serde_json::to_value(&response)
                    .unwrap_or_else(|_| json!({"error": "serialization_failed"}));

                host_result_ok(call_id, output)
            }
            Ok(Err(e)) => {
                if start.elapsed() >= Duration::from_millis(timeout_ms) {
                    let message = format!("Request timeout after {timeout_ms}ms");
                    warn!(
                        call_id = %call_id,
                        url = %log_url,
                        error = %message,
                        "HTTP connector: request timed out"
                    );

                    return host_result_err_with_details(
                        call_id,
                        HostCallErrorCode::Timeout,
                        &message,
                        Self::request_details(&request, timeout_ms),
                        Some(true),
                    );
                }

                let message = e.to_string();
                let code = match e {
                    crate::error::Error::Validation(_) => HostCallErrorCode::InvalidRequest,
                    _ => HostCallErrorCode::Io,
                };

                warn!(
                    call_id = %call_id,
                    url = %log_url,
                    error = %message,
                    "HTTP connector: request failed"
                );

                host_result_err_with_details(
                    call_id,
                    code,
                    &message,
                    Self::request_details(&request, timeout_ms),
                    Some(false),
                )
            }
            Err(_) => {
                let message = format!("Request timeout after {timeout_ms}ms");
                warn!(
                    call_id = %call_id,
                    url = %log_url,
                    error = %message,
                    "HTTP connector: request timed out"
                );

                host_result_err_with_details(
                    call_id,
                    HostCallErrorCode::Timeout,
                    &message,
                    Self::request_details(&request, timeout_ms),
                    Some(true),
                )
            }
        }
    }
}

#[async_trait]
impl Connector for HttpConnector {
    fn capability(&self) -> &'static str {
        "http"
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch(&self, call: &HostCallPayload) -> Result<HostResultPayload> {
        let call_id = &call.call_id;
        let method = call.method.to_ascii_lowercase();

        // Protocol expects connector method name "http".
        if method != "http" {
            warn!(
                call_id = %call_id,
                method = %method,
                "HTTP connector: unsupported method"
            );
            return Ok(host_result_err(
                call_id,
                HostCallErrorCode::InvalidRequest,
                format!("Unsupported HTTP connector method: '{method}'. Use 'http'."),
                None,
            ));
        }

        // Parse request
        let mut request = match self.parse_request(&call.params) {
            Ok(req) => req,
            Err((code, message)) => {
                warn!(
                    call_id = %call_id,
                    error = %message,
                    "HTTP connector: invalid request"
                );
                return Ok(host_result_err(call_id, code, message, None));
            }
        };

        // Prefer explicit per-request timeout in params, otherwise fall back to host_call.timeout_ms.
        if request.timeout_ms.is_none() {
            request.timeout_ms = call.timeout_ms.filter(|ms| *ms > 0);
        }

        Ok(self.dispatch_request(call_id, request).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn run_async<T, Fut>(future: Fut) -> T
    where
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime");
        let join = runtime.handle().spawn(future);
        runtime.block_on(join)
    }

    #[test]
    fn test_default_config() {
        let config = HttpConnectorConfig::default();
        assert!(config.require_tls);
        assert_eq!(config.max_request_bytes, 10 * 1024 * 1024);
        assert_eq!(config.max_response_bytes, 50 * 1024 * 1024);
        assert_eq!(config.default_timeout_ms, 30_000);
        assert!(config.allowlist.is_empty());
        assert!(config.denylist.is_empty());
    }

    #[test]
    fn test_url_validation_tls_required() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: true,
            ..Default::default()
        });

        // HTTPS should pass
        assert!(connector.validate_url("https://example.com").is_ok());

        // HTTP should fail when TLS required
        let result = connector.validate_url("http://example.com");
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::Denied);
    }

    #[test]
    fn test_url_validation_tls_not_required() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            ..Default::default()
        });

        // Both should pass
        assert!(connector.validate_url("https://example.com").is_ok());
        assert!(connector.validate_url("http://example.com").is_ok());
    }

    #[test]
    fn test_url_validation_allowlist() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            allowlist: vec!["api.example.com".to_string(), "*.github.com".to_string()],
            ..Default::default()
        });

        // Exact match should pass
        assert!(
            connector
                .validate_url("http://api.example.com/path")
                .is_ok()
        );

        // Wildcard match should pass
        assert!(connector.validate_url("http://api.github.com/path").is_ok());
        assert!(connector.validate_url("http://raw.github.com/path").is_ok());

        // Non-matching should fail
        let result = connector.validate_url("http://other.com/path");
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::Denied);
    }

    #[test]
    fn test_url_validation_denylist() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            denylist: vec!["evil.com".to_string(), "*.malware.net".to_string()],
            ..Default::default()
        });

        // Non-denied should pass
        assert!(connector.validate_url("http://example.com/path").is_ok());

        // Exact deny match should fail
        let result = connector.validate_url("http://evil.com/path");
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::Denied);

        // Wildcard deny match should fail
        let result = connector.validate_url("http://api.malware.net/path");
        assert!(result.is_err());
    }

    #[test]
    fn test_url_validation_denylist_precedence() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            allowlist: vec!["*.example.com".to_string()],
            denylist: vec!["evil.example.com".to_string()],
            ..Default::default()
        });

        // Allowed subdomain should pass
        assert!(
            connector
                .validate_url("http://api.example.com/path")
                .is_ok()
        );

        // Denied subdomain should fail (denylist takes precedence)
        let result = connector.validate_url("http://evil.example.com/path");
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::Denied);
    }

    #[test]
    fn test_pattern_matching() {
        let wildcard_patterns = vec!["*.example.com".to_string()];

        // Test wildcard patterns
        assert!(HttpConnector::matches_pattern_list(
            "api.example.com",
            &wildcard_patterns
        ));
        assert!(HttpConnector::matches_pattern_list(
            "sub.api.example.com",
            &wildcard_patterns
        ));
        assert!(HttpConnector::matches_pattern_list(
            "example.com",
            &wildcard_patterns
        ));

        // Test exact patterns
        let exact_patterns = vec!["example.com".to_string()];
        assert!(HttpConnector::matches_pattern_list(
            "example.com",
            &exact_patterns
        ));
        assert!(!HttpConnector::matches_pattern_list(
            "api.example.com",
            &exact_patterns
        ));

        // Test case insensitivity
        assert!(HttpConnector::matches_pattern_list(
            "API.Example.COM",
            &wildcard_patterns
        ));
    }

    #[test]
    fn test_parse_request_valid() {
        let connector = HttpConnector::with_defaults();

        let params = json!({
            "url": "https://api.example.com/data",
            "method": "POST",
            "headers": {"Content-Type": "application/json"},
            "body": "{\"key\": \"value\"}"
        });

        let request = connector.parse_request(&params).unwrap();
        assert_eq!(request.url, "https://api.example.com/data");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.headers.get("Content-Type").unwrap(),
            "application/json"
        );
        assert_eq!(request.body.as_ref().unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn test_parse_request_invalid_method() {
        let connector = HttpConnector::with_defaults();

        let params = json!({
            "url": "https://api.example.com/data",
            "method": "INVALID"
        });

        let result = connector.parse_request(&params);
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::InvalidRequest);
    }

    #[test]
    fn test_parse_request_body_too_large() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            max_request_bytes: 100,
            ..Default::default()
        });

        let large_body = "x".repeat(200);
        let params = json!({
            "url": "https://api.example.com/data",
            "method": "POST",
            "body": large_body
        });

        let result = connector.parse_request(&params);
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, HostCallErrorCode::InvalidRequest);
    }

    #[test]
    fn test_config_serialization() {
        let config = HttpConnectorConfig {
            allowlist: vec!["*.example.com".to_string()],
            denylist: vec!["evil.com".to_string()],
            require_tls: true,
            max_request_bytes: 1024,
            max_response_bytes: 2048,
            default_timeout_ms: 5000,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: HttpConnectorConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.allowlist, config.allowlist);
        assert_eq!(parsed.denylist, config.denylist);
        assert_eq!(parsed.require_tls, config.require_tls);
        assert_eq!(parsed.max_request_bytes, config.max_request_bytes);
        assert_eq!(parsed.max_response_bytes, config.max_response_bytes);
        assert_eq!(parsed.default_timeout_ms, config.default_timeout_ms);
    }

    #[test]
    fn test_dispatch_denied_host_returns_deterministic_error() {
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            allowlist: vec!["allowed.example".to_string()],
            ..Default::default()
        });

        let call = HostCallPayload {
            call_id: "call-1".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({
                "url": "http://denied.example/test",
                "method": "GET",
            }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = run_async(async move { connector.dispatch(&call).await.unwrap() });
        assert!(result.is_error);
        let error = result.error.expect("error payload");
        assert_eq!(error.code, HostCallErrorCode::Denied);
    }

    #[test]
    fn test_dispatch_timeout_returns_timeout_error_code() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let _ = ready_tx.send(());
            let (_stream, _peer) = listener.accept().expect("accept");
            thread::sleep(std::time::Duration::from_millis(150));
        });
        let _ = ready_rx.recv();

        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            default_timeout_ms: 50,
            ..Default::default()
        });

        let call = HostCallPayload {
            call_id: "call-1".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({
                "url": format!("http://{addr}/"),
                "method": "GET",
                "timeout_ms": 50,
            }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = run_async(async move { connector.dispatch(&call).await.unwrap() });
        assert!(result.is_error);
        let error = result.error.expect("error payload");
        assert_eq!(error.code, HostCallErrorCode::Timeout);
        assert_eq!(error.retryable, Some(true));

        let _ = join.join();
    }

    #[test]
    fn test_dispatch_uses_call_timeout_ms_when_request_timeout_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let _ = ready_tx.send(());
            let (_stream, _peer) = listener.accept().expect("accept");
            thread::sleep(std::time::Duration::from_millis(150));
        });
        let _ = ready_rx.recv();

        // Default timeout is large; call.timeout_ms should take precedence when params omit it.
        let connector = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            default_timeout_ms: 5000,
            ..Default::default()
        });

        let call = HostCallPayload {
            call_id: "call-1".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({
                "url": format!("http://{addr}/"),
                "method": "GET",
            }),
            timeout_ms: Some(50),
            cancel_token: None,
            context: None,
        };

        let result = run_async(async move { connector.dispatch(&call).await.unwrap() });
        assert!(result.is_error);
        let error = result.error.expect("error payload");
        assert!(
            error.code == HostCallErrorCode::Timeout,
            "expected timeout, got {:?} (details={:?})",
            error.code,
            error.details
        );

        let _ = join.join();
    }

    #[test]
    fn http_connector_redact_url_for_log_strips_sensitive_parts() {
        let redacted =
            HttpConnector::redact_url_for_log("http://user:pass@denied.example/test?q=hello#frag");
        assert!(redacted.contains("http://denied.example/test"));
        assert!(!redacted.contains("q=hello"));
        assert!(!redacted.contains("#frag"));
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("pass"));
    }

    #[test]
    fn http_connector_redact_url_for_log_falls_back_for_invalid_urls() {
        let redacted = HttpConnector::redact_url_for_log("not a url?q=hello#frag");
        assert_eq!(redacted, "not a url");
    }
}
