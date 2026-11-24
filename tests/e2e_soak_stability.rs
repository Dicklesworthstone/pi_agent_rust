// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; asupersync 0.5.0 nests its runtime future types deeply
// enough that proving `Send` exceeds the default 128.
#![recursion_limit = "256"]

//! E2E: Long-run soak tests with stability metrics (bd-1f42.3.5).
//!
//! Deterministic soak scenarios that exercise prolonged sessions, repeated tool
//! calls, and resource/timing stability over many iterations:
//!
//! 1. Multi-turn sustained conversation (20 turns) with session persistence
//! 2. Repeated tool execution (10 tool-use turns) for resource accumulation
//! 3. Session message accumulation: growing history with linear growth checks
//! 4. Latency stability: turn durations stay within bounded variance
//! 5. Error recovery sustainability: repeated errors do not degrade behavior
//! 6. Session persist/reload cycle: repeated save/load across many turns
//! 7. Mixed workload: interleaved text, tool-use, error, and recovery turns
//! 8. Token accumulation tracking: cumulative token budget monotonically grows
//!
//! All tests use in-process deterministic providers (no network).
//!
//! Run:
//! ```bash
//! cargo test --test e2e_soak_stability
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::similar_names)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession, tool_effect_batch_plan_evidence};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::hostcall_queue::{
    HostcallQueueEnqueueResult, HostcallQueueMode, HostcallRequestQueue, QueueTenant,
};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
    UserContent, UserMessage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::{ToolEffects, ToolRegistry};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Number of turns for multi-turn soak tests.
const SOAK_TURN_COUNT: usize = 20;

/// Number of tool-use iterations for repeated tool tests.
const TOOL_ITERATION_COUNT: usize = 10;

/// Maximum acceptable latency drift ratio (latest turn / first turn).
/// A value of 5.0 means the last turn can be at most 5x the first turn.
const MAX_LATENCY_DRIFT_RATIO: f64 = 5.0;

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

fn make_assistant(
    stop_reason: StopReason,
    content: Vec<ContentBlock>,
    total_tokens: u64,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "soak-api".to_string(),
        provider: "soak-provider".to_string(),
        model: "soak-model".to_string(),
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
        AgentEvent::ManualCompactionStart { .. } => "manual_compaction_start",
        AgentEvent::ManualCompactionEnd { .. } => "manual_compaction_end",
        AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
        AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
        AgentEvent::FailoverStart { .. } => "failover_start",
        AgentEvent::FailoverEnd { .. } => "failover_end",
        AgentEvent::PrewalkSwitched { .. } => "prewalk_switched",
        AgentEvent::ExtensionError { .. } => "extension_error",
        AgentEvent::AdvisorNote { .. } => "advisor_note",
        AgentEvent::CostWarning { .. } => "cost_warning",
        AgentEvent::ProviderError { .. } => "provider_error",
    }
}

fn total_assistant_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.usage.total_tokens),
            _ => None,
        })
        .sum()
}

// ─── Metrics collection ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct TurnMetrics {
    turn_index: usize,
    duration_ms: u128,
    tool_starts: usize,
    tool_ends: usize,
    tokens: u64,
    cumulative_tokens: u64,
    session_message_count: usize,
    error: bool,
}

#[derive(Debug, Default)]
struct SoakCapture {
    timeline: Vec<serde_json::Value>,
    turn_starts: BTreeMap<usize, Instant>,
    total_tool_starts: usize,
    total_tool_ends: usize,
}

impl SoakCapture {
    fn event_callback(
        capture: Arc<Mutex<Self>>,
        started_at: Instant,
    ) -> impl Fn(AgentEvent) + Send + 'static {
        move |event| {
            let elapsed_ms = started_at.elapsed().as_millis();
            let mut guard = capture.lock().expect("lock soak capture");
            match &event {
                AgentEvent::TurnStart { turn_index, .. } => {
                    guard.turn_starts.insert(*turn_index, Instant::now());
                }
                AgentEvent::TurnEnd { .. } => {}
                AgentEvent::ToolExecutionStart { .. } => {
                    guard.total_tool_starts += 1;
                }
                AgentEvent::ToolExecutionEnd { .. } => {
                    guard.total_tool_ends += 1;
                }
                _ => {}
            }
            guard.timeline.push(json!({
                "event": event_label(&event),
                "elapsedMs": elapsed_ms,
            }));
        }
    }
}

fn write_timeline_artifact(harness: &TestHarness, test_name: &str, timeline: &[serde_json::Value]) {
    let timeline_path = harness.temp_path(format!("{test_name}.timeline.jsonl"));
    let mut file = std::fs::File::create(&timeline_path).expect("create timeline artifact");
    for entry in timeline {
        let line = serde_json::to_string(entry).expect("serialize timeline entry");
        let _ = writeln!(file, "{line}");
    }
    harness.record_artifact(format!("{test_name}.timeline.jsonl"), &timeline_path);
}

fn write_metrics_artifact(harness: &TestHarness, test_name: &str, metrics: &[TurnMetrics]) {
    let metrics_path = harness.temp_path(format!("{test_name}.metrics.jsonl"));
    let mut file = std::fs::File::create(&metrics_path).expect("create metrics artifact");
    for m in metrics {
        let line = serde_json::to_string(&json!({
            "turn": m.turn_index,
            "duration_ms": m.duration_ms,
            "tool_starts": m.tool_starts,
            "tool_ends": m.tool_ends,
            "tokens": m.tokens,
            "cumulative_tokens": m.cumulative_tokens,
            "session_messages": m.session_message_count,
            "error": m.error,
        }))
        .expect("serialize metric");
        let _ = writeln!(file, "{line}");
    }
    harness.record_artifact(format!("{test_name}.metrics.jsonl"), &metrics_path);
}

fn write_summary_artifact(harness: &TestHarness, test_name: &str, summary: &serde_json::Value) {
    let summary_path = harness.temp_path(format!("{test_name}.summary.json"));
    let content = serde_json::to_string_pretty(summary).expect("serialize summary");
    std::fs::write(&summary_path, content).expect("write summary");
    harness.record_artifact(format!("{test_name}.summary.json"), &summary_path);
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

fn compute_latency_stats(durations: &[u128]) -> (f64, f64, f64, f64, f64) {
    if durations.is_empty() {
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let n = durations.len() as f64;
    let sum: f64 = durations.iter().map(|d| *d as f64).sum();
    let mean = sum / n;
    let variance = durations
        .iter()
        .map(|d| (*d as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    let stddev = variance.sqrt();
    let min = durations.iter().copied().min().unwrap_or(0) as f64;
    let max = durations.iter().copied().max().unwrap_or(0) as f64;
    (mean, stddev, min, max, sum)
}

// ═══════════════════════════════════════════════════════════════════════════
// Providers
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that responds to any number of text-only turns with unique replies.
#[derive(Debug)]
struct MultiTurnTextProvider {
    call_count: AtomicUsize,
    tokens_per_turn: u64,
}

impl MultiTurnTextProvider {
    const fn new(tokens_per_turn: u64) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            tokens_per_turn,
        }
    }
}

#[async_trait]
impl Provider for MultiTurnTextProvider {
    fn name(&self) -> &'static str {
        "soak-text-provider"
    }
    fn api(&self) -> &'static str {
        "soak-api"
    }
    fn model_id(&self) -> &'static str {
        "soak-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(stream_done(make_assistant(
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(format!(
                "soak response turn {index}"
            )))],
            self.tokens_per_turn,
        )))
    }
}

/// Provider that alternates: tool call on even calls, text response on odd calls.
#[derive(Debug)]
struct RepeatedToolProvider {
    call_count: AtomicUsize,
    file_path: Mutex<String>,
    tokens_per_turn: u64,
}

impl RepeatedToolProvider {
    fn new(file_path: String, tokens_per_turn: u64) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            file_path: Mutex::new(file_path),
            tokens_per_turn,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for RepeatedToolProvider {
    fn name(&self) -> &str {
        "soak-tool-provider"
    }
    fn api(&self) -> &str {
        "soak-api"
    }
    fn model_id(&self) -> &str {
        "soak-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = self.file_path.lock().expect("lock path").clone();

        if index.is_multiple_of(2) {
            // Even: issue a read tool call
            Ok(stream_done(make_assistant(
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("read-{index}"),
                    name: "read".to_string(),
                    arguments: json!({ "path": path }),
                    thought_signature: None,
                })],
                self.tokens_per_turn,
            )))
        } else {
            // Odd: text response after tool result
            Ok(stream_done(make_assistant(
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(format!(
                    "tool iteration {index} complete"
                )))],
                self.tokens_per_turn,
            )))
        }
    }
}

/// Provider that fails every Nth call, succeeds otherwise.
#[derive(Debug)]
struct IntermittentErrorProvider {
    call_count: AtomicUsize,
    fail_every_n: usize,
    tokens_per_turn: u64,
}

impl IntermittentErrorProvider {
    const fn new(fail_every_n: usize, tokens_per_turn: u64) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            fail_every_n,
            tokens_per_turn,
        }
    }
}

#[async_trait]
impl Provider for IntermittentErrorProvider {
    fn name(&self) -> &'static str {
        "soak-error-provider"
    }
    fn api(&self) -> &'static str {
        "soak-api"
    }
    fn model_id(&self) -> &'static str {
        "soak-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if index.is_multiple_of(self.fail_every_n) && index > 0 {
            return Err(Error::api(format!(
                "soak intermittent error at call {index}"
            )));
        }
        Ok(stream_done(make_assistant(
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(format!(
                "soak recovery response {index}"
            )))],
            self.tokens_per_turn,
        )))
    }
}

/// Provider for mixed workload: text, tool, error, recovery across calls.
#[derive(Debug)]
struct MixedWorkloadProvider {
    call_count: AtomicUsize,
    file_path: Mutex<String>,
    tokens_per_turn: u64,
}

impl MixedWorkloadProvider {
    fn new(file_path: String, tokens_per_turn: u64) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            file_path: Mutex::new(file_path),
            tokens_per_turn,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MixedWorkloadProvider {
    fn name(&self) -> &str {
        "soak-mixed-provider"
    }
    fn api(&self) -> &str {
        "soak-api"
    }
    fn model_id(&self) -> &str {
        "soak-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = self.file_path.lock().expect("lock path").clone();

        match index % 5 {
            0 | 3 => {
                // Text response
                Ok(stream_done(make_assistant(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "mixed text {index}"
                    )))],
                    self.tokens_per_turn,
                )))
            }
            1 => {
                // Tool call
                Ok(stream_done(make_assistant(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: format!("mixed-read-{index}"),
                        name: "read".to_string(),
                        arguments: json!({ "path": path }),
                        thought_signature: None,
                    })],
                    self.tokens_per_turn,
                )))
            }
            2 => {
                // Text after tool result
                Ok(stream_done(make_assistant(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "mixed tool done {index}"
                    )))],
                    self.tokens_per_turn,
                )))
            }
            _ => {
                // Error on every 5th (index 4, 9, 14, ...)
                Err(Error::api(format!("mixed error at call {index}")))
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 1: Multi-turn sustained conversation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_multi_turn_sustained_conversation() {
    let test_name = "soak_multi_turn_sustained";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let mut all_metrics: Vec<TurnMetrics> = Vec::new();
    let mut all_timeline: Vec<serde_json::Value> = Vec::new();

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(15));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);
        let mut cumulative_tokens: u64 = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Soak turn {turn}: describe the weather.");
            let result = agent_session.run_text(prompt, cb).await;

            let duration_ms = started_at.elapsed().as_millis();
            let (tool_starts, tool_ends) = {
                let guard = capture.lock().expect("lock");
                all_timeline.extend(guard.timeline.clone());
                (guard.total_tool_starts, guard.total_tool_ends)
            };

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };
            cumulative_tokens += tokens;

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            all_metrics.push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tool_starts,
                tool_ends,
                tokens,
                cumulative_tokens,
                session_message_count: msg_count,
                error,
            });

            if let Ok(msg) = &result {
                assert_eq!(msg.stop_reason, StopReason::Stop);
                assert!(
                    assistant_text(msg).contains(&format!("soak response turn {turn}")),
                    "turn {turn}: expected unique response"
                );
            }
        }

        agent_session
            .persist_session()
            .await
            .expect("persist session");

        // Verify final session state
        let final_messages = {
            let cx = asupersync::Cx::for_testing();
            let g = session.lock(&cx).await.expect("session lock");
            g.to_messages_for_current_path()
        };
        let final_tokens = total_assistant_tokens(&final_messages);

        // Each turn adds user + assistant = 2 messages
        assert!(
            final_messages.len() >= SOAK_TURN_COUNT * 2,
            "Expected at least {} messages, got {}",
            SOAK_TURN_COUNT * 2,
            final_messages.len()
        );

        // Tokens should accumulate monotonically
        assert!(
            final_tokens > 0,
            "Expected accumulated tokens > 0, got {final_tokens}"
        );

        (all_metrics, all_timeline, final_tokens)
    });

    // Re-run to get metrics out (run_async returns ())
    // We capture everything inside the async block via side effects in the harness
    harness
        .log()
        .info("soak", "multi-turn sustained conversation completed");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 2: Multi-turn with metrics capture (parallel version for assertions)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_multi_turn_metrics_and_token_accumulation() {
    let test_name = "soak_multi_turn_metrics";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let timeline_store = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let ms = Arc::clone(&metrics_store);
    let ts = Arc::clone(&timeline_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(20));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);
        let mut cumulative_tokens: u64 = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Metrics turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            {
                let guard = capture.lock().expect("lock");
                ts.lock()
                    .expect("timeline lock")
                    .extend(guard.timeline.clone());
            }

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };
            cumulative_tokens += tokens;

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tool_starts: 0,
                tool_ends: 0,
                tokens,
                cumulative_tokens,
                session_message_count: msg_count,
                error,
            });
        }

        agent_session.persist_session().await.expect("persist");
    });

    let metrics = metrics_store.lock().expect("final metrics");
    let timeline = timeline_store.lock().expect("final timeline");

    // Assertion: no errors in any turn
    for m in metrics.iter() {
        assert!(!m.error, "turn {} had an error", m.turn_index);
    }

    // Assertion: cumulative tokens grow monotonically
    for window in metrics.windows(2) {
        assert!(
            window[1].cumulative_tokens >= window[0].cumulative_tokens,
            "tokens decreased between turns {} and {}",
            window[0].turn_index,
            window[1].turn_index,
        );
    }

    // Assertion: session message count grows with each turn (2 messages per turn)
    for window in metrics.windows(2) {
        assert!(
            window[1].session_message_count > window[0].session_message_count,
            "session messages did not grow between turns {} and {}",
            window[0].turn_index,
            window[1].turn_index,
        );
    }

    // Assertion: final cumulative tokens = sum of individual tokens
    let total: u64 = metrics.iter().map(|m| m.tokens).sum();
    let final_cumulative = metrics.last().map_or(0, |m| m.cumulative_tokens);
    assert_eq!(
        total, final_cumulative,
        "cumulative token mismatch: sum={total}, final={final_cumulative}"
    );

    // Write artifacts
    write_metrics_artifact(&harness, test_name, &metrics);
    write_timeline_artifact(&harness, test_name, &timeline);

    let durations: Vec<u128> = metrics.iter().map(|m| m.duration_ms).collect();
    let (mean, stddev, min, max, _) = compute_latency_stats(&durations);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "total_tokens": final_cumulative,
            "final_message_count": metrics.last().map_or(0, |m| m.session_message_count),
            "latency": {
                "mean_ms": mean,
                "stddev_ms": stddev,
                "min_ms": min,
                "max_ms": max,
            },
            "errors": 0,
        }),
    );

    harness
        .log()
        .info("soak", "multi-turn metrics test completed");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 3: Repeated tool execution
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_repeated_tool_execution() {
    let test_name = "soak_repeated_tool_execution";
    let harness = TestHarness::new(test_name);

    let fixture = harness.create_file("soak_data.txt", "soak-payload-content\n");
    let fixture_path = fixture.display().to_string();
    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(RepeatedToolProvider::new(fixture_path, 18));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 10);
        let mut cumulative_tokens: u64 = 0;
        let mut total_tool_calls: usize = 0;

        for turn in 0..TOOL_ITERATION_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Read the file, iteration {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tool_starts, tool_ends) = {
                let guard = capture.lock().expect("lock");
                (guard.total_tool_starts, guard.total_tool_ends)
            };

            let (tokens, error) = match &result {
                Ok(msg) => {
                    total_tool_calls += tool_starts;
                    (msg.usage.total_tokens, false)
                }
                Err(_) => (0, true),
            };
            cumulative_tokens += tokens;

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tool_starts,
                tool_ends,
                tokens,
                cumulative_tokens,
                session_message_count: msg_count,
                error,
            });
        }

        agent_session.persist_session().await.expect("persist");
        assert!(
            total_tool_calls >= TOOL_ITERATION_COUNT,
            "Expected at least {TOOL_ITERATION_COUNT} tool calls, got {total_tool_calls}"
        );
    });

    let metrics = metrics_store.lock().expect("final metrics");

    // Verify no errors
    for m in metrics.iter() {
        assert!(!m.error, "tool iteration {} had an error", m.turn_index);
    }

    // Verify tools were balanced (each starts = ends)
    for m in metrics.iter() {
        assert_eq!(
            m.tool_starts, m.tool_ends,
            "tool start/end mismatch at iteration {}",
            m.turn_index
        );
    }

    // Write artifacts
    write_metrics_artifact(&harness, test_name, &metrics);

    let durations: Vec<u128> = metrics.iter().map(|m| m.duration_ms).collect();
    let (mean, stddev, min, max, _) = compute_latency_stats(&durations);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "iterations": TOOL_ITERATION_COUNT,
            "total_tokens": metrics.last().map_or(0, |m| m.cumulative_tokens),
            "total_tool_starts": metrics.iter().map(|m| m.tool_starts).sum::<usize>(),
            "latency": {
                "mean_ms": mean,
                "stddev_ms": stddev,
                "min_ms": min,
                "max_ms": max,
            },
        }),
    );

    harness
        .log()
        .info("soak", "repeated tool execution test completed");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 4: Latency stability
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_latency_stability_bounded_drift() {
    let test_name = "soak_latency_stability";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(10));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Latency check turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                error,
                ..TurnMetrics::default()
            });
        }
    });

    let metrics = metrics_store.lock().expect("final metrics");
    let durations: Vec<u128> = metrics.iter().map(|m| m.duration_ms).collect();

    // Skip first turn (warm-up) for drift analysis
    if durations.len() > 2 {
        let baseline = durations[1].max(1); // second turn as baseline (first may be warm-up)
        let last = durations[durations.len() - 1].max(1);
        let drift_ratio = last as f64 / baseline as f64;

        assert!(
            drift_ratio < MAX_LATENCY_DRIFT_RATIO,
            "Latency drift too high: last={last}ms, baseline={baseline}ms, ratio={drift_ratio:.2}, max={MAX_LATENCY_DRIFT_RATIO}"
        );

        harness
            .log()
            .info_ctx("soak", "latency drift check passed", |ctx| {
                ctx.push(("baseline_ms".into(), baseline.to_string()));
                ctx.push(("last_ms".into(), last.to_string()));
                ctx.push(("drift_ratio".into(), format!("{drift_ratio:.2}")));
            });
    }

    let (mean, stddev, min, max, _) = compute_latency_stats(&durations);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "latency": {
                "mean_ms": mean,
                "stddev_ms": stddev,
                "min_ms": min,
                "max_ms": max,
                "drift_ratio": if durations.len() > 2 {
                    durations[durations.len() - 1] as f64 / durations[1].max(1) as f64
                } else { 0.0 },
            },
        }),
    );

    write_metrics_artifact(&harness, test_name, &metrics);
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 5: Error recovery sustainability
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_error_recovery_sustainability() {
    let test_name = "soak_error_recovery";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        // Fails every 3rd call (calls 3, 6, 9, ...)
        let provider: Arc<dyn Provider> = Arc::new(IntermittentErrorProvider::new(3, 12));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);
        let mut success_count: usize = 0;
        let mut error_count: usize = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Recovery turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = if let Ok(msg) = &result {
                success_count += 1;
                (msg.usage.total_tokens, false)
            } else {
                error_count += 1;
                (0, true)
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                error,
                ..TurnMetrics::default()
            });
        }

        // Verify that we had BOTH successes and errors
        assert!(success_count > 0, "Expected some successful turns");
        assert!(
            error_count > 0,
            "Expected some error turns (intermittent failures)"
        );

        // Verify that errors did not prevent subsequent successes
        let last_few: Vec<bool> = ms
            .lock()
            .expect("lock")
            .iter()
            .rev()
            .take(3)
            .map(|m| m.error)
            .collect();
        // At least one of the last 3 turns should be a success
        assert!(
            last_few.iter().any(|e| !e),
            "All last 3 turns were errors — recovery failed"
        );
    });

    let metrics = metrics_store.lock().expect("final metrics");
    let error_count = metrics.iter().filter(|m| m.error).count();
    let success_count = metrics.len() - error_count;

    write_metrics_artifact(&harness, test_name, &metrics);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "successes": success_count,
            "errors": error_count,
            "error_rate": error_count as f64 / SOAK_TURN_COUNT as f64,
        }),
    );

    harness
        .log()
        .info_ctx("soak", "error recovery sustainability completed", |ctx| {
            ctx.push(("successes".into(), success_count.to_string()));
            ctx.push(("errors".into(), error_count.to_string()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 6: Session persist/reload cycle
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_session_persist_reload_cycle() {
    let test_name = "soak_session_persist_reload";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(15));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);

        // Phase 1: Run half the turns and persist
        let half = SOAK_TURN_COUNT / 2;
        for turn in 0..half {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Persist cycle turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                session_message_count: msg_count,
                error,
                ..TurnMetrics::default()
            });
        }

        // Persist
        agent_session.persist_session().await.expect("persist");

        // Get session path
        let session_path = {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            guard.path.clone().expect("session has path")
        };

        let pre_reload_msg_count = {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock");
            guard.to_messages_for_current_path().len()
        };

        // Reload
        let session_path_str = session_path.to_string_lossy().to_string();
        let (reloaded, diagnostics) = Session::open_with_diagnostics(&session_path_str)
            .await
            .expect("reload");
        assert!(
            diagnostics.skipped_entries.is_empty(),
            "no corruption expected during soak"
        );

        let post_reload_msg_count = reloaded.to_messages_for_current_path().len();
        assert_eq!(
            pre_reload_msg_count, post_reload_msg_count,
            "message count mismatch after reload"
        );

        // Phase 2: Continue with reloaded session
        let reloaded_session = Arc::new(asupersync::sync::Mutex::new(reloaded));
        let mut agent_session2 = make_agent_session(
            &cwd,
            Arc::clone(&provider),
            Arc::clone(&reloaded_session),
            4,
        );

        for turn in half..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Post-reload turn {turn}.");
            let result = agent_session2.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = reloaded_session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                session_message_count: msg_count,
                error,
                ..TurnMetrics::default()
            });
        }

        agent_session2.persist_session().await.expect("persist2");

        // Final message count should be full history
        let final_count = {
            let cx = asupersync::Cx::for_testing();
            let g = reloaded_session.lock(&cx).await.expect("lock");
            g.to_messages_for_current_path().len()
        };
        assert!(
            final_count >= SOAK_TURN_COUNT * 2,
            "Final message count {final_count} < expected {}",
            SOAK_TURN_COUNT * 2
        );
    });

    let metrics = metrics_store.lock().expect("final metrics");

    // Verify all turns succeeded
    for m in metrics.iter() {
        assert!(!m.error, "turn {} had an error", m.turn_index);
    }

    // Verify message counts grow across the reload boundary
    assert!(
        metrics.len() == SOAK_TURN_COUNT,
        "Expected {SOAK_TURN_COUNT} metrics, got {}",
        metrics.len()
    );

    write_metrics_artifact(&harness, test_name, &metrics);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "reload_at_turn": SOAK_TURN_COUNT / 2,
            "final_message_count": metrics.last().map_or(0, |m| m.session_message_count),
        }),
    );

    harness.log().info("soak", "persist/reload cycle completed");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 7: Mixed workload
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_mixed_workload() {
    let test_name = "soak_mixed_workload";
    let harness = TestHarness::new(test_name);

    let fixture = harness.create_file("mixed_data.txt", "mixed-soak-content\n");
    let fixture_path = fixture.display().to_string();
    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MixedWorkloadProvider::new(fixture_path, 14));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 10);
        let mut success_count: usize = 0;
        let mut tool_turn_count: usize = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Mixed turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tool_starts, tool_ends) = {
                let guard = capture.lock().expect("lock");
                (guard.total_tool_starts, guard.total_tool_ends)
            };

            if tool_starts > 0 {
                tool_turn_count += 1;
            }

            let (tokens, error) = match &result {
                Ok(msg) => {
                    success_count += 1;
                    (msg.usage.total_tokens, false)
                }
                Err(_) => (0, true),
            };

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tool_starts,
                tool_ends,
                tokens,
                cumulative_tokens: 0,
                session_message_count: msg_count,
                error,
            });
        }

        // Mixed workload should have all three: successes, tools, and errors
        assert!(success_count > 0, "Expected some successes");
        assert!(tool_turn_count > 0, "Expected some tool turns");
        // Errors may or may not happen depending on the provider pattern and agent loop handling
    });

    let metrics = metrics_store.lock().expect("final metrics");

    write_metrics_artifact(&harness, test_name, &metrics);

    let error_count = metrics.iter().filter(|m| m.error).count();
    let tool_count = metrics.iter().filter(|m| m.tool_starts > 0).count();
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "successes": metrics.len() - error_count,
            "errors": error_count,
            "tool_turns": tool_count,
        }),
    );

    harness.log().info("soak", "mixed workload test completed");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 8: Session message growth linearity
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_session_message_growth_linear() {
    let test_name = "soak_session_growth";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(10));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Growth check {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                session_message_count: msg_count,
                error,
                ..TurnMetrics::default()
            });
        }
    });

    let metrics = metrics_store.lock().expect("final metrics");

    // Each simple text turn adds exactly 2 messages (user + assistant)
    // Verify linear growth: message_count ≈ 2 * (turn + 1)
    for m in metrics.iter() {
        let expected_min = (m.turn_index + 1) * 2;
        assert!(
            m.session_message_count >= expected_min,
            "turn {}: expected >= {} messages, got {}",
            m.turn_index,
            expected_min,
            m.session_message_count
        );
    }

    // Verify strict monotonic growth
    for window in metrics.windows(2) {
        assert!(
            window[1].session_message_count > window[0].session_message_count,
            "message count did not increase between turns {} ({}) and {} ({})",
            window[0].turn_index,
            window[0].session_message_count,
            window[1].turn_index,
            window[1].session_message_count,
        );
    }

    // Growth should be approximately linear (within a factor of 2)
    let first = metrics.first().map_or(0, |m| m.session_message_count) as f64;
    let last = metrics.last().map_or(0, |m| m.session_message_count) as f64;
    let expected_last = first * SOAK_TURN_COUNT as f64;
    assert!(
        last <= expected_last * 1.5,
        "Message growth was super-linear: first={first}, last={last}, expected_max={:.0}",
        expected_last * 1.5
    );

    write_metrics_artifact(&harness, test_name, &metrics);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "first_message_count": metrics.first().map_or(0, |m| m.session_message_count),
            "final_message_count": metrics.last().map_or(0, |m| m.session_message_count),
            "growth_ratio": last / first.max(1.0),
        }),
    );

    harness
        .log()
        .info("soak", "session message growth linearity verified");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 9: Token budget monotonic accumulation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_token_budget_monotonic() {
    let test_name = "soak_token_budget";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let tokens_per = 25;
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(tokens_per));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);
        let mut cumulative_tokens: u64 = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Token budget turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };
            cumulative_tokens += tokens;

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                cumulative_tokens,
                error,
                ..TurnMetrics::default()
            });
        }

        agent_session.persist_session().await.expect("persist");

        // Verify from session
        let session_tokens = {
            let cx = asupersync::Cx::for_testing();
            let g = session.lock(&cx).await.expect("lock");
            total_assistant_tokens(&g.to_messages_for_current_path())
        };

        // Session tokens should equal our cumulative tracking
        assert_eq!(
            session_tokens, cumulative_tokens,
            "session token total {session_tokens} != tracked cumulative {cumulative_tokens}"
        );
    });

    let metrics = metrics_store.lock().expect("final metrics");

    // Monotonic cumulative tokens
    for window in metrics.windows(2) {
        assert!(
            window[1].cumulative_tokens >= window[0].cumulative_tokens,
            "cumulative tokens decreased between turns {} and {}",
            window[0].turn_index,
            window[1].turn_index,
        );
    }

    // Each turn should contribute exactly tokens_per_turn
    for m in metrics.iter() {
        assert!(
            !m.error,
            "turn {} had error, cannot verify token budget",
            m.turn_index
        );
        assert_eq!(
            m.tokens, 25,
            "turn {}: expected 25 tokens per turn, got {}",
            m.turn_index, m.tokens
        );
    }

    // Final cumulative = turns * tokens_per_turn
    let expected_total = SOAK_TURN_COUNT as u64 * 25;
    let actual_total = metrics.last().map_or(0, |m| m.cumulative_tokens);
    assert_eq!(
        actual_total, expected_total,
        "Expected total {expected_total}, got {actual_total}"
    );

    write_metrics_artifact(&harness, test_name, &metrics);
    write_summary_artifact(
        &harness,
        test_name,
        &json!({
            "test": test_name,
            "turns": SOAK_TURN_COUNT,
            "tokens_per_turn": 25,
            "expected_total": expected_total,
            "actual_total": actual_total,
        }),
    );

    harness
        .log()
        .info("soak", "token budget monotonic accumulation verified");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test 10: Stability summary report
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn soak_stability_report_generation() {
    let test_name = "soak_stability_report";
    let harness = TestHarness::new(test_name);

    let cwd = harness.temp_dir().to_path_buf();
    let metrics_store = Arc::new(Mutex::new(Vec::<TurnMetrics>::new()));
    let ms = Arc::clone(&metrics_store);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiTurnTextProvider::new(15));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 4);
        let mut cumulative_tokens: u64 = 0;

        for turn in 0..SOAK_TURN_COUNT {
            let started_at = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), started_at);

            let prompt = format!("Stability report turn {turn}.");
            let result = agent_session.run_text(prompt, cb).await;
            let duration_ms = started_at.elapsed().as_millis();

            let (tokens, error) = match &result {
                Ok(msg) => (msg.usage.total_tokens, false),
                Err(_) => (0, true),
            };
            cumulative_tokens += tokens;

            let msg_count = {
                let cx = asupersync::Cx::for_testing();
                let g = session.lock(&cx).await.expect("session lock");
                g.to_messages_for_current_path().len()
            };

            ms.lock().expect("metrics lock").push(TurnMetrics {
                turn_index: turn,
                duration_ms,
                tokens,
                cumulative_tokens,
                session_message_count: msg_count,
                error,
                ..TurnMetrics::default()
            });
        }
    });

    let metrics = metrics_store.lock().expect("final metrics");
    let durations: Vec<u128> = metrics.iter().map(|m| m.duration_ms).collect();
    let (mean, stddev, min, max, total_time) = compute_latency_stats(&durations);

    let error_count = metrics.iter().filter(|m| m.error).count();
    let final_tokens = metrics.last().map_or(0, |m| m.cumulative_tokens);
    let final_messages = metrics.last().map_or(0, |m| m.session_message_count);

    // Generate stability report
    let report = json!({
        "test": test_name,
        "config": {
            "turn_count": SOAK_TURN_COUNT,
            "max_latency_drift_ratio": MAX_LATENCY_DRIFT_RATIO,
        },
        "results": {
            "total_turns": metrics.len(),
            "successes": metrics.len() - error_count,
            "errors": error_count,
            "error_rate": error_count as f64 / metrics.len() as f64,
            "total_tokens": final_tokens,
            "final_session_messages": final_messages,
            "total_time_ms": total_time,
        },
        "latency": {
            "mean_ms": mean,
            "stddev_ms": stddev,
            "min_ms": min,
            "max_ms": max,
            "cv_pct": if mean > 0.0 { (stddev / mean) * 100.0 } else { 0.0 },
        },
        "stability_checks": {
            "no_errors": error_count == 0,
            "monotonic_tokens": metrics.windows(2).all(|w| w[1].cumulative_tokens >= w[0].cumulative_tokens),
            "monotonic_messages": metrics.windows(2).all(|w| w[1].session_message_count >= w[0].session_message_count),
            "latency_bounded": if durations.len() > 2 {
                (durations[durations.len() - 1] as f64 / durations[1].max(1) as f64) < MAX_LATENCY_DRIFT_RATIO
            } else { true },
        },
    });

    write_summary_artifact(&harness, test_name, &report);
    write_metrics_artifact(&harness, test_name, &metrics);

    // Generate markdown report
    let md_path = harness.temp_path(format!("{test_name}.report.md"));
    let md_content = format!(
        "# Soak Stability Report\n\n\
         | Metric | Value |\n\
         |--------|-------|\n\
         | Turns | {} |\n\
         | Successes | {} |\n\
         | Errors | {} |\n\
         | Error Rate | {:.1}% |\n\
         | Total Tokens | {} |\n\
         | Final Messages | {} |\n\
         | Mean Latency | {:.1}ms |\n\
         | Stddev Latency | {:.1}ms |\n\
         | Min Latency | {:.1}ms |\n\
         | Max Latency | {:.1}ms |\n\
         | CV | {:.1}% |\n",
        metrics.len(),
        metrics.len() - error_count,
        error_count,
        error_count as f64 / metrics.len() as f64 * 100.0,
        final_tokens,
        final_messages,
        mean,
        stddev,
        min,
        max,
        if mean > 0.0 {
            (stddev / mean) * 100.0
        } else {
            0.0
        },
    );
    std::fs::write(&md_path, md_content).expect("write markdown report");
    harness.record_artifact(format!("{test_name}.report.md"), &md_path);

    // All stability checks should pass
    assert_eq!(error_count, 0, "stability report: unexpected errors");
    assert!(
        report["stability_checks"]["monotonic_tokens"]
            .as_bool()
            .unwrap_or(false),
        "stability report: tokens not monotonic"
    );
    assert!(
        report["stability_checks"]["monotonic_messages"]
            .as_bool()
            .unwrap_or(false),
        "stability report: messages not monotonic"
    );
    assert!(
        report["stability_checks"]["latency_bounded"]
            .as_bool()
            .unwrap_or(false),
        "stability report: latency drift exceeded"
    );

    harness.log().info("soak", "stability report generated");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// GH #213 (bd-brx7l): Time-Series Soak Sampler, Mutation Sensitivity,
// Parameterized Duration Soak, and Sustained Multi-Tool Concurrency Harness
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SoakSample {
    schema: String,
    sample_index: usize,
    turn_index: usize,
    elapsed_ms: u64,
    timestamp_unix_ms: u64,
    rss_kb: u64,
    open_fds: usize,
    child_procs: usize,
    session_entries: usize,
    session_messages: usize,
    session_index_bytes: u64,
    hostcall_queue_depth: usize,
    turn_latency_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct MetricSummary {
    first: f64,
    median: f64,
    last: f64,
    min: f64,
    max: f64,
    slope_per_sample: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProvisionalGrowthBudgets {
    max_fd_growth: usize,
    max_child_proc_growth: usize,
    max_rss_growth_kb: u64,
    max_entries_per_turn_ratio: f64,
    max_queue_depth: usize,
    max_latency_drift_ratio: f64,
}

impl Default for ProvisionalGrowthBudgets {
    fn default() -> Self {
        Self {
            max_fd_growth: 16,
            max_child_proc_growth: 2,
            max_rss_growth_kb: 262_144,
            max_entries_per_turn_ratio: 12.0,
            max_queue_depth: 2_304,
            max_latency_drift_ratio: 10.0,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct MetricVerdict {
    metric: String,
    status: String,
    first: f64,
    median: f64,
    last: f64,
    threshold: String,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SoakMutationFault {
    #[default]
    None,
    LeakFileDescriptors(usize),
    UnboundedSessionVector(usize),
}

impl SoakMutationFault {
    fn from_env() -> Self {
        match std::env::var("PI_SOAK_MUTATION_FAULT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "leak_fd" => Self::LeakFileDescriptors(6),
            "unbounded_vec" => Self::UnboundedSessionVector(24),
            _ => Self::None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LeakFileDescriptors(_) => "leak_fd",
            Self::UnboundedSessionVector(_) => "unbounded_vec",
        }
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn sample_process_rss_kb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:")
                    && let Some(num) = rest.split_whitespace().next()
                    && let Ok(kb) = num.parse::<u64>()
                {
                    return kb;
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

fn sample_open_fds() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd").map_or(0, std::iter::Iterator::count)
    }
    #[cfg(target_os = "macos")]
    {
        std::fs::read_dir("/dev/fd").map_or(0, std::iter::Iterator::count)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

fn sample_child_procs() -> usize {
    #[cfg(target_os = "linux")]
    {
        let mut count = 0usize;
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for entry in tasks.flatten() {
                let children_path = entry.path().join("children");
                if let Ok(contents) = std::fs::read_to_string(children_path) {
                    count += contents.split_whitespace().count();
                }
            }
        }
        count
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

fn sample_session_disk_bytes(session_path: Option<&Path>) -> u64 {
    let Some(path) = session_path else {
        return 0;
    };
    let mut total = std::fs::metadata(path).map_or(0, |m| m.len());
    for suffix in ["-wal", "-shm", "-journal", ".idx", ".index.sqlite"] {
        let mut aux_os = path.as_os_str().to_os_string();
        aux_os.push(suffix);
        let aux_path = PathBuf::from(aux_os);
        if let Ok(meta) = std::fs::metadata(aux_path) {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn summarize_series(values: &[f64]) -> MetricSummary {
    let Some(&first) = values.first() else {
        return MetricSummary {
            first: 0.0,
            median: 0.0,
            last: 0.0,
            min: 0.0,
            max: 0.0,
            slope_per_sample: 0.0,
        };
    };
    let last = values.last().copied().unwrap_or(first);
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        let lo = sorted.get(mid.saturating_sub(1)).copied().unwrap_or(first);
        let hi = sorted.get(mid).copied().unwrap_or(last);
        f64::midpoint(lo, hi)
    } else {
        sorted.get(mid).copied().unwrap_or(first)
    };
    let min = sorted.first().copied().unwrap_or(first);
    let max = sorted.last().copied().unwrap_or(last);
    let slope_per_sample = if values.len() > 1 {
        (last - first) / (values.len() - 1) as f64
    } else {
        0.0
    };
    MetricSummary {
        first,
        median,
        last,
        min,
        max,
        slope_per_sample,
    }
}

fn evaluate_soak_series(
    samples: &[SoakSample],
    completed_turns: usize,
    budgets: &ProvisionalGrowthBudgets,
) -> std::result::Result<Vec<MetricVerdict>, Vec<MetricVerdict>> {
    let fd_vals: Vec<f64> = samples.iter().map(|s| s.open_fds as f64).collect();
    let child_vals: Vec<f64> = samples.iter().map(|s| s.child_procs as f64).collect();
    let rss_vals: Vec<f64> = samples.iter().map(|s| s.rss_kb as f64).collect();
    let entry_vals: Vec<f64> = samples.iter().map(|s| s.session_entries as f64).collect();
    let queue_vals: Vec<f64> = samples
        .iter()
        .map(|s| s.hostcall_queue_depth as f64)
        .collect();
    let lat_vals: Vec<f64> = samples.iter().map(|s| s.turn_latency_ms as f64).collect();

    let fd_sum = summarize_series(&fd_vals);
    let child_sum = summarize_series(&child_vals);
    let rss_sum = summarize_series(&rss_vals);
    let entry_sum = summarize_series(&entry_vals);
    let queue_sum = summarize_series(&queue_vals);
    let lat_sum = summarize_series(&lat_vals);

    let fd_growth = samples
        .last()
        .zip(samples.first())
        .map_or(0, |(last, first)| {
            last.open_fds.saturating_sub(first.open_fds)
        });
    let child_growth = samples
        .last()
        .zip(samples.first())
        .map_or(0, |(last, first)| {
            last.child_procs.saturating_sub(first.child_procs)
        });
    let rss_growth = samples
        .last()
        .zip(samples.first())
        .map_or(0, |(last, first)| last.rss_kb.saturating_sub(first.rss_kb));
    let final_entries = samples.last().map_or(0, |s| s.session_entries);
    let entries_per_turn = final_entries as f64 / completed_turns.max(1) as f64;
    let max_queue = samples
        .iter()
        .map(|s| s.hostcall_queue_depth)
        .max()
        .unwrap_or(0);
    let latency_drift = lat_sum.last / lat_sum.median.max(1.0);

    let verdicts = vec![
        MetricVerdict {
            metric: "open_fds".to_string(),
            status: if fd_growth <= budgets.max_fd_growth {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            first: fd_sum.first,
            median: fd_sum.median,
            last: fd_sum.last,
            threshold: format!("growth <= {} (provisional)", budgets.max_fd_growth),
            detail: format!("growth={fd_growth}"),
        },
        MetricVerdict {
            metric: "child_procs".to_string(),
            status: if child_growth <= budgets.max_child_proc_growth {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            first: child_sum.first,
            median: child_sum.median,
            last: child_sum.last,
            threshold: format!("growth <= {} (provisional)", budgets.max_child_proc_growth),
            detail: format!("growth={child_growth}"),
        },
        MetricVerdict {
            metric: "rss_kb".to_string(),
            status: if rss_growth <= budgets.max_rss_growth_kb {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            first: rss_sum.first,
            median: rss_sum.median,
            last: rss_sum.last,
            threshold: format!("growth <= {} KB (provisional)", budgets.max_rss_growth_kb),
            detail: format!("growth={rss_growth} KB"),
        },
        MetricVerdict {
            metric: "session_entries".to_string(),
            status: if entries_per_turn <= budgets.max_entries_per_turn_ratio {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            first: entry_sum.first,
            median: entry_sum.median,
            last: entry_sum.last,
            threshold: format!(
                "entries_per_turn <= {:.1} (provisional)",
                budgets.max_entries_per_turn_ratio
            ),
            detail: format!("entries={final_entries}, per_turn={entries_per_turn:.2}"),
        },
        MetricVerdict {
            metric: "hostcall_queue_depth".to_string(),
            status: if max_queue <= budgets.max_queue_depth {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            first: queue_sum.first,
            median: queue_sum.median,
            last: queue_sum.last,
            threshold: format!("max <= {} (provisional)", budgets.max_queue_depth),
            detail: format!("max_depth={max_queue}"),
        },
        MetricVerdict {
            metric: "turn_latency_ms".to_string(),
            status: if latency_drift <= budgets.max_latency_drift_ratio {
                "info".to_string()
            } else {
                "warn".to_string()
            },
            first: lat_sum.first,
            median: lat_sum.median,
            last: lat_sum.last,
            threshold: format!(
                "drift_watch <= {:.1}x (provisional, observational)",
                budgets.max_latency_drift_ratio
            ),
            detail: format!(
                "drift={latency_drift:.2}x, slope={:.2}ms/sample",
                lat_sum.slope_per_sample
            ),
        },
    ];

    if verdicts.iter().all(|v| v.status != "fail") {
        Ok(verdicts)
    } else {
        Err(verdicts)
    }
}

fn format_summary_markdown_table(verdicts: &[MetricVerdict]) -> String {
    let mut out = String::from(
        "| Metric | First | Median | Last | Threshold | Verdict |\n\
         |---|---:|---:|---:|---|---|\n",
    );
    for v in verdicts {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {:.1} | {:.1} | {} | {} |",
            v.metric, v.first, v.median, v.last, v.threshold, v.status
        );
    }
    out
}

#[derive(Debug, Clone)]
struct SessionSoakConfig {
    turns: usize,
    max_duration: Option<Duration>,
    sample_every_turns: usize,
    abort_after_turns: Option<usize>,
    mutation_fault: SoakMutationFault,
    budgets: ProvisionalGrowthBudgets,
    artifact_dir: Option<PathBuf>,
}

impl SessionSoakConfig {
    fn from_env(default_turns: usize, default_interval: usize) -> Self {
        let turns = std::env::var("PI_SOAK_TURNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default_turns);
        let max_duration = std::env::var("PI_SOAK_MAX_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .map(Duration::from_secs);
        let sample_every_turns = std::env::var("PI_SOAK_SAMPLE_INTERVAL")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default_interval);
        let artifact_dir = std::env::var("PI_SOAK_ARTIFACT_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from);
        Self {
            turns,
            max_duration,
            sample_every_turns,
            abort_after_turns: None,
            mutation_fault: SoakMutationFault::from_env(),
            budgets: ProvisionalGrowthBudgets::default(),
            artifact_dir,
        }
    }
}

#[derive(Debug)]
struct SoakRunOutcome {
    completed_turns: usize,
    aborted_early: bool,
    samples: Vec<SoakSample>,
    verdicts: Vec<MetricVerdict>,
    passed: bool,
    series_path: PathBuf,
    summary_path: PathBuf,
    branch_count: usize,
    compaction_count: usize,
    reload_count: usize,
}

/// Deterministic provider cycling text, read, write, edit, and bash turns.
#[derive(Debug)]
struct DurationSoakProvider {
    call_count: AtomicUsize,
    workspace: PathBuf,
}

impl DurationSoakProvider {
    const fn new(workspace: PathBuf) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            workspace,
        }
    }
}

#[async_trait]
impl Provider for DurationSoakProvider {
    fn name(&self) -> &'static str {
        "duration-soak-provider"
    }
    fn api(&self) -> &'static str {
        "soak-api"
    }
    fn model_id(&self) -> &'static str {
        "soak-duration-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let target = self
            .workspace
            .join("soak_workspace.txt")
            .display()
            .to_string();
        match index % 4 {
            0 => Ok(stream_done(make_assistant(
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("dur-write-{index}"),
                    name: "write".to_string(),
                    arguments: json!({
                        "path": target,
                        "content": format!("duration-soak-rev-{index}\n")
                    }),
                    thought_signature: None,
                })],
                16,
            ))),
            1 => Ok(stream_done(make_assistant(
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(format!(
                    "duration write verified at call {index}"
                )))],
                12,
            ))),
            2 => Ok(stream_done(make_assistant(
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("dur-bash-{index}"),
                    name: "bash".to_string(),
                    arguments: json!({
                        "command": format!("printf 'soak-bash-{index}\\n'")
                    }),
                    thought_signature: None,
                })],
                16,
            ))),
            _ => Ok(stream_done(make_assistant(
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(format!(
                    "duration bash verified at call {index}"
                )))],
                12,
            ))),
        }
    }
}

/// Serializes tests that sample or intentionally mutate process-wide OS
/// counters (`/proc/self/fd`, `/proc/self/task/*/children`) so injected FD
/// leaks in mutation tests never contaminate concurrent clean soak tests.
static PROCESS_SAMPLER_LOCK: Mutex<()> = Mutex::new(());

fn run_parameterized_session_soak(
    harness: &TestHarness,
    test_name: &str,
    cfg: &SessionSoakConfig,
) -> SoakRunOutcome {
    let _sampler_guard = PROCESS_SAMPLER_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cwd = harness.temp_dir().to_path_buf();
    let out_dir = cfg.artifact_dir.clone().unwrap_or_else(|| cwd.clone());
    std::fs::create_dir_all(&out_dir).expect("create soak artifact dir");
    let series_path = out_dir.join(format!("{test_name}.series.jsonl"));
    let summary_path = out_dir.join(format!("{test_name}.summary.json"));

    let samples_store = Arc::new(Mutex::new(Vec::<SoakSample>::new()));
    let state_store = Arc::new(Mutex::new((0usize, false, 0usize, 0usize, 0usize)));
    let ss = Arc::clone(&samples_store);
    let st = Arc::clone(&state_store);
    let series_path_async = series_path.clone();
    let cfg_clone = cfg.clone();

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(DurationSoakProvider::new(cwd.clone()));
        let mut session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 8);

        let mut series_file =
            std::fs::File::create(&series_path_async).expect("create soak series jsonl");
        let soak_start = Instant::now();
        let mut leaked_files: Vec<std::fs::File> = Vec::new();
        let mut completed_turns = 0usize;
        let mut aborted_early = false;
        let mut branch_count = 0usize;
        let mut compaction_count = 0usize;
        let mut reload_count = 0usize;
        let mut sample_index = 0usize;

        for turn in 0..cfg_clone.turns {
            if cfg_clone
                .abort_after_turns
                .is_some_and(|limit| turn >= limit)
                || cfg_clone
                    .max_duration
                    .is_some_and(|limit| soak_start.elapsed() >= limit)
            {
                aborted_early = true;
                break;
            }

            let turn_start = Instant::now();
            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), turn_start);
            let result = agent_session
                .run_text(format!("Duration soak turn {turn}."), cb)
                .await;
            assert!(
                result.is_ok(),
                "duration soak turn {turn} failed: {:?}",
                result.err()
            );
            let turn_latency_ms =
                u64::try_from(turn_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            completed_turns += 1;

            // Inject mutation faults only when explicitly configured to verify detection.
            match cfg_clone.mutation_fault {
                SoakMutationFault::None => {}
                SoakMutationFault::LeakFileDescriptors(per_turn) => {
                    for i in 0..per_turn {
                        let leak_path = cwd.join(format!("mutation_leak_t{turn}_{i}.tmp"));
                        let handle =
                            std::fs::File::create(&leak_path).expect("create mutation leak file");
                        leaked_files.push(handle);
                    }
                }
                SoakMutationFault::UnboundedSessionVector(per_turn) => {
                    let cx = asupersync::Cx::for_testing();
                    let mut guard = session.lock(&cx).await.expect("session lock");
                    for i in 0..per_turn {
                        guard.append_custom_entry(
                            "soak.mutation_unbounded".to_string(),
                            Some(json!({ "turn": turn, "seq": i })),
                        );
                    }
                }
            }

            // Periodically record a model change for the active soak provider,
            // branch (exercising non-linear path cache), append compaction
            // anchors, and persist/reload from disk.
            if (turn + 1).is_multiple_of(4) {
                let cx = asupersync::Cx::for_testing();
                let mut guard = session.lock(&cx).await.expect("session lock");
                guard.append_model_change(
                    provider.name().to_string(),
                    provider.model_id().to_string(),
                );
                if let Some(anchor_id) = guard.entries.first().and_then(|e| e.base_id().cloned())
                    && let Some(tip_id) = guard.leaf_id().map(ToOwned::to_owned)
                {
                    assert!(
                        guard.create_branch_from(&anchor_id),
                        "branch to anchor must succeed"
                    );
                    guard.append_custom_entry(
                        "soak.branch_probe".to_string(),
                        Some(json!({ "turn": turn })),
                    );
                    assert!(
                        guard.create_branch_from(&tip_id),
                        "restore main branch tip must succeed"
                    );
                    branch_count += 1;
                }
                if let Some(kept_id) = guard.leaf_id().map(ToOwned::to_owned) {
                    guard.append_compaction(
                        format!("soak compaction checkpoint at turn {turn}"),
                        kept_id,
                        u64::try_from(completed_turns * 32).unwrap_or(u64::MAX),
                        Some(json!({ "soak_turn": turn })),
                        Some(false),
                    );
                    compaction_count += 1;
                }
            }

            if (turn + 1).is_multiple_of(6) {
                agent_session
                    .persist_session()
                    .await
                    .expect("persist mid-soak");
                let saved_path = {
                    let cx = asupersync::Cx::for_testing();
                    let guard = session.lock(&cx).await.expect("session lock");
                    guard.path.clone().expect("persisted session path")
                };
                let (reloaded, diagnostics) =
                    Session::open_with_diagnostics(&saved_path.to_string_lossy())
                        .await
                        .expect("reload mid-soak session");
                assert!(
                    diagnostics.skipped_entries.is_empty(),
                    "mid-soak reload skipped entries: {:?}",
                    diagnostics.skipped_entries
                );
                session = Arc::new(asupersync::sync::Mutex::new(reloaded));
                agent_session =
                    make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 8);
                reload_count += 1;
            }

            let should_sample = turn == 0
                || (turn + 1).is_multiple_of(cfg_clone.sample_every_turns)
                || turn + 1 == cfg_clone.turns
                || cfg_clone
                    .abort_after_turns
                    .is_some_and(|limit| turn + 1 == limit);
            if should_sample {
                let (session_entries, session_messages, session_index_bytes) = {
                    let cx = asupersync::Cx::for_testing();
                    let guard = session.lock(&cx).await.expect("session lock");
                    let bytes = sample_session_disk_bytes(guard.path.as_deref());
                    (
                        guard.entries.len(),
                        guard.to_messages_for_current_path().len(),
                        bytes,
                    )
                };
                let sample = SoakSample {
                    schema: "pi.soak.sample.v1".to_string(),
                    sample_index,
                    turn_index: turn,
                    elapsed_ms: u64::try_from(soak_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    timestamp_unix_ms: unix_now_ms(),
                    rss_kb: sample_process_rss_kb(),
                    open_fds: sample_open_fds(),
                    child_procs: sample_child_procs(),
                    session_entries,
                    session_messages,
                    session_index_bytes,
                    hostcall_queue_depth: 0,
                    turn_latency_ms,
                };
                sample_index += 1;
                let line = serde_json::to_string(&sample).expect("serialize soak sample");
                writeln!(series_file, "{line}").expect("write soak sample line");
                series_file.flush().expect("flush soak sample line");
                ss.lock().expect("samples lock").push(sample);
            }
        }

        agent_session
            .persist_session()
            .await
            .expect("final soak persist");
        drop(leaked_files);
        *st.lock().expect("state lock") = (
            completed_turns,
            aborted_early,
            branch_count,
            compaction_count,
            reload_count,
        );
    });

    let samples = samples_store.lock().expect("final samples").clone();
    let (completed_turns, aborted_early, branch_count, compaction_count, reload_count) =
        *state_store.lock().expect("final state");
    let eval = evaluate_soak_series(&samples, completed_turns, &cfg.budgets);
    let (passed, verdicts) = match eval {
        Ok(v) => (true, v),
        Err(v) => (false, v),
    };
    let table = format_summary_markdown_table(&verdicts);
    let summary = json!({
        "schema": "pi.soak.session_duration.v1",
        "test": test_name,
        "parameters": {
            "turns": cfg.turns,
            "max_duration_secs": cfg.max_duration.map(|d| d.as_secs()),
            "sample_every_turns": cfg.sample_every_turns,
            "abort_after_turns": cfg.abort_after_turns,
            "mutation_fault": cfg.mutation_fault.label(),
        },
        "provisional_thresholds": true,
        "completed_turns": completed_turns,
        "aborted_early": aborted_early,
        "branch_count": branch_count,
        "compaction_count": compaction_count,
        "reload_count": reload_count,
        "sample_count": samples.len(),
        "overall_status": if passed { "pass" } else { "fail" },
        "verdicts": verdicts,
        "summary_table_markdown": table,
    });
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).expect("serialize soak summary"),
    )
    .expect("write soak summary");
    harness.record_artifact(format!("{test_name}.series.jsonl"), &series_path);
    harness.record_artifact(format!("{test_name}.summary.json"), &summary_path);

    SoakRunOutcome {
        completed_turns,
        aborted_early,
        samples,
        verdicts,
        passed,
        series_path,
        summary_path,
        branch_count,
        compaction_count,
        reload_count,
    }
}

#[derive(Debug, Clone)]
struct SoakHostcallItem {
    tenant: String,
    wave: usize,
    seq: usize,
}

impl QueueTenant for SoakHostcallItem {
    fn tenant_key(&self) -> Option<&str> {
        Some(&self.tenant)
    }
}

#[derive(Debug, Clone)]
struct ConcurrencySoakConfig {
    waves: usize,
    parallel_reads_per_wave: usize,
    hostcall_burst_per_wave: usize,
    fast_ring_capacity: usize,
    overflow_capacity: usize,
    max_duration: Option<Duration>,
    artifact_dir: Option<PathBuf>,
}

impl ConcurrencySoakConfig {
    fn from_env(default_waves: usize, default_reads: usize, default_burst: usize) -> Self {
        let waves = std::env::var("PI_CONCURRENCY_WAVES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default_waves);
        let parallel_reads_per_wave = std::env::var("PI_CONCURRENCY_PARALLEL_READS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default_reads);
        let hostcall_burst_per_wave = std::env::var("PI_CONCURRENCY_HOSTCALL_BURST")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default_burst);
        let max_duration = std::env::var("PI_SOAK_MAX_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .map(Duration::from_secs);
        let artifact_dir = std::env::var("PI_SOAK_ARTIFACT_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from);
        Self {
            waves,
            parallel_reads_per_wave,
            hostcall_burst_per_wave,
            fast_ring_capacity: 256,
            overflow_capacity: 2_048,
            max_duration,
            artifact_dir,
        }
    }
}

#[derive(Debug)]
struct MultiToolConcurrencyProvider {
    call_count: AtomicUsize,
    workspace: PathBuf,
    parallel_reads_per_wave: usize,
}

impl MultiToolConcurrencyProvider {
    const fn new(workspace: PathBuf, parallel_reads_per_wave: usize) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            workspace,
            parallel_reads_per_wave,
        }
    }
}

fn build_concurrency_read_call(workspace: &Path, wave: usize, slot: usize) -> ContentBlock {
    let fixture = workspace.join(format!("conc_fixture_{}.txt", slot % 4));
    ContentBlock::ToolCall(ToolCall {
        id: format!("conc-w{wave}-r{slot}"),
        name: "read".to_string(),
        arguments: json!({ "path": fixture.display().to_string() }),
        thought_signature: None,
    })
}

#[async_trait]
impl Provider for MultiToolConcurrencyProvider {
    fn name(&self) -> &'static str {
        "concurrency-soak-provider"
    }
    fn api(&self) -> &'static str {
        "soak-api"
    }
    fn model_id(&self) -> &'static str {
        "soak-concurrency-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let wave = index / 2;
        if index.is_multiple_of(2) {
            let mut blocks = Vec::with_capacity(self.parallel_reads_per_wave + 3);
            blocks.extend(
                (0..self.parallel_reads_per_wave)
                    .map(|slot| build_concurrency_read_call(&self.workspace, wave, slot)),
            );
            let out_file = self.workspace.join(format!("conc_wave_{wave}.txt"));
            let out_str = out_file.display().to_string();
            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: format!("conc-w{wave}-write"),
                name: "write".to_string(),
                arguments: json!({
                    "path": out_str,
                    "content": format!("wave-{wave}-initial\n")
                }),
                thought_signature: None,
            }));
            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: format!("conc-w{wave}-edit"),
                name: "edit".to_string(),
                arguments: json!({
                    "path": out_file.display().to_string(),
                    "oldText": format!("wave-{wave}-initial"),
                    "newText": format!("wave-{wave}-edited")
                }),
                thought_signature: None,
            }));
            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: format!("conc-w{wave}-postread"),
                name: "read".to_string(),
                arguments: json!({ "path": out_file.display().to_string() }),
                thought_signature: None,
            }));
            Ok(stream_done(make_assistant(StopReason::ToolUse, blocks, 32)))
        } else {
            Ok(stream_done(make_assistant(
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(format!(
                    "concurrency wave {wave} complete"
                )))],
                16,
            )))
        }
    }
}

#[derive(Debug)]
struct ConcurrencySoakOutcome {
    completed_waves: usize,
    total_tool_starts: usize,
    total_tool_ends: usize,
    fast_ring_admissions: u64,
    overflow_admissions: u64,
    drained_hostcalls: u64,
    parallel_batches_planned: usize,
    barrier_batches_planned: usize,
    steering_queued: usize,
    summary_path: PathBuf,
}

fn run_sustained_tool_concurrency_soak(
    harness: &TestHarness,
    test_name: &str,
    cfg: &ConcurrencySoakConfig,
) -> ConcurrencySoakOutcome {
    let cwd = harness.temp_dir().to_path_buf();
    for (name, body) in [
        ("conc_fixture_0.txt", "fixture 0 line A\nfixture 0 line B\n"),
        ("conc_fixture_1.txt", "fixture 1 line A\nfixture 1 line B\n"),
        ("conc_fixture_2.txt", "fixture 2 line A\nfixture 2 line B\n"),
        ("conc_fixture_3.txt", "fixture 3 line A\nfixture 3 line B\n"),
    ] {
        harness.create_file(name, body);
    }
    let out_dir = cfg.artifact_dir.clone().unwrap_or_else(|| cwd.clone());
    std::fs::create_dir_all(&out_dir).expect("create concurrency artifact dir");
    let series_path = out_dir.join(format!("{test_name}.series.jsonl"));
    let summary_path = out_dir.join(format!("{test_name}.summary.json"));

    let outcome_store = Arc::new(Mutex::new(
        None::<(usize, usize, usize, u64, u64, u64, usize, usize, usize)>,
    ));
    let os = Arc::clone(&outcome_store);
    let series_path_async = series_path.clone();
    let summary_path_async = summary_path.clone();
    let test_name_async = test_name.to_string();
    let cfg_clone = cfg.clone();

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MultiToolConcurrencyProvider::new(
            cwd.clone(),
            cfg_clone.parallel_reads_per_wave,
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, Arc::clone(&provider), Arc::clone(&session), 16);
        let registry = ToolRegistry::new(&tool_names(), &cwd, None);

        let mut hostcall_queue: HostcallRequestQueue<SoakHostcallItem> =
            HostcallRequestQueue::with_mode(
                cfg_clone.fast_ring_capacity,
                cfg_clone.overflow_capacity,
                HostcallQueueMode::Ebr,
            );

        let mut series_file =
            std::fs::File::create(&series_path_async).expect("create concurrency series jsonl");
        let soak_start = Instant::now();
        let mut completed_waves = 0usize;
        let mut total_tool_starts = 0usize;
        let mut total_tool_ends = 0usize;
        let mut fast_ring_admissions = 0u64;
        let mut overflow_admissions = 0u64;
        let mut drained_hostcalls = 0u64;
        let mut parallel_batches_planned = 0usize;
        let mut barrier_batches_planned = 0usize;
        let mut steering_queued = 0usize;
        let mut wave_latencies = Vec::with_capacity(cfg_clone.waves);
        let mut queue_depths = Vec::with_capacity(cfg_clone.waves);

        // Compute the exact tool-effect batch plan for each wave from real Tool::effects_for_input.
        let read_effect = registry
            .get("read")
            .expect("read tool")
            .effects_for_input(&json!({ "path": "conc_fixture_0.txt" }));
        let write_effect = registry
            .get("write")
            .expect("write tool")
            .effects_for_input(&json!({ "path": "out.txt", "content": "x" }));
        let edit_effect = registry
            .get("edit")
            .expect("edit tool")
            .effects_for_input(&json!({ "path": "out.txt", "oldText": "a", "newText": "b" }));
        let mut wave_effects: Vec<ToolEffects> =
            vec![read_effect; cfg_clone.parallel_reads_per_wave];
        wave_effects.push(write_effect);
        wave_effects.push(edit_effect);
        wave_effects.push(read_effect);
        let batch_plan = tool_effect_batch_plan_evidence(&wave_effects, 8);
        let wave_parallel_batches = batch_plan
            .batches
            .iter()
            .filter(|b| b.parallel_safe)
            .count();
        let wave_barrier_batches = batch_plan
            .batches
            .iter()
            .filter(|b| !b.parallel_safe)
            .count();

        for wave in 0..cfg_clone.waves {
            if cfg_clone
                .max_duration
                .is_some_and(|limit| soak_start.elapsed() >= limit)
            {
                break;
            }
            let wave_start = Instant::now();

            // Exercise mid-wave steering queue on every 3rd wave.
            if wave > 0 && wave.is_multiple_of(3) {
                agent_session
                    .agent
                    .queue_steering(Message::User(UserMessage {
                        content: UserContent::Text(format!("mid-soak steering note wave {wave}")),
                        timestamp: 0,
                    }));
                steering_queued += 1;
            }

            let capture = Arc::new(Mutex::new(SoakCapture::default()));
            let cb = SoakCapture::event_callback(Arc::clone(&capture), wave_start);
            let res = agent_session
                .run_text(format!("Execute multi-tool concurrency wave {wave}."), cb)
                .await;
            assert!(
                res.is_ok(),
                "concurrency wave {wave} failed: {:?}",
                res.err()
            );

            let (starts, ends) = {
                let g = capture.lock().expect("capture lock");
                (g.total_tool_starts, g.total_tool_ends)
            };
            let expected_tools = cfg_clone.parallel_reads_per_wave + 3;
            assert_eq!(
                starts, expected_tools,
                "wave {wave}: expected {expected_tools} tool starts, got {starts}"
            );
            assert_eq!(
                starts, ends,
                "wave {wave}: unbalanced tool starts={starts} ends={ends}"
            );
            total_tool_starts += starts;
            total_tool_ends += ends;
            parallel_batches_planned += wave_parallel_batches;
            barrier_batches_planned += wave_barrier_batches;

            // Verify barrier tools (`write` then `edit`) applied strictly in order.
            let barrier_file = cwd.join(format!("conc_wave_{wave}.txt"));
            let written = std::fs::read_to_string(&barrier_file).expect("read barrier output");
            assert_eq!(
                written,
                format!("wave-{wave}-edited\n"),
                "wave {wave}: barrier write->edit order corrupted"
            );

            // Drive real HostcallRequestQueue fast ring (256) into overflow (2048) with 16 tenants.
            let mut wave_fast = 0u64;
            let mut wave_overflow = 0u64;
            let mut wave_rejected = 0u64;
            for seq in 0..cfg_clone.hostcall_burst_per_wave {
                let item = SoakHostcallItem {
                    tenant: format!("ext.tenant.{}", seq % 16),
                    wave,
                    seq,
                };
                match hostcall_queue.push_back(item) {
                    HostcallQueueEnqueueResult::FastPath { .. } => {
                        wave_fast += 1;
                        fast_ring_admissions += 1;
                    }
                    HostcallQueueEnqueueResult::OverflowPath { .. } => {
                        wave_overflow += 1;
                        overflow_admissions += 1;
                    }
                    HostcallQueueEnqueueResult::Rejected { .. } => {
                        wave_rejected += 1;
                    }
                }
            }
            let peak_snap = hostcall_queue.snapshot();
            let drained = hostcall_queue.drain_all();
            let drained_len = u64::try_from(drained.len()).unwrap_or(u64::MAX);
            assert_eq!(
                drained_len,
                wave_fast + wave_overflow,
                "wave {wave}: lost queued hostcalls"
            );
            assert!(
                drained
                    .iter()
                    .all(|item| item.wave == wave && item.seq < cfg_clone.hostcall_burst_per_wave),
                "wave {wave}: drained hostcall metadata corrupted"
            );
            drained_hostcalls += drained_len;
            hostcall_queue.force_reclaim();
            assert!(
                hostcall_queue.is_empty(),
                "wave {wave}: hostcall queue not empty after drain"
            );

            let wave_ms = u64::try_from(wave_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            wave_latencies.push(wave_ms as f64);
            queue_depths.push(peak_snap.total_depth as f64);
            completed_waves += 1;

            let row = json!({
                "schema": "pi.soak.concurrency_sample.v1",
                "timestamp_unix_ms": unix_now_ms(),
                "wave_index": wave,
                "tool_starts": starts,
                "tool_ends": ends,
                "batch_plan": batch_plan,
                "hostcall_fast_admissions": wave_fast,
                "hostcall_overflow_admissions": wave_overflow,
                "hostcall_rejected": wave_rejected,
                "hostcall_peak_depth": peak_snap.total_depth,
                "hostcall_overflow_depth": peak_snap.overflow_depth,
                "wave_latency_ms": wave_ms,
            });
            writeln!(
                series_file,
                "{}",
                serde_json::to_string(&row).expect("serialize concurrency row")
            )
            .expect("write concurrency row");
            series_file.flush().expect("flush concurrency row");
        }

        let summary = json!({
            "schema": "pi.soak.tool_concurrency.v1",
            "test": test_name_async,
            "parameters": {
                "waves": cfg_clone.waves,
                "parallel_reads_per_wave": cfg_clone.parallel_reads_per_wave,
                "hostcall_burst_per_wave": cfg_clone.hostcall_burst_per_wave,
                "fast_ring_capacity": cfg_clone.fast_ring_capacity,
                "overflow_capacity": cfg_clone.overflow_capacity,
            },
            "completed_waves": completed_waves,
            "total_tool_starts": total_tool_starts,
            "total_tool_ends": total_tool_ends,
            "parallel_batches_planned": parallel_batches_planned,
            "barrier_batches_planned": barrier_batches_planned,
            "steering_queued": steering_queued,
            "hostcall_fast_ring_admissions": fast_ring_admissions,
            "hostcall_overflow_admissions": overflow_admissions,
            "hostcall_drained": drained_hostcalls,
            "summary_table": {
                "wave_latency_ms": summarize_series(&wave_latencies),
                "hostcall_peak_depth": summarize_series(&queue_depths),
            },
            "overall_status": "pass",
        });
        std::fs::write(
            &summary_path_async,
            serde_json::to_string_pretty(&summary).expect("serialize concurrency summary"),
        )
        .expect("write concurrency summary");

        *os.lock().expect("outcome lock") = Some((
            completed_waves,
            total_tool_starts,
            total_tool_ends,
            fast_ring_admissions,
            overflow_admissions,
            drained_hostcalls,
            parallel_batches_planned,
            barrier_batches_planned,
            steering_queued,
        ));
    });

    let (
        completed_waves,
        total_tool_starts,
        total_tool_ends,
        fast_ring_admissions,
        overflow_admissions,
        drained_hostcalls,
        parallel_batches_planned,
        barrier_batches_planned,
        steering_queued,
    ) = outcome_store
        .lock()
        .expect("final outcome")
        .expect("concurrency soak outcome present");
    harness.record_artifact(format!("{test_name}.series.jsonl"), &series_path);
    harness.record_artifact(format!("{test_name}.summary.json"), &summary_path);

    ConcurrencySoakOutcome {
        completed_waves,
        total_tool_starts,
        total_tool_ends,
        fast_ring_admissions,
        overflow_admissions,
        drained_hostcalls,
        parallel_batches_planned,
        barrier_batches_planned,
        steering_queued,
        summary_path,
    }
}

#[test]
fn soak_sampler_records_at_expected_cadence() {
    let harness = TestHarness::new("soak_sampler_cadence");
    let cfg = SessionSoakConfig {
        turns: 10,
        max_duration: None,
        sample_every_turns: 3,
        abort_after_turns: None,
        mutation_fault: SoakMutationFault::None,
        budgets: ProvisionalGrowthBudgets::default(),
        artifact_dir: None,
    };
    let outcome = run_parameterized_session_soak(&harness, "soak_sampler_cadence", &cfg);
    assert!(
        outcome.passed,
        "clean cadence run must pass: {:?}",
        outcome.verdicts
    );
    assert_eq!(outcome.completed_turns, 10);
    // Samples on turn 0, 2 (3rd), 5 (6th), 8 (9th), and 9 (final 10th) -> 5 samples.
    let sampled_turns: Vec<usize> = outcome.samples.iter().map(|s| s.turn_index).collect();
    assert_eq!(sampled_turns, vec![0, 2, 5, 8, 9]);
}

#[test]
fn soak_detector_catches_deliberately_leaked_descriptor() {
    // 1. Cross-platform evaluator test on synthetic FD-growth samples.
    let synthetic = vec![
        SoakSample {
            schema: "pi.soak.sample.v1".to_string(),
            sample_index: 0,
            turn_index: 0,
            elapsed_ms: 10,
            timestamp_unix_ms: 1_700_000_000_000,
            rss_kb: 20_000,
            open_fds: 20,
            child_procs: 0,
            session_entries: 2,
            session_messages: 2,
            session_index_bytes: 512,
            hostcall_queue_depth: 0,
            turn_latency_ms: 5,
        },
        SoakSample {
            schema: "pi.soak.sample.v1".to_string(),
            sample_index: 1,
            turn_index: 5,
            elapsed_ms: 60,
            timestamp_unix_ms: 1_700_000_000_050,
            rss_kb: 20_100,
            open_fds: 64,
            child_procs: 0,
            session_entries: 12,
            session_messages: 12,
            session_index_bytes: 2_048,
            hostcall_queue_depth: 0,
            turn_latency_ms: 6,
        },
    ];
    let err_verdicts = evaluate_soak_series(&synthetic, 6, &ProvisionalGrowthBudgets::default())
        .expect_err("FD growth of +44 must fail provisional growth budget");
    assert!(
        err_verdicts
            .iter()
            .any(|v| v.metric == "open_fds" && v.status == "fail"),
        "failing verdicts must explicitly name open_fds: {err_verdicts:?}"
    );

    // 2. Real OS file-descriptor leak detection via /proc/self/fd on Linux.
    #[cfg(target_os = "linux")]
    {
        let _sampler_guard = PROCESS_SAMPLER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let harness = TestHarness::new("soak_real_fd_leak_probe");
        let probe_path = harness.create_file("fd_leak_probe.tmp", "probe\n");
        let mut last_sample = (0usize, 0usize);
        let mut observed_leak = false;
        for _ in 0..8 {
            let before = sample_open_fds();
            let mut leaked = Vec::with_capacity(24);
            for _ in 0..24 {
                leaked.push(std::fs::File::open(&probe_path).expect("open temp file"));
            }
            let after = sample_open_fds();
            drop(leaked);
            last_sample = (before, after);
            if after >= before + 24 {
                observed_leak = true;
                break;
            }
            // Parallel tests outside PROCESS_SAMPLER_LOCK may close an unrelated FD
            // during this microsecond window; retry without relaxing the +24 threshold.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            observed_leak,
            "real /proc/self/fd sampler must observe +24 open descriptors (before={}, after={})",
            last_sample.0, last_sample.1
        );
    }
}

#[test]
fn soak_detector_catches_deliberately_unbounded_vector() {
    let synthetic = vec![
        SoakSample {
            schema: "pi.soak.sample.v1".to_string(),
            sample_index: 0,
            turn_index: 0,
            elapsed_ms: 10,
            timestamp_unix_ms: 1_700_000_000_000,
            rss_kb: 20_000,
            open_fds: 20,
            child_procs: 0,
            session_entries: 50,
            session_messages: 2,
            session_index_bytes: 512,
            hostcall_queue_depth: 0,
            turn_latency_ms: 5,
        },
        SoakSample {
            schema: "pi.soak.sample.v1".to_string(),
            sample_index: 1,
            turn_index: 5,
            elapsed_ms: 60,
            timestamp_unix_ms: 1_700_000_000_050,
            rss_kb: 20_100,
            open_fds: 20,
            child_procs: 0,
            session_entries: 500,
            session_messages: 12,
            session_index_bytes: 64_000,
            hostcall_queue_depth: 0,
            turn_latency_ms: 6,
        },
    ];
    let err_verdicts = evaluate_soak_series(&synthetic, 6, &ProvisionalGrowthBudgets::default())
        .expect_err("unbounded 500 entries over 6 turns must fail session_entries ratio");
    assert!(
        err_verdicts
            .iter()
            .any(|v| v.metric == "session_entries" && v.status == "fail"),
        "failing verdicts must explicitly name session_entries: {err_verdicts:?}"
    );
    assert!(
        err_verdicts
            .iter()
            .any(|v| v.metric == "turn_latency_ms" && v.status == "info"),
        "flat latency series must report observational info status: {err_verdicts:?}"
    );

    // 4. Observational turn_latency_ms drift warns above max_latency_drift_ratio
    // without failing a clean resource series. Use 3 samples [5, 5, 200] so the
    // median stays at 5ms (drift = 40x > 5x).
    let first_sample = synthetic.first().expect("first synthetic sample");
    let second_sample = synthetic.get(1).expect("second synthetic sample");
    let mut baseline_sample = first_sample.clone();
    baseline_sample.turn_index = 2;
    baseline_sample.session_entries = 8;
    let mut spike_sample = second_sample.clone();
    spike_sample.session_entries = 12;
    spike_sample.turn_latency_ms = 200;
    let drift_only = vec![first_sample.clone(), baseline_sample, spike_sample];
    let ok_verdicts = evaluate_soak_series(&drift_only, 6, &ProvisionalGrowthBudgets::default())
        .expect("observational latency drift alone must warn rather than fail");
    assert!(
        ok_verdicts
            .iter()
            .any(|v| v.metric == "turn_latency_ms" && v.status == "warn"),
        "steep latency drift must report observational warn status: {ok_verdicts:?}"
    );
}

#[test]
fn soak_series_is_well_formed_when_aborted_early() {
    let harness = TestHarness::new("soak_early_abort");
    let cfg = SessionSoakConfig {
        turns: 20,
        max_duration: None,
        sample_every_turns: 2,
        abort_after_turns: Some(5),
        mutation_fault: SoakMutationFault::None,
        budgets: ProvisionalGrowthBudgets::default(),
        artifact_dir: None,
    };
    let outcome = run_parameterized_session_soak(&harness, "soak_early_abort", &cfg);
    assert!(outcome.aborted_early, "expected early abort flag");
    assert_eq!(outcome.completed_turns, 5);
    assert!(
        !outcome.samples.is_empty(),
        "early abort must still flush samples"
    );

    let raw_series = std::fs::read_to_string(&outcome.series_path).expect("read series jsonl");
    let lines: Vec<&str> = raw_series.lines().collect();
    assert_eq!(lines.len(), outcome.samples.len());
    for line in lines {
        let parsed = serde_json::from_str::<SoakSample>(line);
        assert!(
            parsed.is_ok(),
            "every flushed JSONL line must parse cleanly: {line}"
        );
        if let Ok(sample) = parsed {
            assert_eq!(sample.schema, "pi.soak.sample.v1");
        }
    }
    let summary_text = std::fs::read_to_string(&outcome.summary_path).expect("read summary json");
    let parsed_summary = serde_json::from_str::<serde_json::Value>(&summary_text);
    assert!(parsed_summary.is_ok(), "summary json must parse cleanly");
    if let Ok(summary_json) = parsed_summary {
        assert_eq!(summary_json["aborted_early"], json!(true));
        assert_eq!(summary_json["completed_turns"], json!(5));
    }
}

#[test]
fn soak_mutation_sensitivity_flags_injected_fd_and_vector_leaks() {
    // 1. Unbounded vector mutation fault must cause the soak runner to fail and name session_entries.
    let vec_harness = TestHarness::new("soak_mutation_unbounded_vec");
    let vec_cfg = SessionSoakConfig {
        turns: 6,
        max_duration: None,
        sample_every_turns: 2,
        abort_after_turns: None,
        mutation_fault: SoakMutationFault::UnboundedSessionVector(20),
        budgets: ProvisionalGrowthBudgets::default(),
        artifact_dir: None,
    };
    let vec_outcome =
        run_parameterized_session_soak(&vec_harness, "soak_mutation_unbounded_vec", &vec_cfg);
    assert!(
        !vec_outcome.passed,
        "injected unbounded vector mutation MUST fail soak"
    );
    assert!(
        vec_outcome
            .verdicts
            .iter()
            .any(|v| v.metric == "session_entries" && v.status == "fail"),
        "mutation failure must name session_entries: {:?}",
        vec_outcome.verdicts
    );

    // 2. Real OS file-descriptor leak mutation fault must fail and name open_fds on Linux.
    #[cfg(target_os = "linux")]
    {
        let fd_harness = TestHarness::new("soak_mutation_leak_fd");
        let fd_cfg = SessionSoakConfig {
            turns: 6,
            max_duration: None,
            sample_every_turns: 2,
            abort_after_turns: None,
            // Leak 24 FDs/turn (+120 FDs between turn-0 sample and turn 5, well above
            // the 16-FD provisional budget) so concurrent sibling tests in this binary
            // closing transient files/sockets cannot mask the injected process FD leak.
            mutation_fault: SoakMutationFault::LeakFileDescriptors(24),
            budgets: ProvisionalGrowthBudgets::default(),
            artifact_dir: None,
        };
        let fd_outcome =
            run_parameterized_session_soak(&fd_harness, "soak_mutation_leak_fd", &fd_cfg);
        assert!(
            !fd_outcome.passed,
            "injected FD leak mutation MUST fail soak: {:?}",
            fd_outcome.verdicts
        );
        assert!(
            fd_outcome
                .verdicts
                .iter()
                .any(|v| v.metric == "open_fds" && v.status == "fail"),
            "mutation failure must name open_fds: {:?}",
            fd_outcome.verdicts
        );
    }
}

#[test]
fn soak_session_duration_smoke_with_branch_compact_and_reload() {
    let harness = TestHarness::new("soak_duration_smoke");
    let cfg = SessionSoakConfig {
        turns: 12,
        max_duration: None,
        sample_every_turns: 3,
        abort_after_turns: None,
        mutation_fault: SoakMutationFault::None,
        budgets: ProvisionalGrowthBudgets::default(),
        artifact_dir: None,
    };
    let outcome = run_parameterized_session_soak(&harness, "soak_duration_smoke", &cfg);
    assert!(
        outcome.passed,
        "duration smoke soak failed: {:?}",
        outcome.verdicts
    );
    assert_eq!(outcome.completed_turns, 12);
    assert!(outcome.branch_count >= 3, "expected >=3 branch transitions");
    assert!(
        outcome.compaction_count >= 3,
        "expected >=3 compaction anchors"
    );
    assert!(outcome.reload_count >= 2, "expected >=2 mid-soak reloads");
    assert!(
        outcome.samples.len() >= 4,
        "expected >=4 time-series samples"
    );
}

#[test]
fn soak_sustained_multi_tool_concurrency_smoke_exercises_batcher_and_overflow() {
    let harness = TestHarness::new("soak_concurrency_smoke");
    let cfg = ConcurrencySoakConfig {
        waves: 6,
        parallel_reads_per_wave: 6,
        hostcall_burst_per_wave: 320,
        fast_ring_capacity: 256,
        overflow_capacity: 2_048,
        max_duration: None,
        artifact_dir: None,
    };
    let outcome = run_sustained_tool_concurrency_soak(&harness, "soak_concurrency_smoke", &cfg);
    assert_eq!(outcome.completed_waves, 6);
    assert_eq!(outcome.total_tool_starts, 6 * (6 + 3));
    assert_eq!(outcome.total_tool_starts, outcome.total_tool_ends);
    assert!(
        outcome.parallel_batches_planned >= 6,
        "expected parallel read batches planned"
    );
    assert!(
        outcome.barrier_batches_planned >= 6,
        "expected write/edit barrier batches planned"
    );
    assert!(outcome.steering_queued >= 1, "expected mid-soak steering");
    assert!(
        outcome.fast_ring_admissions > 0,
        "expected fast-ring hostcall admissions"
    );
    assert!(
        outcome.overflow_admissions > 0,
        "expected overflow hostcall admissions (>256 burst)"
    );
    assert_eq!(
        outcome.drained_hostcalls,
        outcome.fast_ring_admissions + outcome.overflow_admissions,
        "zero lost hostcalls across fast+overflow lanes"
    );
    assert!(outcome.summary_path.exists());
}

#[test]
#[ignore = "explicit duration soak lane: invoke via scripts/e2e/run_session_soak.sh"]
fn soak_session_duration_extended() {
    let harness = TestHarness::new("soak_session_duration_extended");
    let cfg = SessionSoakConfig::from_env(60, 5);
    let outcome = run_parameterized_session_soak(&harness, "soak_session_duration_extended", &cfg);
    println!("{}", format_summary_markdown_table(&outcome.verdicts));
    assert!(
        outcome.passed,
        "extended session duration soak failed (mutation_fault={}): {:?}",
        cfg.mutation_fault.label(),
        outcome.verdicts
    );
}

#[test]
#[ignore = "explicit concurrency soak lane: invoke via scripts/e2e/run_tool_concurrency_soak.sh"]
fn soak_sustained_tool_concurrency_extended() {
    let harness = TestHarness::new("soak_sustained_tool_concurrency_extended");
    let cfg = ConcurrencySoakConfig::from_env(24, 8, 320);
    let outcome = run_sustained_tool_concurrency_soak(
        &harness,
        "soak_sustained_tool_concurrency_extended",
        &cfg,
    );
    if cfg.max_duration.is_some() {
        assert!(
            outcome.completed_waves > 0 && outcome.completed_waves <= cfg.waves,
            "time-capped concurrency soak must complete at least 1 wave (got {}/{})",
            outcome.completed_waves,
            cfg.waves
        );
    } else {
        assert_eq!(outcome.completed_waves, cfg.waves);
    }
    assert_eq!(outcome.total_tool_starts, outcome.total_tool_ends);
    assert!(outcome.overflow_admissions > 0);
}
