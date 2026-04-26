//! HTTP connector for extension hostcalls (pi.http).
//!
//! This adapter validates request payloads, enforces TLS policy, executes
//! a simple GET/POST request via the internal HTTP client, and returns a
//! normalized response shape.

use crate::connectors::{
    Connector, HostCallErrorCode, HostCallPayload, HostResultPayload, host_result_err,
    host_result_err_with_details, host_result_ok,
};
use crate::error::{Error, Result};
use crate::http::client::Client;
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

#[derive(Debug, Clone)]
pub struct HttpConnectorConfig {
    pub require_tls: bool,
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
            enforce_allowlist: false,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
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
}

fn invalid_request(call_id: &str, message: impl Into<String>) -> HostResultPayload {
    host_result_err(call_id, HostCallErrorCode::InvalidRequest, message, None)
}

/// Developer opt-in: when set to "1", plain http:// is permitted for
/// loopback hosts only. Default behavior (require TLS) is unchanged.
fn allow_loopback_http_env() -> bool {
    std::env::var("PI_HTTP_ALLOW_LOOPBACK")
        .ok()
        .as_deref()
        == Some("1")
}

/// True when host is a loopback address. Matches 127.0.0.0/8, ::1,
/// localhost, and bracketed forms used in URL parsing.
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") || h == "::1" {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
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

    let mut host = trimmed.to_string();
    if trimmed.contains("://") {
        if let Ok(parsed) = ParsedUrl::parse(trimmed) {
            host = parsed.host;
        }
    }

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

struct PreparedRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
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
                // Explicit developer opt-in: PI_HTTP_ALLOW_LOOPBACK=1 lets
                // plain http through ONLY for loopback hosts. Everything
                // else still requires TLS by default. Intended for local-
                // worker development where setting up a cert is friction.
                if !(allow_loopback_http_env() && is_loopback_host(&parsed.host)) {
                    return Err(Box::new(host_result_err(
                        &call.call_id,
                        HostCallErrorCode::Denied,
                        "TLS required: use https:// URLs (set PI_HTTP_ALLOW_LOOPBACK=1 for local development)",
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

        if method != "GET" && method != "POST" {
            return Err(Box::new(invalid_request(
                &call.call_id,
                format!("Unsupported HTTP method: {method}"),
            )));
        }

        let body_val = params.get("body");
        let body_bytes_val = params.get("body_bytes").or_else(|| params.get("bodyBytes"));

        if body_val.is_some() && body_bytes_val.is_some() {
            return Err(Box::new(invalid_request(
                &call.call_id,
                "body and body_bytes are mutually exclusive",
            )));
        }

        if method == "GET" && (body_val.is_some() || body_bytes_val.is_some()) {
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

        if let Some(ref bytes) = body {
            if self.config.max_request_bytes > 0 && bytes.len() > self.config.max_request_bytes {
                return Err(Box::new(invalid_request(
                    &call.call_id,
                    "request body too large",
                )));
            }
        }

        let timeout_ms_param = params
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| params.get("timeoutMs").and_then(Value::as_u64))
            .or_else(|| params.get("timeout_ms").and_then(Value::as_u64))
            .filter(|value| *value > 0);

        let timeout_ms_call = call.timeout_ms.filter(|value| *value > 0);

        let timeout_ms = timeout_ms_param.or(timeout_ms_call).or({
            if self.config.default_timeout_ms > 0 {
                Some(self.config.default_timeout_ms)
            } else {
                None
            }
        });

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

        let mut builder = if prepared.method == "GET" {
            self.client.get(&prepared.url)
        } else {
            self.client.post(&prepared.url)
        };

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
    pub async fn dispatch_streaming(
        &self,
        call: &HostCallPayload,
    ) -> std::result::Result<crate::http::client::Response, HostResultPayload> {
        let prepared = match self.prepare_request(call) {
            Ok(prepared) => prepared,
            Err(payload) => return Err(*payload),
        };

        let mut builder = if prepared.method == "GET" {
            self.client.get(&prepared.url)
        } else {
            self.client.post(&prepared.url)
        };

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
