//! VCR-style recording for HTTP streaming tests.
//!
//! This module provides utilities to record and replay real HTTP streaming
//! responses (e.g., SSE) for deterministic provider tests.

use crate::error::{Error, Result};
use chrono::{SecondsFormat, Utc};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use tracing::{debug, info, warn};

pub const VCR_ENV_MODE: &str = "VCR_MODE";
pub const VCR_ENV_DIR: &str = "VCR_CASSETTE_DIR";
pub const DEFAULT_CASSETTE_DIR: &str = "tests/fixtures/vcr";
const CASSETTE_VERSION: &str = "1.0";
const REDACTED: &str = "[REDACTED]";

#[derive(Debug, Clone, Copy, Default)]
pub struct RedactionSummary {
    pub headers_redacted: usize,
    pub json_fields_redacted: usize,
}

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
    pub fn into_byte_stream(
        self,
    ) -> BoxStream<'static, std::result::Result<Vec<u8>, std::io::Error>> {
        stream::iter(
            self.body_chunks
                .into_iter()
                .map(|chunk| Ok(chunk.into_bytes())),
        )
        .boxed()
    }
}

#[derive(Debug, Clone)]
pub struct VcrRecorder {
    cassette_path: PathBuf,
    mode: VcrMode,
    test_name: String,
    playback_cursor: Arc<AtomicUsize>,
}

impl VcrRecorder {
    pub fn new(test_name: &str) -> Result<Self> {
        let mode = VcrMode::from_env()?.unwrap_or_else(default_mode);
        let cassette_dir =
            env_var(VCR_ENV_DIR).map_or_else(|| PathBuf::from(DEFAULT_CASSETTE_DIR), PathBuf::from);
        let cassette_name = sanitize_test_name(test_name);
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        let recorder = Self {
            cassette_path,
            mode,
            test_name: test_name.to_string(),
            playback_cursor: Arc::new(AtomicUsize::new(0)),
        };
        info!(
            mode = ?recorder.mode,
            cassette_path = %recorder.cassette_path.display(),
            test_name = %recorder.test_name,
            "VCR recorder initialized"
        );
        Ok(recorder)
    }

    pub fn new_with(test_name: &str, mode: VcrMode, cassette_dir: impl AsRef<Path>) -> Self {
        let cassette_name = sanitize_test_name(test_name);
        let cassette_path = cassette_dir.as_ref().join(format!("{cassette_name}.json"));
        Self {
            cassette_path,
            mode,
            test_name: test_name.to_string(),
            playback_cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub const fn mode(&self) -> VcrMode {
        self.mode
    }

    pub fn cassette_path(&self) -> &Path {
        &self.cassette_path
    }

    pub async fn request_streaming_with<F, Fut, S>(
        &self,
        request: RecordedRequest,
        send: F,
    ) -> Result<RecordedResponse>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(u16, Vec<(String, String)>, S)>>,
        S: futures::Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
    {
        let request_key = request_debug_key(&request);

        match self.mode {
            VcrMode::Playback => {
                info!(
                    cassette_path = %self.cassette_path.display(),
                    request = %request_key,
                    "VCR playback request"
                );
                self.playback(&request)
            }
            VcrMode::Record => {
                info!(
                    cassette_path = %self.cassette_path.display(),
                    request = %request_key,
                    "VCR recording request"
                );
                self.record_streaming_with(request, send).await
            }
            VcrMode::Auto => {
                if self.cassette_path.exists() {
                    info!(
                        cassette_path = %self.cassette_path.display(),
                        request = %request_key,
                        "VCR auto mode: cassette exists, using playback"
                    );
                    self.playback(&request)
                } else {
                    info!(
                        cassette_path = %self.cassette_path.display(),
                        request = %request_key,
                        "VCR auto mode: cassette missing, recording"
                    );
                    self.record_streaming_with(request, send).await
                }
            }
        }
    }

    pub async fn record_streaming_with<F, Fut, S>(
        &self,
        request: RecordedRequest,
        send: F,
    ) -> Result<RecordedResponse>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(u16, Vec<(String, String)>, S)>>,
        S: futures::Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
    {
        debug!(
            cassette_path = %self.cassette_path.display(),
            request = %request_debug_key(&request),
            "VCR record: sending streaming HTTP request"
        );
        let (status, headers, mut stream) = send().await?;

        let mut body_chunks = Vec::new();
        let mut body_bytes = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::api(format!("HTTP stream read failed: {e}")))?;
            if chunk.is_empty() {
                continue;
            }
            body_bytes = body_bytes.saturating_add(chunk.len());
            body_chunks.push(String::from_utf8_lossy(&chunk).to_string());
        }

        let recorded = RecordedResponse {
            status,
            headers,
            body_chunks,
        };

        info!(
            cassette_path = %self.cassette_path.display(),
            status = recorded.status,
            header_count = recorded.headers.len(),
            chunk_count = recorded.body_chunks.len(),
            body_bytes,
            "VCR record: captured streaming response"
        );

        let mut cassette = if self.cassette_path.exists() {
            load_cassette(&self.cassette_path)?
        } else {
            Cassette {
                version: CASSETTE_VERSION.to_string(),
                test_name: self.test_name.clone(),
                recorded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                interactions: Vec::new(),
            }
        };
        cassette.test_name.clone_from(&self.test_name);
        cassette.recorded_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        cassette.interactions.push(Interaction {
            request,
            response: recorded.clone(),
        });

        let redaction = redact_cassette(&mut cassette);
        info!(
            cassette_path = %self.cassette_path.display(),
            headers_redacted = redaction.headers_redacted,
            json_fields_redacted = redaction.json_fields_redacted,
            "VCR record: redacted sensitive data"
        );
        save_cassette(&self.cassette_path, &cassette)?;
        info!(
            cassette_path = %self.cassette_path.display(),
            "VCR record: saved cassette"
        );

        Ok(recorded)
    }

    fn playback(&self, request: &RecordedRequest) -> Result<RecordedResponse> {
        let cassette = load_cassette(&self.cassette_path)?;
        let start_index = self.playback_cursor.load(Ordering::SeqCst);
        let Some((matched_index, interaction)) =
            find_interaction_from(&cassette, request, start_index)
        else {
            let incoming_key = request_debug_key(request);
            let recorded_keys: Vec<String> = cassette
                .interactions
                .iter()
                .enumerate()
                .map(|(idx, interaction)| {
                    format!("[{idx}] {}", request_debug_key(&interaction.request))
                })
                .collect();

            warn!(
                cassette_path = %self.cassette_path.display(),
                request = %incoming_key,
                recorded_count = recorded_keys.len(),
                start_index,
                "VCR playback: no matching interaction"
            );

            let mut message = format!(
                "No matching interaction found in cassette {}.\nIncoming: {incoming_key}\nRecorded interactions ({}):\n",
                self.cassette_path.display(),
                recorded_keys.len()
            );
            for key in recorded_keys {
                message.push_str("  ");
                message.push_str(&key);
                message.push('\n');
            }

            // Always dump debug bodies to a file when VCR_DEBUG_BODY_FILE is set
            if let Ok(debug_path) = std::env::var("VCR_DEBUG_BODY_FILE") {
                use std::fmt::Write as _;

                let mut debug = String::new();
                if let Some(body) = &request.body {
                    let mut redacted = body.clone();
                    redact_json(&mut redacted);
                    if let Ok(pretty) = serde_json::to_string_pretty(&redacted) {
                        debug.push_str("=== INCOMING (redacted) ===\n");
                        debug.push_str(&pretty);
                        debug.push('\n');
                    }
                }
                for (idx, interaction) in cassette.interactions.iter().enumerate() {
                    if let Some(body) = &interaction.request.body {
                        if let Ok(pretty) = serde_json::to_string_pretty(body) {
                            let _ = writeln!(debug, "=== RECORDED [{idx}] ===");
                            debug.push_str(&pretty);
                            debug.push('\n');
                        }
                    }
                }
                let _ = std::fs::write(&debug_path, &debug);
            }

            if env_truthy("VCR_DEBUG_BODY") {
                use std::fmt::Write as _;

                let mut incoming_body = request.body.clone();
                if let Some(body) = &mut incoming_body {
                    redact_json(body);
                }

                if let Some(body) = &incoming_body {
                    if let Ok(pretty) = serde_json::to_string_pretty(body) {
                        message.push_str("\nIncoming JSON body (redacted):\n");
                        message.push_str(&pretty);
                        message.push('\n');
                    }
                }

                if let Some(body_text) = &request.body_text {
                    message.push_str("\nIncoming text body:\n");
                    message.push_str(body_text);
                    message.push('\n');
                }

                for (idx, interaction) in cassette.interactions.iter().enumerate() {
                    if let Some(body) = &interaction.request.body {
                        if let Ok(pretty) = serde_json::to_string_pretty(body) {
                            let _ = write!(message, "\nRecorded JSON body [{idx}]:\n");
                            message.push_str(&pretty);
                            message.push('\n');
                        }
                    }

                    if let Some(body_text) = &interaction.request.body_text {
                        let _ = write!(message, "\nRecorded text body [{idx}]:\n");
                        message.push_str(body_text);
                        message.push('\n');
                    }
                }
            }
            message.push_str(
                "Match criteria: method + url + body + body_text (headers ignored). If the request changed, re-record with VCR_MODE=record.",
            );
            return Err(Error::config(message));
        };

        info!(
            cassette_path = %self.cassette_path.display(),
            request = %request_debug_key(request),
            "VCR playback: matched interaction"
        );
        self.playback_cursor
            .store(matched_index + 1, Ordering::SeqCst);
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

fn find_interaction_from<'a>(
    cassette: &'a Cassette,
    request: &RecordedRequest,
    start: usize,
) -> Option<(usize, &'a Interaction)> {
    cassette
        .interactions
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, interaction)| request_matches(&interaction.request, request))
}

fn request_debug_key(request: &RecordedRequest) -> String {
    use std::fmt::Write as _;

    let method = request.method.to_ascii_uppercase();
    let mut out = format!("{method} {}", request.url);

    if let Some(body) = &request.body {
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        let hash = short_sha256(&body_bytes);
        let _ = write!(out, " body_sha256={hash}");
    } else {
        out.push_str(" body_sha256=<none>");
    }

    if let Some(body_text) = &request.body_text {
        let hash = short_sha256(body_text.as_bytes());
        let _ = write!(
            out,
            " body_text_sha256={hash} body_text_len={}",
            body_text.len()
        );
    } else {
        out.push_str(" body_text_sha256=<none>");
    }

    out
}

fn short_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(12);
    for b in &digest[..6] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn request_matches(recorded: &RecordedRequest, incoming: &RecordedRequest) -> bool {
    if !recorded.method.eq_ignore_ascii_case(&incoming.method) {
        return false;
    }
    if recorded.url != incoming.url {
        return false;
    }

    // Redact incoming body to match recorded body state (which is always redacted)
    let mut incoming_body = incoming.body.clone();
    if let Some(body) = &mut incoming_body {
        redact_json(body);
    }

    if recorded.body != incoming_body {
        return false;
    }
    if recorded.body_text != incoming.body_text {
        return false;
    }
    true
}

pub fn redact_cassette(cassette: &mut Cassette) -> RedactionSummary {
    let sensitive_headers = sensitive_header_keys();
    let mut summary = RedactionSummary::default();
    for interaction in &mut cassette.interactions {
        summary.headers_redacted +=
            redact_headers(&mut interaction.request.headers, &sensitive_headers);
        summary.headers_redacted +=
            redact_headers(&mut interaction.response.headers, &sensitive_headers);
        if let Some(body) = &mut interaction.request.body {
            summary.json_fields_redacted += redact_json(body);
        }
    }
    summary
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

fn redact_headers(headers: &mut Vec<(String, String)>, sensitive: &HashSet<String>) -> usize {
    let mut count = 0usize;
    for (name, value) in headers {
        if sensitive.contains(&name.to_ascii_lowercase()) {
            count += 1;
            *value = REDACTED.to_string();
        }
    }
    count
}

fn redact_json(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut count = 0usize;
            for (key, entry) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *entry = Value::String(REDACTED.to_string());
                    count += 1;
                } else {
                    count += redact_json(entry);
                }
            }
            count
        }
        Value::Array(items) => {
            let mut count = 0usize;
            for item in items {
                count += redact_json(item);
            }
            count
        }
        _ => 0usize,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("authorization")
        // "token" is sensitive when it refers to auth tokens (access_token, id_token, etc),
        // but many APIs also use fields like "max_tokens"/"prompt_tokens" which are just counts.
        // Redacting those breaks matching with existing cassettes and is not necessary.
        || ((key.contains("token") && !key.contains("tokens"))
            || key.contains("access_tokens")
            || key.contains("refresh_tokens")
            || key.contains("id_tokens"))
        || key.contains("secret")
        || key.contains("password")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    type ByteStream = BoxStream<'static, std::result::Result<Vec<u8>, std::io::Error>>;

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
    fn oauth_refresh_invalid_matches_after_redaction() {
        let cassette_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vcr/oauth_refresh_invalid.json");
        let cassette = load_cassette(&cassette_path).expect("load cassette");
        let recorded = &cassette.interactions.first().expect("interaction").request;
        let recorded_body = recorded.body.as_ref().expect("recorded body");
        let client_id = recorded_body
            .get("client_id")
            .and_then(serde_json::Value::as_str)
            .expect("client_id string");

        let incoming = RecordedRequest {
            method: "POST".to_string(),
            url: recorded.url.clone(),
            headers: Vec::new(),
            body: Some(serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": client_id,
                "refresh_token": "refresh-invalid",
            })),
            body_text: None,
        };

        assert!(request_matches(recorded, &incoming));
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

        let summary = redact_cassette(&mut cassette);

        let request = &cassette.interactions[0].request;
        assert_eq!(request.headers[0].1, REDACTED);
        let body = request.body.as_ref().expect("body exists");
        assert_eq!(body["api_key"], REDACTED);
        assert_eq!(body["nested"]["token"], REDACTED);
        assert_eq!(summary.headers_redacted, 2);
        assert_eq!(summary.json_fields_redacted, 2);
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
                        let recorded = RecordedResponse {
                            status: 200,
                            headers: vec![(
                                "content-type".to_string(),
                                "text/event-stream".to_string(),
                            )],
                            body_chunks: vec!["event: message\ndata: ok\n\n".to_string()],
                        };
                        Ok((
                            recorded.status,
                            recorded.headers.clone(),
                            recorded.into_byte_stream(),
                        ))
                    })
                    .await
                    .expect("record")
            }
        });

        assert_eq!(recorded.status, 200);
        assert_eq!(recorded.body_chunks.len(), 1);

        let playback = run_async(async move {
            let recorder =
                VcrRecorder::new_with("record_playback", VcrMode::Playback, &cassette_dir);
            recorder
                .request_streaming_with::<_, _, ByteStream>(request, || async {
                    Err(Error::config("Unexpected record in playback mode"))
                })
                .await
                .expect("playback")
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
