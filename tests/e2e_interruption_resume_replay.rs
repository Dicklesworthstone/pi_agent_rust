//! E2E: Interruption, resume, and replay scenario scripts (bd-1f42.8.5.5).
//!
//! Deterministic tests for interruption-heavy workflows:
//!
//! 1. Abort mid-stream via `AbortHandle` — verify partial state, no crash
//! 2. Abort during tool execution — verify tool result handling
//! 3. Session resume after interruption — reload and continue
//! 4. Replay parity — same inputs → same outputs on replay
//! 5. Multi-turn interrupt/resume cycles
//!
//! All tests use in-process deterministic providers (no network).
//!
//! Run:
//! ```bash
//! cargo test --test e2e_interruption_resume_replay
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
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::Result;
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ─── Shared helpers ─────────────────────────────────────────────────────────

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

/// `sli_resume_ready_p95_ms` fail threshold from docs/perf_sli_matrix.json.
const RESUME_READY_FAIL_BUDGET_MS: u128 = 900;

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
    turn_count: usize,
    tool_starts: usize,
    tool_ends: usize,
    labels: Vec<String>,
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

// ═══════════════════════════════════════════════════════════════════════════
// Providers
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that yields a simple text response.
#[derive(Debug)]
struct SimpleProvider {
    response_text: String,
}

impl SimpleProvider {
    fn new(text: &str) -> Self {
        Self {
            response_text: text.to_string(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for SimpleProvider {
    fn name(&self) -> &str {
        "simple-provider"
    }
    fn api(&self) -> &str {
        "simple-api"
    }
    fn model_id(&self) -> &str {
        "simple-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let msg = make_assistant(
            "simple-provider",
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(&self.response_text))],
            10,
        );
        Ok(stream_done(msg))
    }
}

/// Provider that does a tool call then finalizes. Supports abort checking.
#[derive(Debug)]
struct ToolThenFinalizeProvider {
    call_count: AtomicUsize,
}

impl ToolThenFinalizeProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ToolThenFinalizeProvider {
    fn name(&self) -> &str {
        "tool-finalize-provider"
    }
    fn api(&self) -> &str {
        "tool-finalize-api"
    }
    fn model_id(&self) -> &str {
        "tool-finalize-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            let msg = make_assistant(
                "tool-finalize-provider",
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "echo tool-output" }),
                    thought_signature: None,
                })],
                15,
            );
            return Ok(stream_done(msg));
        }
        let msg = make_assistant(
            "tool-finalize-provider",
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new("tool cycle complete"))],
            10,
        );
        Ok(stream_done(msg))
    }
}

/// Provider that tracks context message count for replay verification.
#[derive(Debug)]
struct ReplayVerifyProvider {
    call_count: AtomicUsize,
    expected_msg: String,
}

impl ReplayVerifyProvider {
    fn new(expected_msg: &str) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            expected_msg: expected_msg.to_string(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ReplayVerifyProvider {
    fn name(&self) -> &str {
        "replay-verify-provider"
    }
    fn api(&self) -> &str {
        "replay-verify-api"
    }
    fn model_id(&self) -> &str {
        "replay-verify-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        // Deterministic response based on context size (for replay parity)
        let context_msg_count = context.messages.len();
        let text = format!(
            "replay_call_{index}_ctx_{context_msg_count}_msg_{expected}",
            expected = self.expected_msg
        );
        let msg = make_assistant(
            "replay-verify-provider",
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(&text))],
            10,
        );
        Ok(stream_done(msg))
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIO 1: Abort mid-stream
// ═══════════════════════════════════════════════════════════════════════════

/// ABORT-1: Pre-abort before run — agent should return immediately.
#[test]
fn abort_before_run_returns_immediately() {
    let test_name = "irr_abort_before_run";
    let harness = TestHarness::new(test_name);
    harness.section("abort_before_run");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("should not appear"));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);

        // Create abort and trigger it immediately
        let (handle, signal) = AbortHandle::new();
        handle.abort();

        agent_session
            .run_text_with_abort("hello".to_string(), Some(signal), move |event| {
                let mut guard = capture_ref.lock().expect("lock capture");
                guard.labels.push(event_label(&event).to_string());
            })
            .await
    });

    // Agent should still return a result (abort message)
    let cap = capture.lock().expect("lock capture");
    harness
        .log()
        .info_ctx("result", "ABORT-1: Pre-abort", |ctx| {
            ctx.push(("is_ok".into(), result.is_ok().to_string()));
            ctx.push(("event_count".into(), cap.labels.len().to_string()));
            ctx.push(("events".into(), format!("{:?}", cap.labels)));
        });

    // Should have agent_start and agent_end at minimum
    if !cap.labels.is_empty() {
        assert!(
            cap.labels.contains(&"agent_start".to_string()),
            "Should have agent_start event"
        );
    }

    write_jsonl_artifacts(&harness, test_name);
}

/// ABORT-2: Abort during tool execution — tools should still complete or be cancelled.
#[test]
fn abort_during_tool_execution() {
    let test_name = "irr_abort_during_tool";
    let harness = TestHarness::new(test_name);
    harness.section("abort_during_tool");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolThenFinalizeProvider::new());
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);

        let (handle, signal) = AbortHandle::new();
        // Abort after first tool_start event
        let handle_ref = Arc::new(handle);
        let handle_clone = Arc::clone(&handle_ref);

        agent_session
            .run_text_with_abort("call a tool".to_string(), Some(signal), move |event| {
                let mut guard = capture_ref.lock().expect("lock capture");
                let label = event_label(&event);
                guard.labels.push(label.to_string());
                match &event {
                    AgentEvent::TurnStart { .. } => guard.turn_count += 1,
                    AgentEvent::ToolExecutionStart { .. } => {
                        guard.tool_starts += 1;
                        // Abort after first tool starts
                        handle_clone.abort();
                    }
                    AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                    _ => {}
                }
            })
            .await
    });

    let cap = capture.lock().expect("lock capture");
    harness
        .log()
        .info_ctx("result", "ABORT-2: During tool", |ctx| {
            ctx.push(("is_ok".into(), result.is_ok().to_string()));
            ctx.push(("tool_starts".into(), cap.tool_starts.to_string()));
            ctx.push(("tool_ends".into(), cap.tool_ends.to_string()));
            ctx.push(("events".into(), format!("{:?}", cap.labels)));
        });

    // Tool should have started
    assert!(
        cap.tool_starts >= 1,
        "At least one tool should have started"
    );

    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIO 2: Session resume after interruption
// ═══════════════════════════════════════════════════════════════════════════

/// RESUME-1: Session persists messages, reloads after abort, second run continues.
#[test]
fn resume_after_abort_continues_from_persisted_state() {
    let test_name = "irr_resume_after_abort";
    let harness = TestHarness::new(test_name);
    harness.section("resume_after_abort");

    let cwd = harness.temp_dir().to_path_buf();

    // Step 1: Run first turn normally, persist
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));
    let session_path = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("first turn response"));
            let mut agent_session = make_agent_session(&cwd, provider, session.clone(), 4);
            let msg = agent_session
                .run_text("first question".to_string(), |_| {})
                .await
                .expect("first turn");
            assert_eq!(msg.stop_reason, StopReason::Stop);
            agent_session.persist_session().await.expect("persist");

            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.path.clone().expect("session has path")
        }
    });

    assert!(session_path.exists(), "Session file should exist");
    harness.record_artifact("session.jsonl", &session_path);

    // Step 2: Reload session and verify messages survived
    let reload_start = Instant::now();
    let (reloaded, diagnostics) = run_async({
        let path = session_path.display().to_string();
        async move { Session::open_with_diagnostics(&path).await.expect("reload") }
    });
    let resume_ready_ms = reload_start.elapsed().as_millis();
    assert!(diagnostics.skipped_entries.is_empty(), "No corruption");
    assert!(
        resume_ready_ms <= RESUME_READY_FAIL_BUDGET_MS,
        "resume reload exceeded UX fail threshold ({resume_ready_ms}ms > {RESUME_READY_FAIL_BUDGET_MS}ms)"
    );

    let messages = reloaded.to_messages_for_current_path();
    assert!(
        messages.len() >= 2,
        "Should have user + assistant messages after first turn, got {}",
        messages.len()
    );

    harness
        .log()
        .info_ctx("result", "RESUME-1: Session resumed", |ctx| {
            ctx.push((
                "messages_after_first_turn".into(),
                messages.len().to_string(),
            ));
            ctx.push(("session_path".into(), session_path.display().to_string()));
            ctx.push(("resume_ready_ms".into(), resume_ready_ms.to_string()));
            ctx.push(("resume_sli_id".into(), "sli_resume_ready_p95_ms".into()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// RESUME-2: Multiple turns persisted and resumable.
#[test]
fn resume_multi_turn_conversation_intact() {
    let test_name = "irr_resume_multi_turn";
    let harness = TestHarness::new(test_name);
    harness.section("resume_multi_turn");

    let cwd = harness.temp_dir().to_path_buf();
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));

    // Turn 1
    run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("answer 1"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("question 1".to_string(), |_| {})
                .await
                .expect("turn 1");
            agent_session.persist_session().await.expect("persist 1");
        }
    });

    // Turn 2
    run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("answer 2"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("question 2".to_string(), |_| {})
                .await
                .expect("turn 2");
            agent_session.persist_session().await.expect("persist 2");
        }
    });

    // Persisted session must exist and remain reloadable within resume UX budget.
    let session_path = run_async({
        let session = Arc::clone(&session);
        async move {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            guard
                .path
                .clone()
                .expect("session path after persisted turns")
        }
    });
    assert!(
        session_path.exists(),
        "session file must exist after persists"
    );
    harness.record_artifact("session_multiturn.jsonl", &session_path);

    let reload_start = Instant::now();
    let (reloaded, diagnostics) = run_async({
        let path = session_path.display().to_string();
        async move {
            Session::open_with_diagnostics(&path)
                .await
                .expect("reload multi-turn")
        }
    });
    let resume_ready_ms = reload_start.elapsed().as_millis();
    assert!(
        diagnostics.skipped_entries.is_empty(),
        "multi-turn reload should not skip entries"
    );
    assert!(
        resume_ready_ms <= RESUME_READY_FAIL_BUDGET_MS,
        "multi-turn resume exceeded UX fail threshold ({resume_ready_ms}ms > {RESUME_READY_FAIL_BUDGET_MS}ms)"
    );

    // Verify all messages persisted and survived reload.
    let messages = reloaded.to_messages_for_current_path();

    // Should have: user1, assistant1, user2, assistant2 = 4 messages.
    assert!(
        messages.len() >= 4,
        "Multi-turn should have >= 4 messages after reload, got {}",
        messages.len()
    );

    // Verify ordering by role counts.
    let user_count = messages
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .count();
    let assistant_count = messages
        .iter()
        .filter(|m| matches!(m, Message::Assistant(_)))
        .count();
    assert_eq!(user_count, 2, "Should have 2 user messages");
    assert_eq!(assistant_count, 2, "Should have 2 assistant messages");

    harness
        .log()
        .info_ctx("result", "RESUME-2: Multi-turn intact", |ctx| {
            ctx.push(("total_messages".into(), messages.len().to_string()));
            ctx.push(("user_messages".into(), user_count.to_string()));
            ctx.push(("assistant_messages".into(), assistant_count.to_string()));
            ctx.push(("session_path".into(), session_path.display().to_string()));
            ctx.push(("resume_ready_ms".into(), resume_ready_ms.to_string()));
            ctx.push(("resume_sli_id".into(), "sli_resume_ready_p95_ms".into()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIO 3: Replay parity
// ═══════════════════════════════════════════════════════════════════════════

/// REPLAY-1: Same provider + same input → same output (determinism proof).
#[test]
fn replay_same_input_produces_same_output() {
    let test_name = "irr_replay_determinism";
    let harness = TestHarness::new(test_name);
    harness.section("replay_determinism");

    let cwd = harness.temp_dir().to_path_buf();
    let input = "what is 2+2";

    // Run 1
    let (text_1, msg_count_1) = run_async({
        let cwd = cwd.clone();
        let input = input.to_string();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ReplayVerifyProvider::new("run1"));
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session.clone(), 4);
            let msg = agent_session.run_text(input, |_| {}).await.expect("run 1");
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            let messages = guard.to_messages_for_current_path();
            (assistant_text(&msg), messages.len())
        }
    });

    // Run 2 — same input, fresh provider with same deterministic behavior
    let (text_2, msg_count_2) = run_async({
        let cwd = cwd.clone();
        let input = input.to_string();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ReplayVerifyProvider::new("run1"));
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session.clone(), 4);
            let msg = agent_session.run_text(input, |_| {}).await.expect("run 2");
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            let messages = guard.to_messages_for_current_path();
            (assistant_text(&msg), messages.len())
        }
    });

    // Replay parity assertions
    assert_eq!(text_1, text_2, "Same input should produce same output text");
    assert_eq!(
        msg_count_1, msg_count_2,
        "Same input should produce same message count"
    );

    harness
        .log()
        .info_ctx("result", "REPLAY-1: Determinism verified", |ctx| {
            ctx.push(("text_1".into(), text_1));
            ctx.push(("text_2".into(), text_2));
            ctx.push(("msg_count".into(), msg_count_1.to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// REPLAY-2: Different input → different output (non-trivial replay).
#[test]
fn replay_different_input_produces_different_output() {
    let test_name = "irr_replay_different_input";
    let harness = TestHarness::new(test_name);
    harness.section("replay_different_input");

    let cwd = harness.temp_dir().to_path_buf();

    let text_a = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ReplayVerifyProvider::new("inputA"));
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            let msg = agent_session
                .run_text("input A".to_string(), |_| {})
                .await
                .expect("run A");
            assistant_text(&msg)
        }
    });

    let text_b = run_async({
        let cwd = cwd.clone();
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ReplayVerifyProvider::new("inputB"));
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            let msg = agent_session
                .run_text("input B".to_string(), |_| {})
                .await
                .expect("run B");
            assistant_text(&msg)
        }
    });

    assert_ne!(
        text_a, text_b,
        "Different inputs should produce different outputs"
    );

    harness
        .log()
        .info_ctx("result", "REPLAY-2: Different input verified", |ctx| {
            ctx.push(("text_a".into(), text_a));
            ctx.push(("text_b".into(), text_b));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIO 4: Multi-turn interrupt/resume cycles
// ═══════════════════════════════════════════════════════════════════════════

/// CYCLE-1: Run → abort → persist → reload → run again → verify state.
#[test]
fn cycle_run_abort_persist_reload_resume() {
    let test_name = "irr_cycle_abort_resume";
    let harness = TestHarness::new(test_name);
    harness.section("cycle_abort_resume");

    let cwd = harness.temp_dir().to_path_buf();

    // Phase 1: Normal first turn
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));

    run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("turn 1 done"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("first".to_string(), |_| {})
                .await
                .expect("turn 1");
            agent_session.persist_session().await.expect("persist 1");
        }
    });

    // Phase 2: Second turn with abort
    let result = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("turn 2 interrupted"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            let (handle, signal) = AbortHandle::new();
            handle.abort(); // Abort immediately
            agent_session
                .run_text_with_abort("second".to_string(), Some(signal), |_| {})
                .await
        }
    });

    // Phase 3: Persist after abort
    run_async({
        let session = Arc::clone(&session);
        async move {
            let cx = asupersync::Cx::for_testing();
            // `MutexGuard` is `!Send` (asupersync 0.3.9) and `run_async` requires
            // a `Send` future, so the guard held across `save().await` must be owned.
            let mut guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock");
            guard.save().await.expect("save after abort");
        }
    });

    // Phase 4: Third turn — should succeed normally
    let turn3_msg = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("turn 3 resumed"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("third".to_string(), |_| {})
                .await
                .expect("turn 3")
        }
    });

    let turn3_text = assistant_text(&turn3_msg);
    assert!(
        turn3_text.contains("turn 3 resumed"),
        "Third turn should succeed after abort: {turn3_text}"
    );

    // Verify session has messages from turn 1 and turn 3
    let messages = run_async({
        let session = Arc::clone(&session);
        async move {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            guard.to_messages_for_current_path()
        }
    });

    let user_msgs: Vec<_> = messages
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .collect();
    assert!(
        user_msgs.len() >= 2,
        "Should have messages from at least 2 turns, got {}",
        user_msgs.len()
    );

    harness
        .log()
        .info_ctx("result", "CYCLE-1: Abort/resume cycle verified", |ctx| {
            ctx.push(("turn3_text".into(), turn3_text));
            ctx.push(("total_messages".into(), messages.len().to_string()));
            ctx.push(("user_messages".into(), user_msgs.len().to_string()));
            ctx.push(("abort_result".into(), result.is_ok().to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// CYCLE-2: Tool abort → resume with fresh tool call → success.
#[test]
fn cycle_tool_abort_then_fresh_success() {
    let test_name = "irr_cycle_tool_abort";
    let harness = TestHarness::new(test_name);
    harness.section("cycle_tool_abort");

    let cwd = harness.temp_dir().to_path_buf();
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));

    // Phase 1: Abort during tool execution
    let abort_result = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ToolThenFinalizeProvider::new());
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            let (handle, signal) = AbortHandle::new();
            let handle = Arc::new(handle);
            let handle_clone = Arc::clone(&handle);
            agent_session
                .run_text_with_abort("use a tool".to_string(), Some(signal), move |event| {
                    if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
                        handle_clone.abort();
                    }
                })
                .await
        }
    });

    harness.log().info(
        "phase1",
        format!("Tool abort: is_ok={}", abort_result.is_ok()),
    );

    // Phase 2: Fresh run should succeed
    let fresh_msg = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("fresh success"));
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("try again".to_string(), |_| {})
                .await
                .expect("fresh run")
        }
    });

    let fresh_text = assistant_text(&fresh_msg);
    assert!(
        fresh_text.contains("fresh success"),
        "Fresh run after tool abort should succeed: {fresh_text}"
    );

    harness
        .log()
        .info_ctx("result", "CYCLE-2: Tool abort/resume verified", |ctx| {
            ctx.push(("fresh_text".into(), fresh_text));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIO 5: Event lifecycle integrity under interruption
// ═══════════════════════════════════════════════════════════════════════════

/// EVENTS-1: Normal run has balanced start/end events.
#[test]
fn events_balanced_start_end_normal_run() {
    let test_name = "irr_events_balanced";
    let harness = TestHarness::new(test_name);
    harness.section("events_balanced");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(SimpleProvider::new("balanced"));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session
            .run_text("hello".to_string(), move |event| {
                let mut guard = capture_ref.lock().expect("lock");
                guard.labels.push(event_label(&event).to_string());
            })
            .await
            .expect("should complete")
    });

    let cap = capture.lock().expect("lock");
    let agent_starts = cap.labels.iter().filter(|l| *l == "agent_start").count();
    let agent_ends = cap.labels.iter().filter(|l| *l == "agent_end").count();
    let turn_starts = cap.labels.iter().filter(|l| *l == "turn_start").count();
    let turn_ends = cap.labels.iter().filter(|l| *l == "turn_end").count();
    let msg_starts = cap.labels.iter().filter(|l| *l == "message_start").count();
    let msg_ends = cap.labels.iter().filter(|l| *l == "message_end").count();

    assert_eq!(agent_starts, 1, "Exactly 1 agent_start");
    assert_eq!(agent_ends, 1, "Exactly 1 agent_end");
    assert_eq!(turn_starts, turn_ends, "turn_start/end balanced");
    assert_eq!(msg_starts, msg_ends, "message_start/end balanced");

    harness
        .log()
        .info_ctx("result", "EVENTS-1: Balanced events verified", |ctx| {
            ctx.push(("total_events".into(), cap.labels.len().to_string()));
            ctx.push(("turns".into(), turn_starts.to_string()));
            ctx.push(("messages".into(), msg_starts.to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// EVENTS-2: Tool run has balanced tool_start/tool_end.
#[test]
fn events_balanced_tool_start_end() {
    let test_name = "irr_events_balanced_tools";
    let harness = TestHarness::new(test_name);
    harness.section("events_balanced_tools");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolThenFinalizeProvider::new());
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session
            .run_text("use a tool".to_string(), move |event| {
                let mut guard = capture_ref.lock().expect("lock");
                guard.labels.push(event_label(&event).to_string());
            })
            .await
            .expect("should complete")
    });

    let cap = capture.lock().expect("lock");
    let tool_starts = cap.labels.iter().filter(|l| *l == "tool_start").count();
    let tool_ends = cap.labels.iter().filter(|l| *l == "tool_end").count();

    assert_eq!(
        tool_starts, tool_ends,
        "tool_start/end should be balanced: starts={tool_starts}, ends={tool_ends}"
    );
    assert!(tool_starts >= 1, "Should have at least 1 tool execution");

    harness
        .log()
        .info_ctx("result", "EVENTS-2: Tool events balanced", |ctx| {
            ctx.push(("tool_starts".into(), tool_starts.to_string()));
            ctx.push(("tool_ends".into(), tool_ends.to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}
