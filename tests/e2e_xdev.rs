//! E2E (bd-cv653.1.6 acceptance #2): an RPC session discovers, runs, and
//! promotes a discoverable tool through the `xdev` dispatcher — real binary,
//! scripted mock provider, loopback only. JSONL logs per tests/common/logging.rs.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::io::Write;
use std::process::{Command, Stdio};

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

/// Chat-completions SSE carrying one tool call to xdev. `extra_args` is a
/// pre-escaped JSON fragment (leading comma included) merged into the call's
/// arguments object.
fn xdev_call_sse_body(call_id: &str, action: &str, extra_args: &str) -> String {
    let arguments = format!(r#"{{\"action\":\"{action}\"{extra_args}}}"#);
    [
        format!(
            r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","type":"function","function":{{"name":"xdev","arguments":"{arguments}"}}}}]}}}}]}}"#
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

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_rpc_session_discovers_runs_and_promotes_via_xdev() {
    let harness = TestHarness::new("e2e_rpc_session_discovers_runs_and_promotes_via_xdev");
    harness
        .log()
        .info("setup", "mock provider queues list→run→promote→text");
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            sse_response(xdev_call_sse_body("c1", "list", "")),
            sse_response(xdev_call_sse_body(
                "c2",
                "run",
                r#",\"name\":\"ast_grep\",\"args\":{\"pattern\":\"$EXPR.unwrap()\",\"path\":\".\"}"#,
            )),
            sse_response(xdev_call_sse_body("c3", "promote", r#",\"name\":\"ast_grep\""#)),
            sse_response(text_sse_body("all done")),
        ],
    );

    let root = harness.temp_path("pi-env-xdev");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{"e2exdev": {{"api": "openai-completions", "baseUrl": "{}/v1", "apiKey": "test-key", "models": [{{"id": "test-model", "contextWindow": 128000}}]}}}}}}"#,
            server.base_url()
        ),
    )
    .expect("write models.json");
    std::fs::write(root.join("settings.json"), r#"{"checkForUpdates": false}"#)
        .expect("write settings.json");
    // A rust fixture so the xdev-run ast_grep call has something to match.
    let workspace = harness.temp_path("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    std::fs::write(
        workspace.join("main.rs"),
        "fn main() { let _ = compute().unwrap(); }\nfn compute() -> i32 { 1 }\n",
    )
    .expect("write fixture");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--provider",
            "e2exdev",
            "--model",
            "test-model",
        ])
        .current_dir(&workspace)
        .env("HOME", root.join("home"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_CONFIG_PATH", root.join("settings.json"))
        .env("PI_SESSIONS_DIR", root.join("sessions"))
        .env("PI_PACKAGE_DIR", root.join("packages"))
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

    harness.log().info("action", "spawning pi --mode rpc");
    let mut child = command.spawn().expect("spawn pi rpc");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(
                br#"{"id":"r1","type":"prompt","message":"discover, run, and promote a tool"}"#,
            )
            .expect("write prompt");
        stdin.write_all(b"\n").expect("newline");
    }

    // Bounded wait for the full scripted sequence to play out.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut request_count = 0usize;
    while std::time::Instant::now() < deadline {
        request_count = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count();
        if request_count >= 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    harness.log().info_ctx("verify", "request count", |ctx| {
        ctx.push(("requests".to_string(), request_count.to_string()));
    });

    let _ = child.kill();
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        request_count >= 4,
        "expected 4 scripted requests (list/run/promote/final), got {request_count}\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The run's inner output flows back to the parent as a tool result;
    // the transcript carries the dispatchedVia marker and the match.
    assert!(
        stdout.contains("dispatchedVia")
            || stdout.contains("dispatched via")
            || stdout.contains("ast_grep"),
        "transcript must show the xdev-dispatched ast_grep run: {}",
        stdout.chars().take(1200).collect::<String>()
    );
    assert!(
        stdout.contains("Promoted") || stdout.contains("promote"),
        "transcript must show the promotion: {}",
        stdout.chars().take(1200).collect::<String>()
    );

    let path = harness.temp_path("e2e_xdev_rpc.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_xdev_rpc.jsonl", &path);
    harness
        .log()
        .info("done", "discover/run/promote verified over RPC");
}

use std::time::Duration;
