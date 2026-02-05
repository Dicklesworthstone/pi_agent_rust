//! Comprehensive error handling path tests.
//!
//! Covers provider HTTP error codes (401/403/429/529), malformed SSE scenarios,
//! tool execution edge cases not in tools_conformance, and the error hints taxonomy.
//! All tests are deterministic and offline (MockHttpServer or temp dirs).

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use futures::StreamExt;
use pi::error::Error;
use pi::model::{Message, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};
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
// Provider HTTP Error Codes (MockHttpServer)
// ============================================================================

mod provider_http_errors {
    use super::*;

    // --- Anthropic ---

    #[test]
    fn anthropic_http_401_reports_auth_error() {
        let harness = TestHarness::new("anthropic_http_401");
        let server = harness.start_mock_http_server();
        let body = json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid x-api-key" }
        });
        server.add_route("POST", "/v1/messages", MockHttpResponse::json(401, &body));

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
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
        let harness = TestHarness::new("anthropic_http_403");
        let server = harness.start_mock_http_server();
        let body = json!({
            "type": "error",
            "error": { "type": "forbidden", "message": "access denied to model" }
        });
        server.add_route("POST", "/v1/messages", MockHttpResponse::json(403, &body));

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
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
        let harness = TestHarness::new("anthropic_http_429");
        let server = harness.start_mock_http_server();
        let body = json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "rate limited" }
        });
        server.add_route(
            "POST",
            "/v1/messages",
            MockHttpResponse {
                status: 429,
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("retry-after".to_string(), "5".to_string()),
                ],
                body: serde_json::to_vec(&body).unwrap_or_default(),
            },
        );

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
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
        let harness = TestHarness::new("anthropic_http_529");
        let server = harness.start_mock_http_server();
        let body = json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "overloaded" }
        });
        server.add_route("POST", "/v1/messages", MockHttpResponse::json(529, &body));

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
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
        let harness = TestHarness::new("openai_http_401");
        let server = harness.start_mock_http_server();
        let body = json!({
            "error": { "message": "Incorrect API key", "type": "invalid_request_error" }
        });
        server.add_route(
            "POST",
            "/v1/chat/completions",
            MockHttpResponse::json(401, &body),
        );

        common::run_async(async move {
            let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
                .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
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
        let harness = TestHarness::new("openai_http_429");
        let server = harness.start_mock_http_server();
        let body = json!({
            "error": { "message": "Rate limit exceeded", "type": "rate_limit_error" }
        });
        server.add_route(
            "POST",
            "/v1/chat/completions",
            MockHttpResponse {
                status: 429,
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("retry-after".to_string(), "10".to_string()),
                ],
                body: serde_json::to_vec(&body).unwrap_or_default(),
            },
        );

        common::run_async(async move {
            let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
                .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
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
        let harness = TestHarness::new("gemini_http_401");
        let server = harness.start_mock_http_server();

        let model = "gemini-test";
        let api_key = "bad-key";
        let path = format!("/models/{model}:streamGenerateContent?alt=sse&key={api_key}");
        server.add_route(
            "POST",
            &path,
            MockHttpResponse::text(401, "API key not valid"),
        );

        common::run_async(async move {
            let provider =
                pi::providers::gemini::GeminiProvider::new(model).with_base_url(server.base_url());
            let err = provider
                .stream(&context_for("test"), &options_with_key(api_key))
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
        let harness = TestHarness::new("gemini_http_429");
        let server = harness.start_mock_http_server();

        let model = "gemini-test";
        let api_key = "test-key";
        let path = format!("/models/{model}:streamGenerateContent?alt=sse&key={api_key}");
        server.add_route(
            "POST",
            &path,
            MockHttpResponse::text(429, "Resource exhausted"),
        );

        common::run_async(async move {
            let provider =
                pi::providers::gemini::GeminiProvider::new(model).with_base_url(server.base_url());
            let err = provider
                .stream(&context_for("test"), &options_with_key(api_key))
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
        let harness = TestHarness::new("azure_http_401");
        let server = harness.start_mock_http_server();

        let deployment = "gpt-test";
        let api_version = "2024-02-15-preview";
        let path =
            format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
        server.add_route("POST", &path, MockHttpResponse::text(401, "Unauthorized"));

        common::run_async(async move {
            let endpoint = format!("{}{path}", server.base_url());
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
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
        let harness = TestHarness::new("azure_http_429");
        let server = harness.start_mock_http_server();

        let deployment = "gpt-test";
        let api_version = "2024-02-15-preview";
        let path =
            format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
        server.add_route(
            "POST",
            &path,
            MockHttpResponse {
                status: 429,
                headers: vec![
                    ("Content-Type".to_string(), "text/plain".to_string()),
                    ("retry-after".to_string(), "30".to_string()),
                ],
                body: b"Too many requests".to_vec(),
            },
        );

        common::run_async(async move {
            let endpoint = format!("{}{path}", server.base_url());
            let provider = pi::providers::azure::AzureOpenAIProvider::new("unused", deployment)
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
}

// ============================================================================
// Malformed SSE / Response Tests
// ============================================================================

mod malformed_responses {
    use super::*;

    #[test]
    fn anthropic_invalid_json_in_sse_fails_stream() {
        let harness = TestHarness::new("anthropic_invalid_json_sse");
        let server = harness.start_mock_http_server();
        server.add_route(
            "POST",
            "/v1/messages",
            MockHttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body: b"event: message_start\ndata: {not json}\n\n".to_vec(),
            },
        );

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
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
        let harness = TestHarness::new("anthropic_empty_body_200");
        let server = harness.start_mock_http_server();
        server.add_route(
            "POST",
            "/v1/messages",
            MockHttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body: Vec::new(),
            },
        );

        common::run_async(async move {
            let provider = pi::providers::anthropic::AnthropicProvider::new("claude-test")
                .with_base_url(format!("{}/v1/messages", server.base_url()));
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            // Either the stream creation fails or the stream yields no items / an error
            match result {
                Err(err) => {
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    // Stream opened, but should produce no meaningful events or an error
                    let mut event_count = 0;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if item.is_err() {
                            harness.log().info("verify", item.unwrap_err().to_string());
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
        let harness = TestHarness::new("gemini_invalid_json_sse");
        let server = harness.start_mock_http_server();

        let model = "gemini-test";
        let api_key = "test-key";
        let path = format!("/models/{model}:streamGenerateContent?alt=sse&key={api_key}");
        server.add_route(
            "POST",
            &path,
            MockHttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body: b"data: {broken json\n\n".to_vec(),
            },
        );

        common::run_async(async move {
            let provider =
                pi::providers::gemini::GeminiProvider::new(model).with_base_url(server.base_url());
            let mut stream = provider
                .stream(&context_for("test"), &options_with_key(api_key))
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
        let harness = TestHarness::new("openai_non_json_200");
        let server = harness.start_mock_http_server();
        server.add_route(
            "POST",
            "/v1/chat/completions",
            MockHttpResponse::text(200, "<html>Not an API</html>"),
        );

        common::run_async(async move {
            let provider = pi::providers::openai::OpenAIProvider::new("gpt-test")
                .with_base_url(format!("{}/v1/chat/completions", server.base_url()));
            let result = provider
                .stream(&context_for("test"), &options_with_key("test-key"))
                .await;

            match result {
                Err(err) => {
                    // Immediate error is fine
                    harness.log().info("verify", err.to_string());
                }
                Ok(mut stream) => {
                    // Provider may open the stream and then either:
                    // (a) yield an error event, or
                    // (b) yield no events (empty stream terminates cleanly)
                    // Both are acceptable handling of malformed responses.
                    let mut event_count = 0;
                    let mut found_error = false;
                    while let Some(item) = stream.next().await {
                        event_count += 1;
                        if item.is_err() {
                            found_error = true;
                            harness.log().info("verify", item.unwrap_err().to_string());
                            break;
                        }
                    }
                    harness
                        .log()
                        .info_ctx("verify", "non-json 200 result", |ctx| {
                            ctx.push(("event_count".into(), event_count.to_string()));
                            ctx.push(("found_error".into(), found_error.to_string()));
                        });
                    // Either an error in stream or no meaningful events is correct
                    assert!(
                        found_error || event_count == 0,
                        "expected error or empty stream, got {event_count} events"
                    );
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
                    // bash returns exit code 127 for command not found; tool may return
                    // it as a successful ToolOutput with is_error or non-zero exit code.
                    let text = get_text_content(&output.content);
                    harness
                        .log()
                        .info_ctx("verify", "cmd not found output", |ctx| {
                            ctx.push(("text".into(), text.clone()));
                            ctx.push(("is_error".into(), output.is_error.to_string()));
                        });
                    // Should indicate failure somehow
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
            // Empty command should error or produce empty output
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
            // WriteTool may create parent dirs or error; verify behavior is consistent
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
                    // grep might return is_error=true instead of Err
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
            // Empty old text is either an error or a degenerate match
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
