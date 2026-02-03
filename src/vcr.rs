//! VCR-style recording for HTTP streaming tests.
//!
//! This module provides utilities to record and replay real HTTP streaming
//! responses (e.g., SSE) for deterministic provider tests.

use crate::error::{Error, Result};
use crate::http::client::StreamingResponse;
use crate::http::{Client, Method};
use asupersync::Cx;
use chrono::{SecondsFormat, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

pub const VCR_ENV_MODE: &str = "VCR_MODE";
pub const VCR_ENV_DIR: &str = "VCR_CASSETTE_DIR";
pub const DEFAULT_CASSETTE_DIR: &str = "tests/fixtures/vcr";
const CASSETTE_VERSION: &str = "1.0";
const REDACTED: &str = "[REDACTED]";

#[cfg(test)]
static TEST_ENV_OVERRIDES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

#[cfg(test)]
fn test_env_overrides() -> &'static Mutex<HashMap<String, String>> {
    TEST_ENV_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn env_var(name: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Ok(guard) = test_env_overrides().lock() {
            if let Some(value) = guard.get(name) {
                return Some(value.clone());
            }
        }
    }
    std::env::var(name).ok()
}

#[cfg(test)]
fn set_test_env_var(name: &str, value: Option<&str>) -> Option<String> {
    let mut guard = test_env_overrides().lock().expect("env override lock");
    let previous = guard.get(name).cloned();
    match value {
        Some(value) => {
            guard.insert(name.to_string(), value.to_string());
        }
        None => {
            guard.remove(name);
        }
    }
    previous
}

#[cfg(test)]
fn restore_test_env_var(name: &str, previous: Option<String>) {
    let mut guard = test_env_overrides().lock().expect("env override lock");
    match previous {
        Some(value) => {
            guard.insert(name.to_string(), value);
        }
        None => {
            guard.remove(name);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcrMode {
    Record,
    Playback,
    Auto,
}

impl VcrMode {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(value) = env_var(VCR_ENV_MODE) else {
            return Ok(None);
        };
        let mode = match value.to_ascii_lowercase().as_str() {
            "record" => Self::Record,
            "playback" => Self::Playback,
            "auto" => Self::Auto,
            _ => {
                return Err(Error::config(format!(
                    "Invalid {VCR_ENV_MODE} value: {value}"
                )));
            }
        };
        Ok(Some(mode))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cassette {
    pub version: String,
    pub test_name: String,
    pub recorded_at: String,
    pub interactions: Vec<Interaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_chunks: Vec<String>,
}

impl RecordedResponse {
    pub fn into_streaming_response(self) -> StreamingResponse {
        let chunks = self
            .body_chunks
            .into_iter()
            .map(String::into_bytes)
            .collect();
        StreamingResponse::from_parts(self.status, self.headers, chunks)
    }
}

pub struct VcrRecorder {
    cassette_path: PathBuf,
    mode: VcrMode,
    test_name: String,
}

impl VcrRecorder {
    pub fn new(test_name: &str) -> Result<Self> {
        let mode = VcrMode::from_env()?.unwrap_or_else(default_mode);
        let cassette_dir =
            env_var(VCR_ENV_DIR).map_or_else(|| PathBuf::from(DEFAULT_CASSETTE_DIR), PathBuf::from);
        let cassette_name = sanitize_test_name(test_name);
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        Ok(Self {
            cassette_path,
            mode,
            test_name: test_name.to_string(),
        })
    }

    pub fn new_with(test_name: &str, mode: VcrMode, cassette_dir: impl AsRef<Path>) -> Self {
        let cassette_name = sanitize_test_name(test_name);
        let cassette_path = cassette_dir.as_ref().join(format!("{cassette_name}.json"));
        Self {
            cassette_path,
            mode,
            test_name: test_name.to_string(),
        }
    }

    pub const fn mode(&self) -> VcrMode {
        self.mode
    }

    pub fn cassette_path(&self) -> &Path {
        &self.cassette_path
    }

    pub async fn request_streaming(&self, request: RecordedRequest) -> Result<RecordedResponse> {
        match self.mode {
            VcrMode::Playback => self.playback(&request),
            VcrMode::Record => self.record_request(request).await,
            VcrMode::Auto => {
                if self.cassette_path.exists() {
                    self.playback(&request)
                } else {
                    self.record_request(request).await
                }
            }
        }
    }

    async fn record_request(&self, request: RecordedRequest) -> Result<RecordedResponse> {
        let client = Client::new();
        let response = self
            .record_streaming_with(request.clone(), || async {
                let mut builder = client.request(method_from_str(&request.method)?, &request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.clone(), value.clone());
                }
                if let Some(body) = &request.body {
                    builder = builder.json(body)?;
                } else if let Some(body_text) = &request.body_text {
                    builder = builder.body(body_text.clone());
                }
                let cx = Cx::for_request();
                builder.send_streaming(&cx).await
            })
            .await?;

        Ok(response)
    }

    pub async fn record_streaming_with<F, Fut>(
        &self,
        request: RecordedRequest,
        send: F,
    ) -> Result<RecordedResponse>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<StreamingResponse>>,
    {
        let response = send().await?;
        let status = response.status().as_u16();
        let headers = response.headers().to_vec();

        let mut body_chunks = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::api(format!("HTTP stream read failed: {e}")))?;
            if chunk.is_empty() {
                continue;
            }
            body_chunks.push(String::from_utf8_lossy(&chunk).to_string());
        }

        let recorded = RecordedResponse {
            status,
            headers,
            body_chunks,
        };

        let mut cassette = Cassette {
            version: CASSETTE_VERSION.to_string(),
            test_name: self.test_name.clone(),
            recorded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            interactions: vec![Interaction {
                request,
                response: recorded.clone(),
            }],
        };

        redact_cassette(&mut cassette);
        save_cassette(&self.cassette_path, &cassette)?;

        Ok(recorded)
    }

    fn playback(&self, request: &RecordedRequest) -> Result<RecordedResponse> {
        let cassette = load_cassette(&self.cassette_path)?;
        let interaction = find_interaction(&cassette, request).ok_or_else(|| {
            Error::config(format!(
                "No matching interaction found in cassette {}",
                self.cassette_path.display()
            ))
        })?;
        Ok(interaction.response.clone())
    }
}

fn default_mode() -> VcrMode {
    if env_truthy("CI") {
        VcrMode::Playback
    } else {
        VcrMode::Auto
    }
}

fn env_truthy(name: &str) -> bool {
    env_var(name).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn sanitize_test_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "vcr".to_string()
    } else {
        out
    }
}

fn method_from_str(value: &str) -> Result<Method> {
    match value.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "PATCH" => Ok(Method::Patch),
        "DELETE" => Ok(Method::Delete),
        "HEAD" => Ok(Method::Head),
        other => Err(Error::config(format!("Unsupported HTTP method: {other}"))),
    }
}

fn load_cassette(path: &Path) -> Result<Cassette> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("Failed to read cassette {}: {e}", path.display())))?;
    serde_json::from_str(&content)
        .map_err(|e| Error::config(format!("Failed to parse cassette {}: {e}", path.display())))
}

fn save_cassette(path: &Path, cassette: &Cassette) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::config(format!(
                "Failed to create cassette dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let content = serde_json::to_string_pretty(cassette)
        .map_err(|e| Error::config(format!("Failed to serialize cassette: {e}")))?;
    std::fs::write(path, content)
        .map_err(|e| Error::config(format!("Failed to write cassette {}: {e}", path.display())))?;
    Ok(())
}

fn find_interaction<'a>(
    cassette: &'a Cassette,
    request: &RecordedRequest,
) -> Option<&'a Interaction> {
    cassette
        .interactions
        .iter()
        .find(|interaction| request_matches(&interaction.request, request))
}

fn request_matches(recorded: &RecordedRequest, incoming: &RecordedRequest) -> bool {
    if !recorded.method.eq_ignore_ascii_case(&incoming.method) {
        return false;
    }
    if recorded.url != incoming.url {
        return false;
    }
    if let (Some(lhs), Some(rhs)) = (&recorded.body, &incoming.body) {
        if lhs != rhs {
            return false;
        }
    }
    if let (Some(lhs), Some(rhs)) = (&recorded.body_text, &incoming.body_text) {
        if lhs != rhs {
            return false;
        }
    }
    true
}

pub fn redact_cassette(cassette: &mut Cassette) {
    let sensitive_headers = sensitive_header_keys();
    for interaction in &mut cassette.interactions {
        redact_headers(&mut interaction.request.headers, &sensitive_headers);
        redact_headers(&mut interaction.response.headers, &sensitive_headers);
        if let Some(body) = &mut interaction.request.body {
            redact_json(body);
        }
    }
}

fn sensitive_header_keys() -> HashSet<String> {
    [
        "authorization",
        "x-api-key",
        "api-key",
        "x-goog-api-key",
        "x-azure-api-key",
        "proxy-authorization",
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

fn redact_headers(headers: &mut Vec<(String, String)>, sensitive: &HashSet<String>) {
    for (name, value) in headers {
        if sensitive.contains(&name.to_ascii_lowercase()) {
            *value = REDACTED.to_string();
        }
    }
}

fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *entry = Value::String(REDACTED.to_string());
                } else {
                    redact_json(entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("authorization")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[test]
    fn cassette_round_trip() {
        let cassette = Cassette {
            version: CASSETTE_VERSION.to_string(),
            test_name: "round_trip".to_string(),
            recorded_at: "2026-02-03T00:00:00.000Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://example.com".to_string(),
                    headers: vec![("authorization".to_string(), "secret".to_string())],
                    body: Some(serde_json::json!({"prompt": "hello"})),
                    body_text: None,
                },
                response: RecordedResponse {
                    status: 200,
                    headers: vec![("x-api-key".to_string(), "secret".to_string())],
                    body_chunks: vec!["event: message\n\n".to_string()],
                },
            }],
        };

        let serialized = serde_json::to_string(&cassette).expect("serialize cassette");
        let parsed: Cassette = serde_json::from_str(&serialized).expect("parse cassette");
        assert_eq!(parsed.version, CASSETTE_VERSION);
        assert_eq!(parsed.test_name, "round_trip");
        assert_eq!(parsed.interactions.len(), 1);
    }

    #[test]
    fn matches_interaction_on_method_url_body() {
        let recorded = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com".to_string(),
            headers: vec![],
            body: Some(serde_json::json!({"a": 1})),
            body_text: None,
        };
        let incoming = RecordedRequest {
            method: "post".to_string(),
            url: "https://example.com".to_string(),
            headers: vec![("x-api-key".to_string(), "secret".to_string())],
            body: Some(serde_json::json!({"a": 1})),
            body_text: None,
        };
        assert!(request_matches(&recorded, &incoming));
    }

    #[test]
    fn redacts_sensitive_headers_and_body_fields() {
        let mut cassette = Cassette {
            version: CASSETTE_VERSION.to_string(),
            test_name: "redact".to_string(),
            recorded_at: "2026-02-03T00:00:00.000Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://example.com".to_string(),
                    headers: vec![("Authorization".to_string(), "secret".to_string())],
                    body: Some(serde_json::json!({"api_key": "secret", "nested": {"token": "t"}})),
                    body_text: None,
                },
                response: RecordedResponse {
                    status: 200,
                    headers: vec![("x-api-key".to_string(), "secret".to_string())],
                    body_chunks: vec![],
                },
            }],
        };

        redact_cassette(&mut cassette);

        let request = &cassette.interactions[0].request;
        assert_eq!(request.headers[0].1, REDACTED);
        let body = request.body.as_ref().expect("body exists");
        assert_eq!(body["api_key"], REDACTED);
        assert_eq!(body["nested"]["token"], REDACTED);
    }

    #[test]
    fn record_and_playback_cycle() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cassette_dir = temp_dir.path().to_path_buf();

        let request = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(serde_json::json!({"prompt": "hello"})),
            body_text: None,
        };

        let recorded = run_async({
            let cassette_dir = cassette_dir.clone();
            let request = request.clone();
            async move {
                let recorder =
                    VcrRecorder::new_with("record_playback", VcrMode::Record, &cassette_dir);
                recorder
                    .record_streaming_with(request.clone(), || async {
                        Ok(StreamingResponse::from_parts(
                            200,
                            vec![("content-type".to_string(), "text/event-stream".to_string())],
                            vec![b"event: message\ndata: ok\n\n".to_vec()],
                        ))
                    })
                    .await
                    .expect("record")
            }
        });

        assert_eq!(recorded.status, 200);
        assert_eq!(recorded.body_chunks.len(), 1);

        let playback = run_async({
            let cassette_dir = cassette_dir;
            let request = request;
            async move {
                let recorder =
                    VcrRecorder::new_with("record_playback", VcrMode::Playback, &cassette_dir);
                recorder.request_streaming(request).await.expect("playback")
            }
        });

        assert_eq!(playback.body_chunks.len(), 1);
        assert!(playback.body_chunks[0].contains("event: message"));
    }

    fn run_async<T>(future: impl Future<Output = T> + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 2)
            .build()
            .expect("build runtime");
        let join = runtime.handle().spawn(future);
        runtime.block_on(join)
    }
}
