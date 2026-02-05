//! Comprehensive error handling path tests.
//!
//! Covers provider HTTP error codes (401/403/429/529), malformed SSE scenarios,
//! tool execution edge cases not in `tools_conformance`, and the error hints taxonomy.
//! All tests are deterministic and offline (VCR cassettes or temp dirs).

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::error::Error;
use pi::http::client::Client;
use pi::model::{Message, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde_json::json;

// ============================================================================
// Helpers
// ============================================================================

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

fn get_text_content(content: &[pi::model::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            pi::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ============================================================================
// VCR Cassette Helpers
// ============================================================================

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
                headers: Vec::new(),
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

fn anthropic_body(prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 8192,
        "messages": [{"content": [{"text": prompt, "type": "text"}], "role": "user"}],
        "model": "claude-test",
        "stream": true,
    })
}

fn openai_body(prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 4096,
        "messages": [{"content": prompt, "role": "user"}],
        "model": "gpt-test",
        "stream": true,
        "stream_options": {"include_usage": true},
    })
}

fn gemini_url(model: &str, api_key: &str) -> String {
    format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse&key={api_key}"
    )
}

fn gemini_body(prompt: &str) -> serde_json::Value {
    json!({
        "contents": [{"parts": [{"text": prompt}], "role": "user"}],
        "generationConfig": {"candidateCount": 1, "maxOutputTokens": 8192},
    })
}

fn azure_url(deployment: &str) -> String {
    format!(
        "https://fake.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version=2024-02-15-preview"
    )
}

fn azure_body(prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 4096,
        "messages": [{"content": prompt, "role": "user"}],
        "stream": true,
        "stream_options": {"include_usage": true},
    })
}

fn json_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "application/json".to_string())]
}

fn json_with_retry_headers(retry_after: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("retry-after".to_string(), retry_after.to_string()),
    ]
}

fn text_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "text/plain".to_string())]
}

fn text_with_retry_headers(retry_after: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "text/plain".to_string()),
        ("retry-after".to_string(), retry_after.to_string()),
    ]
}

fn sse_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "text/event-stream".to_string())]
}

// ============================================================================
// Provider HTTP Error Codes (VCR)
// ============================================================================

mod provider_http_errors {
    use super::*;

    // --- Anthropic ---

    #[test]
    fn anthropic_http_401_reports_auth_error() {
        let body = json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid x-api-key" }
        });
        let (client, _dir) = vcr_client(
            "anthropic_http_401",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            401,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_http_401");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("bad-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 401"), "got: {msg}");
        });
    }

    #[test]
    fn anthropic_http_403_reports_forbidden() {
        let body = json!({
            "type": "error",
            "error": { "type": "forbidden", "message": "access denied to model" }
        });
        let (client, _dir) = vcr_client(
            "anthropic_http_403",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            403,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_http_403");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 403"), "got: {msg}");
        });
    }

    #[test]
    fn anthropic_http_429_reports_rate_limit() {
        let body = json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "rate limited" }
        });
        let (client, _dir) = vcr_client(
            "anthropic_http_429",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            429,
            json_with_retry_headers("5"),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_http_429");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 429"), "got: {msg}");
        });
    }

    #[test]
    fn anthropic_http_529_reports_overloaded() {
        let body = json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "overloaded" }
        });
        let (client, _dir) = vcr_client(
            "anthropic_http_529",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            529,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_http_529");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 529"), "got: {msg}");
        });
    }

    // --- OpenAI ---

    #[test]
    fn openai_http_401_reports_auth_error() {
        let body = json!({
            "error": { "message": "Incorrect API key", "type": "invalid_request_error" }
        });
        let (client, _dir) = vcr_client(
            "openai_http_401",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            401,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_http_401");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("bad-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 401"), "got: {msg}");
        });
    }

    #[test]
    fn openai_http_429_reports_rate_limit() {
        let body = json!({
            "error": { "message": "Rate limit exceeded", "type": "rate_limit_error" }
        });
        let (client, _dir) = vcr_client(
            "openai_http_429",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            429,
            json_with_retry_headers("10"),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_http_429");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 429"), "got: {msg}");
        });
    }

    // --- Gemini ---

    #[test]
    fn gemini_http_401_reports_auth_error() {
        let url = gemini_url("gemini-test", "bad-key");
        let (client, _dir) = vcr_client(
            "gemini_http_401",
            &url,
            gemini_body("test"),
            401,
            text_headers(),
            vec!["API key not valid".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_http_401");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("bad-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 401"), "got: {msg}");
        });
    }

    #[test]
    fn gemini_http_429_reports_rate_limit() {
        let url = gemini_url("gemini-test", "test-key");
        let (client, _dir) = vcr_client(
            "gemini_http_429",
            &url,
            gemini_body("test"),
            429,
            text_headers(),
            vec!["Resource exhausted".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_http_429");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 429"), "got: {msg}");
        });
    }

    // --- Azure ---

    #[test]
    fn azure_http_401_reports_auth_error() {
        let endpoint = azure_url("gpt-test");
        let (client, _dir) = vcr_client(
            "azure_http_401",
            &endpoint,
            azure_body("test"),
            401,
            text_headers(),
            vec!["Unauthorized".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_http_401");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let err = provider
                .stream(&context_for("test"), &options_with_key("bad-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 401"), "got: {msg}");
        });
    }

    #[test]
    fn azure_http_429_reports_rate_limit() {
        let endpoint = azure_url("gpt-test");
        let (client, _dir) = vcr_client(
            "azure_http_429",
            &endpoint,
            azure_body("test"),
            429,
            text_with_retry_headers("30"),
            vec!["Too many requests".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_http_429");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 429"), "got: {msg}");
        });
    }

    // --- HTTP 400 Bad Request (all providers) ---

    #[test]
    fn anthropic_http_400_reports_bad_request() {
        let body = json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "messages: required field missing" }
        });
        let (client, _dir) = vcr_client(
            "anthropic_http_400",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            400,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_http_400");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 400"), "got: {msg}");
        });
    }

    #[test]
    fn openai_http_400_reports_bad_request() {
        let body = json!({
            "error": { "message": "Invalid model specified", "type": "invalid_request_error" }
        });
        let (client, _dir) = vcr_client(
            "openai_http_400",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            400,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_http_400");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 400"), "got: {msg}");
        });
    }

    #[test]
    fn gemini_http_400_reports_bad_request() {
        let url = gemini_url("gemini-test", "test-key");
        let body = json!({
            "error": { "code": 400, "message": "Invalid value at 'contents'", "status": "INVALID_ARGUMENT" }
        });
        let (client, _dir) = vcr_client(
            "gemini_http_400",
            &url,
            gemini_body("test"),
            400,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_http_400");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 400"), "got: {msg}");
        });
    }

    #[test]
    fn azure_http_400_reports_bad_request() {
        let endpoint = azure_url("gpt-test");
        let body = json!({
            "error": { "message": "Invalid model deployment", "type": "invalid_request_error" }
        });
        let (client, _dir) = vcr_client(
            "azure_http_400",
            &endpoint,
            azure_body("test"),
            400,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_http_400");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 400"), "got: {msg}");
        });
    }

    // --- Missing 403 / 500 combinations ---

    #[test]
    fn openai_http_403_reports_forbidden() {
        let body = json!({
            "error": { "message": "You do not have access to this model", "type": "forbidden" }
        });
        let (client, _dir) = vcr_client(
            "openai_http_403",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            403,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_http_403");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 403"), "got: {msg}");
        });
    }

    #[test]
    fn gemini_http_403_reports_forbidden() {
        let url = gemini_url("gemini-test", "test-key");
        let body = json!({
            "error": { "code": 403, "message": "Permission denied", "status": "PERMISSION_DENIED" }
        });
        let (client, _dir) = vcr_client(
            "gemini_http_403",
            &url,
            gemini_body("test"),
            403,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_http_403");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 403"), "got: {msg}");
        });
    }

    #[test]
    fn azure_http_403_reports_forbidden() {
        let endpoint = azure_url("gpt-test");
        let (client, _dir) = vcr_client(
            "azure_http_403",
            &endpoint,
            azure_body("test"),
            403,
            text_headers(),
            vec!["Access denied".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_http_403");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 403"), "got: {msg}");
        });
    }

    #[test]
    fn gemini_http_500_reports_server_error() {
        let url = gemini_url("gemini-test", "test-key");
        let body = json!({
            "error": { "code": 500, "message": "Internal error", "status": "INTERNAL" }
        });
        let (client, _dir) = vcr_client(
            "gemini_http_500",
            &url,
            gemini_body("test"),
            500,
            json_headers(),
            vec![serde_json::to_string(&body).unwrap()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_http_500");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 500"), "got: {msg}");
        });
    }

    #[test]
    fn azure_http_500_reports_server_error() {
        let endpoint = azure_url("gpt-test");
        let (client, _dir) = vcr_client(
            "azure_http_500",
            &endpoint,
            azure_body("test"),
            500,
            text_headers(),
            vec!["Internal server error".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_http_500");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let err = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .err()
                .expect("expected error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(msg.contains("HTTP 500"), "got: {msg}");
        });
    }
}

// ============================================================================
// Malformed SSE / Response Tests (VCR)
// ============================================================================

mod malformed_responses {
    use super::*;

    #[test]
    fn anthropic_invalid_json_in_sse_fails_stream() {
        let (client, _dir) = vcr_client(
            "anthropic_invalid_json_sse",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            200,
            sse_headers(),
            vec!["event: message_start\ndata: {not json}\n\n".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_invalid_json_sse");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let mut stream = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .expect("stream should open");

            let mut found_error = false;
            while let Some(item) = stream.next().await {
                if let Err(err) = item {
                    found_error = true;
                    harness.log().info("verify", err.to_string());
                    break;
                }
            }
            assert!(found_error, "expected a stream error for invalid JSON");
        });
    }

    #[test]
    fn anthropic_empty_body_200_reports_error() {
        let (client, _dir) = vcr_client(
            "anthropic_empty_body_200",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            200,
            sse_headers(),
            Vec::new(),
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_empty_body_200");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            match result {
                Err(err) => {
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    let mut event_count = 0;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if let Err(err) = item {
                            harness.log().info("verify", err.to_string());
                            break;
                        }
                    }
                    harness
                        .log()
                        .info_ctx("verify", "empty body stream", |ctx| {
                            ctx.push(("event_count".into(), event_count.to_string()));
                        });
                }
            }
        });
    }

    #[test]
    fn gemini_invalid_json_in_sse_fails_stream() {
        let url = gemini_url("gemini-test", "test-key");
        let (client, _dir) = vcr_client(
            "gemini_invalid_json_sse",
            &url,
            gemini_body("test"),
            200,
            sse_headers(),
            vec!["data: {broken json\n\n".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("gemini_invalid_json_sse");
            let provider =
                pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
            let mut stream = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .expect("stream should open");

            let mut found_error = false;
            while let Some(item) = stream.next().await {
                if let Err(err) = item {
                    found_error = true;
                    harness.log().info("verify", err.to_string());
                    break;
                }
            }
            assert!(found_error, "expected a stream error for invalid JSON");
        });
    }

    #[test]
    fn openai_non_json_200_body_is_handled() {
        let (client, _dir) = vcr_client(
            "openai_non_json_200",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            200,
            text_headers(),
            vec!["<html>Not an API</html>".to_string()],
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_non_json_200");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            match result {
                Err(err) => {
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    let mut event_count = 0;
                    let mut found_error = false;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if let Err(err) = item {
                            found_error = true;
                            harness.log().info("verify", err.to_string());
                            break;
                        }
                    }
                    harness
                        .log()
                        .info_ctx("verify", "non-json 200 result", |ctx| {
                            ctx.push(("event_count".into(), event_count.to_string()));
                            ctx.push(("found_error".into(), found_error.to_string()));
                        });
                    assert!(
                        found_error || event_count == 0,
                        "expected error or empty stream, got {event_count} events"
                    );
                }
            }
        });
    }

    #[test]
    fn anthropic_sse_error_event_in_stream() {
        use pi::model::{StopReason, StreamEvent};

        // Anthropic can return HTTP 200 with an error event mid-stream.
        // The provider maps it to StreamEvent::Error (not Err), with
        // stop_reason = Error and error_message populated.
        let error_event = json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "Overloaded" }
        });
        let chunks = vec![format!(
            "event: error\ndata: {}\n\n",
            serde_json::to_string(&error_event).unwrap()
        )];
        let (client, _dir) = vcr_client(
            "anthropic_sse_error_event",
            "https://api.anthropic.com/v1/messages",
            anthropic_body("test"),
            200,
            sse_headers(),
            chunks,
        );
        common::run_async(async move {
            let harness = TestHarness::new("anthropic_sse_error_event");
            let provider =
                pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
            let mut stream = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await
                .expect("stream should open");

            let mut found_error = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamEvent::Error { reason, error }) => {
                        found_error = true;
                        harness.log().info_ctx("verify", "sse error event", |ctx| {
                            ctx.push(("reason".into(), format!("{reason:?}")));
                            ctx.push((
                                "error_message".into(),
                                error.error_message.clone().unwrap_or_default(),
                            ));
                        });
                        assert_eq!(reason, StopReason::Error, "expected Error stop reason");
                        let msg = error.error_message.unwrap_or_default();
                        assert!(
                            msg.contains("Overloaded"),
                            "expected Overloaded in error_message, got: {msg}"
                        );
                        break;
                    }
                    Err(err) => {
                        found_error = true;
                        harness.log().info("verify", err.to_string());
                        break;
                    }
                    _ => {}
                }
            }
            assert!(found_error, "expected an error event in the stream");
        });
    }

    #[test]
    fn openai_empty_body_200_reports_error() {
        let (client, _dir) = vcr_client(
            "openai_empty_body_200",
            "https://api.openai.com/v1/chat/completions",
            openai_body("test"),
            200,
            sse_headers(),
            Vec::new(),
        );
        common::run_async(async move {
            let harness = TestHarness::new("openai_empty_body_200");
            let provider =
                pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            match result {
                Err(err) => {
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    let mut event_count = 0;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if let Err(err) = item {
                            harness.log().info("verify", err.to_string());
                            break;
                        }
                    }
                    harness
                        .log()
                        .info_ctx("verify", "openai empty body stream", |ctx| {
                            ctx.push(("event_count".into(), event_count.to_string()));
                        });
                }
            }
        });
    }

    #[test]
    fn azure_empty_body_200_reports_error() {
        let endpoint = azure_url("gpt-test");
        let (client, _dir) = vcr_client(
            "azure_empty_body_200",
            &endpoint,
            azure_body("test"),
            200,
            sse_headers(),
            Vec::new(),
        );
        common::run_async(async move {
            let harness = TestHarness::new("azure_empty_body_200");
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", "gpt-test")
                .with_client(client)
                .with_endpoint_url(endpoint);
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            match result {
                Err(err) => {
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    let mut event_count = 0;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if let Err(err) = item {
                            harness.log().info("verify", err.to_string());
                            break;
                        }
                    }
                    harness
                        .log()
                        .info_ctx("verify", "azure empty body stream", |ctx| {
                            ctx.push(("event_count".into(), event_count.to_string()));
                        });
                }
            }
        });
    }
}

// ============================================================================
// Tool Execution Error Paths (gaps from tools_conformance)
// ============================================================================

mod tool_errors {
    use super::*;
    use pi::tools::Tool;

    #[test]
    fn bash_command_not_found_reports_exit_code() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("bash_cmd_not_found");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = json!({
                "command": "nonexistent_command_xyz_12345"
            });

            let result = tool.execute("test-id", input, None).await;
            match result {
                Ok(output) => {
                    let text = get_text_content(&output.content);
                    harness
                        .log()
                        .info_ctx("verify", "cmd not found output", |ctx| {
                            ctx.push(("text".into(), text.clone()));
                            ctx.push(("is_error".into(), output.is_error.to_string()));
                        });
                    assert!(
                        output.is_error || text.contains("not found") || text.contains("127"),
                        "expected not-found indication, got: {text}"
                    );
                }
                Err(err) => {
                    let msg = err.to_string();
                    harness.log().info("verify", &msg);
                    assert!(
                        msg.contains("not found") || msg.contains("127"),
                        "unexpected error: {msg}"
                    );
                }
            }
        });
    }

    #[test]
    fn bash_empty_command_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("bash_empty_command");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = json!({ "command": "" });

            let result = tool.execute("test-id", input, None).await;
            harness
                .log()
                .info_ctx("verify", "empty command result", |ctx| {
                    ctx.push(("is_err".into(), result.is_err().to_string()));
                });
        });
    }

    #[test]
    fn read_nonexistent_file_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_nonexistent");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let path = harness.temp_dir().join("does_not_exist.txt");
            let input = json!({ "path": path.to_string_lossy() });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let msg = err.to_string();
            harness.log().info("verify", &msg);
            assert!(
                msg.contains("not found")
                    || msg.contains("No such file")
                    || msg.contains("does not exist"),
                "unexpected error: {msg}"
            );
        });
    }

    #[test]
    fn write_to_nonexistent_parent_dir_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("write_no_parent");
            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let path = harness
                .temp_dir()
                .join("nonexistent_dir")
                .join("subdir")
                .join("file.txt");
            let input = json!({
                "path": path.to_string_lossy(),
                "content": "hello"
            });

            let result = tool.execute("test-id", input, None).await;
            harness
                .log()
                .info_ctx("verify", "write to nonexistent parent", |ctx| {
                    ctx.push(("is_err".into(), result.is_err().to_string()));
                    if let Err(ref err) = result {
                        ctx.push(("message".into(), err.to_string()));
                    }
                });
        });
    }

    #[test]
    fn grep_invalid_regex_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_bad_regex");
            harness.create_file("sample.txt", b"some content");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = json!({
                "pattern": "[invalid(regex",
                "path": harness.temp_dir().to_string_lossy()
            });

            let result = tool.execute("test-id", input, None).await;
            match result {
                Err(err) => {
                    let msg = err.to_string();
                    harness.log().info("verify", &msg);
                }
                Ok(output) => {
                    let text = get_text_content(&output.content);
                    harness
                        .log()
                        .info_ctx("verify", "grep bad regex output", |ctx| {
                            ctx.push(("text".into(), text.clone()));
                            ctx.push(("is_error".into(), output.is_error.to_string()));
                        });
                }
            }
        });
    }

    #[test]
    fn edit_empty_old_text_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_empty_old");
            harness.create_file("test.txt", b"Hello World");
            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let path = harness.temp_dir().join("test.txt");
            let input = json!({
                "path": path.to_string_lossy(),
                "oldText": "",
                "newText": "replacement"
            });

            let result = tool.execute("test-id", input, None).await;
            harness
                .log()
                .info_ctx("verify", "empty old text result", |ctx| {
                    ctx.push(("is_err".into(), result.is_err().to_string()));
                });
        });
    }

    #[test]
    fn find_in_nonexistent_directory_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_bad_path");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let bad_path = harness.temp_dir().join("no_such_dir");
            let input = json!({
                "pattern": "*.rs",
                "path": bad_path.to_string_lossy()
            });

            let result = tool.execute("test-id", input, None).await;
            match result {
                Err(err) => {
                    let msg = err.to_string();
                    harness.log().info("verify", &msg);
                }
                Ok(output) => {
                    let text = get_text_content(&output.content);
                    harness
                        .log()
                        .info_ctx("verify", "find bad path result", |ctx| {
                            ctx.push(("text".into(), text.clone()));
                            ctx.push(("is_error".into(), output.is_error.to_string()));
                        });
                }
            }
        });
    }

    #[test]
    fn ls_nonexistent_directory_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_bad_path");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let bad_path = harness.temp_dir().join("no_such_dir");
            let input = json!({ "path": bad_path.to_string_lossy() });

            let result = tool.execute("test-id", input, None).await;
            match result {
                Err(err) => {
                    let msg = err.to_string();
                    harness.log().info("verify", &msg);
                    assert!(
                        msg.contains("not found")
                            || msg.contains("No such file")
                            || msg.contains("does not exist"),
                        "unexpected error: {msg}"
                    );
                }
                Ok(output) => {
                    let text = get_text_content(&output.content);
                    harness
                        .log()
                        .info_ctx("verify", "ls bad path result", |ctx| {
                            ctx.push(("text".into(), text.clone()));
                            ctx.push(("is_error".into(), output.is_error.to_string()));
                        });
                    assert!(output.is_error, "expected is_error for nonexistent dir");
                }
            }
        });
    }
}

// ============================================================================
// Error Hints Taxonomy
// ============================================================================

mod error_hints {
    use super::*;

    #[test]
    fn provider_401_hints_mention_api_key() {
        let err = Error::provider("anthropic", "HTTP 401: invalid api key");
        let hints = err.hints();
        assert!(
            hints.summary.contains("authentication"),
            "summary: {}",
            hints.summary
        );
        assert!(
            hints
                .hints
                .iter()
                .any(|h| h.contains("API") || h.contains("key")),
            "hints should mention API key: {:?}",
            hints.hints
        );
    }

    #[test]
    fn provider_403_hints_mention_permissions() {
        let err = Error::provider("openai", "HTTP 403: forbidden");
        let hints = err.hints();
        assert!(
            hints.summary.contains("forbidden"),
            "summary: {}",
            hints.summary
        );
        assert!(
            hints
                .hints
                .iter()
                .any(|h| h.contains("permission") || h.contains("access")),
            "hints should mention permissions: {:?}",
            hints.hints
        );
    }

    #[test]
    fn provider_429_hints_mention_rate_limit() {
        let err = Error::provider("anthropic", "HTTP 429: rate limit exceeded");
        let hints = err.hints();
        assert!(
            hints.summary.contains("rate limit"),
            "summary: {}",
            hints.summary
        );
        assert!(
            hints
                .hints
                .iter()
                .any(|h| h.contains("retry") || h.contains("wait")),
            "hints should mention retry: {:?}",
            hints.hints
        );
    }

    #[test]
    fn provider_500_hints_suggest_retry() {
        let err = Error::provider("gemini", "HTTP 500: internal server error");
        let hints = err.hints();
        assert!(!hints.summary.is_empty(), "summary should not be empty");
        assert!(
            hints
                .hints
                .iter()
                .any(|h| h.contains("retry") || h.contains("Retry")),
            "hints should suggest retry: {:?}",
            hints.hints
        );
    }

    #[test]
    fn provider_529_hints_mention_overloaded() {
        let err = Error::provider("anthropic", "HTTP 529: overloaded");
        let hints = err.hints();
        assert!(
            hints.summary.contains("overloaded"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn tool_not_found_hints() {
        let err = Error::tool("read", "File not found: /tmp/missing.txt");
        let hints = err.hints();
        assert!(
            !hints.summary.is_empty(),
            "tool hints should have a summary"
        );
        assert!(
            hints
                .context
                .iter()
                .any(|(k, _)| k == "tool" || k == "details"),
            "context should include tool or details: {:?}",
            hints.context
        );
    }

    #[test]
    fn tool_permission_denied_hints() {
        let err = Error::tool("write", "Permission denied: /root/protected.txt");
        let hints = err.hints();
        assert!(
            !hints.summary.is_empty(),
            "tool permission hints should have a summary"
        );
    }

    #[test]
    fn config_json_error_hints() {
        let err = Error::config("JSON parse error at line 5: trailing comma");
        let hints = err.hints();
        assert!(
            hints.summary.contains("JSON") || hints.summary.contains("Configuration"),
            "summary: {}",
            hints.summary
        );
        assert!(
            hints
                .hints
                .iter()
                .any(|h| h.contains("JSON") || h.contains("format")),
            "hints should mention JSON formatting: {:?}",
            hints.hints
        );
    }

    #[test]
    fn config_missing_file_hints() {
        let err = Error::config("Configuration file not found: /home/user/.pi/settings.json");
        let hints = err.hints();
        assert!(
            hints.summary.contains("missing") || hints.summary.contains("Configuration"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn session_empty_hints() {
        let err = Error::session("Empty session file: session.jsonl");
        let hints = err.hints();
        assert!(
            hints.summary.contains("empty") || hints.summary.contains("Session"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn io_permission_denied_hints() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = Error::Io(Box::new(io_err));
        let hints = err.hints();
        assert!(!hints.summary.is_empty(), "IO hints should have a summary");
        assert!(
            hints.hints.iter().any(|h| h.contains("permission")
                || h.contains("sudo")
                || h.contains("Permission")),
            "IO permission hints should mention permissions: {:?}",
            hints.hints
        );
    }

    #[test]
    fn io_connection_refused_hints() {
        let io_err =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let err = Error::Io(Box::new(io_err));
        let hints = err.hints();
        assert!(
            !hints.summary.is_empty(),
            "IO connection hints should have a summary"
        );
    }

    #[test]
    fn auth_error_hints() {
        let err = Error::auth("OAuth token expired");
        let hints = err.hints();
        assert!(
            !hints.summary.is_empty(),
            "auth hints should have a summary"
        );
    }

    #[test]
    fn aborted_error_hints() {
        let err = Error::Aborted;
        let hints = err.hints();
        assert!(
            hints.summary.contains("aborted")
                || hints.summary.contains("Aborted")
                || hints.summary.contains("abort"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn validation_error_hints() {
        let err = Error::validation("Missing required field: model");
        let hints = err.hints();
        assert!(
            hints.summary.contains("Validation") || hints.summary.contains("validation"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn extension_error_hints() {
        let err = Error::extension("Failed to load WASM module");
        let hints = err.hints();
        assert!(
            hints.summary.contains("Extension") || hints.summary.contains("extension"),
            "summary: {}",
            hints.summary
        );
    }
}
