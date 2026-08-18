//! E2E (bd-cv653.2.1): `web_search` provider chain over real processes with
//! loopback mocks (per-provider base-url overrides). JSONL logs per
//! `tests/common/logging.rs`.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn json_response(status: u16, body: &str) -> MockHttpResponse {
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.as_bytes().to_vec(),
    }
}

fn web_search_call_sse_body(query: &str) -> String {
    let args = format!(r#"{{\"query\":\"{query}\"}}"#);
    [
        format!(
            r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"s1","type":"function","function":{{"name":"web_search","arguments":"{args}"}}}}]}}}}]}}"#
        )
        .as_str(),
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn text_sse_body(text: &str) -> String {
    [
        format!(r#"data: {{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#).as_str(),
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn brave_results_body() -> String {
    r#"{"web":{"results":[
        {"title":"Tokio tutorial","url":"https://tokio.rs/tutorial","description":"Async Rust runtime guide"},
        {"title":"Rust async book","url":"https://rust-lang.github.io/async-book/","description":"Official async book"}
    ]}}"#
    .to_string()
}

fn ddg_html_page() -> String {
    r#"<html><body>
    <div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdoc">Example Doc</a></div>
    <div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fother.io%2Fguide">Other Guide</a></div>
    </body></html>"#
        .to_string()
}

struct PiEnv {
    root: std::path::PathBuf,
}

impl PiEnv {
    fn new(harness: &TestHarness, case: &str) -> Self {
        let root = harness.temp_path(format!("pi-env-{case}"));
        std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
        std::fs::create_dir_all(root.join("home")).expect("mkdir home");
        std::fs::write(root.join("settings.json"), r#"{"checkForUpdates": false}"#)
            .expect("write settings");
        Self { root }
    }

    fn write_models(&self, base_url: &str) {
        std::fs::write(
            self.root.join("agent/models.json"),
            format!(
                r#"{{"providers": {{"e2esearch": {{"api": "openai-completions", "baseUrl": "{base_url}/v1", "apiKey": "test-key", "models": [{{"id": "test-model", "contextWindow": 128000}}]}}}}}}"#
            ),
        )
        .expect("write models.json");
    }

    fn spawn(&self, binary: &std::path::Path, extra_env: &[(&str, String)]) -> std::process::Child {
        let mut command = Command::new(binary);
        command
            .args([
                "--print",
                "--no-session",
                "--provider",
                "e2esearch",
                "--model",
                "test-model",
                "--tools",
                "web_search",
                "search for async rust",
            ])
            .env("HOME", self.root.join("home"))
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .env("PI_CONFIG_PATH", self.root.join("settings.json"))
            .env("PI_SESSIONS_DIR", self.root.join("sessions"))
            .env("PI_PACKAGE_DIR", self.root.join("packages"))
            .env("PI_NO_AUTO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "DEEPSEEK_API_KEY",
            "PERPLEXITY_API_KEY",
            "TAVILY_API_KEY",
            "BRAVE_API_KEY",
            "BRAVE_SEARCH_API_KEY",
            "EXA_API_KEY",
            "JINA_API_KEY",
            "KAGI_API_KEY",
        ] {
            command.env_remove(key);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.spawn().expect("spawn pi")
    }
}

fn run_to_finish(mut child: std::process::Child, secs: u64) -> (String, String) {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let out = child.wait_with_output().expect("output");
                return (
                    String::from_utf8_lossy(&out.stdout).to_string(),
                    String::from_utf8_lossy(&out.stderr).to_string(),
                );
            }
            Ok(None) if start.elapsed() > Duration::from_secs(secs) => {
                let _ = child.kill();
                let out = child.wait_with_output().expect("output");
                return (
                    String::from_utf8_lossy(&out.stdout).to_string(),
                    format!("TIMEOUT\n{}", String::from_utf8_lossy(&out.stderr)),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => panic!("wait failed: {err}"),
        }
    }
}

fn finish_case(harness: &TestHarness, case: &str) {
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn e2e_chain_falls_through_failing_provider_to_next() {
    let harness = TestHarness::new("e2e_chain_falls_through_failing_provider_to_next");
    harness.log().info("setup", "tavily 500s, brave answers");
    let server = harness.start_mock_http_server();
    server.add_route("POST", "/search", json_response(500, r#"{"error":"boom"}"#));
    server.add_route(
        "GET",
        "/res/v1/web/search",
        json_response(200, &brave_results_body()),
    );

    let env = PiEnv::new(&harness, "fallthrough");
    env.write_models(&server.base_url());
    // Script the model call now that the URL is concrete.
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response(web_search_call_sse_body("rust async runtime")),
            sse_response(text_sse_body("search complete")),
        ],
    );

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let child = env.spawn(
        &binary,
        &[
            ("TAVILY_API_KEY", "tavily-test".to_string()),
            ("BRAVE_API_KEY", "brave-test".to_string()),
            ("PI_WEBSEARCH_BASE_TAVILY", server.base_url()),
            ("PI_WEBSEARCH_BASE_BRAVE", server.base_url()),
        ],
    );
    let (stdout, stderr) = run_to_finish(child, 90);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("stdout".to_string(), stdout.clone()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(300).collect(),
        ));
    });

    assert!(
        stdout.contains("search complete"),
        "turn completes: {stdout}\n{stderr}"
    );
    let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    assert!(
        paths.iter().any(|p| p == "/search"),
        "tavily rung hit first: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p == "/res/v1/web/search"),
        "brave rung answered after tavily failed: {paths:?}"
    );
    let model_bound = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/v1/chat/completions")
        .expect("final model call");
    let body = String::from_utf8_lossy(&model_bound.body);
    assert!(
        body.contains("tokio.rs/tutorial"),
        "results reach the model"
    );
    assert!(
        body.contains("brave"),
        "provider attribution reaches the model"
    );
    finish_case(&harness, "e2e_chain_falls_through");
}

#[test]
fn e2e_keyless_path_with_no_keys() {
    let harness = TestHarness::new("e2e_keyless_path_with_no_keys");
    harness
        .log()
        .info("setup", "no keys anywhere; duckduckgo answers");
    let server = harness.start_mock_http_server();
    server.add_route("GET", "/html/", {
        MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            body: ddg_html_page().into_bytes(),
        }
    });
    let env = PiEnv::new(&harness, "keyless");
    env.write_models(&server.base_url());
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response(web_search_call_sse_body("rust async runtime")),
            sse_response(text_sse_body("keyless search complete")),
        ],
    );

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let child = env.spawn(
        &binary,
        &[("PI_WEBSEARCH_BASE_DUCKDUCKGO", server.base_url())],
    );
    let (stdout, stderr) = run_to_finish(child, 90);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("stdout".to_string(), stdout.clone()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(300).collect(),
        ));
    });

    assert!(
        stdout.contains("keyless search complete"),
        "turn completes keyless: {stdout}\n{stderr}"
    );
    let model_bound = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/v1/chat/completions")
        .expect("final model call");
    let body = String::from_utf8_lossy(&model_bound.body);
    assert!(
        body.contains("example.com/doc"),
        "decoded duckduckgo redirect URL reaches the model: {}",
        body.chars().take(400).collect::<String>()
    );
    assert!(body.contains("duckduckgo"), "source attribution present");
    finish_case(&harness, "e2e_keyless_path");
}

#[test]
fn e2e_provider_pin_uses_only_that_rung() {
    let harness = TestHarness::new("e2e_provider_pin_uses_only_that_rung");
    harness.log().info("setup", "provider pin to brave only");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/search",
        json_response(500, r#"{"error":"must not be called"}"#),
    );
    server.add_route(
        "GET",
        "/res/v1/web/search",
        json_response(200, &brave_results_body()),
    );
    let env = PiEnv::new(&harness, "pin");
    env.write_models(&server.base_url());
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response({
                let args = r#"{"query":"rust async\",\"provider\":\"brave\"}"#;
                [
                    format!(
                        r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"s1","type":"function","function":{{"name":"web_search","arguments":"{args}"}}}}]}}}}]}}"#
                    )
                    .as_str(),
                    "",
                    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    "",
                    "data: [DONE]",
                    "",
                ]
                .join("\n")
            }),
            sse_response(text_sse_body("pinned search complete")),
        ],
    );

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let child = env.spawn(
        &binary,
        &[
            ("TAVILY_API_KEY", "tavily-test".to_string()),
            ("BRAVE_API_KEY", "brave-test".to_string()),
            ("PI_WEBSEARCH_BASE_TAVILY", server.base_url()),
            ("PI_WEBSEARCH_BASE_BRAVE", server.base_url()),
        ],
    );
    let (stdout, _stderr) = run_to_finish(child, 90);
    assert!(stdout.contains("pinned search complete"));
    let tavily_hits = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/search")
        .count();
    assert_eq!(tavily_hits, 0, "provider pin must not touch other rungs");
    finish_case(&harness, "e2e_provider_pin");
}
