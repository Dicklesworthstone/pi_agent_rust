//! E2E (bd-cv653.3.1 acceptance #2): subagent fan-out routes to the task/smol
//! role model end-to-end over real processes.
//!
//! Flow: a mock HTTP server fronts two OpenAI-compatible providers on distinct
//! path prefixes (`/default/v1` parent, `/role/v1` child). The parent `pi`
//! process runs the `e2edefault` provider; its scripted first response calls
//! the `subagent` tool on a `scout` agent whose definition pins NO model.
//! Settings assign `modelRoles.task = "e2erole/role-model"`. The child must
//! then reach the `/role/v1` prefix with model `role-model` in the request
//! body — proving parent → child role routing through the real binary
//! boundary (parent spawns the actual `pi` binary with `--model <role spec>`).
//!
//! No network beyond loopback; structured JSONL logs per tests/common/logging.rs.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// OpenAI chat-completions SSE: assistant message that calls the subagent tool.
fn tool_call_sse_body() -> String {
    [
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"subagent","arguments":"{\"agent\":\"scout\",\"task\":\"reply briefly\"}"}}]}}]}"#,
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

/// OpenAI chat-completions SSE: plain final text.
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

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

#[test]
fn e2e_subagent_child_uses_task_role_model() {
    let harness = TestHarness::new("e2e_subagent_child_uses_task_role_model");
    harness
        .log()
        .info("setup", "mock server + isolated pi env with two providers");

    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/default/v1/chat/completions",
        vec![
            sse_response(tool_call_sse_body()),
            sse_response(text_sse_body("parent done")),
        ],
    );
    server.add_route(
        "POST",
        "/role/v1/chat/completions",
        sse_response(text_sse_body("child ok")),
    );

    // Isolated environment (same isolation discipline as tests/e2e_cli.rs).
    let env_root = harness.temp_path("pi-env");
    std::fs::create_dir_all(env_root.join("agent/agents")).expect("mkdir agents");
    let home = env_root.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");

    // scout agent definition: deliberately NO model pin — the task role must win.
    std::fs::write(
        env_root.join("agent/agents/scout.md"),
        "---\nname: scout\ndescription: test scout\ntools: read\n---\nYou are a test scout.\n",
    )
    .expect("write scout agent");

    // Two OpenAI-compatible providers on distinct path prefixes.
    let models_json = format!(
        r#"{{"providers": {{
            "e2edefault": {{
                "api": "openai-completions",
                "baseUrl": "{}/default/v1",
                "apiKey": "test-key",
                "models": [{{"id": "parent-model", "contextWindow": 128000}}]
            }},
            "e2erole": {{
                "api": "openai-completions",
                "baseUrl": "{}/role/v1",
                "apiKey": "test-key",
                "models": [{{"id": "role-model", "contextWindow": 128000}}]
            }}
        }}}}"#,
        server.base_url(),
        server.base_url()
    );
    std::fs::write(env_root.join("agent/models.json"), models_json).expect("write models.json");

    // Task role assignment (bd-cv653.3.1).
    std::fs::write(
        env_root.join("settings.json"),
        r#"{"modelRoles": {"task": "e2erole/role-model"}, "checkForUpdates": false}"#,
    )
    .expect("write settings.json");

    harness
        .log()
        .info_ctx("action", "spawning parent pi", |ctx| {
            ctx.push((
                "settings".to_string(),
                env_root.join("settings.json").display().to_string(),
            ));
        });

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--print",
            "--no-session",
            "--provider",
            "e2edefault",
            "--model",
            "parent-model",
            "--tools",
            "subagent",
        ])
        .arg("run the scout")
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", env_root.join("agent"))
        .env("PI_CONFIG_PATH", env_root.join("settings.json"))
        .env("PI_SESSIONS_DIR", env_root.join("sessions"))
        .env("PI_PACKAGE_DIR", env_root.join("packages"))
        .env("PI_NO_AUTO_UPDATE_CHECK", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in [
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        command.env_remove(key);
    }

    let mut child = command.spawn().expect("spawn pi");

    // Poll the mock for the child request, bounded at 90s.
    let deadline = Instant::now() + Duration::from_secs(90);
    let (mut saw_parent, mut saw_child) = (false, false);
    let mut child_body_model = String::new();
    while Instant::now() < deadline && !(saw_parent && saw_child) {
        for request in server.requests() {
            if request.path == "/default/v1/chat/completions" {
                saw_parent = true;
            }
            if request.path == "/role/v1/chat/completions" {
                saw_child = true;
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&request.body) {
                    child_body_model = value["model"].as_str().unwrap_or_default().to_string();
                }
            }
        }
        if saw_parent && saw_child {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    harness
        .log()
        .info_ctx("verify", "requests observed", |ctx| {
            ctx.push(("saw_parent".to_string(), saw_parent.to_string()));
            ctx.push(("saw_child".to_string(), saw_child.to_string()));
            ctx.push(("child_body_model".to_string(), child_body_model.clone()));
        });

    let _ = child.kill();
    let output = child.wait_with_output().expect("wait");

    assert!(saw_parent, "parent never reached /default/v1 prefix");
    assert!(
        saw_child,
        "subagent child never reached /role/v1 prefix — task role routing failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        child_body_model, "role-model",
        "child request must carry the task-role model id"
    );

    let path = harness.temp_path("e2e_subagent_child_uses_task_role_model.jsonl");
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL schema violations: {errors:?}");
    harness.record_artifact("e2e_subagent_child_uses_task_role_model.jsonl", &path);
    harness.log().info("done", "case assertions passed");
}
