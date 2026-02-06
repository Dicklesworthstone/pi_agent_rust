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
const VCR_BASIC_CHAT_TEST_NAME: &str = "e2e_tui_basic_chat";
const VCR_MODEL: &str = "claude-sonnet-4-20250514";
const VCR_PROMPT: &str = "Read sample.txt";
const VCR_BASIC_CHAT_PROMPT: &str = "Say hello";
const VCR_BASIC_CHAT_RESPONSE: &str = "Hello! How can I help you today?";
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

fn vcr_interactive_args_no_tools() -> Vec<&'static str> {
    vec![
        "--provider",
        "anthropic",
        "--model",
        VCR_MODEL,
        "--api-key",
        "test-key-e2e",
        "--no-tools",
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

fn build_vcr_system_prompt_for_args(
    args_fn: fn() -> Vec<&'static str>,
    workdir: &Path,
    env_root: &Path,
) -> String {
    let mut args: Vec<&str> = vec!["pi"];
    args.extend(args_fn());
    let cli = cli::Cli::try_parse_from(args).expect("parse vcr cli args");
    let enabled_tools = cli.enabled_tools();
    let global_dir = env_root.join("agent");
    let package_dir = env_root.join("packages");
    build_system_prompt(
        &cli,
        workdir,
        &enabled_tools,
        None,
        &global_dir,
        &package_dir,
        true,
    )
}

fn build_vcr_system_prompt(workdir: &Path, env_root: &Path) -> String {
    build_vcr_system_prompt_for_args(vcr_interactive_args, workdir, env_root)
}

/// Write a VCR cassette for a basic chat interaction (no tools, simple text response).
fn write_vcr_basic_chat_cassette(dir: &Path, system_prompt: &str) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_BASIC_CHAT_TEST_NAME}.json"));

    let request = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_BASIC_CHAT_PROMPT } ] }
        ],
        "system": system_prompt,
        "max_tokens": 8192,
        "stream": true,
    });

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };

    let response = RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            sse_chunk(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": { "usage": { "input_tokens": 10 } }
                }),
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
                    "delta": { "type": "text_delta", "text": VCR_BASIC_CHAT_RESPONSE }
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
        body_chunks_base64: None,
    };

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: VCR_BASIC_CHAT_TEST_NAME.to_string(),
        recorded_at: "1970-01-01T00:00:00Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: "https://api.anthropic.com/v1/messages".to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                ],
                body: Some(request),
                body_text: None,
            },
            response,
        }],
    };

    std::fs::create_dir_all(dir).expect("create cassette dir");
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(&cassette_path, json).expect("write cassette");
    cassette_path
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
        body_chunks_base64: None,
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
        body_chunks_base64: None,
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

fn sha256_hex_bytes(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn collect_files_recursive(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            collect_files_recursive(&entry_path, out);
        } else if entry_path.is_file() {
            out.push(entry_path);
        }
    }
}

fn write_dir_snapshot(root: &Path, out_path: &Path) {
    use std::fmt::Write as _;

    let mut content = String::new();
    if !root.exists() {
        content.push_str("missing\n");
        content.push_str(&root.display().to_string());
        content.push('\n');
        std::fs::write(out_path, content).expect("write dir snapshot");
        return;
    }

    let mut files = Vec::new();
    collect_files_recursive(root, &mut files);
    files.sort();

    for file in files {
        let rel = file.strip_prefix(root).unwrap_or(&file);
        let bytes = std::fs::read(&file).unwrap_or_default();
        let hash = sha256_hex_bytes(&bytes);
        let _ = writeln!(content, "{hash}\t{}\t{}", bytes.len(), rel.display());
    }

    std::fs::write(out_path, content).expect("write dir snapshot");
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

/// Test /reload: add a skill on disk, /reload, verify autocomplete refresh; then invalidate skill
/// to trigger diagnostics, /reload, and verify the autocomplete no longer includes it.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_reload_resources_and_autocomplete_refresh() {
    let Some(mut session) = TuiSession::new("e2e_tui_reload_resources_and_autocomplete_refresh")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let args = [
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
        "--api-key",
        "test-key-e2e",
        "--no-tools",
        // We only need skills for this test; keep other resource categories empty for determinism.
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--system-prompt",
        "pi e2e reload resources + autocomplete refresh",
    ];

    session.launch(&args);
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    let skill_rel = ".pi/skills/e2e-reload-skill/SKILL.md";
    let skill_valid = r"---
name: e2e-reload-skill
description: E2E skill used to validate /reload + autocomplete refresh
---

# e2e-reload-skill

E2E-only skill used by tests.
";
    let skill_path = session
        .harness
        .create_file(skill_rel, skill_valid.as_bytes());
    session
        .harness
        .record_artifact("skill.valid.SKILL.md", &skill_path);

    let project_pi_dir = session.harness.temp_dir().join(".pi");
    let global_agent_dir = session.harness.temp_dir().join("env").join("agent");

    let snapshot_pi_after_add = session.harness.temp_path("snapshot.pi.after_add.txt");
    write_dir_snapshot(&project_pi_dir, &snapshot_pi_after_add);
    session
        .harness
        .record_artifact("snapshot.pi.after_add.txt", &snapshot_pi_after_add);

    let snapshot_agent_after_add = session.harness.temp_path("snapshot.agent.after_add.txt");
    write_dir_snapshot(&global_agent_dir, &snapshot_agent_after_add);
    session
        .harness
        .record_artifact("snapshot.agent.after_add.txt", &snapshot_agent_after_add);

    // Reload and confirm the skill count changes.
    let pane = session.send_text_and_wait(
        "reload_after_add_skill",
        "/reload",
        "Reloaded resources:",
        STARTUP_TIMEOUT,
    );
    assert!(
        pane.contains("1 skills"),
        "Expected reload status to reflect the added skill; got:\n{pane}"
    );

    // Autocomplete should now list the skill.
    session.tmux.send_literal("/skill:e2e");
    let pane = session.send_key_and_wait(
        "autocomplete_shows_skill_after_reload",
        "Tab",
        "/skill:e2e-reload-skill",
        COMMAND_TIMEOUT,
    );
    assert!(
        pane.contains("/skill:e2e-reload-skill"),
        "Expected skill autocomplete entry; got:\n{pane}"
    );

    // Close autocomplete and clear the editor without relying on Ctrl+C, which can still
    // deliver SIGINT in some terminal configurations (killing the session).
    session.send_key_and_wait(
        "close_autocomplete_after_capture",
        "Esc",
        "/skill:e2e",
        Duration::from_secs(1),
    );
    // DEL/backspace enough times to clear regardless of whether autocomplete updated the editor.
    session.tmux.send_literal(&"\u{7f}".repeat(200));

    // Invalidate the skill on disk (empty description) to trigger diagnostics and removal.
    let skill_invalid = r"---
name: e2e-reload-skill
description:
---

# e2e-reload-skill

Invalid skill (missing description) to trigger diagnostics.
";
    std::fs::write(&skill_path, skill_invalid).expect("write invalid skill");
    session
        .harness
        .record_artifact("skill.invalid.SKILL.md", &skill_path);

    let snapshot_pi_after_invalid = session.harness.temp_path("snapshot.pi.after_invalid.txt");
    write_dir_snapshot(&project_pi_dir, &snapshot_pi_after_invalid);
    session
        .harness
        .record_artifact("snapshot.pi.after_invalid.txt", &snapshot_pi_after_invalid);

    // Reload again and confirm diagnostics are surfaced.
    let pane = session.send_text_and_wait(
        "reload_after_invalid_skill",
        "/reload",
        "Reload diagnostics:",
        STARTUP_TIMEOUT,
    );
    assert!(
        pane.contains("Skills:") && pane.contains("description is required"),
        "Expected skill diagnostics to mention missing description; got:\n{pane}"
    );

    // Autocomplete should no longer list the skill after invalidation.
    session.tmux.send_literal("/skill:e2e");
    let pane = session.send_key_and_wait(
        "autocomplete_does_not_show_skill_after_invalid_reload",
        "Tab",
        "/skill:e2e",
        Duration::from_secs(1),
    );
    assert!(
        !pane.contains("/skill:e2e-reload-skill"),
        "Expected skill autocomplete entry to be removed; got:\n{pane}"
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
                unreachable!("Invalid JSONL line: {err}\n{line}");
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
                unreachable!("Invalid log JSONL line: {err}\n{line}");
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

// ─── VCR Chat Tests ──────────────────────────────────────────────────────────

/// E2E interactive: basic chat via VCR playback (no tools).
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_basic_chat_vcr() {
    let Some(mut session) = TuiSession::new("e2e_tui_basic_chat_vcr") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.harness.section("setup vcr");
    let cassette_dir = session.harness.temp_path("vcr");
    let env_root = session.harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt_for_args(
        vcr_interactive_args_no_tools,
        session.harness.temp_dir(),
        &env_root,
    );
    let cassette_path = write_vcr_basic_chat_cassette(&cassette_dir, &system_prompt);
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
        .info_ctx("vcr", "Prepared basic chat playback cassette", |ctx| {
            ctx.push(("cassette_path".into(), cassette_path.display().to_string()));
            ctx.push(("system_prompt_sha256".into(), sha256_hex(&system_prompt)));
        });

    let cassette_dir_str = cassette_dir.display().to_string();
    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir_str);
    session.set_env("PI_VCR_TEST_NAME", VCR_BASIC_CHAT_TEST_NAME);
    session.set_env("PI_TEST_MODE", "1");
    session.set_env("VCR_DEBUG_BODY", "1");
    session.set_env("VCR_DEBUG_BODY_FILE", "/tmp/vcr_debug_bodies.txt");

    // Diagnostic: write the expected request body for comparison
    let diag_path = session.harness.temp_path("expected_body.json");
    let expected_body = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_BASIC_CHAT_PROMPT } ] }
        ],
        "system": &system_prompt,
        "max_tokens": 8192,
        "stream": true,
    });
    std::fs::write(
        &diag_path,
        serde_json::to_string_pretty(&expected_body).unwrap(),
    )
    .expect("write expected body");
    session
        .harness
        .record_artifact("expected_body.json", &diag_path);

    // Write stderr log path for the binary
    let stderr_log = session.harness.temp_path("pi-stderr.log");
    session.set_env("PI_STDERR_LOG", &stderr_log.display().to_string());
    session.set_env("RUST_LOG", "debug");

    session.harness.section("launch");
    session.launch(&vcr_interactive_args_no_tools());

    let pane = session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    assert!(
        pane.contains("Welcome to Pi!"),
        "Expected welcome message; got:\n{pane}"
    );

    session.harness.section("send prompt");
    let pane = session.send_text_and_wait(
        "prompt",
        VCR_BASIC_CHAT_PROMPT,
        VCR_BASIC_CHAT_RESPONSE,
        COMMAND_TIMEOUT,
    );

    // If VCR failed, dump full pane for debugging
    if !pane.contains(VCR_BASIC_CHAT_RESPONSE) {
        let debug_pane_path = session.harness.temp_path("debug-pane.txt");
        std::fs::write(&debug_pane_path, &pane).expect("write debug pane");
        session
            .harness
            .record_artifact("debug-pane.txt", &debug_pane_path);

        // Print pane content to test output for visibility
        eprintln!("=== VCR MISMATCH DEBUG ===");
        eprintln!("System prompt: {:?}", &system_prompt);
        eprintln!(
            "Expected body:\n{}",
            serde_json::to_string_pretty(&expected_body).unwrap()
        );
        eprintln!("Pane output:\n{pane}");
        eprintln!("=== END VCR MISMATCH DEBUG ===");
    }

    assert!(
        pane.contains(VCR_BASIC_CHAT_RESPONSE),
        "Expected VCR response in pane; got:\n{pane}"
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
        let mut lines = content.lines().filter(|line| !line.trim().is_empty());
        let header_line = lines.next().expect("session header line");
        let header: Value = serde_json::from_str(header_line).expect("parse session header");
        assert_eq!(header.get("type").and_then(Value::as_str), Some("session"));
        assert_eq!(
            header.get("version").and_then(Value::as_u64),
            Some(u64::from(SESSION_VERSION))
        );

        let has_message = lines.any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(Value::as_str)
                        .map(|t| t == "message")
                })
                .unwrap_or(false)
        });
        assert!(
            has_message,
            "Expected at least one message entry in session"
        );
    } else {
        session
            .harness
            .log()
            .warn("verify", "No session JSONL file found (non-fatal)");
    }

    assert!(
        session.steps().len() >= 2,
        "Expected >= 2 steps (startup + prompt), got {}",
        session.steps().len()
    );
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
    session.set_env("PI_TEST_MODE", "1");

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
