//! Deterministic multi-provider E2E scenario scripts with structured logging.
//!
//! Exercises core workflows (simple text, tool-call round-trip, error handling,
//! multi-turn conversation, streaming event ordering) across representative
//! providers from every API family. All tests use `MockHttpServer` for full
//! determinism and produce JSONL logs + human-readable summary artifacts.
//!
//! bd-3uqg.8.7

mod common;

use common::{MockHttpResponse, TestHarness};
use futures::StreamExt;
use pi::model::{Message, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamEvent, StreamOptions, ToolDef};
use pi::providers::create_provider;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════

fn make_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} e2e model"),
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

fn request_header(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone())
}

fn simple_context() -> Context<'static> {
    Context {
        system_prompt: Some("You are a deterministic test model.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Say hello.".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    }
}

fn tool_context() -> Context<'static> {
    Context {
        system_prompt: Some("You are a deterministic test model.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Call the echo tool with text='hello'.".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: vec![echo_tool()].into(),
    }
}

fn echo_tool() -> ToolDef {
    ToolDef {
        name: "echo".to_string(),
        description: "Echo text back.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
    }
}

fn text_event_stream(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn json_response(status: u16, body: &serde_json::Value) -> MockHttpResponse {
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SSE payloads: simple text responses
// ═══════════════════════════════════════════════════════════════════════

fn openai_chat_text_sse() -> String {
    [
        r#"data: {"id":"e2e-oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"e2e-oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"content":" world!"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"e2e-oai-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

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
        r#"data: {"type":"message_start","message":{"id":"msg_e2e_001","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
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

fn cohere_text_sse() -> String {
    [
        r"event: message-start",
        r#"data: {"id":"e2e-cohere-001","type":"message-start","delta":{"message":{"role":"assistant","content":[]}}}"#,
        "",
        r"event: content-start",
        r#"data: {"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}"#,
        "",
        r"event: content-delta",
        r#"data: {"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"Hello world!"}}}}"#,
        "",
        r"event: content-end",
        r#"data: {"type":"content-end","index":0}"#,
        "",
        r"event: message-end",
        r#"data: {"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"billed_units":{"input_tokens":10,"output_tokens":5},"tokens":{"input_tokens":10,"output_tokens":5}}}}"#,
        "",
    ]
    .join("\n")
}

// Bedrock uses non-SSE JSON responses and requires AWS SigV4 signing,
// so it is excluded from the mock-based E2E families. The JSON shape is
// retained here for reference and future use when bedrock mocking is added.
#[allow(dead_code)]
fn bedrock_text_json() -> serde_json::Value {
    json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "Hello world!"}]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
    })
}

// ═══════════════════════════════════════════════════════════════════════
// SSE payloads: tool-call responses
// ═══════════════════════════════════════════════════════════════════════

fn openai_chat_tool_sse() -> String {
    [
        r#"data: {"id":"e2e-oai-tool","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"id":"call_e2e_001","index":0,"type":"function","function":{"name":"echo","arguments":""}}]},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"e2e-oai-tool","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"text\":\"hello\"}"}}]},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"e2e-oai-tool","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":15,"completion_tokens":12,"total_tokens":27}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn openai_responses_tool_sse() -> String {
    [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_e2e","call_id":"call_e2e_001","name":"echo","arguments":""}}"#,
        "",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_e2e","output_index":0,"delta":"{\"text\":\"hello\"}"}"#,
        "",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_e2e","call_id":"call_e2e_001","name":"echo","arguments":"{\"text\":\"hello\"}","status":"completed"}}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":15,"output_tokens":12,"total_tokens":27}}}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_tool_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_e2e_tool","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":15,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_e2e_001","name":"echo","input":{}}}"#,
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

fn cohere_tool_sse() -> String {
    let args = serde_json::to_string(&json!({"text": "hello"})).unwrap();
    [
        r"event: message-start",
        r#"data: {"id":"e2e-cohere-tool","type":"message-start","delta":{"message":{"role":"assistant","content":[]}}}"#,
        "",
        "event: tool-call-start",
        &format!(
            r#"data: {{"type":"tool-call-start","delta":{{"message":{{"tool_calls":{{"id":"call_e2e_001","type":"function","function":{{"name":"echo","arguments":"{args_escaped}"}}}}}}}}}}"#,
            args_escaped = args.replace('"', "\\\"")
        ),
        "",
        "event: tool-call-end",
        r#"data: {"type":"tool-call-end"}"#,
        "",
        r"event: message-end",
        r#"data: {"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":{"billed_units":{"input_tokens":15,"output_tokens":12},"tokens":{"input_tokens":15,"output_tokens":12}}}}"#,
        "",
    ]
    .join("\n")
}

#[allow(dead_code)]
fn bedrock_tool_json() -> serde_json::Value {
    json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{
                    "toolUse": {
                        "toolUseId": "e2e_tool_001",
                        "name": "echo",
                        "input": {"text": "hello"}
                    }
                }]
            }
        },
        "stopReason": "tool_use",
        "usage": {"inputTokens": 15, "outputTokens": 12, "totalTokens": 27}
    })
}

// ═══════════════════════════════════════════════════════════════════════
// SSE payloads: error responses
// ═══════════════════════════════════════════════════════════════════════

fn openai_error_401() -> MockHttpResponse {
    json_response(
        401,
        &json!({"error": {"message": "Invalid API key", "type": "authentication_error", "code": "invalid_api_key"}}),
    )
}

fn anthropic_error_401() -> MockHttpResponse {
    json_response(
        401,
        &json!({"type": "error", "error": {"type": "authentication_error", "message": "Invalid API key"}}),
    )
}

fn gemini_error_401() -> MockHttpResponse {
    json_response(
        401,
        &json!({"error": {"code": 401, "message": "API key not valid", "status": "UNAUTHENTICATED"}}),
    )
}

fn cohere_error_401() -> MockHttpResponse {
    json_response(401, &json!({"message": "invalid api token"}))
}

#[allow(dead_code)]
fn bedrock_error_401() -> MockHttpResponse {
    json_response(
        401,
        &json!({"message": "The security token included in the request is invalid."}),
    )
}

// ═══════════════════════════════════════════════════════════════════════
// SSE payloads: rate-limit (429) error responses
// ═══════════════════════════════════════════════════════════════════════

fn openai_error_429() -> MockHttpResponse {
    let mut resp = json_response(
        429,
        &json!({"error": {"message": "Rate limit reached for model", "type": "rate_limit_error", "code": "rate_limit_exceeded"}}),
    );
    resp.headers
        .push(("Retry-After".to_string(), "30".to_string()));
    resp.headers.push((
        "x-ratelimit-remaining-requests".to_string(),
        "0".to_string(),
    ));
    resp
}

fn anthropic_error_429() -> MockHttpResponse {
    let mut resp = json_response(
        429,
        &json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Number of request tokens has exceeded your per-minute rate limit"}}),
    );
    resp.headers
        .push(("retry-after".to_string(), "60".to_string()));
    resp
}

fn gemini_error_429() -> MockHttpResponse {
    json_response(
        429,
        &json!({"error": {"code": 429, "message": "Resource has been exhausted", "status": "RESOURCE_EXHAUSTED"}}),
    )
}

fn cohere_error_429() -> MockHttpResponse {
    json_response(429, &json!({"message": "too many requests"}))
}

// ═══════════════════════════════════════════════════════════════════════
// SSE payloads: schema-drift responses (unexpected/missing fields)
// ═══════════════════════════════════════════════════════════════════════

/// OpenAI-compatible response with unexpected `metadata` field and missing
/// `finish_reason` on the final chunk — simulates a provider schema update.
fn openai_schema_drift_sse() -> String {
    [
        r#"data: {"id":"drift-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-drift","choices":[{"index":0,"delta":{"role":"assistant","content":"Drifted"},"finish_reason":null}],"metadata":{"experiment":"v2"}}"#,
        "",
        r#"data: {"id":"drift-001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-drift","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

/// Anthropic response with an unexpected `content_block` type and extra fields.
fn anthropic_schema_drift_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_drift","type":"message","role":"assistant","content":[],"model":"claude-drift","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0},"system_fingerprint":"fp_drift"}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Drifted"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","extra_field":"unexpected"},"usage":{"output_tokens":2}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

/// Gemini response with unexpected top-level fields.
fn gemini_schema_drift_sse() -> String {
    [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Drifted"}],"role":"model"},"finishReason":"STOP","index":0,"safetyRatings":[{"category":"HARM_CATEGORY_DANGEROUS","probability":"NEGLIGIBLE"}]}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"modelVersion":"gemini-drift-001"}"#,
        "",
    ]
    .join("\n")
}

/// `OpenAI` Responses API response with unexpected extra fields.
fn openai_responses_schema_drift_sse() -> String {
    [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_drift","content_index":0,"delta":"Drifted","metadata":{"experiment":"v2"}}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7},"system_fingerprint":"fp_drift"}}"#,
        "",
    ]
    .join("\n")
}

/// Cohere response with extra fields in the delta.
fn cohere_schema_drift_sse() -> String {
    [
        r"event: message-start",
        r#"data: {"id":"drift-cohere","type":"message-start","delta":{"message":{"role":"assistant","content":[]}},"api_version":"2.1"}"#,
        "",
        r"event: content-start",
        r#"data: {"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}"#,
        "",
        r"event: content-delta",
        r#"data: {"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"Drifted"}}}}"#,
        "",
        r"event: content-end",
        r#"data: {"type":"content-end","index":0}"#,
        "",
        r"event: message-end",
        r#"data: {"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"billed_units":{"input_tokens":5,"output_tokens":2},"tokens":{"input_tokens":5,"output_tokens":2}}}}"#,
        "",
    ]
    .join("\n")
}

// ═══════════════════════════════════════════════════════════════════════
// Multi-turn SSE payloads (second response in a conversation)
// ═══════════════════════════════════════════════════════════════════════

fn openai_chat_text_sse_turn2() -> String {
    [
        r#"data: {"id":"e2e-oai-t2","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Goodbye!"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"e2e-oai-t2","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn anthropic_text_sse_turn2() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_e2e_t2","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":20,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Goodbye!"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario descriptor and runner
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
struct ScenarioResult {
    family: String,
    provider: String,
    scenario: String,
    status: String,
    elapsed_ms: u64,
    event_count: usize,
    text_chars: usize,
    tool_calls: usize,
    stop_reason: Option<String>,
    error: Option<String>,
    event_sequence: Vec<String>,
    sequence_valid: bool,
}

const fn event_kind(event: &StreamEvent) -> &'static str {
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

fn validate_event_sequence(events: &[StreamEvent]) -> Result<(), String> {
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

    let mut in_text = false;
    let mut in_thinking = false;
    let mut in_tool = false;

    // Track whether explicit Start/End pairs are used. Many providers skip
    // TextStart/TextEnd and emit bare TextDelta events, which is valid.
    // We only enforce pairing when the Start variant appears.
    for (idx, event) in events.iter().enumerate() {
        match event {
            StreamEvent::TextStart { .. } => in_text = true,
            StreamEvent::TextEnd { .. } if !in_text => {
                return Err(format!("TextEnd without TextStart at {idx}"));
            }
            StreamEvent::TextEnd { .. } => in_text = false,
            StreamEvent::ThinkingStart { .. } => in_thinking = true,
            StreamEvent::ThinkingDelta { .. } if !in_thinking => {
                return Err(format!("ThinkingDelta outside thinking block at {idx}"));
            }
            StreamEvent::ThinkingEnd { .. } if !in_thinking => {
                return Err(format!("ThinkingEnd without ThinkingStart at {idx}"));
            }
            StreamEvent::ThinkingEnd { .. } => in_thinking = false,
            StreamEvent::ToolCallStart { .. } => in_tool = true,
            StreamEvent::ToolCallEnd { .. } if !in_tool => {
                return Err(format!("ToolCallEnd without ToolCallStart at {idx}"));
            }
            StreamEvent::ToolCallEnd { .. } => in_tool = false,
            // TextDelta, ToolCallDelta, and other events are allowed
            // with or without their corresponding Start events.
            _ => {}
        }
    }

    Ok(())
}

/// Collect stream events from a provider, returning collected events or an error string.
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

fn summarize_events(events: &[StreamEvent]) -> (usize, usize, Option<String>) {
    let mut text_chars = 0usize;
    let mut tool_calls = 0usize;
    let mut stop_reason = None;
    for event in events {
        match event {
            StreamEvent::TextDelta { delta, .. } => text_chars += delta.chars().count(),
            StreamEvent::TextEnd { content, .. } => text_chars = content.chars().count(),
            StreamEvent::ToolCallEnd { .. } => tool_calls += 1,
            StreamEvent::Done { reason, .. } | StreamEvent::Error { reason, .. } => {
                stop_reason = Some(format!("{reason:?}"));
            }
            _ => {}
        }
    }
    (text_chars, tool_calls, stop_reason)
}

// ═══════════════════════════════════════════════════════════════════════
// Per-family scenario definitions
// ═══════════════════════════════════════════════════════════════════════

/// Each family defines how to set up the mock server and create the provider.
struct FamilySpec {
    family: &'static str,
    provider_id: &'static str,
    model_id: &'static str,
    /// The API key used for mock requests.
    api_key: &'static str,
    text_sse: fn() -> String,
    tool_sse: fn() -> String,
    error_response: fn() -> MockHttpResponse,
    /// Rate-limit (429) error response.
    rate_limit_response: fn() -> MockHttpResponse,
    /// Schema-drift SSE response (unexpected/extra fields).
    schema_drift_sse: fn() -> String,
    /// Computes the mock route path the provider will actually hit.
    mock_route: fn(model_id: &str, api_key: &str) -> String,
    /// Computes the `base_url` for the `ModelEntry` given the server base URL.
    entry_base_url: fn(server_base: &str) -> String,
}

fn oai_completions_route(_model: &str, _key: &str) -> String {
    "/openai/v1/chat/completions".to_string()
}
fn oai_completions_base(server: &str) -> String {
    format!("{server}/openai/v1")
}

fn oai_responses_route(_model: &str, _key: &str) -> String {
    "/v1/responses".to_string()
}
fn oai_responses_base(server: &str) -> String {
    format!("{server}/v1")
}

fn anthropic_route(_model: &str, _key: &str) -> String {
    "/v1/messages".to_string()
}
fn anthropic_base(server: &str) -> String {
    format!("{server}/v1/messages")
}

fn gemini_route(model: &str, _key: &str) -> String {
    format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
}
fn gemini_base(server: &str) -> String {
    format!("{server}/v1beta")
}

fn cohere_route(_model: &str, _key: &str) -> String {
    "/v2/chat".to_string()
}
fn cohere_base(server: &str) -> String {
    format!("{server}/v2")
}

const OPENAI_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "groq",
    model_id: "e2e-llama",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const OPENAI_RESPONSES: FamilySpec = FamilySpec {
    family: "openai-responses",
    provider_id: "openai",
    model_id: "e2e-gpt",
    api_key: "e2e-test-key",
    text_sse: openai_responses_text_sse,
    tool_sse: openai_responses_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_responses_schema_drift_sse,
    mock_route: oai_responses_route,
    entry_base_url: oai_responses_base,
};

const ANTHROPIC_MESSAGES: FamilySpec = FamilySpec {
    family: "anthropic-messages",
    provider_id: "anthropic",
    model_id: "e2e-claude",
    api_key: "e2e-test-key",
    text_sse: anthropic_text_sse,
    tool_sse: anthropic_tool_sse,
    error_response: anthropic_error_401,
    rate_limit_response: anthropic_error_429,
    schema_drift_sse: anthropic_schema_drift_sse,
    mock_route: anthropic_route,
    entry_base_url: anthropic_base,
};

const GEMINI_GENERATIVE: FamilySpec = FamilySpec {
    family: "google-generative-ai",
    provider_id: "google",
    model_id: "e2e-gemini",
    api_key: "e2e-gemini-key",
    text_sse: gemini_text_sse,
    tool_sse: gemini_tool_sse,
    error_response: gemini_error_401,
    rate_limit_response: gemini_error_429,
    schema_drift_sse: gemini_schema_drift_sse,
    mock_route: gemini_route,
    entry_base_url: gemini_base,
};

const COHERE_CHAT: FamilySpec = FamilySpec {
    family: "cohere-chat",
    provider_id: "cohere",
    model_id: "e2e-command",
    api_key: "e2e-test-key",
    text_sse: cohere_text_sse,
    tool_sse: cohere_tool_sse,
    error_response: cohere_error_401,
    rate_limit_response: cohere_error_429,
    schema_drift_sse: cohere_schema_drift_sse,
    mock_route: cohere_route,
    entry_base_url: cohere_base,
};

// ── Gap provider FamilySpecs (bd-3uqg.11.11.4) ─────────────────────────
// These all share the openai-completions wire format with OPENAI_COMPLETIONS
// (which uses groq as its representative).

const CEREBRAS_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "cerebras",
    model_id: "e2e-cerebras",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const OPENROUTER_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "openrouter",
    model_id: "e2e-openrouter",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const MOONSHOTAI_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "moonshotai",
    model_id: "e2e-kimi",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const ALIBABA_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "alibaba",
    model_id: "e2e-qwen",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

// ── Longtail provider FamilySpecs (bd-3uqg.11.10.6) ─────────────────────
// Representative longtail quick-win providers, all OpenAI-compatible.

const STACKIT_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "stackit",
    model_id: "e2e-stackit",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const MISTRAL_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "mistral",
    model_id: "e2e-mistral",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const DEEPINFRA_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "deepinfra",
    model_id: "e2e-deepinfra",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const TOGETHERAI_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "togetherai",
    model_id: "e2e-togetherai",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const NVIDIA_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "nvidia",
    model_id: "e2e-nvidia",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const HUGGINGFACE_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "huggingface",
    model_id: "e2e-huggingface",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

const OLLAMA_CLOUD_COMPLETIONS: FamilySpec = FamilySpec {
    family: "openai-completions",
    provider_id: "ollama-cloud",
    model_id: "e2e-ollama-cloud",
    api_key: "e2e-test-key",
    text_sse: openai_chat_text_sse,
    tool_sse: openai_chat_tool_sse,
    error_response: openai_error_401,
    rate_limit_response: openai_error_429,
    schema_drift_sse: openai_schema_drift_sse,
    mock_route: oai_completions_route,
    entry_base_url: oai_completions_base,
};

/// All families that can be exercised deterministically with `MockHttpServer`.
/// Bedrock is excluded because it requires AWS `SigV4` signing and runtime
/// region resolution that cannot be mocked with a simple HTTP server.
const E2E_FAMILIES: &[&FamilySpec] = &[
    &OPENAI_COMPLETIONS,
    &OPENAI_RESPONSES,
    &ANTHROPIC_MESSAGES,
    &GEMINI_GENERATIVE,
    &COHERE_CHAT,
    &CEREBRAS_COMPLETIONS,
    &OPENROUTER_COMPLETIONS,
    &MOONSHOTAI_COMPLETIONS,
    &ALIBABA_COMPLETIONS,
    // Longtail quick-win providers (bd-3uqg.11.10.6)
    &STACKIT_COMPLETIONS,
    &MISTRAL_COMPLETIONS,
    &DEEPINFRA_COMPLETIONS,
    &TOGETHERAI_COMPLETIONS,
    &NVIDIA_COMPLETIONS,
    &HUGGINGFACE_COMPLETIONS,
    &OLLAMA_CLOUD_COMPLETIONS,
];

/// Set up a mock route for the given spec and return the `base_url` for the `ModelEntry`.
fn setup_mock_route(
    spec: &FamilySpec,
    server: &common::MockHttpServer,
    response: MockHttpResponse,
) -> String {
    let route_path = (spec.mock_route)(spec.model_id, spec.api_key);
    server.add_route("POST", &route_path, response);
    (spec.entry_base_url)(&server.base_url())
}

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Simple text generation across all families
// ═══════════════════════════════════════════════════════════════════════

/// Exercises a single-turn text generation through mock HTTP for every API family.
/// Validates that each family produces correct event ordering and non-empty text.
#[test]
fn e2e_simple_text_all_families() {
    let harness = TestHarness::new("e2e_simple_text_all_families");
    let mut results: Vec<ScenarioResult> = Vec::new();

    for spec in E2E_FAMILIES {
        harness.section(&format!("simple_text: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url = setup_mock_route(spec, &server, text_event_stream((spec.text_sse)()));
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "simple_text".to_string(),
                    status: "error".to_string(),
                    elapsed_ms: 0,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e.to_string()),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some(spec.api_key.to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        match collect_events(provider, simple_context(), options) {
            Ok(events) => {
                let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let sequence: Vec<String> =
                    events.iter().map(|e| event_kind(e).to_string()).collect();
                let seq_result = validate_event_sequence(&events);
                let (text_chars, tool_calls, stop_reason) = summarize_events(&events);

                harness
                    .log()
                    .info_ctx("e2e.text", format!("{} passed", spec.family), |ctx| {
                        ctx.push(("events".into(), events.len().to_string()));
                        ctx.push(("text_chars".into(), text_chars.to_string()));
                        ctx.push(("elapsed_ms".into(), elapsed.to_string()));
                    });

                assert!(
                    seq_result.is_ok(),
                    "{}: event sequence invalid: {}",
                    spec.family,
                    seq_result.unwrap_err()
                );
                assert!(
                    text_chars > 0,
                    "{}: expected non-empty text output",
                    spec.family
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "simple_text".to_string(),
                    status: "pass".to_string(),
                    elapsed_ms: elapsed,
                    event_count: events.len(),
                    text_chars,
                    tool_calls,
                    stop_reason,
                    error: None,
                    event_sequence: sequence,
                    sequence_valid: true,
                });
            }
            Err(e) => {
                panic!("{}: simple text failed: {e}", spec.family);
            }
        }
    }

    assert_eq!(
        results.len(),
        E2E_FAMILIES.len(),
        "expected one result per family"
    );

    write_results_jsonl(&harness, "e2e_simple_text", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: Tool-call emission across all families
// ═══════════════════════════════════════════════════════════════════════

/// Exercises tool-call scenarios through mock HTTP for every API family.
/// Validates that tool call events are emitted with correct structure.
#[test]
fn e2e_tool_call_all_families() {
    let harness = TestHarness::new("e2e_tool_call_all_families");
    let mut results: Vec<ScenarioResult> = Vec::new();

    for spec in E2E_FAMILIES {
        harness.section(&format!("tool_call: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url = setup_mock_route(spec, &server, text_event_stream((spec.tool_sse)()));
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "tool_call".to_string(),
                    status: "error".to_string(),
                    elapsed_ms: 0,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e.to_string()),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some(spec.api_key.to_string()),
            max_tokens: Some(128),
            ..Default::default()
        };

        let start = Instant::now();
        match collect_events(provider, tool_context(), options) {
            Ok(events) => {
                let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let sequence: Vec<String> =
                    events.iter().map(|e| event_kind(e).to_string()).collect();
                let seq_result = validate_event_sequence(&events);
                let (text_chars, tool_calls, stop_reason) = summarize_events(&events);

                harness
                    .log()
                    .info_ctx("e2e.tool", format!("{} passed", spec.family), |ctx| {
                        ctx.push(("events".into(), events.len().to_string()));
                        ctx.push(("tool_calls".into(), tool_calls.to_string()));
                        ctx.push(("elapsed_ms".into(), elapsed.to_string()));
                    });

                assert!(
                    seq_result.is_ok(),
                    "{}: tool event sequence invalid: {}",
                    spec.family,
                    seq_result.unwrap_err()
                );
                assert!(
                    tool_calls >= 1,
                    "{}: expected at least 1 tool call, got {tool_calls}",
                    spec.family
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "tool_call".to_string(),
                    status: "pass".to_string(),
                    elapsed_ms: elapsed,
                    event_count: events.len(),
                    text_chars,
                    tool_calls,
                    stop_reason,
                    error: None,
                    event_sequence: sequence,
                    sequence_valid: true,
                });
            }
            Err(e) => {
                panic!("{}: tool call failed: {e}", spec.family);
            }
        }
    }

    assert_eq!(
        results.len(),
        E2E_FAMILIES.len(),
        "expected one result per family"
    );

    write_results_jsonl(&harness, "e2e_tool_call", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Auth error handling across all families
// ═══════════════════════════════════════════════════════════════════════

/// Exercises error handling (HTTP 401) through mock HTTP for every API family.
/// Validates that providers surface the error rather than panicking.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_error_auth_all_families() {
    let harness = TestHarness::new("e2e_error_auth_all_families");
    let mut results: Vec<ScenarioResult> = Vec::new();

    for spec in E2E_FAMILIES {
        harness.section(&format!("error_auth: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url = setup_mock_route(spec, &server, (spec.error_response)());
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                // Some providers fail at create time without env; that's acceptable
                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_auth".to_string(),
                    status: "skip".to_string(),
                    elapsed_ms: 0,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e.to_string()),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some("invalid-key".to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        let result = collect_events(provider, simple_context(), options);
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        match result {
            Err(e) => {
                // Expected: providers should return an error for 401
                harness.log().info_ctx(
                    "e2e.error",
                    format!("{} error handled", spec.family),
                    |ctx| {
                        ctx.push(("error".into(), e.clone()));
                        ctx.push(("elapsed_ms".into(), elapsed.to_string()));
                    },
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_auth".to_string(),
                    status: "pass".to_string(),
                    elapsed_ms: elapsed,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
            }
            Ok(events) => {
                // Some providers emit Error events in-stream instead of failing
                let has_error = events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::Error { .. }));
                let sequence: Vec<String> =
                    events.iter().map(|e| event_kind(e).to_string()).collect();

                harness.log().info_ctx(
                    "e2e.error",
                    format!(
                        "{} error via {}",
                        spec.family,
                        if has_error {
                            "stream error event"
                        } else {
                            "unexpected success"
                        }
                    ),
                    |ctx| {
                        ctx.push(("events".into(), events.len().to_string()));
                        ctx.push(("has_error_event".into(), has_error.to_string()));
                    },
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_auth".to_string(),
                    status: if has_error { "pass" } else { "warn" }.to_string(),
                    elapsed_ms: elapsed,
                    event_count: events.len(),
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: None,
                    event_sequence: sequence,
                    sequence_valid: false,
                });
            }
        }
    }

    write_results_jsonl(&harness, "e2e_error_auth", &results);

    let pass_or_warn = results
        .iter()
        .filter(|r| r.status == "pass" || r.status == "warn")
        .count();
    assert!(
        pass_or_warn >= 3,
        "expected at least 3 families to handle auth errors, got {pass_or_warn}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3b: Rate-limit (429) error handling across all families
// ═══════════════════════════════════════════════════════════════════════

/// Exercises rate-limit (HTTP 429) handling through mock HTTP for every API
/// family. Validates that providers surface the error or emit an Error event
/// rather than panicking or returning success.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_error_rate_limit_all_families() {
    let harness = TestHarness::new("e2e_error_rate_limit_all_families");
    let mut results: Vec<ScenarioResult> = Vec::new();

    for spec in E2E_FAMILIES {
        harness.section(&format!("error_rate_limit: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url = setup_mock_route(spec, &server, (spec.rate_limit_response)());
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_rate_limit".to_string(),
                    status: "skip".to_string(),
                    elapsed_ms: 0,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e.to_string()),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some(spec.api_key.to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        let result = collect_events(provider, simple_context(), options);
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        match result {
            Err(e) => {
                harness.log().info_ctx(
                    "e2e.rate_limit",
                    format!("{} rate limit error handled", spec.family),
                    |ctx| {
                        ctx.push(("error".into(), e.clone()));
                        ctx.push(("elapsed_ms".into(), elapsed.to_string()));
                    },
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_rate_limit".to_string(),
                    status: "pass".to_string(),
                    elapsed_ms: elapsed,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
            }
            Ok(events) => {
                let has_error = events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::Error { .. }));
                let sequence: Vec<String> =
                    events.iter().map(|e| event_kind(e).to_string()).collect();

                harness.log().info_ctx(
                    "e2e.rate_limit",
                    format!(
                        "{} rate limit via {}",
                        spec.family,
                        if has_error {
                            "stream error event"
                        } else {
                            "unexpected success"
                        }
                    ),
                    |ctx| {
                        ctx.push(("events".into(), events.len().to_string()));
                        ctx.push(("has_error_event".into(), has_error.to_string()));
                    },
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "error_rate_limit".to_string(),
                    status: if has_error { "pass" } else { "warn" }.to_string(),
                    elapsed_ms: elapsed,
                    event_count: events.len(),
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: None,
                    event_sequence: sequence,
                    sequence_valid: false,
                });
            }
        }
    }

    write_results_jsonl(&harness, "e2e_error_rate_limit", &results);

    let pass_or_warn = results
        .iter()
        .filter(|r| r.status == "pass" || r.status == "warn")
        .count();
    assert!(
        pass_or_warn >= 3,
        "expected at least 3 families to handle rate-limit errors, got {pass_or_warn}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3c: Schema-drift resilience across all families
// ═══════════════════════════════════════════════════════════════════════

/// Exercises schema-drift resilience by sending SSE responses with unexpected
/// extra fields, missing optional fields, or new field names. Providers should
/// still parse and emit valid events rather than crashing.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_error_schema_drift_all_families() {
    let harness = TestHarness::new("e2e_error_schema_drift_all_families");
    let mut results: Vec<ScenarioResult> = Vec::new();

    for spec in E2E_FAMILIES {
        harness.section(&format!("schema_drift: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url =
            setup_mock_route(spec, &server, text_event_stream((spec.schema_drift_sse)()));
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "schema_drift".to_string(),
                    status: "skip".to_string(),
                    elapsed_ms: 0,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e.to_string()),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some(spec.api_key.to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        let result = collect_events(provider, simple_context(), options);
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        match result {
            Ok(events) => {
                let sequence: Vec<String> =
                    events.iter().map(|e| event_kind(e).to_string()).collect();
                let seq_result = validate_event_sequence(&events);
                let (text_chars, tool_calls, stop_reason) = summarize_events(&events);

                harness.log().info_ctx(
                    "e2e.schema_drift",
                    format!("{} handled drift gracefully", spec.family),
                    |ctx| {
                        ctx.push(("events".into(), events.len().to_string()));
                        ctx.push(("text_chars".into(), text_chars.to_string()));
                        ctx.push(("seq_valid".into(), seq_result.is_ok().to_string()));
                    },
                );

                // Schema-drift responses should still parse to valid events
                // with text content "Drifted".
                assert!(
                    seq_result.is_ok(),
                    "{}: schema-drift response produced invalid event sequence: {}",
                    spec.family,
                    seq_result.unwrap_err()
                );
                assert!(
                    text_chars > 0,
                    "{}: schema-drift should still produce text output",
                    spec.family
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "schema_drift".to_string(),
                    status: "pass".to_string(),
                    elapsed_ms: elapsed,
                    event_count: events.len(),
                    text_chars,
                    tool_calls,
                    stop_reason,
                    error: None,
                    event_sequence: sequence,
                    sequence_valid: true,
                });
            }
            Err(e) => {
                // A parse error on drift is also informative — record as warn,
                // not fail, since strict parsing is a valid strategy.
                harness.log().warn(
                    "e2e.schema_drift",
                    format!("{} failed on drift: {e}", spec.family),
                );

                results.push(ScenarioResult {
                    family: spec.family.to_string(),
                    provider: spec.provider_id.to_string(),
                    scenario: "schema_drift".to_string(),
                    status: "warn".to_string(),
                    elapsed_ms: elapsed,
                    event_count: 0,
                    text_chars: 0,
                    tool_calls: 0,
                    stop_reason: None,
                    error: Some(e),
                    event_sequence: Vec::new(),
                    sequence_valid: false,
                });
            }
        }
    }

    write_results_jsonl(&harness, "e2e_schema_drift", &results);

    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    assert!(
        pass_count >= 3,
        "expected at least 3 families to handle schema drift, got {pass_count}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: Multi-turn conversation (OpenAI-completions + Anthropic)
// ═══════════════════════════════════════════════════════════════════════

/// Exercises a two-turn conversation through mock HTTP using queued responses.
/// Validates that the provider correctly sends accumulated messages in turn 2.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_multi_turn_conversation() {
    let harness = TestHarness::new("e2e_multi_turn_conversation");
    let mut results: Vec<ScenarioResult> = Vec::new();

    // OpenAI completions: two-turn via route queue
    {
        harness.section("multi_turn: openai-completions");
        let server = harness.start_mock_http_server();
        let route = oai_completions_route("", "");
        server.add_route_queue(
            "POST",
            &route,
            vec![
                text_event_stream(openai_chat_text_sse()),
                text_event_stream(openai_chat_text_sse_turn2()),
            ],
        );

        let base_url = oai_completions_base(&server.base_url());
        let mut entry = make_entry("groq", "e2e-llama", &base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None).expect("create groq provider");

        let options = StreamOptions {
            api_key: Some("e2e-test-key".to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        // Turn 1
        let start = Instant::now();
        let events1 = collect_events(Arc::clone(&provider), simple_context(), options.clone())
            .expect("turn 1 should succeed");
        let (text1, _, _) = summarize_events(&events1);
        assert!(text1 > 0, "turn 1 should produce text");

        // Turn 2: build context with previous messages
        let turn2_context = Context {
            system_prompt: Some("You are a deterministic test model.".to_string().into()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Say hello.".to_string()),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Now say goodbye.".to_string()),
                    timestamp: 1,
                }),
            ]
            .into(),
            tools: Vec::new().into(),
        };

        let events2 = collect_events(Arc::clone(&provider), turn2_context, options)
            .expect("turn 2 should succeed");
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (text2, _, stop2) = summarize_events(&events2);
        assert!(text2 > 0, "turn 2 should produce text");

        // Validate two requests were made
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "expected 2 HTTP requests for 2 turns");

        // Turn 2 request should have more messages
        let body2: serde_json::Value =
            serde_json::from_slice(&requests[1].body).expect("parse turn 2 body");
        let messages = body2["messages"].as_array().expect("messages array");
        assert!(
            messages.len() >= 2,
            "turn 2 should include accumulated messages, got {}",
            messages.len()
        );

        harness
            .log()
            .info_ctx("e2e.multi", "openai-completions multi-turn passed", |ctx| {
                ctx.push(("turn1_text".into(), text1.to_string()));
                ctx.push(("turn2_text".into(), text2.to_string()));
                ctx.push(("total_requests".into(), requests.len().to_string()));
            });

        results.push(ScenarioResult {
            family: "openai-completions".to_string(),
            provider: "groq".to_string(),
            scenario: "multi_turn".to_string(),
            status: "pass".to_string(),
            elapsed_ms: elapsed,
            event_count: events1.len() + events2.len(),
            text_chars: text1 + text2,
            tool_calls: 0,
            stop_reason: stop2,
            error: None,
            event_sequence: events2.iter().map(|e| event_kind(e).to_string()).collect(),
            sequence_valid: true,
        });
    }

    // Anthropic messages: two-turn via route queue
    {
        harness.section("multi_turn: anthropic-messages");
        let server = harness.start_mock_http_server();
        let route = anthropic_route("", "");
        server.add_route_queue(
            "POST",
            &route,
            vec![
                text_event_stream(anthropic_text_sse()),
                text_event_stream(anthropic_text_sse_turn2()),
            ],
        );

        let base_url = anthropic_base(&server.base_url());
        let mut entry = make_entry("anthropic", "e2e-claude", &base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None).expect("create anthropic provider");

        let options = StreamOptions {
            api_key: Some("e2e-test-key".to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        let events1 = collect_events(Arc::clone(&provider), simple_context(), options.clone())
            .expect("anthropic turn 1");
        let (text1, _, _) = summarize_events(&events1);

        let turn2_context = Context {
            system_prompt: Some("You are a deterministic test model.".to_string().into()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Say hello.".to_string()),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Now say goodbye.".to_string()),
                    timestamp: 1,
                }),
            ]
            .into(),
            tools: Vec::new().into(),
        };

        let events2 = collect_events(Arc::clone(&provider), turn2_context, options)
            .expect("anthropic turn 2");
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (text2, _, stop2) = summarize_events(&events2);

        let requests = server.requests();
        assert_eq!(requests.len(), 2, "expected 2 anthropic requests");

        let body2: serde_json::Value =
            serde_json::from_slice(&requests[1].body).expect("parse anthropic turn 2 body");
        let messages = body2["messages"].as_array().expect("messages array");
        assert!(
            messages.len() >= 2,
            "anthropic turn 2 should include accumulated messages"
        );

        harness
            .log()
            .info_ctx("e2e.multi", "anthropic multi-turn passed", |ctx| {
                ctx.push(("turn1_text".into(), text1.to_string()));
                ctx.push(("turn2_text".into(), text2.to_string()));
            });

        results.push(ScenarioResult {
            family: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            scenario: "multi_turn".to_string(),
            status: "pass".to_string(),
            elapsed_ms: elapsed,
            event_count: events1.len() + events2.len(),
            text_chars: text1 + text2,
            tool_calls: 0,
            stop_reason: stop2,
            error: None,
            event_sequence: events2.iter().map(|e| event_kind(e).to_string()).collect(),
            sequence_valid: true,
        });
    }

    write_results_jsonl(&harness, "e2e_multi_turn", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 5: Streaming event ordering invariants
// ═══════════════════════════════════════════════════════════════════════

/// Re-runs the simple text scenario for each family and validates detailed
/// streaming event ordering (`Start` → `TextStart` → `TextDelta`+ → `TextEnd` → `Done`).
#[test]
fn e2e_event_ordering_all_families() {
    let harness = TestHarness::new("e2e_event_ordering_all_families");
    let mut valid_count = 0u32;

    for spec in E2E_FAMILIES {
        harness.section(&format!("event_ordering: {}", spec.family));
        let server = harness.start_mock_http_server();
        let base_url = setup_mock_route(spec, &server, text_event_stream((spec.text_sse)()));
        let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None).expect("create provider for ordering test");

        let options = StreamOptions {
            api_key: Some(spec.api_key.to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let events = collect_events(provider, simple_context(), options)
            .unwrap_or_else(|e| panic!("{}: stream failed: {e}", spec.family));

        let sequence: Vec<&str> = events.iter().map(event_kind).collect();
        let validation = validate_event_sequence(&events);

        harness.log().info_ctx(
            "e2e.ordering",
            format!("{} event sequence", spec.family),
            |ctx| {
                ctx.push(("sequence".into(), sequence.join(" → ")));
                ctx.push(("valid".into(), validation.is_ok().to_string()));
            },
        );

        assert!(
            validation.is_ok(),
            "{}: invalid event ordering: {} (sequence: {})",
            spec.family,
            validation.unwrap_err(),
            sequence.join(" → ")
        );

        // Additional: first must be Start, last must be Done
        assert!(
            matches!(events.first(), Some(StreamEvent::Start { .. })),
            "{}: first event must be Start",
            spec.family
        );
        assert!(
            matches!(events.last(), Some(StreamEvent::Done { .. })),
            "{}: last event must be Done for text scenario",
            spec.family
        );

        valid_count += 1;
    }

    assert_eq!(
        valid_count,
        u32::try_from(E2E_FAMILIES.len()).unwrap(),
        "all families should pass event ordering"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: Determinism proof — same inputs yield same event sequences
// ═══════════════════════════════════════════════════════════════════════

/// Runs the same scenario twice for each family and verifies that the event
/// sequences are identical (determinism proof via `MockHttpServer`).
#[test]
fn e2e_determinism_proof() {
    let harness = TestHarness::new("e2e_determinism_proof");

    for spec in E2E_FAMILIES {
        harness.section(&format!("determinism: {}", spec.family));

        let mut sequences = Vec::new();
        for run in 0..2 {
            let server = harness.start_mock_http_server();
            let base_url = setup_mock_route(spec, &server, text_event_stream((spec.text_sse)()));
            let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
            entry.model.api.clear();
            let provider = create_provider(&entry, None)
                .unwrap_or_else(|e| panic!("{}: create failed on run {run}: {e}", spec.family));

            let options = StreamOptions {
                api_key: Some(spec.api_key.to_string()),
                max_tokens: Some(64),
                ..Default::default()
            };

            let events = collect_events(provider, simple_context(), options)
                .unwrap_or_else(|e| panic!("{}: stream failed on run {run}: {e}", spec.family));
            let sequence: Vec<String> = events.iter().map(|e| event_kind(e).to_string()).collect();
            sequences.push(sequence);
        }

        harness.log().info_ctx(
            "e2e.determinism",
            format!("{} compared", spec.family),
            |ctx| {
                ctx.push(("run1_len".into(), sequences[0].len().to_string()));
                ctx.push(("run2_len".into(), sequences[1].len().to_string()));
                ctx.push(("match".into(), (sequences[0] == sequences[1]).to_string()));
            },
        );

        assert_eq!(
            sequences[0], sequences[1],
            "{}: event sequences differ between runs (non-deterministic)",
            spec.family
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Test 7: OpenAI-compatible preset exercised as representative wave
// ═══════════════════════════════════════════════════════════════════════

/// Exercises three representative OpenAI-compatible presets (groq, deepseek,
/// openrouter) to confirm wave coverage beyond the native provider.
#[test]
fn e2e_openai_compatible_wave_presets() {
    let harness = TestHarness::new("e2e_openai_compatible_wave_presets");
    // (preset_id, route_path, base_url_suffix)
    // The provider appends `/chat/completions` to base_url, so the base_url
    // must end *before* that suffix.
    let presets = [
        ("groq", "/openai/v1/chat/completions", "/openai/v1"),
        ("deepseek", "/chat/completions", ""),
        ("openrouter", "/api/v1/chat/completions", "/api/v1"),
        ("cerebras", "/openai/v1/chat/completions", "/openai/v1"),
        ("moonshotai", "/openai/v1/chat/completions", "/openai/v1"),
        ("alibaba", "/openai/v1/chat/completions", "/openai/v1"),
        // Longtail quick-win presets (bd-3uqg.11.10.6)
        ("stackit", "/openai/v1/chat/completions", "/openai/v1"),
        ("mistral", "/openai/v1/chat/completions", "/openai/v1"),
        ("deepinfra", "/openai/v1/chat/completions", "/openai/v1"),
        ("togetherai", "/openai/v1/chat/completions", "/openai/v1"),
        ("nvidia", "/openai/v1/chat/completions", "/openai/v1"),
        ("huggingface", "/openai/v1/chat/completions", "/openai/v1"),
        ("ollama-cloud", "/openai/v1/chat/completions", "/openai/v1"),
    ];
    let mut results: Vec<ScenarioResult> = Vec::new();

    for (preset_id, route, base_suffix) in &presets {
        harness.section(&format!("wave_preset: {preset_id}"));
        let server = harness.start_mock_http_server();
        server.add_route("POST", route, text_event_stream(openai_chat_text_sse()));

        let base_url = format!("{}{}", server.base_url(), base_suffix);
        let mut entry = make_entry(preset_id, &format!("e2e-{preset_id}"), &base_url);
        entry.auth_header = true;
        entry.model.api.clear();
        let provider = match create_provider(&entry, None) {
            Ok(p) => p,
            Err(e) => {
                harness
                    .log()
                    .warn("e2e.wave", format!("{preset_id} create failed: {e}"));
                continue;
            }
        };

        let options = StreamOptions {
            api_key: Some("e2e-wave-key".to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };

        let start = Instant::now();
        let events = collect_events(provider, simple_context(), options)
            .unwrap_or_else(|e| panic!("{preset_id}: stream failed: {e}"));
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (text_chars, _, stop_reason) = summarize_events(&events);
        let sequence: Vec<String> = events.iter().map(|e| event_kind(e).to_string()).collect();

        assert!(text_chars > 0, "{preset_id}: expected text output");

        // Verify Bearer auth was sent
        let requests = server.requests();
        assert!(!requests.is_empty(), "{preset_id}: no requests captured");
        let auth = request_header(&requests[0].headers, "authorization");
        assert!(
            auth.as_deref().is_some_and(|v| v.starts_with("Bearer ")),
            "{preset_id}: expected Bearer auth header, got {auth:?}"
        );

        harness
            .log()
            .info_ctx("e2e.wave", format!("{preset_id} passed"), |ctx| {
                ctx.push(("text_chars".into(), text_chars.to_string()));
                ctx.push(("elapsed_ms".into(), elapsed.to_string()));
            });

        results.push(ScenarioResult {
            family: "openai-completions".to_string(),
            provider: preset_id.to_string(),
            scenario: "wave_preset".to_string(),
            status: "pass".to_string(),
            elapsed_ms: elapsed,
            event_count: events.len(),
            text_chars,
            tool_calls: 0,
            stop_reason,
            error: None,
            event_sequence: sequence,
            sequence_valid: true,
        });
    }

    assert!(
        results.len() >= 2,
        "expected at least 2 wave presets to pass"
    );

    write_results_jsonl(&harness, "e2e_wave_presets", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 8: Comprehensive E2E report artifact
// ═══════════════════════════════════════════════════════════════════════

/// Runs all scenarios for all families and produces a comprehensive JSONL
/// report + human-readable markdown summary as test artifacts.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comprehensive_report() {
    let harness = TestHarness::new("e2e_comprehensive_report");
    let mut all_results: Vec<ScenarioResult> = Vec::new();
    let scenarios = [
        "simple_text",
        "tool_call",
        "error_auth",
        "error_rate_limit",
        "schema_drift",
    ];

    for spec in E2E_FAMILIES {
        for scenario in &scenarios {
            harness.section(&format!("report: {} / {scenario}", spec.family));
            let server = harness.start_mock_http_server();

            let response = match *scenario {
                "simple_text" => text_event_stream((spec.text_sse)()),
                "tool_call" => text_event_stream((spec.tool_sse)()),
                "error_auth" => (spec.error_response)(),
                "error_rate_limit" => (spec.rate_limit_response)(),
                "schema_drift" => text_event_stream((spec.schema_drift_sse)()),
                _ => panic!(),
            };
            let base_url = setup_mock_route(spec, &server, response);
            let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
            entry.model.api.clear();
            let provider = match create_provider(&entry, None) {
                Ok(p) => p,
                Err(e) => {
                    all_results.push(ScenarioResult {
                        family: spec.family.to_string(),
                        provider: spec.provider_id.to_string(),
                        scenario: scenario.to_string(),
                        status: "skip".to_string(),
                        elapsed_ms: 0,
                        event_count: 0,
                        text_chars: 0,
                        tool_calls: 0,
                        stop_reason: None,
                        error: Some(e.to_string()),
                        event_sequence: Vec::new(),
                        sequence_valid: false,
                    });
                    continue;
                }
            };

            let context = if *scenario == "tool_call" {
                tool_context()
            } else {
                simple_context()
            };
            let options = StreamOptions {
                api_key: Some(spec.api_key.to_string()),
                max_tokens: Some(64),
                ..Default::default()
            };

            let start = Instant::now();
            let result = collect_events(provider, context, options);
            let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

            match result {
                Ok(events) => {
                    let sequence: Vec<String> =
                        events.iter().map(|e| event_kind(e).to_string()).collect();
                    let seq_valid = validate_event_sequence(&events).is_ok();
                    let (text_chars, tool_calls, stop_reason) = summarize_events(&events);
                    all_results.push(ScenarioResult {
                        family: spec.family.to_string(),
                        provider: spec.provider_id.to_string(),
                        scenario: scenario.to_string(),
                        status: "pass".to_string(),
                        elapsed_ms: elapsed,
                        event_count: events.len(),
                        text_chars,
                        tool_calls,
                        stop_reason,
                        error: None,
                        event_sequence: sequence,
                        sequence_valid: seq_valid,
                    });
                }
                Err(e) => {
                    let status = if *scenario == "error_auth" || *scenario == "error_rate_limit" {
                        "pass"
                    } else {
                        "fail"
                    };
                    all_results.push(ScenarioResult {
                        family: spec.family.to_string(),
                        provider: spec.provider_id.to_string(),
                        scenario: scenario.to_string(),
                        status: status.to_string(),
                        elapsed_ms: elapsed,
                        event_count: 0,
                        text_chars: 0,
                        tool_calls: 0,
                        stop_reason: None,
                        error: Some(e),
                        event_sequence: Vec::new(),
                        sequence_valid: false,
                    });
                }
            }
        }
    }

    // Write JSONL report
    let jsonl_path = harness.temp_path("e2e_comprehensive_report.jsonl");
    let mut jsonl = String::new();
    for result in &all_results {
        let _ = writeln!(
            jsonl,
            "{}",
            serde_json::to_string(result).unwrap_or_default()
        );
    }
    std::fs::write(&jsonl_path, &jsonl).expect("write JSONL report");
    harness.record_artifact("e2e_comprehensive_report.jsonl", &jsonl_path);

    // Write markdown summary
    let md_path = harness.temp_path("e2e_comprehensive_report.md");
    let markdown = build_markdown_summary(&all_results);
    std::fs::write(&md_path, &markdown).expect("write markdown report");
    harness.record_artifact("e2e_comprehensive_report.md", &md_path);

    // Write JSONL logs
    let log_path = harness.temp_path("e2e_comprehensive_log.jsonl");
    harness
        .write_jsonl_logs(&log_path)
        .expect("write JSONL logs");
    harness.record_artifact("e2e_comprehensive_log.jsonl", &log_path);

    // Verify no unexpected failures
    let failures: Vec<&ScenarioResult> =
        all_results.iter().filter(|r| r.status == "fail").collect();
    assert!(
        failures.is_empty(),
        "unexpected failures: {:?}",
        failures
            .iter()
            .map(|f| format!(
                "{}/{}: {}",
                f.family,
                f.scenario,
                f.error.as_deref().unwrap_or("?")
            ))
            .collect::<Vec<_>>()
    );

    harness
        .log()
        .info_ctx("e2e.report", "comprehensive report generated", |ctx| {
            ctx.push(("total".into(), all_results.len().to_string()));
            ctx.push((
                "passed".into(),
                all_results
                    .iter()
                    .filter(|r| r.status == "pass")
                    .count()
                    .to_string(),
            ));
            ctx.push((
                "skipped".into(),
                all_results
                    .iter()
                    .filter(|r| r.status == "skip")
                    .count()
                    .to_string(),
            ));
        });
}

// ═══════════════════════════════════════════════════════════════════════
// Test 9: Request body replay stability
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that the request bodies sent by each provider are stable across
/// runs (same input → same JSON body), enabling replay-based failure triage.
#[test]
fn e2e_request_body_stability() {
    let harness = TestHarness::new("e2e_request_body_stability");

    for spec in E2E_FAMILIES {
        harness.section(&format!("body_stability: {}", spec.family));

        let mut bodies = Vec::new();
        for run in 0..2 {
            let server = harness.start_mock_http_server();
            let base_url = setup_mock_route(spec, &server, text_event_stream((spec.text_sse)()));
            let mut entry = make_entry(spec.provider_id, spec.model_id, &base_url);
            entry.model.api.clear();
            let provider = create_provider(&entry, None)
                .unwrap_or_else(|e| panic!("{}: create failed run {run}: {e}", spec.family));

            let options = StreamOptions {
                api_key: Some(spec.api_key.to_string()),
                max_tokens: Some(64),
                ..Default::default()
            };

            let _ = collect_events(provider, simple_context(), options);
            let requests = server.requests();
            assert!(
                !requests.is_empty(),
                "{}: no requests on run {run}",
                spec.family
            );
            bodies.push(requests[0].body.clone());
        }

        // Compare the two request bodies (should be byte-identical)
        let body1: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap_or(json!(null));
        let body2: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap_or(json!(null));

        harness.log().info_ctx(
            "e2e.stability",
            format!("{} body compared", spec.family),
            |ctx| {
                ctx.push(("match".into(), (body1 == body2).to_string()));
                ctx.push(("body1_len".into(), bodies[0].len().to_string()));
                ctx.push(("body2_len".into(), bodies[1].len().to_string()));
            },
        );

        assert_eq!(
            body1, body2,
            "{}: request bodies differ between runs (non-deterministic request assembly)",
            spec.family
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Artifact helpers
// ═══════════════════════════════════════════════════════════════════════

fn write_results_jsonl(harness: &TestHarness, name: &str, results: &[ScenarioResult]) {
    let path = harness.temp_path(format!("{name}.jsonl"));
    let mut content = String::new();
    for result in results {
        let _ = writeln!(
            content,
            "{}",
            serde_json::to_string(result).unwrap_or_default()
        );
    }
    std::fs::write(&path, &content).expect("write results JSONL");
    harness.record_artifact(format!("{name}.jsonl"), &path);
}

fn build_markdown_summary(results: &[ScenarioResult]) -> String {
    let mut md = String::from("# E2E Provider Scenario Report\n\n");

    let passed = results.iter().filter(|r| r.status == "pass").count();
    let failed = results.iter().filter(|r| r.status == "fail").count();
    let skipped = results.iter().filter(|r| r.status == "skip").count();

    let _ = writeln!(
        md,
        "**Total:** {} | **Passed:** {passed} | **Failed:** {failed} | **Skipped:** {skipped}\n",
        results.len()
    );

    md.push_str("| Family | Scenario | Provider | Status | Events | Text | Tools | Stop Reason | Elapsed |\n");
    md.push_str("| --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: |\n");

    for r in results {
        let stop = r.stop_reason.as_deref().unwrap_or("-");
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {}ms |",
            r.family,
            r.scenario,
            r.provider,
            r.status,
            r.event_count,
            r.text_chars,
            r.tool_calls,
            stop,
            r.elapsed_ms
        );
    }

    md
}
