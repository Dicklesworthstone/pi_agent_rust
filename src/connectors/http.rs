//! HTTP connector for extension hostcalls (pi.http).
//!
//! This adapter validates request payloads, enforces TLS policy, executes
//! a bounded set of HTTP methods via the internal HTTP client, and returns a
//! normalized response shape.

use crate::connectors::{
    Connector, HostCallErrorCode, HostCallPayload, HostResultPayload, host_result_err,
    host_result_err_with_details, host_result_ok,
};
use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder};
use asupersync::http::h1::http_client::Scheme;
use asupersync::http::h1::{ClientError, ParsedUrl};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 50 * 1024 * 1024;

/// Environment variable that opts in to plain HTTP for loopback hosts only.
///
/// Set to `1` to permit `http://localhost`, `http://127.0.0.0/8`, and
/// `http://[::1]` even when `require_tls` is enabled. Any other value
/// (including unset) keeps the secure default. Loopback HTTP is treated as
/// a "secure context" by browsers for the same reason: traffic never leaves
/// the host, so TLS is friction without security gain.
pub const ALLOW_LOOPBACK_HTTP_ENV: &str = "PI_HTTP_ALLOW_LOOPBACK";

#[derive(Debug, Clone)]
pub struct HttpConnectorConfig {
    pub require_tls: bool,
    /// Opt-in escape hatch: when `true`, `http://` is permitted **only** for
    /// loopback hosts (`127.0.0.0/8`, `::1`, `localhost`) even if
    /// `require_tls` is set. Defaults to `true` when the
    /// [`ALLOW_LOOPBACK_HTTP_ENV`] environment variable is set to exactly
    /// `"1"` and `false` otherwise, so callers can flip it per-shell without
    /// rebuilding while tests can also override it directly without
    /// polluting process env.
    ///
    /// This bypass applies to the TLS check only — denylist and allowlist
    /// policies still run, so a request to `http://localhost/` will still
    /// be denied if `localhost` is not in an active allowlist. Callers
    /// who want loopback freely accessible should add `localhost`/`127.0.0.1`
    /// to their allowlist (or leave allowlist empty).
    pub allow_loopback_http: bool,
    pub enforce_allowlist: bool,
    pub allowlist: Vec<String>,
    pub denylist: Vec<String>,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub default_timeout_ms: u64,
}

impl Default for HttpConnectorConfig {
    fn default() -> Self {
        Self {
            require_tls: true,
            allow_loopback_http: allow_loopback_http_from_env(),
            enforce_allowlist: false,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// Returns `true` iff [`ALLOW_LOOPBACK_HTTP_ENV`] is set to exactly `"1"`.
/// Any other value (`"0"`, `"true"`, unset, etc.) keeps the secure default —
/// the strict `"1"` check is intentional so a typo can never accidentally
/// loosen the policy.
fn allow_loopback_http_from_env() -> bool {
    std::env::var(ALLOW_LOOPBACK_HTTP_ENV).as_deref() == Ok("1")
}

/// Returns `true` iff `host` resolves to a loopback address.
///
/// Accepts the literal `localhost` (case-insensitive), the bare/ bracketed
/// IPv6 loopback (`::1`, `[::1]`), and any address in `127.0.0.0/8`. Anything
/// that fails to parse as an IP and isn't `localhost` is rejected — DNS
/// names that *resolve* to loopback are not honored, since resolving here
/// would reintroduce the trust problem the loopback exception is meant to
/// avoid.
fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim();
    let stripped = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if stripped.eq_ignore_ascii_case("localhost") {
        return true;
    }
    stripped
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[derive(Debug, Clone)]
pub struct HttpConnector {
    config: HttpConnectorConfig,
    client: Client,
}

impl HttpConnector {
    #[must_use]
    pub fn new(mut config: HttpConnectorConfig) -> Self {
        config.allowlist = normalize_allowlist(config.allowlist);
        config.denylist = normalize_allowlist(config.denylist);
        Self {
            config,
            client: Client::new(),
        }
    }

    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(HttpConnectorConfig::default())
    }

    #[must_use]
    pub(crate) const fn max_response_bytes(&self) -> usize {
        self.config.max_response_bytes
    }
}

fn invalid_request(call_id: &str, message: impl Into<String>) -> HostResultPayload {
    host_result_err(call_id, HostCallErrorCode::InvalidRequest, message, None)
}

fn io_error(call_id: &str, message: impl Into<String>) -> HostResultPayload {
    host_result_err(call_id, HostCallErrorCode::Io, message, None)
}

fn timeout_error(call_id: &str, message: impl Into<String>) -> HostResultPayload {
    host_result_err(call_id, HostCallErrorCode::Timeout, message, Some(true))
}

fn sanitize_invalid_url_reason(err: &ClientError) -> String {
    match err {
        ClientError::InvalidUrl(reason) => {
            let reason = reason.trim();
            if reason
                .to_ascii_lowercase()
                .starts_with("unsupported scheme in:")
            {
                "Unsupported URL scheme".to_string()
            } else {
                reason.to_string()
            }
        }
        _ => "Invalid URL".to_string(),
    }
}

fn deny_allowlist(call_id: &str, host: &str, allowlist: &[String]) -> HostResultPayload {
    let details = json!({
        "host": host,
        "allowlist": allowlist,
        "hint": "Add the host to capability_manifest.scope.hosts"
    });
    host_result_err_with_details(
        call_id,
        HostCallErrorCode::Denied,
        "Host not in allowlist",
        details,
        None,
    )
}

fn deny_denylist(call_id: &str, host: &str, denylist: &[String]) -> HostResultPayload {
    let details = json!({
        "host": host,
        "denylist": denylist,
        "hint": "Remove the host from the denylist to allow access"
    });
    host_result_err_with_details(
        call_id,
        HostCallErrorCode::Denied,
        "Host is in denylist",
        details,
        None,
    )
}

fn normalize_allowlist(allowlist: Vec<String>) -> Vec<String> {
    allowlist
        .into_iter()
        .filter_map(|entry| normalize_host_entry(&entry))
        .collect()
}

fn normalize_host_entry(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let host = if trimmed.contains("://")
        && let Ok(parsed) = ParsedUrl::parse(trimmed)
    {
        parsed.host
    } else {
        trimmed.to_string()
    };

    let host = host.trim().trim_end_matches('.');
    let host = if host.starts_with('[') {
        host.find(']').map_or(host, |end| &host[1..end])
    } else if host.matches(':').count() == 1 {
        host.split_once(':').map_or(host, |(left, _)| left)
    } else {
        host
    };

    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn host_is_allowed(host: &str, allowlist: &[String]) -> bool {
    let Some(host) = normalize_host_entry(host) else {
        return false;
    };

    for entry in allowlist {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return true;
        }
        if let Some(suffix) = entry.strip_prefix("*.") {
            if host == suffix || host.ends_with(&format!(".{suffix}")) {
                return true;
            }
            continue;
        }
        if host == entry {
            return true;
        }
        if host.ends_with(&format!(".{entry}")) {
            return true;
        }
    }
    false
}

fn host_is_denied(host: &str, denylist: &[String]) -> bool {
    let Some(host) = normalize_host_entry(host) else {
        return false;
    };

    for entry in denylist {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return true;
        }
        if let Some(suffix) = entry.strip_prefix("*.") {
            if host == suffix || host.ends_with(&format!(".{suffix}")) {
                return true;
            }
            continue;
        }
        if host == entry {
            return true;
        }
        if host.ends_with(&format!(".{entry}")) {
            return true;
        }
    }
    false
}

fn is_timeout_error(err: &Error) -> bool {
    match err {
        Error::Api(message) => message.to_ascii_lowercase().contains("timed out"),
        Error::Io(err) => {
            err.kind() == std::io::ErrorKind::TimedOut
                || err.to_string().to_ascii_lowercase().contains("timed out")
        }
        _ => false,
    }
}

fn is_timeout_io(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::TimedOut
        || err.to_string().to_ascii_lowercase().contains("timed out")
}

/// Keep validation and dispatch exhaustive. Accepting a new method must never
/// silently send it as POST, or expose CONNECT/TRACE and arbitrary wire tokens.
#[derive(Clone, Copy)]
enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "PATCH" => Some(Self::Patch),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }

    fn request<'a>(self, client: &'a Client, url: &str) -> RequestBuilder<'a> {
        match self {
            Self::Get => client.get(url),
            Self::Post => client.post(url),
            Self::Put => client.put(url),
            Self::Patch => client.patch(url),
            Self::Delete => client.delete(url),
        }
    }
}

struct PreparedRequest {
    url: String,
    method: HttpMethod,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
}

/// Request hints can shorten a hostcall budget, never extend it. Zero remains
/// "unset" for all existing timeout spellings. Resolve aliases independently:
/// a zero or longer `timeout` must not mask a shorter `timeoutMs`/`timeout_ms`.
/// The connector default is a fallback, not a maximum; otherwise Freeflow's
/// explicit five-minute requests would still be truncated to thirty seconds.
fn effective_timeout_ms(
    params: &serde_json::Map<String, Value>,
    call_timeout_ms: Option<u64>,
    default_timeout_ms: u64,
) -> Option<u64> {
    ["timeout", "timeoutMs", "timeout_ms"]
        .into_iter()
        .filter_map(|key| params.get(key).and_then(Value::as_u64))
        .chain(call_timeout_ms)
        .filter(|value| *value > 0)
        .min()
        .or_else(|| (default_timeout_ms > 0).then_some(default_timeout_ms))
}

impl HttpConnector {
    #[allow(clippy::too_many_lines)]
    fn prepare_request(
        &self,
        call: &HostCallPayload,
    ) -> std::result::Result<PreparedRequest, Box<HostResultPayload>> {
        if !call.method.trim().eq_ignore_ascii_case("http") {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "Unsupported hostcall method for http connector",
            )));
        }

        let Some(params) = call.params.as_object() else {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "http params must be an object",
            )));
        };

        let url = match params.get("url").and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => return Err(Box::new(invalid_request(&call.call_id, "url is required"))),
        };

        let parsed = match ParsedUrl::parse(&url) {
            Ok(parsed) => parsed,
            Err(err) => {
                let reason = sanitize_invalid_url_reason(&err);
                return Err(Box::new(invalid_request(
                    &call.call_id,
                    format!("Invalid URL: {reason}"),
                )));
            }
        };

        if parsed.host.trim().is_empty() {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "URL host is required",
            )));
        }

        match parsed.scheme {
            Scheme::Http if self.config.require_tls => {
                // Loopback escape hatch: only when the operator explicitly
                // opted in (`allow_loopback_http`, default-driven by the
                // PI_HTTP_ALLOW_LOOPBACK=1 env var) AND the host is actually
                // loopback. Anything else still gets the strict TLS error.
                let host_is_loopback = is_loopback_host(&parsed.host);
                if !(self.config.allow_loopback_http && host_is_loopback) {
                    // Surface the env-var hint only when it would actually
                    // help — i.e. for loopback hosts that the user could
                    // unblock by opting in. For non-loopback hosts (e.g.
                    // `http://example.com/`), the env var is irrelevant and
                    // mentioning it sends the user down the wrong path.
                    let message = if host_is_loopback {
                        "TLS required: use https:// URLs (set PI_HTTP_ALLOW_LOOPBACK=1 \
                         to permit plain http for loopback hosts)"
                    } else {
                        "TLS required: use https:// URLs"
                    };
                    return Err(Box::new(host_result_err(
                        &call.call_id,
                        HostCallErrorCode::Denied,
                        message,
                        None,
                    )));
                }
            }
            Scheme::Http | Scheme::Https => {}
        }

        if host_is_denied(&parsed.host, &self.config.denylist) {
            return Err(Box::new(deny_denylist(
                &call.call_id,
                &parsed.host,
                &self.config.denylist,
            )));
        }

        let enforce_allowlist = self.config.enforce_allowlist || !self.config.allowlist.is_empty();
        if enforce_allowlist {
            if self.config.allowlist.is_empty() {
                return Err(Box::new(host_result_err(
                    &call.call_id,
                    HostCallErrorCode::Denied,
                    "HTTP allowlist is empty; update capability_manifest scope.hosts",
                    None,
                )));
            }

            if !host_is_allowed(&parsed.host, &self.config.allowlist) {
                return Err(Box::new(deny_allowlist(
                    &call.call_id,
                    &parsed.host,
                    &self.config.allowlist,
                )));
            }
        }

        let method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .trim()
            .to_ascii_uppercase();

        let Some(method) = HttpMethod::parse(&method) else {
            return Err(Box::new(invalid_request(
                &call.call_id,
                format!("Unsupported HTTP method: {method}"),
            )));
        };

        let body_val = params.get("body");
        let body_bytes_val = params.get("body_bytes").or_else(|| params.get("bodyBytes"));

        if body_val.is_some() && body_bytes_val.is_some() {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "body and body_bytes are mutually exclusive",
            )));
        }

        if matches!(method, HttpMethod::Get) && (body_val.is_some() || body_bytes_val.is_some()) {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "GET requests must not include a body",
            )));
        }

        let headers = if let Some(headers_value) = params.get("headers") {
            let Some(headers_obj) = headers_value.as_object() else {
                return Err(Box::new(invalid_request(
                    &call.call_id,
                    "headers must be an object",
                )));
            };
            let mut out = Vec::with_capacity(headers_obj.len());
            for (key, value) in headers_obj {
                let value = value.as_str().map_or_else(
                    || {
                        if value.is_null() {
                            String::new()
                        } else {
                            value.to_string()
                        }
                    },
                    str::to_string,
                );
                out.push((key.clone(), value));
            }
            out
        } else {
            Vec::new()
        };

        let body = if let Some(body_bytes_value) = body_bytes_val {
            let Some(encoded) = body_bytes_value.as_str() else {
                return Err(Box::new(invalid_request(
                    &call.call_id,
                    "body_bytes must be a base64 string",
                )));
            };
            let decoded = match BASE64_STANDARD.decode(encoded.as_bytes()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    return Err(Box::new(invalid_request(
                        &call.call_id,
                        format!("Invalid base64 body_bytes: {err}"),
                    )));
                }
            };
            Some(decoded)
        } else if let Some(body_value) = body_val {
            let body = body_value.as_str().map_or_else(
                || {
                    if body_value.is_null() {
                        String::new()
                    } else {
                        body_value.to_string()
                    }
                },
                str::to_string,
            );
            Some(body.into_bytes())
        } else {
            None
        };

        if let Some(ref bytes) = body
            && self.config.max_request_bytes > 0
            && bytes.len() > self.config.max_request_bytes
        {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "request body too large",
            )));
        }

        let timeout_ms =
            effective_timeout_ms(params, call.timeout_ms, self.config.default_timeout_ms);

        Ok(PreparedRequest {
            url,
            method,
            headers,
            body,
            timeout_ms,
        })
    }
}

#[async_trait]
impl Connector for HttpConnector {
    fn capability(&self) -> &'static str {
        "http"
    }

    async fn dispatch(&self, call: &HostCallPayload) -> Result<HostResultPayload> {
        let prepared = match self.prepare_request(call) {
            Ok(prepared) => prepared,
            Err(payload) => return Ok(*payload),
        };

        let mut builder = prepared.method.request(&self.client, &prepared.url);

        for (key, value) in prepared.headers {
            builder = builder.header(&key, value);
        }

        if let Some(body) = prepared.body {
            builder = builder.body(body);
        }

        if let Some(timeout_ms) = prepared.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        } else {
            builder = builder.no_timeout();
        }

        let response = match builder.send().await {
            Ok(response) => response,
            Err(err) => {
                if is_timeout_error(&err) {
                    return Ok(timeout_error(&call.call_id, err.to_string()));
                }
                return Ok(io_error(&call.call_id, err.to_string()));
            }
        };

        let status = response.status();
        let headers = response.headers().to_vec();
        let mut stream = response.bytes_stream();
        let mut body_bytes = Vec::new();

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if self.config.max_response_bytes > 0
                        && body_bytes.len().saturating_add(bytes.len())
                            > self.config.max_response_bytes
                    {
                        return Ok(invalid_request(&call.call_id, "response body too large"));
                    }
                    body_bytes.extend_from_slice(&bytes);
                }
                Err(err) => {
                    if is_timeout_io(&err) {
                        return Ok(timeout_error(&call.call_id, err.to_string()));
                    }
                    return Ok(io_error(&call.call_id, err.to_string()));
                }
            }
        }

        let mut headers_map = serde_json::Map::new();
        for (key, value) in headers {
            match headers_map.get_mut(&key) {
                Some(Value::String(existing)) => {
                    if !existing.is_empty() {
                        existing.push_str(", ");
                    }
                    existing.push_str(&value);
                }
                _ => {
                    headers_map.insert(key, Value::String(value));
                }
            }
        }

        let mut output = serde_json::Map::new();
        output.insert("status".to_string(), json!(status));
        output.insert("headers".to_string(), Value::Object(headers_map));

        if let Ok(text) = String::from_utf8(body_bytes.clone()) {
            output.insert("body".to_string(), Value::String(text));
        } else {
            let encoded = BASE64_STANDARD.encode(&body_bytes);
            output.insert("body_bytes".to_string(), Value::String(encoded));
        }

        Ok(host_result_ok(&call.call_id, Value::Object(output)))
    }
}

impl HttpConnector {
    // The Err payload IS the extension-facing wire format (`HostResultPayload`);
    // boxing it would churn every hostcall dispatch site for a cold error path.
    #[allow(clippy::result_large_err)]
    pub async fn dispatch_streaming(
        &self,
        call: &HostCallPayload,
    ) -> std::result::Result<crate::http::client::Response, HostResultPayload> {
        let prepared = match self.prepare_request(call) {
            Ok(prepared) => prepared,
            Err(payload) => return Err(*payload),
        };

        let mut builder = prepared.method.request(&self.client, &prepared.url);

        for (key, value) in prepared.headers {
            builder = builder.header(&key, value);
        }

        if let Some(body) = prepared.body {
            builder = builder.body(body);
        }

        if let Some(timeout_ms) = prepared.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        } else {
            builder = builder.no_timeout();
        }

        match builder.send().await {
            Ok(response) => Ok(response),
            Err(err) => {
                if is_timeout_error(&err) {
                    Err(timeout_error(&call.call_id, err.to_string()))
                } else {
                    Err(io_error(&call.call_id, err.to_string()))
                }
            }
        }
    }
}

#[cfg(test)]
mod deployment_method_tests {
    use super::*;
    use crate::http::client::{HttpTransport, TransportResponse};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct RecordedCall {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    #[derive(Debug, Default)]
    struct RecordingTransport {
        calls: Mutex<Vec<RecordedCall>>,
    }

    #[async_trait]
    impl HttpTransport for RecordingTransport {
        async fn send(
            &self,
            method: &str,
            url: &str,
            headers: &[(String, String)],
            body: &[u8],
        ) -> Result<TransportResponse> {
            self.calls
                .lock()
                .expect("recording lock")
                .push(RecordedCall {
                    method: method.to_string(),
                    url: url.to_string(),
                    headers: headers.to_vec(),
                    body: body.to_vec(),
                });
            Ok((
                201,
                vec![(
                    "content-type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                futures::stream::iter(vec![Ok(vec![0, 255, 65]), Ok(vec![66])]).boxed(),
            ))
        }
    }

    fn connector(config: HttpConnectorConfig) -> (HttpConnector, Arc<RecordingTransport>) {
        let recording = Arc::new(RecordingTransport::default());
        let mut connector = HttpConnector::new(config);
        connector.client = connector.client.with_transport(recording.clone());
        (connector, recording)
    }

    fn call(method: &str) -> HostCallPayload {
        HostCallPayload {
            call_id: "freeflow-deployment".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({
                "url": "https://relay.example/v1/deployments/fixture?scope=test",
                "method": method,
                "timeout": 300_000,
                "headers": {"Authorization": "Bearer fixture-only"},
            }),
            timeout_ms: Some(25),
            cancel_token: None,
            context: None,
        }
    }

    /// Uses the real connector and native client, replacing only the I/O
    /// boundary. No DNS, cloud resources, sockets, or platform skips are needed.
    #[test]
    #[allow(clippy::significant_drop_tightening)] // `sent` borrows the recorded-calls guard
    fn deployment_methods_preserve_verb_body_headers_and_response_bytes() {
        futures::executor::block_on(async {
            for (input, expected_method) in [
                ("GET", "GET"),
                ("post", "POST"),
                (" put ", "PUT"),
                ("patch", "PATCH"),
                ("DELETE", "DELETE"),
            ] {
                for streaming in [false, true] {
                    for (field, value, expected_body) in [
                        (None, Value::Null, Vec::new()),
                        (
                            Some("body"),
                            json!("{\"enabled\":true}"),
                            b"{\"enabled\":true}".to_vec(),
                        ),
                        (Some("body_bytes"), json!("AP9B"), vec![0, 255, 65]),
                    ] {
                        if expected_method == "GET" && field.is_some() {
                            continue;
                        }
                        let (connector, recording) = connector(HttpConnectorConfig {
                            allowlist: vec!["relay.example".to_string()],
                            ..HttpConnectorConfig::default()
                        });
                        let mut request = call(input);
                        if let Some(field) = field {
                            request.params[field] = value;
                        }
                        assert_eq!(
                            connector
                                .prepare_request(&request)
                                .expect("valid request")
                                .timeout_ms,
                            Some(25),
                            "new methods must retain the outer hostcall budget"
                        );
                        if streaming {
                            let response = connector
                                .dispatch_streaming(&request)
                                .await
                                .expect("stream");
                            assert_eq!(response.status(), 201);
                            assert_eq!(
                                response.bytes_limited(4).await.expect("bytes"),
                                [0, 255, 65, 66]
                            );
                        } else {
                            let response = connector.dispatch(&request).await.expect("dispatch");
                            assert!(!response.is_error, "{:?}", response.error);
                            assert_eq!(response.output["status"], 201);
                            assert_eq!(
                                response.output["body_bytes"],
                                BASE64_STANDARD.encode([0, 255, 65, 66])
                            );
                        }
                        let calls = recording.calls.lock().expect("recorded calls");
                        assert_eq!(
                            calls.len(),
                            1,
                            "exactly one dispatch, never a POST fallback"
                        );
                        let sent = calls.first().expect("sent request");
                        assert_eq!(sent.method, expected_method);
                        assert_eq!(sent.url, request.params["url"].as_str().expect("URL"));
                        assert_eq!(sent.body, expected_body);
                        assert!(sent.headers.iter().any(|(key, value)| {
                            key.eq_ignore_ascii_case("authorization")
                                && value == "Bearer fixture-only"
                        }));
                        assert!(sent.headers.iter().any(|(key, value)| {
                            key.eq_ignore_ascii_case("content-length")
                                && value == &sent.body.len().to_string()
                        }));
                    }
                }
            }
        });
    }

    async fn assert_rejected_before_transport(
        connector: &HttpConnector,
        recording: &RecordingTransport,
        request: &HostCallPayload,
        expected: HostCallErrorCode,
    ) {
        let buffered = connector.dispatch(request).await.expect("dispatch");
        assert!(buffered.is_error);
        assert_eq!(buffered.error.expect("buffered error").code, expected);
        let streamed = connector
            .dispatch_streaming(request)
            .await
            .err()
            .expect("stream error");
        assert_eq!(streamed.error.expect("streamed error").code, expected);
        assert!(recording.calls.lock().expect("recorded calls").is_empty());
    }

    #[test]
    fn deployment_mutations_keep_tls_host_and_body_restrictions() {
        futures::executor::block_on(async {
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                for case in 0..6 {
                    let mut config = HttpConnectorConfig {
                        allow_loopback_http: false,
                        allowlist: vec!["relay.example".to_string()],
                        ..HttpConnectorConfig::default()
                    };
                    let mut request = call(method);
                    let expected = match case {
                        0 => {
                            request.params["url"] = json!("http://relay.example/deploy");
                            HostCallErrorCode::Denied
                        }
                        1 => {
                            request.params["url"] = json!("https://unlisted.example/deploy");
                            HostCallErrorCode::Denied
                        }
                        2 => {
                            config.denylist = vec!["relay.example".to_string()];
                            HostCallErrorCode::Denied
                        }
                        3 => {
                            config.allowlist.clear();
                            config.enforce_allowlist = true;
                            HostCallErrorCode::Denied
                        }
                        4 => {
                            config.max_request_bytes = 2;
                            request.params["body_bytes"] = json!("AP9B");
                            HostCallErrorCode::InvalidRequest
                        }
                        _ => {
                            request.params["body"] = json!("text");
                            request.params["body_bytes"] = json!("AP9B");
                            HostCallErrorCode::InvalidRequest
                        }
                    };
                    let (connector, recording) = connector(config);
                    assert_rejected_before_transport(&connector, &recording, &request, expected)
                        .await;
                }
            }
        });
    }

    #[test]
    fn unsupported_methods_and_get_bodies_never_dispatch() {
        futures::executor::block_on(async {
            let (connector, recording) = connector(HttpConnectorConfig::default());
            for method in [
                "CONNECT",
                "TRACE",
                "HEAD",
                "OPTIONS",
                "CUSTOM",
                "DELETE\r\nPOST",
                "",
            ] {
                assert_rejected_before_transport(
                    &connector,
                    &recording,
                    &call(method),
                    HostCallErrorCode::InvalidRequest,
                )
                .await;
            }
            for field in ["body", "body_bytes", "bodyBytes"] {
                let mut request = call("GET");
                request.params[field] = json!("");
                assert_rejected_before_transport(
                    &connector,
                    &recording,
                    &request,
                    HostCallErrorCode::InvalidRequest,
                )
                .await;
            }
        });
    }
}

#[cfg(test)]
mod timeout_budget_tests {
    use super::*;

    fn prepared_timeout(
        params: Value,
        call_timeout_ms: Option<u64>,
        default_ms: u64,
    ) -> Option<u64> {
        let connector = HttpConnector::new(HttpConnectorConfig {
            default_timeout_ms: default_ms,
            allowlist: vec!["relay.example".to_string()],
            ..HttpConnectorConfig::default()
        });
        let mut call = HostCallPayload {
            call_id: "freeflow-timeout".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params,
            timeout_ms: call_timeout_ms,
            cancel_token: None,
            context: None,
        };
        call.params["url"] = json!("https://relay.example/relay");
        call.params["method"] = json!("POST");
        // Both dispatch paths use this preparation, before any network I/O.
        connector
            .prepare_request(&call)
            .expect("valid request")
            .timeout_ms
    }

    #[test]
    fn every_timeout_alias_respects_the_hostcall_budget() {
        for alias in ["timeout", "timeoutMs", "timeout_ms"] {
            let mut params = json!({});
            params[alias] = json!(300_000);
            assert_eq!(
                prepared_timeout(params.clone(), Some(5_000), 30_000),
                Some(5_000)
            );
            assert_eq!(
                prepared_timeout(params.clone(), Some(600_000), 30_000),
                Some(300_000)
            );
            assert_eq!(prepared_timeout(params, None, 30_000), Some(300_000));
        }
    }

    #[test]
    fn conflicting_aliases_use_the_shortest_positive_budget() {
        let aliases = ["timeout", "timeoutMs", "timeout_ms"];
        for short_alias in aliases {
            let mut params = json!({
                "timeout": 300_000, "timeoutMs": 600_000, "timeout_ms": 900_000
            });
            params[short_alias] = json!(25);
            assert_eq!(
                prepared_timeout(params.clone(), Some(1_000), 30_000),
                Some(25)
            );
            assert_eq!(prepared_timeout(params, Some(10), 30_000), Some(10));

            for unset_alias in aliases {
                if unset_alias != short_alias {
                    let mut masked = json!({});
                    masked[unset_alias] = json!(0);
                    masked[short_alias] = json!(25);
                    assert_eq!(prepared_timeout(masked, None, 30_000), Some(25));
                }
            }
        }
    }

    #[test]
    fn absent_zero_and_invalid_hints_keep_existing_fallback_semantics() {
        for value in [
            Value::Null,
            json!(0),
            json!(-1),
            json!(1.5),
            json!("25"),
            json!(false),
        ] {
            let params = json!({"timeout": value, "timeoutMs": value, "timeout_ms": value});
            assert_eq!(prepared_timeout(params.clone(), Some(75), 30_000), Some(75));
            assert_eq!(
                prepared_timeout(params.clone(), Some(0), 30_000),
                Some(30_000)
            );
            assert_eq!(prepared_timeout(params, None, 0), None);
        }
        assert_eq!(prepared_timeout(json!({}), None, 30_000), Some(30_000));
        assert_eq!(prepared_timeout(json!({}), Some(0), 0), None);
    }

    #[test]
    fn extreme_request_hint_cannot_remove_or_overflow_a_host_budget() {
        assert_eq!(
            prepared_timeout(json!({"timeout": u64::MAX}), Some(1), 0),
            Some(1)
        );
        assert_eq!(
            prepared_timeout(json!({"timeout": 1}), Some(u64::MAX), 0),
            Some(1)
        );
        assert_eq!(
            prepared_timeout(json!({"timeout": u64::MAX}), None, 0),
            Some(u64::MAX)
        );
    }
}
