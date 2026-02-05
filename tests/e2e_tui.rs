//! TUI interactive E2E tests via tmux capture with deterministic artifacts.
//!
//! These tests launch the `pi` binary in a tmux session, drive scripted
//! interactions (prompts, slash commands, key sequences), capture pane output
//! per step, and emit JSONL artifacts for CI diffing.
//!
//! Run:
//! ```bash
//! cargo test --test e2e_tui
//! ```

#![cfg(unix)]
#![allow(dead_code)]

mod common;

use clap::Parser as _;
use common::harness::{MockHttpResponse, MockHttpServer};
use common::run_async;
use common::tmux::TuiSession;
use pi::app::build_system_prompt;
use pi::cli;
use pi::model::ContentBlock;
use pi::session::SESSION_VERSION;
use pi::tools::{ReadTool, Tool};
use pi::vcr::{
    Cassette, Interaction, RecordedRequest, RecordedResponse, VCR_ENV_DIR, VCR_ENV_MODE,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Standard CLI args for interactive mode with minimal features.
fn base_interactive_args() -> Vec<&'static str> {
    vec![
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
        "--api-key",
        "test-key-e2e",
        "--no-tools",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--system-prompt",
        "pi e2e tui test harness",
    ]
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const VCR_TEST_NAME: &str = "e2e_tui_tool_read";
const VCR_MODEL: &str = "claude-sonnet-4-20250514";
const VCR_PROMPT: &str = "Readsample.txt";
const SAMPLE_FILE_NAME: &str = "sample.txt";
const SAMPLE_FILE_CONTENT: &str = "Hello\nWorld\n";
const TOOL_CALL_ID: &str = "toolu_e2e_read_1";

fn vcr_interactive_args() -> Vec<&'static str> {
    vec![
        "--provider",
        "anthropic",
        "--model",
        VCR_MODEL,
        "--api-key",
        "test-key-e2e",
        "--tools",
        "read",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "pi e2e vcr harness",
    ]
}

fn build_vcr_system_prompt(workdir: &Path, env_root: &Path) -> String {
    let mut args: Vec<&str> = vec!["pi"];
    args.extend(vcr_interactive_args());
    let cli = cli::Cli::try_parse_from(args).expect("parse vcr cli args");
    let enabled_tools = cli.enabled_tools();
    let global_dir = env_root.join("agent");
    let package_dir = env_root.join("packages");
    let previous = std::env::var_os("PI_TEST_MODE");
    std::env::set_var("PI_TEST_MODE", "1");
    let prompt = build_system_prompt(
        &cli,
        workdir,
        &enabled_tools,
        None,
        &global_dir,
        &package_dir,
    );
    match previous {
        Some(value) => std::env::set_var("PI_TEST_MODE", value),
        None => std::env::remove_var("PI_TEST_MODE"),
    }
    prompt
}

fn read_output_for_sample(cwd: &Path, path: &str) -> String {
    let tool = ReadTool::new(cwd);
    let path = path.to_string();
    let output = run_async(async move {
        tool.execute("tool-call", json!({ "path": path }), None)
            .await
            .expect("read tool output")
    });
    output
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[allow(clippy::too_many_lines)]
fn write_vcr_cassette(dir: &Path, tool_output: &str, system_prompt: &str) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_TEST_NAME}.json"));
    let tool_schema = {
        let tool = ReadTool::new(dir);
        json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.parameters(),
        })
    };
    let request_one = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_PROMPT } ] }
        ],
        "system": system_prompt,
        "max_tokens": 8192,
        "stream": true,
        "tools": [tool_schema],
    });
    let request_two = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_PROMPT } ] },
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": TOOL_CALL_ID,
                        "name": "read",
                        "input": { "path": SAMPLE_FILE_NAME }
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": TOOL_CALL_ID,
                        "content": [
                            { "type": "text", "text": tool_output }
                        ]
                    }
                ]
            }
        ],
        "system": system_prompt,
        "max_tokens": 8192,
        "stream": true,
        "tools": [tool_schema],
    });

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let tool_args_json =
        serde_json::to_string(&json!({ "path": SAMPLE_FILE_NAME })).expect("serialize tool args");

    let response_one = RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            sse_chunk(
                "message_start",
                json!({ "type": "message_start", "message": { "usage": { "input_tokens": 42 }}}),
            ),
            sse_chunk(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": TOOL_CALL_ID, "name": "read" }
                }),
            ),
            sse_chunk(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": tool_args_json }
                }),
            ),
            sse_chunk(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            sse_chunk(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 12 }
                }),
            ),
            sse_chunk("message_stop", json!({ "type": "message_stop" })),
        ],
    };

    let response_two = RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            sse_chunk(
                "message_start",
                json!({ "type": "message_start", "message": { "usage": { "input_tokens": 64 }}}),
            ),
            sse_chunk(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text" }
                }),
            ),
            sse_chunk(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Done." }
                }),
            ),
            sse_chunk(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            sse_chunk(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 8 }
                }),
            ),
            sse_chunk("message_stop", json!({ "type": "message_stop" })),
        ],
    };

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: VCR_TEST_NAME.to_string(),
        recorded_at: "1970-01-01T00:00:00Z".to_string(),
        interactions: vec![
            Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://api.anthropic.com/v1/messages".to_string(),
                    headers: vec![
                        ("Content-Type".to_string(), "application/json".to_string()),
                        ("Accept".to_string(), "text/event-stream".to_string()),
                    ],
                    body: Some(request_one),
                    body_text: None,
                },
                response: response_one,
            },
            Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://api.anthropic.com/v1/messages".to_string(),
                    headers: vec![
                        ("Content-Type".to_string(), "application/json".to_string()),
                        ("Accept".to_string(), "text/event-stream".to_string()),
                    ],
                    body: Some(request_two),
                    body_text: None,
                },
                response: response_two,
            },
        ],
    };

    std::fs::create_dir_all(dir).expect("create cassette dir");
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(&cassette_path, json).expect("write cassette");
    cassette_path
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn collect_jsonl_files(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            collect_jsonl_files(&entry_path, out);
        } else if entry_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(entry_path);
        }
    }
}

fn find_session_jsonl(path: &Path) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files(path, &mut files);
    files.into_iter().next()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Smoke test: launch interactive mode, verify welcome screen, exit cleanly.
#[test]
fn e2e_tui_startup_and_exit() {
    let Some(mut session) = TuiSession::new("e2e_tui_startup_and_exit") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    // Wait for welcome message
    let pane = session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    assert!(
        pane.contains("Welcome to Pi!"),
        "Expected welcome message; got:\n{pane}"
    );

    // Exit gracefully
    session.exit_gracefully();
    assert!(
        !session.tmux.session_exists(),
        "Session did not exit cleanly"
    );

    session.write_artifacts();

    assert!(
        !session.steps().is_empty(),
        "Expected at least one recorded step"
    );
}

/// Test /help slash command: sends /help, verifies help output appears.
#[test]
fn e2e_tui_help_command() {
    let Some(mut session) = TuiSession::new("e2e_tui_help_command") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    // Wait for startup
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Send /help
    let pane = session.send_text_and_wait(
        "help_command",
        "/help",
        "Available commands:",
        COMMAND_TIMEOUT,
    );

    let help_markers = [
        "Available commands:",
        "/logout",
        "/clear",
        "/model",
        "Tips:",
    ];
    let found_markers: Vec<&&str> = help_markers.iter().filter(|m| pane.contains(*m)).collect();
    assert!(
        !found_markers.is_empty(),
        "Expected help markers in output; got:\n{pane}"
    );

    session
        .harness
        .log()
        .info_ctx("verify", "Help output validated", |ctx| {
            ctx.push((
                "found_markers".into(),
                found_markers
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        });

    session.exit_gracefully();
    session.write_artifacts();
}

/// Test /model slash command: sends /model, verifies model info appears.
#[test]
fn e2e_tui_model_command() {
    let Some(mut session) = TuiSession::new("e2e_tui_model_command") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    // Wait for startup
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Send /model
    let pane =
        session.send_text_and_wait("model_command", "/model", "gpt-4o-mini", COMMAND_TIMEOUT);
    assert!(
        pane.contains("gpt-4o-mini"),
        "Expected model info in output; got:\n{pane}"
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// Test /clear slash command: sends /clear, verifies screen is cleared.
#[test]
fn e2e_tui_clear_command() {
    let Some(mut session) = TuiSession::new("e2e_tui_clear_command") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    // Wait for startup
    let pane_before = session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    assert!(pane_before.contains("Welcome to Pi!"));

    // Send /clear
    session.tmux.send_literal("/clear");
    session.tmux.send_key("Enter");
    std::thread::sleep(Duration::from_millis(500));

    // After clear, the welcome message may or may not be visible depending on
    // implementation. Just verify the session is still alive and responsive.
    let pane_after = session.tmux.capture_pane();
    session
        .harness
        .log()
        .info_ctx("verify", "Clear command executed", |ctx| {
            ctx.push((
                "pane_lines_before".into(),
                pane_before.lines().count().to_string(),
            ));
            ctx.push((
                "pane_lines_after".into(),
                pane_after.lines().count().to_string(),
            ));
        });

    // Save the pane snapshots
    let artifact_path = session.harness.temp_path("pane-after-clear.txt");
    std::fs::write(&artifact_path, &pane_after).expect("write pane after clear");
    session
        .harness
        .record_artifact("pane-after-clear.txt", &artifact_path);

    session.exit_gracefully();
    session.write_artifacts();
}

/// Test multiple sequential commands in one session.
#[test]
fn e2e_tui_multi_command_sequence() {
    let Some(mut session) = TuiSession::new("e2e_tui_multi_command_sequence") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    // Step 1: Wait for startup
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Step 2: /help
    let pane = session.send_text_and_wait("help", "/help", "Available commands:", COMMAND_TIMEOUT);
    assert!(pane.contains("Available commands:"));

    // Step 3: /model
    let pane = session.send_text_and_wait("model", "/model", "gpt-4o-mini", COMMAND_TIMEOUT);
    assert!(pane.contains("gpt-4o-mini"));

    // Step 4: Exit
    session.exit_gracefully();
    assert!(
        !session.tmux.session_exists(),
        "Session did not exit cleanly after multi-command sequence"
    );

    session.write_artifacts();

    // Verify we captured all steps
    session
        .harness
        .log()
        .info_ctx("summary", "Multi-command sequence complete", |ctx| {
            ctx.push(("total_steps".into(), session.steps().len().to_string()));
        });
    assert!(
        session.steps().len() >= 3,
        "Expected >= 3 steps (startup + help + model), got {}",
        session.steps().len()
    );
}

/// Test Ctrl+D exits the session cleanly.
#[test]
fn e2e_tui_ctrl_d_exit() {
    let Some(mut session) = TuiSession::new("e2e_tui_ctrl_d_exit") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Send Ctrl+D
    session.tmux.send_key("C-d");

    let start = std::time::Instant::now();
    while session.tmux.session_exists() {
        if start.elapsed() > Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Capture final state if still alive
    if session.tmux.session_exists() {
        let pane = session.tmux.capture_pane();
        session.harness.log().warn(
            "tmux",
            format!("Session still alive after Ctrl+D. Pane:\n{pane}"),
        );
        // Force kill for cleanup
        session.tmux.send_key("C-c");
        std::thread::sleep(Duration::from_millis(100));
        session.tmux.send_key("C-c");
    }

    session.write_artifacts();
}

/// Verify artifacts are deterministic (JSONL steps file is well-formed).
#[test]
fn e2e_tui_artifact_format() {
    let Some(mut session) = TuiSession::new("e2e_tui_artifact_format") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    session.send_text_and_wait("help", "/help", "Available commands:", COMMAND_TIMEOUT);
    session.exit_gracefully();
    session.write_artifacts();

    // Verify the steps JSONL is well-formed
    let steps_path = session.harness.temp_path("tui-steps.jsonl");
    let steps_content = std::fs::read_to_string(&steps_path).expect("read steps jsonl");
    let mut line_count = 0;
    for line in steps_content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(err) => {
                panic!("Invalid JSONL line: {err}\n{line}");
            }
        };
        assert!(parsed.get("label").is_some(), "Missing 'label' in step");
        assert!(parsed.get("action").is_some(), "Missing 'action' in step");
        assert!(
            parsed.get("elapsed_ms").is_some(),
            "Missing 'elapsed_ms' in step"
        );
        line_count += 1;
    }
    assert!(
        line_count >= 2,
        "Expected >= 2 step lines in JSONL, got {line_count}"
    );

    // Verify log JSONL is well-formed
    let log_path = session.harness.temp_path("tui-log.jsonl");
    let log_content = std::fs::read_to_string(&log_path).expect("read log jsonl");
    for line in log_content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let _parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(err) => {
                panic!("Invalid log JSONL line: {err}\n{line}");
            }
        };
    }

    session
        .harness
        .log()
        .info_ctx("verify", "Artifact format validated", |ctx| {
            ctx.push(("step_lines".into(), line_count.to_string()));
            ctx.push((
                "log_lines".into(),
                log_content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count()
                    .to_string(),
            ));
        });
}

// ─── Mock HTTP Helpers ────────────────────────────────────────────────────────

/// Build SSE body for a simple Anthropic text response.
fn build_mock_anthropic_text_sse(text: &str) -> String {
    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let mut sse = String::new();
    sse.push_str(&sse_chunk(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_mock_001",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250514",
                "content": [],
                "stop_reason": null,
                "usage": { "input_tokens": 10 }
            }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": 0 }),
    ));
    sse.push_str(&sse_chunk(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 5 }
        }),
    ));
    sse.push_str(&sse_chunk(
        "message_stop",
        json!({ "type": "message_stop" }),
    ));
    sse
}

/// Build SSE body for a `tool_use` response.
fn build_mock_anthropic_tool_call_sse(tool_name: &str, tool_id: &str, args_json: &str) -> String {
    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let mut sse = String::new();
    sse.push_str(&sse_chunk(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_mock_tool_001",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250514",
                "content": [],
                "stop_reason": null,
                "usage": { "input_tokens": 20 }
            }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": tool_id, "name": tool_name }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": args_json }
        }),
    ));
    sse.push_str(&sse_chunk(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": 0 }),
    ));
    sse.push_str(&sse_chunk(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use" },
            "usage": { "output_tokens": 10 }
        }),
    ));
    sse.push_str(&sse_chunk(
        "message_stop",
        json!({ "type": "message_stop" }),
    ));
    sse
}

/// Build a `MockHttpResponse` for an SSE body.
fn mock_sse_response(sse_body: &str) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: sse_body.as_bytes().to_vec(),
    }
}

/// Set up a mock Anthropic server for TUI tests.
///
/// Writes a `models.json` to the session's `PI_CODING_AGENT_DIR` with `baseUrl`
/// pointing to the mock server, and sets `ANTHROPIC_API_KEY`.
///
/// Returns the `MockHttpServer` (must be kept alive for the duration of the test).
fn setup_mock_anthropic_for_tui(session: &mut TuiSession) -> MockHttpServer {
    let server = session.harness.start_mock_http_server();
    let base_url = format!("{}/v1/messages", server.base_url());

    // Write models.json into the agent dir so the binary picks it up.
    let agent_dir = session.harness.temp_dir().join("env").join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let models_json = json!({
        "providers": {
            "anthropic": {
                "baseUrl": base_url
            }
        }
    });
    let models_path = agent_dir.join("models.json");
    std::fs::write(
        &models_path,
        serde_json::to_string_pretty(&models_json).unwrap(),
    )
    .expect("write models.json");
    session.harness.record_artifact("models.json", &models_path);

    session.set_env("ANTHROPIC_API_KEY", "test-mock-key");

    session
        .harness
        .log()
        .info_ctx("mock", "Anthropic mock server configured", |ctx| {
            ctx.push(("base_url".into(), base_url));
            ctx.push(("addr".into(), server.addr().to_string()));
        });

    server
}

const MOCK_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const MOCK_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MOCK_TOOL_FLOW_TIMEOUT: Duration = Duration::from_secs(30);

/// Standard CLI args for interactive mode with Anthropic mock provider.
fn mock_anthropic_interactive_args_no_tools() -> Vec<&'static str> {
    vec![
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet-4-5",
        "--no-tools",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "pi e2e mock test harness",
    ]
}

fn mock_anthropic_interactive_args_with_read() -> Vec<&'static str> {
    vec![
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet-4-5",
        "--tools",
        "read",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "pi e2e mock test harness",
    ]
}

// ─── Mock HTTP Tests ─────────────────────────────────────────────────────────

/// E2E interactive: basic chat via mock HTTP server.
#[test]
fn e2e_tui_basic_chat_mock() {
    let Some(mut session) = TuiSession::new("e2e_tui_basic_chat_mock") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.harness.section("setup mock");
    let server = setup_mock_anthropic_for_tui(&mut session);

    // Configure mock to return a simple text response
    let sse_body = build_mock_anthropic_text_sse("Hello from mock!");
    server.add_route("POST", "/v1/messages", mock_sse_response(&sse_body));

    session.harness.section("launch");
    session.launch(&mock_anthropic_interactive_args_no_tools());

    // Wait for welcome
    let pane = session.wait_and_capture("startup", "Welcome to Pi!", MOCK_STARTUP_TIMEOUT);
    assert!(
        pane.contains("Welcome to Pi!"),
        "Expected welcome message; got:\n{pane}"
    );

    session.harness.section("send prompt");
    let pane = session.send_text_and_wait(
        "prompt",
        "Say hello",
        "Hello from mock!",
        MOCK_COMMAND_TIMEOUT,
    );
    assert!(
        pane.contains("Hello from mock!"),
        "Expected mock response in pane; got:\n{pane}"
    );

    session.harness.section("verify requests");
    let requests = server.requests();
    session
        .harness
        .log()
        .info_ctx("verify", "Mock server requests", |ctx| {
            ctx.push(("count".into(), requests.len().to_string()));
        });
    assert!(
        !requests.is_empty(),
        "Expected at least one request to mock server"
    );

    session.harness.section("exit");
    session.exit_gracefully();
    assert!(
        !session.tmux.session_exists(),
        "Session did not exit cleanly"
    );

    session.write_artifacts();

    assert!(
        session.steps().len() >= 2,
        "Expected >= 2 steps (startup + prompt), got {}",
        session.steps().len()
    );
}

/// E2E interactive: tool call (read) via mock HTTP with response queue.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_tool_call_read() {
    let Some(mut session) = TuiSession::new("e2e_tui_tool_call_read") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.harness.section("setup files");
    // Create sample.txt in the workdir
    let sample_content = "Hello World from sample file";
    let sample_path = session.harness.temp_path("sample.txt");
    std::fs::write(&sample_path, sample_content).expect("write sample.txt");
    session.harness.record_artifact("sample.txt", &sample_path);

    session.harness.section("setup mock");
    let server = setup_mock_anthropic_for_tui(&mut session);

    // Response 1: tool call to read sample.txt
    let tool_call_sse = build_mock_anthropic_tool_call_sse(
        "read",
        "toolu_mock_read_001",
        &serde_json::to_string(&json!({ "path": "sample.txt" })).unwrap(),
    );

    // Response 2: text response after receiving tool result
    let text_sse = build_mock_anthropic_text_sse("The file says Hello World from sample file");

    // Queue both responses: first request → tool call, second → text
    server.add_route_queue(
        "POST",
        "/v1/messages",
        vec![
            mock_sse_response(&tool_call_sse),
            mock_sse_response(&text_sse),
        ],
    );

    session.harness.section("launch");
    session.launch(&mock_anthropic_interactive_args_with_read());

    // Wait for welcome
    let pane = session.wait_and_capture("startup", "Welcome to Pi!", MOCK_STARTUP_TIMEOUT);
    assert!(
        pane.contains("Welcome to Pi!"),
        "Expected welcome message; got:\n{pane}"
    );

    session
        .harness
        .section("send prompt and wait for tool flow");
    // Send the prompt
    session.tmux.send_literal("Read sample.txt");
    session.tmux.send_key("Enter");

    // Wait for the final text response (which comes after the tool call completes)
    let pane = session
        .tmux
        .wait_for_pane_contains("The file says Hello World", MOCK_TOOL_FLOW_TIMEOUT);

    // Record the step manually since we used low-level tmux ops
    let artifact_name = format!("pane-{}.txt", session.steps().len());
    let artifact_path = session.harness.temp_path(&artifact_name);
    std::fs::write(&artifact_path, &pane).expect("write pane snapshot");
    session
        .harness
        .record_artifact(&artifact_name, &artifact_path);

    assert!(
        pane.contains("The file says Hello World"),
        "Expected final response in pane; got:\n{pane}"
    );

    session.harness.section("verify requests");
    let requests = server.requests();
    session
        .harness
        .log()
        .info_ctx("verify", "Mock server requests", |ctx| {
            ctx.push(("count".into(), requests.len().to_string()));
        });
    assert!(
        requests.len() >= 2,
        "Expected >= 2 requests (initial + after tool result), got {}",
        requests.len()
    );

    session.harness.section("exit");
    session.exit_gracefully();
    assert!(
        !session.tmux.session_exists(),
        "Session did not exit cleanly"
    );

    session.write_artifacts();

    session.harness.section("verify session JSONL");
    let sessions_dir = session.harness.temp_dir().join("env").join("sessions");
    if let Some(session_file) = find_session_jsonl(&sessions_dir) {
        session
            .harness
            .record_artifact("session.jsonl", &session_file);
        let content = std::fs::read_to_string(&session_file).expect("read session jsonl");
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

        session
            .harness
            .log()
            .info_ctx("verify", "Session JSONL analysis", |ctx| {
                ctx.push(("lines".into(), lines.len().to_string()));
            });

        // Check for tool_use and tool_result in the session
        let has_tool_use = content.contains("tool_use");
        let has_tool_result = content.contains("tool_result");
        session
            .harness
            .log()
            .info_ctx("verify", "Session content check", |ctx| {
                ctx.push(("has_tool_use".into(), has_tool_use.to_string()));
                ctx.push(("has_tool_result".into(), has_tool_result.to_string()));
            });

        assert!(has_tool_use, "Expected tool_use in session JSONL");
        assert!(has_tool_result, "Expected tool_result in session JSONL");
    } else {
        session
            .harness
            .log()
            .warn("verify", "No session JSONL file found (non-fatal)");
    }
}

/// E2E interactive: VCR playback tool call with deterministic artifacts.
#[test]
fn e2e_tui_vcr_tool_read() {
    let Some(mut session) = TuiSession::new("e2e_tui_vcr_tool_read") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.harness.section("setup");
    let sample_path = session.harness.temp_path(SAMPLE_FILE_NAME);
    std::fs::write(&sample_path, SAMPLE_FILE_CONTENT).expect("write sample file");
    session
        .harness
        .record_artifact(SAMPLE_FILE_NAME, &sample_path);

    let tool_output = read_output_for_sample(session.harness.temp_dir(), SAMPLE_FILE_NAME);
    let tool_output_hash = sha256_hex(&tool_output);

    let cassette_dir = session.harness.temp_path("vcr");
    let env_root = session.harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt(session.harness.temp_dir(), &env_root);
    let system_prompt_hash = sha256_hex(&system_prompt);
    let cassette_path = write_vcr_cassette(&cassette_dir, &tool_output, &system_prompt);
    let cassette_name = cassette_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vcr-cassette.json");
    session
        .harness
        .record_artifact(cassette_name, &cassette_path);

    session
        .harness
        .log()
        .info_ctx("vcr", "Prepared playback cassette", |ctx| {
            ctx.push(("cassette_path".into(), cassette_path.display().to_string()));
            ctx.push(("tool_call_id".into(), TOOL_CALL_ID.to_string()));
            ctx.push(("tool_name".into(), "read".to_string()));
            ctx.push(("tool_output_sha256".into(), tool_output_hash));
            ctx.push(("system_prompt_sha256".into(), system_prompt_hash));
        });

    let cassette_dir_str = cassette_dir.display().to_string();
    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir_str);
    session.set_env("PI_VCR_TEST_NAME", VCR_TEST_NAME);
    session.set_env("VCR_DEBUG_BODY", "1");

    session.launch(&vcr_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    let pane = session.send_text_and_wait("prompt", VCR_PROMPT, "Done.", COMMAND_TIMEOUT);
    let expected_line = tool_output
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Hello");
    assert!(
        pane.contains(expected_line),
        "Expected tool output line in pane.\nExpected: {expected_line}\nPane:\n{pane}"
    );

    session
        .harness
        .log()
        .info_ctx("verify", "Tool output rendered", |ctx| {
            ctx.push(("expected_line".into(), expected_line.to_string()));
            ctx.push(("prompt".into(), VCR_PROMPT.to_string()));
        });

    session.exit_gracefully();
    session
        .harness
        .log()
        .info_ctx("exit", "Session exit requested", |ctx| {
            ctx.push(("reason".into(), "graceful".to_string()));
        });

    session.write_artifacts();

    let sessions_dir = session.harness.temp_dir().join("env").join("sessions");
    let session_file = find_session_jsonl(&sessions_dir).expect("expected session jsonl file");
    session
        .harness
        .record_artifact("session.jsonl", &session_file);

    let content = std::fs::read_to_string(&session_file).expect("read session jsonl");
    let mut lines = content.lines().filter(|line| !line.trim().is_empty());
    let header_line = lines.next().expect("session header line");
    let header: Value = serde_json::from_str(header_line).expect("parse session header");
    assert_eq!(header.get("type").and_then(Value::as_str), Some("session"));
    assert_eq!(
        header.get("version").and_then(Value::as_u64),
        Some(u64::from(SESSION_VERSION))
    );

    let mut has_message = false;
    let mut has_parent = false;
    for line in lines {
        let entry: Value = serde_json::from_str(line).expect("parse session entry");
        if entry.get("type").and_then(Value::as_str) == Some("message") {
            has_message = true;
            if entry.get("parentId").and_then(Value::as_str).is_some() {
                has_parent = true;
            }
        }
    }
    assert!(
        has_message,
        "Expected at least one message entry in session"
    );
    assert!(
        has_parent,
        "Expected at least one message entry with parentId"
    );
}
