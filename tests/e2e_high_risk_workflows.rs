//! E2E: High-risk workflow scenarios (bd-1f42.8.5.2).
//!
//! These tests cover failure/error/edge paths within the covered workflows
//! in the scenario matrix. Each test targets a specific gap:
//!
//! - Provider stream errors and partial failures
//! - Agent loop resilience (max-turns, tool failures, error cascades)
//! - Session corruption recovery and JSONL edge cases
//! - CLI error handling for config/arg edge cases
//!
//! All tests use deterministic in-process providers (no network, no VCR).
//!
//! Run:
//! ```bash
//! cargo test --test e2e_high_risk_workflows
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::similar_names)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::missing_const_for_fn)]

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─── Helpers ─────────────────────────────────────────────────────────────────

const fn tool_names() -> [&'static str; 7] {
    ["read", "write", "edit", "bash", "grep", "find", "ls"]
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>()
}

fn make_assistant(
    provider_name: &str,
    stop_reason: StopReason,
    content: Vec<ContentBlock>,
    total_tokens: u64,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test-api".to_string(),
        provider: provider_name.to_string(),
        model: "test-model".to_string(),
        usage: Usage {
            total_tokens,
            output: total_tokens,
            ..Usage::default()
        },
        stop_reason,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    }
}

fn stream_done(msg: AssistantMessage) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
    let partial = AssistantMessage {
        content: Vec::new(),
        api: msg.api.clone(),
        provider: msg.provider.clone(),
        model: msg.model.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    };
    Box::pin(futures::stream::iter(vec![
        Ok(StreamEvent::Start { partial }),
        Ok(StreamEvent::Done {
            reason: msg.stop_reason,
            message: msg,
        }),
    ]))
}

fn make_agent_session(
    cwd: &Path,
    provider: Arc<dyn Provider>,
    session: Arc<asupersync::sync::Mutex<Session>>,
    max_tool_iterations: usize,
) -> AgentSession {
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&tool_names(), cwd, None),
        AgentConfig {
            max_tool_iterations,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    AgentSession::new(agent, session, true, ResolvedCompactionSettings::default())
}

#[derive(Debug, Default)]
struct EventCapture {
    timeline: Vec<serde_json::Value>,
    tool_starts: usize,
    tool_ends: usize,
    turn_count: usize,
    _errors: Vec<String>,
}

const fn event_label(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart { .. } => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_end",
        AgentEvent::AutoCompactionStart { .. } => "auto_compaction_start",
        AgentEvent::AutoCompactionEnd { .. } => "auto_compaction_end",
        AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
        AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
        AgentEvent::FailoverStart { .. } => "failover_start",
        AgentEvent::FailoverEnd { .. } => "failover_end",
        AgentEvent::ExtensionError { .. } => "extension_error",
    }
}

fn write_jsonl_artifacts(harness: &TestHarness, test_name: &str) {
    let log_path = harness.temp_path(format!("{test_name}.log.jsonl"));
    harness
        .write_jsonl_logs(&log_path)
        .expect("write jsonl logs");
    harness.record_artifact(format!("{test_name}.log.jsonl"), &log_path);

    let artifacts_path = harness.temp_path(format!("{test_name}.artifacts.jsonl"));
    harness
        .write_artifact_index_jsonl(&artifacts_path)
        .expect("write artifact index");
    harness.record_artifact(format!("{test_name}.artifacts.jsonl"), &artifacts_path);
}

fn cli_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

fn isolated_cli_env(harness: &TestHarness) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let env_root = harness.temp_path("pi-env");
    let _ = std::fs::create_dir_all(&env_root);

    env.insert(
        "PI_CODING_AGENT_DIR".to_string(),
        env_root.join("agent").display().to_string(),
    );
    env.insert(
        "PI_CONFIG_PATH".to_string(),
        env_root.join("settings.json").display().to_string(),
    );
    env.insert(
        "PI_SESSIONS_DIR".to_string(),
        env_root.join("sessions").display().to_string(),
    );
    env.insert(
        "PI_PACKAGE_DIR".to_string(),
        env_root.join("packages").display().to_string(),
    );
    env.insert("PI_TEST_MODE".to_string(), "1".to_string());
    env
}

const CLI_TIMEOUT_SECS: u64 = 60;

struct CliResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn run_cli(
    harness: &TestHarness,
    env: &BTreeMap<String, String>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> CliResult {
    harness
        .log()
        .info("action", format!("Running CLI: {}", args.join(" ")));

    let mut command = Command::new(cli_binary_path());
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("OPENAI_API_KEY");
    command.env_remove("GEMINI_API_KEY");
    command.env_remove("GROQ_API_KEY");
    command.env_remove("KIMI_API_KEY");
    command.env_remove("AZURE_OPENAI_API_KEY");
    command.env_remove("PI_OPENROUTER_API_KEY");
    command.env_remove("PI_AWS_ACCESS_KEY_ID");
    command
        .args(args)
        .envs(env.clone())
        .current_dir(harness.temp_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let start = Instant::now();
    let mut child = command.spawn().expect("run pi");
    let mut child_stdout = child.stdout.take().expect("child stdout piped");
    let mut child_stderr = child.stderr.take().expect("child stderr piped");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut child_stdout, &mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut child_stderr, &mut buf);
        buf
    });

    if let Some(input) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(input).expect("write stdin");
    }

    let timeout = Duration::from_secs(CLI_TIMEOUT_SECS);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => panic!("try_wait failed: {err}"),
        }
        if start.elapsed() > timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().expect("wait child after kill");
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    let exit_code = if timed_out {
        stderr = format!("ERROR: timed out after {timeout:?}\n{stderr}");
        -1
    } else {
        status.code().unwrap_or(-1)
    };

    CliResult {
        exit_code,
        stdout,
        stderr,
    }
}

// ─── Provider: Always errors ────────────────────────────────────────────────

/// Provider that always returns an error on stream().
#[derive(Debug)]
struct ErrorOnStreamProvider {
    error_message: String,
    call_count: AtomicUsize,
}

impl ErrorOnStreamProvider {
    fn new(msg: &str) -> Self {
        Self {
            error_message: msg.to_string(),
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ErrorOnStreamProvider {
    fn name(&self) -> &str {
        "error-provider"
    }
    fn api(&self) -> &str {
        "error-api"
    }
    fn model_id(&self) -> &str {
        "error-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Err(Error::api(&self.error_message))
    }
}

// ─── Provider: Error in stream items ────────────────────────────────────────

/// Provider that starts a stream but yields an error mid-stream.
#[derive(Debug)]
struct MidStreamErrorProvider {
    call_count: AtomicUsize,
}

impl MidStreamErrorProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MidStreamErrorProvider {
    fn name(&self) -> &str {
        "midstream-error-provider"
    }
    fn api(&self) -> &str {
        "midstream-api"
    }
    fn model_id(&self) -> &str {
        "midstream-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let partial = make_assistant("midstream-error-provider", StopReason::Stop, Vec::new(), 0);
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Err(Error::api("connection reset during streaming")),
        ])))
    }
}

// ─── Provider: Infinite tool loop ───────────────────────────────────────────

/// Provider that keeps requesting tool calls forever, to test max_tool_iterations.
#[derive(Debug)]
struct InfiniteToolProvider {
    call_count: AtomicUsize,
}

impl InfiniteToolProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for InfiniteToolProvider {
    fn name(&self) -> &str {
        "infinite-tool-provider"
    }
    fn api(&self) -> &str {
        "infinite-api"
    }
    fn model_id(&self) -> &str {
        "infinite-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let msg = make_assistant(
            "infinite-tool-provider",
            StopReason::ToolUse,
            vec![ContentBlock::ToolCall(ToolCall {
                id: format!("loop-tool-{index}"),
                name: "bash".to_string(),
                arguments: json!({ "command": "echo iteration" }),
                thought_signature: None,
            })],
            10,
        );
        Ok(stream_done(msg))
    }
}

// ─── Provider: StopReason::Length ────────────────────────────────────────────

/// Provider that returns `StopReason::Length` (max tokens exhausted).
#[derive(Debug)]
struct MaxTokensProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MaxTokensProvider {
    fn name(&self) -> &str {
        "max-tokens-provider"
    }
    fn api(&self) -> &str {
        "max-tokens-api"
    }
    fn model_id(&self) -> &str {
        "max-tokens-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let msg = make_assistant(
            "max-tokens-provider",
            StopReason::Length,
            vec![ContentBlock::Text(TextContent::new(
                "I was cut off mid-sentence because I ran out of tok",
            ))],
            4096,
        );
        Ok(stream_done(msg))
    }
}

// ─── Provider: Multiple error tool calls ────────────────────────────────────

/// Provider that returns multiple tool calls where one references a nonexistent tool.
#[derive(Debug)]
struct MixedToolErrorProvider {
    call_count: AtomicUsize,
}

impl MixedToolErrorProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MixedToolErrorProvider {
    fn name(&self) -> &str {
        "mixed-tool-provider"
    }
    fn api(&self) -> &str {
        "mixed-tool-api"
    }
    fn model_id(&self) -> &str {
        "mixed-tool-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);

        if index == 0 {
            // First turn: emit one good tool call + one bad tool call
            let msg = make_assistant(
                "mixed-tool-provider",
                StopReason::ToolUse,
                vec![
                    ContentBlock::ToolCall(ToolCall {
                        id: "good-1".to_string(),
                        name: "bash".to_string(),
                        arguments: json!({ "command": "echo ok" }),
                        thought_signature: None,
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "bad-1".to_string(),
                        name: "nonexistent_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    }),
                ],
                25,
            );
            return Ok(stream_done(msg));
        }

        if index == 1 {
            // Second turn: verify we got results from both
            let has_results = context
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult(r) => Some(r),
                    _ => None,
                })
                .count();
            let msg = make_assistant(
                "mixed-tool-provider",
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(format!(
                    "got {has_results} tool results including errors"
                )))],
                12,
            );
            return Ok(stream_done(msg));
        }

        Err(Error::api("unexpected call"))
    }
}

// ─── Provider: Empty response ───────────────────────────────────────────────

/// Provider that returns an empty content response.
#[derive(Debug)]
struct EmptyResponseProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for EmptyResponseProvider {
    fn name(&self) -> &str {
        "empty-response-provider"
    }
    fn api(&self) -> &str {
        "empty-api"
    }
    fn model_id(&self) -> &str {
        "empty-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let msg = make_assistant("empty-response-provider", StopReason::Stop, Vec::new(), 5);
        Ok(stream_done(msg))
    }
}

// ─── Tests: Provider stream error paths ─────────────────────────────────────

/// Provider returns error immediately on stream() — agent loop should surface the error.
#[test]
fn provider_error_on_stream_surfaces_to_caller() {
    let test_name = "e2e_hr_provider_error_on_stream";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> =
                Arc::new(ErrorOnStreamProvider::new("401 Unauthorized"));
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session.run_text("hello".to_string(), |_| {}).await
        }
    });

    assert!(result.is_err(), "Expected error from provider");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("401") || err_msg.contains("Unauthorized"),
        "Error should contain provider error details: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("Provider error surfaced: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// Provider starts stream but yields error mid-stream — agent should handle gracefully.
#[test]
fn provider_mid_stream_error_handled_gracefully() {
    let test_name = "e2e_hr_mid_stream_error";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(MidStreamErrorProvider::new());
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("tell me something".to_string(), |_| {})
                .await
        }
    });

    let assistant_message =
        result.expect("Expected Ok from mid-stream failure with partial recovery");
    assert_eq!(assistant_message.stop_reason, StopReason::Error);
    let err_msg = assistant_message
        .error_message
        .expect("Expected error message");
    assert!(
        err_msg.contains("connection reset") || err_msg.contains("streaming"),
        "Error should describe the streaming failure: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("Mid-stream error handled: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// Provider returns empty content — agent should not crash.
#[test]
fn provider_empty_response_does_not_crash() {
    let test_name = "e2e_hr_empty_response";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(EmptyResponseProvider);
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session.run_text("hello".to_string(), |_| {}).await
        }
    });

    // Empty response should still succeed (just no text content)
    let message = result.expect("empty response should not error");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(
        assistant_text(&message).is_empty(),
        "Expected no text content"
    );

    harness.log().info("result", "Empty response handled");
    write_jsonl_artifacts(&harness, test_name);
}

/// Provider returns `StopReason::Length` — verify it's surfaced correctly.
#[test]
fn provider_max_tokens_stop_reason_surfaced() {
    let test_name = "e2e_hr_max_tokens_stop";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(MaxTokensProvider);
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("write a long essay".to_string(), |_| {})
                .await
                .expect("max tokens should not error")
        }
    });

    assert_eq!(
        message.stop_reason,
        StopReason::Length,
        "Expected StopReason::Length"
    );
    assert!(
        !assistant_text(&message).is_empty(),
        "Should have partial text content"
    );

    harness
        .log()
        .info("result", "MaxTokens stop reason surfaced");
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: Agent loop resilience ───────────────────────────────────────────

/// Tool calls in infinite loop should be bounded by `max_tool_iterations`.
#[test]
fn agent_loop_max_tool_iterations_enforced() {
    let test_name = "e2e_hr_max_tool_iterations";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let result = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(InfiniteToolProvider::new());
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            // Set max_tool_iterations to 3 — after 3 tool rounds the agent should stop
            let mut agent_session = make_agent_session(&cwd, provider, session, 3);
            agent_session
                .run_text("keep calling tools".to_string(), move |event| {
                    let mut guard = capture_ref.lock().expect("lock capture");
                    match &event {
                        AgentEvent::TurnStart { .. } => guard.turn_count += 1,
                        AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                        AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                        _ => {}
                    }
                    guard.timeline.push(json!({
                        "event": event_label(&event),
                    }));
                })
                .await
        }
    });

    // The agent should eventually terminate (either with error or a forced stop)
    let cap = capture.lock().expect("lock capture");
    harness
        .log()
        .info_ctx("summary", "max iterations test", |ctx| {
            ctx.push(("turn_count".into(), cap.turn_count.to_string()));
            ctx.push(("tool_starts".into(), cap.tool_starts.to_string()));
            ctx.push(("tool_ends".into(), cap.tool_ends.to_string()));
            ctx.push(("result_is_ok".into(), result.is_ok().to_string()));
        });

    // Should have executed tools but eventually stopped
    assert!(
        cap.tool_starts <= 4,
        "Should not exceed max_tool_iterations + 1 tool calls, got {}",
        cap.tool_starts
    );
    assert!(cap.tool_starts >= 1, "Should have executed at least 1 tool");

    write_jsonl_artifacts(&harness, test_name);
}

/// Mixed tool batch: one good tool + one bad tool name — verify error handling.
#[test]
fn agent_loop_mixed_tool_success_and_error() {
    let test_name = "e2e_hr_mixed_tool_error";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let message = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(MixedToolErrorProvider::new());
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 6);
            agent_session
                .run_text("call tools".to_string(), move |event| {
                    let mut guard = capture_ref.lock().expect("lock capture");
                    match &event {
                        AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                        AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                        _ => {}
                    }
                })
                .await
                .expect("mixed tools should complete")
        }
    });

    let cap = capture.lock().expect("lock capture");
    assert_eq!(
        cap.tool_starts, 2,
        "Expected 2 tool executions (1 good + 1 bad)"
    );
    assert_eq!(cap.tool_ends, 2, "Both tools should have ended");
    assert_eq!(message.stop_reason, StopReason::Stop);
    let text = assistant_text(&message);
    assert!(
        text.contains("tool results"),
        "Response should mention tool results: {text}"
    );

    harness.log().info("result", "Mixed tool errors handled");
    write_jsonl_artifacts(&harness, test_name);
}

/// Agent session events have proper lifecycle ordering.
#[test]
fn agent_event_lifecycle_ordering() {
    let test_name = "e2e_hr_event_lifecycle";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let events_ref = Arc::clone(&events);

    run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(EmptyResponseProvider);
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("hello".to_string(), move |event| {
                    let mut guard = events_ref.lock().expect("lock events");
                    guard.push(event_label(&event).to_string());
                })
                .await
                .expect("should complete")
        }
    });

    let event_list = events.lock().expect("lock events");
    // Must start with agent_start and end with agent_end
    assert!(
        !event_list.is_empty(),
        "Should have received at least some events"
    );
    assert_eq!(
        event_list.first().map(String::as_str),
        Some("agent_start"),
        "First event should be agent_start"
    );
    assert_eq!(
        event_list.last().map(String::as_str),
        Some("agent_end"),
        "Last event should be agent_end"
    );

    // turn_start must come before turn_end
    let start_idx = event_list.iter().position(|e| e == "turn_start");
    let end_idx = event_list.iter().position(|e| e == "turn_end");
    if let (Some(start), Some(end)) = (start_idx, end_idx) {
        assert!(
            start < end,
            "turn_start ({start}) must come before turn_end ({end})"
        );
    }

    harness
        .log()
        .info("result", format!("Event sequence: {:?}", *event_list));
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: Session JSONL corruption recovery ───────────────────────────────

/// Session with corrupted JSONL lines should skip bad entries and recover.
#[test]
fn session_corrupted_jsonl_skips_bad_entries() {
    let test_name = "e2e_hr_session_corrupted_jsonl";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/corrupted.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }

    // Write a valid header + one valid entry + one corrupted entry + one valid entry
    let header = json!({
        "type": "session",
        "version": 3,
        "id": "test-corrupted-123",
        "timestamp": "2026-02-13T00:00:00.000Z",
        "cwd": harness.temp_dir().display().to_string()
    });
    let valid_entry_1 = json!({
        "type": "message",
        "id": "entry-1",
        "timestamp": "2026-02-13T00:00:01.000Z",
        "message": {
            "role": "user",
            "content": "first message"
        }
    });
    let corrupted_line = "this is not valid json {{{";
    let valid_entry_2 = json!({
        "type": "message",
        "id": "entry-2",
        "parentId": "entry-1",
        "timestamp": "2026-02-13T00:00:02.000Z",
        "message": {
            "role": "user",
            "content": "second message"
        }
    });

    let content = format!(
        "{}\n{}\n{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&valid_entry_1).unwrap(),
        corrupted_line,
        serde_json::to_string(&valid_entry_2).unwrap()
    );
    std::fs::write(&session_path, content).expect("write corrupted session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    let (session, diagnostics) = result.expect("session should open despite corruption");
    assert_eq!(
        diagnostics.skipped_entries.len(),
        1,
        "Should have 1 skipped entry"
    );
    assert!(
        diagnostics.skipped_entries[0].error.contains("expected"),
        "Skipped entry error should mention parse failure"
    );
    // Both valid entries should have been loaded
    let messages = session.to_messages_for_current_path();
    assert!(
        !messages.is_empty(),
        "Should have loaded at least the valid entries"
    );

    harness
        .log()
        .info_ctx("result", "Corrupted session recovery", |ctx| {
            ctx.push((
                "skipped_entries".into(),
                diagnostics.skipped_entries.len().to_string(),
            ));
            ctx.push((
                "orphaned_links".into(),
                diagnostics.orphaned_parent_links.len().to_string(),
            ));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// Session file with only a header and no entries should open as empty.
#[test]
fn session_header_only_opens_as_empty() {
    let test_name = "e2e_hr_session_header_only";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/header_only.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }

    let header = json!({
        "type": "session",
        "version": 3,
        "id": "test-header-only",
        "timestamp": "2026-02-13T00:00:00.000Z",
        "cwd": harness.temp_dir().display().to_string()
    });
    std::fs::write(
        &session_path,
        format!("{}\n", serde_json::to_string(&header).unwrap()),
    )
    .expect("write header-only session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    let (session, diagnostics) = result.expect("header-only session should open");
    assert!(diagnostics.skipped_entries.is_empty());
    assert!(diagnostics.orphaned_parent_links.is_empty());
    let messages = session.to_messages_for_current_path();
    assert!(messages.is_empty(), "Header-only session has no messages");

    harness.log().info("result", "Header-only session opened");
    write_jsonl_artifacts(&harness, test_name);
}

/// Session file that doesn't exist should return `SessionNotFound`.
#[test]
fn session_nonexistent_file_returns_error() {
    let test_name = "e2e_hr_session_nonexistent";
    let harness = TestHarness::new(test_name);

    let result = run_async(async {
        Session::open_with_diagnostics("/tmp/nonexistent_session_a7b3c9d2.jsonl").await
    });

    assert!(result.is_err(), "Should error for nonexistent file");
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("not found") || err_msg.contains("No such"),
        "Error should indicate file not found: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("Nonexistent session error: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// Empty session file should return an error (no header).
#[test]
fn session_empty_file_returns_error() {
    let test_name = "e2e_hr_session_empty_file";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/empty.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }
    std::fs::write(&session_path, "").expect("write empty session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    assert!(result.is_err(), "Empty session file should error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Empty") || err_msg.contains("header"),
        "Should indicate empty/invalid file: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("Empty session error: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// Session with orphaned parent links should report diagnostics.
#[test]
fn session_orphaned_parent_links_reported() {
    let test_name = "e2e_hr_session_orphaned_links";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/orphaned.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }

    let header = json!({
        "type": "session",
        "version": 3,
        "id": "test-orphaned",
        "timestamp": "2026-02-13T00:00:00.000Z",
        "cwd": harness.temp_dir().display().to_string()
    });
    // Entry that references a parent_id that doesn't exist
    let orphan_entry = json!({
        "type": "message",
        "id": "entry-orphan",
        "parentId": "missing-parent-id",
        "timestamp": "2026-02-13T00:00:01.000Z",
        "message": {
            "role": "user",
            "content": "orphaned message"
        }
    });

    let content = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&orphan_entry).unwrap()
    );
    std::fs::write(&session_path, content).expect("write orphaned session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    let (_, diagnostics) = result.expect("session should open despite orphans");
    assert!(
        !diagnostics.orphaned_parent_links.is_empty(),
        "Should report orphaned parent links"
    );
    assert_eq!(
        diagnostics.orphaned_parent_links[0].missing_parent_id,
        "missing-parent-id"
    );

    harness
        .log()
        .info_ctx("result", "Orphaned links detected", |ctx| {
            ctx.push((
                "orphan_count".into(),
                diagnostics.orphaned_parent_links.len().to_string(),
            ));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// Session with invalid JSON header should error with descriptive message.
#[test]
fn session_invalid_header_returns_descriptive_error() {
    let test_name = "e2e_hr_session_invalid_header";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/bad_header.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }
    std::fs::write(&session_path, "not a json header\n").expect("write bad header session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    assert!(result.is_err(), "Invalid header should error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("header") || err_msg.contains("Invalid"),
        "Error should mention invalid header: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("Invalid header error: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: Session persistence round-trip with agent ───────────────────────

/// Persist a session, reload it, and verify messages survive.
#[test]
fn session_persist_reload_messages_survive() {
    let test_name = "e2e_hr_session_persist_reload";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();

    // Step 1: Create session, run agent, persist
    let session_path = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(EmptyResponseProvider);
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, Arc::clone(&session), 4);
            let _ = agent_session
                .run_text("persist me".to_string(), |_| {})
                .await
                .expect("first run");
            agent_session
                .persist_session()
                .await
                .expect("persist session");

            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.path.clone().expect("session has path")
        }
    });

    assert!(session_path.exists(), "Session file should exist");

    // Step 2: Reload and verify
    let (reloaded, diagnostics) = run_async({
        let path_str = session_path.display().to_string();
        async move {
            Session::open_with_diagnostics(&path_str)
                .await
                .expect("reload session")
        }
    });

    assert!(
        diagnostics.skipped_entries.is_empty(),
        "No corruption expected"
    );
    let messages = reloaded.to_messages_for_current_path();
    assert!(
        messages.len() >= 2,
        "Should have at least user + assistant messages, got {}",
        messages.len()
    );

    // Verify user message content survived
    let has_user_msg = messages.iter().any(|m| match m {
        Message::User(u) => match &u.content {
            pi::model::UserContent::Text(t) => t.contains("persist me"),
            pi::model::UserContent::Blocks(blocks) => blocks.iter().any(|b| match b {
                ContentBlock::Text(t) => t.text.contains("persist me"),
                _ => false,
            }),
        },
        _ => false,
    });
    assert!(has_user_msg, "User message should survive persist/reload");

    harness
        .log()
        .info_ctx("result", "Session persist/reload verified", |ctx| {
            ctx.push(("message_count".into(), messages.len().to_string()));
            ctx.push(("session_path".into(), session_path.display().to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: CLI error handling ──────────────────────────────────────────────

/// CLI with conflicting flags should exit with an error.
#[test]
fn cli_conflicting_flags_error() {
    let test_name = "e2e_hr_cli_conflicting_flags";
    let harness = TestHarness::new(test_name);
    let env = isolated_cli_env(&harness);

    // --rpc and --print cannot be combined (both are output modes)
    let result = run_cli(&harness, &env, &["--rpc", "--print", "hello"], None);

    harness
        .log()
        .info_ctx("result", "CLI conflicting flags", |ctx| {
            ctx.push(("exit_code".into(), result.exit_code.to_string()));
            ctx.push(("stderr_len".into(), result.stderr.len().to_string()));
        });

    // Should exit with non-zero
    assert_ne!(
        result.exit_code, 0,
        "Conflicting flags should error. stderr: {}",
        result.stderr
    );

    write_jsonl_artifacts(&harness, test_name);
}

/// CLI with completely invalid model ID should error before streaming.
#[test]
fn cli_invalid_model_id_errors_before_streaming() {
    let test_name = "e2e_hr_cli_invalid_model";
    let harness = TestHarness::new(test_name);
    let env = isolated_cli_env(&harness);

    let result = run_cli(
        &harness,
        &env,
        &[
            "--print",
            "--model",
            "",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--thinking",
            "off",
            "hello",
        ],
        None,
    );

    harness
        .log()
        .info_ctx("result", "CLI invalid model", |ctx| {
            ctx.push(("exit_code".into(), result.exit_code.to_string()));
        });

    // Empty model ID should trigger an error
    assert_ne!(
        result.exit_code, 0,
        "Empty model should error. stdout: {}, stderr: {}",
        result.stdout, result.stderr
    );

    write_jsonl_artifacts(&harness, test_name);
}

/// CLI with missing API key should fail with clear error message.
#[test]
fn cli_missing_api_key_clear_error() {
    let test_name = "e2e_hr_cli_no_api_key";
    let harness = TestHarness::new(test_name);
    let mut env = isolated_cli_env(&harness);

    // Ensure no API keys are set
    env.insert("ANTHROPIC_API_KEY".to_string(), String::new());
    env.insert("OPENAI_API_KEY".to_string(), String::new());
    env.insert("PI_API_KEY".to_string(), String::new());

    let result = run_cli(
        &harness,
        &env,
        &[
            "--print",
            "--provider",
            "anthropic",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--thinking",
            "off",
            "hello",
        ],
        None,
    );

    let output = format!("{}{}", result.stdout, result.stderr);
    harness.log().info_ctx("result", "CLI no API key", |ctx| {
        ctx.push(("exit_code".into(), result.exit_code.to_string()));
        ctx.push(("output_len".into(), output.len().to_string()));
    });

    assert_ne!(result.exit_code, 0, "Missing API key should error");
    // Should mention API key or authentication in error
    assert!(
        output.contains("API")
            || output.contains("api_key")
            || output.contains("key")
            || output.contains("auth")
            || output.contains("credential"),
        "Error should mention API key requirement: {output}"
    );

    write_jsonl_artifacts(&harness, test_name);
}

/// CLI with unknown provider should error.
#[test]
fn cli_unknown_provider_errors() {
    let test_name = "e2e_hr_cli_unknown_provider";
    let harness = TestHarness::new(test_name);
    let env = isolated_cli_env(&harness);

    let result = run_cli(
        &harness,
        &env,
        &[
            "--print",
            "--provider",
            "nonexistent-provider-xyz",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--thinking",
            "off",
            "hello",
        ],
        None,
    );

    harness
        .log()
        .info_ctx("result", "CLI unknown provider", |ctx| {
            ctx.push(("exit_code".into(), result.exit_code.to_string()));
        });

    assert_ne!(
        result.exit_code, 0,
        "Unknown provider should error. stderr: {}",
        result.stderr
    );

    write_jsonl_artifacts(&harness, test_name);
}

/// CLI `--version` should succeed and print version string.
#[test]
fn cli_version_flag_succeeds() {
    let test_name = "e2e_hr_cli_version";
    let harness = TestHarness::new(test_name);
    let env = isolated_cli_env(&harness);

    let result = run_cli(&harness, &env, &["--version"], None);

    assert_eq!(
        result.exit_code, 0,
        "--version should succeed. stderr: {}",
        result.stderr
    );
    assert!(
        !result.stdout.trim().is_empty(),
        "Should print version info"
    );

    harness
        .log()
        .info("result", format!("Version: {}", result.stdout.trim()));
    write_jsonl_artifacts(&harness, test_name);
}

/// CLI `--help` should succeed and contain expected sections.
#[test]
fn cli_help_flag_contains_expected_sections() {
    let test_name = "e2e_hr_cli_help";
    let harness = TestHarness::new(test_name);
    let env = isolated_cli_env(&harness);

    let result = run_cli(&harness, &env, &["--help"], None);

    assert_eq!(
        result.exit_code, 0,
        "--help should succeed. stderr: {}",
        result.stderr
    );
    let help_text = result.stdout.to_lowercase();
    assert!(
        help_text.contains("usage")
            || help_text.contains("options")
            || help_text.contains("arguments"),
        "Help should contain usage info"
    );

    harness.log().info("result", "Help text verified");
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: Agent session with tool errors ──────────────────────────────────

/// Tool call with invalid JSON arguments should be handled gracefully.
#[derive(Debug)]
struct InvalidToolArgsProvider {
    call_count: AtomicUsize,
}

impl InvalidToolArgsProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for InvalidToolArgsProvider {
    fn name(&self) -> &str {
        "invalid-args-provider"
    }
    fn api(&self) -> &str {
        "invalid-args-api"
    }
    fn model_id(&self) -> &str {
        "invalid-args-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            // Pass arguments that don't match the expected schema for "read" tool
            let msg = make_assistant(
                "invalid-args-provider",
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "bad-args-1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "nonexistent_param": 42 }),
                    thought_signature: None,
                })],
                15,
            );
            return Ok(stream_done(msg));
        }
        if index == 1 {
            let msg = make_assistant(
                "invalid-args-provider",
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(
                    "handled invalid tool args",
                ))],
                10,
            );
            return Ok(stream_done(msg));
        }
        Err(Error::api("unexpected call"))
    }
}

#[test]
fn agent_tool_invalid_arguments_handled() {
    let test_name = "e2e_hr_tool_invalid_args";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(InvalidToolArgsProvider::new());
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 6);
            agent_session
                .run_text("read with bad args".to_string(), |_| {})
                .await
                .expect("should complete despite bad args")
        }
    });

    assert_eq!(message.stop_reason, StopReason::Stop);
    let text = assistant_text(&message);
    assert!(
        text.contains("handled"),
        "Should have recovered from bad tool args: {text}"
    );

    harness.log().info("result", "Invalid tool args handled");
    write_jsonl_artifacts(&harness, test_name);
}

/// Tool reading a nonexistent file should surface error to the agent.
#[test]
fn agent_tool_read_nonexistent_file_surfaces_error() {
    let test_name = "e2e_hr_tool_read_nonexistent";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    #[derive(Debug)]
    struct ReadMissingFileProvider {
        call_count: AtomicUsize,
        missing_path: String,
    }
    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ReadMissingFileProvider {
        fn name(&self) -> &str {
            "read-missing-provider"
        }
        fn api(&self) -> &str {
            "read-missing-api"
        }
        fn model_id(&self) -> &str {
            "read-missing-model"
        }
        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let index = self.call_count.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                let msg = make_assistant(
                    "read-missing-provider",
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "read-missing".to_string(),
                        name: "read".to_string(),
                        arguments: json!({ "path": self.missing_path }),
                        thought_signature: None,
                    })],
                    15,
                );
                return Ok(stream_done(msg));
            }
            if index == 1 {
                // Check that error was reported in tool result
                let has_error = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "read-missing" => Some(r),
                        _ => None,
                    })
                    .any(|r| r.is_error);
                let msg = make_assistant(
                    "read-missing-provider",
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "file error detected: {has_error}"
                    )))],
                    10,
                );
                return Ok(stream_done(msg));
            }
            Err(Error::api("unexpected call"))
        }
    }

    let missing_path = harness
        .temp_path("nonexistent_dir/ghost_file.txt")
        .display()
        .to_string();

    let message = run_async({
        let cwd = cwd.clone();
        let missing_path = missing_path.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ReadMissingFileProvider {
                call_count: AtomicUsize::new(0),
                missing_path,
            });
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 6);
            agent_session
                .run_text("read a missing file".to_string(), move |event| {
                    let mut guard = capture_ref.lock().expect("lock capture");
                    match &event {
                        AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                        AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                        _ => {}
                    }
                })
                .await
                .expect("should complete")
        }
    });

    let cap = capture.lock().expect("lock capture");
    assert_eq!(cap.tool_starts, 1, "Should have 1 tool execution");
    assert_eq!(cap.tool_ends, 1, "Tool should have ended");

    let text = assistant_text(&message);
    assert!(
        text.contains("true"),
        "Agent should see file read error: {text}"
    );

    harness.log().info("result", "Missing file error surfaced");
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tests: Session unicode/special character resilience ────────────────────

/// Session with unicode characters in messages should round-trip correctly.
#[test]
fn session_unicode_messages_round_trip() {
    let test_name = "e2e_hr_session_unicode";
    let harness = TestHarness::new(test_name);

    let session_path = harness.temp_path("sessions/unicode.jsonl");
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent).expect("create session dir");
    }

    let unicode_text = "Hello\u{1F600} \u{4E16}\u{754C} \u{1F680}\u{2764}\u{FE0F}"; // emoji, CJK, rocket, heart
    let header = json!({
        "type": "session",
        "version": 3,
        "id": "test-unicode-session",
        "timestamp": "2026-02-13T00:00:00.000Z",
        "cwd": harness.temp_dir().display().to_string()
    });
    let entry = json!({
        "type": "message",
        "id": "entry-unicode",
        "timestamp": "2026-02-13T00:00:01.000Z",
        "message": {
            "role": "user",
            "content": unicode_text
        }
    });

    let content = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&entry).unwrap()
    );
    std::fs::write(&session_path, content).expect("write unicode session");

    let result = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await }
    });

    let (session, diagnostics) = result.expect("unicode session should open");
    assert!(diagnostics.skipped_entries.is_empty());
    let messages = session.to_messages_for_current_path();
    assert!(!messages.is_empty(), "Should have loaded unicode message");

    // Verify unicode survived
    let has_unicode = messages.iter().any(|m| match m {
        Message::User(u) => match &u.content {
            pi::model::UserContent::Text(t) => t.contains('\u{1F600}'),
            pi::model::UserContent::Blocks(blocks) => blocks.iter().any(|b| match b {
                ContentBlock::Text(t) => t.text.contains('\u{1F600}'),
                _ => false,
            }),
        },
        _ => false,
    });
    assert!(has_unicode, "Unicode characters should survive round-trip");

    harness
        .log()
        .info("result", "Unicode session round-trip OK");
    write_jsonl_artifacts(&harness, test_name);
}
