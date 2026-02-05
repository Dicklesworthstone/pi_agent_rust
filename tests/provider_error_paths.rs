//! Deterministic provider error-path tests (offline).
//!
//! These tests use VCR cassette playback for HTTP error handling and malformed SSE validation
//! without requiring API keys or real provider endpoints.
//!
//! The `openai_invalid_utf8_in_sse_is_reported` test is the sole exception: it requires raw
//! byte injection (invalid UTF-8) which VCR cassettes cannot represent, so it uses
//! `MockHttpServer`.

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::http::client::Client;
use pi::model::{Message, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde_json::json;

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

// ---------------------------------------------------------------------------
// VCR cassette helpers
// ---------------------------------------------------------------------------

/// Build a VCR-backed HTTP client with a single pre-built cassette interaction.
/// Returns (Client, `TempDir`) — caller must keep `TempDir` alive for the test duration.
fn vcr_client(
    test_name: &str,
    url: &str,
    request_body: serde_json::Value,
    status: u16,
    response_headers: Vec<(String, String)>,
    response_chunks: Vec<String>,
) -> (Client, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
        recorded_at: "2026-02-05T00:00:00.000Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: Vec::new(), // VCR matching ignores headers
                body: Some(request_body),
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: response_headers,
                body_chunks: response_chunks,
            },
        }],
    };
    let serialized = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(temp.path().join(format!("{test_name}.json")), serialized)
        .expect("write cassette");
    let recorder = VcrRecorder::new_with(test_name, VcrMode::Playback, temp.path());
    let client = Client::new().with_vcr(recorder);
    (client, temp)
}

/// Build the request body that `AnthropicProvider` serializes for a simple prompt.
fn anthropic_body(model: &str, prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 8192,
        "messages": [{"content": [{"text": prompt, "type": "text"}], "role": "user"}],
        "model": model,
        "stream": true,
    })
}

/// Build the request body that `OpenAIProvider` serializes for a simple prompt.
fn openai_body(model: &str, prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 4096,
        "messages": [{"content": prompt, "role": "user"}],
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
    })
}

/// Build the request body that `GeminiProvider` serializes for a simple prompt.
fn gemini_body(prompt: &str) -> serde_json::Value {
    json!({
        "contents": [{"parts": [{"text": prompt}], "role": "user"}],
        "generationConfig": {"candidateCount": 1, "maxOutputTokens": 8192},
    })
}

/// Build the request body that `AzureOpenAIProvider` serializes for a simple prompt.
fn azure_body(prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 4096,
        "messages": [{"content": prompt, "role": "user"}],
        "stream": true,
        "stream_options": {"include_usage": true},
    })
}

// Convenience: error response with text/plain body
fn text_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "text/plain".to_string())]
}

fn sse_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "text/event-stream".to_string())]
}

// ---------------------------------------------------------------------------
// HTTP 500 Tests
// ---------------------------------------------------------------------------

#[test]
fn openai_http_500_is_reported() {
    let (client, _dir) = vcr_client(
        "openai_http_500_is_reported",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger server error."),
        500,
        text_headers(),
        vec!["boom".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger server error."),
                &options_with_key("test-key"),
            )
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
    let (client, _dir) = vcr_client(
        "anthropic_http_500_is_reported",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger server error."),
        500,
        text_headers(),
        vec!["boom".to_string()],
    );
    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger server error."),
                &options_with_key("test-key"),
            )
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
    let model = "gemini-test";
    let api_key = "test-key";
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse&key={api_key}"
    );
    let (client, _dir) = vcr_client(
        "gemini_http_500_is_reported",
        &url,
        gemini_body("Trigger server error."),
        500,
        text_headers(),
        vec!["boom".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::gemini::GeminiProvider::new(model).with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger server error."),
                &options_with_key(api_key),
            )
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
    let deployment = "gpt-test";
    let api_version = "2024-02-15-preview";
    let endpoint = format!(
        "https://fake.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
    );
    let (client, _dir) = vcr_client(
        "azure_http_500_is_reported",
        &endpoint,
        azure_body("Trigger server error."),
        500,
        text_headers(),
        vec!["boom".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
            .with_client(client)
            .with_endpoint_url(endpoint);
        let err = provider
            .stream(
                &context_for("Trigger server error."),
                &options_with_key("test-key"),
            )
            .await
            .err()
            .expect("expected error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    });
}

// ---------------------------------------------------------------------------
// Malformed SSE / Invalid JSON Tests (VCR)
// ---------------------------------------------------------------------------

#[test]
fn openai_invalid_json_event_fails_stream() {
    let (client, _dir) = vcr_client(
        "openai_invalid_json_event_fails_stream",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger invalid json."),
        200,
        sse_headers(),
        vec!["data: {not json}\n\n".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger invalid json."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream");
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
    let deployment = "gpt-test";
    let api_version = "2024-02-15-preview";
    let endpoint = format!(
        "https://fake.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
    );
    let (client, _dir) = vcr_client(
        "azure_invalid_json_event_fails_stream",
        &endpoint,
        azure_body("Trigger invalid json."),
        200,
        sse_headers(),
        vec!["data: {not json}\n\n".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
            .with_client(client)
            .with_endpoint_url(endpoint);
        let mut stream = provider
            .stream(
                &context_for("Trigger invalid json."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream");
        let err = stream.next().await.expect("expected one item").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("JSON parse error"),
            "unexpected stream error: {message}"
        );
    });
}

// ---------------------------------------------------------------------------
// Anthropic / Gemini Malformed SSE (VCR)
// ---------------------------------------------------------------------------

#[test]
fn anthropic_invalid_json_event_fails_stream() {
    let (client, _dir) = vcr_client(
        "anthropic_invalid_json_event_fails_stream",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger invalid json."),
        200,
        sse_headers(),
        vec!["event: message_start\ndata: {not json}\n\n".to_string()],
    );
    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger invalid json."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream");
        let mut found_error = false;
        while let Some(item) = stream.next().await {
            if let Err(err) = item {
                found_error = true;
                let message = err.to_string();
                assert!(
                    message.contains("JSON") || message.contains("parse"),
                    "unexpected stream error: {message}"
                );
                break;
            }
        }
        assert!(found_error, "expected a stream error for invalid JSON");
    });
}

#[test]
fn gemini_invalid_json_event_fails_stream() {
    let model = "gemini-test";
    let api_key = "test-key";
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse&key={api_key}"
    );
    let (client, _dir) = vcr_client(
        "gemini_invalid_json_event_fails_stream",
        &url,
        gemini_body("Trigger invalid json."),
        200,
        sse_headers(),
        vec!["data: {broken json\n\n".to_string()],
    );
    common::run_async(async move {
        let provider = pi::providers::gemini::GeminiProvider::new(model).with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger invalid json."),
                &options_with_key(api_key),
            )
            .await
            .expect("stream");
        let mut found_error = false;
        while let Some(item) = stream.next().await {
            if let Err(err) = item {
                found_error = true;
                let message = err.to_string();
                assert!(
                    message.contains("JSON") || message.contains("parse"),
                    "unexpected stream error: {message}"
                );
                break;
            }
        }
        assert!(found_error, "expected a stream error for invalid JSON");
    });
}

// ---------------------------------------------------------------------------
// Invalid UTF-8 Test (MockHttpServer — allowlisted)
//
// VCR cassettes store body_chunks as UTF-8 strings and cannot represent raw
// invalid byte sequences. This is the only test that genuinely requires byte-
// level control, so MockHttpServer is retained.
// ---------------------------------------------------------------------------

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
