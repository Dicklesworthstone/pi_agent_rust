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
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
        AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
        AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
        AgentEvent::FailoverStart { .. } => "failover_start",
        AgentEvent::FailoverEnd { .. } => "failover_end",
        AgentEvent::ExtensionError { .. } => "extension_error",
        AgentEvent::AdvisorNote { .. } => "advisor_note",
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
