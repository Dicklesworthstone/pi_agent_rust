//! Deterministic provider error-path tests (offline).
//!
//! These tests use the `MockHttpServer` harness rather than VCR cassettes so they can:
//! - validate HTTP error handling (4xx/5xx) without API keys
//! - validate malformed SSE / invalid JSON behavior without relying on real providers

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::model::{Message, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};

fn context_for(prompt: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(prompt.to_string()),
            timestamp: 0,
        })],
        tools: Vec::new(),
    }
}

fn options_with_key(key: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(key.to_string()),
        ..Default::default()
    }
}

#[test]
fn openai_http_500_is_reported() {
    let harness = TestHarness::new("openai_http_500_is_reported");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/chat/completions",
        common::harness::MockHttpResponse::text(500, "boom"),
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
            .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
        let context = context_for("Trigger server error.");
        let options = options_with_key("test-key");

        let err = provider
            .stream(&context, &options)
            .await
            .err()
            .expect("expected error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    });
}

#[test]
fn anthropic_http_500_is_reported() {
    let harness = TestHarness::new("anthropic_http_500_is_reported");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/messages",
        common::harness::MockHttpResponse::text(500, "boom"),
    );

    common::run_async(async move {
        let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
            .with_base_url(format!("{}/v1/messages", server.base_url()));
        let context = context_for("Trigger server error.");
        let options = options_with_key("test-key");

        let err = provider
            .stream(&context, &options)
            .await
            .err()
            .expect("expected error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    });
}

#[test]
fn gemini_http_500_is_reported() {
    let harness = TestHarness::new("gemini_http_500_is_reported");
    let server = harness.start_mock_http_server();

    let model = "gemini-test";
    let api_key = "test-key";
    let path = format!("/models/{model}:streamGenerateContent?alt=sse&key={api_key}");
    server.add_route(
        "POST",
        &path,
        common::harness::MockHttpResponse::text(500, "boom"),
    );

    common::run_async(async move {
        let provider =
            pi::providers::gemini::GeminiProvider::new(model).with_base_url(server.base_url());
        let context = context_for("Trigger server error.");
        let options = options_with_key(api_key);

        let err = provider
            .stream(&context, &options)
            .await
            .err()
            .expect("expected error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    });
}

#[test]
fn azure_http_500_is_reported() {
    let harness = TestHarness::new("azure_http_500_is_reported");
    let server = harness.start_mock_http_server();

    let deployment = "gpt-test";
    let api_version = "2024-02-15-preview";
    let path =
        format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
    server.add_route(
        "POST",
        &path,
        common::harness::MockHttpResponse::text(500, "boom"),
    );

    common::run_async(async move {
        let endpoint = format!("{}{path}", server.base_url());
        let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
            .with_endpoint_url(endpoint);
        let context = context_for("Trigger server error.");
        let options = options_with_key("test-key");

        let err = provider
            .stream(&context, &options)
            .await
            .err()
            .expect("expected error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    });
}

#[test]
fn openai_invalid_json_event_fails_stream() {
    let harness = TestHarness::new("openai_invalid_json_event_fails_stream");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/chat/completions",
        common::harness::MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: b"data: {not json}\n\n".to_vec(),
        },
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
            .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
        let context = context_for("Trigger invalid json.");
        let options = options_with_key("test-key");

        let mut stream = provider.stream(&context, &options).await.expect("stream");
        let err = stream.next().await.expect("expected one item").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("JSON parse error"),
            "unexpected stream error: {message}"
        );
    });
}

#[test]
fn azure_invalid_json_event_fails_stream() {
    let harness = TestHarness::new("azure_invalid_json_event_fails_stream");
    let server = harness.start_mock_http_server();

    let deployment = "gpt-test";
    let api_version = "2024-02-15-preview";
    let path =
        format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
    server.add_route(
        "POST",
        &path,
        common::harness::MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: b"data: {not json}\n\n".to_vec(),
        },
    );

    common::run_async(async move {
        let endpoint = format!("{}{path}", server.base_url());
        let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
            .with_endpoint_url(endpoint);
        let context = context_for("Trigger invalid json.");
        let options = options_with_key("test-key");

        let mut stream = provider.stream(&context, &options).await.expect("stream");
        let err = stream.next().await.expect("expected one item").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("JSON parse error"),
            "unexpected stream error: {message}"
        );
    });
}

#[test]
fn openai_invalid_utf8_in_sse_is_reported() {
    let harness = TestHarness::new("openai_invalid_utf8_in_sse_is_reported");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/chat/completions",
        common::harness::MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: b"data: \xFF\xFF\n\n".to_vec(),
        },
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
            .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
        let context = context_for("Trigger invalid utf8.");
        let options = options_with_key("test-key");

        let mut stream = provider.stream(&context, &options).await.expect("stream");
        let err = stream.next().await.expect("expected one item").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("SSE error"),
            "unexpected stream error: {message}"
        );
    });
}
