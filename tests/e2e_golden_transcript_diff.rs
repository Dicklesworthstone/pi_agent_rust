//! Golden transcript capture + cross-provider diff tooling (bd-3uqg.8.9).
//!
//! Captures normalized event streams from multiple provider families using
//! identical prompts, diffs them across providers, and produces JSONL/Markdown
//! reports for drift detection.
//!
//! # Approach
//!
//! 1. Each provider family has a "golden transcript" — a normalized event
//!    sequence captured from a deterministic mock response.
//! 2. Cross-provider diffs compare text extraction, tool-call shape, stop
//!    reasons, and event sequence structure.
//! 3. All output is JSONL for machine consumption + human-readable summaries.
//!
//! Run:
//! ```bash
//! cargo test --test e2e_golden_transcript_diff
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::similar_names)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::type_complexity)]
#![allow(clippy::redundant_closure)]

mod common;

use common::{MockHttpResponse, MockHttpServer, TestHarness};
use futures::StreamExt;
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamOptions, ToolDef};
use pi::providers::create_provider;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════
// Core types
// ═══════════════════════════════════════════════════════════════════════

/// Schema for golden transcript JSONL artifacts.
const TRANSCRIPT_SCHEMA: &str = "pi.golden_transcript.v1";

/// A single normalized event in the golden transcript.
#[derive(Debug, Clone, Serialize)]
struct NormalizedEvent {
    /// Event kind (Start, TextDelta, TextEnd, ToolCallEnd, Done, Error, etc.)
    kind: String,
    /// Content index for block events.
    content_index: Option<usize>,
    /// Text content (for TextDelta/TextEnd).
    text: Option<String>,
    /// Tool call details (for ToolCallEnd).
    tool_name: Option<String>,
    tool_arguments: Option<String>,
    /// Stop reason (for Done/Error).
    stop_reason: Option<String>,
    /// Error message (for Error events).
    error_message: Option<String>,
}

/// Complete golden transcript for one provider.
#[derive(Debug, Clone, Serialize)]
struct GoldenTranscript {
    /// Provider family name.
    family: String,
    /// Provider ID.
    provider: String,
    /// Scenario name (e.g., "text", "tool_call").
    scenario: String,
    /// Normalized event sequence.
    events: Vec<NormalizedEvent>,
    /// Extracted final text content.
    final_text: String,
    /// Number of tool calls.
    tool_call_count: usize,
    /// Stop reason string.
    stop_reason: Option<String>,
    /// Whether the event sequence passed structural validation.
    sequence_valid: bool,
    /// Validation error, if any.
    sequence_error: Option<String>,
    /// Total event count.
    event_count: usize,
}

/// A difference found between two provider transcripts.
#[derive(Debug, Clone, Serialize)]
struct TranscriptDiff {
    /// Field that differs.
    field: String,
    /// Baseline provider family.
    baseline_family: String,
    /// Baseline value.
    baseline_value: String,
    /// Comparison provider family.
    compare_family: String,
    /// Comparison value.
    compare_value: String,
    /// Severity: "structural" (event shape), "semantic" (content), "cosmetic" (non-critical).
    severity: String,
}

/// Cross-provider diff report.
#[derive(Debug, Clone, Serialize)]
struct DiffReport {
    schema: String,
    scenario: String,
    baseline_family: String,
    families_compared: Vec<String>,
    diffs: Vec<TranscriptDiff>,
    all_match: bool,
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

fn make_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} golden model"),
            api: String::new(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                audio: None,
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 8192,
            max_tokens: 4096,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn simple_context() -> Context<'static> {
    Context::owned(
        Some("You are a helpful assistant.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Say hello world.".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn tool_context() -> Context<'static> {
    Context::owned(
        Some("You are a helpful assistant.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Call the echo function with text hello.".to_string()),
            timestamp: 0,
        })],
        vec![ToolDef {
            name: "echo".to_string(),
            description: "Echo text back".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "text to echo"}
                },
                "required": ["text"],
            }),
        }],
    )
}

fn default_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("golden-test-key".to_string()),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn make_sse_response(body: &str) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.as_bytes().to_vec(),
    }
}

fn collect_events(
    provider: Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) -> Result<Vec<StreamEvent>, String> {
    common::run_async(async move {
        let stream = provider
            .stream(&context, &options)
            .await
            .map_err(|e| e.to_string())?;
        let mut pinned = std::pin::pin!(stream);
        let mut events = Vec::new();
        while let Some(item) = pinned.next().await {
            let event = item.map_err(|e| e.to_string())?;
            let terminal = matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        Ok(events)
    })
}

// ═══════════════════════════════════════════════════════════════════════
// Normalization
// ═══════════════════════════════════════════════════════════════════════

fn event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Start { .. } => "Start",
        StreamEvent::TextStart { .. } => "TextStart",
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::TextEnd { .. } => "TextEnd",
        StreamEvent::ThinkingStart { .. } => "ThinkingStart",
        StreamEvent::ThinkingDelta { .. } => "ThinkingDelta",
        StreamEvent::ThinkingEnd { .. } => "ThinkingEnd",
        StreamEvent::ToolCallStart { .. } => "ToolCallStart",
        StreamEvent::ToolCallDelta { .. } => "ToolCallDelta",
        StreamEvent::ToolCallEnd { .. } => "ToolCallEnd",
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
    }
}

fn normalize_event(event: &StreamEvent) -> NormalizedEvent {
    match event {
        StreamEvent::Start { .. } => NormalizedEvent {
            kind: "Start".to_string(),
            content_index: None,
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::TextDelta {
            content_index,
            delta,
            ..
        } => NormalizedEvent {
            kind: "TextDelta".to_string(),
            content_index: Some(*content_index),
            text: Some(delta.clone()),
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::TextEnd {
            content_index,
            content,
            ..
        } => NormalizedEvent {
            kind: "TextEnd".to_string(),
            content_index: Some(*content_index),
            text: Some(content.clone()),
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::TextStart { content_index, .. } => NormalizedEvent {
            kind: "TextStart".to_string(),
            content_index: Some(*content_index),
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::ToolCallStart { content_index, .. } => NormalizedEvent {
            kind: "ToolCallStart".to_string(),
            content_index: Some(*content_index),
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => NormalizedEvent {
            kind: "ToolCallDelta".to_string(),
            content_index: Some(*content_index),
            text: Some(delta.clone()),
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => NormalizedEvent {
            kind: "ToolCallEnd".to_string(),
            content_index: Some(*content_index),
            text: None,
            tool_name: Some(tool_call.name.clone()),
            tool_arguments: Some(tool_call.arguments.to_string()),
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::Done { reason, .. } => NormalizedEvent {
            kind: "Done".to_string(),
            content_index: None,
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: Some(format!("{reason:?}")),
            error_message: None,
        },
        StreamEvent::Error { reason, error, .. } => NormalizedEvent {
            kind: "Error".to_string(),
            content_index: None,
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: Some(format!("{reason:?}")),
            error_message: error.error_message.clone(),
        },
        StreamEvent::ThinkingStart { content_index, .. } => NormalizedEvent {
            kind: "ThinkingStart".to_string(),
            content_index: Some(*content_index),
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        } => NormalizedEvent {
            kind: "ThinkingDelta".to_string(),
            content_index: Some(*content_index),
            text: Some(delta.clone()),
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
        StreamEvent::ThinkingEnd {
            content_index,
            content,
            ..
        } => NormalizedEvent {
            kind: "ThinkingEnd".to_string(),
            content_index: Some(*content_index),
            text: Some(content.clone()),
            tool_name: None,
            tool_arguments: None,
            stop_reason: None,
            error_message: None,
        },
    }
}

fn validate_sequence(events: &[StreamEvent]) -> Result<(), String> {
    if events.is_empty() {
        return Err("no events emitted".to_string());
    }
    if !matches!(events.first(), Some(StreamEvent::Start { .. })) {
        return Err("first event must be Start".to_string());
    }
    if !matches!(
        events.last(),
        Some(StreamEvent::Done { .. } | StreamEvent::Error { .. })
    ) {
        return Err("last event must be Done or Error".to_string());
    }
    Ok(())
}

fn build_transcript(
    family: &str,
    provider: &str,
    scenario: &str,
    result: &Result<Vec<StreamEvent>, String>,
) -> GoldenTranscript {
    match result {
        Ok(events) => {
            let normalized: Vec<NormalizedEvent> = events.iter().map(normalize_event).collect();
            let final_text = extract_final_text(events);
            let tool_call_count = events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolCallEnd { .. }))
                .count();
            let stop_reason = events.iter().find_map(|e| match e {
                StreamEvent::Done { reason, .. } | StreamEvent::Error { reason, .. } => {
                    Some(format!("{reason:?}"))
                }
                _ => None,
            });
            let validation = validate_sequence(events);
            GoldenTranscript {
                family: family.to_string(),
                provider: provider.to_string(),
                scenario: scenario.to_string(),
                event_count: normalized.len(),
                events: normalized,
                final_text,
                tool_call_count,
                stop_reason,
                sequence_valid: validation.is_ok(),
                sequence_error: validation.err(),
            }
        }
        Err(e) => GoldenTranscript {
            family: family.to_string(),
            provider: provider.to_string(),
            scenario: scenario.to_string(),
            events: Vec::new(),
            final_text: String::new(),
            tool_call_count: 0,
            stop_reason: None,
            sequence_valid: false,
            sequence_error: Some(e.clone()),
            event_count: 0,
        },
    }
}

fn extract_final_text(events: &[StreamEvent]) -> String {
    // Prefer TextEnd content; fall back to concatenating TextDelta.
    let from_end: Option<String> = events.iter().find_map(|e| match e {
        StreamEvent::TextEnd { content, .. } => Some(content.clone()),
        _ => None,
    });
    if let Some(text) = from_end {
        return text;
    }
    let mut buf = String::new();
    for e in events {
        if let StreamEvent::TextDelta { delta, .. } = e {
            buf.push_str(delta);
        }
    }
    buf
}

// ═══════════════════════════════════════════════════════════════════════
// Diff engine
// ═══════════════════════════════════════════════════════════════════════

fn diff_transcripts(
    baseline: &GoldenTranscript,
    compare: &GoldenTranscript,
) -> Vec<TranscriptDiff> {
    let mut diffs = Vec::new();

    // 1. Final text content
    if baseline.final_text != compare.final_text {
        diffs.push(TranscriptDiff {
            field: "final_text".to_string(),
            baseline_family: baseline.family.clone(),
            baseline_value: baseline.final_text.clone(),
            compare_family: compare.family.clone(),
            compare_value: compare.final_text.clone(),
            severity: "semantic".to_string(),
        });
    }

    // 2. Tool call count
    if baseline.tool_call_count != compare.tool_call_count {
        diffs.push(TranscriptDiff {
            field: "tool_call_count".to_string(),
            baseline_family: baseline.family.clone(),
            baseline_value: baseline.tool_call_count.to_string(),
            compare_family: compare.family.clone(),
            compare_value: compare.tool_call_count.to_string(),
            severity: "structural".to_string(),
        });
    }

    // 3. Stop reason
    if baseline.stop_reason != compare.stop_reason {
        diffs.push(TranscriptDiff {
            field: "stop_reason".to_string(),
            baseline_family: baseline.family.clone(),
            baseline_value: baseline.stop_reason.clone().unwrap_or_default(),
            compare_family: compare.family.clone(),
            compare_value: compare.stop_reason.clone().unwrap_or_default(),
            severity: "structural".to_string(),
        });
    }

    // 4. Event kind sequence (abstract shape)
    let baseline_kinds: Vec<&str> = baseline.events.iter().map(|e| e.kind.as_str()).collect();
    let compare_kinds: Vec<&str> = compare.events.iter().map(|e| e.kind.as_str()).collect();
    let baseline_shape = abstract_event_shape(&baseline_kinds);
    let compare_shape = abstract_event_shape(&compare_kinds);
    if baseline_shape != compare_shape {
        diffs.push(TranscriptDiff {
            field: "event_shape".to_string(),
            baseline_family: baseline.family.clone(),
            baseline_value: baseline_shape,
            compare_family: compare.family.clone(),
            compare_value: compare_shape,
            severity: "structural".to_string(),
        });
    }

    // 5. Sequence validity
    if baseline.sequence_valid != compare.sequence_valid {
        diffs.push(TranscriptDiff {
            field: "sequence_valid".to_string(),
            baseline_family: baseline.family.clone(),
            baseline_value: baseline.sequence_valid.to_string(),
            compare_family: compare.family.clone(),
            compare_value: compare.sequence_valid.to_string(),
            severity: "structural".to_string(),
        });
    }

    // 6. Tool call names (if both have tool calls)
    if baseline.tool_call_count > 0 && compare.tool_call_count > 0 {
        let baseline_names: Vec<&str> = baseline
            .events
            .iter()
            .filter_map(|e| e.tool_name.as_deref())
            .collect();
        let compare_names: Vec<&str> = compare
            .events
            .iter()
            .filter_map(|e| e.tool_name.as_deref())
            .collect();
        if baseline_names != compare_names {
            diffs.push(TranscriptDiff {
                field: "tool_names".to_string(),
                baseline_family: baseline.family.clone(),
                baseline_value: format!("{baseline_names:?}"),
                compare_family: compare.family.clone(),
                compare_value: format!("{compare_names:?}"),
                severity: "semantic".to_string(),
            });
        }
    }

    diffs
}

/// Collapse event kinds into an abstract shape, merging consecutive deltas.
fn abstract_event_shape(kinds: &[&str]) -> String {
    let mut shape = Vec::new();
    let mut last = "";
    for kind in kinds {
        if *kind == last && kind.contains("Delta") {
            // Merge consecutive deltas
            continue;
        }
        shape.push(*kind);
        last = kind;
    }
    shape.join(" → ")
}

fn build_diff_report(
    scenario: &str,
    baseline: &GoldenTranscript,
    others: &[GoldenTranscript],
) -> DiffReport {
    let mut all_diffs = Vec::new();
    let mut families = Vec::new();
    for other in others {
        families.push(other.family.clone());
        all_diffs.extend(diff_transcripts(baseline, other));
    }
    DiffReport {
        schema: TRANSCRIPT_SCHEMA.to_string(),
        scenario: scenario.to_string(),
        baseline_family: baseline.family.clone(),
        families_compared: families,
        all_match: all_diffs.is_empty(),
        diffs: all_diffs,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Output
// ═══════════════════════════════════════════════════════════════════════

fn write_transcript_jsonl(harness: &TestHarness, name: &str, transcripts: &[GoldenTranscript]) {
    let mut buf = String::new();
    for t in transcripts {
        let _ = writeln!(buf, "{}", serde_json::to_string(t).unwrap_or_default());
    }
    let path = harness.temp_path(format!("{name}_transcripts.jsonl"));
    std::fs::write(&path, &buf).expect("write transcript JSONL");
    harness.record_artifact(format!("{name}_transcripts.jsonl"), &path);
}

fn write_diff_report_jsonl(harness: &TestHarness, name: &str, report: &DiffReport) {
    let content = serde_json::to_string_pretty(report).unwrap_or_default();
    let path = harness.temp_path(format!("{name}_diff.json"));
    std::fs::write(&path, &content).expect("write diff report");
    harness.record_artifact(format!("{name}_diff.json"), &path);
}

fn write_markdown_summary(harness: &TestHarness, name: &str, report: &DiffReport) {
    let mut md = String::new();
    let _ = writeln!(md, "# Golden Transcript Diff: {}", report.scenario);
    let _ = writeln!(md, "\nBaseline: **{}**", report.baseline_family);
    let _ = writeln!(md, "Compared: {}", report.families_compared.join(", "));
    let _ = writeln!(
        md,
        "Result: **{}**\n",
        if report.all_match {
            "ALL MATCH"
        } else {
            "DIFFS FOUND"
        }
    );

    if !report.diffs.is_empty() {
        let _ = writeln!(md, "| Field | Baseline | Compare | Severity |");
        let _ = writeln!(md, "|-------|----------|---------|----------|");
        for d in &report.diffs {
            let _ = writeln!(
                md,
                "| {} | {}={} | {}={} | {} |",
                d.field,
                d.baseline_family,
                truncate_for_table(&d.baseline_value, 30),
                d.compare_family,
                truncate_for_table(&d.compare_value, 30),
                d.severity,
            );
        }
    }

    let path = harness.temp_path(format!("{name}_diff.md"));
    std::fs::write(&path, &md).expect("write diff markdown");
    harness.record_artifact(format!("{name}_diff.md"), &path);
}

fn truncate_for_table(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a char boundary at or before `max` to avoid panicking on
        // multi-byte UTF-8 sequences.
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SSE fixtures (reused from e2e_provider_scenarios patterns)
// ═══════════════════════════════════════════════════════════════════════

fn openai_responses_text_sse() -> String {
    [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"Hello"}"#,
        "",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":" world!"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_text_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_001","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello world!"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

fn gemini_text_sse() -> String {
    [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello world!"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        "",
    ]
    .join("\n")
}

fn openai_completions_text_sse() -> String {
    [
        r#"data: {"id":"oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"content":" world!"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn openai_responses_tool_sse() -> String {
    [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_001","name":"echo","arguments":""}}"#,
        "",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"text\":\"hello\"}"}"#,
        "",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_001","name":"echo","arguments":"{\"text\":\"hello\"}","status":"completed"}}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":15,"output_tokens":12,"total_tokens":27}}}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_tool_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":15,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_001","name":"echo","input":{}}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"hello\"}"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

fn gemini_tool_sse() -> String {
    let chunk = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "echo", "args": {"text": "hello"}}}]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 12,
            "totalTokenCount": 27
        }
    });
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

// ═══════════════════════════════════════════════════════════════════════
// Provider setup helpers
// ═══════════════════════════════════════════════════════════════════════

fn setup_openai_responses(
    harness: &TestHarness,
    sse: &str,
) -> (Arc<dyn pi::provider::Provider>, MockHttpServer) {
    let server = harness.start_mock_http_server();
    server.add_route("POST", "/v1/responses", make_sse_response(sse));
    let base_url = format!("{}/v1", server.base_url());
    let mut entry = make_entry("openai", "golden-gpt", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create openai provider");
    (provider, server)
}

fn setup_openai_completions(
    harness: &TestHarness,
    sse: &str,
) -> (Arc<dyn pi::provider::Provider>, MockHttpServer) {
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/openai/v1/chat/completions",
        make_sse_response(sse),
    );
    let base_url = format!("{}/openai/v1", server.base_url());
    let mut entry = make_entry("groq", "golden-llama", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create openai-compat provider");
    (provider, server)
}

fn setup_anthropic(
    harness: &TestHarness,
    sse: &str,
) -> (Arc<dyn pi::provider::Provider>, MockHttpServer) {
    let server = harness.start_mock_http_server();
    server.add_route("POST", "/v1/messages", make_sse_response(sse));
    let base_url = format!("{}/v1/messages", server.base_url());
    let mut entry = make_entry("anthropic", "golden-claude", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create anthropic provider");
    (provider, server)
}

fn setup_gemini(
    harness: &TestHarness,
    sse: &str,
) -> (Arc<dyn pi::provider::Provider>, MockHttpServer) {
    let server = harness.start_mock_http_server();
    let route = "/v1beta/models/golden-gemini:streamGenerateContent?alt=sse";
    server.add_route("POST", route, make_sse_response(sse));
    let base_url = format!("{}/v1beta", server.base_url());
    let mut entry = make_entry("google", "golden-gemini", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create gemini provider");
    (provider, server)
}

// ═══════════════════════════════════════════════════════════════════════
// Section 1: Golden transcript capture for text responses
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn capture_text_golden_transcripts() {
    let harness = TestHarness::new("capture_text_golden_transcripts");
    let ctx = simple_context();
    let opts = default_options();
    let mut transcripts = Vec::new();

    // OpenAI Responses API
    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "openai-responses",
        "openai",
        "text",
        &result,
    ));

    // Anthropic
    let (provider, _server) = setup_anthropic(&harness, &anthropic_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "anthropic-messages",
        "anthropic",
        "text",
        &result,
    ));

    // Gemini
    let (provider, _server) = setup_gemini(&harness, &gemini_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "gemini-generative",
        "google",
        "text",
        &result,
    ));

    // OpenAI Completions (via groq)
    let (provider, _server) = setup_openai_completions(&harness, &openai_completions_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "openai-completions",
        "groq",
        "text",
        &result,
    ));

    // All should produce valid transcripts
    for t in &transcripts {
        assert!(
            t.sequence_valid,
            "{} transcript should be valid: {:?}",
            t.family, t.sequence_error
        );
    }

    write_transcript_jsonl(&harness, "text", &transcripts);
}

#[test]
fn text_transcripts_all_extract_hello_world() {
    let harness = TestHarness::new("text_transcripts_all_extract_hello_world");
    let ctx = simple_context();
    let opts = default_options();

    let families: Vec<(
        &str,
        &str,
        String,
        Box<dyn Fn(&TestHarness, &str) -> (Arc<dyn pi::provider::Provider>, MockHttpServer)>,
    )> = vec![
        (
            "openai-responses",
            "openai",
            openai_responses_text_sse(),
            Box::new(|h, s| setup_openai_responses(h, s)),
        ),
        (
            "anthropic-messages",
            "anthropic",
            anthropic_text_sse(),
            Box::new(|h, s| setup_anthropic(h, s)),
        ),
        (
            "gemini-generative",
            "google",
            gemini_text_sse(),
            Box::new(|h, s| setup_gemini(h, s)),
        ),
        (
            "openai-completions",
            "groq",
            openai_completions_text_sse(),
            Box::new(|h, s| setup_openai_completions(h, s)),
        ),
    ];

    for (family, provider, sse, setup) in &families {
        let (prov, _server) = setup(&harness, sse);
        let result = collect_events(prov, ctx.clone(), opts.clone());
        let transcript = build_transcript(family, provider, "text", &result);
        assert!(
            transcript.final_text.contains("Hello") && transcript.final_text.contains("world"),
            "{family}: expected 'Hello world' but got '{}'",
            transcript.final_text
        );
    }
}

#[test]
fn text_transcripts_all_have_stop_reason() {
    let harness = TestHarness::new("text_transcripts_all_have_stop_reason");
    let ctx = simple_context();
    let opts = default_options();

    let setups: Vec<(
        &str,
        String,
        Box<dyn Fn(&TestHarness, &str) -> (Arc<dyn pi::provider::Provider>, MockHttpServer)>,
    )> = vec![
        (
            "openai-responses",
            openai_responses_text_sse(),
            Box::new(|h, s| setup_openai_responses(h, s)),
        ),
        (
            "anthropic-messages",
            anthropic_text_sse(),
            Box::new(|h, s| setup_anthropic(h, s)),
        ),
        (
            "gemini-generative",
            gemini_text_sse(),
            Box::new(|h, s| setup_gemini(h, s)),
        ),
        (
            "openai-completions",
            openai_completions_text_sse(),
            Box::new(|h, s| setup_openai_completions(h, s)),
        ),
    ];

    for (family, sse, setup) in &setups {
        let (provider, _server) = setup(&harness, sse);
        let result = collect_events(provider, ctx.clone(), opts.clone());
        let transcript = build_transcript(family, family, "text", &result);
        assert!(
            transcript.stop_reason.is_some(),
            "{family}: must have a stop reason"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Section 2: Golden transcript capture for tool-call responses
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn capture_tool_golden_transcripts() {
    let harness = TestHarness::new("capture_tool_golden_transcripts");
    let ctx = tool_context();
    let opts = default_options();
    let mut transcripts = Vec::new();

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "openai-responses",
        "openai",
        "tool",
        &result,
    ));

    let (provider, _server) = setup_anthropic(&harness, &anthropic_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "anthropic-messages",
        "anthropic",
        "tool",
        &result,
    ));

    let (provider, _server) = setup_gemini(&harness, &gemini_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    transcripts.push(build_transcript(
        "gemini-generative",
        "google",
        "tool",
        &result,
    ));

    for t in &transcripts {
        assert!(
            t.sequence_valid,
            "{} tool transcript should be valid: {:?}",
            t.family, t.sequence_error
        );
        assert!(
            t.tool_call_count >= 1,
            "{}: expected at least 1 tool call, got {}",
            t.family,
            t.tool_call_count
        );
    }

    write_transcript_jsonl(&harness, "tool", &transcripts);
}

#[test]
fn tool_transcripts_all_call_echo() {
    let harness = TestHarness::new("tool_transcripts_all_call_echo");
    let ctx = tool_context();
    let opts = default_options();

    let setups: Vec<(
        &str,
        String,
        Box<dyn Fn(&TestHarness, &str) -> (Arc<dyn pi::provider::Provider>, MockHttpServer)>,
    )> = vec![
        (
            "openai-responses",
            openai_responses_tool_sse(),
            Box::new(|h, s| setup_openai_responses(h, s)),
        ),
        (
            "anthropic-messages",
            anthropic_tool_sse(),
            Box::new(|h, s| setup_anthropic(h, s)),
        ),
        (
            "gemini-generative",
            gemini_tool_sse(),
            Box::new(|h, s| setup_gemini(h, s)),
        ),
    ];

    for (family, sse, setup) in &setups {
        let (provider, _server) = setup(&harness, sse);
        let result = collect_events(provider, ctx.clone(), opts.clone());
        let transcript = build_transcript(family, family, "tool", &result);
        let has_echo = transcript
            .events
            .iter()
            .any(|e| e.tool_name.as_deref() == Some("echo"));
        assert!(has_echo, "{family}: tool transcript must call 'echo'");
    }
}

#[test]
fn tool_transcripts_have_valid_arguments() {
    let harness = TestHarness::new("tool_transcripts_have_valid_arguments");
    let ctx = tool_context();
    let opts = default_options();

    let setups: Vec<(
        &str,
        String,
        Box<dyn Fn(&TestHarness, &str) -> (Arc<dyn pi::provider::Provider>, MockHttpServer)>,
    )> = vec![
        (
            "openai-responses",
            openai_responses_tool_sse(),
            Box::new(|h, s| setup_openai_responses(h, s)),
        ),
        (
            "anthropic-messages",
            anthropic_tool_sse(),
            Box::new(|h, s| setup_anthropic(h, s)),
        ),
        (
            "gemini-generative",
            gemini_tool_sse(),
            Box::new(|h, s| setup_gemini(h, s)),
        ),
    ];

    for (family, sse, setup) in &setups {
        let (provider, _server) = setup(&harness, sse);
        let result = collect_events(provider, ctx.clone(), opts.clone());
        let transcript = build_transcript(family, family, "tool", &result);
        for event in &transcript.events {
            if let Some(args) = &event.tool_arguments {
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(args);
                assert!(
                    parsed.is_ok(),
                    "{family}: tool arguments must be valid JSON, got: {args}"
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Section 3: Cross-provider text diff
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cross_provider_text_diff_report() {
    let harness = TestHarness::new("cross_provider_text_diff_report");
    let ctx = simple_context();
    let opts = default_options();

    // Baseline: Anthropic
    let (provider, _server) = setup_anthropic(&harness, &anthropic_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let baseline = build_transcript("anthropic-messages", "anthropic", "text", &result);

    // Compare: OpenAI Responses
    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let openai = build_transcript("openai-responses", "openai", "text", &result);

    // Compare: Gemini
    let (provider, _server) = setup_gemini(&harness, &gemini_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let gemini = build_transcript("gemini-generative", "google", "text", &result);

    let report = build_diff_report("text", &baseline, &[openai, gemini]);
    write_diff_report_jsonl(&harness, "text_cross_provider", &report);
    write_markdown_summary(&harness, "text_cross_provider", &report);

    // All should extract the same final text
    let text_diffs: Vec<_> = report
        .diffs
        .iter()
        .filter(|d| d.field == "final_text")
        .collect();
    assert!(
        text_diffs.is_empty(),
        "All providers should extract the same text: {text_diffs:?}"
    );
}

#[test]
fn cross_provider_text_stop_reasons_are_equivalent() {
    let harness = TestHarness::new("cross_provider_text_stop_reasons_are_equivalent");
    let ctx = simple_context();
    let opts = default_options();

    let (provider, _server) = setup_anthropic(&harness, &anthropic_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let anthropic = build_transcript("anthropic", "anthropic", "text", &result);

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let openai = build_transcript("openai", "openai", "text", &result);

    let (provider, _server) = setup_gemini(&harness, &gemini_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let gemini = build_transcript("gemini", "google", "text", &result);

    // All text responses should have a "stop"-equivalent reason
    for t in [&anthropic, &openai, &gemini] {
        let reason = t.stop_reason.as_deref().unwrap_or("none");
        assert!(
            reason.contains("Stop") || reason.contains("EndTurn"),
            "{}: stop reason should be Stop or EndTurn, got {reason}",
            t.family
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Section 4: Cross-provider tool-call diff
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cross_provider_tool_diff_report() {
    let harness = TestHarness::new("cross_provider_tool_diff_report");
    let ctx = tool_context();
    let opts = default_options();

    let (provider, _server) = setup_anthropic(&harness, &anthropic_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let baseline = build_transcript("anthropic-messages", "anthropic", "tool", &result);

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let openai = build_transcript("openai-responses", "openai", "tool", &result);

    let (provider, _server) = setup_gemini(&harness, &gemini_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let gemini = build_transcript("gemini-generative", "google", "tool", &result);

    let report = build_diff_report("tool", &baseline, &[openai, gemini]);
    write_diff_report_jsonl(&harness, "tool_cross_provider", &report);
    write_markdown_summary(&harness, "tool_cross_provider", &report);

    // Tool call count should match
    let count_diffs: Vec<_> = report
        .diffs
        .iter()
        .filter(|d| d.field == "tool_call_count")
        .collect();
    assert!(
        count_diffs.is_empty(),
        "All providers should have same tool call count: {count_diffs:?}"
    );

    // Tool names should match
    let name_diffs: Vec<_> = report
        .diffs
        .iter()
        .filter(|d| d.field == "tool_names")
        .collect();
    assert!(
        name_diffs.is_empty(),
        "All providers should call the same tools: {name_diffs:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 5: Event shape normalization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn abstract_event_shape_merges_consecutive_deltas() {
    let kinds = vec!["Start", "TextDelta", "TextDelta", "TextDelta", "Done"];
    let shape = abstract_event_shape(&kinds);
    assert_eq!(shape, "Start → TextDelta → Done");
}

#[test]
fn abstract_event_shape_preserves_non_delta_events() {
    let kinds = vec![
        "Start",
        "TextStart",
        "TextDelta",
        "TextEnd",
        "ToolCallStart",
        "ToolCallDelta",
        "ToolCallEnd",
        "Done",
    ];
    let shape = abstract_event_shape(&kinds);
    assert_eq!(
        shape,
        "Start → TextStart → TextDelta → TextEnd → ToolCallStart → ToolCallDelta → ToolCallEnd → Done"
    );
}

#[test]
fn text_event_shapes_are_consistent_per_family() {
    let harness = TestHarness::new("text_event_shapes_are_consistent_per_family");
    let ctx = simple_context();
    let opts = default_options();

    let (provider, _server) = setup_anthropic(&harness, &anthropic_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone()).unwrap();
    let kinds: Vec<&str> = result.iter().map(event_kind).collect();
    let anthropic_shape = abstract_event_shape(&kinds);

    // Anthropic should have Start → TextStart → TextDelta → TextEnd → Done
    assert!(
        anthropic_shape.contains("Start") && anthropic_shape.contains("Done"),
        "Anthropic text shape: {anthropic_shape}"
    );

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone()).unwrap();
    let kinds: Vec<&str> = result.iter().map(event_kind).collect();
    let openai_shape = abstract_event_shape(&kinds);

    assert!(
        openai_shape.contains("Start") && openai_shape.contains("Done"),
        "OpenAI text shape: {openai_shape}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 6: Diff engine unit tests
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn diff_identical_transcripts_has_no_diffs() {
    let t1 = GoldenTranscript {
        family: "a".to_string(),
        provider: "a".to_string(),
        scenario: "text".to_string(),
        events: vec![NormalizedEvent {
            kind: "Done".to_string(),
            content_index: None,
            text: None,
            tool_name: None,
            tool_arguments: None,
            stop_reason: Some("Stop".to_string()),
            error_message: None,
        }],
        final_text: "Hello".to_string(),
        tool_call_count: 0,
        stop_reason: Some("Stop".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 1,
    };
    let t2 = GoldenTranscript {
        family: "b".to_string(),
        ..t1.clone()
    };
    let diffs = diff_transcripts(&t1, &t2);
    assert!(
        diffs.is_empty(),
        "identical content should produce no diffs"
    );
}

#[test]
fn diff_detects_text_mismatch() {
    let t1 = GoldenTranscript {
        family: "baseline".to_string(),
        provider: "a".to_string(),
        scenario: "text".to_string(),
        events: Vec::new(),
        final_text: "Hello world!".to_string(),
        tool_call_count: 0,
        stop_reason: Some("Stop".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 0,
    };
    let t2 = GoldenTranscript {
        family: "compare".to_string(),
        final_text: "Bonjour!".to_string(),
        ..t1.clone()
    };
    let diffs = diff_transcripts(&t1, &t2);
    assert!(
        diffs.iter().any(|d| d.field == "final_text"),
        "should detect text mismatch"
    );
}

#[test]
fn diff_detects_tool_count_mismatch() {
    let t1 = GoldenTranscript {
        family: "baseline".to_string(),
        provider: "a".to_string(),
        scenario: "tool".to_string(),
        events: Vec::new(),
        final_text: String::new(),
        tool_call_count: 1,
        stop_reason: Some("ToolUse".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 0,
    };
    let t2 = GoldenTranscript {
        family: "compare".to_string(),
        tool_call_count: 2,
        ..t1.clone()
    };
    let diffs = diff_transcripts(&t1, &t2);
    assert!(
        diffs.iter().any(|d| d.field == "tool_call_count"),
        "should detect tool count mismatch"
    );
}

#[test]
fn diff_detects_stop_reason_mismatch() {
    let t1 = GoldenTranscript {
        family: "baseline".to_string(),
        provider: "a".to_string(),
        scenario: "text".to_string(),
        events: Vec::new(),
        final_text: "Hello".to_string(),
        tool_call_count: 0,
        stop_reason: Some("Stop".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 0,
    };
    let t2 = GoldenTranscript {
        family: "compare".to_string(),
        stop_reason: Some("Length".to_string()),
        ..t1.clone()
    };
    let diffs = diff_transcripts(&t1, &t2);
    assert!(
        diffs.iter().any(|d| d.field == "stop_reason"),
        "should detect stop_reason mismatch"
    );
}

#[test]
fn diff_detects_event_shape_mismatch() {
    let t1 = GoldenTranscript {
        family: "baseline".to_string(),
        provider: "a".to_string(),
        scenario: "text".to_string(),
        events: vec![
            NormalizedEvent {
                kind: "Start".to_string(),
                content_index: None,
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "TextDelta".to_string(),
                content_index: Some(0),
                text: Some("hi".to_string()),
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "Done".to_string(),
                content_index: None,
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: Some("Stop".to_string()),
                error_message: None,
            },
        ],
        final_text: "hi".to_string(),
        tool_call_count: 0,
        stop_reason: Some("Stop".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 3,
    };
    let t2 = GoldenTranscript {
        family: "compare".to_string(),
        events: vec![
            NormalizedEvent {
                kind: "Start".to_string(),
                content_index: None,
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "TextStart".to_string(),
                content_index: Some(0),
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "TextDelta".to_string(),
                content_index: Some(0),
                text: Some("hi".to_string()),
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "TextEnd".to_string(),
                content_index: Some(0),
                text: Some("hi".to_string()),
                tool_name: None,
                tool_arguments: None,
                stop_reason: None,
                error_message: None,
            },
            NormalizedEvent {
                kind: "Done".to_string(),
                content_index: None,
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: Some("Stop".to_string()),
                error_message: None,
            },
        ],
        event_count: 5,
        ..t1.clone()
    };
    let diffs = diff_transcripts(&t1, &t2);
    assert!(
        diffs.iter().any(|d| d.field == "event_shape"),
        "should detect event shape divergence"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 7: Report generation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn diff_report_schema_version() {
    let report = DiffReport {
        schema: TRANSCRIPT_SCHEMA.to_string(),
        scenario: "test".to_string(),
        baseline_family: "a".to_string(),
        families_compared: vec!["b".to_string()],
        diffs: Vec::new(),
        all_match: true,
    };
    assert_eq!(report.schema, "pi.golden_transcript.v1");
}

#[test]
fn diff_report_all_match_when_no_diffs() {
    let report = build_diff_report(
        "text",
        &GoldenTranscript {
            family: "a".to_string(),
            provider: "a".to_string(),
            scenario: "text".to_string(),
            events: Vec::new(),
            final_text: "Hello".to_string(),
            tool_call_count: 0,
            stop_reason: Some("Stop".to_string()),
            sequence_valid: true,
            sequence_error: None,
            event_count: 0,
        },
        &[GoldenTranscript {
            family: "b".to_string(),
            provider: "b".to_string(),
            scenario: "text".to_string(),
            events: Vec::new(),
            final_text: "Hello".to_string(),
            tool_call_count: 0,
            stop_reason: Some("Stop".to_string()),
            sequence_valid: true,
            sequence_error: None,
            event_count: 0,
        }],
    );
    assert!(report.all_match);
    assert!(report.diffs.is_empty());
}

#[test]
fn diff_report_jsonl_artifact_is_valid_json() {
    let harness = TestHarness::new("diff_report_jsonl_artifact_is_valid_json");
    let report = DiffReport {
        schema: TRANSCRIPT_SCHEMA.to_string(),
        scenario: "test".to_string(),
        baseline_family: "a".to_string(),
        families_compared: vec!["b".to_string()],
        diffs: vec![TranscriptDiff {
            field: "final_text".to_string(),
            baseline_family: "a".to_string(),
            baseline_value: "Hello".to_string(),
            compare_family: "b".to_string(),
            compare_value: "Bonjour".to_string(),
            severity: "semantic".to_string(),
        }],
        all_match: false,
    };
    write_diff_report_jsonl(&harness, "test_artifact", &report);
    let path = harness.temp_path("test_artifact_diff.json");
    let content = std::fs::read_to_string(path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["schema"], "pi.golden_transcript.v1");
    assert!(!parsed["all_match"].as_bool().unwrap());
}

#[test]
fn markdown_summary_artifact_is_written() {
    let harness = TestHarness::new("markdown_summary_artifact_is_written");
    let report = DiffReport {
        schema: TRANSCRIPT_SCHEMA.to_string(),
        scenario: "text".to_string(),
        baseline_family: "anthropic".to_string(),
        families_compared: vec!["openai".to_string()],
        diffs: vec![TranscriptDiff {
            field: "event_shape".to_string(),
            baseline_family: "anthropic".to_string(),
            baseline_value: "Start → TextDelta → Done".to_string(),
            compare_family: "openai".to_string(),
            compare_value: "Start → TextStart → TextDelta → TextEnd → Done".to_string(),
            severity: "structural".to_string(),
        }],
        all_match: false,
    };
    write_markdown_summary(&harness, "test_md", &report);
    let path = harness.temp_path("test_md_diff.md");
    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.contains("DIFFS FOUND"));
    assert!(content.contains("event_shape"));
    assert!(content.contains("structural"));
}

// ═══════════════════════════════════════════════════════════════════════
// Section 8: Transcript JSONL serialization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn transcript_jsonl_is_valid() {
    let harness = TestHarness::new("transcript_jsonl_is_valid");
    let transcripts = vec![
        GoldenTranscript {
            family: "a".to_string(),
            provider: "a".to_string(),
            scenario: "text".to_string(),
            events: vec![NormalizedEvent {
                kind: "Done".to_string(),
                content_index: None,
                text: None,
                tool_name: None,
                tool_arguments: None,
                stop_reason: Some("Stop".to_string()),
                error_message: None,
            }],
            final_text: "Hello".to_string(),
            tool_call_count: 0,
            stop_reason: Some("Stop".to_string()),
            sequence_valid: true,
            sequence_error: None,
            event_count: 1,
        },
        GoldenTranscript {
            family: "b".to_string(),
            provider: "b".to_string(),
            scenario: "text".to_string(),
            events: Vec::new(),
            final_text: "World".to_string(),
            tool_call_count: 0,
            stop_reason: Some("Stop".to_string()),
            sequence_valid: true,
            sequence_error: None,
            event_count: 0,
        },
    ];
    write_transcript_jsonl(&harness, "serialization_test", &transcripts);
    let path = harness.temp_path("serialization_test_transcripts.jsonl");
    let content = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "should have 2 JSONL lines");
    for line in &lines {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(parsed["family"].is_string());
        assert!(parsed["scenario"].is_string());
    }
}

#[test]
fn normalized_event_serializes_all_fields() {
    let event = NormalizedEvent {
        kind: "ToolCallEnd".to_string(),
        content_index: Some(0),
        text: None,
        tool_name: Some("echo".to_string()),
        tool_arguments: Some(r#"{"text":"hello"}"#.to_string()),
        stop_reason: None,
        error_message: None,
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["kind"], "ToolCallEnd");
    assert_eq!(json["tool_name"], "echo");
    assert!(json["tool_arguments"].is_string());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 9: Edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn empty_transcript_produces_error_diff() {
    let ok_transcript = GoldenTranscript {
        family: "ok".to_string(),
        provider: "a".to_string(),
        scenario: "text".to_string(),
        events: Vec::new(),
        final_text: "Hello".to_string(),
        tool_call_count: 0,
        stop_reason: Some("Stop".to_string()),
        sequence_valid: true,
        sequence_error: None,
        event_count: 0,
    };
    let error_transcript = GoldenTranscript {
        family: "error".to_string(),
        provider: "b".to_string(),
        scenario: "text".to_string(),
        events: Vec::new(),
        final_text: String::new(),
        tool_call_count: 0,
        stop_reason: None,
        sequence_valid: false,
        sequence_error: Some("connection refused".to_string()),
        event_count: 0,
    };
    let diffs = diff_transcripts(&ok_transcript, &error_transcript);
    assert!(
        !diffs.is_empty(),
        "should detect differences vs error transcript"
    );
}

#[test]
fn truncate_for_table_handles_short_strings() {
    assert_eq!(truncate_for_table("hello", 10), "hello");
    assert_eq!(truncate_for_table("hello world test", 5), "hello...");
}

// ═══════════════════════════════════════════════════════════════════════
// Section 10: Comprehensive cross-provider report
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn comprehensive_golden_transcript_report() {
    let harness = TestHarness::new("comprehensive_golden_transcript_report");
    let mut all_transcripts = Vec::new();
    let mut all_reports = Vec::new();

    // Capture text transcripts from all families
    let ctx = simple_context();
    let opts = default_options();

    let (provider, _server) = setup_anthropic(&harness, &anthropic_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let anthropic_text = build_transcript("anthropic-messages", "anthropic", "text", &result);
    all_transcripts.push(anthropic_text.clone());

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let openai_text = build_transcript("openai-responses", "openai", "text", &result);
    all_transcripts.push(openai_text.clone());

    let (provider, _server) = setup_gemini(&harness, &gemini_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let gemini_text = build_transcript("gemini-generative", "google", "text", &result);
    all_transcripts.push(gemini_text.clone());

    let (provider, _server) = setup_openai_completions(&harness, &openai_completions_text_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let completions_text = build_transcript("openai-completions", "groq", "text", &result);
    all_transcripts.push(completions_text.clone());

    // Text diff report (Anthropic as baseline)
    let text_report = build_diff_report(
        "text",
        &anthropic_text,
        &[openai_text, gemini_text, completions_text],
    );
    all_reports.push(text_report.clone());

    // Capture tool transcripts
    let ctx = tool_context();

    let (provider, _server) = setup_anthropic(&harness, &anthropic_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let anthropic_tool = build_transcript("anthropic-messages", "anthropic", "tool", &result);
    all_transcripts.push(anthropic_tool.clone());

    let (provider, _server) = setup_openai_responses(&harness, &openai_responses_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let openai_tool = build_transcript("openai-responses", "openai", "tool", &result);
    all_transcripts.push(openai_tool.clone());

    let (provider, _server) = setup_gemini(&harness, &gemini_tool_sse());
    let result = collect_events(provider, ctx.clone(), opts.clone());
    let gemini_tool = build_transcript("gemini-generative", "google", "tool", &result);
    all_transcripts.push(gemini_tool.clone());

    let tool_report = build_diff_report("tool", &anthropic_tool, &[openai_tool, gemini_tool]);
    all_reports.push(tool_report.clone());

    // Write comprehensive artifacts
    write_transcript_jsonl(&harness, "comprehensive", &all_transcripts);

    // Write per-scenario diff reports
    for report in &all_reports {
        write_diff_report_jsonl(
            &harness,
            &format!("comprehensive_{}", report.scenario),
            report,
        );
        write_markdown_summary(
            &harness,
            &format!("comprehensive_{}", report.scenario),
            report,
        );
    }

    // Write summary
    let mut summary = String::new();
    let _ = writeln!(summary, "# Golden Transcript Comprehensive Report\n");
    let _ = writeln!(
        summary,
        "Total transcripts captured: {}",
        all_transcripts.len()
    );
    let _ = writeln!(summary, "Scenarios: text, tool\n");
    for report in &all_reports {
        let _ = writeln!(
            summary,
            "## {} scenario: {} diff(s), all_match={}",
            report.scenario,
            report.diffs.len(),
            report.all_match
        );
        for d in &report.diffs {
            let _ = writeln!(
                summary,
                "  - [{sev}] {field}: {base}({bval}) vs {comp}({cval})",
                sev = d.severity,
                field = d.field,
                base = d.baseline_family,
                bval = truncate_for_table(&d.baseline_value, 40),
                comp = d.compare_family,
                cval = truncate_for_table(&d.compare_value, 40),
            );
        }
    }

    let path = harness.temp_path("comprehensive_summary.md");
    std::fs::write(&path, &summary).expect("write summary");
    harness.record_artifact("comprehensive_summary.md", &path);

    // Structural assertions
    assert!(
        all_transcripts.len() >= 7,
        "Should capture at least 7 transcripts (4 text + 3 tool)"
    );
    for t in &all_transcripts {
        assert!(
            t.sequence_valid,
            "{} {} should be valid: {:?}",
            t.family, t.scenario, t.sequence_error
        );
    }
}
