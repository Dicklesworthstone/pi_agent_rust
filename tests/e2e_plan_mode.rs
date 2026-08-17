//! E2E (bd-cv653.3.5 acceptance #1-#3): plan mode over RPC with a real binary
//! and scripted mock provider. Sequence: `set_plan_mode(on)` → scripted write
//! is blocked (`PLAN_MODE_BLOCKED`, zero bytes changed) → scripted
//! `submit_plan` → `approve_plan` command → scripted write executes. JSONL
//! logs per `tests/common/logging.rs`.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

/// Chat-completions SSE carrying one tool call to `tool`.
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

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_rpc_plan_mode_blocks_approves_executes() {
    let harness = TestHarness::new("e2e_rpc_plan_mode_blocks_approves_executes");
    harness
        .log()
        .info("setup", "scripted write→submit→(approve)→write flow");
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/v1/chat/completions",
        vec![
            // Turn 1: model tries to write (blocked), then submits a plan.
            sse_response(tool_call_sse_body(
                "w1",
                "write",
                r#"{"path":"plan_out.txt","content":"executed"}"#,
            )),
            sse_response(tool_call_sse_body(
                "s1",
                "submit_plan",
                r#"{"plan":"Goal: prove the gate. Steps: 1) write plan_out.txt. Verify: file content."}"#,
            )),
            sse_response(text_sse_body("plan submitted, awaiting review")),
            // Turn 2 (after approve_plan): the write now executes.
            sse_response(tool_call_sse_body(
                "w2",
                "write",
                r#"{"path":"plan_out.txt","content":"executed"}"#,
            )),
            sse_response(text_sse_body("executed the approved plan")),
        ],
    );

    let root = harness.temp_path("pi-env-plan");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{"e2eplan": {{"api": "openai-completions", "baseUrl": "{}/v1", "apiKey": "test-key", "models": [{{"id": "test-model", "contextWindow": 128000}}]}}}}}}"#,
            server.base_url()
        ),
    )
    .expect("write models.json");
    std::fs::write(root.join("settings.json"), r#"{"checkForUpdates": false}"#)
        .expect("write settings.json");
    let workspace = harness.temp_path("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--mode",
            "rpc",
            "--provider",
            "e2eplan",
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

    let mut child = command.spawn().expect("spawn pi rpc");
    let mut stdin = child.stdin.take().expect("stdin");

    let send = |stdin: &mut std::process::ChildStdin, line: &str| {
        stdin.write_all(line.as_bytes()).expect("write command");
        stdin.write_all(b"\n").expect("newline");
    };

    // Enter plan mode, then prompt (turn 1: blocked write + submit).
    send(
        &mut stdin,
        r#"{"id":"c0","type":"set_plan_mode","mode":"on"}"#,
    );
    send(
        &mut stdin,
        r#"{"id":"c1","type":"prompt","message":"write the file"}"#,
    );

    // Wait for turn 1 to complete (3 requests), then approve and prompt again.
    let deadline = Instant::now() + Duration::from_secs(120);
    let wait_requests = |server: &common::harness::MockHttpServer, n: usize| {
        while Instant::now() < deadline {
            let count = server
                .requests()
                .into_iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count();
            if count >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    };
    assert!(
        wait_requests(&server, 3),
        "turn 1 never completed (write block + submit + final text)"
    );

    let blocked_file = workspace.join("plan_out.txt");
    assert!(
        !blocked_file.exists(),
        "the blocked write must not create the file while planning"
    );

    send(&mut stdin, r#"{"id":"c2","type":"approve_plan"}"#);
    send(
        &mut stdin,
        r#"{"id":"c3","type":"prompt","message":"proceed with the approved plan"}"#,
    );

    assert!(
        wait_requests(&server, 5),
        "turn 2 never completed (write + final text)"
    );
    let _ = child.kill();
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // After approval, the write executes for real.
    let written = std::fs::read_to_string(&blocked_file).unwrap_or_default();
    assert_eq!(written, "executed", "approved write lands on disk");

    // The blocked attempt's result reached the model (evidence in a request body).
    let saw_block_notice = server.requests().into_iter().any(|r| {
        r.path == "/v1/chat/completions"
            && String::from_utf8_lossy(&r.body).contains("PLAN_MODE_BLOCKED")
    });
    assert!(
        saw_block_notice,
        "the model must receive the PLAN_MODE_BLOCKED tool result"
    );
    assert!(
        stdout.contains("plan_mode") || stdout.contains("approved"),
        "plan transitions surface in RPC output: {}",
        stdout.chars().take(800).collect::<String>()
    );

    let path = harness.temp_path("e2e_plan_mode_rpc.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_plan_mode_rpc.jsonl", &path);
    harness
        .log()
        .info("done", "block → approve → execute verified over RPC");
}
