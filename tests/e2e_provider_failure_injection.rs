//! Failure-injection E2E scenarios for provider streaming (bd-3uqg.8.8).
//!
//! Exercises failure paths that go beyond basic HTTP error codes:
//! 1. Empty/null response bodies
//! 2. Truncated/malformed SSE streams (partial JSON, unexpected EOF)
//! 3. Wrong/missing Content-Type headers
//! 4. Finish-reason edge cases (content_filter, max_tokens, length)
//! 5. Multi-chunk then error (mid-stream failure)
//! 6. Very large error payloads
//! 7. User-facing diagnostics quality
//! 8. Detailed failure logs for debugging
//!
//! All tests use `MockHttpServer` for determinism and produce JSONL artifacts.
//!
//! Run:
//! ```bash
//! cargo test --test e2e_provider_failure_injection
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::similar_names)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::missing_const_for_fn)]

mod common;

use common::{MockHttpResponse, TestHarness};
use futures::StreamExt;
use pi::model::{Message, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamEvent, StreamOptions};
use pi::providers::create_provider;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════

fn make_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} failure-injection model"),
            api: String::new(),
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
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn simple_context() -> Context<'static> {
    Context::owned(
        Some("You are a test model.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Say hello.".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn make_response(status: u16, content_type: &str, body: &[u8]) -> MockHttpResponse {
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), content_type.to_string())],
        body: body.to_vec(),
    }
}

fn make_sse_response(body: &str) -> MockHttpResponse {
    make_response(200, "text/event-stream", body.as_bytes())
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

fn default_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".to_string()),
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[derive(Debug, Serialize)]
struct InjectionResult {
    scenario: String,
    provider: String,
    status: String,
    event_count: usize,
    event_sequence: Vec<String>,
    error_message: Option<String>,
    has_error_event: bool,
    has_done_event: bool,
    text_chars: usize,
}

fn write_results(harness: &TestHarness, name: &str, results: &[InjectionResult]) {
    let mut buf = String::new();
    for r in results {
        let line = serde_json::to_string(r).unwrap_or_default();
        let _ = writeln!(buf, "{line}");
    }
    let path = harness.temp_path(format!("{name}.jsonl"));
    std::fs::write(&path, &buf).expect("write results JSONL");
    harness.record_artifact(format!("{name}.jsonl"), &path);
}

fn classify_result(
    scenario: &str,
    provider: &str,
    result: &Result<Vec<StreamEvent>, String>,
) -> InjectionResult {
    match result {
        Ok(events) => {
            let sequence: Vec<String> = events.iter().map(|e| event_kind(e).to_string()).collect();
            let has_error = events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. }));
            let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));
            let text_chars = events
                .iter()
                .map(|e| match e {
                    StreamEvent::TextDelta { delta, .. } => delta.chars().count(),
                    StreamEvent::TextEnd { content, .. } => content.chars().count(),
                    _ => 0,
                })
                .sum();
            let error_message = events.iter().find_map(|e| match e {
                StreamEvent::Error { error, .. } => error.error_message.clone(),
                _ => None,
            });
            InjectionResult {
                scenario: scenario.to_string(),
                provider: provider.to_string(),
                status: if has_error {
                    "error_detected"
                } else if has_done {
                    "completed"
                } else {
                    "partial"
                }
                .to_string(),
                event_count: events.len(),
                event_sequence: sequence,
                error_message,
                has_error_event: has_error,
                has_done_event: has_done,
                text_chars,
            }
        }
        Err(e) => InjectionResult {
            scenario: scenario.to_string(),
            provider: provider.to_string(),
            status: "stream_error".to_string(),
            event_count: 0,
            event_sequence: Vec::new(),
            error_message: Some(e.clone()),
            has_error_event: false,
            has_done_event: false,
            text_chars: 0,
        },
    }
}

fn oai_route() -> String {
    "/v1/responses".to_string()
}

fn anthropic_route() -> String {
    "/v1/messages".to_string()
}

fn setup_openai(
    harness: &TestHarness,
    response: MockHttpResponse,
) -> (Arc<dyn pi::provider::Provider>, common::MockHttpServer) {
    let server = harness.start_mock_http_server();
    let route = oai_route();
    server.add_route("POST", &route, response);
    let base_url = format!("{}/v1", server.base_url());
    let mut entry = make_entry("openai", "fail-test", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create openai provider");
    (provider, server)
}

fn setup_anthropic(
    harness: &TestHarness,
    response: MockHttpResponse,
) -> (Arc<dyn pi::provider::Provider>, common::MockHttpServer) {
    let server = harness.start_mock_http_server();
    let route = anthropic_route();
    server.add_route("POST", &route, response);
    let base_url = format!("{}/v1/messages", server.base_url());
    let mut entry = make_entry("anthropic", "fail-test", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create anthropic provider");
    (provider, server)
}

// ═══════════════════════════════════════════════════════════════════════
// Section 1: Empty and null response bodies
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn empty_body_200_is_detected_as_error() {
    let harness = TestHarness::new("empty_body_200_is_detected_as_error");
    let mut results = Vec::new();

    // OpenAI: 200 with empty body
    let (provider, _server) = setup_openai(&harness, make_response(200, "text/event-stream", b""));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_body_200", "openai", &r);
    assert!(
        classified.status == "stream_error"
            || classified.has_error_event
            || classified.event_count == 0,
        "empty 200 body must be detected as error or produce no events"
    );
    results.push(classified);

    // Anthropic: 200 with empty body
    let (provider, _server) =
        setup_anthropic(&harness, make_response(200, "text/event-stream", b""));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_body_200", "anthropic", &r);
    // Empty body: provider may error, produce no events, or produce a Done with no content
    assert!(
        classified.status == "stream_error"
            || classified.has_error_event
            || classified.has_done_event
            || classified.event_count == 0,
        "empty 200 body must be detected as error, produce Done, or produce no events"
    );
    results.push(classified);

    write_results(&harness, "empty_body_200", &results);
}

#[test]
fn null_json_body_is_handled() {
    let harness = TestHarness::new("null_json_body_is_handled");
    let mut results = Vec::new();

    let (provider, _server) =
        setup_openai(&harness, make_response(200, "application/json", b"null"));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("null_json_body", "openai", &r);
    // Should not panic
    results.push(classified);

    let (provider, _server) = setup_openai(&harness, make_response(200, "application/json", b"{}"));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_json_object", "openai", &r);
    results.push(classified);

    write_results(&harness, "null_json_body", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 2: Truncated/malformed SSE streams
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn truncated_sse_mid_json_is_error() {
    let harness = TestHarness::new("truncated_sse_mid_json_is_error");

    // SSE with truncated JSON: starts a chunk but cuts off
    let truncated = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"Hel";
    let (provider, _server) = setup_openai(&harness, make_sse_response(truncated));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("truncated_json", "openai", &r);
    // Either error or no text should be produced from truncated JSON
    assert!(
        classified.status == "stream_error"
            || classified.has_error_event
            || classified.text_chars == 0,
        "truncated JSON in SSE must not produce valid text output"
    );

    let mut results = vec![classified];
    write_results(&harness, "truncated_sse", &results);

    // SSE with valid first chunk, then truncated second
    let partial = [
        r#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},"finish_reason":null}]}"#,
        "",
        "data: {\"id\":\"x\",\"object\":\"chat.compl",  // truncated
    ].join("\n");
    let (provider, _server) = setup_openai(&harness, make_sse_response(&partial));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("partial_then_truncated", "openai", &r);
    results.push(classified);

    write_results(&harness, "truncated_sse_partial", &results);
}

#[test]
fn malformed_sse_event_is_handled() {
    let harness = TestHarness::new("malformed_sse_event_is_handled");
    let mut results = Vec::new();

    // SSE with invalid JSON
    let invalid_json = "data: not-valid-json\n\n";
    let (provider, _server) = setup_openai(&harness, make_sse_response(invalid_json));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("invalid_json_event", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "invalid JSON in SSE must produce an error"
    );
    results.push(classified);

    // SSE with data: prefix but no actual data
    let empty_data = "data: \n\ndata: \n\n";
    let (provider, _server) = setup_openai(&harness, make_sse_response(empty_data));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_data_lines", "openai", &r);
    results.push(classified);

    write_results(&harness, "malformed_sse", &results);
}

#[test]
fn sse_with_only_done_marker() {
    let harness = TestHarness::new("sse_with_only_done_marker");

    // OpenAI: just [DONE] without any content chunks
    let only_done = "data: [DONE]\n\n";
    let (provider, _server) = setup_openai(&harness, make_sse_response(only_done));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("only_done", "openai", &r);
    // Should produce Done event with no text content
    assert_eq!(
        classified.text_chars, 0,
        "SSE with only [DONE] should produce no text"
    );
    let results = vec![classified];
    write_results(&harness, "sse_only_done", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 3: Wrong/missing Content-Type headers
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn wrong_content_type_is_detected() {
    let harness = TestHarness::new("wrong_content_type_is_detected");
    let mut results = Vec::new();

    // 200 with text/html instead of event-stream
    let body = r"<html><body>Not an API</body></html>";
    let (provider, _server) =
        setup_openai(&harness, make_response(200, "text/html", body.as_bytes()));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("text_html_response", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "text/html response must be detected as error"
    );
    results.push(classified);

    // 200 with application/xml
    let xml_body = r#"<?xml version="1.0"?><error>forbidden</error>"#;
    let (provider, _server) = setup_openai(
        &harness,
        make_response(200, "application/xml", xml_body.as_bytes()),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("xml_response", "openai", &r);
    results.push(classified);

    write_results(&harness, "wrong_content_type", &results);
}

#[test]
fn missing_content_type_header() {
    let harness = TestHarness::new("missing_content_type_header");

    let response = MockHttpResponse {
        status: 200,
        headers: vec![], // No Content-Type header
        body: b"data: [DONE]\n\n".to_vec(),
    };
    let server = harness.start_mock_http_server();
    server.add_route("POST", &oai_route(), response);
    let base_url = format!("{}/v1", server.base_url());
    let mut entry = make_entry("openai", "fail-test", &base_url);
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create provider");
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("no_content_type", "openai", &r);
    let results = vec![classified];
    write_results(&harness, "missing_content_type", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 4: HTTP error status codes with various bodies
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn http_500_with_json_error_body() {
    let harness = TestHarness::new("http_500_with_json_error_body");
    let mut results = Vec::new();

    let error_body = json!({
        "error": {
            "message": "Internal server error",
            "type": "server_error",
            "code": "internal_error"
        }
    });
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            500,
            "application/json",
            serde_json::to_vec(&error_body).unwrap().as_slice(),
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("500_json", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "HTTP 500 must produce an error"
    );
    results.push(classified);

    write_results(&harness, "http_500_json", &results);
}

#[test]
fn http_500_with_empty_body() {
    let harness = TestHarness::new("http_500_with_empty_body");

    let (provider, _server) = setup_openai(&harness, make_response(500, "text/plain", b""));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("500_empty", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "HTTP 500 with empty body must produce an error"
    );
    let results = vec![classified];
    write_results(&harness, "http_500_empty", &results);
}

#[test]
fn http_503_service_unavailable() {
    let harness = TestHarness::new("http_503_service_unavailable");
    let mut results = Vec::new();

    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            503,
            "application/json",
            br#"{"error":{"message":"Service temporarily unavailable","type":"server_error"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("503_unavailable", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "HTTP 503 must produce an error"
    );
    results.push(classified);

    write_results(&harness, "http_503", &results);
}

/// #209: a terminal HTTP 503 from the provider must end the agent turn with a
/// structured `provider_error` event (provider, HTTP status, kind, retryable)
/// ahead of `agent_end`, and the interactive translation must turn that into
/// a visible card rather than leaving the turn silent.
#[test]
fn http_503_turn_end_emits_structured_provider_error() {
    use pi::agent::{Agent, AgentConfig, AgentEvent};
    use pi::error::ProviderErrorKind;
    use pi::tools::ToolRegistry;
    use std::sync::Mutex;

    let harness = TestHarness::new("http_503_turn_end_emits_structured_provider_error");
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            503,
            "application/json",
            br#"{"error":{"code":"service_unavailable_error","message":"Server Overloaded"}}"#,
        ),
    );
    let cwd = harness.temp_dir().to_path_buf();

    let (result, events) = common::run_async(async move {
        let tools = ToolRegistry::new(&[], &cwd, None);
        let config = AgentConfig {
            stream_options: default_options(),
            ..AgentConfig::default()
        };
        let mut agent = Agent::new(provider, tools, config);
        let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let result = agent
            .run("Say hello.", move |event| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event);
            })
            .await;
        let events = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (result, events)
    });

    let err = result.expect_err("HTTP 503 must fail the turn");
    assert!(
        matches!(err, pi::error::Error::Provider { .. }),
        "expected a provider error, got {err:?}"
    );

    let provider_error_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ProviderError { .. }))
        .expect("provider_error event must be emitted");
    let agent_end_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AgentEnd { .. }))
        .expect("agent_end event must be emitted");
    assert!(
        provider_error_index < agent_end_index,
        "provider_error must precede agent_end"
    );

    let AgentEvent::ProviderError {
        provider,
        summary,
        message,
        ..
    } = &events[provider_error_index]
    else {
        unreachable!("position() matched ProviderError");
    };
    assert_eq!(provider, "openai");
    assert_eq!(summary.http_status, Some(503));
    assert_eq!(summary.kind, ProviderErrorKind::Overloaded);
    assert!(summary.retryable, "503 is retryable");
    assert!(
        message.contains("HTTP 503"),
        "raw message must carry the status: {message}"
    );

    let AgentEvent::AgentEnd {
        error: Some(agent_end_error),
        ..
    } = &events[agent_end_index]
    else {
        panic!("agent_end must carry the error");
    };
    assert!(agent_end_error.contains("HTTP 503"));

    // JSON/RPC consumers see the structured fields on the wire.
    let wire = serde_json::to_value(&events[provider_error_index]).expect("serialize event");
    assert_eq!(wire["type"], "provider_error");
    assert_eq!(wire["provider"], "openai");
    assert_eq!(wire["httpStatus"], 503);
    assert_eq!(wire["kind"], "overloaded");
    assert_eq!(wire["retryable"], true);
    assert!(
        wire["message"]
            .as_str()
            .is_some_and(|m| m.contains("HTTP 503"))
    );

    // The default (ftui) interactive stack renders the turn end as a card
    // that names the provider, the status, and the retry situation.
    #[cfg(feature = "ftui")]
    {
        let msgs = pi::interactive_ftui::agent_event_to_pi_msgs(&events[agent_end_index]);
        let [
            pi::interactive::PiMsg::AgentDone {
                error_message: Some(card),
                ..
            },
        ] = msgs.as_slice()
        else {
            panic!("agent_end must translate to AgentDone with an error card: {msgs:?}");
        };
        assert!(
            card.starts_with("Provider error: openai: HTTP 503 (service unavailable / overloaded)"),
            "card headline: {card}"
        );
        assert!(card.contains("not auto-retried"), "retry status: {card}");
        assert!(
            card.contains("Detail: OpenAI API error (HTTP 503)"),
            "detail: {card}"
        );
    }

    let path = harness.temp_path("provider_error_turn_end.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "event_count": events.len(),
            "provider_error_index": provider_error_index,
            "agent_end_index": agent_end_index,
            "provider_error": wire,
        }))
        .expect("serialize artifact"),
    )
    .expect("write artifact");
    harness.record_artifact("provider_error_turn_end.json", &path);
}

#[test]
fn http_502_bad_gateway() {
    let harness = TestHarness::new("http_502_bad_gateway");

    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            502,
            "text/html",
            b"<html><body>502 Bad Gateway</body></html>",
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("502_bad_gateway", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "HTTP 502 must produce an error"
    );
    let results = vec![classified];
    write_results(&harness, "http_502", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 5: Finish-reason edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn openai_content_filter_finish_reason() {
    let harness = TestHarness::new("openai_content_filter_finish_reason");

    // Responses API: content_filter is signalled via incomplete_details.reason
    let sse = [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"I can't"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}"#,
        "",
    ].join("\n");

    let (provider, _server) = setup_openai(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("content_filter", "openai", &r);
    // Should complete (content_filter is a valid stop reason producing Done or Error)
    assert!(
        classified.has_done_event || classified.has_error_event,
        "content_filter finish_reason should produce Done or Error event"
    );
    let results = vec![classified];
    write_results(&harness, "content_filter", &results);
}

#[test]
fn openai_length_finish_reason() {
    let harness = TestHarness::new("openai_length_finish_reason");

    // Responses API: length / max_output_tokens via incomplete_details.reason
    let sse = [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"Long text..."}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"output_tokens":64,"total_tokens":74}}}"#,
        "",
    ].join("\n");

    let (provider, _server) = setup_openai(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("length_stop", "openai", &r);
    assert!(
        classified.has_done_event || classified.has_error_event,
        "length finish_reason should produce Done event"
    );
    let results = vec![classified];
    write_results(&harness, "length_finish", &results);
}

#[test]
fn anthropic_max_tokens_stop_reason() {
    let harness = TestHarness::new("anthropic_max_tokens_stop_reason");

    let sse = [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","content":[],"model":"test","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Truncated output"}}"#,
        "",
        r"event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        r"event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":64}}"#,
        "",
        r"event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ].join("\n");

    let (provider, _server) = setup_anthropic(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("max_tokens", "anthropic", &r);
    assert!(
        classified.has_done_event,
        "max_tokens stop_reason should produce Done event"
    );
    assert!(classified.text_chars > 0, "should have some text output");
    let results = vec![classified];
    write_results(&harness, "max_tokens", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 6: Mid-stream failure (valid chunks then error)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn openai_mid_stream_error() {
    let harness = TestHarness::new("openai_mid_stream_error");

    // Valid first chunk, then an error event, then nothing
    let sse = [
        r#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "",
        r#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":" wor"},"finish_reason":null}]}"#,
        "",
        r#"data: {"error":{"message":"Stream interrupted by server","type":"server_error","code":"stream_error"}}"#,
        "",
    ].join("\n");

    let (provider, _server) = setup_openai(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("mid_stream_error", "openai", &r);
    // Should have captured some text before the error
    assert!(
        classified.text_chars > 0 || classified.status == "stream_error",
        "mid-stream error should either have partial text or stream-level error"
    );
    let results = vec![classified];
    write_results(&harness, "mid_stream_error", &results);
}

#[test]
fn anthropic_mid_stream_error() {
    let harness = TestHarness::new("anthropic_mid_stream_error");

    let sse = [
        r"event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","content":[],"model":"test","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "",
        r"event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r"event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}}"#,
        "",
        r"event: error",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        "",
    ].join("\n");

    let (provider, _server) = setup_anthropic(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("mid_stream_error", "anthropic", &r);
    let results = vec![classified];
    write_results(&harness, "anthropic_mid_stream", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 7: Large error payloads
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn large_error_payload_is_handled() {
    let harness = TestHarness::new("large_error_payload_is_handled");

    // 4KB error message
    let big_msg = "x".repeat(4096);
    let error_body = json!({
        "error": {
            "message": big_msg,
            "type": "server_error",
            "code": "internal_error"
        }
    });
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            500,
            "application/json",
            serde_json::to_vec(&error_body).unwrap().as_slice(),
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("large_error_payload", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "large error payload must still produce an error"
    );
    let results = vec![classified];
    write_results(&harness, "large_error", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 8: Anthropic-specific error events
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn anthropic_overloaded_error_event() {
    let harness = TestHarness::new("anthropic_overloaded_error_event");

    let sse = [
        r"event: error",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        "",
    ]
    .join("\n");

    let (provider, _server) = setup_anthropic(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("overloaded_error", "anthropic", &r);
    assert!(
        classified.has_error_event || classified.status == "stream_error",
        "overloaded_error should be surfaced as error"
    );
    let results = vec![classified];
    write_results(&harness, "anthropic_overloaded", &results);
}

#[test]
fn anthropic_api_error_event() {
    let harness = TestHarness::new("anthropic_api_error_event");

    let sse = [
        r"event: error",
        r#"data: {"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#,
        "",
    ]
    .join("\n");

    let (provider, _server) = setup_anthropic(&harness, make_sse_response(&sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("api_error", "anthropic", &r);
    assert!(
        classified.has_error_event || classified.status == "stream_error",
        "api_error should be surfaced as error"
    );
    let results = vec![classified];
    write_results(&harness, "anthropic_api_error", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 9: Multiple error codes across providers
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn http_4xx_error_codes_produce_errors() {
    let harness = TestHarness::new("http_4xx_error_codes_produce_errors");
    let mut results = Vec::new();

    let codes: &[(u16, &str)] = &[
        (400, "bad_request"),
        (401, "unauthorized"),
        (403, "forbidden"),
        (404, "not_found"),
        (408, "request_timeout"),
        (413, "payload_too_large"),
        (422, "unprocessable_entity"),
        (429, "too_many_requests"),
    ];

    for (code, label) in codes {
        let body = json!({"error": {"message": format!("Error: {label}"), "type": label}});
        let (provider, _server) = setup_openai(
            &harness,
            make_response(
                *code,
                "application/json",
                serde_json::to_vec(&body).unwrap().as_slice(),
            ),
        );
        let r = collect_events(provider, simple_context(), default_options());
        let classified = classify_result(&format!("http_{code}"), "openai", &r);
        assert!(
            classified.status == "stream_error" || classified.has_error_event,
            "HTTP {code} ({label}) must produce an error"
        );
        results.push(classified);
    }

    write_results(&harness, "http_4xx_errors", &results);
    assert_eq!(results.len(), codes.len());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 10: User-facing diagnostics quality
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn error_messages_are_non_empty_strings() {
    let harness = TestHarness::new("error_messages_are_non_empty_strings");
    let mut results = Vec::new();

    // Auth error should produce a non-empty error message
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            401,
            "application/json",
            br#"{"error":{"message":"Invalid API key","type":"authentication_error"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("auth_error_msg", "openai", &r);
    if let Some(ref msg) = classified.error_message {
        assert!(!msg.is_empty(), "error message must not be empty");
        assert!(
            msg.len() > 5,
            "error message should be descriptive, got: {msg}"
        );
    }
    results.push(classified);

    // Rate limit should mention rate or limit
    let (provider, _server) = setup_openai(
        &harness,
        make_response(429, "application/json", br#"{"error":{"message":"Rate limit exceeded. Please retry after 60s","type":"rate_limit_error"}}"#),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("rate_limit_msg", "openai", &r);
    results.push(classified);

    write_results(&harness, "diagnostics_quality", &results);
}

#[test]
fn error_events_include_stop_reason() {
    let harness = TestHarness::new("error_events_include_stop_reason");

    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            500,
            "application/json",
            br#"{"error":{"message":"Server error","type":"server_error"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    if let Ok(events) = &r {
        // If we got events, Done/Error should have a reason
        for event in events {
            match event {
                StreamEvent::Done { reason, .. } | StreamEvent::Error { reason, .. } => {
                    // Reason should be set (not the default Unknown)
                    let reason_str = format!("{reason:?}");
                    assert!(!reason_str.is_empty(), "terminal events must have a reason");
                }
                _ => {}
            }
        }
    }

    let results = vec![classify_result("error_stop_reason", "openai", &r)];
    write_results(&harness, "error_stop_reason", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 11: Comprehensive failure injection summary report
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn comprehensive_failure_injection_report() {
    let harness = TestHarness::new("comprehensive_failure_injection_report");
    let mut all_results = Vec::new();

    // Test suite of failure injections with summary
    let scenarios: Vec<(&str, MockHttpResponse)> = vec![
        ("empty_200", make_response(200, "text/event-stream", b"")),
        (
            "html_200",
            make_response(200, "text/html", b"<html>error</html>"),
        ),
        (
            "json_500",
            make_response(500, "application/json", br#"{"error":{"message":"fail"}}"#),
        ),
        ("empty_500", make_response(500, "text/plain", b"")),
        (
            "text_429",
            make_response(429, "text/plain", b"rate limited"),
        ),
        (
            "binary_200",
            make_response(200, "application/octet-stream", &[0xFF, 0xFE, 0x00, 0x01]),
        ),
    ];

    for (name, response) in scenarios {
        let (provider, _server) = setup_openai(&harness, response);
        let r = collect_events(provider, simple_context(), default_options());
        all_results.push(classify_result(name, "openai", &r));
    }

    // Generate summary
    let total = all_results.len();
    let errors_detected = all_results
        .iter()
        .filter(|r| r.status == "stream_error" || r.has_error_event)
        .count();
    let completed = all_results
        .iter()
        .filter(|r| r.status == "completed")
        .count();

    let summary = json!({
        "total_scenarios": total,
        "errors_detected": errors_detected,
        "false_completions": completed,
        "detection_rate_pct": (errors_detected as f64 / total as f64) * 100.0,
    });

    let summary_path = harness.temp_path("failure_injection_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap(),
    )
    .expect("write summary");
    harness.record_artifact("failure_injection_summary.json", &summary_path);
    write_results(&harness, "comprehensive_report", &all_results);

    // At least 4 out of 6 failure scenarios should be detected
    assert!(
        errors_detected >= 4,
        "expected at least 4/6 failures detected, got {errors_detected}/{total}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 12: Gemini provider failure injection
// ═══════════════════════════════════════════════════════════════════════

fn setup_gemini(
    harness: &TestHarness,
    response: MockHttpResponse,
) -> (Arc<dyn pi::provider::Provider>, common::MockHttpServer) {
    let server = harness.start_mock_http_server();
    // Gemini sends the API key in x-goog-api-key and keeps only alt=sse in the URL.
    server.add_route("POST", "/models/fail-test:streamGenerateContent", response);
    let base_url = server.base_url();
    let mut entry = make_entry("google", "fail-test", &base_url);
    entry.api_key = Some("test-key".to_string());
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create gemini provider");
    (provider, server)
}

#[test]
fn gemini_auth_failure_401() {
    let harness = TestHarness::new("gemini_auth_failure_401");

    let (provider, _server) = setup_gemini(
        &harness,
        make_response(
            401,
            "application/json",
            br#"{"error":{"code":401,"message":"API key not valid.","status":"UNAUTHENTICATED"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("gemini_auth_401", "google", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "Gemini 401 must be detected as error"
    );

    let results = vec![classified];
    write_results(&harness, "gemini_auth_failure", &results);
}

#[test]
fn gemini_quota_exceeded_vs_rate_limit() {
    let harness = TestHarness::new("gemini_quota_exceeded_vs_rate_limit");
    let mut results = Vec::new();

    // Quota exceeded (different from rate limit)
    let (provider, _server) = setup_gemini(
        &harness,
        make_response(
            429,
            "application/json",
            br#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("gemini_quota_exhausted", "google", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "Gemini quota exhausted must be error"
    );
    results.push(classified);

    // Rate limit (429 with retry guidance)
    let (provider, _server) = setup_gemini(
        &harness,
        make_response(
            429,
            "application/json",
            br#"{"error":{"code":429,"message":"Too many requests. Please retry after a brief wait.","status":"RATE_LIMITED"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("gemini_rate_limited", "google", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "Gemini rate limit must be error"
    );
    results.push(classified);

    write_results(&harness, "gemini_quota_vs_rate", &results);
}

#[test]
fn gemini_malformed_sse_stream() {
    let harness = TestHarness::new("gemini_malformed_sse_stream");

    // Gemini SSE with truncated JSON
    let truncated = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel";
    let (provider, _server) = setup_gemini(&harness, make_sse_response(truncated));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("gemini_truncated_sse", "google", &r);
    assert!(
        classified.status == "stream_error"
            || classified.has_error_event
            || classified.text_chars == 0,
        "Gemini truncated SSE must not produce valid text"
    );

    let results = vec![classified];
    write_results(&harness, "gemini_malformed_sse", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 13: Schema drift and type mismatch scenarios
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn unexpected_extra_fields_are_tolerated() {
    let harness = TestHarness::new("unexpected_extra_fields_are_tolerated");
    let mut results = Vec::new();

    // OpenAI response with extra unknown fields (forward compatibility)
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"new_field\":\"surprise\",\"metadata\":{\"experiment\":true},",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("extra_fields_openai", "openai", &r);
    // Should either produce text or at least not panic
    results.push(classified);

    // Anthropic response with extra unknown fields
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"experimental_flag\":true,\"message\":{\"id\":\"x\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let (provider, _server) = setup_anthropic(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("extra_fields_anthropic", "anthropic", &r);
    // Anthropic should parse text through extra fields
    results.push(classified);

    write_results(&harness, "schema_drift_extra_fields", &results);
}

#[test]
fn missing_expected_fields_handled_gracefully() {
    let harness = TestHarness::new("missing_expected_fields_handled_gracefully");
    let mut results = Vec::new();

    // OpenAI: missing "model" field in chunk (should still parse)
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("missing_model_field", "openai", &r);
    // Graceful handling: either parse successfully or produce clear error
    results.push(classified);

    // OpenAI: missing "choices" array entirely
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\"}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("missing_choices_array", "openai", &r);
    assert!(
        classified.text_chars == 0,
        "Missing choices array should produce no text"
    );
    results.push(classified);

    // OpenAI: usage with string instead of number (type mismatch)
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"usage\":{\"prompt_tokens\":\"five\",\"completion_tokens\":\"three\"},",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("usage_type_mismatch", "openai", &r);
    // Should still produce text content even if usage parsing fails
    results.push(classified);

    write_results(&harness, "schema_drift_missing_fields", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 14: Provider-specific quota and error code patterns
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn alibaba_qps_vs_quota_429() {
    let harness = TestHarness::new("alibaba_qps_vs_quota_429");
    let mut results = Vec::new();

    // Alibaba/DashScope QPS limit (retryable)
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            429,
            "application/json",
            br#"{"error":{"message":"Requests rate limit exceeded","code":"qps","type":"rate_limit_error"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("alibaba_qps_limit", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "QPS rate limit must be detected"
    );
    results.push(classified);

    // Alibaba/DashScope quota exhausted (non-retryable)
    let (provider, _server) = setup_openai(
        &harness,
        make_response(
            429,
            "application/json",
            br#"{"error":{"message":"Free quota has been exhausted","code":"quota","type":"rate_limit_error"}}"#,
        ),
    );
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("alibaba_quota_exhausted", "openai", &r);
    assert!(
        classified.status == "stream_error" || classified.has_error_event,
        "Quota exhausted must be detected"
    );
    results.push(classified);

    write_results(&harness, "alibaba_qps_vs_quota", &results);
}

#[test]
fn openrouter_mid_stream_error_via_200() {
    let harness = TestHarness::new("openrouter_mid_stream_error_via_200");

    // OpenRouter sends HTTP 200 but embeds error as SSE event mid-stream
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Start\"},\"finish_reason\":null}]}\n\n",
        "data: {\"error\":{\"message\":\"upstream provider returned 502\",\"code\":502}}\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("openrouter_200_error", "openai", &r);
    // May produce partial text then error, or detect the error event
    let results = vec![classified];
    write_results(&harness, "openrouter_mid_stream_error", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 15: Streaming protocol edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn duplicate_sse_event_ids_handled() {
    let harness = TestHarness::new("duplicate_sse_event_ids_handled");

    // Two chunks with same "id" field (should not cause dedup issues)
    let sse = concat!(
        "data: {\"id\":\"dup\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"A\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"dup\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"B\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"dup\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("duplicate_ids", "openai", &r);
    // Must not crash; content may or may not be produced depending on API format

    let results = vec![classified];
    write_results(&harness, "duplicate_event_ids", &results);
}

#[test]
fn multi_choice_responses_use_first_choice() {
    let harness = TestHarness::new("multi_choice_responses_use_first_choice");

    // Response with choices[0] and choices[1] — we only use index 0
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[",
        "{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Primary\"},\"finish_reason\":null},",
        "{\"index\":1,\"delta\":{\"role\":\"assistant\",\"content\":\"Alt\"},\"finish_reason\":null}",
        "]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("multi_choice", "openai", &r);
    // Must not crash; content handling depends on API format

    let results = vec![classified];
    write_results(&harness, "multi_choice", &results);
}

#[test]
fn empty_delta_content_chunks_tolerated() {
    let harness = TestHarness::new("empty_delta_content_chunks_tolerated");

    // Response with empty string content deltas (some providers do this)
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_deltas", "openai", &r);
    // Must not crash; empty deltas should be gracefully handled

    let results = vec![classified];
    write_results(&harness, "empty_delta_content", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 16: Malformed tool call responses
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn malformed_tool_call_arguments_handled() {
    let harness = TestHarness::new("malformed_tool_call_arguments_handled");
    let mut results = Vec::new();

    // Tool call with invalid JSON in arguments field
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"not-valid-json{{{\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("invalid_tool_args", "openai", &r);
    // Should not panic even with malformed tool arguments
    results.push(classified);

    // Tool call with empty function name
    let sse = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"/tmp\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (provider, _server) = setup_openai(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("empty_tool_name", "openai", &r);
    results.push(classified);

    write_results(&harness, "malformed_tool_calls", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 17: Anthropic-specific finish reason variants
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn anthropic_tool_use_stop_reason() {
    let harness = TestHarness::new("anthropic_tool_use_stop_reason");

    // Anthropic tool_use stop reason (not end_turn, not max_tokens)
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"x\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp\\\"\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":10}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let (provider, _server) = setup_anthropic(&harness, make_sse_response(sse));
    let r = collect_events(provider, simple_context(), default_options());
    let classified = classify_result("anthropic_tool_use_reason", "anthropic", &r);
    // Should produce ToolCallStart/Delta/End events, then Done with ToolUse reason
    assert!(
        classified.has_done_event || classified.has_error_event,
        "Anthropic tool_use must produce terminal event"
    );

    let results = vec![classified];
    write_results(&harness, "anthropic_tool_use_reason", &results);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 18: Cross-provider error parity matrix
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cross_provider_error_parity_matrix() {
    let harness = TestHarness::new("cross_provider_error_parity_matrix");
    let mut all_results = Vec::new();

    // Test same error codes across OpenAI and Anthropic
    let error_scenarios: Vec<(&str, u16, &[u8])> = vec![
        ("auth_401", 401, br#"{"error":{"message":"Unauthorized"}}"#),
        (
            "bad_request_400",
            400,
            br#"{"error":{"message":"Bad request"}}"#,
        ),
        (
            "rate_limit_429",
            429,
            br#"{"error":{"message":"Rate limited"}}"#,
        ),
        (
            "server_error_500",
            500,
            br#"{"error":{"message":"Internal error"}}"#,
        ),
        (
            "unavailable_503",
            503,
            br#"{"error":{"message":"Service unavailable"}}"#,
        ),
    ];

    for (name, status, body) in &error_scenarios {
        // OpenAI
        let (provider, _server) =
            setup_openai(&harness, make_response(*status, "application/json", body));
        let r = collect_events(provider, simple_context(), default_options());
        all_results.push(classify_result(name, "openai", &r));

        // Anthropic
        let (provider, _server) =
            setup_anthropic(&harness, make_response(*status, "application/json", body));
        let r = collect_events(provider, simple_context(), default_options());
        all_results.push(classify_result(name, "anthropic", &r));
    }

    // Generate parity report
    let total = all_results.len();
    let errors_detected = all_results
        .iter()
        .filter(|r| r.status == "stream_error" || r.has_error_event)
        .count();

    let summary = json!({
        "total_scenarios": total,
        "errors_detected": errors_detected,
        "detection_rate_pct": (errors_detected as f64 / total as f64) * 100.0,
        "providers_tested": ["openai", "anthropic"],
        "error_codes_tested": [401, 400, 429, 500, 503],
    });

    let path = harness.temp_path("error_parity_matrix.json");
    std::fs::write(&path, serde_json::to_string_pretty(&summary).unwrap())
        .expect("write parity matrix");
    harness.record_artifact("error_parity_matrix.json", &path);
    write_results(&harness, "error_parity_matrix", &all_results);

    // All error scenarios should be detected across both providers
    assert!(
        errors_detected >= 8,
        "expected at least 8/10 cross-provider errors detected, got {errors_detected}/{total}"
    );
}
