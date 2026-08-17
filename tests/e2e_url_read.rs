//! E2E (bd-cv653.2.2): URL reads through the real binary — reader-mode
//! conversion, SSRF denial, and pagination. Loopback mock server only;
//! JSONL logs per tests/common/logging.rs.

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

fn html_response(html: &str) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/html".to_string())],
        body: html.as_bytes().to_vec(),
    }
}

/// Chat-completions SSE carrying one `read` tool call with `path` = url.
fn read_call_sse_body(url: &str) -> String {
    let args = format!(r#"{{\"path\":\"{url}\"}}"#);
    [
        format!(
            r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"r1","type":"function","function":{{"name":"read","arguments":"{args}"}}}}]}}}}]}}"#
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

struct PiEnv {
    root: std::path::PathBuf,
}

impl PiEnv {
    fn new(harness: &TestHarness, settings: &str) -> Self {
        let root = harness.temp_path("pi-env-url");
        std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
        std::fs::create_dir_all(root.join("home")).expect("mkdir home");
        std::fs::write(root.join("settings.json"), settings).expect("write settings");
        Self { root }
    }

    fn write_models(&self, base_url: &str) {
        std::fs::write(
            self.root.join("agent/models.json"),
            format!(
                r#"{{"providers": {{"e2eurl": {{"api": "openai-completions", "baseUrl": "{base_url}/v1", "apiKey": "test-key", "models": [{{"id": "test-model", "contextWindow": 128000}}]}}}}}}"#
            ),
        )
        .expect("write models.json");
    }

    fn command(&self, binary: &std::path::Path) -> Command {
        let mut command = Command::new(binary);
        command
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
        ] {
            command.env_remove(key);
        }
        command
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

const FIXTURE_PAGE: &str = r#"<!DOCTYPE html><html><head><title>Guide</title><style>nav{}</style></head>
<body><nav>Home | Docs | About</nav><main><h1>Install Guide</h1><h2 id="step-1">Step 1</h2>
<p>Download the binary first.</p><h2 id="step-2">Step 2</h2><p>Run the installer.</p></main>
<footer>© 2026</footer><script>analytics()</script></body></html>"#;

#[test]
fn e2e_read_url_reader_mode_and_pagination() {
    let harness = TestHarness::new("e2e_read_url_reader_mode_and_pagination");
    harness
        .log()
        .info("setup", "mock page + scripted read calls");
    let server = harness.start_mock_http_server();
    server.add_route("GET", "/guide", html_response(FIXTURE_PAGE));
    // The route queue must carry the concrete URL (base_url known only after
    // server start), so register it after the server is up.
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response(read_call_sse_body(&format!("{}/guide", server.base_url()))),
            sse_response(text_sse_body("page read complete")),
        ],
    );

    let env = PiEnv::new(
        &harness,
        r#"{"read": {"urlAllowPrivateTargets": true}, "checkForUpdates": false}"#,
    );
    env.write_models(&server.base_url());

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eurl",
        "--model",
        "test-model",
        "read the install guide",
    ]);
    harness.log().info("action", "spawning pi --print");
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_to_finish(child, 90);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("stdout".to_string(), stdout.clone()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });

    assert!(
        stdout.contains("page read complete"),
        "turn completes: stdout {stdout}\nstderr: {stderr}"
    );
    // The read tool's converted output reached the model: check the SECOND
    // request body for converted markers (heading, no boilerplate).
    let requests = server.requests();
    let second = requests
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .nth(1)
        .expect("second request (post-tool)");
    let body = String::from_utf8_lossy(&second.body);
    assert!(
        body.contains("Install Guide"),
        "converted heading reaches the model"
    );
    assert!(
        body.contains("Download the binary first."),
        "article text reaches the model"
    );
    assert!(
        !body.contains("analytics()"),
        "script boilerplate stripped before the model sees it"
    );

    let path = harness.temp_path("e2e_url_read.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_url_read.jsonl", &path);
}

#[test]
fn e2e_read_url_ssrf_denied_by_default() {
    let harness = TestHarness::new("e2e_read_url_ssrf_denied_by_default");
    harness
        .log()
        .info("setup", "no override → loopback read must be denied");
    let server = harness.start_mock_http_server();
    server.add_route("GET", "/secret", html_response("<p>secret</p>"));

    let env = PiEnv::new(&harness, r#"{"checkForUpdates": false}"#);
    env.write_models(&server.base_url());
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response(read_call_sse_body(&format!("{}/secret", server.base_url()))),
            sse_response(text_sse_body("blocked as expected")),
        ],
    );

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eurl",
        "--model",
        "test-model",
        "read the secret page",
    ]);
    let child = command.spawn().expect("spawn pi");
    let (_stdout, _stderr) = run_to_finish(child, 90);

    // The mock must NEVER see the /secret request — the SSRF guard fired
    // before any network activity.
    let secret_hits = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/secret")
        .count();
    assert_eq!(secret_hits, 0, "SSRF guard must block before any fetch");
    // And the block evidence must reach the model as a tool error in a
    // subsequent request body.
    let saw_block = server.requests().into_iter().any(|r| {
        r.path == "/v1/chat/completions"
            && String::from_utf8_lossy(&r.body).contains("SSRF_BLOCKED")
    });
    assert!(
        saw_block,
        "block evidence must reach the model as a tool error"
    );

    let path = harness.temp_path("e2e_url_ssrf.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_url_ssrf.jsonl", &path);
}
