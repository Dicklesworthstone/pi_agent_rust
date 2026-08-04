//! Native-provider verification harness.
//!
//! Defines a **provider-agnostic** set of canonical scenarios and runs them
//! against every native provider using VCR cassettes for deterministic,
//! offline playback.  Each run produces a `VerificationReport` JSON artifact
//! that can be diffed across providers to surface behavior divergence.
//!
//! ## Design
//!
//! ```text
//! CanonicalScenario  ──►  fixture_for_<provider>()  ──►  VCR cassette
//!                         run_scenario()            ──►  StreamSummary
//!                         assert + artifact         ──►  VerificationReport
//! ```
//!
//! The harness is consumed by the **parity slices** (bd-3uqg.3.8.2/3) which
//! wire specific providers into the canonical scenario set.
//!
//! ## Running
//!
//! ```bash
//! # All harness tests (playback, no API keys needed):
//! cargo test provider_native_verify
//!
//! # Record real cassettes (Vertex example):
//! GOOGLE_CLOUD_API_KEY=... GOOGLE_CLOUD_PROJECT=... VCR_MODE=record \
//!   cargo test provider_native_verify::vertex_
//! ```

mod common;

use common::TestHarness;
use futures::{Stream, StreamExt};
use pi::http::client::Client;
use pi::model::{Message, StopReason, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions, ToolDef};
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// ============================================================================
// Shared helpers (mirror provider_streaming.rs so we're self-contained)
// ============================================================================

fn cassette_root() -> PathBuf {
    env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

#[allow(dead_code)]
fn vcr_mode() -> VcrMode {
    match env::var("VCR_MODE")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Some("record") => VcrMode::Record,
        Some("auto") => VcrMode::Auto,
        _ => VcrMode::Playback,
    }
}

#[allow(dead_code)]
fn vcr_strict() -> bool {
    env::var("VCR_STRICT")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn should_generate_fixture(path: &Path, cassette_name: &str) -> bool {
    if path.exists() {
        return false;
    }
    if matches!(vcr_mode(), VcrMode::Record) {
        return false;
    }
    assert!(
        !vcr_strict(),
        "missing cassette in strict mode: {cassette_name} ({})",
        path.display()
    );
    true
}

// ============================================================================
// Stream collection & summarization (self-contained copy)
// ============================================================================

struct StreamOutcome {
    events: Vec<StreamEvent>,
    stream_error: Option<String>,
}

async fn collect_events<S>(mut stream: S) -> StreamOutcome
where
    S: Stream<Item = pi::PiResult<StreamEvent>> + Unpin,
{
    let mut events = Vec::new();
    let mut stream_error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(err) => {
                stream_error = Some(err.to_string());
                break;
            }
        }
    }
    StreamOutcome {
        events,
        stream_error,
    }
}

#[derive(Debug)]
struct StreamSummary {
    timeline: Vec<String>,
    event_count: usize,
    has_start: bool,
    has_done: bool,
    has_error_event: bool,
    text: String,
    thinking: String,
    tool_calls: Vec<pi::model::ToolCall>,
    text_deltas: usize,
    thinking_deltas: usize,
    tool_call_deltas: usize,
    stop_reason: Option<StopReason>,
    stream_error: Option<String>,
}

fn summarize_events(outcome: &StreamOutcome) -> StreamSummary {
    let mut summary = StreamSummary {
        timeline: Vec::new(),
        event_count: outcome.events.len(),
        has_start: false,
        has_done: false,
        has_error_event: false,
        text: String::new(),
        thinking: String::new(),
        tool_calls: Vec::new(),
        text_deltas: 0,
        thinking_deltas: 0,
        tool_call_deltas: 0,
        stop_reason: None,
        stream_error: outcome.stream_error.clone(),
    };

    for event in &outcome.events {
        match event {
            StreamEvent::Start { .. } => {
                summary.has_start = true;
                summary.timeline.push("start".into());
            }
            StreamEvent::TextStart { .. } => summary.timeline.push("text_start".into()),
            StreamEvent::TextDelta { delta, .. } => {
                summary.text_deltas += 1;
                summary.text.push_str(delta);
                summary.timeline.push("text_delta".into());
            }
            StreamEvent::TextEnd { content, .. } => {
                summary.text.clone_from(content);
                summary.timeline.push("text_end".into());
            }
            StreamEvent::ThinkingStart { .. } => summary.timeline.push("thinking_start".into()),
            StreamEvent::ThinkingDelta { delta, .. } => {
                summary.thinking_deltas += 1;
                summary.thinking.push_str(delta);
                summary.timeline.push("thinking_delta".into());
            }
            StreamEvent::ThinkingEnd { content, .. } => {
                summary.thinking.clone_from(content);
                summary.timeline.push("thinking_end".into());
            }
            StreamEvent::ToolCallStart { .. } => summary.timeline.push("tool_call_start".into()),
            StreamEvent::ToolCallDelta { .. } => {
                summary.tool_call_deltas += 1;
                summary.timeline.push("tool_call_delta".into());
            }
            StreamEvent::ToolCallEnd { tool_call, .. } => {
                summary.tool_calls.push(tool_call.clone());
                summary.timeline.push("tool_call_end".into());
            }
            StreamEvent::Done { reason, .. } => {
                summary.has_done = true;
                summary.stop_reason = Some(*reason);
                summary.timeline.push("done".into());
            }
            StreamEvent::Error { reason, .. } => {
                summary.has_error_event = true;
                summary.stop_reason = Some(*reason);
                summary.timeline.push("error".into());
            }
        }
    }

    summary
}

// ============================================================================
// Canonical Scenarios
// ============================================================================

/// A provider-agnostic scenario specification.
#[derive(Clone)]
struct CanonicalScenario {
    /// Short identifier (used for cassette naming: `{provider}_{tag}`).
    tag: &'static str,
    /// Human-readable description.
    description: &'static str,
    /// Messages sent to the provider.
    messages: Vec<Message>,
    /// Tools available for the provider to invoke.
    tools: Vec<ToolDef>,
    /// Expected behavior of the stream.
    expectation: CanonicalExpectation,
}

/// Expected outcome of a canonical scenario.
#[derive(Clone, Debug)]
enum CanonicalExpectation {
    /// Expect a successful stream with these minimum properties.
    Stream(StreamExpectations),
    /// Expect the provider to return an HTTP error.
    Error(ErrorExpectation),
}

#[derive(Clone, Debug, Default)]
struct StreamExpectations {
    min_text_deltas: usize,
    min_tool_calls: usize,
    allowed_stop_reasons: Option<Vec<StopReason>>,
    require_unicode: bool,
    allow_stream_error: bool,
    /// When false, the harness does NOT require a `Start` event.
    /// Batch-response providers (e.g. Bedrock Converse) emit events from
    /// a fully-buffered JSON response and may skip `Start`.
    require_start_event: bool,
}

#[derive(Clone, Debug)]
struct ErrorExpectation {
    status: u16,
    contains: Option<&'static str>,
}

fn tool_echo() -> ToolDef {
    ToolDef {
        name: "echo".to_string(),
        description: "Echo the provided text.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"]
        }),
    }
}

fn tool_add() -> ToolDef {
    ToolDef {
        name: "add".to_string(),
        description: "Add two numbers.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        }),
    }
}

/// The full set of canonical verification scenarios.
fn canonical_scenarios() -> Vec<CanonicalScenario> {
    vec![
        // ── Text streaming ──────────────────────────────────────────
        CanonicalScenario {
            tag: "simple_text",
            description: "Basic text generation",
            messages: vec![user_text(
                "Say exactly: 'Hello from the verification harness.'",
            )],
            tools: vec![],
            expectation: CanonicalExpectation::Stream(StreamExpectations {
                min_text_deltas: 1,
                allowed_stop_reasons: Some(vec![StopReason::Stop]),
                ..Default::default()
            }),
        },
        CanonicalScenario {
            tag: "unicode_text",
            description: "Non-ASCII text generation",
            messages: vec![user_text(
                "Respond with exactly: '日本語テスト — émojis: 🦀🔥'",
            )],
            tools: vec![],
            expectation: CanonicalExpectation::Stream(StreamExpectations {
                min_text_deltas: 1,
                require_unicode: true,
                allowed_stop_reasons: Some(vec![StopReason::Stop]),
                ..Default::default()
            }),
        },
        // ── Tool use ────────────────────────────────────────────────
        CanonicalScenario {
            tag: "tool_call_single",
            description: "Single tool invocation",
            messages: vec![user_text("Use the echo tool to echo 'verification test'.")],
            tools: vec![tool_echo()],
            expectation: CanonicalExpectation::Stream(StreamExpectations {
                min_tool_calls: 1,
                allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
                ..Default::default()
            }),
        },
        CanonicalScenario {
            tag: "tool_call_multiple",
            description: "Multiple tool invocations",
            messages: vec![user_text(
                "Use the add tool to compute 2+3, then use echo to say 'done'.",
            )],
            tools: vec![tool_add(), tool_echo()],
            expectation: CanonicalExpectation::Stream(StreamExpectations {
                min_tool_calls: 2,
                allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
                ..Default::default()
            }),
        },
        // ── Error paths ─────────────────────────────────────────────
        CanonicalScenario {
            tag: "error_auth_401",
            description: "Authentication failure",
            messages: vec![user_text("Hello")],
            tools: vec![],
            expectation: CanonicalExpectation::Error(ErrorExpectation {
                status: 401,
                contains: None,
            }),
        },
        CanonicalScenario {
            tag: "error_bad_request_400",
            description: "Malformed request",
            messages: vec![user_text("Hello")],
            tools: vec![],
            expectation: CanonicalExpectation::Error(ErrorExpectation {
                status: 400,
                contains: None,
            }),
        },
        CanonicalScenario {
            tag: "error_rate_limit_429",
            description: "Rate limit exceeded",
            messages: vec![user_text("Hello")],
            tools: vec![],
            expectation: CanonicalExpectation::Error(ErrorExpectation {
                status: 429,
                contains: None,
            }),
        },
    ]
}

// ============================================================================
// Verification Report
// ============================================================================

/// Build a verification report artifact.
fn build_verification_report(
    provider_name: &str,
    scenario_tag: &str,
    description: &str,
    summary: &StreamSummary,
) -> Value {
    json!({
        "schema": "pi.test.native_verify.v1",
        "provider": provider_name,
        "scenario": scenario_tag,
        "description": description,
        "event_count": summary.event_count,
        "has_start": summary.has_start,
        "has_done": summary.has_done,
        "has_error_event": summary.has_error_event,
        "timeline": &summary.timeline,
        "stop_reason": summary.stop_reason.map(|r| format!("{r:?}")),
        "text_sha256": sha256_hex(summary.text.as_bytes()),
        "text_chars": summary.text.chars().count(),
        "text_deltas": summary.text_deltas,
        "thinking_chars": summary.thinking.chars().count(),
        "thinking_deltas": summary.thinking_deltas,
        "tool_call_count": summary.tool_calls.len(),
        "tool_call_names": summary.tool_calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
        "stream_error": summary.stream_error.as_deref(),
    })
}

fn write_report(harness: &TestHarness, provider: &str, tag: &str, report: &Value) {
    let file_name = format!("{provider}_{tag}.verify.json");
    let path = harness.temp_path(&file_name);
    let serialized = serde_json::to_string_pretty(report)
        .unwrap_or_else(|_| r#"{"schema":"serialization_error"}"#.to_string());
    std::fs::write(&path, serialized)
        .unwrap_or_else(|err| panic!("write verify artifact {}: {err}", path.display()));
    harness.record_artifact(format!("verify/{file_name}"), &path);
}

// ============================================================================
// Assertions
// ============================================================================

fn assert_stream_ok(
    _harness: &TestHarness,
    tag: &str,
    summary: &StreamSummary,
    expectations: &StreamExpectations,
) {
    if !expectations.allow_stream_error {
        assert!(
            summary.stream_error.is_none(),
            "{tag}: unexpected stream error {:?}",
            summary.stream_error
        );
    }

    if expectations.require_start_event && summary.event_count > 0 {
        assert!(summary.has_start, "{tag}: missing Start event");
    }

    if expectations.min_text_deltas > 0 {
        assert!(
            summary.text_deltas >= expectations.min_text_deltas,
            "{tag}: expected >= {} text deltas, got {}",
            expectations.min_text_deltas,
            summary.text_deltas
        );
    }

    if expectations.min_tool_calls > 0 {
        assert!(
            summary.tool_calls.len() >= expectations.min_tool_calls,
            "{tag}: expected >= {} tool calls, got {}",
            expectations.min_tool_calls,
            summary.tool_calls.len()
        );
    }

    if expectations.require_unicode {
        assert!(!summary.text.is_ascii(), "{tag}: expected unicode text");
    }

    if let Some(allowed) = &expectations.allowed_stop_reasons {
        let reason = summary
            .stop_reason
            .unwrap_or_else(|| panic!("{tag}: missing stop reason"));
        assert!(
            allowed.contains(&reason),
            "{tag}: stop reason {reason:?} not in {allowed:?}"
        );
    }

    assert_timeline_shape(tag, summary);
}

fn assert_timeline_shape(tag: &str, summary: &StreamSummary) {
    if summary.event_count == 0 {
        return;
    }

    assert!(
        !summary.timeline.is_empty(),
        "{tag}: non-empty event stream produced an empty timeline"
    );

    let done_idx = summary.timeline.iter().position(|step| step == "done");
    let error_idx = summary.timeline.iter().position(|step| step == "error");

    assert!(
        !(done_idx.is_some() && error_idx.is_some()),
        "{tag}: timeline cannot contain both done and error terminal events: {:?}",
        summary.timeline
    );

    if let Some(idx) = done_idx {
        assert_eq!(
            idx + 1,
            summary.timeline.len(),
            "{tag}: done must be terminal, timeline={:?}",
            summary.timeline
        );
    }

    if let Some(idx) = error_idx {
        assert_eq!(
            idx + 1,
            summary.timeline.len(),
            "{tag}: error must be terminal, timeline={:?}",
            summary.timeline
        );
    }

    if summary.has_start {
        let start_idx = summary
            .timeline
            .iter()
            .position(|step| step == "start")
            .unwrap_or_else(|| panic!("{tag}: has_start=true but no start event in timeline"));
        assert_eq!(
            start_idx, 0,
            "{tag}: start event must appear first, timeline={:?}",
            summary.timeline
        );
    }
}

fn assert_error_ok(tag: &str, message: &str, expectation: &ErrorExpectation) {
    let needle = format!("HTTP {}", expectation.status);
    assert!(
        message.contains(&needle),
        "{tag}: expected '{needle}' in error, got: {message}"
    );
    if let Some(fragment) = expectation.contains {
        assert!(
            message.contains(fragment),
            "{tag}: expected '{fragment}' in error, got: {message}"
        );
    }
}

fn assert_tool_schema_fidelity(
    tag: &str,
    tool_defs: &[ToolDef],
    tool_calls: &[pi::model::ToolCall],
) {
    for tool_call in tool_calls {
        let tool_def = tool_defs
            .iter()
            .find(|t| t.name == tool_call.name)
            .unwrap_or_else(|| panic!("{tag}: tool call '{}' has no schema", tool_call.name));
        let validator = jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&tool_def.parameters)
            .unwrap_or_else(|err| panic!("{tag}: invalid schema for '{}': {err}", tool_call.name));
        if let Err(err) = validator.validate(&tool_call.arguments) {
            panic!(
                "{tag}: tool '{}' args failed schema validation: {err}; args={}",
                tool_call.name, tool_call.arguments
            );
        }
    }
}

// ============================================================================
// VCR fixture generators (per wire format)
// ============================================================================

/// Generate an error fixture cassette for the Gemini/Vertex SSE wire format.
fn generate_vertex_error_fixture(
    cassette_path: &Path,
    cassette_name: &str,
    url: &str,
    status: u16,
) {
    let error_body = json!({
        "error": {
            "code": status,
            "message": format!("Simulated error {status}"),
            "status": match status {
                401 => "UNAUTHENTICATED",
                400 => "INVALID_ARGUMENT",
                429 => "RESOURCE_EXHAUSTED",
                _ => "INTERNAL",
            },
        },
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                    ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![serde_json::to_string(&error_body).unwrap_or_default()],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

/// Generate a simple text fixture cassette for the Gemini/Vertex SSE format.
fn generate_vertex_text_fixture(cassette_path: &Path, cassette_name: &str, url: &str, text: &str) {
    let chunk1 = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": text}]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 10,
            "totalTokenCount": 25
        }
    });

    let sse_chunk = format!("data: {}\n\n", serde_json::to_string(&chunk1).unwrap());

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                    ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body_chunks: vec![sse_chunk],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

/// Generate a tool-call fixture cassette for the Gemini/Vertex SSE format.
fn generate_vertex_tool_fixture(
    cassette_path: &Path,
    cassette_name: &str,
    url: &str,
    tool_name: &str,
    tool_args: &Value,
) {
    let chunk = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "name": tool_name,
                        "args": tool_args
                    }
                }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 25,
            "candidatesTokenCount": 15,
            "totalTokenCount": 40
        }
    });

    let sse_chunk = format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap());

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                    ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body_chunks: vec![sse_chunk],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

/// Build an OpenAI-compatible JSON error response body.
fn openai_error_body(status: u16) -> Value {
    let code = match status {
        401 => "invalid_api_key",
        400 => "invalid_request_error",
        429 => "rate_limit_exceeded",
        _ => "server_error",
    };
    json!({
        "error": {
            "message": format!("Simulated error {status}"),
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": code,
        }
    })
}

fn synthetic_tool_arg_value(type_name: Option<&str>) -> Value {
    match type_name {
        Some("number" | "integer") => json!(2),
        Some("boolean") => json!(true),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => json!("verification test"),
    }
}

fn synthesize_tool_args(tool: &ToolDef) -> Value {
    let schema = &tool.parameters;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let required_fields: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .filter(|fields: &Vec<String>| !fields.is_empty())
        .unwrap_or_else(|| properties.keys().cloned().collect());

    let mut args = serde_json::Map::new();
    for field in required_fields {
        let type_name = properties
            .get(&field)
            .and_then(|entry| entry.get("type"))
            .and_then(Value::as_str);
        args.insert(field, synthetic_tool_arg_value(type_name));
    }

    Value::Object(args)
}

/// Build an OpenAI-compatible SSE response for plain text generation.
fn openai_text_response(model: &str, text: &str) -> RecordedResponse {
    let chunk_start = json!({
        "id": "chatcmpl-verify-001",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": ""},
            "finish_reason": Value::Null
        }]
    });
    let chunk_text = json!({
        "id": "chatcmpl-verify-001",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": Value::Null
        }]
    });
    let chunk_done = json!({
        "id": "chatcmpl-verify-001",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 20,
            "completion_tokens": 10,
            "total_tokens": 30
        }
    });
    RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            format!("data: {}\n\n", serde_json::to_string(&chunk_start).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_text).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_done).unwrap()),
            "data: [DONE]\n\n".to_string(),
        ],
        body_chunks_base64: None,
    }
}

/// Build an OpenAI-compatible SSE response for single tool invocation.
fn openai_tool_response(model: &str, tool_name: &str, tool_args: &Value) -> RecordedResponse {
    let args = serde_json::to_string(tool_args).unwrap_or_else(|_| "{}".to_string());
    let chunk_start = json!({
        "id": "chatcmpl-verify-002",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": format!("call_verify_{tool_name}"),
                    "type": "function",
                    "function": {"name": tool_name, "arguments": ""}
                }]
            },
            "finish_reason": Value::Null
        }]
    });
    let chunk_args = json!({
        "id": "chatcmpl-verify-002",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": args}
                }]
            },
            "finish_reason": Value::Null
        }]
    });
    let chunk_done = json!({
        "id": "chatcmpl-verify-002",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 12,
            "total_tokens": 42
        }
    });
    RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            format!("data: {}\n\n", serde_json::to_string(&chunk_start).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_args).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_done).unwrap()),
            "data: [DONE]\n\n".to_string(),
        ],
        body_chunks_base64: None,
    }
}

fn openai_tool_response_multiple(model: &str, tools: &[ToolDef]) -> RecordedResponse {
    let selected: Vec<&ToolDef> = if tools.is_empty() {
        Vec::new()
    } else {
        tools.iter().take(2).collect()
    };

    let chunk_start_calls: Vec<Value> = selected
        .iter()
        .enumerate()
        .map(|(idx, tool)| {
            json!({
                "index": idx,
                "id": format!("call_verify_{}_{}", tool.name, idx),
                "type": "function",
                "function": {"name": tool.name, "arguments": ""}
            })
        })
        .collect();

    let chunk_args_calls: Vec<Value> = selected
        .iter()
        .enumerate()
        .map(|(idx, tool)| {
            let args = serde_json::to_string(&synthesize_tool_args(tool))
                .unwrap_or_else(|_| "{}".to_string());
            json!({
                "index": idx,
                "function": {"arguments": args}
            })
        })
        .collect();

    let chunk_start = json!({
        "id": "chatcmpl-verify-003",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": chunk_start_calls
            },
            "finish_reason": Value::Null
        }]
    });
    let chunk_args = json!({
        "id": "chatcmpl-verify-003",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": chunk_args_calls
            },
            "finish_reason": Value::Null
        }]
    });
    let chunk_done = json!({
        "id": "chatcmpl-verify-003",
        "object": "chat.completion.chunk",
        "created": 1_738_875_600,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 36,
            "completion_tokens": 18,
            "total_tokens": 54
        }
    });

    RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            format!("data: {}\n\n", serde_json::to_string(&chunk_start).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_args).unwrap()),
            format!("data: {}\n\n", serde_json::to_string(&chunk_done).unwrap()),
            "data: [DONE]\n\n".to_string(),
        ],
        body_chunks_base64: None,
    }
}

fn openai_tool_response_for_scenario(
    model: &str,
    scenario: &CanonicalScenario,
) -> RecordedResponse {
    if scenario.tag == "tool_call_multiple" {
        return openai_tool_response_multiple(model, &scenario.tools);
    }
    let tool = scenario.tools.first();
    let tool_name = tool.map_or("echo", |item| item.name.as_str());
    let tool_args = tool.map_or_else(
        || json!({"text": "verification test"}),
        synthesize_tool_args,
    );
    openai_tool_response(model, tool_name, &tool_args)
}

/// Generate an error fixture cassette for the Bedrock Converse JSON format.
fn generate_bedrock_error_fixture(
    cassette_path: &Path,
    cassette_name: &str,
    url: &str,
    status: u16,
) {
    let error_body = json!({
        "message": format!("Simulated error {status}"),
        "__type": match status {
            401 | 403 => "UnrecognizedClientException",
            400 => "ValidationException",
            429 => "ThrottlingException",
            _ => "InternalServerException",
        },
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![serde_json::to_string(&error_body).unwrap_or_default()],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

/// Generate a text response fixture for the Bedrock Converse JSON format.
fn generate_bedrock_text_fixture(cassette_path: &Path, cassette_name: &str, url: &str, text: &str) {
    let body = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": text}]
            }
        },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 15,
            "outputTokens": 10,
            "totalTokens": 25
        }
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![serde_json::to_string(&body).unwrap_or_default()],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

/// Generate a tool-call response fixture for the Bedrock Converse format.
fn generate_bedrock_tool_fixture(
    cassette_path: &Path,
    cassette_name: &str,
    url: &str,
    tool_name: &str,
    tool_args: &Value,
) {
    let body = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{
                    "toolUse": {
                        "toolUseId": format!("call_verify_{tool_name}"),
                        "name": tool_name,
                        "input": tool_args
                    }
                }]
            }
        },
        "stopReason": "tool_use",
        "usage": {
            "inputTokens": 25,
            "outputTokens": 15,
            "totalTokens": 40
        }
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![serde_json::to_string(&body).unwrap_or_default()],
                body_chunks_base64: None,
            },
        }],
    };

    write_cassette(cassette_path, &cassette);
}

fn write_cassette(path: &Path, cassette: &Cassette) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let serialized = serde_json::to_string_pretty(cassette).expect("Failed to serialize cassette");
    std::fs::write(path, serialized)
        .unwrap_or_else(|err| panic!("write cassette {}: {err}", path.display()));
}

// ============================================================================
// Harness runner
// ============================================================================

const SYSTEM_PROMPT: &str =
    "You are a test harness model. Follow instructions precisely and deterministically.";

/// Run a single canonical scenario against a pre-built provider.
///
/// Returns `true` if the scenario ran (cassette present), `false` if skipped.
async fn run_canonical_scenario(
    provider: &dyn Provider,
    scenario: &CanonicalScenario,
    harness: &TestHarness,
) -> bool {
    let context = Context {
        system_prompt: Some(SYSTEM_PROMPT.to_string().into()),
        messages: scenario.messages.clone().into(),
        tools: scenario.tools.clone().into(),
    };

    let options = StreamOptions {
        api_key: Some("vcr-playback".to_string()),
        max_tokens: Some(256),
        temperature: Some(0.0),
        ..Default::default()
    };

    let provider_name = provider.name();
    let tag = scenario.tag;

    harness
        .log()
        .info_ctx("scenario", "Running canonical scenario", |ctx| {
            ctx.push(("provider".into(), provider_name.to_string()));
            ctx.push(("tag".into(), tag.to_string()));
            ctx.push(("description".into(), scenario.description.to_string()));
        });

    match &scenario.expectation {
        CanonicalExpectation::Stream(expectations) => {
            let stream = provider
                .stream(&context, &options)
                .await
                .unwrap_or_else(|err| {
                    panic!("{provider_name}/{tag}: expected stream, got error: {err}")
                });
            let outcome = collect_events(stream).await;
            let summary = summarize_events(&outcome);

            harness.log().info_ctx("result", "Stream complete", |ctx| {
                ctx.push(("events".into(), summary.event_count.to_string()));
                ctx.push(("text_deltas".into(), summary.text_deltas.to_string()));
                ctx.push(("tool_calls".into(), summary.tool_calls.len().to_string()));
                if let Some(r) = summary.stop_reason {
                    ctx.push(("stop_reason".into(), format!("{r:?}")));
                }
            });

            assert_stream_ok(
                harness,
                &format!("{provider_name}/{tag}"),
                &summary,
                expectations,
            );
            assert_tool_schema_fidelity(
                &format!("{provider_name}/{tag}"),
                &scenario.tools,
                &summary.tool_calls,
            );

            let report =
                build_verification_report(provider_name, tag, scenario.description, &summary);
            write_report(harness, provider_name, tag, &report);
        }
        CanonicalExpectation::Error(expectation) => {
            let result = provider.stream(&context, &options).await;
            let Err(err) = result else {
                panic!("{provider_name}/{tag}: expected error, got success");
            };
            let message = err.to_string();
            assert_error_ok(&format!("{provider_name}/{tag}"), &message, expectation);

            harness.log().info("error", &message);
        }
    }

    true
}

// ============================================================================
// Vertex Provider Smoke Tests
// ============================================================================

mod vertex_smoke {
    use super::*;
    use pi::providers::vertex::VertexProvider;

    const TEST_PROJECT: &str = "verify-project";
    const TEST_LOCATION: &str = "us-central1";
    const TEST_MODEL: &str = "gemini-2.0-flash";

    fn vertex_url() -> String {
        format!(
            "https://{TEST_LOCATION}-aiplatform.googleapis.com/v1/projects/{TEST_PROJECT}/locations/{TEST_LOCATION}/publishers/google/models/{TEST_MODEL}:streamGenerateContent"
        )
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_vertex_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let url = vertex_url();
            match &scenario.expectation {
                CanonicalExpectation::Error(e) => {
                    generate_vertex_error_fixture(&path, &name, &url, e.status);
                }
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        let tool_args = json!({"text": "verification test"});
                        generate_vertex_tool_fixture(&path, &name, &url, tool_name, &tool_args);
                    } else if exp.require_unicode {
                        generate_vertex_text_fixture(
                            &path,
                            &name,
                            &url,
                            "日本語テスト — émojis: 🦀🔥",
                        );
                    } else {
                        generate_vertex_text_fixture(
                            &path,
                            &name,
                            &url,
                            "Hello from the verification harness.",
                        );
                    }
                }
            }
        }

        path
    }

    fn build_provider(tag: &str) -> VertexProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);

        VertexProvider::new(TEST_MODEL)
            .with_project(TEST_PROJECT)
            .with_location(TEST_LOCATION)
            .with_client(client)
    }

    #[test]
    fn vertex_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn vertex_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn vertex_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn vertex_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn vertex_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn vertex_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_vertex_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Bedrock Provider Smoke Tests
// ============================================================================

mod bedrock_smoke {
    use super::*;
    use pi::providers::bedrock::BedrockProvider;

    const TEST_MODEL: &str = "anthropic.claude-3-haiku-20240307-v1:0";
    const TEST_REGION: &str = "us-east-1";

    fn bedrock_url() -> String {
        format!("https://bedrock-runtime.{TEST_REGION}.amazonaws.com/model/{TEST_MODEL}/converse")
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_bedrock_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let url = bedrock_url();
            match &scenario.expectation {
                CanonicalExpectation::Error(e) => {
                    generate_bedrock_error_fixture(&path, &name, &url, e.status);
                }
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        let tool_args = json!({"text": "verification test"});
                        generate_bedrock_tool_fixture(&path, &name, &url, tool_name, &tool_args);
                    } else if exp.require_unicode {
                        generate_bedrock_text_fixture(
                            &path,
                            &name,
                            &url,
                            "日本語テスト — émojis: 🦀🔥",
                        );
                    } else {
                        generate_bedrock_text_fixture(
                            &path,
                            &name,
                            &url,
                            "Hello from the verification harness.",
                        );
                    }
                }
            }
        }

        path
    }

    fn build_provider(tag: &str) -> BedrockProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);

        // Use a temp auth path so we don't need real AWS credentials.
        let tmp = std::env::temp_dir().join("verify_bedrock_auth.json");
        if !tmp.exists() {
            // Write a minimal auth file with bearer token for VCR playback.
            let auth = json!({
                "version": "1.0",
                "entries": {
                    "amazon-bedrock": {
                        "token": "vcr-playback-token"
                    }
                }
            });
            std::fs::write(&tmp, serde_json::to_string_pretty(&auth).unwrap()).ok();
        }

        BedrockProvider::new(TEST_MODEL)
            .with_base_url(format!(
                "https://bedrock-runtime.{TEST_REGION}.amazonaws.com"
            ))
            .with_client(client)
    }

    #[test]
    fn bedrock_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_bedrock_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn bedrock_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_bedrock_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn bedrock_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_bedrock_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn bedrock_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_bedrock_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Copilot Provider Smoke Tests
// ============================================================================

mod copilot_smoke {
    use super::*;
    use pi::providers::copilot::CopilotProvider;

    const TEST_MODEL: &str = "gpt-4o-mini";
    const TEST_GITHUB_TOKEN: &str = "ghp_vcr_playback_token";
    const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
    const CHAT_BASE_URL: &str = "https://api.githubcopilot.com";
    const CHAT_URL: &str = "https://api.githubcopilot.com/chat/completions";

    fn cassette_name(tag: &str) -> String {
        format!("verify_copilot_{tag}")
    }

    fn token_success_interaction() -> Interaction {
        let body = json!({
            "token": "ghu_vcr_session_token",
            "expires_at": chrono::Utc::now().timestamp() + 3600,
            "endpoints": {
                "api": CHAT_BASE_URL
            }
        });
        Interaction {
            request: RecordedRequest {
                method: "GET".to_string(),
                url: TOKEN_URL.to_string(),
                headers: vec![
                    ("Accept".to_string(), "application/json".to_string()),
                    ("Authorization".to_string(), "token [REDACTED]".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![serde_json::to_string(&body).unwrap_or_default()],
                body_chunks_base64: None,
            },
        }
    }

    fn token_error_interaction(status: u16) -> Interaction {
        Interaction {
            request: RecordedRequest {
                method: "GET".to_string(),
                url: TOKEN_URL.to_string(),
                headers: vec![],
                body: None,
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![
                    serde_json::to_string(&json!({"message": format!("Simulated error {status}")}))
                        .unwrap_or_default(),
                ],
                body_chunks_base64: None,
            },
        }
    }

    fn chat_interaction(response: RecordedResponse) -> Interaction {
        Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: CHAT_URL.to_string(),
                headers: vec![
                    ("Accept".to_string(), "text/event-stream".to_string()),
                    ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ],
                body: None,
                body_text: None,
            },
            response,
        }
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let interactions = match &scenario.expectation {
                CanonicalExpectation::Error(e) if e.status == 401 => {
                    vec![token_error_interaction(e.status)]
                }
                CanonicalExpectation::Error(e) => vec![
                    token_success_interaction(),
                    chat_interaction(RecordedResponse {
                        status: e.status,
                        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                        body_chunks: vec![
                            serde_json::to_string(&openai_error_body(e.status)).unwrap_or_default(),
                        ],
                        body_chunks_base64: None,
                    }),
                ],
                CanonicalExpectation::Stream(exp) => {
                    let response = if exp.min_tool_calls > 0 {
                        openai_tool_response_for_scenario(TEST_MODEL, scenario)
                    } else if exp.require_unicode {
                        openai_text_response(TEST_MODEL, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        openai_text_response(TEST_MODEL, "Hello from the verification harness.")
                    };
                    vec![token_success_interaction(), chat_interaction(response)]
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions,
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> CopilotProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);

        CopilotProvider::new(TEST_MODEL, TEST_GITHUB_TOKEN).with_client(client)
    }

    #[test]
    fn copilot_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn copilot_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn copilot_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn copilot_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn copilot_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn copilot_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_copilot_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// GitLab Provider Smoke Tests
// ============================================================================

mod gitlab_smoke {
    use super::*;
    use pi::providers::gitlab::GitLabProvider;

    const TEST_MODEL: &str = "gitlab-duo-chat";
    const BASE_URL: &str = "https://gitlab.com";

    fn gitlab_url() -> String {
        format!("{BASE_URL}/api/v4/chat/completions")
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_gitlab_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(
                            &json!({"message": format!("Simulated error {}", e.status)}),
                        )
                        .unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    let text = if exp.require_unicode {
                        "日本語テスト — émojis: 🦀🔥"
                    } else {
                        "Hello from the verification harness."
                    };
                    RecordedResponse {
                        status: 200,
                        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                        body_chunks: vec![
                            serde_json::to_string(&json!({"response": text})).unwrap_or_default(),
                        ],
                        body_chunks_base64: None,
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: gitlab_url(),
                        headers: vec![
                            ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                            ("Accept".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> GitLabProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        GitLabProvider::new(TEST_MODEL)
            .with_base_url(BASE_URL)
            .with_client(client)
    }

    #[test]
    fn gitlab_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gitlab_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gitlab_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gitlab_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gitlab_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gitlab_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gitlab_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gitlab_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gitlab_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gitlab_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Harness metadata test — validates the canonical scenario set itself.
// ============================================================================

#[test]
fn canonical_scenarios_are_valid() {
    let scenarios = canonical_scenarios();
    assert!(
        scenarios.len() >= 5,
        "Expected at least 5 canonical scenarios, got {}",
        scenarios.len()
    );

    // Tags must be unique.
    let mut tags: Vec<&str> = scenarios.iter().map(|s| s.tag).collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(
        tags.len(),
        scenarios.len(),
        "Duplicate tags in canonical scenarios"
    );

    // Every scenario must have at least one message.
    for scenario in &scenarios {
        assert!(
            !scenario.messages.is_empty(),
            "Scenario '{}' has no messages",
            scenario.tag
        );
    }

    // Tool scenarios must provide at least one tool definition.
    for scenario in &scenarios {
        if let CanonicalExpectation::Stream(exp) = &scenario.expectation
            && exp.min_tool_calls > 0
        {
            assert!(
                !scenario.tools.is_empty(),
                "Scenario '{}' expects tool calls but provides no tools",
                scenario.tag
            );
        }
    }
}

/// Cross-provider report comparison: ensures all reports follow the same schema.
#[test]
fn verification_report_schema_is_consistent() {
    let summary = StreamSummary {
        timeline: vec!["start".into(), "text_delta".into(), "done".into()],
        event_count: 3,
        has_start: true,
        has_done: true,
        has_error_event: false,
        text: "Hello".into(),
        thinking: String::new(),
        tool_calls: vec![],
        text_deltas: 1,
        thinking_deltas: 0,
        tool_call_deltas: 0,
        stop_reason: Some(StopReason::Stop),
        stream_error: None,
    };

    let report = build_verification_report("test-provider", "simple_text", "Test", &summary);

    // Validate required fields exist.
    assert_eq!(report["schema"], "pi.test.native_verify.v1");
    assert_eq!(report["provider"], "test-provider");
    assert_eq!(report["scenario"], "simple_text");
    assert!(report["event_count"].is_number());
    assert!(report["has_start"].is_boolean());
    assert!(report["has_done"].is_boolean());
    assert!(report["timeline"].is_array());
    assert!(report["text_sha256"].is_string());
    assert!(report["text_chars"].is_number());
    assert!(report["tool_call_count"].is_number());
    assert!(report["tool_call_names"].is_array());
}

// ============================================================================
// OpenAI Provider Smoke Tests
// ============================================================================

mod openai_smoke {
    use super::*;
    use pi::providers::openai::OpenAIProvider;

    const TEST_MODEL: &str = "gpt-4o-mini";
    const API_URL: &str = "https://api.openai.com/v1/chat/completions";

    fn cassette_name(tag: &str) -> String {
        format!("verify_openai_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&openai_error_body(e.status)).unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        openai_tool_response_for_scenario(TEST_MODEL, scenario)
                    } else if exp.require_unicode {
                        openai_text_response(TEST_MODEL, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        openai_text_response(TEST_MODEL, "Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: API_URL.to_string(),
                        headers: vec![
                            ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> OpenAIProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        OpenAIProvider::new(TEST_MODEL).with_client(client)
    }

    #[test]
    fn openai_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_tool_call_multiple() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_multiple")
            .unwrap();
        ensure_fixture("tool_call_multiple", scenario);
        let provider = build_provider("tool_call_multiple");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_tool_call_multiple");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn openai_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_openai_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// SAP AI Core Provider Smoke Tests
// ============================================================================

mod sap_ai_core_smoke {
    use super::*;
    use pi::providers::openai::OpenAIProvider;

    const TEST_DEPLOYMENT: &str = "verify-deployment";
    const SERVICE_URL: &str = "https://api.ai.sap.example.com";

    fn sap_url() -> String {
        format!("{SERVICE_URL}/v2/inference/deployments/{TEST_DEPLOYMENT}/chat/completions")
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_sap_ai_core_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&openai_error_body(e.status)).unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        openai_tool_response_for_scenario(TEST_DEPLOYMENT, scenario)
                    } else if exp.require_unicode {
                        openai_text_response(TEST_DEPLOYMENT, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        openai_text_response(
                            TEST_DEPLOYMENT,
                            "Hello from the verification harness.",
                        )
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: sap_url(),
                        headers: vec![
                            ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> OpenAIProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        OpenAIProvider::new(TEST_DEPLOYMENT)
            .with_provider_name("sap-ai-core")
            .with_base_url(sap_url())
            .with_client(client)
    }

    #[test]
    fn sap_ai_core_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_tool_call_multiple() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_multiple")
            .unwrap();
        ensure_fixture("tool_call_multiple", scenario);
        let provider = build_provider("tool_call_multiple");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_tool_call_multiple");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn sap_ai_core_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_sap_ai_core_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Wave B1 Representative Smoke Tests (regional + coding-plan presets)
// ============================================================================

mod wave_b1_smoke {
    use super::*;
    use pi::providers::anthropic::AnthropicProvider;
    use pi::providers::openai::OpenAIProvider;

    const ALIBABA_CN_PROVIDER: &str = "alibaba-cn";
    const ALIBABA_CN_MODEL: &str = "qwen-plus";
    const ALIBABA_CN_URL: &str =
        "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions";

    const KIMI_FOR_CODING_PROVIDER: &str = "kimi-for-coding";
    const KIMI_FOR_CODING_MODEL: &str = "k2p5";
    const KIMI_FOR_CODING_URL: &str = "https://api.kimi.com/coding/v1/messages";

    const MINIMAX_PROVIDER: &str = "minimax";
    const MINIMAX_MODEL: &str = "MiniMax-M2.1";
    const MINIMAX_URL: &str = "https://api.minimax.io/anthropic/v1/messages";

    fn scenario_by_tag(tag: &str) -> CanonicalScenario {
        canonical_scenarios()
            .into_iter()
            .find(|s| s.tag == tag)
            .unwrap_or_else(|| panic!("missing canonical scenario: {tag}"))
    }

    fn openai_cassette_name(provider_id: &str, tag: &str) -> String {
        format!("verify_{provider_id}_{tag}")
    }

    fn ensure_openai_fixture(
        provider_id: &str,
        model: &str,
        url: &str,
        scenario: &CanonicalScenario,
    ) -> PathBuf {
        let dir = cassette_root();
        let name = openai_cassette_name(provider_id, scenario.tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&openai_error_body(e.status)).unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        openai_tool_response_for_scenario(model, scenario)
                    } else if exp.require_unicode {
                        openai_text_response(model, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        openai_text_response(model, "Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: url.to_string(),
                        headers: vec![
                            ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_openai_provider(
        provider_id: &str,
        model: &str,
        url: &str,
        tag: &str,
    ) -> OpenAIProvider {
        let cassette_dir = cassette_root();
        let name = openai_cassette_name(provider_id, tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        OpenAIProvider::new(model)
            .with_provider_name(provider_id)
            .with_base_url(url)
            .with_client(client)
    }

    fn anthropic_cassette_name(provider_id: &str, tag: &str) -> String {
        format!("verify_{provider_id}_{tag}")
    }

    fn anthropic_text_sse(model: &str, text: &str) -> RecordedResponse {
        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": "msg_verify_b1_001",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 20, "output_tokens": 1}
                }
            }))
            .unwrap_or_default()
        );
        let block_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }))
            .unwrap_or_default()
        );
        let block_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }))
            .unwrap_or_default()
        );
        let block_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 0
            }))
            .unwrap_or_default()
        );
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                "usage": {"output_tokens": 10}
            }))
            .unwrap_or_default()
        );
        let msg_stop = format!(
            "event: message_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({"type": "message_stop"})).unwrap_or_default()
        );

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                msg_start,
                block_start,
                block_delta,
                block_stop,
                msg_delta,
                msg_stop,
            ],
            body_chunks_base64: None,
        }
    }

    fn anthropic_tool_sse(model: &str, tool_name: &str, tool_args: &Value) -> RecordedResponse {
        let args_str = serde_json::to_string(tool_args).unwrap_or_else(|_| "{}".to_string());
        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": "msg_verify_b1_002",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 25, "output_tokens": 1}
                }
            }))
            .unwrap_or_default()
        );
        let block_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": format!("toolu_verify_b1_{tool_name}"),
                    "name": tool_name,
                    "input": {}
                }
            }))
            .unwrap_or_default()
        );
        let block_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": args_str}
            }))
            .unwrap_or_default()
        );
        let block_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 0
            }))
            .unwrap_or_default()
        );
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": Value::Null},
                "usage": {"output_tokens": 12}
            }))
            .unwrap_or_default()
        );
        let msg_stop = format!(
            "event: message_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({"type": "message_stop"})).unwrap_or_default()
        );

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                msg_start,
                block_start,
                block_delta,
                block_stop,
                msg_delta,
                msg_stop,
            ],
            body_chunks_base64: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn anthropic_multi_tool_sse(model: &str, tool_defs: &[ToolDef]) -> RecordedResponse {
        let first_tool = tool_defs.first().expect("need at least 1 tool");
        let second_tool = tool_defs.get(1).unwrap_or(first_tool);
        let args0 = super::synthesize_tool_args(first_tool);
        let args1 = super::synthesize_tool_args(second_tool);
        let args0_str = serde_json::to_string(&args0).unwrap_or_else(|_| "{}".to_string());
        let args1_str = serde_json::to_string(&args1).unwrap_or_else(|_| "{}".to_string());

        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": "msg_verify_b1_003",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 30, "output_tokens": 1}
                }
            }))
            .unwrap_or_default()
        );
        let block0_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": format!("toolu_verify_b1_{}", first_tool.name),
                    "name": first_tool.name,
                    "input": {}
                }
            }))
            .unwrap_or_default()
        );
        let block0_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": args0_str}
            }))
            .unwrap_or_default()
        );
        let block0_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 0
            }))
            .unwrap_or_default()
        );
        let block1_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": format!("toolu_verify_b1_{}", second_tool.name),
                    "name": second_tool.name,
                    "input": {}
                }
            }))
            .unwrap_or_default()
        );
        let block1_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": args1_str}
            }))
            .unwrap_or_default()
        );
        let block1_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 1
            }))
            .unwrap_or_default()
        );
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": Value::Null},
                "usage": {"output_tokens": 18}
            }))
            .unwrap_or_default()
        );
        let msg_stop = format!(
            "event: message_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({"type": "message_stop"})).unwrap_or_default()
        );

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                msg_start,
                block0_start,
                block0_delta,
                block0_stop,
                block1_start,
                block1_delta,
                block1_stop,
                msg_delta,
                msg_stop,
            ],
            body_chunks_base64: None,
        }
    }

    fn ensure_anthropic_fixture(
        provider_id: &str,
        model: &str,
        url: &str,
        scenario: &CanonicalScenario,
    ) -> PathBuf {
        let dir = cassette_root();
        let name = anthropic_cassette_name(provider_id, scenario.tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&json!({
                            "type": "error",
                            "error": {
                                "type": "authentication_error",
                                "message": format!("Simulated error {}", e.status)
                            }
                        }))
                        .unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 1 {
                        anthropic_multi_tool_sse(model, &scenario.tools)
                    } else if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        anthropic_tool_sse(model, tool_name, &json!({"text": "verification test"}))
                    } else if exp.require_unicode {
                        anthropic_text_sse(model, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        anthropic_text_sse(model, "Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: url.to_string(),
                        headers: vec![
                            ("Content-Type".to_string(), "application/json".to_string()),
                            ("X-API-Key".to_string(), "[REDACTED]".to_string()),
                            ("anthropic-version".to_string(), "2023-06-01".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_anthropic_provider(
        provider_id: &str,
        model: &str,
        url: &str,
        tag: &str,
    ) -> AnthropicProvider {
        let cassette_dir = cassette_root();
        let name = anthropic_cassette_name(provider_id, tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        AnthropicProvider::new(model)
            .with_base_url(url)
            .with_client(client)
    }

    pub fn run_openai_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        let scenario = scenario_by_tag(tag);
        ensure_openai_fixture(provider_id, model, url, &scenario);
        let provider = build_openai_provider(provider_id, model, url, tag);

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new(format!("verify_{provider_id}_{tag}"));
                run_canonical_scenario(&provider, &scenario, &harness).await;
            });
    }

    pub fn run_anthropic_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        let scenario = scenario_by_tag(tag);
        ensure_anthropic_fixture(provider_id, model, url, &scenario);
        let provider = build_anthropic_provider(provider_id, model, url, tag);

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new(format!("verify_{provider_id}_{tag}"));
                run_canonical_scenario(&provider, &scenario, &harness).await;
            });
    }

    #[test]
    fn b1_alibaba_cn_simple_text() {
        run_openai_case(
            ALIBABA_CN_PROVIDER,
            ALIBABA_CN_MODEL,
            ALIBABA_CN_URL,
            "simple_text",
        );
    }

    #[test]
    fn b1_alibaba_cn_tool_call_single() {
        run_openai_case(
            ALIBABA_CN_PROVIDER,
            ALIBABA_CN_MODEL,
            ALIBABA_CN_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b1_alibaba_cn_tool_call_multiple() {
        run_openai_case(
            ALIBABA_CN_PROVIDER,
            ALIBABA_CN_MODEL,
            ALIBABA_CN_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b1_alibaba_cn_error_auth_401() {
        run_openai_case(
            ALIBABA_CN_PROVIDER,
            ALIBABA_CN_MODEL,
            ALIBABA_CN_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b1_kimi_for_coding_simple_text() {
        run_anthropic_case(
            KIMI_FOR_CODING_PROVIDER,
            KIMI_FOR_CODING_MODEL,
            KIMI_FOR_CODING_URL,
            "simple_text",
        );
    }

    #[test]
    fn b1_kimi_for_coding_tool_call_single() {
        run_anthropic_case(
            KIMI_FOR_CODING_PROVIDER,
            KIMI_FOR_CODING_MODEL,
            KIMI_FOR_CODING_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b1_kimi_for_coding_tool_call_multiple() {
        run_anthropic_case(
            KIMI_FOR_CODING_PROVIDER,
            KIMI_FOR_CODING_MODEL,
            KIMI_FOR_CODING_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b1_kimi_for_coding_error_auth_401() {
        run_anthropic_case(
            KIMI_FOR_CODING_PROVIDER,
            KIMI_FOR_CODING_MODEL,
            KIMI_FOR_CODING_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b1_minimax_simple_text() {
        run_anthropic_case(MINIMAX_PROVIDER, MINIMAX_MODEL, MINIMAX_URL, "simple_text");
    }

    #[test]
    fn b1_minimax_tool_call_single() {
        run_anthropic_case(
            MINIMAX_PROVIDER,
            MINIMAX_MODEL,
            MINIMAX_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b1_minimax_tool_call_multiple() {
        run_anthropic_case(
            MINIMAX_PROVIDER,
            MINIMAX_MODEL,
            MINIMAX_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b1_minimax_error_auth_401() {
        run_anthropic_case(
            MINIMAX_PROVIDER,
            MINIMAX_MODEL,
            MINIMAX_URL,
            "error_auth_401",
        );
    }
}

// ============================================================================
// Wave B2 Representative Smoke Tests (regional + cloud presets)
// ============================================================================

mod wave_b2_smoke {
    const MODELSCOPE_PROVIDER: &str = "modelscope";
    const MODELSCOPE_MODEL: &str = "ZhipuAI/GLM-4.5";
    const MODELSCOPE_URL: &str = "https://api-inference.modelscope.cn/v1/chat/completions";

    const MOONSHOT_CN_PROVIDER: &str = "moonshotai-cn";
    const MOONSHOT_CN_MODEL: &str = "kimi-k2-0905-preview";
    const MOONSHOT_CN_URL: &str = "https://api.moonshot.cn/v1/chat/completions";

    const NEBIUS_PROVIDER: &str = "nebius";
    const NEBIUS_MODEL: &str = "NousResearch/hermes-4-70b";
    const NEBIUS_URL: &str = "https://api.tokenfactory.nebius.com/v1/chat/completions";

    const OVHCLOUD_PROVIDER: &str = "ovhcloud";
    const OVHCLOUD_MODEL: &str = "mixtral-8x7b-instruct-v0.1";
    const OVHCLOUD_URL: &str = "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1/chat/completions";

    const SCALEWAY_PROVIDER: &str = "scaleway";
    const SCALEWAY_MODEL: &str = "qwen3-235b-a22b-instruct-2507";
    const SCALEWAY_URL: &str = "https://api.scaleway.ai/v1/chat/completions";

    fn run_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        super::wave_b1_smoke::run_openai_case(provider_id, model, url, tag);
    }

    #[test]
    fn b2_modelscope_simple_text() {
        run_case(
            MODELSCOPE_PROVIDER,
            MODELSCOPE_MODEL,
            MODELSCOPE_URL,
            "simple_text",
        );
    }

    #[test]
    fn b2_modelscope_tool_call_single() {
        run_case(
            MODELSCOPE_PROVIDER,
            MODELSCOPE_MODEL,
            MODELSCOPE_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b2_modelscope_tool_call_multiple() {
        run_case(
            MODELSCOPE_PROVIDER,
            MODELSCOPE_MODEL,
            MODELSCOPE_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b2_modelscope_error_auth_401() {
        run_case(
            MODELSCOPE_PROVIDER,
            MODELSCOPE_MODEL,
            MODELSCOPE_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b2_moonshotai_cn_simple_text() {
        run_case(
            MOONSHOT_CN_PROVIDER,
            MOONSHOT_CN_MODEL,
            MOONSHOT_CN_URL,
            "simple_text",
        );
    }

    #[test]
    fn b2_moonshotai_cn_tool_call_single() {
        run_case(
            MOONSHOT_CN_PROVIDER,
            MOONSHOT_CN_MODEL,
            MOONSHOT_CN_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b2_moonshotai_cn_tool_call_multiple() {
        run_case(
            MOONSHOT_CN_PROVIDER,
            MOONSHOT_CN_MODEL,
            MOONSHOT_CN_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b2_moonshotai_cn_error_auth_401() {
        run_case(
            MOONSHOT_CN_PROVIDER,
            MOONSHOT_CN_MODEL,
            MOONSHOT_CN_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b2_nebius_simple_text() {
        run_case(NEBIUS_PROVIDER, NEBIUS_MODEL, NEBIUS_URL, "simple_text");
    }

    #[test]
    fn b2_nebius_tool_call_single() {
        run_case(
            NEBIUS_PROVIDER,
            NEBIUS_MODEL,
            NEBIUS_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b2_nebius_tool_call_multiple() {
        run_case(
            NEBIUS_PROVIDER,
            NEBIUS_MODEL,
            NEBIUS_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b2_nebius_error_auth_401() {
        run_case(NEBIUS_PROVIDER, NEBIUS_MODEL, NEBIUS_URL, "error_auth_401");
    }

    #[test]
    fn b2_ovhcloud_simple_text() {
        run_case(
            OVHCLOUD_PROVIDER,
            OVHCLOUD_MODEL,
            OVHCLOUD_URL,
            "simple_text",
        );
    }

    #[test]
    fn b2_ovhcloud_tool_call_single() {
        run_case(
            OVHCLOUD_PROVIDER,
            OVHCLOUD_MODEL,
            OVHCLOUD_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b2_ovhcloud_tool_call_multiple() {
        run_case(
            OVHCLOUD_PROVIDER,
            OVHCLOUD_MODEL,
            OVHCLOUD_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b2_ovhcloud_error_auth_401() {
        run_case(
            OVHCLOUD_PROVIDER,
            OVHCLOUD_MODEL,
            OVHCLOUD_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b2_scaleway_simple_text() {
        run_case(
            SCALEWAY_PROVIDER,
            SCALEWAY_MODEL,
            SCALEWAY_URL,
            "simple_text",
        );
    }

    #[test]
    fn b2_scaleway_tool_call_single() {
        run_case(
            SCALEWAY_PROVIDER,
            SCALEWAY_MODEL,
            SCALEWAY_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b2_scaleway_tool_call_multiple() {
        run_case(
            SCALEWAY_PROVIDER,
            SCALEWAY_MODEL,
            SCALEWAY_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b2_scaleway_error_auth_401() {
        run_case(
            SCALEWAY_PROVIDER,
            SCALEWAY_MODEL,
            SCALEWAY_URL,
            "error_auth_401",
        );
    }
}

// ============================================================================
// Wave C Special Routing Smoke Tests (opencode + vercel + zenmux)
// ============================================================================

mod wave_c_special_smoke {
    const OPENCODE_PROVIDER: &str = "opencode";
    const OPENCODE_MODEL: &str = "openai/gpt-5";
    const OPENCODE_URL: &str = "https://opencode.ai/zen/v1/chat/completions";

    const VERCEL_PROVIDER: &str = "vercel";
    const VERCEL_MODEL: &str = "openai/gpt-5";
    const VERCEL_URL: &str = "https://ai-gateway.vercel.sh/v1/chat/completions";

    const ZENMUX_PROVIDER: &str = "zenmux";
    const ZENMUX_MODEL: &str = "claude-sonnet-4-5";
    const ZENMUX_URL: &str = "https://zenmux.ai/api/anthropic/v1/messages";

    fn run_openai_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        super::wave_b1_smoke::run_openai_case(provider_id, model, url, tag);
    }

    fn run_anthropic_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        super::wave_b1_smoke::run_anthropic_case(provider_id, model, url, tag);
    }

    #[test]
    fn c_special_opencode_simple_text() {
        run_openai_case(
            OPENCODE_PROVIDER,
            OPENCODE_MODEL,
            OPENCODE_URL,
            "simple_text",
        );
    }

    #[test]
    fn c_special_opencode_tool_call_single() {
        run_openai_case(
            OPENCODE_PROVIDER,
            OPENCODE_MODEL,
            OPENCODE_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn c_special_opencode_tool_call_multiple() {
        run_openai_case(
            OPENCODE_PROVIDER,
            OPENCODE_MODEL,
            OPENCODE_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn c_special_opencode_error_auth_401() {
        run_openai_case(
            OPENCODE_PROVIDER,
            OPENCODE_MODEL,
            OPENCODE_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn c_special_vercel_simple_text() {
        run_openai_case(VERCEL_PROVIDER, VERCEL_MODEL, VERCEL_URL, "simple_text");
    }

    #[test]
    fn c_special_vercel_tool_call_single() {
        run_openai_case(
            VERCEL_PROVIDER,
            VERCEL_MODEL,
            VERCEL_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn c_special_vercel_tool_call_multiple() {
        run_openai_case(
            VERCEL_PROVIDER,
            VERCEL_MODEL,
            VERCEL_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn c_special_vercel_error_auth_401() {
        run_openai_case(VERCEL_PROVIDER, VERCEL_MODEL, VERCEL_URL, "error_auth_401");
    }

    #[test]
    fn c_special_zenmux_simple_text() {
        run_anthropic_case(ZENMUX_PROVIDER, ZENMUX_MODEL, ZENMUX_URL, "simple_text");
    }

    #[test]
    fn c_special_zenmux_tool_call_single() {
        run_anthropic_case(
            ZENMUX_PROVIDER,
            ZENMUX_MODEL,
            ZENMUX_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn c_special_zenmux_tool_call_multiple() {
        run_anthropic_case(
            ZENMUX_PROVIDER,
            ZENMUX_MODEL,
            ZENMUX_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn c_special_zenmux_error_auth_401() {
        run_anthropic_case(ZENMUX_PROVIDER, ZENMUX_MODEL, ZENMUX_URL, "error_auth_401");
    }
}

// ============================================================================
// Wave B3 Representative Smoke Tests (regional + coding-plan presets)
// ============================================================================

mod wave_b3_smoke {
    const SILICONFLOW_PROVIDER: &str = "siliconflow";
    const SILICONFLOW_MODEL: &str = "Qwen/Qwen3-Coder-480B-A35B-Instruct";
    const SILICONFLOW_URL: &str = "https://api.siliconflow.com/v1/chat/completions";

    const SILICONFLOW_CN_PROVIDER: &str = "siliconflow-cn";
    const SILICONFLOW_CN_MODEL: &str = "Qwen/Qwen3-Coder-480B-A35B-Instruct";
    const SILICONFLOW_CN_URL: &str = "https://api.siliconflow.cn/v1/chat/completions";

    const UPSTAGE_PROVIDER: &str = "upstage";
    const UPSTAGE_MODEL: &str = "solar-pro2";
    const UPSTAGE_URL: &str = "https://api.upstage.ai/v1/solar/chat/completions";

    const VENICE_PROVIDER: &str = "venice";
    const VENICE_MODEL: &str = "venice-uncensored";
    const VENICE_URL: &str = "https://api.venice.ai/api/v1/chat/completions";

    const ZAI_PROVIDER: &str = "zai";
    const ZAI_MODEL: &str = "glm-4.5";
    const ZAI_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";

    const ZAI_CODING_PROVIDER: &str = "zai-coding-plan";
    const ZAI_CODING_MODEL: &str = "glm-4.5";
    const ZAI_CODING_URL: &str = "https://api.z.ai/api/coding/paas/v4/chat/completions";

    const ZHIPU_PROVIDER: &str = "zhipuai";
    const ZHIPU_MODEL: &str = "glm-4.5";
    const ZHIPU_URL: &str = "https://open.bigmodel.cn/api/paas/v4/chat/completions";

    const ZHIPU_CODING_PROVIDER: &str = "zhipuai-coding-plan";
    const ZHIPU_CODING_MODEL: &str = "glm-4.5";
    const ZHIPU_CODING_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions";

    fn run_case(provider_id: &str, model: &str, url: &str, tag: &str) {
        super::wave_b1_smoke::run_openai_case(provider_id, model, url, tag);
    }

    #[test]
    fn b3_siliconflow_simple_text() {
        run_case(
            SILICONFLOW_PROVIDER,
            SILICONFLOW_MODEL,
            SILICONFLOW_URL,
            "simple_text",
        );
    }

    #[test]
    fn b3_siliconflow_tool_call_single() {
        run_case(
            SILICONFLOW_PROVIDER,
            SILICONFLOW_MODEL,
            SILICONFLOW_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_siliconflow_tool_call_multiple() {
        run_case(
            SILICONFLOW_PROVIDER,
            SILICONFLOW_MODEL,
            SILICONFLOW_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_siliconflow_error_auth_401() {
        run_case(
            SILICONFLOW_PROVIDER,
            SILICONFLOW_MODEL,
            SILICONFLOW_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b3_siliconflow_cn_simple_text() {
        run_case(
            SILICONFLOW_CN_PROVIDER,
            SILICONFLOW_CN_MODEL,
            SILICONFLOW_CN_URL,
            "simple_text",
        );
    }

    #[test]
    fn b3_siliconflow_cn_tool_call_single() {
        run_case(
            SILICONFLOW_CN_PROVIDER,
            SILICONFLOW_CN_MODEL,
            SILICONFLOW_CN_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_siliconflow_cn_tool_call_multiple() {
        run_case(
            SILICONFLOW_CN_PROVIDER,
            SILICONFLOW_CN_MODEL,
            SILICONFLOW_CN_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_siliconflow_cn_error_auth_401() {
        run_case(
            SILICONFLOW_CN_PROVIDER,
            SILICONFLOW_CN_MODEL,
            SILICONFLOW_CN_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b3_upstage_simple_text() {
        run_case(UPSTAGE_PROVIDER, UPSTAGE_MODEL, UPSTAGE_URL, "simple_text");
    }

    #[test]
    fn b3_upstage_tool_call_single() {
        run_case(
            UPSTAGE_PROVIDER,
            UPSTAGE_MODEL,
            UPSTAGE_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_upstage_tool_call_multiple() {
        run_case(
            UPSTAGE_PROVIDER,
            UPSTAGE_MODEL,
            UPSTAGE_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_upstage_error_auth_401() {
        run_case(
            UPSTAGE_PROVIDER,
            UPSTAGE_MODEL,
            UPSTAGE_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b3_venice_simple_text() {
        run_case(VENICE_PROVIDER, VENICE_MODEL, VENICE_URL, "simple_text");
    }

    #[test]
    fn b3_venice_tool_call_single() {
        run_case(
            VENICE_PROVIDER,
            VENICE_MODEL,
            VENICE_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_venice_tool_call_multiple() {
        run_case(
            VENICE_PROVIDER,
            VENICE_MODEL,
            VENICE_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_venice_error_auth_401() {
        run_case(VENICE_PROVIDER, VENICE_MODEL, VENICE_URL, "error_auth_401");
    }

    #[test]
    fn b3_zai_simple_text() {
        run_case(ZAI_PROVIDER, ZAI_MODEL, ZAI_URL, "simple_text");
    }

    #[test]
    fn b3_zai_tool_call_single() {
        run_case(ZAI_PROVIDER, ZAI_MODEL, ZAI_URL, "tool_call_single");
    }

    #[test]
    fn b3_zai_tool_call_multiple() {
        run_case(ZAI_PROVIDER, ZAI_MODEL, ZAI_URL, "tool_call_multiple");
    }

    #[test]
    fn b3_zai_error_auth_401() {
        run_case(ZAI_PROVIDER, ZAI_MODEL, ZAI_URL, "error_auth_401");
    }

    #[test]
    fn b3_zai_coding_simple_text() {
        run_case(
            ZAI_CODING_PROVIDER,
            ZAI_CODING_MODEL,
            ZAI_CODING_URL,
            "simple_text",
        );
    }

    #[test]
    fn b3_zai_coding_tool_call_single() {
        run_case(
            ZAI_CODING_PROVIDER,
            ZAI_CODING_MODEL,
            ZAI_CODING_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_zai_coding_tool_call_multiple() {
        run_case(
            ZAI_CODING_PROVIDER,
            ZAI_CODING_MODEL,
            ZAI_CODING_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_zai_coding_error_auth_401() {
        run_case(
            ZAI_CODING_PROVIDER,
            ZAI_CODING_MODEL,
            ZAI_CODING_URL,
            "error_auth_401",
        );
    }

    #[test]
    fn b3_zhipuai_simple_text() {
        run_case(ZHIPU_PROVIDER, ZHIPU_MODEL, ZHIPU_URL, "simple_text");
    }

    #[test]
    fn b3_zhipuai_tool_call_single() {
        run_case(ZHIPU_PROVIDER, ZHIPU_MODEL, ZHIPU_URL, "tool_call_single");
    }

    #[test]
    fn b3_zhipuai_tool_call_multiple() {
        run_case(ZHIPU_PROVIDER, ZHIPU_MODEL, ZHIPU_URL, "tool_call_multiple");
    }

    #[test]
    fn b3_zhipuai_error_auth_401() {
        run_case(ZHIPU_PROVIDER, ZHIPU_MODEL, ZHIPU_URL, "error_auth_401");
    }

    #[test]
    fn b3_zhipuai_coding_simple_text() {
        run_case(
            ZHIPU_CODING_PROVIDER,
            ZHIPU_CODING_MODEL,
            ZHIPU_CODING_URL,
            "simple_text",
        );
    }

    #[test]
    fn b3_zhipuai_coding_tool_call_single() {
        run_case(
            ZHIPU_CODING_PROVIDER,
            ZHIPU_CODING_MODEL,
            ZHIPU_CODING_URL,
            "tool_call_single",
        );
    }

    #[test]
    fn b3_zhipuai_coding_tool_call_multiple() {
        run_case(
            ZHIPU_CODING_PROVIDER,
            ZHIPU_CODING_MODEL,
            ZHIPU_CODING_URL,
            "tool_call_multiple",
        );
    }

    #[test]
    fn b3_zhipuai_coding_error_auth_401() {
        run_case(
            ZHIPU_CODING_PROVIDER,
            ZHIPU_CODING_MODEL,
            ZHIPU_CODING_URL,
            "error_auth_401",
        );
    }
}

// ============================================================================
// Azure OpenAI Provider Smoke Tests
// ============================================================================

mod azure_smoke {
    use super::*;
    use pi::providers::azure::AzureOpenAIProvider;

    const TEST_RESOURCE: &str = "test-resource";
    const TEST_DEPLOYMENT: &str = "gpt-4o-mini";
    const TEST_API_VERSION: &str = "2024-12-01-preview";

    fn azure_url() -> String {
        format!(
            "https://{TEST_RESOURCE}.openai.azure.com/openai/deployments/{TEST_DEPLOYMENT}/chat/completions?api-version={TEST_API_VERSION}"
        )
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_azure_{tag}")
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&openai_error_body(e.status)).unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        openai_tool_response_for_scenario(TEST_DEPLOYMENT, scenario)
                    } else if exp.require_unicode {
                        openai_text_response(TEST_DEPLOYMENT, "日本語テスト — émojis: 🦀🔥")
                    } else {
                        openai_text_response(
                            TEST_DEPLOYMENT,
                            "Hello from the verification harness.",
                        )
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: azure_url(),
                        headers: vec![
                            ("api-key".to_string(), "[REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> AzureOpenAIProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        AzureOpenAIProvider::new(TEST_RESOURCE, TEST_DEPLOYMENT).with_client(client)
    }

    #[test]
    fn azure_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_tool_call_multiple() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_multiple")
            .unwrap();
        ensure_fixture("tool_call_multiple", scenario);
        let provider = build_provider("tool_call_multiple");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_tool_call_multiple");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn azure_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_azure_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Cohere Provider Smoke Tests
// ============================================================================

mod cohere_smoke {
    use super::*;
    use pi::providers::cohere::CohereProvider;

    const TEST_MODEL: &str = "command-r-plus";
    const CHAT_URL: &str = "https://api.cohere.com/v2/chat";

    fn cassette_name(tag: &str) -> String {
        format!("verify_cohere_{tag}")
    }

    /// Build Cohere SSE text response body.
    fn cohere_text_sse(text: &str) -> RecordedResponse {
        let msg_start = json!({"type": "message-start", "id": "msg_verify_1"});
        let content_start = json!({
            "type": "content-start",
            "index": 0,
            "delta": {"message": {"content": {"type": "text", "text": ""}}}
        });
        let content_delta = json!({
            "type": "content-delta",
            "index": 0,
            "delta": {"message": {"content": {"text": text}}}
        });
        let content_end = json!({"type": "content-end", "index": 0});
        let msg_end = json!({
            "type": "message-end",
            "delta": {
                "finish_reason": "COMPLETE",
                "usage": {"tokens": {"input_tokens": 15, "output_tokens": 10}}
            }
        });

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                format!("data: {}\n\n", serde_json::to_string(&msg_start).unwrap()),
                format!(
                    "data: {}\n\n",
                    serde_json::to_string(&content_start).unwrap()
                ),
                format!(
                    "data: {}\n\n",
                    serde_json::to_string(&content_delta).unwrap()
                ),
                format!("data: {}\n\n", serde_json::to_string(&content_end).unwrap()),
                format!("data: {}\n\n", serde_json::to_string(&msg_end).unwrap()),
                "data: [DONE]\n\n".to_string(),
            ],
            body_chunks_base64: None,
        }
    }

    /// Build Cohere SSE tool call response body.
    fn cohere_tool_sse(tool_name: &str, tool_args: &Value) -> RecordedResponse {
        let args_str = serde_json::to_string(tool_args).unwrap_or_else(|_| "{}".to_string());
        let msg_start = json!({"type": "message-start", "id": "msg_verify_2"});
        let tool_start = json!({
            "type": "tool-call-start",
            "delta": {
                "message": {
                    "tool_calls": {
                        "id": format!("call_verify_{tool_name}"),
                        "type": "function",
                        "function": {"name": tool_name, "arguments": args_str}
                    }
                }
            }
        });
        let tool_end = json!({"type": "tool-call-end"});
        let msg_end = json!({
            "type": "message-end",
            "delta": {
                "finish_reason": "TOOL_CALL",
                "usage": {"tokens": {"input_tokens": 20, "output_tokens": 8}}
            }
        });

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                format!("data: {}\n\n", serde_json::to_string(&msg_start).unwrap()),
                format!("data: {}\n\n", serde_json::to_string(&tool_start).unwrap()),
                format!("data: {}\n\n", serde_json::to_string(&tool_end).unwrap()),
                format!("data: {}\n\n", serde_json::to_string(&msg_end).unwrap()),
                "data: [DONE]\n\n".to_string(),
            ],
            body_chunks_base64: None,
        }
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(
                            &json!({"message": format!("Simulated error {}", e.status)}),
                        )
                        .unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        cohere_tool_sse(tool_name, &json!({"text": "verification test"}))
                    } else if exp.require_unicode {
                        cohere_text_sse("日本語テスト — émojis: 🦀🔥")
                    } else {
                        cohere_text_sse("Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: CHAT_URL.to_string(),
                        headers: vec![
                            ("Authorization".to_string(), "Bearer [REDACTED]".to_string()),
                            ("Content-Type".to_string(), "application/json".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> CohereProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        CohereProvider::new(TEST_MODEL).with_client(client)
    }

    #[test]
    fn cohere_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn cohere_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn cohere_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn cohere_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn cohere_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn cohere_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_cohere_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Anthropic Provider Smoke Tests
// ============================================================================

mod anthropic_smoke {
    use super::*;
    use pi::providers::anthropic::AnthropicProvider;

    const TEST_MODEL: &str = "claude-sonnet-4-20250514";
    const API_URL: &str = "https://api.anthropic.com/v1/messages";

    fn cassette_name(tag: &str) -> String {
        format!("verify_anthropic_{tag}")
    }

    /// Build an Anthropic SSE text response.
    fn anthropic_text_sse(text: &str) -> RecordedResponse {
        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": "msg_verify_001",
                    "type": "message",
                    "role": "assistant",
                    "model": TEST_MODEL,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 20, "output_tokens": 1}
                }
            }))
            .unwrap()
        );
        let block_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }))
            .unwrap()
        );
        let block_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }))
            .unwrap()
        );
        let block_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 0
            }))
            .unwrap()
        );
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                "usage": {"output_tokens": 10}
            }))
            .unwrap()
        );
        let msg_stop = format!(
            "event: message_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({"type": "message_stop"})).unwrap()
        );

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                msg_start,
                block_start,
                block_delta,
                block_stop,
                msg_delta,
                msg_stop,
            ],
            body_chunks_base64: None,
        }
    }

    /// Build an Anthropic SSE tool call response.
    fn anthropic_tool_sse(tool_name: &str, tool_args: &Value) -> RecordedResponse {
        let args_str = serde_json::to_string(tool_args).unwrap_or_else(|_| "{}".to_string());
        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": "msg_verify_002",
                    "type": "message",
                    "role": "assistant",
                    "model": TEST_MODEL,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 25, "output_tokens": 1}
                }
            }))
            .unwrap()
        );
        let block_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": format!("toolu_verify_{tool_name}"),
                    "name": tool_name,
                    "input": {}
                }
            }))
            .unwrap()
        );
        let block_delta = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": args_str}
            }))
            .unwrap()
        );
        let block_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": 0
            }))
            .unwrap()
        );
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": Value::Null},
                "usage": {"output_tokens": 12}
            }))
            .unwrap()
        );
        let msg_stop = format!(
            "event: message_stop\ndata: {}\n\n",
            serde_json::to_string(&json!({"type": "message_stop"})).unwrap()
        );

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                msg_start,
                block_start,
                block_delta,
                block_stop,
                msg_delta,
                msg_stop,
            ],
            body_chunks_base64: None,
        }
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&json!({
                            "type": "error",
                            "error": {
                                "type": "authentication_error",
                                "message": format!("Simulated error {}", e.status)
                            }
                        }))
                        .unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        anthropic_tool_sse(tool_name, &json!({"text": "verification test"}))
                    } else if exp.require_unicode {
                        anthropic_text_sse("日本語テスト — émojis: 🦀🔥")
                    } else {
                        anthropic_text_sse("Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: API_URL.to_string(),
                        headers: vec![
                            ("Content-Type".to_string(), "application/json".to_string()),
                            ("X-API-Key".to_string(), "[REDACTED]".to_string()),
                            ("anthropic-version".to_string(), "2023-06-01".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> AnthropicProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        AnthropicProvider::new(TEST_MODEL).with_client(client)
    }

    #[test]
    fn anthropic_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn anthropic_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn anthropic_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn anthropic_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn anthropic_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn anthropic_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_anthropic_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Gemini Provider Smoke Tests
// ============================================================================

mod gemini_smoke {
    use super::*;
    use pi::providers::gemini::GeminiProvider;

    const TEST_MODEL: &str = "gemini-1.5-flash";

    fn gemini_url() -> String {
        format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{TEST_MODEL}:streamGenerateContent?alt=sse"
        )
    }

    fn cassette_name(tag: &str) -> String {
        format!("verify_gemini_{tag}")
    }

    /// Build a Gemini SSE text response.
    fn gemini_text_sse(text: &str) -> RecordedResponse {
        let chunk = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": text}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 15,
                "candidatesTokenCount": 10,
                "totalTokenCount": 25
            }
        });

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![format!(
                "data: {}\n\n",
                serde_json::to_string(&chunk).unwrap()
            )],
            body_chunks_base64: None,
        }
    }

    /// Build a Gemini SSE tool call response.
    fn gemini_tool_sse(tool_name: &str, tool_args: &Value) -> RecordedResponse {
        let chunk = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": tool_name, "args": tool_args}}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 20,
                "candidatesTokenCount": 8,
                "totalTokenCount": 28
            }
        });

        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![format!(
                "data: {}\n\n",
                serde_json::to_string(&chunk).unwrap()
            )],
            body_chunks_base64: None,
        }
    }

    fn ensure_fixture(tag: &str, scenario: &CanonicalScenario) -> PathBuf {
        let dir = cassette_root();
        let name = cassette_name(tag);
        let path = dir.join(format!("{name}.json"));

        if should_generate_fixture(&path, &name) {
            let response = match &scenario.expectation {
                CanonicalExpectation::Error(e) => RecordedResponse {
                    status: e.status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body_chunks: vec![
                        serde_json::to_string(&json!({
                            "error": {
                                "code": e.status,
                                "message": format!("Simulated error {}", e.status),
                                "status": "UNAUTHENTICATED"
                            }
                        }))
                        .unwrap_or_default(),
                    ],
                    body_chunks_base64: None,
                },
                CanonicalExpectation::Stream(exp) => {
                    if exp.min_tool_calls > 0 {
                        let tool_name = scenario.tools.first().map_or("echo", |t| t.name.as_str());
                        gemini_tool_sse(tool_name, &json!({"text": "verification test"}))
                    } else if exp.require_unicode {
                        gemini_text_sse("日本語テスト — émojis: 🦀🔥")
                    } else {
                        gemini_text_sse("Hello from the verification harness.")
                    }
                }
            };

            let cassette = Cassette {
                version: "1.0".to_string(),
                test_name: name,
                recorded_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                interactions: vec![Interaction {
                    request: RecordedRequest {
                        method: "POST".to_string(),
                        url: gemini_url(),
                        headers: vec![
                            ("Content-Type".to_string(), "application/json".to_string()),
                            ("Accept".to_string(), "text/event-stream".to_string()),
                        ],
                        body: None,
                        body_text: None,
                    },
                    response,
                }],
            };
            write_cassette(&path, &cassette);
        }

        path
    }

    fn build_provider(tag: &str) -> GeminiProvider {
        let cassette_dir = cassette_root();
        let name = cassette_name(tag);
        let recorder = VcrRecorder::new_with(&name, vcr_mode(), &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        GeminiProvider::new(TEST_MODEL).with_client(client)
    }

    #[test]
    fn gemini_simple_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "simple_text").unwrap();
        ensure_fixture("simple_text", scenario);
        let provider = build_provider("simple_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_simple_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gemini_unicode_text() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios.iter().find(|s| s.tag == "unicode_text").unwrap();
        ensure_fixture("unicode_text", scenario);
        let provider = build_provider("unicode_text");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_unicode_text");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gemini_tool_call_single() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "tool_call_single")
            .unwrap();
        ensure_fixture("tool_call_single", scenario);
        let provider = build_provider("tool_call_single");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_tool_call_single");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gemini_error_auth_401() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_auth_401")
            .unwrap();
        ensure_fixture("error_auth_401", scenario);
        let provider = build_provider("error_auth_401");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_error_auth_401");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gemini_error_bad_request_400() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_bad_request_400")
            .unwrap();
        ensure_fixture("error_bad_request_400", scenario);
        let provider = build_provider("error_bad_request_400");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_error_bad_request_400");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }

    #[test]
    fn gemini_error_rate_limit_429() {
        let scenarios = canonical_scenarios();
        let scenario = scenarios
            .iter()
            .find(|s| s.tag == "error_rate_limit_429")
            .unwrap();
        ensure_fixture("error_rate_limit_429", scenario);
        let provider = build_provider("error_rate_limit_429");

        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let harness = TestHarness::new("verify_gemini_error_rate_limit_429");
                run_canonical_scenario(&provider, scenario, &harness).await;
            });
    }
}

// ============================================================================
// Gap Provider Conformance Tests (bd-3uqg.11.11.3)
//
// Canonical-scenario conformance for the 5 gap providers:
//   groq, cerebras, openrouter, moonshotai (kimi), alibaba (qwen/dashscope)
//
// All are OpenAI-compatible presets. Each module runs the full 7-scenario
// canonical set via VCR fixture playback.
// ============================================================================

mod groq_conformance {
    const PROVIDER: &str = "groq";
    const MODEL: &str = "llama-3.3-70b-versatile";
    const URL: &str = "https://api.groq.com/openai/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }

    #[test]
    fn unicode_text() {
        run("unicode_text");
    }

    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }

    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }

    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }

    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }

    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod cerebras_conformance {
    const PROVIDER: &str = "cerebras";
    const MODEL: &str = "llama-3.3-70b";
    const URL: &str = "https://api.cerebras.ai/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }

    #[test]
    fn unicode_text() {
        run("unicode_text");
    }

    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }

    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }

    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }

    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }

    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod openrouter_conformance {
    const PROVIDER: &str = "openrouter";
    const MODEL: &str = "anthropic/claude-3.5-sonnet";
    const URL: &str = "https://openrouter.ai/api/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }

    #[test]
    fn unicode_text() {
        run("unicode_text");
    }

    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }

    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }

    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }

    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }

    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod moonshotai_conformance {
    const PROVIDER: &str = "moonshotai";
    const MODEL: &str = "moonshot-v1-8k";
    const URL: &str = "https://api.moonshot.cn/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }

    #[test]
    fn unicode_text() {
        run("unicode_text");
    }

    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }

    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }

    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }

    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }

    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod alibaba_conformance {
    const PROVIDER: &str = "alibaba";
    const MODEL: &str = "qwen-plus";
    const URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }

    #[test]
    fn unicode_text() {
        run("unicode_text");
    }

    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }

    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }

    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }

    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }

    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// LONGTAIL PROVIDER CONFORMANCE (bd-3uqg.11.10.5)
//
// VCR-based conformance tests for representative longtail quick-win providers.
// ═══════════════════════════════════════════════════════════════════════

mod stackit_conformance {
    const PROVIDER: &str = "stackit";
    const MODEL: &str = "stackit-test-model";
    const URL: &str =
        "https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod mistral_conformance {
    const PROVIDER: &str = "mistral";
    const MODEL: &str = "mistral-large-latest";
    const URL: &str = "https://api.mistral.ai/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod deepinfra_conformance {
    const PROVIDER: &str = "deepinfra";
    const MODEL: &str = "meta-llama/Meta-Llama-3.1-70B-Instruct";
    const URL: &str = "https://api.deepinfra.com/v1/openai/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod togetherai_conformance {
    const PROVIDER: &str = "togetherai";
    const MODEL: &str = "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo";
    const URL: &str = "https://api.together.xyz/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod nvidia_conformance {
    const PROVIDER: &str = "nvidia";
    const MODEL: &str = "meta/llama-3.1-70b-instruct";
    const URL: &str = "https://integrate.api.nvidia.com/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod huggingface_conformance {
    const PROVIDER: &str = "huggingface";
    const MODEL: &str = "meta-llama/Meta-Llama-3.1-70B-Instruct";
    const URL: &str = "https://router.huggingface.co/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}

mod ollama_cloud_conformance {
    const PROVIDER: &str = "ollama-cloud";
    const MODEL: &str = "llama3.1:70b";
    const URL: &str = "https://ollama.com/v1/chat/completions";

    fn run(tag: &str) {
        super::wave_b1_smoke::run_openai_case(PROVIDER, MODEL, URL, tag);
    }

    #[test]
    fn simple_text() {
        run("simple_text");
    }
    #[test]
    fn unicode_text() {
        run("unicode_text");
    }
    #[test]
    fn tool_call_single() {
        run("tool_call_single");
    }
    #[test]
    fn tool_call_multiple() {
        run("tool_call_multiple");
    }
    #[test]
    fn error_auth_401() {
        run("error_auth_401");
    }
    #[test]
    fn error_bad_request_400() {
        run("error_bad_request_400");
    }
    #[test]
    fn error_rate_limit_429() {
        run("error_rate_limit_429");
    }
}
