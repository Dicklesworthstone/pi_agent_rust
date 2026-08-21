//! Native adapter contract tests (bd-3uqg.8.2).
//!
//! For each native provider integration, these tests validate:
//! 1. **Request payload shape**: correct URL path, Content-Type, body structure
//! 2. **Auth header construction**: Bearer vs X-API-Key vs provider-specific
//! 3. **Tool-schema translation**: internal `ToolDef` → provider-specific wire format
//! 4. **Response event decoding**: provider SSE → internal `StreamEvent` variants
//!
//! All tests run with mock HTTP transports so failures isolate adapter logic
//! rather than network behavior.

mod common;

use common::harness::MockHttpRequest;
use common::{MockHttpResponse, TestHarness};
use futures::StreamExt;
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamOptions, ToolDef};
use pi::providers::create_provider;
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// Helpers
// ============================================================================

fn make_model_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} {model_id}"),
            api: String::new(), // let factory infer
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
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
        // Real registry entries for these providers derive auth_header=true from
        // routing defaults, and create_provider honors the flag; mirror that.
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

fn make_model_entry_with_api(
    provider: &str,
    model_id: &str,
    base_url: &str,
    api: &str,
) -> ModelEntry {
    let mut entry = make_model_entry(provider, model_id, base_url);
    entry.model.api = api.to_string();
    entry
}

fn text_event_stream_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn request_header(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone())
}

fn request_body_json(request: &MockHttpRequest) -> serde_json::Value {
    serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null)
}

fn simple_context() -> Context<'static> {
    Context::owned(
        Some("You are a test assistant.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn context_with_tools() -> Context<'static> {
    Context::owned(
        Some("You are a test assistant.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Use the echo tool".to_string()),
            timestamp: 0,
        })],
        vec![
            ToolDef {
                name: "echo".to_string(),
                description: "Echoes text back".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "The text to echo"
                        }
                    },
                    "required": ["text"]
                }),
            },
            ToolDef {
                name: "calculate".to_string(),
                description: "Performs arithmetic".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "expression": {
                            "type": "string",
                            "description": "Math expression to evaluate"
                        }
                    },
                    "required": ["expression"]
                }),
            },
        ],
    )
}

fn options_with_key(key: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(key.to_string()),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// Drive a provider stream to Done and collect all events.
fn collect_stream_events(
    provider: Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) -> Vec<StreamEvent> {
    common::run_async(async move {
        let mut stream = provider
            .stream(&context, &options)
            .await
            .expect("provider stream should start");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.expect("stream event");
            let is_done = matches!(event, StreamEvent::Done { .. });
            events.push(event);
            if is_done {
                break;
            }
        }
        events
    })
}

// ============================================================================
// SSE body generators for each provider API
// ============================================================================

fn anthropic_simple_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_ct_001","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello there!"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_tool_call_sse() -> String {
    [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_ct_002","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":15,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_ct_001","name":"echo","input":{}}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"contract test\"}"}}"#,
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

fn openai_simple_sse() -> String {
    [
        r#"data: {"id":"chatcmpl-ct001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"chatcmpl-ct001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"content":" there!"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"chatcmpl-ct001","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn openai_tool_call_sse() -> String {
    [
        r#"data: {"id":"chatcmpl-ct002","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"id":"call_ct_001","index":0,"type":"function","function":{"name":"echo","arguments":""}}]},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"chatcmpl-ct002","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"text\":\"contract test\"}"}}]},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"chatcmpl-ct002","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":15,"completion_tokens":12,"total_tokens":27}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn cohere_simple_sse() -> String {
    [
        r"event: message-start",
        r#"data: {"id":"ct-cohere-001","type":"message-start","delta":{"message":{"role":"assistant","content":[]}}}"#,
        "",
        r"event: content-start",
        r#"data: {"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}"#,
        "",
        r"event: content-delta",
        r#"data: {"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"Hello there!"}}}}"#,
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

fn gemini_simple_sse() -> String {
    // Gemini uses JSON streaming with generateContent endpoint
    [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello there!"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        "",
    ]
    .join("\n")
}

// ============================================================================
// ANTHROPIC CONTRACT TESTS
// ============================================================================

mod anthropic_contract {
    use super::*;

    #[test]
    fn request_payload_shape() {
        let harness = TestHarness::new("anthropic_contract_request_payload_shape");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/messages";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(anthropic_simple_sse()),
        );

        let entry = make_model_entry(
            "anthropic",
            "claude-test",
            &format!("{}{endpoint}", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create anthropic");

        let context = simple_context();
        let options = options_with_key("sk-ant-test-key");
        collect_stream_events(provider, context, options);

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let req = &requests[0];

        // Path correctness
        assert_eq!(req.path, endpoint);
        assert_eq!(req.method, "POST");

        // Content-Type
        assert_eq!(
            request_header(&req.headers, "content-type").as_deref(),
            Some("application/json")
        );

        // Body structure
        let body = request_body_json(req);
        assert!(body.get("model").is_some(), "body must contain 'model'");
        assert_eq!(body["model"], "claude-test");
        assert!(
            body.get("messages").is_some(),
            "body must contain 'messages'"
        );
        assert!(body["messages"].is_array(), "messages must be an array");
        assert!(
            body.get("max_tokens").is_some(),
            "body must contain 'max_tokens'"
        );
        assert!(body.get("stream").is_some(), "body must contain 'stream'");
        assert_eq!(body["stream"], true, "stream must be true");

        // System prompt handling (Anthropic uses top-level 'system' field)
        assert!(
            body.get("system").is_some(),
            "body must contain 'system' for system prompt"
        );

        harness
            .log()
            .info_ctx("contract", "anthropic request payload validated", |ctx| {
                ctx.push(("model".to_string(), body["model"].to_string()));
                ctx.push((
                    "messages_count".to_string(),
                    body["messages"].as_array().map_or(0, Vec::len).to_string(),
                ));
            });
    }

    #[test]
    fn auth_header_x_api_key() {
        let harness = TestHarness::new("anthropic_contract_auth_header");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/messages";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(anthropic_simple_sse()),
        );

        let entry = make_model_entry(
            "anthropic",
            "claude-test",
            &format!("{}{endpoint}", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create anthropic");

        let context = simple_context();
        let options = options_with_key("sk-ant-contract-test");
        collect_stream_events(provider, context, options);

        let req = &server.requests()[0];

        // Anthropic uses x-api-key header, NOT Bearer auth
        assert_eq!(
            request_header(&req.headers, "x-api-key").as_deref(),
            Some("sk-ant-contract-test"),
            "Anthropic must use x-api-key header"
        );

        // Anthropic-specific headers
        assert!(
            request_header(&req.headers, "anthropic-version").is_some(),
            "Anthropic must send anthropic-version header"
        );

        harness
            .log()
            .info("contract", "anthropic auth headers validated");
    }

    #[test]
    fn tool_schema_translation() {
        let harness = TestHarness::new("anthropic_contract_tool_schema");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/messages";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(anthropic_tool_call_sse()),
        );

        let entry = make_model_entry(
            "anthropic",
            "claude-test",
            &format!("{}{endpoint}", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create anthropic");

        let context = context_with_tools();
        let options = options_with_key("sk-ant-tool-test");
        collect_stream_events(provider, context, options);

        let req = &server.requests()[0];
        let body = request_body_json(req);

        // Anthropic tool format: { "name", "description", "input_schema" }
        let tools = body["tools"].as_array().expect("tools must be an array");
        assert_eq!(tools.len(), 2, "expected 2 tools");

        let echo_tool = &tools[0];
        assert_eq!(echo_tool["name"], "echo");
        assert_eq!(echo_tool["description"], "Echoes text back");
        // Anthropic uses "input_schema" (not "parameters")
        assert!(
            echo_tool.get("input_schema").is_some(),
            "Anthropic tools must use 'input_schema' key"
        );
        let schema = &echo_tool["input_schema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["text"].is_object());

        harness
            .log()
            .info_ctx("contract", "anthropic tool schema validated", |ctx| {
                ctx.push(("tool_count".to_string(), tools.len().to_string()));
            });
    }

    #[test]
    fn response_event_decoding_text() {
        let harness = TestHarness::new("anthropic_contract_response_decoding_text");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/messages";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(anthropic_simple_sse()),
        );

        let entry = make_model_entry(
            "anthropic",
            "claude-test",
            &format!("{}{endpoint}", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create anthropic");

        let events = collect_stream_events(provider, simple_context(), options_with_key("test"));

        // Must contain text delta and Done events
        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(
            has_text,
            "Anthropic text stream must produce TextDelta events"
        );
        assert!(has_done, "Anthropic text stream must end with Done event");

        harness
            .log()
            .info_ctx("contract", "anthropic response decoding validated", |ctx| {
                ctx.push(("event_count".to_string(), events.len().to_string()));
            });
    }

    #[test]
    fn response_event_decoding_tool_call() {
        let harness = TestHarness::new("anthropic_contract_response_decoding_tool_call");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/messages";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(anthropic_tool_call_sse()),
        );

        let entry = make_model_entry(
            "anthropic",
            "claude-test",
            &format!("{}{endpoint}", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create anthropic");

        let events =
            collect_stream_events(provider, context_with_tools(), options_with_key("test"));

        let has_tool_call = events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(
            has_tool_call,
            "Anthropic tool stream must produce ToolCallStart events"
        );
        assert!(has_done, "Anthropic tool stream must end with Done event");

        harness.log().info_ctx(
            "contract",
            "anthropic tool call decoding validated",
            |ctx| {
                ctx.push(("event_count".to_string(), events.len().to_string()));
            },
        );
    }
}

// ============================================================================
// OPENAI (Chat Completions) CONTRACT TESTS
// ============================================================================

mod openai_contract {
    use super::*;

    #[test]
    fn request_payload_shape() {
        let harness = TestHarness::new("openai_contract_request_payload_shape");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            "openai",
            "gpt-test",
            &format!("{}{endpoint}", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None).expect("create openai");

        let context = simple_context();
        let options = options_with_key("sk-openai-test-key");
        collect_stream_events(provider, context, options);

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let req = &requests[0];

        assert_eq!(req.path, endpoint);
        assert_eq!(req.method, "POST");

        let body = request_body_json(req);
        assert_eq!(body["model"], "gpt-test");
        assert!(body["messages"].is_array());
        assert_eq!(body["stream"], true);

        // OpenAI uses messages array with role/content objects
        let messages = body["messages"].as_array().unwrap();
        // System prompt should be in messages (OpenAI style)
        let has_system = messages.iter().any(|m| m["role"] == "system");
        let has_user = messages.iter().any(|m| m["role"] == "user");
        assert!(
            has_system,
            "OpenAI must include system message in messages array"
        );
        assert!(has_user, "OpenAI must include user message");

        harness
            .log()
            .info("contract", "openai request payload validated");
    }

    #[test]
    fn auth_header_bearer() {
        let harness = TestHarness::new("openai_contract_auth_header");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            "openai",
            "gpt-test",
            &format!("{}{endpoint}", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None).expect("create openai");

        collect_stream_events(
            provider,
            simple_context(),
            options_with_key("sk-openai-contract"),
        );

        let req = &server.requests()[0];

        // OpenAI uses Bearer token
        assert_eq!(
            request_header(&req.headers, "authorization").as_deref(),
            Some("Bearer sk-openai-contract"),
            "OpenAI must use Bearer auth"
        );

        harness
            .log()
            .info("contract", "openai auth header validated");
    }

    #[test]
    fn tool_schema_translation() {
        let harness = TestHarness::new("openai_contract_tool_schema");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_tool_call_sse()),
        );

        let entry = make_model_entry_with_api(
            "openai",
            "gpt-test",
            &format!("{}{endpoint}", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None).expect("create openai");

        collect_stream_events(provider, context_with_tools(), options_with_key("test"));

        let req = &server.requests()[0];
        let body = request_body_json(req);

        // OpenAI tool format: { "type": "function", "function": { "name", "description", "parameters" } }
        let tools = body["tools"].as_array().expect("tools must be an array");
        assert_eq!(tools.len(), 2, "expected 2 tools");

        let echo_tool = &tools[0];
        assert_eq!(echo_tool["type"], "function");
        assert_eq!(echo_tool["function"]["name"], "echo");
        assert_eq!(echo_tool["function"]["description"], "Echoes text back");
        // OpenAI uses "parameters" (not "input_schema")
        assert!(
            echo_tool["function"].get("parameters").is_some(),
            "OpenAI tools must use 'parameters' key inside 'function'"
        );

        harness
            .log()
            .info("contract", "openai tool schema validated");
    }

    #[test]
    fn response_event_decoding_text() {
        let harness = TestHarness::new("openai_contract_response_decoding_text");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            "openai",
            "gpt-test",
            &format!("{}{endpoint}", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None).expect("create openai");

        let events = collect_stream_events(provider, simple_context(), options_with_key("test"));

        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(has_text, "OpenAI text stream must produce Text events");
        assert!(has_done, "OpenAI text stream must end with Done event");

        harness
            .log()
            .info("contract", "openai response decoding validated");
    }

    #[test]
    fn response_event_decoding_tool_call() {
        let harness = TestHarness::new("openai_contract_response_decoding_tool_call");
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_tool_call_sse()),
        );

        let entry = make_model_entry_with_api(
            "openai",
            "gpt-test",
            &format!("{}{endpoint}", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None).expect("create openai");

        let events =
            collect_stream_events(provider, context_with_tools(), options_with_key("test"));

        let has_tool_call = events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(
            has_tool_call,
            "OpenAI tool stream must produce ToolCallStart events"
        );
        assert!(has_done, "OpenAI tool stream must end with Done event");

        harness
            .log()
            .info("contract", "openai tool call decoding validated");
    }
}

// ============================================================================
// GEMINI CONTRACT TESTS
// ============================================================================

mod gemini_contract {
    use super::*;

    /// Gemini uses `?alt=sse` on the path and sends the key in `x-goog-api-key`.
    /// The mock server
    /// matches on the full request-line path (including query string), so the
    /// registered route must include the expected `alt=sse` parameter.
    fn gemini_route() -> String {
        "/v1beta/models/gemini-test:streamGenerateContent?alt=sse".to_string()
    }

    #[test]
    fn request_payload_shape() {
        let harness = TestHarness::new("gemini_contract_request_payload_shape");
        let server = harness.start_mock_http_server();
        let api_key = "gemini-test-key";
        let endpoint = gemini_route();

        server.add_route(
            "POST",
            &endpoint,
            text_event_stream_response(gemini_simple_sse()),
        );

        let entry = make_model_entry(
            "google",
            "gemini-test",
            &format!("{}/v1beta", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create gemini");

        let context = simple_context();
        let options = options_with_key(api_key);
        collect_stream_events(provider, context, options);

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let req = &requests[0];

        assert_eq!(req.method, "POST");
        // Gemini endpoint should include model name in path
        assert!(
            req.path.contains("gemini-test"),
            "Gemini request path must contain model name"
        );

        let body = request_body_json(req);
        // Gemini uses 'contents' instead of 'messages'
        assert!(
            body.get("contents").is_some(),
            "Gemini body must contain 'contents'"
        );

        harness
            .log()
            .info("contract", "gemini request payload validated");
    }

    #[test]
    fn auth_via_header() {
        let harness = TestHarness::new("gemini_contract_auth_header");
        let server = harness.start_mock_http_server();
        let api_key = "gemini-key-test";
        let endpoint = gemini_route();

        server.add_route(
            "POST",
            &endpoint,
            text_event_stream_response(gemini_simple_sse()),
        );

        let entry = make_model_entry(
            "google",
            "gemini-test",
            &format!("{}/v1beta", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create gemini");

        collect_stream_events(provider, simple_context(), options_with_key(api_key));

        let req = &server.requests()[0];

        assert_eq!(
            request_header(&req.headers, "x-goog-api-key").as_deref(),
            Some(api_key),
            "Gemini must pass API key as x-goog-api-key"
        );

        harness.log().info("contract", "gemini auth validated");
    }

    #[test]
    fn tool_schema_translation() {
        let harness = TestHarness::new("gemini_contract_tool_schema");
        let server = harness.start_mock_http_server();
        let api_key = "gemini-tool-test";
        let endpoint = gemini_route();

        server.add_route(
            "POST",
            &endpoint,
            text_event_stream_response(gemini_simple_sse()),
        );

        let entry = make_model_entry(
            "google",
            "gemini-test",
            &format!("{}/v1beta", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create gemini");

        collect_stream_events(provider, context_with_tools(), options_with_key(api_key));

        let req = &server.requests()[0];
        let body = request_body_json(req);

        // Gemini uses 'tools' array with 'function_declarations'
        assert!(
            body.get("tools").is_some(),
            "Gemini body must contain 'tools'"
        );
        let tools = body["tools"].as_array().expect("tools must be an array");
        assert!(!tools.is_empty(), "tools must not be empty");

        // Gemini wraps functions in functionDeclarations
        let first_tool = &tools[0];
        assert!(
            first_tool.get("function_declarations").is_some()
                || first_tool.get("functionDeclarations").is_some(),
            "Gemini tools must contain function_declarations or functionDeclarations"
        );

        harness
            .log()
            .info("contract", "gemini tool schema validated");
    }

    #[test]
    fn response_event_decoding_text() {
        let harness = TestHarness::new("gemini_contract_response_decoding_text");
        let server = harness.start_mock_http_server();
        let api_key = "gemini-decode-test";
        let endpoint = gemini_route();

        server.add_route(
            "POST",
            &endpoint,
            text_event_stream_response(gemini_simple_sse()),
        );

        let entry = make_model_entry(
            "google",
            "gemini-test",
            &format!("{}/v1beta", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create gemini");

        let events = collect_stream_events(provider, simple_context(), options_with_key(api_key));

        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(has_text, "Gemini text stream must produce Text events");
        assert!(has_done, "Gemini text stream must end with Done event");

        harness
            .log()
            .info("contract", "gemini response decoding validated");
    }
}

// ============================================================================
// COHERE CONTRACT TESTS
// ============================================================================

mod cohere_contract {
    use super::*;

    #[test]
    fn request_payload_shape() {
        let harness = TestHarness::new("cohere_contract_request_payload_shape");
        let server = harness.start_mock_http_server();
        let endpoint = "/v2/chat";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(cohere_simple_sse()),
        );

        let entry = make_model_entry(
            "cohere",
            "command-r-test",
            &format!("{}/v2", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create cohere");

        let context = simple_context();
        let options = options_with_key("cohere-test-key");
        collect_stream_events(provider, context, options);

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let req = &requests[0];

        assert_eq!(req.method, "POST");

        let body = request_body_json(req);
        assert_eq!(body["model"], "command-r-test");
        assert_eq!(body["stream"], true);
        // Cohere uses 'messages' array
        assert!(
            body.get("messages").is_some(),
            "Cohere body must contain 'messages'"
        );

        harness
            .log()
            .info("contract", "cohere request payload validated");
    }

    #[test]
    fn auth_header_bearer() {
        let harness = TestHarness::new("cohere_contract_auth_header");
        let server = harness.start_mock_http_server();
        let endpoint = "/v2/chat";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(cohere_simple_sse()),
        );

        let entry = make_model_entry(
            "cohere",
            "command-r-test",
            &format!("{}/v2", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create cohere");

        collect_stream_events(
            provider,
            simple_context(),
            options_with_key("cohere-bearer-test"),
        );

        let req = &server.requests()[0];

        // Cohere uses Bearer token
        assert_eq!(
            request_header(&req.headers, "authorization").as_deref(),
            Some("Bearer cohere-bearer-test"),
            "Cohere must use Bearer auth"
        );

        harness
            .log()
            .info("contract", "cohere auth header validated");
    }

    #[test]
    fn response_event_decoding_text() {
        let harness = TestHarness::new("cohere_contract_response_decoding_text");
        let server = harness.start_mock_http_server();
        let endpoint = "/v2/chat";

        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(cohere_simple_sse()),
        );

        let entry = make_model_entry(
            "cohere",
            "command-r-test",
            &format!("{}/v2", server.base_url()),
        );
        let provider = create_provider(&entry, None).expect("create cohere");

        let events = collect_stream_events(provider, simple_context(), options_with_key("test"));

        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(has_text, "Cohere text stream must produce Text events");
        assert!(has_done, "Cohere text stream must end with Done event");

        harness
            .log()
            .info("contract", "cohere response decoding validated");
    }
}

// ============================================================================
// OPENAI-COMPATIBLE PRESET CONTRACT TESTS
// ============================================================================
//
// These tests verify that OpenAI-compatible presets (groq, deepseek, etc.)
// correctly route through the OpenAI chat completions adapter.

mod openai_compat_preset_contract {
    use super::*;

    const PRESET_PROVIDERS: [&str; 16] = [
        "groq",
        "deepseek",
        "xai",
        "perplexity",
        "fireworks",
        "cerebras",
        "openrouter",
        "moonshotai",
        "alibaba",
        // Longtail quick-win providers (bd-3uqg.11.10.5)
        "stackit",
        "mistral",
        "deepinfra",
        "togetherai",
        "nvidia",
        "huggingface",
        "ollama-cloud",
    ];

    #[test]
    fn preset_providers_use_bearer_auth_and_chat_completions() {
        let harness = TestHarness::new("preset_contract_bearer_and_chat_completions");

        for &provider_id in &PRESET_PROVIDERS {
            let server = harness.start_mock_http_server();
            let endpoint = "/v1/chat/completions";
            server.add_route(
                "POST",
                endpoint,
                text_event_stream_response(openai_simple_sse()),
            );

            let entry = make_model_entry_with_api(
                provider_id,
                "preset-model",
                &format!("{}/v1", server.base_url()),
                "openai-completions",
            );
            let provider = create_provider(&entry, None)
                .unwrap_or_else(|e| panic!("create_provider should work for {provider_id}: {e}"));

            let api_key = format!("{provider_id}-contract-key");
            collect_stream_events(provider, simple_context(), options_with_key(&api_key));

            let req = &server.requests()[0];

            // All OAI-compat presets use Bearer auth
            let expected_auth = format!("Bearer {api_key}");
            assert_eq!(
                request_header(&req.headers, "authorization").as_deref(),
                Some(expected_auth.as_str()),
                "{provider_id} must use Bearer auth"
            );

            // All route to /chat/completions
            assert_eq!(
                req.path, endpoint,
                "{provider_id} must route to /v1/chat/completions"
            );

            // Body must contain standard OpenAI fields
            let body = request_body_json(req);
            assert_eq!(body["model"], "preset-model");
            assert_eq!(body["stream"], true);
            assert!(body["messages"].is_array());

            harness
                .log()
                .info_ctx("contract", "preset validated", |ctx| {
                    ctx.push(("provider".to_string(), provider_id.to_string()));
                });
        }
    }
}

// ============================================================================
// CROSS-PROVIDER INVARIANTS
// ============================================================================

mod cross_provider_invariants {
    use super::*;

    /// All native providers must produce at least one Text or `ToolCallStart` event
    /// followed by a Done event when given a valid simple SSE stream.
    #[test]
    fn all_native_providers_produce_done_event() {
        let harness = TestHarness::new("cross_provider_done_event");

        let cases: Vec<(&str, &str, &str, String)> = vec![
            (
                "anthropic",
                "claude-test",
                "/v1/messages",
                anthropic_simple_sse(),
            ),
            (
                "openai",
                "gpt-test",
                "/v1/chat/completions",
                openai_simple_sse(),
            ),
            ("cohere", "command-r-test", "/v2/chat", cohere_simple_sse()),
        ];

        for (provider_id, model_id, endpoint, sse_body) in cases {
            let server = harness.start_mock_http_server();
            server.add_route("POST", endpoint, text_event_stream_response(sse_body));

            let mut entry = make_model_entry(
                provider_id,
                model_id,
                &format!("{}{endpoint}", server.base_url()),
            );
            if provider_id == "openai" {
                entry.model.api = "openai-completions".to_string();
            }
            let provider = create_provider(&entry, None)
                .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

            let events =
                collect_stream_events(provider, simple_context(), options_with_key("test"));

            let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));
            assert!(
                has_done,
                "{provider_id} must produce a Done event from a valid stream"
            );

            harness
                .log()
                .info_ctx("invariant", "done event confirmed", |ctx| {
                    ctx.push(("provider".to_string(), provider_id.to_string()));
                    ctx.push(("event_count".to_string(), events.len().to_string()));
                });
        }
    }

    /// All native providers must set Content-Type: application/json on requests.
    #[test]
    fn all_native_providers_send_json_content_type() {
        let harness = TestHarness::new("cross_provider_json_content_type");

        let cases: Vec<(&str, &str, &str, String)> = vec![
            (
                "anthropic",
                "claude-test",
                "/v1/messages",
                anthropic_simple_sse(),
            ),
            (
                "openai",
                "gpt-test",
                "/v1/chat/completions",
                openai_simple_sse(),
            ),
            ("cohere", "command-r-test", "/v2/chat", cohere_simple_sse()),
        ];

        for (provider_id, model_id, endpoint, sse_body) in cases {
            let server = harness.start_mock_http_server();
            server.add_route("POST", endpoint, text_event_stream_response(sse_body));

            let mut entry = make_model_entry(
                provider_id,
                model_id,
                &format!("{}{endpoint}", server.base_url()),
            );
            if provider_id == "openai" {
                entry.model.api = "openai-completions".to_string();
            }
            let provider = create_provider(&entry, None)
                .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

            collect_stream_events(provider, simple_context(), options_with_key("test"));

            let req = &server.requests()[0];
            assert_eq!(
                request_header(&req.headers, "content-type").as_deref(),
                Some("application/json"),
                "{provider_id} must send Content-Type: application/json"
            );
        }
    }
}

// ============================================================================
// GAP PROVIDER CONTRACT TESTS (bd-3uqg.11.11.2)
//
// Explicit per-provider contract tests for the 5 gap providers:
//   groq, cerebras, openrouter, moonshotai (kimi), alibaba (qwen/dashscope)
//
// Each module validates: request shape, auth, tools, text decoding, tool-call
// decoding, and HTTP error responses (401, 429).
// ============================================================================

fn openai_error_json(status: u16, error_type: &str, message: &str) -> MockHttpResponse {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
            "code": status.to_string()
        }
    });
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

/// Shared contract-test driver for an OpenAI-compatible gap provider.
mod gap_provider_helpers {
    use super::*;

    pub fn assert_factory_routes_and_api(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_factory_route"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        assert_eq!(
            provider.api(),
            "openai-completions",
            "{provider_id} must route to openai-completions"
        );
        assert_eq!(
            provider.model_id(),
            "gap-model",
            "{provider_id} model_id must match"
        );

        collect_stream_events(provider, simple_context(), options_with_key("key"));

        let req = &server.requests()[0];
        assert_eq!(req.path, endpoint, "{provider_id} path mismatch");
        assert_eq!(req.method, "POST", "{provider_id} method mismatch");
    }

    pub fn assert_bearer_auth(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_bearer_auth"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        let api_key = format!("{provider_id}-secret-key-42");
        collect_stream_events(provider, simple_context(), options_with_key(&api_key));

        let req = &server.requests()[0];
        let expected = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&req.headers, "authorization").as_deref(),
            Some(expected.as_str()),
            "{provider_id} must use Bearer auth"
        );
    }

    pub fn assert_request_body_shape(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_request_body"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        collect_stream_events(provider, simple_context(), options_with_key("key"));

        let req = &server.requests()[0];
        let body = request_body_json(req);

        assert_eq!(body["model"], "gap-model", "{provider_id} model field");
        assert_eq!(body["stream"], true, "{provider_id} stream field");
        assert!(
            body["messages"].is_array(),
            "{provider_id} messages must be array"
        );

        let messages = body["messages"].as_array().unwrap();
        let has_user = messages.iter().any(|m| m["role"] == "user");
        assert!(has_user, "{provider_id} must include user message");

        assert_eq!(
            request_header(&req.headers, "content-type").as_deref(),
            Some("application/json"),
            "{provider_id} content-type"
        );
    }

    pub fn assert_tool_schema_translation(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_tool_schema"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_tool_call_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        collect_stream_events(provider, context_with_tools(), options_with_key("key"));

        let req = &server.requests()[0];
        let body = request_body_json(req);

        let tools = body["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{provider_id}: tools must be array"));
        assert_eq!(tools.len(), 2, "{provider_id} tool count");

        let echo_tool = &tools[0];
        assert_eq!(
            echo_tool["function"]["name"], "echo",
            "{provider_id} echo tool name"
        );
        assert!(
            echo_tool["function"]["parameters"].is_object(),
            "{provider_id} echo tool parameters"
        );
    }

    pub fn assert_text_response_decoding(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_text_decoding"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_simple_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        let events = collect_stream_events(provider, simple_context(), options_with_key("key"));

        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(has_text, "{provider_id} must produce TextDelta");
        assert!(has_done, "{provider_id} must produce Done");
    }

    pub fn assert_tool_call_response_decoding(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_tool_call_decoding"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            text_event_stream_response(openai_tool_call_sse()),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        let events = collect_stream_events(provider, context_with_tools(), options_with_key("key"));

        let has_tool_call = events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }));
        let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));

        assert!(has_tool_call, "{provider_id} must produce ToolCallStart");
        assert!(has_done, "{provider_id} must produce Done");
    }

    pub fn assert_http_401_error(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_error_401"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            openai_error_json(401, "authentication_error", "Invalid API key"),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        let ctx = simple_context();
        let opts = options_with_key("bad-key");
        let provider_id_owned = provider_id.to_string();
        common::run_async(async move {
            match provider.stream(&ctx, &opts).await {
                Err(e) => {
                    let msg = format!("{e}");
                    assert!(
                        msg.contains("401")
                            || msg.to_lowercase().contains("auth")
                            || msg.to_lowercase().contains("unauthorized")
                            || msg.to_lowercase().contains("error")
                            || msg.to_lowercase().contains("http"),
                        "{provider_id_owned} 401 error message should reference the error: {msg}"
                    );
                }
                Ok(mut stream) => {
                    let mut events = Vec::new();
                    while let Some(event) = stream.next().await {
                        if let Ok(ev) = event {
                            let is_done = matches!(ev, StreamEvent::Done { .. });
                            events.push(ev);
                            if is_done {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    let has_text = events
                        .iter()
                        .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
                    assert!(
                        !has_text,
                        "{provider_id_owned} 401 error must not produce TextDelta events"
                    );
                }
            }
        });

        harness
            .log()
            .info_ctx("error", "401 handled correctly", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
            });
    }

    pub fn assert_http_429_error(provider_id: &str) {
        let harness = TestHarness::new(format!("{provider_id}_error_429"));
        let server = harness.start_mock_http_server();
        let endpoint = "/v1/chat/completions";
        server.add_route(
            "POST",
            endpoint,
            openai_error_json(429, "rate_limit_error", "Rate limit exceeded"),
        );

        let entry = make_model_entry_with_api(
            provider_id,
            "gap-model",
            &format!("{}/v1", server.base_url()),
            "openai-completions",
        );
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider for {provider_id}: {e}"));

        let ctx = simple_context();
        let opts = options_with_key("key");
        let provider_id_owned = provider_id.to_string();
        common::run_async(async move {
            match provider.stream(&ctx, &opts).await {
                Err(e) => {
                    let msg = format!("{e}");
                    assert!(
                        msg.contains("429")
                            || msg.to_lowercase().contains("rate")
                            || msg.to_lowercase().contains("limit")
                            || msg.to_lowercase().contains("error")
                            || msg.to_lowercase().contains("http"),
                        "{provider_id_owned} 429 error message should reference the error: {msg}"
                    );
                }
                Ok(mut stream) => {
                    let mut events = Vec::new();
                    while let Some(event) = stream.next().await {
                        if let Ok(ev) = event {
                            let is_done = matches!(ev, StreamEvent::Done { .. });
                            events.push(ev);
                            if is_done {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    let has_text = events
                        .iter()
                        .any(|e| matches!(e, StreamEvent::TextDelta { .. }));
                    assert!(
                        !has_text,
                        "{provider_id_owned} 429 error must not produce TextDelta events"
                    );
                }
            }
        });

        harness
            .log()
            .info_ctx("error", "429 handled correctly", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
            });
    }
}

// ── Groq ────────────────────────────────────────────────────────────────

mod groq_contract {
    use super::*;

    #[test]
    fn factory_route_and_api() {
        gap_provider_helpers::assert_factory_routes_and_api("groq");
    }

    #[test]
    fn bearer_auth() {
        gap_provider_helpers::assert_bearer_auth("groq");
    }

    #[test]
    fn request_body_shape() {
        gap_provider_helpers::assert_request_body_shape("groq");
    }

    #[test]
    fn tool_schema_translation() {
        gap_provider_helpers::assert_tool_schema_translation("groq");
    }

    #[test]
    fn text_response_decoding() {
        gap_provider_helpers::assert_text_response_decoding("groq");
    }

    #[test]
    fn tool_call_response_decoding() {
        gap_provider_helpers::assert_tool_call_response_decoding("groq");
    }

    #[test]
    fn error_401_auth_failure() {
        gap_provider_helpers::assert_http_401_error("groq");
    }

    #[test]
    fn error_429_rate_limit() {
        gap_provider_helpers::assert_http_429_error("groq");
    }
}

// ── Cerebras ────────────────────────────────────────────────────────────

mod cerebras_contract {
    use super::*;

    #[test]
    fn factory_route_and_api() {
        gap_provider_helpers::assert_factory_routes_and_api("cerebras");
    }

    #[test]
    fn bearer_auth() {
        gap_provider_helpers::assert_bearer_auth("cerebras");
    }

    #[test]
    fn request_body_shape() {
        gap_provider_helpers::assert_request_body_shape("cerebras");
    }

    #[test]
    fn tool_schema_translation() {
        gap_provider_helpers::assert_tool_schema_translation("cerebras");
    }

    #[test]
    fn text_response_decoding() {
        gap_provider_helpers::assert_text_response_decoding("cerebras");
    }

    #[test]
    fn tool_call_response_decoding() {
        gap_provider_helpers::assert_tool_call_response_decoding("cerebras");
    }

    #[test]
    fn error_401_auth_failure() {
        gap_provider_helpers::assert_http_401_error("cerebras");
    }

    #[test]
    fn error_429_rate_limit() {
        gap_provider_helpers::assert_http_429_error("cerebras");
    }
}

// ── OpenRouter ──────────────────────────────────────────────────────────

mod openrouter_contract {
    use super::*;

    #[test]
    fn factory_route_and_api() {
        gap_provider_helpers::assert_factory_routes_and_api("openrouter");
    }

    #[test]
    fn bearer_auth() {
        gap_provider_helpers::assert_bearer_auth("openrouter");
    }

    #[test]
    fn request_body_shape() {
        gap_provider_helpers::assert_request_body_shape("openrouter");
    }

    #[test]
    fn tool_schema_translation() {
        gap_provider_helpers::assert_tool_schema_translation("openrouter");
    }

    #[test]
    fn text_response_decoding() {
        gap_provider_helpers::assert_text_response_decoding("openrouter");
    }

    #[test]
    fn tool_call_response_decoding() {
        gap_provider_helpers::assert_tool_call_response_decoding("openrouter");
    }

    #[test]
    fn error_401_auth_failure() {
        gap_provider_helpers::assert_http_401_error("openrouter");
    }

    #[test]
    fn error_429_rate_limit() {
        gap_provider_helpers::assert_http_429_error("openrouter");
    }
}

// ── Moonshotai / Kimi ───────────────────────────────────────────────────

mod moonshotai_contract {
    use super::*;

    #[test]
    fn factory_route_and_api() {
        gap_provider_helpers::assert_factory_routes_and_api("moonshotai");
    }

    #[test]
    fn bearer_auth() {
        gap_provider_helpers::assert_bearer_auth("moonshotai");
    }

    #[test]
    fn request_body_shape() {
        gap_provider_helpers::assert_request_body_shape("moonshotai");
    }

    #[test]
    fn tool_schema_translation() {
        gap_provider_helpers::assert_tool_schema_translation("moonshotai");
    }

    #[test]
    fn text_response_decoding() {
        gap_provider_helpers::assert_text_response_decoding("moonshotai");
    }

    #[test]
    fn tool_call_response_decoding() {
        gap_provider_helpers::assert_tool_call_response_decoding("moonshotai");
    }

    #[test]
    fn error_401_auth_failure() {
        gap_provider_helpers::assert_http_401_error("moonshotai");
    }

    #[test]
    fn error_429_rate_limit() {
        gap_provider_helpers::assert_http_429_error("moonshotai");
    }
}

// ── Alibaba / Qwen / DashScope ─────────────────────────────────────────

mod alibaba_contract {
    use super::*;

    #[test]
    fn factory_route_and_api() {
        gap_provider_helpers::assert_factory_routes_and_api("alibaba");
    }

    #[test]
    fn bearer_auth() {
        gap_provider_helpers::assert_bearer_auth("alibaba");
    }

    #[test]
    fn request_body_shape() {
        gap_provider_helpers::assert_request_body_shape("alibaba");
    }

    #[test]
    fn tool_schema_translation() {
        gap_provider_helpers::assert_tool_schema_translation("alibaba");
    }

    #[test]
    fn text_response_decoding() {
        gap_provider_helpers::assert_text_response_decoding("alibaba");
    }

    #[test]
    fn tool_call_response_decoding() {
        gap_provider_helpers::assert_tool_call_response_decoding("alibaba");
    }

    #[test]
    fn error_401_auth_failure() {
        gap_provider_helpers::assert_http_401_error("alibaba");
    }

    #[test]
    fn error_429_rate_limit() {
        gap_provider_helpers::assert_http_429_error("alibaba");
    }
}

// ── Gap-provider metadata consistency ───────────────────────────────────

mod gap_provider_metadata {
    use pi::provider_metadata::{
        PROVIDER_METADATA, canonical_provider_id, provider_auth_env_keys, provider_routing_defaults,
    };

    const GAP_PROVIDERS: [(&str, &str, &[&str]); 5] = [
        ("groq", "openai-completions", &["GROQ_API_KEY"]),
        ("cerebras", "openai-completions", &["CEREBRAS_API_KEY"]),
        ("openrouter", "openai-completions", &["OPENROUTER_API_KEY"]),
        (
            "moonshotai",
            "openai-completions",
            &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
        ),
        (
            "alibaba",
            "openai-completions",
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        ),
    ];

    #[test]
    fn all_gap_providers_present_in_metadata() {
        for (id, _, _) in &GAP_PROVIDERS {
            let resolved = canonical_provider_id(id);
            assert_eq!(
                resolved,
                Some(*id),
                "{id} must be a canonical provider ID in metadata"
            );
        }
    }

    #[test]
    fn all_gap_providers_have_routing_defaults() {
        for (id, expected_api, _) in &GAP_PROVIDERS {
            let defaults = provider_routing_defaults(id);
            assert!(
                defaults.is_some(),
                "{id} must have routing defaults in metadata"
            );
            let defaults = defaults.unwrap();
            assert_eq!(
                defaults.api, *expected_api,
                "{id} routing default API mismatch"
            );
            assert!(
                defaults.auth_header,
                "{id} must use auth_header=true for Bearer auth"
            );
        }
    }

    #[test]
    fn all_gap_providers_have_auth_env_keys() {
        for (id, _, expected_keys) in &GAP_PROVIDERS {
            let keys = provider_auth_env_keys(id);
            assert!(
                !keys.is_empty(),
                "{id} must have at least one auth env key in metadata"
            );
            assert_eq!(
                keys[0], expected_keys[0],
                "{id} primary auth env key mismatch"
            );
        }
    }

    #[test]
    fn kimi_alias_resolves_to_moonshotai() {
        let meta = PROVIDER_METADATA
            .iter()
            .find(|m| m.canonical_id == "moonshotai");
        assert!(meta.is_some(), "moonshotai must exist in PROVIDER_METADATA");
        let meta = meta.unwrap();
        assert!(
            meta.aliases.contains(&"kimi") || meta.aliases.contains(&"moonshot"),
            "moonshotai must have kimi or moonshot as alias"
        );
    }

    #[test]
    fn dashscope_alias_resolves_to_alibaba() {
        let resolved = canonical_provider_id("dashscope");
        assert_eq!(
            resolved,
            Some("alibaba"),
            "dashscope must resolve to alibaba"
        );
    }

    #[test]
    fn all_gap_providers_require_tests() {
        for (id, _, _) in &GAP_PROVIDERS {
            let meta = PROVIDER_METADATA.iter().find(|m| m.canonical_id == *id);
            assert!(meta.is_some(), "{id} must exist in PROVIDER_METADATA");
            let meta = meta.unwrap();
            assert!(
                meta.test_obligations.unit,
                "{id} must have unit test obligation"
            );
            assert!(
                meta.test_obligations.contract,
                "{id} must have contract test obligation"
            );
        }
    }
}

// ============================================================================
// LONGTAIL PROVIDER CONTRACT TESTS (bd-3uqg.11.10.5)
//
// Unit/contract verification for longtail quick-win providers that route
// through existing OpenAI-compatible adapter paths. Representative coverage
// for: stackit, mistral, deepinfra, togetherai, nvidia, huggingface,
// ollama-cloud.
// ============================================================================

mod longtail_contract {
    use super::*;

    mod stackit_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("stackit");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("stackit");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("stackit");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("stackit");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("stackit");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("stackit");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("stackit");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("stackit");
        }
    }

    mod mistral_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("mistral");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("mistral");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("mistral");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("mistral");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("mistral");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("mistral");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("mistral");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("mistral");
        }
    }

    mod deepinfra_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("deepinfra");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("deepinfra");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("deepinfra");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("deepinfra");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("deepinfra");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("deepinfra");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("deepinfra");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("deepinfra");
        }
    }

    mod togetherai_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("togetherai");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("togetherai");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("togetherai");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("togetherai");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("togetherai");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("togetherai");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("togetherai");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("togetherai");
        }
    }

    mod nvidia_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("nvidia");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("nvidia");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("nvidia");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("nvidia");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("nvidia");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("nvidia");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("nvidia");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("nvidia");
        }
    }

    mod huggingface_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("huggingface");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("huggingface");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("huggingface");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("huggingface");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("huggingface");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("huggingface");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("huggingface");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("huggingface");
        }
    }

    mod ollama_cloud_contract {
        use super::*;

        #[test]
        fn factory_routes_and_api() {
            gap_provider_helpers::assert_factory_routes_and_api("ollama-cloud");
        }
        #[test]
        fn bearer_auth() {
            gap_provider_helpers::assert_bearer_auth("ollama-cloud");
        }
        #[test]
        fn text_response_decoding() {
            gap_provider_helpers::assert_text_response_decoding("ollama-cloud");
        }
        #[test]
        fn tool_call_response_decoding() {
            gap_provider_helpers::assert_tool_call_response_decoding("ollama-cloud");
        }
        #[test]
        fn tool_schema_translation() {
            gap_provider_helpers::assert_tool_schema_translation("ollama-cloud");
        }
        #[test]
        fn request_shape() {
            gap_provider_helpers::assert_request_body_shape("ollama-cloud");
        }
        #[test]
        fn error_401() {
            gap_provider_helpers::assert_http_401_error("ollama-cloud");
        }
        #[test]
        fn error_429() {
            gap_provider_helpers::assert_http_429_error("ollama-cloud");
        }
    }
}

mod longtail_provider_metadata {
    use pi::provider_metadata::{
        PROVIDER_METADATA, provider_auth_env_keys, provider_routing_defaults,
    };

    const LONGTAIL_PROVIDERS: [(&str, &str, &[&str]); 7] = [
        ("stackit", "openai-completions", &["STACKIT_API_KEY"]),
        ("mistral", "openai-completions", &["MISTRAL_API_KEY"]),
        ("deepinfra", "openai-completions", &["DEEPINFRA_API_KEY"]),
        ("togetherai", "openai-completions", &["TOGETHER_API_KEY"]),
        ("nvidia", "openai-completions", &["NVIDIA_API_KEY"]),
        ("huggingface", "openai-completions", &["HF_TOKEN"]),
        ("ollama-cloud", "openai-completions", &["OLLAMA_API_KEY"]),
    ];

    #[test]
    fn all_longtail_providers_exist_in_metadata() {
        for (id, _, _) in &LONGTAIL_PROVIDERS {
            let meta = PROVIDER_METADATA.iter().find(|m| m.canonical_id == *id);
            assert!(
                meta.is_some(),
                "{id} must be a canonical provider ID in metadata"
            );
        }
    }

    #[test]
    fn all_longtail_providers_have_routing_defaults() {
        for (id, expected_api, _) in &LONGTAIL_PROVIDERS {
            let defaults = provider_routing_defaults(id);
            assert!(
                defaults.is_some(),
                "{id} must have routing defaults in metadata"
            );
            let defaults = defaults.unwrap();
            assert_eq!(
                defaults.api, *expected_api,
                "{id} routing default API mismatch"
            );
        }
    }

    #[test]
    fn all_longtail_providers_have_auth_env_keys() {
        for (id, _, expected_keys) in &LONGTAIL_PROVIDERS {
            let keys = provider_auth_env_keys(id);
            assert!(
                !keys.is_empty(),
                "{id} must have at least one auth env key in metadata"
            );
            assert_eq!(
                keys[0], expected_keys[0],
                "{id} primary auth env key mismatch"
            );
        }
    }

    #[test]
    fn all_longtail_providers_require_tests() {
        for (id, _, _) in &LONGTAIL_PROVIDERS {
            let meta = PROVIDER_METADATA.iter().find(|m| m.canonical_id == *id);
            assert!(meta.is_some(), "{id} must exist in PROVIDER_METADATA");
            let meta = meta.unwrap();
            assert!(
                meta.test_obligations.unit,
                "{id} must have unit test obligation"
            );
            assert!(
                meta.test_obligations.contract,
                "{id} must have contract test obligation"
            );
        }
    }

    #[test]
    fn longtail_auth_header_defaults_are_correct() {
        for (id, _, _) in &LONGTAIL_PROVIDERS {
            let defaults = provider_routing_defaults(id).unwrap();
            assert!(
                defaults.auth_header,
                "{id} must use auth_header=true for Bearer auth"
            );
        }
    }
}

// ============================================================================
// FAILURE TAXONOMY VALIDATION (bd-3uqg.11.11.6)
//
// Validates that failures from provider suites are categorized consistently
// and mapped to deterministic remediation paths via the error hint system.
// ============================================================================

mod failure_taxonomy {
    use pi::error::Error;
    use pi::provider_metadata::{PROVIDER_METADATA, provider_auth_env_keys};

    /// All gap + longtail providers that must have complete error hint coverage.
    const TAXONOMY_PROVIDERS: [&str; 12] = [
        "groq",
        "cerebras",
        "openrouter",
        "moonshotai",
        "alibaba",
        "stackit",
        "mistral",
        "deepinfra",
        "togetherai",
        "nvidia",
        "huggingface",
        "ollama-cloud",
    ];

    /// Canonical failure categories that every provider must handle.
    const FAILURE_CATEGORIES: [(&str, &str); 7] = [
        ("missing_api_key", "missing api key"),
        ("auth_401", "401 unauthorized"),
        ("forbidden_403", "403 forbidden"),
        ("rate_limit_429", "429 too many requests"),
        ("quota_exceeded", "insufficient_quota"),
        ("overloaded_529", "529 overloaded"),
        ("timeout", "request timed out"),
    ];

    #[test]
    fn all_providers_produce_hint_summary_for_missing_key() {
        for provider in &TAXONOMY_PROVIDERS {
            let err = Error::Provider {
                provider: provider.to_string(),
                message: "Missing API key".to_string(),
            };
            let hints = err.hints();
            assert!(
                !hints.summary.is_empty(),
                "{provider}: missing-key error must produce a non-empty hint summary"
            );
            assert!(
                !hints.hints.is_empty(),
                "{provider}: missing-key error must produce at least one remediation hint"
            );
        }
    }

    #[test]
    fn all_providers_produce_hint_for_auth_failure() {
        for provider in &TAXONOMY_PROVIDERS {
            let err = Error::Provider {
                provider: provider.to_string(),
                message: "401 Unauthorized - Invalid API key".to_string(),
            };
            let hints = err.hints();
            assert!(
                !hints.summary.is_empty(),
                "{provider}: 401 error must produce a hint summary"
            );
            assert!(
                hints.summary.to_lowercase().contains("auth"),
                "{provider}: 401 hint summary should mention auth: got '{}'",
                hints.summary
            );
        }
    }

    #[test]
    fn all_providers_produce_hint_for_rate_limit() {
        for provider in &TAXONOMY_PROVIDERS {
            let err = Error::Provider {
                provider: provider.to_string(),
                message: "429 Too Many Requests - Rate limit exceeded".to_string(),
            };
            let hints = err.hints();
            assert!(
                !hints.summary.is_empty(),
                "{provider}: 429 error must produce a hint summary"
            );
            assert!(
                hints.summary.to_lowercase().contains("rate"),
                "{provider}: 429 hint should mention rate limiting: got '{}'",
                hints.summary
            );
        }
    }

    #[test]
    fn provider_key_hints_reference_correct_env_var() {
        for provider in &TAXONOMY_PROVIDERS {
            let env_keys = provider_auth_env_keys(provider);
            if env_keys.is_empty() {
                continue;
            }
            let err = Error::Provider {
                provider: provider.to_string(),
                message: "Missing API key".to_string(),
            };
            let hints = err.hints();
            let all_hints = hints.hints.join(" ");
            assert!(
                env_keys.iter().any(|key| all_hints.contains(key)),
                "{provider}: remediation hints must reference the auth env var ({env_keys:?}), got: {all_hints}"
            );
        }
    }

    #[test]
    fn all_failure_categories_produce_distinct_summaries() {
        // Validate ALL taxonomy providers, not just a subset.
        for provider in &TAXONOMY_PROVIDERS {
            let mut summaries = std::collections::HashSet::new();
            for (category, message) in &FAILURE_CATEGORIES {
                let err = Error::Provider {
                    provider: provider.to_string(),
                    message: message.to_string(),
                };
                let hints = err.hints();
                assert!(
                    !hints.summary.is_empty(),
                    "{provider}/{category}: must produce a hint summary"
                );
                summaries.insert(hints.summary.clone());
            }
            assert!(
                summaries.len() >= 4,
                "{provider}: expected at least 4 distinct failure summaries across 7 categories, got {}",
                summaries.len()
            );
        }
    }

    #[test]
    fn auth_diagnostic_codes_all_have_remediation() {
        use pi::error::AuthDiagnosticCode;
        let codes = [
            AuthDiagnosticCode::MissingApiKey,
            AuthDiagnosticCode::InvalidApiKey,
            AuthDiagnosticCode::QuotaExceeded,
            AuthDiagnosticCode::MissingOAuthAuthorizationCode,
            AuthDiagnosticCode::OAuthTokenExchangeFailed,
            AuthDiagnosticCode::OAuthTokenRefreshFailed,
            AuthDiagnosticCode::MissingAzureDeployment,
            AuthDiagnosticCode::MissingRegion,
            AuthDiagnosticCode::MissingProject,
            AuthDiagnosticCode::MissingProfile,
            AuthDiagnosticCode::MissingEndpoint,
            AuthDiagnosticCode::MissingCredentialChain,
            AuthDiagnosticCode::UnknownAuthFailure,
        ];
        for code in &codes {
            let stable_str = code.as_str();
            assert!(
                !stable_str.is_empty(),
                "{code:?}: stable string code must not be empty"
            );
            let remediation = code.remediation();
            assert!(
                !remediation.is_empty(),
                "{code:?}: remediation text must not be empty"
            );
            let policy = code.redaction_policy();
            assert!(
                !policy.is_empty(),
                "{code:?}: redaction policy must not be empty"
            );
            // All auth errors should have a remediation hint, not just a code.
            assert!(
                remediation.len() > 10,
                "{code:?}: remediation should be actionable (got {remediation})"
            );
        }
    }

    #[test]
    fn flake_classifier_covers_all_categories() {
        use pi::flake_classifier::{FlakeCategory, classify_failure};
        // Each FlakeCategory must be reachable via classify_failure.
        let cases: Vec<(FlakeCategory, &str)> = vec![
            (
                FlakeCategory::OracleTimeout,
                "error: TS oracle process timed out after 30s",
            ),
            (
                FlakeCategory::ResourceExhaustion,
                "fatal: out of memory (allocator returned null)",
            ),
            (
                FlakeCategory::FsContention,
                "error: EBUSY: resource busy or locked",
            ),
            (
                FlakeCategory::PortConflict,
                "listen EADDRINUSE: address already in use :::3000",
            ),
            (
                FlakeCategory::TmpdirRace,
                "error: No such file or directory (os error 2), path: /tmp/pi-test-xyz",
            ),
            (
                FlakeCategory::JsGcPressure,
                "quickjs runtime: allocation failed, out of memory",
            ),
        ];
        let mut covered = std::collections::HashSet::new();
        for (expected_cat, output) in &cases {
            let result = classify_failure(output);
            match result {
                pi::flake_classifier::FlakeClassification::Transient { category, .. } => {
                    assert_eq!(
                        &category, expected_cat,
                        "Expected {expected_cat:?} for output: {output}"
                    );
                    covered.insert(category);
                }
                pi::flake_classifier::FlakeClassification::Deterministic => {
                    panic!(
                        "Expected Transient({expected_cat:?}) but got Deterministic for: {output}"
                    );
                }
            }
        }
        assert_eq!(
            covered.len(),
            FlakeCategory::all().len(),
            "All FlakeCategory variants must be covered by classify_failure"
        );
    }

    #[test]
    fn provider_error_messages_map_to_correct_hint_category() {
        // Provider-specific error messages should map to the correct
        // hint category (summary). This validates the triage runbook's
        // failure-to-remediation mapping is deterministic.
        let cases: Vec<(&str, &str, &str)> = vec![
            // (provider, error message, expected summary substring)
            ("groq", "401 Unauthorized - Invalid API key", "auth"),
            ("cerebras", "429 Too Many Requests", "rate"),
            ("moonshotai", "Missing API key", "missing"),
            (
                "alibaba",
                "429 Too Many Requests - Rate limit exceeded",
                "rate",
            ),
            ("alibaba", "insufficient_quota - payment overdue", "quota"),
            ("groq", "529 overloaded - please retry", "overloaded"),
            ("cerebras", "request timed out after 30s", "timed out"),
            ("openrouter", "400 Bad Request - invalid model", "rejected"),
            ("moonshotai", "500 Internal Server Error", "server error"),
            ("stackit", "403 Forbidden", "forbidden"),
        ];
        for (provider, message, expected_substr) in &cases {
            let err = Error::Provider {
                provider: provider.to_string(),
                message: message.to_string(),
            };
            let hints = err.hints();
            assert!(
                hints.summary.to_lowercase().contains(expected_substr),
                "{provider}/{message}: expected summary to contain '{expected_substr}', got '{}'",
                hints.summary
            );
        }
    }

    #[test]
    fn deterministic_failures_produce_non_retriable_classification() {
        use pi::flake_classifier::classify_failure;
        // Real assertion failures (not flakes) must be classified as Deterministic.
        let deterministic_outputs = [
            "assertion failed: expected 200 but got 401",
            "test provider_factory::groq_routing ... FAILED",
            "thread 'main' panicked at 'explicit panic'",
            "error[E0277]: the trait bound is not satisfied",
            "missing field `content` in stream event",
        ];
        for output in &deterministic_outputs {
            let result = classify_failure(output);
            assert!(
                !result.is_retriable(),
                "Output should be Deterministic (not retriable): {output}"
            );
        }
    }

    #[test]
    fn all_taxonomy_providers_exist_in_metadata() {
        for provider in &TAXONOMY_PROVIDERS {
            let meta = PROVIDER_METADATA
                .iter()
                .find(|m| m.canonical_id == *provider);
            assert!(
                meta.is_some(),
                "{provider} must exist in PROVIDER_METADATA for taxonomy validation"
            );
        }
    }

    #[test]
    fn error_hints_include_provider_context() {
        for provider in &TAXONOMY_PROVIDERS {
            let err = Error::Provider {
                provider: provider.to_string(),
                message: "401 Unauthorized".to_string(),
            };
            let hints = err.hints();
            let has_provider_ctx = hints
                .context
                .iter()
                .any(|(k, v)| k == "provider" && v == *provider);
            assert!(
                has_provider_ctx,
                "{provider}: error hints must include provider context"
            );
        }
    }
}

// ============================================================================
// DOCS/RUNTIME CONSISTENCY CHECKS (bd-3uqg.11.12.5)
//
// Validates that published documentation (setup guides, config examples)
// cannot silently diverge from the implementation truth in provider_metadata
// and auth resolution logic.
// ============================================================================

mod docs_runtime_consistency {
    use pi::provider_metadata::{PROVIDER_METADATA, provider_auth_env_keys};

    /// Gap providers whose setup docs we validate.
    const DOC_PROVIDERS: [(&str, &str); 5] = [
        ("groq", "docs/provider-groq-setup.json"),
        ("cerebras", "docs/provider-cerebras-setup.json"),
        ("openrouter", "docs/provider-openrouter-setup.json"),
        ("moonshotai", "docs/provider-kimi-setup.json"),
        ("alibaba", "docs/provider-qwen-setup.json"),
    ];

    fn load_doc(path: &str) -> serde_json::Value {
        let full = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path);
        let content =
            std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("Failed to read {full}: {e}"));
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("Failed to parse {full}: {e}"))
    }

    #[test]
    fn setup_docs_exist_and_parse_as_valid_json() {
        for (provider, path) in &DOC_PROVIDERS {
            let doc = load_doc(path);
            assert!(
                doc.is_object(),
                "{provider}: setup doc must be a JSON object"
            );
            assert!(
                doc.get("schema").is_some(),
                "{provider}: setup doc must have a schema field"
            );
        }
    }

    #[test]
    fn setup_doc_provider_ids_match_metadata() {
        for (provider, path) in &DOC_PROVIDERS {
            let doc = load_doc(path);
            let doc_provider = doc
                .get("provider_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let meta = PROVIDER_METADATA
                .iter()
                .find(|m| m.canonical_id == *provider);
            assert!(
                meta.is_some(),
                "{provider}: must exist in PROVIDER_METADATA"
            );
            // Doc provider_id should match canonical or be a valid alias.
            let meta = meta.unwrap();
            let is_canonical = doc_provider == meta.canonical_id;
            let is_alias = meta.aliases.contains(&doc_provider);
            assert!(
                is_canonical || is_alias,
                "{provider}: doc provider_id '{doc_provider}' must match canonical '{}' or aliases {:?}",
                meta.canonical_id,
                meta.aliases
            );
        }
    }

    #[test]
    fn setup_doc_auth_env_matches_runtime() {
        for (provider, path) in &DOC_PROVIDERS {
            let doc = load_doc(path);
            let runtime_keys = provider_auth_env_keys(provider);
            if runtime_keys.is_empty() {
                continue;
            }
            // Check quick_start.auth_env
            if let Some(auth_env) = doc
                .pointer("/quick_start/auth_env")
                .and_then(|v| v.as_str())
            {
                assert!(
                    runtime_keys.contains(&auth_env),
                    "{provider}: doc auth_env '{auth_env}' not in runtime keys {runtime_keys:?}"
                );
            }
            // Check quick_start.minimal_config.example_env keys
            if let Some(example_env) = doc
                .pointer("/quick_start/minimal_config/example_env")
                .and_then(|v| v.as_object())
            {
                for key in example_env.keys() {
                    assert!(
                        runtime_keys.contains(&key.as_str()),
                        "{provider}: doc example_env key '{key}' not in runtime keys {runtime_keys:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn setup_doc_base_url_matches_runtime_default() {
        for (provider, path) in &DOC_PROVIDERS {
            let doc = load_doc(path);
            let meta = PROVIDER_METADATA
                .iter()
                .find(|m| m.canonical_id == *provider);
            let Some(meta) = meta else { continue };
            let Some(defaults) = meta.routing_defaults.as_ref() else {
                continue;
            };
            // Check quick_start.base_url
            if let Some(doc_url) = doc
                .pointer("/quick_start/base_url")
                .and_then(|v| v.as_str())
            {
                assert_eq!(
                    doc_url, defaults.base_url,
                    "{provider}: doc base_url '{doc_url}' must match runtime default '{}'",
                    defaults.base_url
                );
            }
        }
    }

    #[test]
    fn config_examples_doc_covers_all_gap_providers() {
        let doc = load_doc("docs/provider-config-examples.json");
        let families = doc
            .get("provider_families")
            .and_then(|v| v.as_array())
            .expect("config-examples must have provider_families array");
        // Collect all provider_ids across all families
        let ids: Vec<&str> = families
            .iter()
            .filter_map(|f| f.get("providers").and_then(|v| v.as_array()))
            .flatten()
            .filter_map(|p| p.get("provider_id").and_then(|v| v.as_str()))
            .collect();
        for (provider, _) in &DOC_PROVIDERS {
            assert!(
                ids.iter().any(|id| {
                    PROVIDER_METADATA
                        .iter()
                        .find(|m| m.canonical_id == *provider)
                        .is_some_and(|m| *id == m.canonical_id || m.aliases.contains(id))
                }),
                "{provider}: must be covered in config-examples.json provider_families"
            );
        }
    }

    #[test]
    fn config_examples_env_vars_match_runtime() {
        let doc = load_doc("docs/provider-config-examples.json");
        let env_ref = doc
            .get("env_quick_reference")
            .expect("config-examples must have env_quick_reference");
        // Collect env var entries from all category arrays
        let categories = ["built_in_native", "native_adapter", "openai_compatible"];
        for category in &categories {
            let entries = env_ref.get(*category).and_then(|v| v.as_array());
            let Some(entries) = entries else { continue };
            for entry in entries {
                let var_name = entry.get("var").and_then(|v| v.as_str()).unwrap_or("");
                let provider_str = entry.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                // Extract first provider from slash/comma-separated list
                let first_provider = provider_str
                    .split(&[',', '/'][..])
                    .next()
                    .unwrap_or("")
                    .trim();
                if first_provider.is_empty() {
                    continue;
                }
                let runtime_keys = provider_auth_env_keys(first_provider);
                if !runtime_keys.is_empty() {
                    assert!(
                        runtime_keys.contains(&var_name),
                        "env_quick_reference[{category}] var '{var_name}' for '{first_provider}' not in runtime keys {runtime_keys:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn migration_guide_references_correct_env_vars() {
        let path = format!(
            "{}/docs/provider-migration-guide.md",
            env!("CARGO_MANIFEST_DIR")
        );
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read migration guide: {e}"));
        // Verify all documented env vars exist in runtime
        let env_vars = [
            ("GROQ_API_KEY", "groq"),
            ("CEREBRAS_API_KEY", "cerebras"),
            ("OPENROUTER_API_KEY", "openrouter"),
            ("MOONSHOT_API_KEY", "moonshotai"),
            ("DASHSCOPE_API_KEY", "alibaba"),
        ];
        for (var, provider) in &env_vars {
            assert!(
                content.contains(var),
                "Migration guide must reference {var} for {provider}"
            );
            let runtime_keys = provider_auth_env_keys(provider);
            assert!(
                runtime_keys.contains(var),
                "{var} referenced in migration guide but not in runtime keys for {provider}: {runtime_keys:?}"
            );
        }
    }
}
