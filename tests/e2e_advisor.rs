//! E2E (bd-cv653.3.3): the advisor over RPC — a scripted risky turn yields a
//! CONCERN injected into the next turn's context; a failing advisor never
//! breaks the main turn. Two mock providers (doer + advisor) on one server,
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

fn tool_call_sse_body(call_id: &str, tool: &str, arguments_json: &str) -> String {
    let escaped = arguments_json.replace('\\', "\\\\").replace('"', "\\\"");
    [
        format!(
            r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","type":"function","function":{{"name":"{tool}","arguments":"{escaped}"}}}}]}}}}]}}"#
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

fn build_binary(harness: &TestHarness) -> std::path::PathBuf {
    let _ = harness;
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

struct PiEnv {
    root: std::path::PathBuf,
}

impl PiEnv {
    fn new(
        harness: &TestHarness,
        advisor_base: &str,
        doer_base: &str,
        extra_settings: &str,
    ) -> Self {
        let root = harness.temp_path("pi-env-advisor");
        std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
        std::fs::create_dir_all(root.join("home")).expect("mkdir home");
        std::fs::write(
            root.join("agent/models.json"),
            format!(
                r#"{{"providers": {{
                    "e2edoer": {{"api": "openai-completions", "baseUrl": "{doer_base}/v1", "apiKey": "test-key", "models": [{{"id": "doer-model", "contextWindow": 128000}}]}},
                    "e2eadvisor": {{"api": "openai-completions", "baseUrl": "{advisor_base}/v1", "apiKey": "test-key", "models": [{{"id": "advisor-model", "contextWindow": 128000}}]}}
                }}}}"#
            ),
        )
        .expect("write models.json");
        std::fs::write(
            root.join("settings.json"),
            format!(
                r#"{{"modelRoles": {{"advisor": "e2eadvisor/advisor-model"}}, "advisor": {{"timeoutSecs": 2}}, "checkForUpdates": false{extra_settings}}}"#
            ),
        )
        .expect("write settings.json");
        Self { root }
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
            .stdin(Stdio::piped())
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

#[test]
fn e2e_advisor_concern_injected_into_next_turn() {
    let harness = TestHarness::new("e2e_advisor_concern_injected_into_next_turn");
    harness
        .log()
        .info("setup", "doer risky turn; advisor returns CONCERN");
    let server = harness.start_mock_http_server();
    // Doer: one read call, then text.
    server.add_route_queue(
        "POST",
        "/doer/v1/chat/completions",
        vec![
            sse_response(tool_call_sse_body("r1", "read", r#"{"path":"."}"#)),
            sse_response(text_sse_body("broad sweep done")),
            sse_response(text_sse_body("continued after the note")),
        ],
    );
    // Advisor: always returns the CONCERN verdict.
    server.add_route(
        "POST",
        "/advisor/v1/chat/completions",
        sse_response(text_sse_body(
            "CONCERN: swept the whole tree\nScope the read to the target directory.",
        )),
    );

    let env = PiEnv::new(
        &harness,
        &format!("{}/advisor", server.base_url()),
        &format!("{}/doer", server.base_url()),
        "",
    );
    let binary = build_binary(&harness);
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2edoer",
        "--model",
        "doer-model",
        "--tools",
        "read,ls",
        "sweep it",
    ]);
    let mut child = command.spawn().expect("spawn pi");
    let start = Instant::now();
    let (_stdout, _stderr) = loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let out = child.wait_with_output().expect("output");
                break (
                    String::from_utf8_lossy(&out.stdout).to_string(),
                    String::from_utf8_lossy(&out.stderr).to_string(),
                );
            }
            Ok(None) if start.elapsed() > Duration::from_secs(90) => panic!("timed out"),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => panic!("wait failed: {err}"),
        }
    };

    // The advisor was consulted…
    let advisor_hits = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/advisor/v1/chat/completions")
        .count();
    assert!(advisor_hits >= 1, "advisor provider must be consulted");

    // …and the CONCERN injection reached the doer's next-turn context
    // (visible in the final request body).
    let final_request = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/doer/v1/chat/completions")
        .expect("final doer request");
    let body = String::from_utf8_lossy(&final_request.body);
    assert!(
        body.contains("ADVISOR:CONCERN"),
        "the advisor's concern must reach the doer's context"
    );
    assert!(
        body.contains("Scope the read"),
        "the rationale rides along: {}",
        body.chars().take(600).collect::<String>()
    );

    let path = harness.temp_path("e2e_advisor_concern.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_advisor_concern.jsonl", &path);
    harness
        .log()
        .info("done", "advisor injection verified over the wire");
}

#[test]
fn e2e_advisor_failure_isolated_from_main_turn() {
    let harness = TestHarness::new("e2e_advisor_failure_isolated_from_main_turn");
    harness
        .log()
        .info("setup", "advisor 500s; doer must complete anyway");
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/doer/v1/chat/completions",
        vec![
            sse_response(tool_call_sse_body("r1", "read", r#"{"path":"."}"#)),
            sse_response(text_sse_body("done regardless")),
        ],
    );
    server.add_route(
        "POST",
        "/advisor/v1/chat/completions",
        MockHttpResponse {
            status: 500,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"error":"advisor exploded"}"#.to_vec(),
        },
    );

    let env = PiEnv::new(
        &harness,
        &format!("{}/advisor", server.base_url()),
        &format!("{}/doer", server.base_url()),
        "",
    );
    let binary = build_binary(&harness);
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2edoer",
        "--model",
        "doer-model",
        "--tools",
        "read",
        "work",
    ]);
    let mut child = command.spawn().expect("spawn pi");
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(90) => panic!("timed out"),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => panic!("wait failed: {err}"),
        }
    }
    let output = child.wait_with_output().expect("output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("done regardless"),
        "main turn completes despite advisor failure: {}",
        stdout.chars().take(400).collect::<String>()
    );
    let path = harness.temp_path("e2e_advisor_failure.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_advisor_failure.jsonl", &path);
}
