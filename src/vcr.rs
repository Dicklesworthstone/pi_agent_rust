//! VCR-style recording for HTTP streaming tests.
//!
//! This module provides utilities to record and replay real HTTP streaming
//! responses (e.g., SSE) for deterministic provider tests.

use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_chunks_base64: Option<Vec<String>>,
}

impl RecordedResponse {
    pub fn into_byte_stream(
        self,
    ) -> BoxStream<'static, std::result::Result<Vec<u8>, std::io::Error>> {
        if let Some(chunks) = self.body_chunks_base64 {
            stream::iter(chunks.into_iter().map(|chunk| {
                STANDARD
                    .decode(chunk)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
            }))
            .boxed()
        } else {
            stream::iter(
                self.body_chunks
                    .into_iter()
                    .map(|chunk| Ok(chunk.into_bytes())),
            )
            .boxed()
        }
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
        let mut body_chunks_base64: Option<Vec<String>> = None;
        let mut body_bytes = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::api(format!("HTTP stream read failed: {e}")))?;
            if chunk.is_empty() {
                continue;
            }
            body_bytes = body_bytes.saturating_add(chunk.len());
            if let Some(encoded) = body_chunks_base64.as_mut() {
                encoded.push(STANDARD.encode(&chunk));
            } else if let Ok(text) = std::str::from_utf8(&chunk) {
                body_chunks.push(text.to_string());
            } else {
                let mut encoded = Vec::with_capacity(body_chunks.len() + 1);
                for existing in &body_chunks {
                    encoded.push(STANDARD.encode(existing.as_bytes()));
                }
                encoded.push(STANDARD.encode(&chunk));
                body_chunks.clear();
                body_chunks_base64 = Some(encoded);
            }
        }

        let recorded = RecordedResponse {
            status,
            headers,
            body_chunks,
            body_chunks_base64,
        };
        let chunk_count = recorded
            .body_chunks_base64
            .as_ref()
            .map_or(recorded.body_chunks.len(), Vec::len);

        info!(
            cassette_path = %self.cassette_path.display(),
            status = recorded.status,
            header_count = recorded.headers.len(),
            chunk_count,
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

    if !match_optional_json(recorded.body.as_ref(), incoming_body.as_ref()) {
        return false;
    }

    // Treat a missing recorded `body_text` as a wildcard. This is useful for
    // tests where the JSON body is dynamic (paths, timestamps, etc.) and the
    // cassette only wants to constrain method+URL (and optionally structured JSON).
    if let Some(recorded_text) = recorded.body_text.as_ref() {
        if incoming.body_text.as_deref() != Some(recorded_text) {
            return false;
        }
    }

    true
}

fn match_optional_json(recorded: Option<&Value>, incoming: Option<&Value>) -> bool {
    let Some(recorded) = recorded else {
        // Cassette does not constrain JSON body.
        return true;
    };
    let Some(incoming) = incoming else {
        return false;
    };
    match_json_template(recorded, incoming)
}

/// Match a recorded JSON "template" against an incoming JSON value.
///
/// Semantics:
/// - Objects: recorded keys must match; incoming may have extra keys.
///   - A recorded key with `null` matches both missing and `null` incoming keys.
/// - Arrays: strict length + element matching (order-sensitive).
/// - Scalars: strict equality.
fn match_json_template(recorded: &Value, incoming: &Value) -> bool {
    match (recorded, incoming) {
        (Value::Object(recorded_obj), Value::Object(incoming_obj)) => {
            for (key, recorded_value) in recorded_obj {
                match incoming_obj.get(key) {
                    Some(incoming_value) => {
                        if !match_json_template(recorded_value, incoming_value) {
                            return false;
                        }
                    }
                    None => {
                        if !recorded_value.is_null() {
                            return false;
                        }
                    }
                }
            }
            true
        }
        (Value::Array(recorded_items), Value::Array(incoming_items)) => {
            if recorded_items.len() != incoming_items.len() {
                return false;
            }
            recorded_items
                .iter()
                .zip(incoming_items)
                .all(|(left, right)| match_json_template(left, right))
        }
        _ => recorded == incoming,
    }
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
    use std::sync::{Mutex, OnceLock};

    type ByteStream = BoxStream<'static, std::result::Result<Vec<u8>, std::io::Error>>;

    fn env_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
                    body_chunks_base64: None,
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
                    body_chunks_base64: None,
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
                            body_chunks_base64: None,
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

    #[test]
    fn vcr_mode_from_env_values_and_invalid() {
        let _lock = env_test_lock().lock().expect("env test lock");
        let previous = set_test_env_var(VCR_ENV_MODE, None);
        assert_eq!(VcrMode::from_env().expect("unset mode"), None);
        restore_test_env_var(VCR_ENV_MODE, previous);

        for (raw, expected) in [
            ("record", VcrMode::Record),
            ("PLAYBACK", VcrMode::Playback),
            ("Auto", VcrMode::Auto),
        ] {
            let previous = set_test_env_var(VCR_ENV_MODE, Some(raw));
            assert_eq!(VcrMode::from_env().expect("valid mode"), Some(expected));
            restore_test_env_var(VCR_ENV_MODE, previous);
        }

        let previous = set_test_env_var(VCR_ENV_MODE, Some("invalid-mode"));
        let err = VcrMode::from_env().expect_err("invalid mode should fail");
        assert!(
            err.to_string()
                .contains("Invalid VCR_MODE value: invalid-mode"),
            "unexpected error: {err}"
        );
        restore_test_env_var(VCR_ENV_MODE, previous);
    }

    #[test]
    fn auto_mode_records_missing_cassette_then_replays_existing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cassette_dir = temp_dir.path().to_path_buf();
        let cassette_path = cassette_dir.join("auto_mode_cycle.json");

        let request = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com/auto".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(serde_json::json!({"prompt": "first"})),
            body_text: None,
        };

        let first = run_async({
            let request = request.clone();
            let cassette_dir = cassette_dir.clone();
            async move {
                let recorder =
                    VcrRecorder::new_with("auto_mode_cycle", VcrMode::Auto, cassette_dir);
                recorder
                    .request_streaming_with(request, || async {
                        let recorded = RecordedResponse {
                            status: 201,
                            headers: vec![("x-source".to_string(), "record".to_string())],
                            body_chunks: vec!["chunk-one".to_string()],
                            body_chunks_base64: None,
                        };
                        Ok((
                            recorded.status,
                            recorded.headers.clone(),
                            recorded.into_byte_stream(),
                        ))
                    })
                    .await
                    .expect("auto record")
            }
        });

        assert_eq!(first.status, 201);
        assert!(
            cassette_path.exists(),
            "cassette should be written in auto mode"
        );

        let replay = run_async({
            async move {
                let recorder =
                    VcrRecorder::new_with("auto_mode_cycle", VcrMode::Auto, cassette_dir);
                recorder
                    .request_streaming_with::<_, _, ByteStream>(request, || async {
                        Err(Error::config(
                            "send callback should not run during auto playback",
                        ))
                    })
                    .await
                    .expect("auto playback")
            }
        });

        assert_eq!(replay.status, 201);
        assert_eq!(replay.body_chunks, vec!["chunk-one".to_string()]);
    }

    #[test]
    fn playback_mismatch_returns_strict_error_with_debug_hashes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cassette_dir = temp_dir.path().to_path_buf();

        let recorded_request = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com/strict".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(serde_json::json!({"prompt": "expected"})),
            body_text: Some("expected-body".to_string()),
        };

        run_async({
            let cassette_dir = cassette_dir.clone();
            async move {
                let recorder =
                    VcrRecorder::new_with("strict_mismatch", VcrMode::Record, cassette_dir);
                recorder
                    .request_streaming_with(recorded_request, || async {
                        let recorded = RecordedResponse {
                            status: 200,
                            headers: vec![("content-type".to_string(), "text/plain".to_string())],
                            body_chunks: vec!["ok".to_string()],
                            body_chunks_base64: None,
                        };
                        Ok((
                            recorded.status,
                            recorded.headers.clone(),
                            recorded.into_byte_stream(),
                        ))
                    })
                    .await
                    .expect("record strict cassette")
            }
        });

        let mismatched_request = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com/strict".to_string(),
            headers: vec![],
            body: Some(serde_json::json!({"prompt": "different"})),
            body_text: Some("different-body".to_string()),
        };

        let err = run_async({
            async move {
                let recorder =
                    VcrRecorder::new_with("strict_mismatch", VcrMode::Playback, cassette_dir);
                recorder
                    .request_streaming_with::<_, _, ByteStream>(mismatched_request, || async {
                        Err(Error::config(
                            "send callback should not execute during playback mismatch",
                        ))
                    })
                    .await
                    .expect_err("mismatch should fail in playback mode")
            }
        });

        let msg = err.to_string();
        assert!(
            msg.contains("No matching interaction found in cassette"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("Incoming: POST https://example.com/strict"));
        assert!(msg.contains("body_sha256="));
        assert!(msg.contains("body_text_sha256="));
        assert!(msg.contains("Match criteria: method + url + body + body_text"));
    }

    #[test]
    fn test_env_override_helpers_set_and_restore_values() {
        const TEST_VAR: &str = "PI_AGENT_VCR_TEST_ENV_OVERRIDE";
        let _lock = env_test_lock().lock().expect("env test lock");

        let original = set_test_env_var(TEST_VAR, None);
        assert_eq!(env_var(TEST_VAR), None);

        let previous = set_test_env_var(TEST_VAR, Some("override-value"));
        assert_eq!(previous, None);
        assert_eq!(env_var(TEST_VAR).as_deref(), Some("override-value"));

        restore_test_env_var(TEST_VAR, previous);
        assert_eq!(env_var(TEST_VAR), None);

        restore_test_env_var(TEST_VAR, original);
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

    // ─── sanitize_test_name ──────────────────────────────────────────

    #[test]
    fn sanitize_preserves_alphanumeric_and_dash_underscore() {
        assert_eq!(sanitize_test_name("hello-world_123"), "hello-world_123");
    }

    #[test]
    fn sanitize_replaces_special_chars() {
        assert_eq!(sanitize_test_name("a/b::c d.e"), "a_b__c_d_e");
    }

    #[test]
    fn sanitize_empty_returns_vcr() {
        assert_eq!(sanitize_test_name(""), "vcr");
    }

    #[test]
    fn sanitize_all_special_returns_underscores() {
        assert_eq!(sanitize_test_name("..."), "___");
    }

    #[test]
    fn sanitize_unicode_replaced() {
        assert_eq!(sanitize_test_name("café"), "caf_");
    }

    // ─── short_sha256 ────────────────────────────────────────────────

    #[test]
    fn short_sha256_deterministic() {
        let a = short_sha256(b"hello");
        let b = short_sha256(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn short_sha256_length() {
        let hash = short_sha256(b"test data");
        assert_eq!(hash.len(), 12, "6 bytes = 12 hex chars");
    }

    #[test]
    fn short_sha256_different_inputs() {
        let a = short_sha256(b"alpha");
        let b = short_sha256(b"beta");
        assert_ne!(a, b);
    }

    #[test]
    fn short_sha256_empty_input() {
        let hash = short_sha256(b"");
        assert_eq!(hash.len(), 12);
        // SHA-256 of empty is well-known: e3b0c44298fc...
        assert_eq!(&hash[..6], "e3b0c4");
    }

    // ─── is_sensitive_key ────────────────────────────────────────────

    #[test]
    fn sensitive_key_api_key() {
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("x_api_key"));
        assert!(is_sensitive_key("MY_APIKEY"));
    }

    #[test]
    fn sensitive_key_authorization() {
        assert!(is_sensitive_key("authorization"));
        assert!(is_sensitive_key("Authorization"));
    }

    #[test]
    fn sensitive_key_token_but_not_tokens() {
        // "token" (singular) is sensitive
        assert!(is_sensitive_key("access_token"));
        assert!(is_sensitive_key("id_token"));
        assert!(is_sensitive_key("refresh_token"));
        // "tokens" (count) is NOT sensitive...
        assert!(!is_sensitive_key("max_tokens"));
        assert!(!is_sensitive_key("prompt_tokens"));
        assert!(!is_sensitive_key("completion_tokens"));
        // ...except the plural versions of auth token names
        assert!(is_sensitive_key("access_tokens"));
        assert!(is_sensitive_key("refresh_tokens"));
    }

    #[test]
    fn sensitive_key_secret_and_password() {
        assert!(is_sensitive_key("client_secret"));
        assert!(is_sensitive_key("password"));
        assert!(is_sensitive_key("db_password_hash"));
    }

    #[test]
    fn sensitive_key_safe_keys() {
        assert!(!is_sensitive_key("model"));
        assert!(!is_sensitive_key("content"));
        assert!(!is_sensitive_key("messages"));
        assert!(!is_sensitive_key("temperature"));
    }

    // ─── redact_json ─────────────────────────────────────────────────

    #[test]
    fn redact_json_flat_object() {
        let mut val = serde_json::json!({"api_key": "sk-123", "model": "gpt-4"});
        let count = redact_json(&mut val);
        assert_eq!(count, 1);
        assert_eq!(val["api_key"], REDACTED);
        assert_eq!(val["model"], "gpt-4");
    }

    #[test]
    fn redact_json_nested() {
        let mut val = serde_json::json!({
            "config": {
                "secret": "hidden",
                "name": "test"
            }
        });
        let count = redact_json(&mut val);
        assert_eq!(count, 1);
        assert_eq!(val["config"]["secret"], REDACTED);
        assert_eq!(val["config"]["name"], "test");
    }

    #[test]
    fn redact_json_array_of_objects() {
        let mut val = serde_json::json!([
            {"api_key": "a"},
            {"api_key": "b"},
            {"safe": "c"}
        ]);
        let count = redact_json(&mut val);
        assert_eq!(count, 2);
        assert_eq!(val[0]["api_key"], REDACTED);
        assert_eq!(val[1]["api_key"], REDACTED);
        assert_eq!(val[2]["safe"], "c");
    }

    #[test]
    fn redact_json_scalar_returns_zero() {
        let mut val = serde_json::json!("just a string");
        assert_eq!(redact_json(&mut val), 0);
        let mut val = serde_json::json!(42);
        assert_eq!(redact_json(&mut val), 0);
        let mut val = serde_json::json!(null);
        assert_eq!(redact_json(&mut val), 0);
    }

    #[test]
    fn redact_json_empty_object() {
        let mut val = serde_json::json!({});
        assert_eq!(redact_json(&mut val), 0);
    }

    // ─── redact_headers ──────────────────────────────────────────────

    #[test]
    fn redact_headers_case_insensitive() {
        let sensitive = sensitive_header_keys();
        let mut headers = vec![
            ("Authorization".to_string(), "Bearer tok".to_string()),
            ("X-Api-Key".to_string(), "key".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let count = redact_headers(&mut headers, &sensitive);
        assert_eq!(count, 2);
        assert_eq!(headers[0].1, REDACTED);
        assert_eq!(headers[1].1, REDACTED);
        assert_eq!(headers[2].1, "application/json");
    }

    #[test]
    fn redact_headers_empty() {
        let sensitive = sensitive_header_keys();
        let mut headers = vec![];
        assert_eq!(redact_headers(&mut headers, &sensitive), 0);
    }

    #[test]
    fn redact_headers_all_sensitive_keys() {
        let sensitive = sensitive_header_keys();
        let keys = [
            "authorization",
            "x-api-key",
            "api-key",
            "x-goog-api-key",
            "x-azure-api-key",
            "proxy-authorization",
        ];
        let mut headers: Vec<(String, String)> = keys
            .iter()
            .map(|k| (k.to_string(), "secret".to_string()))
            .collect();
        let count = redact_headers(&mut headers, &sensitive);
        assert_eq!(count, 6);
        for (_, val) in &headers {
            assert_eq!(val, REDACTED);
        }
    }

    // ─── request_debug_key ───────────────────────────────────────────

    #[test]
    fn request_debug_key_with_body() {
        let req = RecordedRequest {
            method: "post".to_string(),
            url: "https://api.example.com/v1/chat".to_string(),
            headers: vec![],
            body: Some(serde_json::json!({"prompt": "hello"})),
            body_text: None,
        };
        let key = request_debug_key(&req);
        assert!(key.starts_with("POST https://api.example.com/v1/chat"));
        assert!(key.contains("body_sha256="));
        assert!(key.contains("body_text_sha256=<none>"));
    }

    #[test]
    fn request_debug_key_no_body() {
        let req = RecordedRequest {
            method: "GET".to_string(),
            url: "https://example.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        let key = request_debug_key(&req);
        assert!(key.contains("body_sha256=<none>"));
        assert!(key.contains("body_text_sha256=<none>"));
    }

    #[test]
    fn request_debug_key_with_body_text() {
        let req = RecordedRequest {
            method: "POST".to_string(),
            url: "https://example.com".to_string(),
            headers: vec![],
            body: None,
            body_text: Some("raw text body".to_string()),
        };
        let key = request_debug_key(&req);
        assert!(key.contains("body_text_sha256="));
        assert!(key.contains("body_text_len=13"));
        assert!(!key.contains("body_text_sha256=<none>"));
    }

    // ─── match_json_template ─────────────────────────────────────────

    #[test]
    fn json_template_exact_scalar_match() {
        let a = serde_json::json!("hello");
        let b = serde_json::json!("hello");
        assert!(match_json_template(&a, &b));
    }

    #[test]
    fn json_template_scalar_mismatch() {
        let a = serde_json::json!("hello");
        let b = serde_json::json!("world");
        assert!(!match_json_template(&a, &b));
    }

    #[test]
    fn json_template_number_match() {
        let a = serde_json::json!(42);
        let b = serde_json::json!(42);
        assert!(match_json_template(&a, &b));
    }

    #[test]
    fn json_template_object_extra_incoming_keys_ok() {
        let recorded = serde_json::json!({"model": "gpt-4"});
        let incoming = serde_json::json!({"model": "gpt-4", "extra": "ignored"});
        assert!(match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_object_missing_incoming_key_fails() {
        let recorded = serde_json::json!({"model": "gpt-4", "required": true});
        let incoming = serde_json::json!({"model": "gpt-4"});
        assert!(!match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_null_matches_missing_key() {
        let recorded = serde_json::json!({"model": "gpt-4", "optional": null});
        let incoming = serde_json::json!({"model": "gpt-4"});
        assert!(match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_null_matches_null() {
        let recorded = serde_json::json!({"field": null});
        let incoming = serde_json::json!({"field": null});
        assert!(match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_array_same_length_matches() {
        let recorded = serde_json::json!([1, 2, 3]);
        let incoming = serde_json::json!([1, 2, 3]);
        assert!(match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_array_different_length_fails() {
        let recorded = serde_json::json!([1, 2]);
        let incoming = serde_json::json!([1, 2, 3]);
        assert!(!match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_array_element_mismatch_fails() {
        let recorded = serde_json::json!([1, 2, 3]);
        let incoming = serde_json::json!([1, 99, 3]);
        assert!(!match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_nested_object_in_array() {
        let recorded = serde_json::json!([{"role": "user"}, {"role": "assistant"}]);
        let incoming = serde_json::json!([
            {"role": "user", "id": "1"},
            {"role": "assistant", "id": "2"}
        ]);
        assert!(match_json_template(&recorded, &incoming));
    }

    #[test]
    fn json_template_type_mismatch() {
        let recorded = serde_json::json!({"a": "string"});
        let incoming = serde_json::json!({"a": 42});
        assert!(!match_json_template(&recorded, &incoming));
    }

    // ─── match_optional_json ─────────────────────────────────────────

    #[test]
    fn optional_json_none_recorded_matches_anything() {
        assert!(match_optional_json(None, None));
        assert!(match_optional_json(
            None,
            Some(&serde_json::json!({"anything": true}))
        ));
    }

    #[test]
    fn optional_json_some_recorded_none_incoming_fails() {
        let recorded = serde_json::json!({"a": 1});
        assert!(!match_optional_json(Some(&recorded), None));
    }

    // ─── request_matches ─────────────────────────────────────────────

    #[test]
    fn request_matches_method_case_insensitive() {
        let recorded = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        let incoming = RecordedRequest {
            method: "post".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        assert!(request_matches(&recorded, &incoming));
    }

    #[test]
    fn request_matches_url_mismatch() {
        let recorded = RecordedRequest {
            method: "GET".to_string(),
            url: "https://a.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        let incoming = RecordedRequest {
            method: "GET".to_string(),
            url: "https://b.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        assert!(!request_matches(&recorded, &incoming));
    }

    #[test]
    fn request_matches_body_text_constraint() {
        let recorded = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: Some("expected".to_string()),
        };
        let mut incoming = recorded.clone();
        incoming.body_text = Some("expected".to_string());
        assert!(request_matches(&recorded, &incoming));

        incoming.body_text = Some("different".to_string());
        assert!(!request_matches(&recorded, &incoming));
    }

    #[test]
    fn request_matches_missing_recorded_body_text_is_wildcard() {
        let recorded = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        let incoming = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: Some("anything".to_string()),
        };
        assert!(request_matches(&recorded, &incoming));
    }

    #[test]
    fn request_matches_redacts_incoming_body() {
        // The cassette body is already redacted; incoming body has real secrets.
        let recorded = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: Some(serde_json::json!({"api_key": REDACTED, "model": "gpt-4"})),
            body_text: None,
        };
        let incoming = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: Some(serde_json::json!({"api_key": "sk-real-secret", "model": "gpt-4"})),
            body_text: None,
        };
        assert!(request_matches(&recorded, &incoming));
    }

    // ─── find_interaction_from ───────────────────────────────────────

    #[test]
    fn find_interaction_from_start() {
        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: "test".to_string(),
            recorded_at: "2026-01-01".to_string(),
            interactions: vec![
                Interaction {
                    request: RecordedRequest {
                        method: "GET".to_string(),
                        url: "https://a.com".to_string(),
                        headers: vec![],
                        body: None,
                        body_text: None,
                    },
                    response: RecordedResponse {
                        status: 200,
                        headers: vec![],
                        body_chunks: vec!["a".to_string()],
                        body_chunks_base64: None,
                    },
                },
                Interaction {
                    request: RecordedRequest {
                        method: "GET".to_string(),
                        url: "https://b.com".to_string(),
                        headers: vec![],
                        body: None,
                        body_text: None,
                    },
                    response: RecordedResponse {
                        status: 201,
                        headers: vec![],
                        body_chunks: vec!["b".to_string()],
                        body_chunks_base64: None,
                    },
                },
            ],
        };

        let req_b = RecordedRequest {
            method: "GET".to_string(),
            url: "https://b.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };

        let result = find_interaction_from(&cassette, &req_b, 0);
        assert!(result.is_some());
        let (idx, interaction) = result.unwrap();
        assert_eq!(idx, 1);
        assert_eq!(interaction.response.status, 201);
    }

    #[test]
    fn find_interaction_from_with_cursor_skip() {
        let make_interaction = |url: &str, status: u16| Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: vec![],
                body_chunks: vec![],
                body_chunks_base64: None,
            },
        };

        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: "cursor".to_string(),
            recorded_at: "2026-01-01".to_string(),
            interactions: vec![
                make_interaction("https://x.com", 200),
                make_interaction("https://x.com", 201),
                make_interaction("https://x.com", 202),
            ],
        };

        let req = RecordedRequest {
            method: "POST".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };

        // Start at 0 → finds index 0
        let (idx, _) = find_interaction_from(&cassette, &req, 0).unwrap();
        assert_eq!(idx, 0);

        // Start at 1 → skips index 0, finds index 1
        let (idx, interaction) = find_interaction_from(&cassette, &req, 1).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(interaction.response.status, 201);

        // Start at 3 → past end, nothing found
        assert!(find_interaction_from(&cassette, &req, 3).is_none());
    }

    #[test]
    fn find_interaction_no_match() {
        let cassette = Cassette {
            version: "1.0".to_string(),
            test_name: "empty".to_string(),
            recorded_at: "2026-01-01".to_string(),
            interactions: vec![],
        };
        let req = RecordedRequest {
            method: "GET".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        assert!(find_interaction_from(&cassette, &req, 0).is_none());
    }

    // ─── env_truthy ──────────────────────────────────────────────────

    #[test]
    fn env_truthy_values() {
        let _lock = env_test_lock().lock().expect("env test lock");
        let key = "PI_VCR_TEST_TRUTHY";

        for val in ["1", "true", "TRUE", "yes", "YES"] {
            let prev = set_test_env_var(key, Some(val));
            assert!(env_truthy(key), "expected truthy for '{val}'");
            restore_test_env_var(key, prev);
        }

        for val in ["0", "false", "no", ""] {
            let prev = set_test_env_var(key, Some(val));
            assert!(!env_truthy(key), "expected falsy for '{val}'");
            restore_test_env_var(key, prev);
        }

        let prev = set_test_env_var(key, None);
        assert!(!env_truthy(key), "expected falsy for unset");
        restore_test_env_var(key, prev);
    }

    // ─── default_mode ────────────────────────────────────────────────

    #[test]
    fn default_mode_ci_is_playback() {
        let _lock = env_test_lock().lock().expect("env test lock");
        let prev = set_test_env_var("CI", Some("true"));
        assert_eq!(default_mode(), VcrMode::Playback);
        restore_test_env_var("CI", prev);
    }

    #[test]
    fn default_mode_no_ci_is_auto() {
        let _lock = env_test_lock().lock().expect("env test lock");
        let prev = set_test_env_var("CI", None);
        assert_eq!(default_mode(), VcrMode::Auto);
        restore_test_env_var("CI", prev);
    }

    // ─── RecordedResponse::into_byte_stream ──────────────────────────

    #[test]
    fn into_byte_stream_text_chunks() {
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec!["hello ".to_string(), "world".to_string()],
            body_chunks_base64: None,
        };
        let chunks: Vec<Vec<u8>> = run_async(async move {
            use futures::StreamExt;
            resp.into_byte_stream()
                .map(|r| r.expect("chunk"))
                .collect()
                .await
        });
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], b"hello ");
        assert_eq!(chunks[1], b"world");
    }

    #[test]
    fn into_byte_stream_base64_chunks() {
        let chunk1 = STANDARD.encode(b"binary\x00data");
        let chunk2 = STANDARD.encode(b"\xff\xfe");
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec![],
            body_chunks_base64: Some(vec![chunk1, chunk2]),
        };
        let chunks: Vec<Vec<u8>> = run_async(async move {
            use futures::StreamExt;
            resp.into_byte_stream()
                .map(|r| r.expect("chunk"))
                .collect()
                .await
        });
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], b"binary\x00data");
        assert_eq!(chunks[1], b"\xff\xfe");
    }

    #[test]
    fn into_byte_stream_base64_takes_precedence() {
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec!["ignored".to_string()],
            body_chunks_base64: Some(vec![STANDARD.encode(b"used")]),
        };
        let chunks: Vec<Vec<u8>> = run_async(async move {
            use futures::StreamExt;
            resp.into_byte_stream()
                .map(|r| r.expect("chunk"))
                .collect()
                .await
        });
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], b"used");
    }

    #[test]
    fn into_byte_stream_empty() {
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec![],
            body_chunks_base64: None,
        };
        let chunks: Vec<Vec<u8>> = run_async(async move {
            use futures::StreamExt;
            resp.into_byte_stream()
                .map(|r| r.expect("chunk"))
                .collect()
                .await
        });
        assert!(chunks.is_empty());
    }

    #[test]
    fn into_byte_stream_invalid_base64_errors() {
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec![],
            body_chunks_base64: Some(vec!["not-valid-base64!!!".to_string()]),
        };
        let results: Vec<std::result::Result<Vec<u8>, std::io::Error>> = run_async(async move {
            use futures::StreamExt;
            resp.into_byte_stream().collect().await
        });
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    // ─── Cassette serialization ──────────────────────────────────────

    #[test]
    fn cassette_serde_body_text_omitted_when_none() {
        let req = RecordedRequest {
            method: "GET".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("body_text"));
        assert!(!json.contains("body"));
    }

    #[test]
    fn cassette_serde_body_text_present_when_some() {
        let req = RecordedRequest {
            method: "GET".to_string(),
            url: "https://x.com".to_string(),
            headers: vec![],
            body: None,
            body_text: Some("hello".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("body_text"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn cassette_response_base64_omitted_when_none() {
        let resp = RecordedResponse {
            status: 200,
            headers: vec![],
            body_chunks: vec!["data".to_string()],
            body_chunks_base64: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("body_chunks_base64"));
    }

    #[test]
    fn cassette_save_load_round_trip() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("subdir/test.json");
        let cassette = Cassette {
            version: CASSETTE_VERSION.to_string(),
            test_name: "save_load".to_string(),
            recorded_at: "2026-02-06T00:00:00.000Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://api.example.com".to_string(),
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: Some(serde_json::json!({"key": "value"})),
                    body_text: None,
                },
                response: RecordedResponse {
                    status: 200,
                    headers: vec![],
                    body_chunks: vec!["ok".to_string()],
                    body_chunks_base64: None,
                },
            }],
        };

        save_cassette(&path, &cassette).expect("save");
        assert!(path.exists());

        let loaded = load_cassette(&path).expect("load");
        assert_eq!(loaded.version, CASSETTE_VERSION);
        assert_eq!(loaded.test_name, "save_load");
        assert_eq!(loaded.interactions.len(), 1);
        assert_eq!(loaded.interactions[0].request.method, "POST");
    }

    #[test]
    fn load_cassette_missing_file_errors() {
        let result = load_cassette(Path::new("/nonexistent/cassette.json"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to read"));
    }

    // ─── redact_cassette integration ─────────────────────────────────

    #[test]
    fn redact_cassette_multiple_interactions() {
        let mut cassette = Cassette {
            version: "1.0".to_string(),
            test_name: "multi".to_string(),
            recorded_at: "now".to_string(),
            interactions: vec![
                Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: "https://a.com".to_string(),
                        headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
                        body: Some(serde_json::json!({"password": "p1"})),
                        body_text: None,
                    },
                    response: RecordedResponse {
                        status: 200,
                        headers: vec![("x-api-key".to_string(), "key1".to_string())],
                        body_chunks: vec![],
                        body_chunks_base64: None,
                    },
                },
                Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: "https://b.com".to_string(),
                        headers: vec![],
                        body: Some(serde_json::json!({"client_secret": "s1"})),
                        body_text: None,
                    },
                    response: RecordedResponse {
                        status: 200,
                        headers: vec![],
                        body_chunks: vec![],
                        body_chunks_base64: None,
                    },
                },
            ],
        };

        let summary = redact_cassette(&mut cassette);
        assert_eq!(summary.headers_redacted, 2);
        assert_eq!(summary.json_fields_redacted, 2);
    }

    // ─── VcrRecorder accessors ───────────────────────────────────────

    #[test]
    fn recorder_new_with_sets_mode_and_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let recorder = VcrRecorder::new_with("my::test_name", VcrMode::Playback, temp_dir.path());
        assert_eq!(recorder.mode(), VcrMode::Playback);
        assert!(
            recorder
                .cassette_path()
                .to_string_lossy()
                .contains("my__test_name.json")
        );
    }
}
