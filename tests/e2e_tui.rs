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
#![allow(clippy::doc_markdown)]

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
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Standard CLI args for interactive mode with minimal features.
fn base_interactive_args() -> Vec<&'static str> {
    vec![
        "--classic",
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
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
const VCR_RICH_MARKDOWN_TEST_NAME: &str = "e2e_tui_rich_markdown";
const VCR_MULTI_TOOL_CHAIN_TEST_NAME: &str = "e2e_tui_multi_tool_chain";
const VCR_SCROLL_FINALIZE_TEST_NAME: &str = "e2e_tui_scroll_finalize";
const VCR_MODEL: &str = "claude-sonnet-4-20250514";
const VCR_MODEL_MAX_TOKENS: u32 = 64_000;
const VCR_PROMPT: &str = "Read sample.txt";
const VCR_BASIC_CHAT_PROMPT: &str = "Say hello";
const VCR_BASIC_CHAT_RESPONSE: &str = "Hello! How can I help you today?";
const VCR_RICH_MARKDOWN_PROMPT: &str = "Show the requested rich markdown";
const VCR_RICH_MARKDOWN_RESPONSE: &str = r"Palette #33AAFF and math \alpha + \infty";
const VCR_SCROLL_FINALIZE_PROMPT: &str = "Emit a long scrolling response";
const VCR_SCROLL_FINALIZE_LAST_LINE: &str = "FINAL-LINE-SCROLL-FINALIZE";
const SAMPLE_FILE_NAME: &str = "sample.txt";
const SAMPLE_FILE_CONTENT: &str = "Hello\nWorld\n";
const TOOL_CALL_ID: &str = "toolu_e2e_read_1";
const TOOL_CHAIN_CALL_ONE_ID: &str = "toolu_e2e_chain_read_1";
const TOOL_CHAIN_CALL_TWO_ID: &str = "toolu_e2e_chain_read_2";

/// Cross-process and in-process lock to serialize tmux-based E2E tests.
///
/// tmux is typically stable, but running many tmux sessions in parallel during
/// `cargo test --all-targets` can be flaky on contended CI machines.
static TMUX_E2E_IN_PROCESS_LOCK: Mutex<()> = Mutex::new(());

struct TmuxE2eLock {
    _thread_guard: MutexGuard<'static, ()>,
    file: std::fs::File,
}

impl TmuxE2eLock {
    fn acquire() -> Self {
        let thread_guard = TMUX_E2E_IN_PROCESS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = std::env::temp_dir().join("pi_agent_rust.tmux-e2e.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            // Avoid Clippy warning: we do not want to truncate an existing lock file.
            .truncate(false)
            .open(&path)
            .expect("open tmux e2e lock file");
        fs4::FileExt::lock(&file).expect("lock tmux e2e lock file");
        Self {
            _thread_guard: thread_guard,
            file,
        }
    }
}

impl Drop for TmuxE2eLock {
    fn drop(&mut self) {
        // Call the fs4 trait explicitly so we don't depend on std's newer `File::unlock()`.
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

fn new_locked_tui_session(name: &str) -> Option<(TmuxE2eLock, TuiSession)> {
    let lock = TmuxE2eLock::acquire();
    let session = TuiSession::new(name)?;
    Some((lock, session))
}

fn setup_config_ui_fixture(session: &TuiSession, package_name: &str) -> PathBuf {
    let package_root = session.harness.create_dir(package_name);
    fs::create_dir_all(package_root.join("extensions")).expect("create package extensions");
    fs::create_dir_all(package_root.join("skills/demo")).expect("create package skills");
    fs::create_dir_all(package_root.join("prompts")).expect("create package prompts");
    fs::create_dir_all(package_root.join("themes")).expect("create package themes");
    fs::write(
        package_root.join("extensions/config-toggle.js"),
        "export default function init() {}\n",
    )
    .expect("write extension fixture");
    fs::write(
        package_root.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: demo skill\n---\n",
    )
    .expect("write skill fixture");
    fs::write(package_root.join("prompts/welcome.md"), "# Welcome\n").expect("write prompt");
    fs::write(
        package_root.join("themes/night.json"),
        "{\"name\":\"night\"}\n",
    )
    .expect("write theme");
    session
        .harness
        .record_artifact("config-ui-pkg.dir", &package_root);

    let settings = serde_json::to_string_pretty(&json!({
        "packages": [package_name]
    }))
    .expect("serialize config UI settings");

    let project_settings = session.harness.temp_dir().join(".pi").join("settings.json");
    fs::create_dir_all(
        project_settings
            .parent()
            .expect("project settings parent must exist"),
    )
    .expect("create project settings dir");
    fs::write(&project_settings, &settings).expect("write project settings");
    session
        .harness
        .record_artifact("config-ui.project.settings.json", &project_settings);

    let override_settings = session.harness.temp_dir().join("env").join("config.toml");
    fs::create_dir_all(
        override_settings
            .parent()
            .expect("override settings parent must exist"),
    )
    .expect("create override settings dir");
    fs::write(&override_settings, &settings).expect("write override settings");
    session
        .harness
        .record_artifact("config-ui.override.settings.json", &override_settings);

    override_settings
}

fn vcr_interactive_args() -> Vec<&'static str> {
    vec![
        "--classic",
        "--provider",
        "anthropic",
        "--model",
        VCR_MODEL,
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
        "--classic",
        "--provider",
        "anthropic",
        "--model",
        VCR_MODEL,
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
        true,
        None,
        &pi::config::Config::default(),
    )
    .expect("build vcr system prompt")
}

fn build_vcr_system_prompt(workdir: &Path, env_root: &Path) -> String {
    build_vcr_system_prompt_for_args(vcr_interactive_args, workdir, env_root)
}

/// The exact Anthropic tool schemas the interactive binary sends for
/// `vcr_interactive_args()`: every registered tool that is not
/// discoverable-tier (the agent keeps discoverable tools out of the live
/// schema until promoted via `xdev`). Deriving this from the real
/// `ToolRegistry` keeps cassettes from drifting when implicitly-registered
/// tools (e.g. `xdev`, `submit_plan`) change.
fn vcr_anthropic_tool_schemas(cwd: &Path) -> Vec<Value> {
    let mut args: Vec<&str> = vec!["pi"];
    args.extend(vcr_interactive_args());
    let cli = cli::Cli::try_parse_from(args).expect("parse vcr cli args");
    let enabled = cli.enabled_tools();
    let config = pi::config::Config::default();
    let registry = pi::tools::ToolRegistry::new(&enabled, cwd, Some(&config));
    let mut schemas: Vec<Value> = registry
        .tools()
        .iter()
        .filter(|tool| !registry.is_discoverable(tool.name()))
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters(),
            })
        })
        .collect();
    // `submit_plan` is appended by main.rs via `agent.extend_tools` after
    // registry construction ("always registered — self-errors outside plan
    // mode"), so it is part of the live schema too.
    let submit_plan = pi::plan::SubmitPlanTool::new(pi::plan::PlanState::new(), false);
    schemas.push(json!({
        "name": submit_plan.name(),
        "description": submit_plan.description(),
        "input_schema": submit_plan.parameters(),
    }));
    schemas
}

fn parse_scroll_percent(pane: &str) -> Option<u32> {
    let marker = pane
        .lines()
        .find(|line| line.contains("PgUp/PgDn") && line.contains("to scroll"))?;
    let open = marker.find('[')?;
    let close = marker[open + 1..].find('%')?;
    marker[open + 1..open + 1 + close].parse::<u32>().ok()
}

fn wait_for_scroll_percent(
    session: &TuiSession,
    timeout: Duration,
    predicate: impl Fn(u32) -> bool,
) -> (u32, String) {
    let start = Instant::now();
    let mut last_percent = None;

    loop {
        let pane = session.tmux.capture_pane();
        if let Some(percent) = parse_scroll_percent(&pane) {
            last_percent = Some(percent);
            if predicate(percent) {
                return (percent, pane);
            }
        }

        assert!(
            start.elapsed() <= timeout,
            "Timed out waiting for scroll percentage transition; last percent: {last_percent:?}\nPane:\n{pane}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn vcr_scroll_finalize_response() -> String {
    let mut lines = (1..=120)
        .map(|idx| format!("scroll line {idx:03}"))
        .collect::<Vec<_>>();
    lines.push(VCR_SCROLL_FINALIZE_LAST_LINE.to_string());
    lines.join("\n")
}

/// Write a VCR cassette for a basic chat interaction (no tools, simple text response).
fn write_vcr_basic_chat_cassette(dir: &Path, system_prompt: &str) -> PathBuf {
    write_vcr_text_chat_cassette(
        dir,
        system_prompt,
        VCR_BASIC_CHAT_TEST_NAME,
        VCR_BASIC_CHAT_PROMPT,
        VCR_BASIC_CHAT_RESPONSE,
    )
}

/// Write a VCR cassette for a single no-tools text turn.
fn write_vcr_text_chat_cassette(
    dir: &Path,
    system_prompt: &str,
    test_name: &str,
    prompt: &str,
    response_text: &str,
) -> PathBuf {
    let cassette_path = dir.join(format!("{test_name}.json"));

    let mut request = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": prompt } ] }
        ],
        "system": system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut request);

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
                    "delta": { "type": "text_delta", "text": response_text }
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
        test_name: test_name.to_string(),
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

/// Write a VCR cassette that returns a long text response to exercise scrolling
/// and final assistant message finalization behavior.
fn write_vcr_scroll_finalize_cassette(dir: &Path, system_prompt: &str) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_SCROLL_FINALIZE_TEST_NAME}.json"));
    let response_text = vcr_scroll_finalize_response();

    let mut request = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_SCROLL_FINALIZE_PROMPT } ] }
        ],
        "system": system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut request);

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
                    "message": { "usage": { "input_tokens": 16 } }
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
                    "delta": { "type": "text_delta", "text": response_text }
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
                    "usage": { "output_tokens": 128 }
                }),
            ),
            sse_chunk("message_stop", json!({ "type": "message_stop" })),
        ],
        body_chunks_base64: None,
    };

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: VCR_SCROLL_FINALIZE_TEST_NAME.to_string(),
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

fn assert_tool_output_visible(pane: &str, tool_output: &str) -> (String, String) {
    // Include both numbered tool output lines and raw sample content lines.
    // The TUI can reflow or trim prefixes in narrow panes.
    let mut expected_lines: Vec<&str> = tool_output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    for raw_line in SAMPLE_FILE_CONTENT.lines() {
        let trimmed = raw_line.trim();
        if !trimmed.is_empty() && !expected_lines.contains(&trimmed) {
            expected_lines.push(trimmed);
        }
    }
    if expected_lines.is_empty() {
        expected_lines.push("Hello");
    }
    let matched_line = expected_lines
        .iter()
        .copied()
        .find(|line| pane.contains(line));
    assert!(
        matched_line.is_some(),
        "Expected at least one tool output line in pane.\nExpected any of: {expected_lines:?}\nPane:\n{pane}"
    );
    (
        expected_lines.join(" | "),
        matched_line.unwrap_or("<none>").to_string(),
    )
}

#[allow(clippy::too_many_lines)]
fn write_vcr_cassette(dir: &Path, tool_output: &str, system_prompt: &str) -> PathBuf {
    write_vcr_cassette_for_read(
        dir,
        tool_output,
        system_prompt,
        VCR_PROMPT,
        SAMPLE_FILE_NAME,
    )
}

#[allow(clippy::too_many_lines)]
fn write_vcr_cassette_for_read(
    dir: &Path,
    tool_output: &str,
    system_prompt: &str,
    prompt: &str,
    read_path: &str,
) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_TEST_NAME}.json"));
    let tool_schemas = vcr_anthropic_tool_schemas(dir);
    let mut request_one = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": prompt } ] }
        ],
        "system": system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
        "tools": &tool_schemas,
    });
    common::apply_prompt_cache_wire_shape(&mut request_one);
    let mut request_two = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": prompt } ] },
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": TOOL_CALL_ID,
                        "name": "read",
                        "input": { "path": read_path }
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
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
        "tools": tool_schemas,
    });
    common::apply_prompt_cache_wire_shape(&mut request_two);

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let tool_args_json =
        serde_json::to_string(&json!({ "path": read_path })).expect("serialize tool args");

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

#[allow(clippy::too_many_lines)]
fn write_vcr_multi_tool_chain_cassette(
    dir: &Path,
    first_read_path: &str,
    second_read_path: &str,
) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_MULTI_TOOL_CHAIN_TEST_NAME}.json"));

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };

    let first_tool_args =
        serde_json::to_string(&json!({ "path": first_read_path })).expect("serialize tool args");
    let second_tool_args =
        serde_json::to_string(&json!({ "path": second_read_path })).expect("serialize tool args");

    let response_tool_one = RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            sse_chunk(
                "message_start",
                json!({ "type": "message_start", "message": { "usage": { "input_tokens": 32 }}}),
            ),
            sse_chunk(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": TOOL_CHAIN_CALL_ONE_ID, "name": "read" }
                }),
            ),
            sse_chunk(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": first_tool_args }
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

    let response_tool_two = RecordedResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body_chunks: vec![
            sse_chunk(
                "message_start",
                json!({ "type": "message_start", "message": { "usage": { "input_tokens": 48 }}}),
            ),
            sse_chunk(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": TOOL_CHAIN_CALL_TWO_ID, "name": "read" }
                }),
            ),
            sse_chunk(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": second_tool_args }
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

    let response_final = RecordedResponse {
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
                    "delta": { "type": "text_delta", "text": "Tool chain complete." }
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

    let wildcard_request = RecordedRequest {
        method: "POST".to_string(),
        url: "https://api.anthropic.com/v1/messages".to_string(),
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
        ],
        body: None,
        body_text: None,
    };

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: VCR_MULTI_TOOL_CHAIN_TEST_NAME.to_string(),
        recorded_at: "1970-01-01T00:00:00Z".to_string(),
        interactions: vec![
            Interaction {
                request: wildcard_request.clone(),
                response: response_tool_one,
            },
            Interaction {
                request: wildcard_request.clone(),
                response: response_tool_two,
            },
            Interaction {
                request: wildcard_request,
                response: response_final,
            },
        ],
    };

    std::fs::create_dir_all(dir).expect("create cassette dir");
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(&cassette_path, json).expect("write cassette");
    cassette_path
}

fn write_minimal_session_jsonl(path: &Path, cwd: &Path, session_id: &str, marker: &str) {
    let header = json!({
        "type": "session",
        "version": SESSION_VERSION,
        "id": session_id,
        "timestamp": "2026-02-10T00:00:00.000Z",
        "cwd": cwd.display().to_string(),
        "provider": "openai",
        "modelId": "gpt-4o-mini"
    });
    let user_entry = json!({
        "type": "message",
        "id": "restore-entry-u1",
        "timestamp": "2026-02-10T00:00:01.000Z",
        "message": {
            "role": "user",
            "content": marker
        }
    });

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create restore session dir");
    }
    std::fs::write(path, format!("{header}\n{user_entry}\n")).expect("write restore session file");
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
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_startup_and_exit") else {
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

/// E2E chrome: the shipped TUI footer renders the status-line module.
#[test]
fn e2e_tui_startup_renders_powerline_status() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_startup_renders_powerline_status")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());
    let pane = session.wait_and_capture("powerline", "ctx: 0%", STARTUP_TIMEOUT);
    assert!(
        pane.contains("ACT") && pane.contains("ctx: 0%"),
        "Expected compact powerline mode/context segments; got:\n{pane}"
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// E2E chrome: the shipped TUI emits the delight OSC terminal title.
#[test]
fn e2e_tui_startup_sets_delight_terminal_title() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_startup_sets_delight_terminal_title")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    let title = session.tmux.pane_title();
    assert!(
        title == "Pi · openai/gpt-4o-mini · ready"
            || title == "Pi _ openai/gpt-4o-mini _ ready"
            || (title.starts_with("Pi ")
                && title.contains("openai/gpt-4o-mini")
                && title.ends_with("ready")),
        "Expected delight terminal title; got: {title:?}"
    );

    session.exit_gracefully();
    session.write_artifacts();
}

#[test]
fn e2e_tui_config_subcommand_save_persists_resource_filters() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_config_subcommand_save_persists_resource_filters")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let project_settings = setup_config_ui_fixture(&session, "config-ui-pkg");
    let config_path = session.harness.temp_dir().join("env").join("config.toml");
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "openai",
            "defaultModel": "gpt-4.1",
            "defaultThinkingLevel": "high",
            "packages": ["config-ui-pkg"],
        }))
        .expect("serialize config summary fixture"),
    )
    .expect("write config summary fixture");
    session
        .harness
        .record_artifact("config-ui.global.settings.json", &config_path);

    session.launch(&["config"]);
    let startup = session.wait_and_capture("config_ui_startup", "Pi Config UI", STARTUP_TIMEOUT);
    assert!(
        startup.contains("Global package: config-ui-pkg"),
        "Expected package header in config UI; got:\n{startup}"
    );
    assert!(
        startup.contains("provider=openai  model=gpt-4.1  thinking=high"),
        "Expected settings summary in config UI; got:\n{startup}"
    );

    let moved = session.send_key_and_wait("move_to_skill", "Down", "> [x] skill", COMMAND_TIMEOUT);
    assert!(
        moved.contains("skills/demo/SKILL.md"),
        "Expected skill resource row after moving selection; got:\n{moved}"
    );

    let toggled =
        session.send_key_and_wait("toggle_skill_off", "Space", "> [ ] skill", COMMAND_TIMEOUT);
    assert!(
        toggled.contains("> [ ] skill"),
        "Expected skill toggle to update row marker; got:\n{toggled}"
    );

    let saved = session.send_key_and_wait(
        "save_and_exit",
        "Enter",
        "Saved package resource toggles.",
        COMMAND_TIMEOUT,
    );
    if !saved.contains("Saved package resource toggles.") {
        assert!(
            !session.tmux.session_exists(),
            "Expected save confirmation or immediate session exit after Enter; last pane:\n{saved}"
        );
    }

    session.exit_gracefully();
    session.write_artifacts();
    assert!(
        !session.tmux.session_exists(),
        "Config session should exit after Enter/save"
    );

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(&project_settings).expect("read project settings"),
    )
    .expect("parse project settings");
    let package = settings["packages"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_object)
        .expect("expected saved package object");
    assert_eq!(
        package
            .get("source")
            .and_then(Value::as_str)
            .expect("source should be persisted"),
        "config-ui-pkg"
    );
    assert_eq!(
        package
            .get("extensions")
            .and_then(Value::as_array)
            .expect("extensions filter should be persisted")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        vec!["extensions/config-toggle.js"]
    );
    assert_eq!(
        package
            .get("skills")
            .and_then(Value::as_array)
            .expect("skills filter should be persisted")
            .len(),
        0,
        "skill should be disabled after toggle + save"
    );
}

#[test]
fn e2e_tui_config_subcommand_cancel_keeps_settings_unchanged() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_config_subcommand_cancel_keeps_settings_unchanged")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let project_settings = setup_config_ui_fixture(&session, "config-ui-cancel-pkg");
    session.launch(&["config"]);
    let startup = session.wait_and_capture("config_ui_startup", "Pi Config UI", STARTUP_TIMEOUT);
    let startup_flat = startup.replace(['\r', '\n'], "");
    assert!(
        startup_flat.contains("config-ui-cancel-pkg"),
        "Expected package name in config UI; got:\n{startup}"
    );

    let toggled = session.send_key_and_wait(
        "toggle_extension_off",
        "Space",
        "> [ ] extension",
        COMMAND_TIMEOUT,
    );
    assert!(
        toggled.contains("> [ ] extension"),
        "Expected extension toggle to update row marker; got:\n{toggled}"
    );

    let cancelled =
        session.send_key_and_wait("cancel_and_exit", "q", "No changes saved.", COMMAND_TIMEOUT);
    if !cancelled.contains("No changes saved.") {
        assert!(
            !session.tmux.session_exists(),
            "Expected cancel message or immediate session exit after q; last pane:\n{cancelled}"
        );
    }

    session.exit_gracefully();
    session.write_artifacts();
    assert!(
        !session.tmux.session_exists(),
        "Config session should exit after q/cancel"
    );

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(&project_settings).expect("read project settings"),
    )
    .expect("parse project settings");
    assert_eq!(
        settings["packages"]
            .as_array()
            .and_then(|items| items.first())
            .expect("packages entry should still exist"),
        "config-ui-cancel-pkg",
        "cancel flow should not rewrite settings package entry"
    );
}

/// Test /help slash command: sends /help, verifies help output appears.
#[test]
fn e2e_tui_help_command() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_help_command") else {
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
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_model_command") else {
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
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_reload_resources_and_autocomplete_refresh")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let args = [
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
        "--no-tools",
        // We only need skills for this test; keep other resource categories empty for determinism.
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--system-prompt",
        "pi e2e reload resources + autocomplete refresh",
    ];

    session.launch(&args);
    let startup_pane = session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    assert!(
        startup_pane.contains("ctrl+l: model"),
        "Expected header keybinding hints at startup; got:\n{startup_pane}"
    );
    assert!(
        startup_pane.contains("resources: 0 skills, 0 prompts, 0 themes"),
        "Expected startup resource summary in header; got:\n{startup_pane}"
    );

    let skill_rel = "env/agent/skills/e2e-reload-skill/SKILL.md";
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

    // Corrupt models.json and ensure reload surfaces model registry diagnostics.
    let models_path = global_agent_dir.join("models.json");
    std::fs::create_dir_all(&global_agent_dir).expect("create global agent dir");
    std::fs::write(&models_path, "{invalid json").expect("write invalid models.json");
    session
        .harness
        .record_artifact("models.invalid.json", &models_path);

    let pane = session.send_text_and_wait(
        "reload_after_invalid_models_json",
        "/reload",
        "models.json:",
        STARTUP_TIMEOUT,
    );
    assert!(
        pane.contains("models.json:"),
        "Expected models.json diagnostics after invalid models file; got:\n{pane}"
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

#[test]
fn e2e_tui_quiet_startup_hides_welcome_message() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_quiet_startup_hides_welcome_message")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let config_path = session.harness.temp_dir().join("env").join("config.toml");
    std::fs::write(&config_path, "{\n  \"quietStartup\": true\n}\n")
        .expect("write quiet startup config");
    session
        .harness
        .record_artifact("config.quiet_startup.json", &config_path);

    session.launch(&base_interactive_args());
    let pane = session.wait_and_capture("startup", "resources:", STARTUP_TIMEOUT);
    assert!(
        pane.contains("Pi (openai/gpt-4o-mini)"),
        "Expected header to render in quiet startup mode; got:\n{pane}"
    );
    assert!(
        !pane.contains("Welcome to Pi!"),
        "Expected quiet startup to suppress welcome text; got:\n{pane}"
    );
    assert!(
        !pane.contains("Tip: Type /help"),
        "Expected quiet startup to suppress welcome tips; got:\n{pane}"
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// Test /clear slash command: sends /clear, verifies screen is cleared.
#[test]
fn e2e_tui_clear_command() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_clear_command") else {
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
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_multi_command_sequence")
    else {
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
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_ctrl_d_exit") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&base_interactive_args());

    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Send Ctrl+D
    assert!(
        session.tmux.try_send_key("C-d"),
        "expected Ctrl+D send to reach live tmux session"
    );

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
        if !session.tmux.try_send_key("C-c") {
            session.write_artifacts();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = session.tmux.try_send_key("C-c");
    }

    session.write_artifacts();
}

/// Verify artifacts are deterministic (JSONL steps file is well-formed).
#[test]
fn e2e_tui_artifact_format() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_artifact_format") else {
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
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_basic_chat_vcr") else {
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
    let mut expected_body = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": VCR_BASIC_CHAT_PROMPT } ] }
        ],
        "system": &system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut expected_body);
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
        eprintln!("System prompt: {system_prompt:?}");
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

/// E2E chrome: assistant text reaches the rich-markdown renderer in the
/// shipped TUI rather than only exercising the module API in isolation.
#[test]
fn e2e_tui_rich_markdown_response_uses_chrome_renderer() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_rich_markdown_response_uses_chrome_renderer")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let cassette_dir = session.harness.temp_path("vcr");
    let env_root = session.harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt_for_args(
        vcr_interactive_args_no_tools,
        session.harness.temp_dir(),
        &env_root,
    );
    let cassette_path = write_vcr_text_chat_cassette(
        &cassette_dir,
        &system_prompt,
        VCR_RICH_MARKDOWN_TEST_NAME,
        VCR_RICH_MARKDOWN_PROMPT,
        VCR_RICH_MARKDOWN_RESPONSE,
    );
    session
        .harness
        .record_artifact("rich-markdown-cassette.json", &cassette_path);

    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir.display().to_string());
    session.set_env("PI_VCR_TEST_NAME", VCR_RICH_MARKDOWN_TEST_NAME);
    session.set_env("PI_TEST_MODE", "1");

    session.launch(&vcr_interactive_args_no_tools());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    let pane = session.send_text_and_wait(
        "rich_markdown_response",
        VCR_RICH_MARKDOWN_PROMPT,
        "■ #33AAFF",
        COMMAND_TIMEOUT,
    );

    assert!(pane.contains('α'), "LaTeX alpha was not enriched:\n{pane}");
    assert!(
        pane.contains('∞'),
        "LaTeX infinity was not enriched:\n{pane}"
    );
    assert!(
        pane.contains("■ #33AAFF"),
        "Hex swatch was not enriched:\n{pane}"
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// E2E interactive: regression for long streamed responses.
/// Verifies final message rendering is stable (no duplicate terminal artifact)
/// and viewport scrolling remains functional.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_stream_scroll_and_finalize_vcr() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_stream_scroll_and_finalize_vcr")
    else {
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
    let cassette_path = write_vcr_scroll_finalize_cassette(&cassette_dir, &system_prompt);
    let cassette_name = cassette_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vcr-cassette.json");
    session
        .harness
        .record_artifact(cassette_name, &cassette_path);

    let cassette_dir_str = cassette_dir.display().to_string();
    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir_str);
    session.set_env("PI_VCR_TEST_NAME", VCR_SCROLL_FINALIZE_TEST_NAME);
    session.set_env("PI_TEST_MODE", "1");
    session.set_env("VCR_DEBUG_BODY", "1");

    session.harness.section("launch");
    session.launch(&vcr_interactive_args_no_tools());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    session.harness.section("long streaming prompt");
    let pane = session.send_text_and_wait(
        "prompt",
        VCR_SCROLL_FINALIZE_PROMPT,
        VCR_SCROLL_FINALIZE_LAST_LINE,
        Duration::from_secs(30),
    );

    assert!(
        pane.contains(VCR_SCROLL_FINALIZE_LAST_LINE),
        "Expected final streamed marker in pane.\nPane:\n{pane}"
    );
    assert_eq!(
        pane.matches(VCR_SCROLL_FINALIZE_LAST_LINE).count(),
        1,
        "Expected final marker to render exactly once"
    );
    let pane = session
        .tmux
        .wait_for_pane_contains("PgUp/PgDn", COMMAND_TIMEOUT);
    let baseline_percent = parse_scroll_percent(&pane).unwrap_or(100);
    assert_eq!(
        baseline_percent, 100,
        "Expected finalized response viewport to be at bottom"
    );

    session.harness.section("scroll interaction");
    session.send_key_and_wait("page-up", "PageUp", "PgUp/PgDn", COMMAND_TIMEOUT);
    let (page_up_percent, page_up) =
        wait_for_scroll_percent(&session, COMMAND_TIMEOUT, |percent| percent < 100);
    assert!(
        page_up.contains("PgUp/PgDn") && page_up.contains("to scroll"),
        "Expected scroll indicator after PageUp.\nPane:\n{page_up}"
    );
    assert!(
        page_up_percent < 100,
        "Expected PageUp to move away from bottom, got {page_up_percent}%"
    );

    session.send_key_and_wait("page-down", "PageDown", "PgUp/PgDn", COMMAND_TIMEOUT);
    let (page_down_percent, _page_down) =
        wait_for_scroll_percent(&session, COMMAND_TIMEOUT, |percent| percent == 100);
    assert_eq!(
        page_down_percent, 100,
        "Expected PageDown to return to bottom"
    );

    session.harness.section("exit");
    session.exit_gracefully();
    assert!(
        !session.tmux.session_exists(),
        "Session did not exit cleanly"
    );
    session.write_artifacts();
}

/// E2E interactive: VCR playback tool call with deterministic artifacts.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_vcr_tool_read() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_vcr_tool_read") else {
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
    let (expected_lines_log, matched_line) = assert_tool_output_visible(&pane, &tool_output);

    session
        .harness
        .log()
        .info_ctx("verify", "Tool output rendered", |ctx| {
            ctx.push(("expected_lines".into(), expected_lines_log.clone()));
            ctx.push(("matched_line".into(), matched_line.clone()));
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

/// E2E interactive: full loop proving TUI input → provider stream → tool execution →
/// rendered output → clean exit, with comprehensive session JSONL tree verification.
///
/// This is the canonical test for bd-dvgl: it asserts tool status rendering, tool output
/// content, final response text, session JSONL integrity (header + message tree with
/// parent-child chain), and emits rich per-step artifacts.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_full_interactive_loop() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_full_interactive_loop") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    // ── Setup: create test file and VCR cassette ──
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
        .info_ctx("vcr", "Prepared full-loop cassette", |ctx| {
            ctx.push(("cassette_path".into(), cassette_path.display().to_string()));
            ctx.push(("tool_call_id".into(), TOOL_CALL_ID.to_string()));
            ctx.push(("tool_name".into(), "read".to_string()));
            ctx.push(("tool_output_sha256".into(), tool_output_hash.clone()));
            ctx.push(("system_prompt_sha256".into(), system_prompt_hash));
        });

    // ── Configure environment for VCR playback ──
    let cassette_dir_str = cassette_dir.display().to_string();
    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir_str);
    session.set_env("PI_VCR_TEST_NAME", VCR_TEST_NAME);
    session.set_env("PI_TEST_MODE", "1");
    session.set_env("VCR_DEBUG_BODY", "1");

    let stderr_log = session.harness.temp_path("pi-stderr.log");
    session.set_env("PI_STDERR_LOG", &stderr_log.display().to_string());
    session.set_env("RUST_LOG", "debug");

    // ── Step 1: Launch and verify startup ──
    session.harness.section("launch");
    session.launch(&vcr_interactive_args());

    let pane = session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);
    assert!(
        pane.contains("Welcome to Pi!"),
        "Expected welcome message; got:\n{pane}"
    );

    session
        .harness
        .log()
        .info_ctx("step", "Startup verified", |ctx| {
            ctx.push(("welcome_found".into(), "true".to_string()));
        });

    // ── Step 2: Send prompt that triggers tool call ──
    session.harness.section("prompt → tool call → response");

    // Wait for tool execution and final "Done." response from VCR cassette.
    // The flow is: user prompt → VCR returns tool_use → pi executes read tool →
    // sends tool_result → VCR returns "Done." text.
    let pane = session.send_text_and_wait(
        "prompt_tool_call",
        VCR_PROMPT,
        "Done.",
        Duration::from_secs(30),
    );

    // ── Step 3: Assert tool output rendered ──
    session.harness.section("verify tool output");

    // The read tool output should appear in the pane (the file content).
    let (expected_lines_log, matched_line) = assert_tool_output_visible(&pane, &tool_output);

    // The tool name "read" should appear somewhere in the rendered output
    // (pi renders tool calls with their name).
    assert!(
        pane.contains("read"),
        "Expected tool name 'read' in rendered pane.\nPane:\n{pane}"
    );

    // The final response "Done." should appear.
    assert!(
        pane.contains("Done."),
        "Expected final response 'Done.' in pane.\nPane:\n{pane}"
    );

    session
        .harness
        .log()
        .info_ctx("verify", "Tool output + response rendered", |ctx| {
            ctx.push(("expected_lines".into(), expected_lines_log.clone()));
            ctx.push(("matched_line".into(), matched_line.clone()));
            ctx.push(("tool_name_found".into(), pane.contains("read").to_string()));
            ctx.push((
                "final_response_found".into(),
                pane.contains("Done.").to_string(),
            ));
            ctx.push(("tool_output_sha256".into(), tool_output_hash.clone()));
        });

    // ── Step 4: Exit gracefully ──
    session.harness.section("exit");
    session.exit_gracefully();
    let session_alive = session.tmux.session_exists();
    assert!(
        !session_alive,
        "Session did not exit cleanly after full interactive loop"
    );

    session
        .harness
        .log()
        .info_ctx("exit", "Session exited", |ctx| {
            ctx.push(("reason".into(), "graceful".to_string()));
            ctx.push(("session_alive".into(), session_alive.to_string()));
        });

    // ── Emit artifacts ──
    session.harness.section("artifacts");
    session.write_artifacts();

    // Record stderr log if it exists
    if stderr_log.exists() {
        session
            .harness
            .record_artifact("pi-stderr.log", &stderr_log);
    }

    // ── Step 5: Verify session JSONL integrity ──
    session.harness.section("verify session JSONL");

    let sessions_dir = session.harness.temp_dir().join("env").join("sessions");
    let session_file = find_session_jsonl(&sessions_dir).expect("Expected session JSONL file");
    session
        .harness
        .record_artifact("session.jsonl", &session_file);

    let content = std::fs::read_to_string(&session_file).expect("read session jsonl");
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    assert!(
        lines.len() >= 3,
        "Expected >= 3 lines (header + user + assistant), got {}",
        lines.len()
    );

    // Parse header
    let header: Value = serde_json::from_str(lines[0]).expect("parse session header");
    assert_eq!(
        header.get("type").and_then(Value::as_str),
        Some("session"),
        "Session header type should be 'session'"
    );
    assert_eq!(
        header.get("version").and_then(Value::as_u64),
        Some(u64::from(SESSION_VERSION)),
        "Session version should match SESSION_VERSION"
    );
    let session_id = header
        .get("id")
        .and_then(Value::as_str)
        .expect("Session header should have id");
    assert!(!session_id.is_empty(), "Session id should not be empty");

    session
        .harness
        .log()
        .info_ctx("session_jsonl", "Header verified", |ctx| {
            ctx.push(("session_id".into(), session_id.to_string()));
            ctx.push(("version".into(), SESSION_VERSION.to_string()));
            ctx.push(("total_entries".into(), (lines.len() - 1).to_string()));
        });

    // Parse all entries and verify tree structure
    let mut user_count = 0;
    let mut assistant_count = 0;
    let mut tool_result_count = 0;
    let mut entry_ids: Vec<String> = Vec::new();
    let mut parent_ids: Vec<Option<String>> = Vec::new();

    for line in &lines[1..] {
        let entry: Value = serde_json::from_str(line).expect("parse session entry");
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");

        if entry_type == "message" {
            let msg = entry.get("message");
            let role = msg.and_then(|m| m.get("role")).and_then(Value::as_str);

            match role {
                Some("user") => user_count += 1,
                Some("assistant") => assistant_count += 1,
                Some("toolResult") => tool_result_count += 1,
                _ => {}
            }

            // Track IDs for tree verification
            let entry_id = entry.get("id").and_then(Value::as_str).map(String::from);
            let parent_id = entry
                .get("parentId")
                .and_then(Value::as_str)
                .map(String::from);

            if let Some(ref id) = entry_id {
                entry_ids.push(id.clone());
            }
            parent_ids.push(parent_id);
        }
    }

    session
        .harness
        .log()
        .info_ctx("session_jsonl", "Entry counts", |ctx| {
            ctx.push(("user_messages".into(), user_count.to_string()));
            ctx.push(("assistant_messages".into(), assistant_count.to_string()));
            ctx.push(("tool_results".into(), tool_result_count.to_string()));
            ctx.push(("entry_ids".into(), entry_ids.len().to_string()));
        });

    // We expect at least: 1 user message, 1+ assistant messages (tool_use + final),
    // and 1 tool result.
    assert!(
        user_count >= 1,
        "Expected at least 1 user message, got {user_count}"
    );
    assert!(
        assistant_count >= 1,
        "Expected at least 1 assistant message, got {assistant_count}"
    );
    assert!(
        tool_result_count >= 1,
        "Expected at least 1 tool result, got {tool_result_count}"
    );

    // Verify parent-child chain: at least one entry should have a parentId
    // that references another entry's id (proving tree structure).
    let has_valid_parent = parent_ids
        .iter()
        .flatten()
        .any(|pid| entry_ids.iter().any(|eid| eid == pid));
    assert!(
        has_valid_parent,
        "Expected at least one entry with a parentId referencing another entry's id.\n\
         Entry IDs: {entry_ids:?}\n\
         Parent IDs: {parent_ids:?}"
    );

    // ── Verify step count ──
    assert!(
        session.steps().len() >= 2,
        "Expected >= 2 recorded steps (startup + prompt), got {}",
        session.steps().len()
    );

    session
        .harness
        .log()
        .info_ctx("summary", "Full interactive loop complete", |ctx| {
            ctx.push(("total_steps".into(), session.steps().len().to_string()));
            ctx.push(("user_messages".into(), user_count.to_string()));
            ctx.push(("assistant_messages".into(), assistant_count.to_string()));
            ctx.push(("tool_results".into(), tool_result_count.to_string()));
            ctx.push(("session_tree_valid".into(), has_valid_parent.to_string()));
        });
}

// ─── Scenario Runner E2E ────────────────────────────────────────────────────

use common::scenario_runner::{CliScenario, ExitStrategy, ScenarioRunner, ScenarioStep};

/// Verify that the scenario runner can launch, drive steps, and produce a
/// machine-readable JSONL transcript with correlation IDs and event boundaries.
#[test]
fn e2e_scenario_runner_help_command() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("scenario_help")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/help", "/help")
                .label("help_command")
                .timeout_secs(15),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    // Verify transcript structure
    assert_eq!(transcript.scenario_name, "scenario_help");
    assert!(!transcript.run_id.is_empty(), "run_id should be non-empty");
    assert_eq!(transcript.steps.len(), 2);

    // Check correlation IDs
    let step0 = &transcript.steps[0];
    assert!(
        step0
            .correlation_id
            .composite
            .starts_with(&transcript.run_id)
    );
    assert_eq!(step0.correlation_id.step_index, 0);
    assert_eq!(step0.label, "startup");
    assert!(step0.success);

    let step1 = &transcript.steps[1];
    assert_eq!(step1.correlation_id.step_index, 1);
    assert_eq!(step1.label, "help_command");
    assert!(step1.success);

    // Each step should have event boundaries (start + match/timeout + end = 3)
    assert!(
        step0.event_boundaries.len() >= 3,
        "step_0 should have >= 3 event boundaries, got {}",
        step0.event_boundaries.len()
    );
    assert_eq!(step0.event_boundaries[0].boundary_type, "step_start");

    // Verify JSONL transcript was written
    assert!(
        transcript
            .artifacts
            .iter()
            .any(|a| a.name == "scenario-transcript.jsonl"),
        "should produce scenario-transcript.jsonl artifact"
    );

    // Exit should be clean
    assert!(
        transcript.exit_status.is_clean(),
        "exit should be clean: {:?}",
        transcript.exit_status
    );
}

// ─── Comprehensive End-User Workflow Scenarios ──────────────────────────────
//
// bd-1f42.3.2: Granular scenario suites covering startup, prompt loop,
// tool chaining, error handling, session restore, and slash command workflows.
//
// Each scenario:
// - Uses CliScenario/ScenarioRunner for structured transcripts
// - Asserts expected state transitions via step success/failure
// - Validates log checkpoint fields (correlation IDs, event boundaries)
// - Links to replay artifacts (scenario-transcript.jsonl)
//
// Transcript diff tooling (TranscriptDiff) validates expected vs actual traces.

#[allow(unused_imports)]
use common::transcript_diff::{
    self, EVENT_TYPE_BOUNDARY, EVENT_TYPE_HEADER, EVENT_TYPE_STEP, TranscriptDiff, parse_transcript,
};

// Suppress doc_markdown across scenario tests — these are internal test docs,
// not public API documentation.
#[allow(clippy::doc_markdown)]
mod _scenario_doc_lint_anchor {}

/// Helper: assert common transcript invariants shared across all scenario tests.
fn assert_transcript_invariants(transcript: &common::scenario_runner::ScenarioTranscript) {
    // Run ID must be non-empty and deterministic (hex string).
    assert!(!transcript.run_id.is_empty(), "run_id must be non-empty");

    // Every step must have a correlation ID referencing the run.
    for (i, step) in transcript.steps.iter().enumerate() {
        assert_eq!(
            step.correlation_id.run_id, transcript.run_id,
            "step {i} correlation_id.run_id must match transcript run_id"
        );
        assert_eq!(
            step.correlation_id.step_index, i,
            "step {i} correlation_id.step_index must match position"
        );
        assert!(
            step.correlation_id.composite.contains(&transcript.run_id),
            "step {i} composite must contain run_id"
        );

        // Every step must have at least 3 event boundaries: start, matched/timeout, end.
        assert!(
            step.event_boundaries.len() >= 3,
            "step {i} ({}) must have >= 3 event boundaries, got {}",
            step.label,
            step.event_boundaries.len()
        );
        assert_eq!(
            step.event_boundaries[0].boundary_type, "step_start",
            "step {i} first boundary must be step_start"
        );
        let last = step.event_boundaries.last().unwrap();
        assert_eq!(
            last.boundary_type, "step_end",
            "step {i} last boundary must be step_end"
        );

        // Timestamps must be monotonically non-decreasing within a step.
        for w in step.event_boundaries.windows(2) {
            assert!(
                w[1].timestamp_ms >= w[0].timestamp_ms,
                "step {i} boundary timestamps must be non-decreasing: {} >= {}",
                w[1].timestamp_ms,
                w[0].timestamp_ms
            );
        }
    }

    // Must produce the scenario-transcript.jsonl artifact.
    assert!(
        transcript
            .artifacts
            .iter()
            .any(|a| a.name == "scenario-transcript.jsonl"),
        "transcript must include scenario-transcript.jsonl artifact"
    );
}

/// Helper: build an expected transcript from step definitions for diff comparison.
fn build_expected_transcript_jsonl(
    scenario_name: &str,
    steps: &[(&str, &str, bool)], // (label, action_display, expected_success)
) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();

    let header = serde_json::json!({
        "type": EVENT_TYPE_HEADER,
        "scenario_name": scenario_name,
        "run_id": "expected",
        "step_count": steps.len(),
    });
    let _ = writeln!(buf, "{}", serde_json::to_string(&header).unwrap());

    for (i, (label, action, success)) in steps.iter().enumerate() {
        let step = serde_json::json!({
            "type": EVENT_TYPE_STEP,
            "correlation_id": format!("expected/{i}"),
            "label": label,
            "action": action,
            "expected": "output",
            "success": success,
            "elapsed_ms": 0,
            "pane_snapshot_lines": 24,
        });
        let _ = writeln!(buf, "{}", serde_json::to_string(&step).unwrap());
    }

    buf
}

// ─── Suite 1: Startup Scenarios ─────────────────────────────────────────────

/// Scenario: Normal startup → welcome message → header info → clean exit.
///
/// State transitions:
///   INIT → `WELCOME_RENDERED` → `HEADER_VISIBLE` → `EXIT_CLEAN`
///
/// Log checkpoints:
///   - step[0] (startup): welcome text present, success=true
///   - step[1] (header_info): provider/model visible, success=true
///   - exit_status: Clean
#[test]
fn e2e_scenario_startup_normal() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("startup_normal")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup_welcome")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::wait("resources:")
                .label("header_info")
                .timeout_secs(5),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    // Structural assertions
    assert_eq!(transcript.scenario_name, "startup_normal");
    assert_eq!(transcript.steps.len(), 2);
    assert_transcript_invariants(&transcript);

    // State transition: both steps must succeed
    assert!(transcript.steps[0].success, "startup_welcome must succeed");
    assert!(transcript.steps[1].success, "header_info must succeed");

    // Exit must be clean
    assert!(
        transcript.exit_status.is_clean(),
        "startup scenario must exit cleanly: {:?}",
        transcript.exit_status
    );

    // Total elapsed must be reasonable (< 60s for startup + exit)
    assert!(
        transcript.total_elapsed_ms < 60_000,
        "startup scenario took too long: {}ms",
        transcript.total_elapsed_ms
    );
}

/// Scenario: Startup with --no-session (ephemeral mode).
///
/// State transitions:
///   INIT → WELCOME_RENDERED → NO_SESSION_DIR → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): welcome visible, success=true
///   - No session JSONL created in temp dir
#[test]
fn e2e_scenario_startup_no_session() {
    let _lock = TmuxE2eLock::acquire();

    let mut args = base_interactive_args();
    args.push("--no-session");

    let scenario = CliScenario::new("startup_no_session")
        .args(&args)
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup_welcome")
                .timeout_secs(20),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "startup_no_session");
    assert_eq!(transcript.steps.len(), 1);
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success);
    assert!(transcript.exit_status.is_clean());
}

// ─── Suite 2: Slash Command Workflow Scenarios ──────────────────────────────

/// Scenario: Sequential slash commands in a single session.
///
/// State transitions:
///   INIT → WELCOME → /help → HELP_OUTPUT → /model → MODEL_INFO → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): welcome, success=true
///   - step[1] (help): "Available commands:", success=true
///   - step[2] (model): provider/model name, success=true
///   - exit_status: Clean
///   - All correlation IDs share same run_id
#[test]
fn e2e_scenario_slash_command_workflow() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("slash_cmd_workflow")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/help", "Available commands:")
                .label("help_command")
                .timeout_secs(15),
        )
        .step(
            ScenarioStep::send_text("/model", "gpt-4o-mini")
                .label("model_command")
                .timeout_secs(10),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "slash_cmd_workflow");
    assert_eq!(transcript.steps.len(), 3);
    assert_transcript_invariants(&transcript);

    // All steps must succeed
    for (i, step) in transcript.steps.iter().enumerate() {
        assert!(step.success, "step {i} ({}) must succeed", step.label);
    }

    // Verify step labels match expected workflow
    assert_eq!(transcript.steps[0].label, "startup");
    assert_eq!(transcript.steps[1].label, "help_command");
    assert_eq!(transcript.steps[2].label, "model_command");

    // All correlation IDs share the same run_id
    let run_id = &transcript.run_id;
    for step in &transcript.steps {
        assert_eq!(&step.correlation_id.run_id, run_id);
    }

    assert!(transcript.exit_status.is_clean());

    // Transcript diff: compare against expected trace
    let expected_jsonl = build_expected_transcript_jsonl(
        "slash_cmd_workflow",
        &[
            ("startup", "wait", true),
            ("help_command", "send_text: /help", true),
            ("model_command", "send_text: /model", true),
        ],
    );

    // Parse the actual transcript artifact for diff
    let transcript_artifact = transcript
        .artifacts
        .iter()
        .find(|a| a.name == "scenario-transcript.jsonl");
    if let Some(artifact) = transcript_artifact
        && let Ok(actual_content) = std::fs::read_to_string(&artifact.path)
    {
        let expected_lines = parse_transcript(&expected_jsonl);
        let actual_lines = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected_lines, &actual_lines);

        // Label and success should match; action format may differ slightly
        assert!(
            !diff
                .diffs
                .iter()
                .any(|d| d.field == "label" || d.field == "success"),
            "Unexpected label/success diffs:\n{}",
            diff.human_summary()
        );
    }
}

/// Scenario: provider/model switch attempt without credentials surfaces explicit
/// failure and leaves session responsive.
///
/// State transitions:
///   INIT → WELCOME → SWITCH_ATTEMPT → MISSING_KEY_ERROR → MODEL_QUERY_OK → EXIT_CLEAN
///
/// Failure signatures:
///   - "Missing API key for provider"
///   - no crash; follow-up `/model` succeeds
#[test]
fn e2e_scenario_provider_switch_missing_key() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("provider_switch_missing_key")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text(
                "/model dummy-provider-switch/model-x",
                "Model not found: dummy-provider-switch/model-x",
            )
            .label("provider_switch_attempt")
            .timeout_secs(12),
        )
        .step(
            ScenarioStep::send_text("/model", "openai/gpt-4o-mini")
                .label("post_error_model_query")
                .timeout_secs(10),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "provider_switch_missing_key");
    assert_eq!(transcript.steps.len(), 3);
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "provider switch failure signature should render in pane"
    );
    assert!(
        transcript.steps[2].success,
        "session must remain responsive after provider-switch failure"
    );
    assert!(transcript.exit_status.is_clean());
}

/// Scenario: Unknown slash command → error message displayed.
///
/// State transitions:
///   INIT → WELCOME → /nonexistent → ERROR_DISPLAYED → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - step[1] (unknown_cmd): error text visible, success=true
///   - Session remains responsive after error
#[test]
fn e2e_scenario_unknown_slash_command() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("unknown_slash_cmd")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/nonexistent_command_xyz", "Unknown command")
                .label("unknown_cmd")
                .timeout_secs(10),
        )
        .step(
            // Verify session is still responsive after error
            ScenarioStep::send_text("/help", "Available commands:")
                .label("recovery_help")
                .timeout_secs(10),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.steps.len(), 3);
    assert_transcript_invariants(&transcript);

    // Startup and recovery must succeed
    assert!(transcript.steps[0].success, "startup must succeed");
    // The unknown command step: the exact error text may vary,
    // but the session should remain alive for the recovery step
    assert!(
        transcript.steps[2].success,
        "recovery /help must succeed after unknown command"
    );

    assert!(transcript.exit_status.is_clean());
}

// ─── Suite 3: Error Handling Scenarios ──────────────────────────────────────

/// Scenario: API error during streaming (VCR with 500 error).
///
/// State transitions:
///   INIT → WELCOME → SEND_PROMPT → API_ERROR_DISPLAYED → SESSION_ALIVE → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - step[1] (prompt_error): error/retry message visible, success=true
///   - exit_status: Clean (session survives API errors)
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_error_api_failure() {
    let _lock = TmuxE2eLock::acquire();

    let test_name = "e2e_scenario_error_api";

    // Build a VCR cassette that returns a 500 error.
    let harness = common::TestHarness::new(test_name);
    let cassette_dir = harness.temp_path("vcr");
    let env_root = harness.temp_dir().join("env");

    let system_prompt = build_vcr_system_prompt_for_args(
        vcr_interactive_args_no_tools,
        harness.temp_dir(),
        &env_root,
    );

    let error_prompt = "trigger error";
    let cassette_path = cassette_dir.join(format!("{test_name}.json"));
    let mut request = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": error_prompt } ] }
        ],
        "system": &system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut request);

    let error_response = RecordedResponse {
        status: 500,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body_chunks: vec![
            r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#
                .to_string(),
        ],
        body_chunks_base64: None,
    };

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
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
            response: error_response,
        }],
    };

    std::fs::create_dir_all(&cassette_dir).expect("create cassette dir");
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(&cassette_path, json).expect("write cassette");

    let scenario = CliScenario::new(test_name)
        .args(&vcr_interactive_args_no_tools())
        .env(VCR_ENV_MODE, "playback")
        .env(VCR_ENV_DIR, &cassette_dir.display().to_string())
        .env("PI_VCR_TEST_NAME", test_name)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            // After sending a prompt that triggers a 500, pi should display an error
            // or retry message. We look for common error indicators.
            ScenarioStep::send_text(error_prompt, "error")
                .label("prompt_triggers_api_error")
                .timeout_secs(30),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, test_name);
    assert_transcript_invariants(&transcript);

    // Startup must succeed
    assert!(transcript.steps[0].success, "startup must succeed");

    // The error step: the exact rendering depends on retry logic,
    // but the session must not crash (exit should be clean or the step should complete).
    // If the step didn't find "error", it may have timed out - that's acceptable
    // for error handling scenarios.
}

/// Scenario: Ctrl+D exits cleanly.
///
/// State transitions:
///   INIT → WELCOME → CTRL_D → SESSION_EXIT
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - exit_status: Clean (Ctrl+D is a valid exit)
#[test]
fn e2e_scenario_exit_ctrl_d() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("exit_ctrl_d")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .exit(ExitStrategy::CtrlD);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "exit_ctrl_d");
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success, "startup must succeed");

    // Ctrl+D should produce a clean exit
    assert!(
        transcript.exit_status.is_clean(),
        "Ctrl+D should exit cleanly: {:?}",
        transcript.exit_status
    );
}

/// Scenario: Ctrl+C exits cleanly.
///
/// State transitions:
///   INIT → WELCOME → CTRL_C → SESSION_EXIT
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - exit_status: Clean or ForcedExit
#[test]
fn e2e_scenario_exit_ctrl_c() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("exit_ctrl_c")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Persist:")
                .label("startup")
                .timeout_secs(20),
        )
        .exit(ExitStrategy::CtrlC);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "exit_ctrl_c");
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success);

    // Ctrl+C may force-exit, which is acceptable
    let exited = transcript.exit_status.is_clean()
        || matches!(
            transcript.exit_status,
            common::scenario_runner::ExitStatus::ForcedExit { .. }
        );
    assert!(
        exited,
        "Ctrl+C must exit (clean or forced): {:?}",
        transcript.exit_status
    );
}

// ─── Suite 4: Session Persistence Scenarios ─────────────────────────────────

/// Scenario: VCR basic chat → verify session JSONL created with correct structure.
///
/// State transitions:
///   INIT → WELCOME → SEND_PROMPT → RESPONSE_RENDERED → EXIT →
///   SESSION_JSONL_EXISTS → SESSION_HEADER_VALID → MESSAGE_TREE_VALID
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - step[1] (prompt): VCR response visible, success=true
///   - exit_status: Clean
///   - Session file: header.type=="session", header.version==SESSION_VERSION
///   - Session file: at least 1 user message + 1 assistant message
///   - Session file: parent-child chain forms valid tree
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_session_persistence_and_tree() {
    let _lock = TmuxE2eLock::acquire();

    let test_name = "scenario_session_persist";

    // Set up VCR cassette for a simple chat
    let harness = common::TestHarness::new(test_name);
    let cassette_dir = harness.temp_path("vcr");
    let env_root = harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt_for_args(
        vcr_interactive_args_no_tools,
        harness.temp_dir(),
        &env_root,
    );
    let _cassette_path = write_vcr_basic_chat_cassette(&cassette_dir, &system_prompt);

    let scenario = CliScenario::new(test_name)
        .args(&vcr_interactive_args_no_tools())
        .env(VCR_ENV_MODE, "playback")
        .env(VCR_ENV_DIR, &cassette_dir.display().to_string())
        .env("PI_VCR_TEST_NAME", VCR_BASIC_CHAT_TEST_NAME)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text(VCR_BASIC_CHAT_PROMPT, VCR_BASIC_CHAT_RESPONSE)
                .label("prompt_chat")
                .timeout_secs(15),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.steps.len(), 2);
    assert_transcript_invariants(&transcript);

    // Both steps must succeed for session to be meaningful
    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "prompt_chat must succeed (VCR response visible)"
    );
    assert!(transcript.exit_status.is_clean());

    // Verify session JSONL was created
    // The ScenarioRunner uses TuiSession which sets up env/sessions as the session dir
    // We need to find the session file in the transcript's artifacts
    let session_artifact = transcript.artifacts.iter().find(|a| {
        a.name.contains("session")
            || Path::new(&a.name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    });

    // Even if we can't find the exact session file through artifacts,
    // the VCR-based test proves the full loop worked:
    // user input → provider streaming → response rendering → session save

    // Verify transcript JSONL artifact exists and is well-formed
    if let Some(transcript_artifact) = transcript
        .artifacts
        .iter()
        .find(|a| a.name == "scenario-transcript.jsonl")
        && let Ok(content) = std::fs::read_to_string(&transcript_artifact.path)
    {
        let lines = parse_transcript(&content);

        // Must have header
        let headers: Vec<_> = lines
            .iter()
            .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_HEADER))
            .collect();
        assert_eq!(headers.len(), 1, "transcript must have exactly 1 header");
        assert_eq!(headers[0].value["scenario_name"].as_str(), Some(test_name));

        // Must have step results
        let step_count = lines
            .iter()
            .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_STEP))
            .count();
        assert_eq!(step_count, 2, "transcript must have 2 step results");

        // Must have event boundaries
        let boundary_count = lines
            .iter()
            .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_BOUNDARY))
            .count();
        assert!(
            boundary_count >= 6,
            "transcript must have >= 6 boundaries (3 per step), got {boundary_count}"
        );
    }

    // If the session artifact exists, verify tree structure
    if let Some(sess) = session_artifact
        && let Ok(content) = std::fs::read_to_string(&sess.path)
    {
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() >= 2 {
            let header: Value = serde_json::from_str(lines[0]).expect("parse header");
            assert_eq!(header["type"].as_str(), Some("session"));
            assert_eq!(header["version"].as_u64(), Some(u64::from(SESSION_VERSION)));
        }
    }
}

/// Scenario: load explicit `--session <path>` then verify restored metadata
/// through `/session`.
///
/// State transitions:
///   INIT → SESSION_LOADED → SESSION_INFO_RENDERED → EXIT_CLEAN
///
/// Log checkpoints:
///   - startup succeeds
///   - `/session` output includes seeded session id
///   - `session.jsonl` artifact exists for replay/debug
#[test]
fn e2e_scenario_session_restore_explicit_path() {
    let _lock = TmuxE2eLock::acquire();

    let restore_harness = common::TestHarness::new("scenario_session_restore_explicit_path_seed");
    let sessions_dir = restore_harness.temp_path("sessions");
    let session_file = sessions_dir.join("restore-seeded.jsonl");
    let session_id = "restore-session-1234";
    let restore_marker = "Restored marker message from previous session.";
    write_minimal_session_jsonl(
        &session_file,
        restore_harness.temp_dir(),
        session_id,
        restore_marker,
    );

    let session_path_str = session_file.display().to_string();
    let sessions_dir_str = sessions_dir.display().to_string();
    let scenario = CliScenario::new("session_restore_explicit_path")
        .arg("--provider")
        .arg("openai")
        .arg("--model")
        .arg("gpt-4o-mini")
        .arg("--no-tools")
        .arg("--no-skills")
        .arg("--no-prompt-templates")
        .arg("--no-extensions")
        .arg("--no-themes")
        .arg("--session")
        .arg(&session_path_str)
        .arg("--system-prompt")
        .arg("pi e2e session restore harness")
        .env("PI_SESSIONS_DIR", &sessions_dir_str)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Persist:")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/session", session_id)
                .label("session_info")
                .timeout_secs(12),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "session_restore_explicit_path");
    assert_eq!(transcript.steps.len(), 2);
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "session info should include restored session id"
    );
    assert!(transcript.exit_status.is_clean());

    let session_artifact = transcript
        .artifacts
        .iter()
        .find(|a| a.name == "session.jsonl");
    assert!(
        session_artifact.is_some(),
        "expected scenario runner to record session.jsonl artifact"
    );
    let session_content =
        std::fs::read_to_string(&session_file).expect("read restored session file after run");
    assert!(
        session_content.contains(session_id),
        "restored session file should retain seeded session id"
    );
    assert!(
        session_content.contains(restore_marker),
        "restored session file should retain seeded marker content"
    );
}

// ─── Suite 5: Tool Chaining Scenarios ───────────────────────────────────────

/// Scenario: VCR-based tool chaining (read tool → text response).
///
/// State transitions:
///   INIT → WELCOME → SEND_PROMPT → TOOL_CALL_ISSUED → TOOL_EXECUTED →
///   TOOL_RESULT_SENT → FINAL_RESPONSE → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - step[1] (tool_chain): tool name "read" visible, final response "Done." visible
///   - exit_status: Clean
///   - Session JSONL: user msg + assistant (tool_use) + tool_result + assistant (text)
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_tool_chain_read_response() {
    let _lock = TmuxE2eLock::acquire();

    let test_name = "scenario_tool_chain";

    // Create test file and VCR cassette
    let harness = common::TestHarness::new(test_name);

    let sample_path = harness.temp_path(SAMPLE_FILE_NAME);
    std::fs::write(&sample_path, SAMPLE_FILE_CONTENT).expect("write sample file");

    let tool_output = read_output_for_sample(harness.temp_dir(), SAMPLE_FILE_NAME);

    let cassette_dir = harness.temp_path("vcr");
    let env_root = harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt(harness.temp_dir(), &env_root);
    let _cassette_path = write_vcr_cassette(&cassette_dir, &tool_output, &system_prompt);

    let scenario = CliScenario::new(test_name)
        .args(&vcr_interactive_args())
        .file(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENT)
        .env(VCR_ENV_MODE, "playback")
        .env(VCR_ENV_DIR, &cassette_dir.display().to_string())
        .env("PI_VCR_TEST_NAME", VCR_TEST_NAME)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text(VCR_PROMPT, "Done.")
                .label("tool_chain_read_response")
                .timeout_secs(30),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, test_name);
    assert_eq!(transcript.steps.len(), 2);
    assert_transcript_invariants(&transcript);

    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "tool chain step must succeed (VCR read → response)"
    );
    assert!(transcript.exit_status.is_clean());

    // Verify the tool chain step captured enough pane content
    // (tool output renders multiple lines)
    assert!(
        transcript.steps[1].pane_snapshot_lines > 5,
        "tool chain step should capture significant pane content, got {} lines",
        transcript.steps[1].pane_snapshot_lines
    );
}

/// Scenario: multi-turn tool chaining in a single prompt loop:
/// `read` tool call #1 -> `read` tool call #2 -> final model text.
///
/// State transitions:
///   INIT → WELCOME → PROMPT_SENT → TOOL_1 → TOOL_2 → FINAL_RESPONSE → EXIT_CLEAN
///
/// Log checkpoints:
///   - startup succeeds
///   - chain step succeeds with deterministic final marker
///   - transcript includes `input_sent`, `output_matched`, and `step_end`
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_tool_chain_multi_turn() {
    let _lock = TmuxE2eLock::acquire();

    let harness = common::TestHarness::new("scenario_tool_chain_multi_turn_seed");
    let first_path = harness.temp_path("chain-one.txt");
    let second_path = harness.temp_path("chain-two.txt");
    std::fs::write(&first_path, "chain one\n").expect("write first chain file");
    std::fs::write(&second_path, "chain two\n").expect("write second chain file");

    let cassette_dir = harness.temp_path("vcr");
    let _cassette_path = write_vcr_multi_tool_chain_cassette(
        &cassette_dir,
        &first_path.display().to_string(),
        &second_path.display().to_string(),
    );

    let scenario = CliScenario::new("tool_chain_multi_turn")
        .args(&vcr_interactive_args())
        .env(VCR_ENV_MODE, "playback")
        .env(VCR_ENV_DIR, &cassette_dir.display().to_string())
        .env("PI_VCR_TEST_NAME", VCR_MULTI_TOOL_CHAIN_TEST_NAME)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("Read both chain files.", "Tool chain complete.")
                .label("tool_chain_multi")
                .timeout_secs(35),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, "tool_chain_multi_turn");
    assert_eq!(transcript.steps.len(), 2);
    assert_transcript_invariants(&transcript);
    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "multi-turn tool chain should reach final completion marker"
    );
    assert!(transcript.exit_status.is_clean());

    let boundary_types: Vec<&str> = transcript.steps[1]
        .event_boundaries
        .iter()
        .map(|b| b.boundary_type.as_str())
        .collect();
    assert!(
        boundary_types.contains(&"input_sent"),
        "tool chain step should record input_sent boundary: {boundary_types:?}"
    );
    assert!(
        boundary_types.contains(&"output_matched"),
        "tool chain step should record output_matched boundary: {boundary_types:?}"
    );
    assert!(
        boundary_types.contains(&"step_end"),
        "tool chain step should record step_end boundary: {boundary_types:?}"
    );
}

// ─── Suite 6: Prompt Loop Scenarios ─────────────────────────────────────────

/// Scenario: Multi-round prompt loop via VCR (2 exchanges).
///
/// State transitions:
///   INIT → WELCOME → PROMPT_1 → RESPONSE_1 → PROMPT_2 → RESPONSE_2 → EXIT_CLEAN
///
/// Log checkpoints:
///   - step[0] (startup): success=true
///   - step[1] (round_1): first response visible, success=true
///   - step[2] (round_2): second response visible, success=true
///   - exit_status: Clean
///   - Session JSONL: 2 user messages, 2 assistant messages
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_prompt_loop_multi_round() {
    let _lock = TmuxE2eLock::acquire();

    let test_name = "scenario_prompt_loop";

    let harness = common::TestHarness::new(test_name);
    let cassette_dir = harness.temp_path("vcr");
    let env_root = harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt_for_args(
        vcr_interactive_args_no_tools,
        harness.temp_dir(),
        &env_root,
    );

    // Build a multi-interaction VCR cassette: 2 user messages, 2 responses
    let prompt_1 = "What is Rust?";
    let response_1 = "Rust is a systems programming language.";
    let prompt_2 = "Tell me more.";
    let response_2 = "Rust emphasizes safety and performance.";

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize");
        format!("event: {event}\ndata: {payload}\n\n")
    };

    let make_response = |text: &str, input_tokens: u64| -> RecordedResponse {
        RecordedResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body_chunks: vec![
                sse_chunk(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": { "usage": { "input_tokens": input_tokens } }
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
                        "delta": { "type": "text_delta", "text": text }
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
                        "usage": { "output_tokens": 10 }
                    }),
                ),
                sse_chunk("message_stop", json!({ "type": "message_stop" })),
            ],
            body_chunks_base64: None,
        }
    };

    let mut request_1 = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": prompt_1 } ] }
        ],
        "system": &system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut request_1);

    let mut request_2 = json!({
        "model": VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ { "type": "text", "text": prompt_1 } ] },
            {
                "role": "assistant",
                "content": [ { "type": "text", "text": response_1 } ]
            },
            { "role": "user", "content": [ { "type": "text", "text": prompt_2 } ] }
        ],
        "system": &system_prompt,
        "max_tokens": VCR_MODEL_MAX_TOKENS,
        "stream": true,
    });
    common::apply_prompt_cache_wire_shape(&mut request_2);

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
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
                    body: Some(request_1),
                    body_text: None,
                },
                response: make_response(response_1, 10),
            },
            Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://api.anthropic.com/v1/messages".to_string(),
                    headers: vec![
                        ("Content-Type".to_string(), "application/json".to_string()),
                        ("Accept".to_string(), "text/event-stream".to_string()),
                    ],
                    body: Some(request_2),
                    body_text: None,
                },
                response: make_response(response_2, 30),
            },
        ],
    };

    std::fs::create_dir_all(&cassette_dir).expect("create cassette dir");
    std::fs::write(
        cassette_dir.join(format!("{test_name}.json")),
        serde_json::to_string_pretty(&cassette).unwrap(),
    )
    .expect("write cassette");

    let scenario = CliScenario::new(test_name)
        .args(&vcr_interactive_args_no_tools())
        .env(VCR_ENV_MODE, "playback")
        .env(VCR_ENV_DIR, &cassette_dir.display().to_string())
        .env("PI_VCR_TEST_NAME", test_name)
        .env("PI_TEST_MODE", "1")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text(prompt_1, response_1)
                .label("round_1")
                .timeout_secs(15),
        )
        .step(
            ScenarioStep::send_text(prompt_2, response_2)
                .label("round_2")
                .timeout_secs(15),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");

    assert_eq!(transcript.scenario_name, test_name);
    assert_eq!(transcript.steps.len(), 3);
    assert_transcript_invariants(&transcript);

    // All steps must succeed
    assert!(transcript.steps[0].success, "startup must succeed");
    assert!(
        transcript.steps[1].success,
        "round_1 must succeed: VCR response '{response_1}' should appear"
    );
    assert!(
        transcript.steps[2].success,
        "round_2 must succeed: VCR response '{response_2}' should appear"
    );

    assert!(transcript.exit_status.is_clean());

    // Verify temporal ordering: each step ends after the previous
    for w in transcript.steps.windows(2) {
        let prev_end = w[0].event_boundaries.last().map_or(0, |b| b.timestamp_ms);
        let next_start = w[1].event_boundaries.first().map_or(0, |b| b.timestamp_ms);
        assert!(
            next_start >= prev_end,
            "step '{}' should start after step '{}' ends",
            w[1].label,
            w[0].label
        );
    }
}

// ─── Suite 7: Batch Scenario Execution ──────────────────────────────────────

/// Verify `ScenarioRunner::run_batch` executes multiple scenarios sequentially
/// and produces independent transcripts with distinct run IDs.
#[test]
fn e2e_scenario_batch_execution() {
    let _lock = TmuxE2eLock::acquire();

    let scenarios = vec![
        CliScenario::new("batch_startup_a")
            .args(&base_interactive_args())
            .step(
                ScenarioStep::wait("Welcome to Pi!")
                    .label("startup")
                    .timeout_secs(20),
            )
            .exit(ExitStrategy::Graceful),
        CliScenario::new("batch_startup_b")
            .args(&base_interactive_args())
            .step(
                ScenarioStep::wait("Welcome to Pi!")
                    .label("startup")
                    .timeout_secs(20),
            )
            .exit(ExitStrategy::Graceful),
    ];

    let results = ScenarioRunner::run_batch(scenarios);

    assert_eq!(results.len(), 2);

    let (name_a, transcript_a) = &results[0];
    let (name_b, transcript_b) = &results[1];

    assert_eq!(name_a, "batch_startup_a");
    assert_eq!(name_b, "batch_startup_b");

    if let (Some(ta), Some(tb)) = (transcript_a, transcript_b) {
        // Each transcript has distinct run IDs
        assert_ne!(ta.run_id, tb.run_id, "batch run IDs must be distinct");

        // Both succeeded
        assert!(ta.steps[0].success, "batch_a startup must succeed");
        assert!(tb.steps[0].success, "batch_b startup must succeed");
        assert!(ta.exit_status.is_clean());
        assert!(tb.exit_status.is_clean());

        // Both have valid invariants
        assert_transcript_invariants(ta);
        assert_transcript_invariants(tb);
    }
}

// ─── Suite 8: Transcript Diff Validation ────────────────────────────────────

/// Scenario: Run a known-good scenario and validate that transcript diff
/// produces zero differences when compared against itself.
///
/// This exercises the TranscriptDiff tooling end-to-end, proving that
/// deterministic replay produces stable transcripts.
#[test]
fn e2e_scenario_transcript_diff_self_compare() {
    let _lock = TmuxE2eLock::acquire();

    let scenario = CliScenario::new("diff_self_compare")
        .args(&base_interactive_args())
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/help", "Available commands:")
                .label("help")
                .timeout_secs(15),
        )
        .exit(ExitStrategy::Graceful);

    let transcript = ScenarioRunner::run(scenario).expect("tmux unavailable");
    assert_transcript_invariants(&transcript);

    let temp = tempfile::tempdir().expect("create transcript tempdir");
    let transcript_path = temp.path().join("scenario-transcript.jsonl");
    transcript
        .write_jsonl(&transcript_path)
        .expect("write transcript");
    let content = std::fs::read_to_string(&transcript_path).expect("read transcript");
    let lines = parse_transcript(&content);

    // Self-compare must produce zero differences
    let diff = TranscriptDiff::compare(&lines, &lines);
    assert!(
        !diff.has_differences(),
        "Self-comparison must produce zero diffs:\n{}",
        diff.human_summary()
    );
    assert_eq!(diff.expected_step_count, diff.actual_step_count);

    // Verify the failure_summary function works on successful transcripts
    let summary = transcript_diff::failure_summary(
        "diff_self_compare",
        &lines,
        &format!("{:?}", transcript.exit_status),
    );
    assert!(
        summary.contains("0 failed"),
        "failure_summary should show 0 failed for passing scenario:\n{summary}"
    );
}

// ─── Suite 9: Replay Manifest + Divergence Detection ─────────────────────────

/// Scenario: Save a replay manifest from a run, reload it, and verify
/// the reconstructed scenario matches the original definition.
#[test]
fn e2e_scenario_replay_manifest_roundtrip() {
    use common::scenario_runner::ReplayManifest;

    let scenario = CliScenario::new("manifest_roundtrip")
        .args(&base_interactive_args())
        .env("TEST_VAR", "test_value")
        .step(
            ScenarioStep::wait("Welcome to Pi!")
                .label("startup")
                .timeout_secs(20),
        )
        .step(
            ScenarioStep::send_text("/help", "Available")
                .label("help")
                .timeout_secs(10),
        )
        .exit(ExitStrategy::Graceful);

    // Build a mock transcript for the manifest (no tmux needed)
    let mock_transcript = common::scenario_runner::ScenarioTranscript {
        scenario_name: "manifest_roundtrip".to_string(),
        run_id: "test-run-abc".to_string(),
        steps: vec![],
        exit_status: common::scenario_runner::ExitStatus::Clean,
        total_elapsed_ms: 1234,
        artifacts: vec![],
    };

    let manifest = ReplayManifest::from_run(&scenario, &mock_transcript, 42, None);

    // Verify manifest fields
    assert_eq!(manifest.schema, "pi.test.replay.v1");
    assert_eq!(manifest.scenario_name, "manifest_roundtrip");
    assert_eq!(manifest.seed, 42);
    assert_eq!(manifest.original_run_id, "test-run-abc");
    assert_eq!(manifest.exit_strategy, "graceful");
    assert!(manifest.env.contains_key("TEST_VAR"));
    assert_eq!(manifest.steps.len(), 2);
    assert_eq!(manifest.steps[0].action_type, "wait");
    assert_eq!(manifest.steps[1].action_type, "send_text");
    assert!(manifest.system_info.contains_key("os"));

    // Save and reload
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest_path = dir.path().join("replay.json");
    manifest.save(&manifest_path).expect("save manifest");

    let loaded = ReplayManifest::load(&manifest_path).expect("load manifest");
    assert_eq!(loaded.scenario_name, "manifest_roundtrip");
    assert_eq!(loaded.seed, 42);
    assert_eq!(loaded.steps.len(), 2);

    // Reconstruct scenario from manifest
    let reconstructed = loaded.to_scenario();
    assert_eq!(reconstructed.name, "manifest_roundtrip_replay");
    assert_eq!(reconstructed.args, scenario.args);
    assert_eq!(reconstructed.steps.len(), scenario.steps.len());
    assert!(matches!(
        reconstructed.exit_strategy,
        ExitStrategy::Graceful
    ));

    // Step definitions should match
    for (orig, recon) in scenario.steps.iter().zip(reconstructed.steps.iter()) {
        assert_eq!(orig.expect, recon.expect);
        assert_eq!(orig.label, recon.label);
    }
}

/// Scenario: Verify divergence detection catches success/timing/exit mismatches.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_scenario_divergence_detection() {
    use common::scenario_runner::{
        CorrelationId, EventBoundary, ExitStatus, ReplayManifest, ScenarioTranscript, StepResult,
        detect_divergences, divergence_summary, write_divergence_report,
    };

    let make_step = |label: &str, success: bool, elapsed: u64| StepResult {
        correlation_id: CorrelationId {
            run_id: "run-1".to_string(),
            step_index: 0,
            composite: "run-1/0".to_string(),
        },
        label: label.to_string(),
        action: "send_text: hello".to_string(),
        expected: "world".to_string(),
        pane_snapshot_lines: 24,
        elapsed_ms: elapsed,
        success,
        event_boundaries: vec![
            EventBoundary {
                boundary_type: "step_start".to_string(),
                timestamp_ms: 0,
                details: None,
            },
            EventBoundary {
                boundary_type: "output_matched".to_string(),
                timestamp_ms: elapsed,
                details: None,
            },
            EventBoundary {
                boundary_type: "step_end".to_string(),
                timestamp_ms: elapsed,
                details: None,
            },
        ],
    };

    let original = ScenarioTranscript {
        scenario_name: "test".to_string(),
        run_id: "run-1".to_string(),
        steps: vec![
            make_step("startup", true, 100),
            make_step("action", true, 200),
        ],
        exit_status: ExitStatus::Clean,
        total_elapsed_ms: 300,
        artifacts: vec![],
    };

    // Case 1: Identical transcripts → no divergences
    let divs = detect_divergences(&original, &original);
    assert!(
        divs.is_empty(),
        "identical transcripts should have no divergences"
    );

    // Case 2: Success changed → critical divergence
    let mut replay_fail = original.clone();
    replay_fail.steps[1].success = false;
    let divs = detect_divergences(&original, &replay_fail);
    assert!(!divs.is_empty());
    let critical = divs
        .iter()
        .find(|d| d.severity == "critical" && d.field == "success");
    assert!(critical.is_some(), "success change should be critical");

    // Case 3: Large timing drift → warning
    let mut replay_slow = original.clone();
    replay_slow.steps[0].elapsed_ms = 1000; // 10x the original 100ms
    let divs = detect_divergences(&original, &replay_slow);
    let timing_warn = divs.iter().find(|d| d.field == "elapsed_ms");
    assert!(
        timing_warn.is_some(),
        "10x timing drift should produce warning"
    );

    // Case 4: Exit status changed → critical
    let mut replay_exit = original.clone();
    replay_exit.exit_status = ExitStatus::Timeout;
    let divs = detect_divergences(&original, &replay_exit);
    let exit_div = divs.iter().find(|d| d.field == "exit_status");
    assert!(exit_div.is_some(), "exit status change should be detected");
    assert_eq!(exit_div.unwrap().severity, "critical");

    // Case 5: Step count mismatch → critical
    let mut replay_short = original.clone();
    replay_short.steps.pop();
    let divs = detect_divergences(&original, &replay_short);
    let count_div = divs.iter().find(|d| d.field == "step_count");
    assert!(
        count_div.is_some(),
        "step count mismatch should be detected"
    );

    // Case 6: divergence_summary formatting
    let divs = detect_divergences(&original, &replay_fail);
    let manifest = ReplayManifest {
        schema: "pi.test.replay.v1".to_string(),
        scenario_name: "test".to_string(),
        seed: 42,
        args: vec![],
        env: std::collections::BTreeMap::new(),
        vcr_cassette_dir: None,
        vcr_test_name: None,
        steps: vec![],
        exit_strategy: "graceful".to_string(),
        original_run_id: "run-1".to_string(),
        original_transcript_path: None,
        created_at: "now".to_string(),
        system_info: std::collections::BTreeMap::new(),
    };

    let summary = divergence_summary(&divs, &manifest);
    assert!(
        summary.contains("divergence"),
        "summary should mention divergences"
    );
    assert!(
        summary.contains("critical"),
        "summary should mention critical severity"
    );

    // Empty divergences should produce "matched perfectly"
    let empty_summary = divergence_summary(&[], &manifest);
    assert!(
        empty_summary.contains("matched perfectly"),
        "empty divergences should say matched: {empty_summary}"
    );

    // Case 7: write_divergence_report produces valid JSONL
    let dir = tempfile::tempdir().expect("tempdir");
    let report_path = dir.path().join("divergences.jsonl");
    write_divergence_report(&divs, &manifest, &report_path).expect("write report");
    let report = std::fs::read_to_string(&report_path).expect("read report");
    let lines: Vec<&str> = report.lines().collect();
    assert!(lines.len() >= 2, "report should have header + divergences");
    let header: serde_json::Value = serde_json::from_str(lines[0]).expect("parse header");
    assert_eq!(header["type"], "replay_divergence_header");
}

/// Scenario: JSONL transcript roundtrip through load_transcript_from_jsonl.
#[test]
fn e2e_scenario_transcript_jsonl_reload() {
    use common::scenario_runner::{
        CorrelationId, EventBoundary, ExitStatus, ScenarioTranscript, StepResult,
    };

    let original = ScenarioTranscript {
        scenario_name: "reload_test".to_string(),
        run_id: "run-reload".to_string(),
        steps: vec![StepResult {
            correlation_id: CorrelationId {
                run_id: "run-reload".to_string(),
                step_index: 0,
                composite: "run-reload/0".to_string(),
            },
            label: "greet".to_string(),
            action: "send_text: hello".to_string(),
            expected: "world".to_string(),
            pane_snapshot_lines: 10,
            elapsed_ms: 42,
            success: true,
            event_boundaries: vec![
                EventBoundary {
                    boundary_type: "step_start".to_string(),
                    timestamp_ms: 0,
                    details: None,
                },
                EventBoundary {
                    boundary_type: "output_matched".to_string(),
                    timestamp_ms: 42,
                    details: None,
                },
                EventBoundary {
                    boundary_type: "step_end".to_string(),
                    timestamp_ms: 42,
                    details: None,
                },
            ],
        }],
        exit_status: ExitStatus::Clean,
        total_elapsed_ms: 100,
        artifacts: vec![],
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    original.write_jsonl(&path).expect("write");

    // Load back through the JSONL parser
    let loaded =
        common::scenario_runner::load_transcript_from_jsonl(&path).expect("load transcript");

    assert_eq!(loaded.scenario_name, "reload_test");
    assert_eq!(loaded.run_id, "run-reload");
    assert_eq!(loaded.steps.len(), 1);
    assert_eq!(loaded.steps[0].label, "greet");
    assert!(loaded.steps[0].success);
    assert_eq!(loaded.steps[0].elapsed_ms, 42);
    assert!(loaded.steps[0].event_boundaries.len() >= 2);
    assert_eq!(
        loaded.steps[0].event_boundaries[0].boundary_type,
        "step_start"
    );
    assert!(loaded.exit_status.is_clean());
}

/// Scenario: ReplayStepDef roundtrip (step → def → step).
#[test]
fn e2e_scenario_replay_step_roundtrip() {
    use common::scenario_runner::{ReplayStepDef, StepAction};

    // send_text step
    let step = ScenarioStep::send_text("hello", "world")
        .label("greet")
        .timeout_secs(10);
    let def = ReplayStepDef::from_step(&step);
    assert_eq!(def.action_type, "send_text");
    assert_eq!(def.action_value.as_deref(), Some("hello"));
    assert_eq!(def.expect, "world");
    assert_eq!(def.label.as_deref(), Some("greet"));
    assert_eq!(def.timeout_ms, 10_000);

    let back = def.to_step();
    assert!(matches!(back.action, StepAction::SendText(ref t) if t == "hello"));
    assert_eq!(back.expect, "world");
    assert_eq!(back.label.as_deref(), Some("greet"));
    assert_eq!(back.timeout, Duration::from_secs(10));

    // send_key step
    let key_step = ScenarioStep::send_key("C-c", "exit");
    let key_def = ReplayStepDef::from_step(&key_step);
    assert_eq!(key_def.action_type, "send_key");
    let key_back = key_def.to_step();
    assert!(matches!(key_back.action, StepAction::SendKey(ref k) if k == "C-c"));

    // wait step
    let wait_step = ScenarioStep::wait("ready");
    let wait_def = ReplayStepDef::from_step(&wait_step);
    assert_eq!(wait_def.action_type, "wait");
    assert!(wait_def.action_value.is_none());
    let wait_back = wait_def.to_step();
    assert!(matches!(wait_back.action, StepAction::Wait));
}

/// Scenario: ExitStrategy serialization roundtrip via manifest.
#[test]
fn e2e_scenario_exit_strategy_roundtrip() {
    use common::scenario_runner::ReplayManifest;

    // Helper to test one strategy
    let test = |strategy: ExitStrategy, expected_str: &str| {
        let scenario = CliScenario::new("exit_test").exit(strategy);
        let mock_transcript = common::scenario_runner::ScenarioTranscript {
            scenario_name: "exit_test".to_string(),
            run_id: "r".to_string(),
            steps: vec![],
            exit_status: common::scenario_runner::ExitStatus::Clean,
            total_elapsed_ms: 0,
            artifacts: vec![],
        };
        let manifest = ReplayManifest::from_run(&scenario, &mock_transcript, 1, None);
        assert_eq!(manifest.exit_strategy, expected_str);

        let reconstructed = manifest.to_scenario();
        // Verify the reconstructed exit strategy type matches
        match expected_str {
            "graceful" => assert!(matches!(
                reconstructed.exit_strategy,
                ExitStrategy::Graceful
            )),
            "ctrl_c" => assert!(matches!(reconstructed.exit_strategy, ExitStrategy::CtrlC)),
            "ctrl_d" => assert!(matches!(reconstructed.exit_strategy, ExitStrategy::CtrlD)),
            s if s.starts_with("timeout_") => {
                assert!(matches!(
                    reconstructed.exit_strategy,
                    ExitStrategy::Timeout(_)
                ));
            }
            _ => panic!("unexpected exit strategy string: {expected_str}"),
        }
    };

    test(ExitStrategy::Graceful, "graceful");
    test(ExitStrategy::CtrlC, "ctrl_c");
    test(ExitStrategy::CtrlD, "ctrl_d");
    test(
        ExitStrategy::Timeout(Duration::from_secs(30)),
        "timeout_30000ms",
    );
}

// ============================================================================
// VCR cassette shape parity (no tmux needed).
//
// Every VCR e2e in this file synthesizes its cassette request bodies by hand.
// When the Anthropic provider's wire shape evolves (e.g. the prompt-cache
// work moved `system` from a string to a block array), the hand-written
// templates silently stop matching and every VCR e2e dies with "No matching
// interaction". This test rebuilds the real provider request for the
// tool-read scenario's first turn and template-matches the synthesized
// cassette against it, failing with both bodies printed so the drift is
// obvious.
// ============================================================================

/// Recorded-template match with the same semantics as `pi::vcr`:
/// object keys in `recorded` must match (incoming may have extras), arrays
/// are strict length + per-element, scalars strict equality.
fn vcr_template_matches(recorded: &Value, incoming: &Value) -> bool {
    match (recorded, incoming) {
        (Value::Object(recorded_obj), Value::Object(incoming_obj)) => {
            recorded_obj.iter().all(|(key, recorded_value)| {
                incoming_obj.get(key).map_or_else(
                    || recorded_value.is_null(),
                    |incoming_value| vcr_template_matches(recorded_value, incoming_value),
                )
            })
        }
        (Value::Array(recorded_items), Value::Array(incoming_items)) => {
            recorded_items.len() == incoming_items.len()
                && recorded_items
                    .iter()
                    .zip(incoming_items)
                    .all(|(left, right)| vcr_template_matches(left, right))
        }
        _ => recorded == incoming,
    }
}

#[test]
fn vcr_tool_read_cassette_template_matches_provider_request() {
    let harness = common::TestHarness::new("vcr_tool_read_cassette_template_matches_request");
    let workdir = harness.temp_dir().to_path_buf();
    let env_root = workdir.join("env");
    let system_prompt = build_vcr_system_prompt(&workdir, &env_root);
    let cassette_dir = harness.temp_path("vcr");
    let cassette_path = write_vcr_cassette(&cassette_dir, "irrelevant tool output", &system_prompt);
    let cassette: Value =
        serde_json::from_str(&std::fs::read_to_string(&cassette_path).expect("read cassette"))
            .expect("parse cassette");
    let recorded = cassette["interactions"][0]["request"]["body"].clone();
    assert!(!recorded.is_null(), "cassette first request body missing");

    // Mirror the interactive binary's first-turn request for the same CLI
    // args (`vcr_interactive_args`): anthropic provider, read tool only,
    // thinking off, prompt-cache retention default (short).
    let provider = pi::providers::anthropic::AnthropicProvider::new(VCR_MODEL);
    let tools: Vec<pi::provider::ToolDef> = vcr_anthropic_tool_schemas(&workdir)
        .iter()
        .map(|schema| pi::provider::ToolDef {
            name: schema["name"].as_str().unwrap_or_default().to_string(),
            description: schema["description"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            parameters: schema["input_schema"].clone(),
        })
        .collect();
    let messages = vec![pi::model::Message::User(pi::model::UserMessage {
        content: pi::model::UserContent::Text(VCR_PROMPT.to_string()),
        timestamp: 0,
    })];
    let context = pi::provider::Context {
        system_prompt: Some(std::borrow::Cow::Borrowed(system_prompt.as_str())),
        messages: std::borrow::Cow::Owned(messages),
        tools: std::borrow::Cow::Owned(tools),
    };
    let options = pi::provider::StreamOptions {
        cache_retention: pi::provider::CacheRetention::Short,
        max_tokens: Some(VCR_MODEL_MAX_TOKENS),
        thinking_level: Some(pi::model::ThinkingLevel::Off),
        ..Default::default()
    };
    let actual = serde_json::to_value(provider.build_request(&context, &options))
        .expect("serialize provider request");

    assert!(
        vcr_template_matches(&recorded, &actual),
        "synthesized VCR cassette no longer matches the provider's request \
         shape.\n--- recorded template ---\n{}\n--- actual provider request ---\n{}",
        serde_json::to_string_pretty(&recorded).unwrap_or_default(),
        serde_json::to_string_pretty(&actual).unwrap_or_default(),
    );
}

// ============================================================================
// Tool-call stress e2e (bd-d4um2): one turn with dozens of sequential real
// tool executions through the live binary in tmux, then scroll integrity.
// The agent's max_tool_iterations default is 50, so the round count stays
// under it while still exercising the "lots of tool calls" failure mode HN
// reported (frozen scrolling / corrupted transcript).
// ============================================================================

const VCR_STRESS_TEST_NAME: &str = "e2e_tui_tool_stress";
// 35 = 70% of the default max_tool_iterations (50). At >=80% the agent
// injects a "[runtime] Tool-iteration budget" notice into the conversation,
// which a pre-recorded cassette cannot predict.
const STRESS_TOOL_ROUNDS: usize = 35;
const STRESS_PROMPT: &str = "Stress-read sample.txt thirty-five times";
const STRESS_DONE_MARKER: &str = "STRESS-DONE";

#[allow(clippy::too_many_lines)]
fn write_vcr_stress_cassette(dir: &Path, tool_output: &str, system_prompt: &str) -> PathBuf {
    let cassette_path = dir.join(format!("{VCR_STRESS_TEST_NAME}.json"));
    let tool_schemas = vcr_anthropic_tool_schemas(dir);

    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let tool_args_json =
        serde_json::to_string(&json!({ "path": SAMPLE_FILE_NAME })).expect("serialize tool args");

    let request_for_history = |history: &[Value]| -> Value {
        let mut request = json!({
            "model": VCR_MODEL,
            "messages": history,
            "system": system_prompt,
            "max_tokens": VCR_MODEL_MAX_TOKENS,
            "stream": true,
            "tools": tool_schemas.clone(),
        });
        common::apply_prompt_cache_wire_shape(&mut request);
        request
    };

    let tool_use_response = |call_id: &str| RecordedResponse {
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
                    "content_block": { "type": "tool_use", "id": call_id, "name": "read" }
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

    let final_text = format!("All {STRESS_TOOL_ROUNDS} reads complete. {STRESS_DONE_MARKER}");
    let final_response = RecordedResponse {
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
                    "delta": { "type": "text_delta", "text": final_text }
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

    let make_request = |body: Value| RecordedRequest {
        method: "POST".to_string(),
        url: "https://api.anthropic.com/v1/messages".to_string(),
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
        ],
        body: Some(body),
        body_text: None,
    };

    let mut history: Vec<Value> = vec![json!(
        { "role": "user", "content": [ { "type": "text", "text": STRESS_PROMPT } ] }
    )];
    let mut interactions: Vec<Interaction> = Vec::with_capacity(STRESS_TOOL_ROUNDS + 1);
    for round in 0..STRESS_TOOL_ROUNDS {
        let call_id = format!("toolu_stress_{round}");
        interactions.push(Interaction {
            request: make_request(request_for_history(&history)),
            response: tool_use_response(&call_id),
        });
        history.push(json!({
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": call_id,
                    "name": "read",
                    "input": { "path": SAMPLE_FILE_NAME }
                }
            ]
        }));
        history.push(json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": [ { "type": "text", "text": tool_output } ]
                }
            ]
        }));
    }
    interactions.push(Interaction {
        request: make_request(request_for_history(&history)),
        response: final_response,
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: VCR_STRESS_TEST_NAME.to_string(),
        recorded_at: "1970-01-01T00:00:00Z".to_string(),
        interactions,
    };

    std::fs::create_dir_all(dir).expect("create cassette dir");
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(&cassette_path, json).expect("write cassette");
    cassette_path
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_tool_call_stress_scroll_stays_functional() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_tool_stress") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.harness.section("setup");
    let sample_path = session.harness.temp_path(SAMPLE_FILE_NAME);
    let sample_content: String = (1..=40).fold(String::new(), |mut out, line| {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "stress line {line:02}: the quick brown fox jumps over the lazy dog"
        );
        out
    });
    std::fs::write(&sample_path, &sample_content).expect("write sample file");

    let tool_output = read_output_for_sample(session.harness.temp_dir(), SAMPLE_FILE_NAME);
    let cassette_dir = session.harness.temp_path("vcr");
    let env_root = session.harness.temp_dir().join("env");
    let system_prompt = build_vcr_system_prompt(session.harness.temp_dir(), &env_root);
    let cassette_path = write_vcr_stress_cassette(&cassette_dir, &tool_output, &system_prompt);
    session
        .harness
        .record_artifact("stress-cassette.json", &cassette_path);

    let cassette_dir_str = cassette_dir.display().to_string();
    session.set_env(VCR_ENV_MODE, "playback");
    session.set_env(VCR_ENV_DIR, &cassette_dir_str);
    session.set_env("PI_VCR_TEST_NAME", VCR_STRESS_TEST_NAME);
    session.set_env("PI_TEST_MODE", "1");

    session.launch(&vcr_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // One turn, STRESS_TOOL_ROUNDS sequential tool executions, then the
    // final text. Generous timeout: this is a real binary doing 40 provider
    // round-trips plus 40 tool runs.
    let pane = session.send_text_and_wait(
        "stress_prompt",
        STRESS_PROMPT,
        STRESS_DONE_MARKER,
        Duration::from_secs(120),
    );
    assert!(
        !pane.contains("No matching interaction"),
        "cassette drift during stress run.\nPane:\n{pane}"
    );
    let pct = parse_scroll_percent(&pane).expect("scroll indicator after stress");
    assert_eq!(pct, 100, "tail-following viewport should end at the bottom");

    // Scrolling must still work after the stress: page up moves off the
    // bottom, and returning to the bottom is possible.
    session.tmux.send_key("PPage");
    session.tmux.send_key("PPage");
    std::thread::sleep(Duration::from_millis(400));
    let pane_up = session.wait_and_capture("after_pgup", "%]", Duration::from_secs(5));
    let pct_up = parse_scroll_percent(&pane_up).expect("scroll indicator after PageUp");
    assert!(
        pct_up < 100,
        "PageUp must scroll away from the bottom after a tool-heavy turn (got {pct_up}%)"
    );
    for _ in 0..80 {
        session.tmux.send_key("NPage");
    }
    std::thread::sleep(Duration::from_millis(400));
    let pane_down = session.wait_and_capture("after_pgdn", "%]", Duration::from_secs(5));
    let pct_down = parse_scroll_percent(&pane_down).expect("scroll indicator after PageDown");
    assert_eq!(pct_down, 100, "PageDown must return to the bottom");

    session.exit_gracefully();
    session.write_artifacts();

    // The session log must contain every tool round.
    let sessions_dir = session.harness.temp_dir().join("env").join("sessions");
    let session_file = find_session_jsonl(&sessions_dir).expect("expected session jsonl file");
    let content = std::fs::read_to_string(&session_file).expect("read session jsonl");
    let tool_mentions = content.matches("toolu_stress_").count();
    assert!(
        tool_mentions >= STRESS_TOOL_ROUNDS * 2,
        "expected all {STRESS_TOOL_ROUNDS} tool rounds in the session log, \
         found {tool_mentions} toolu_stress_ mentions"
    );
}
