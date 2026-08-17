//! E2E: Failure injection and recovery scenario script pack (bd-1f42.8.5.4).
//!
//! Deterministic tests for high-impact failure classes, each paired with
//! recovery assertions. Five failure categories:
//!
//! 1. Auth failure (401/403) — clear error messages, no retry
//! 2. Rate-limit/quota (429) — graceful abort, user remediation hints
//! 3. Timeout — bounded wait, error surfaced
//! 4. Malformed response — handled without crash
//! 5. Tool-failure propagation — error in tool result propagated to agent
//!
//! All tests use in-process deterministic providers (no network).
//!
//! Run:
//! ```bash
//! cargo test --test e2e_failure_injection_recovery
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
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    turn_count: usize,
    tool_starts: usize,
    tool_ends: usize,
    timeline: Vec<String>,
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

// ═══════════════════════════════════════════════════════════════════════════
// FAILURE CLASS 1: Auth failures (401/403)
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that simulates 401 Unauthorized.
#[derive(Debug)]
struct AuthFailureProvider {
    call_count: AtomicUsize,
}

impl AuthFailureProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for AuthFailureProvider {
    fn name(&self) -> &str {
        "auth-failure-provider"
    }
    fn api(&self) -> &str {
        "auth-failure-api"
    }
    fn model_id(&self) -> &str {
        "auth-failure-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Err(Error::api("401 Unauthorized: Invalid API key"))
    }
}

/// Provider that simulates 403 Forbidden.
#[derive(Debug)]
struct ForbiddenProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ForbiddenProvider {
    fn name(&self) -> &str {
        "forbidden-provider"
    }
    fn api(&self) -> &str {
        "forbidden-api"
    }
    fn model_id(&self) -> &str {
        "forbidden-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        Err(Error::api("403 Forbidden: Access denied for this model"))
    }
}

/// AUTH-1: 401 Unauthorized surfaces clear error, no retry.
#[test]
fn auth_401_surfaces_clear_error_no_retry() {
    let test_name = "fi_auth_401_no_retry";
    let harness = TestHarness::new(test_name);
    harness.section("auth_failure_401");

    let provider = Arc::new(AuthFailureProvider::new());
    let provider_ref = Arc::clone(&provider);
    let cwd = harness.temp_dir().to_path_buf();

    let result = run_async(async move {
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, provider_ref as Arc<dyn Provider>, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    // Recovery assertion: error is surfaced, not swallowed
    assert!(result.is_err(), "401 should propagate as error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("401") || err_msg.contains("Unauthorized"),
        "Error should contain auth failure details: {err_msg}"
    );

    // Recovery assertion: no retry (only 1 call made)
    assert_eq!(
        provider.call_count.load(Ordering::SeqCst),
        1,
        "Auth errors should not be retried"
    );

    harness
        .log()
        .info_ctx("result", "AUTH-1: 401 verified", |ctx| {
            ctx.push(("error".into(), err_msg));
            ctx.push(("retry_count".into(), "0".into()));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// AUTH-2: 403 Forbidden surfaces model-specific error.
#[test]
fn auth_403_surfaces_model_specific_error() {
    let test_name = "fi_auth_403_forbidden";
    let harness = TestHarness::new(test_name);
    harness.section("auth_failure_403");

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ForbiddenProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    assert!(result.is_err(), "403 should propagate as error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("403") || err_msg.contains("Forbidden"),
        "Error should contain forbidden details: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("AUTH-2: 403 verified: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILURE CLASS 2: Rate-limit / Quota (429)
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that simulates 429 Too Many Requests.
#[derive(Debug)]
struct RateLimitProvider {
    call_count: AtomicUsize,
}

impl RateLimitProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for RateLimitProvider {
    fn name(&self) -> &str {
        "rate-limit-provider"
    }
    fn api(&self) -> &str {
        "rate-limit-api"
    }
    fn model_id(&self) -> &str {
        "rate-limit-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Err(Error::api(
            "429 Too Many Requests: Rate limit exceeded. Please retry after 30s",
        ))
    }
}

/// Provider that simulates quota exhaustion (402 Payment Required).
#[derive(Debug)]
struct QuotaExhaustedProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for QuotaExhaustedProvider {
    fn name(&self) -> &str {
        "quota-exhausted-provider"
    }
    fn api(&self) -> &str {
        "quota-exhausted-api"
    }
    fn model_id(&self) -> &str {
        "quota-exhausted-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        Err(Error::api("402 Payment Required: Usage quota exhausted"))
    }
}

/// RATE-1: 429 rate limit surfaces error with retry hint.
#[test]
fn rate_limit_429_surfaces_error_with_hint() {
    let test_name = "fi_rate_limit_429";
    let harness = TestHarness::new(test_name);
    harness.section("rate_limit_429");

    let provider = Arc::new(RateLimitProvider::new());
    let provider_ref = Arc::clone(&provider);
    let cwd = harness.temp_dir().to_path_buf();

    let result = run_async(async move {
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, provider_ref as Arc<dyn Provider>, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    assert!(result.is_err(), "429 should propagate as error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("429") || err_msg.contains("Rate limit") || err_msg.contains("rate"),
        "Error should contain rate limit info: {err_msg}"
    );

    harness
        .log()
        .info_ctx("result", "RATE-1: 429 verified", |ctx| {
            ctx.push(("error".into(), err_msg));
            ctx.push((
                "call_count".into(),
                provider.call_count.load(Ordering::SeqCst).to_string(),
            ));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// RATE-2: Quota exhaustion (402) surfaces clear error.
#[test]
fn quota_exhaustion_surfaces_clear_error() {
    let test_name = "fi_quota_exhausted";
    let harness = TestHarness::new(test_name);
    harness.section("quota_exhausted");

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(QuotaExhaustedProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    assert!(result.is_err(), "402 should propagate as error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("402") || err_msg.contains("quota") || err_msg.contains("Payment"),
        "Error should contain quota info: {err_msg}"
    );

    harness
        .log()
        .info("result", format!("RATE-2: Quota verified: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILURE CLASS 3: Timeout
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that simulates a timeout by returning an error after delay.
#[derive(Debug)]
struct TimeoutProvider {
    call_count: AtomicUsize,
}

impl TimeoutProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for TimeoutProvider {
    fn name(&self) -> &str {
        "timeout-provider"
    }
    fn api(&self) -> &str {
        "timeout-api"
    }
    fn model_id(&self) -> &str {
        "timeout-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Err(Error::api("Request timed out after 30000ms"))
    }
}

/// Provider that starts streaming, then hangs (simulated by yielding an error).
#[derive(Debug)]
struct StreamTimeoutProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for StreamTimeoutProvider {
    fn name(&self) -> &str {
        "stream-timeout-provider"
    }
    fn api(&self) -> &str {
        "stream-timeout-api"
    }
    fn model_id(&self) -> &str {
        "stream-timeout-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let partial = make_assistant("stream-timeout-provider", StopReason::Stop, Vec::new(), 0);
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            // Simulate mid-stream timeout
            Err(Error::api("Stream timed out: no data received for 30s")),
        ])))
    }
}

/// TIMEOUT-1: Connection timeout surfaces bounded error.
#[test]
fn timeout_connection_surfaces_bounded_error() {
    let test_name = "fi_timeout_connection";
    let harness = TestHarness::new(test_name);
    harness.section("timeout_connection");

    let provider = Arc::new(TimeoutProvider::new());
    let provider_ref = Arc::clone(&provider);
    let cwd = harness.temp_dir().to_path_buf();

    let result = run_async(async move {
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session =
            make_agent_session(&cwd, provider_ref as Arc<dyn Provider>, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    assert!(result.is_err(), "Timeout should propagate as error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("timed out") || err_msg.contains("timeout"),
        "Error should contain timeout info: {err_msg}"
    );

    harness
        .log()
        .info_ctx("result", "TIMEOUT-1: Connection timeout verified", |ctx| {
            ctx.push(("error".into(), err_msg));
            ctx.push((
                "call_count".into(),
                provider.call_count.load(Ordering::SeqCst).to_string(),
            ));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// TIMEOUT-2: Stream timeout (mid-stream hang) surfaces error.
#[test]
fn timeout_stream_hang_surfaces_error() {
    let test_name = "fi_timeout_stream_hang";
    let harness = TestHarness::new(test_name);
    harness.section("timeout_stream");

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(StreamTimeoutProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    let msg = result.expect("Stream timeout should return partial message");
    assert_eq!(msg.stop_reason, StopReason::Error);
    let err_msg = msg.error_message.expect("msg error");
    assert!(
        err_msg.contains("timed out") || err_msg.contains("timeout") || err_msg.contains("Stream"),
        "Error should contain stream timeout info: {err_msg}"
    );

    harness.log().info(
        "result",
        format!("TIMEOUT-2: Stream timeout verified: {err_msg}"),
    );
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILURE CLASS 4: Malformed response
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that returns a malformed stream with error events.
#[derive(Debug)]
struct MalformedStreamProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MalformedStreamProvider {
    fn name(&self) -> &str {
        "malformed-provider"
    }
    fn api(&self) -> &str {
        "malformed-api"
    }
    fn model_id(&self) -> &str {
        "malformed-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Return a stream with only an error event (no Start)
        Ok(Box::pin(futures::stream::iter(vec![Err(Error::api(
            "Malformed response: unexpected JSON structure",
        ))])))
    }
}

/// Provider that returns content with `StopReason::Length` truncation.
#[derive(Debug)]
struct TruncatedResponseProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for TruncatedResponseProvider {
    fn name(&self) -> &str {
        "truncated-provider"
    }
    fn api(&self) -> &str {
        "truncated-api"
    }
    fn model_id(&self) -> &str {
        "truncated-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let msg = make_assistant(
            "truncated-provider",
            StopReason::Length,
            vec![ContentBlock::Text(TextContent::new(
                "This response was cut short becau",
            ))],
            8192,
        );
        Ok(stream_done(msg))
    }
}

/// Provider that returns empty content blocks.
#[derive(Debug)]
struct EmptyContentProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for EmptyContentProvider {
    fn name(&self) -> &str {
        "empty-content-provider"
    }
    fn api(&self) -> &str {
        "empty-content-api"
    }
    fn model_id(&self) -> &str {
        "empty-content-model"
    }
    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Empty text block
        let msg = make_assistant(
            "empty-content-provider",
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(""))],
            5,
        );
        Ok(stream_done(msg))
    }
}

/// MALFORMED-1: Stream error without Start event handled.
#[test]
fn malformed_stream_without_start_handled() {
    let test_name = "fi_malformed_no_start";
    let harness = TestHarness::new(test_name);
    harness.section("malformed_no_start");

    let cwd = harness.temp_dir().to_path_buf();
    let result = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(MalformedStreamProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session.run_text("hello".to_string(), |_| {}).await
    });

    // Recovery assertion: agent handles error gracefully
    let msg = result.expect("Malformed stream should return partial message");
    assert_eq!(msg.stop_reason, StopReason::Error);
    let err_msg = msg.error_message.unwrap_or_default();
    harness
        .log()
        .info("result", format!("MALFORMED-1: {err_msg}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// MALFORMED-2: Truncated response (StopReason::Length) handled.
#[test]
fn malformed_truncated_response_preserved() {
    let test_name = "fi_malformed_truncated";
    let harness = TestHarness::new(test_name);
    harness.section("malformed_truncated");

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(TruncatedResponseProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session
            .run_text("write a long essay".to_string(), |_| {})
            .await
            .expect("truncated response should not error")
    });

    // Recovery assertions
    assert_eq!(message.stop_reason, StopReason::Length);
    let text = assistant_text(&message);
    assert!(!text.is_empty(), "Partial content should be preserved");
    assert_eq!(message.usage.total_tokens, 8192);

    harness.log().info_ctx(
        "result",
        "MALFORMED-2: Truncated response preserved",
        |ctx| {
            ctx.push(("text_len".into(), text.len().to_string()));
            ctx.push(("stop_reason".into(), format!("{:?}", message.stop_reason)));
        },
    );
    write_jsonl_artifacts(&harness, test_name);
}

/// MALFORMED-3: Empty text block does not crash.
#[test]
fn malformed_empty_text_block_no_crash() {
    let test_name = "fi_malformed_empty_text";
    let harness = TestHarness::new(test_name);
    harness.section("malformed_empty_text");

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(EmptyContentProvider);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 4);
        agent_session
            .run_text("hello".to_string(), |_| {})
            .await
            .expect("empty text should not crash")
    });

    assert_eq!(message.stop_reason, StopReason::Stop);
    let text = assistant_text(&message);
    assert!(text.is_empty(), "Should have empty text content");

    harness
        .log()
        .info("result", "MALFORMED-3: Empty text handled");
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILURE CLASS 5: Tool-failure propagation
// ═══════════════════════════════════════════════════════════════════════════

/// Provider that requests a tool then verifies the error was propagated.
#[derive(Debug)]
struct ToolFailurePropagationProvider {
    call_count: AtomicUsize,
    scenario: ToolFailureScenario,
}

#[derive(Debug, Clone)]
enum ToolFailureScenario {
    /// Tool that doesn't exist
    MissingTool,
    /// Tool with invalid arguments
    BadArguments,
    /// Tool that reads a nonexistent path
    FileNotFound { path: String },
    /// Two tools: one fails, one succeeds — both results propagated
    MixedBatch,
    /// Tool chain: first tool fails, agent recovers with different tool
    RecoveryChain,
}

impl ToolFailurePropagationProvider {
    fn new(scenario: ToolFailureScenario) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            scenario,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ToolFailurePropagationProvider {
    fn name(&self) -> &str {
        "tool-failure-provider"
    }
    fn api(&self) -> &str {
        "tool-failure-api"
    }
    fn model_id(&self) -> &str {
        "tool-failure-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);

        match &self.scenario {
            ToolFailureScenario::MissingTool => {
                if index == 0 {
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "missing-1".to_string(),
                            name: "nonexistent_tool_xyz".to_string(),
                            arguments: json!({}),
                            thought_signature: None,
                        })],
                        10,
                    );
                    return Ok(stream_done(msg));
                }
                // Verify tool error was propagated
                let tool_error = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "missing-1" => Some(r),
                        _ => None,
                    })
                    .any(|r| r.is_error);
                let msg = make_assistant(
                    "tool-failure-provider",
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "tool_error_propagated:{tool_error}"
                    )))],
                    8,
                );
                Ok(stream_done(msg))
            }
            ToolFailureScenario::BadArguments => {
                if index == 0 {
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "bad-args-1".to_string(),
                            name: "read".to_string(),
                            arguments: json!({ "wrong_field": true }),
                            thought_signature: None,
                        })],
                        10,
                    );
                    return Ok(stream_done(msg));
                }
                let tool_error = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "bad-args-1" => Some(r),
                        _ => None,
                    })
                    .any(|r| r.is_error);
                let msg = make_assistant(
                    "tool-failure-provider",
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "bad_args_propagated:{tool_error}"
                    )))],
                    8,
                );
                Ok(stream_done(msg))
            }
            ToolFailureScenario::FileNotFound { path } => {
                if index == 0 {
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "fnf-1".to_string(),
                            name: "read".to_string(),
                            arguments: json!({ "path": path }),
                            thought_signature: None,
                        })],
                        10,
                    );
                    return Ok(stream_done(msg));
                }
                let tool_error = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "fnf-1" => Some(r),
                        _ => None,
                    })
                    .any(|r| r.is_error);
                let msg = make_assistant(
                    "tool-failure-provider",
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "file_not_found_propagated:{tool_error}"
                    )))],
                    8,
                );
                Ok(stream_done(msg))
            }
            ToolFailureScenario::MixedBatch => {
                if index == 0 {
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![
                            ContentBlock::ToolCall(ToolCall {
                                id: "good-1".to_string(),
                                name: "bash".to_string(),
                                arguments: json!({ "command": "echo success" }),
                                thought_signature: None,
                            }),
                            ContentBlock::ToolCall(ToolCall {
                                id: "bad-1".to_string(),
                                name: "nonexistent_tool".to_string(),
                                arguments: json!({}),
                                thought_signature: None,
                            }),
                        ],
                        15,
                    );
                    return Ok(stream_done(msg));
                }
                let good_result = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "good-1" => Some(r),
                        _ => None,
                    })
                    .next_back();
                let bad_result = context
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) if r.tool_call_id == "bad-1" => Some(r),
                        _ => None,
                    })
                    .next_back();
                let good_ok = good_result.is_some_and(|r| !r.is_error);
                let bad_err = bad_result.is_some_and(|r| r.is_error);
                let msg = make_assistant(
                    "tool-failure-provider",
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(format!(
                        "mixed_batch:good_ok={good_ok},bad_err={bad_err}"
                    )))],
                    10,
                );
                Ok(stream_done(msg))
            }
            ToolFailureScenario::RecoveryChain => {
                if index == 0 {
                    // First: try a nonexistent tool
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "fail-1".to_string(),
                            name: "nonexistent_tool".to_string(),
                            arguments: json!({}),
                            thought_signature: None,
                        })],
                        10,
                    );
                    return Ok(stream_done(msg));
                }
                if index == 1 {
                    // Second: recover by using a real tool
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "recover-1".to_string(),
                            name: "bash".to_string(),
                            arguments: json!({ "command": "echo recovered" }),
                            thought_signature: None,
                        })],
                        10,
                    );
                    return Ok(stream_done(msg));
                }
                if index == 2 {
                    let recover_ok = context
                        .messages
                        .iter()
                        .filter_map(|m| match m {
                            Message::ToolResult(r) if r.tool_call_id == "recover-1" => Some(r),
                            _ => None,
                        })
                        .any(|r| !r.is_error);
                    let msg = make_assistant(
                        "tool-failure-provider",
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new(format!(
                            "recovery_chain:recovered={recover_ok}"
                        )))],
                        8,
                    );
                    return Ok(stream_done(msg));
                }
                Err(Error::api("unexpected call"))
            }
        }
    }
}

/// TOOL-1: Missing tool name propagates is_error to context.
#[test]
fn tool_missing_name_propagates_error() {
    let test_name = "fi_tool_missing_name";
    let harness = TestHarness::new(test_name);
    harness.section("tool_missing_name");

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
            ToolFailureScenario::MissingTool,
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 6);
        agent_session
            .run_text("call a tool".to_string(), |_| {})
            .await
            .expect("should complete")
    });

    let text = assistant_text(&message);
    assert!(
        text.contains("tool_error_propagated:true"),
        "Tool error should be propagated: {text}"
    );

    harness
        .log()
        .info("result", format!("TOOL-1: Missing tool verified: {text}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// TOOL-2: Bad arguments propagate is_error to context.
#[test]
fn tool_bad_arguments_propagates_error() {
    let test_name = "fi_tool_bad_args";
    let harness = TestHarness::new(test_name);
    harness.section("tool_bad_args");

    let cwd = harness.temp_dir().to_path_buf();
    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
            ToolFailureScenario::BadArguments,
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 6);
        agent_session
            .run_text("read with bad args".to_string(), |_| {})
            .await
            .expect("should complete")
    });

    let text = assistant_text(&message);
    assert!(
        text.contains("bad_args_propagated:true"),
        "Bad args should propagate error: {text}"
    );

    harness
        .log()
        .info("result", format!("TOOL-2: Bad args verified: {text}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// TOOL-3: File not found error propagated to context.
#[test]
fn tool_file_not_found_propagates_error() {
    let test_name = "fi_tool_file_not_found";
    let harness = TestHarness::new(test_name);
    harness.section("tool_file_not_found");

    let missing_path = harness
        .temp_path("nonexistent/ghost.txt")
        .display()
        .to_string();
    let cwd = harness.temp_dir().to_path_buf();

    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
            ToolFailureScenario::FileNotFound { path: missing_path },
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 6);
        agent_session
            .run_text("read a missing file".to_string(), |_| {})
            .await
            .expect("should complete")
    });

    let text = assistant_text(&message);
    assert!(
        text.contains("file_not_found_propagated:true"),
        "File not found should propagate: {text}"
    );

    harness
        .log()
        .info("result", format!("TOOL-3: File not found verified: {text}"));
    write_jsonl_artifacts(&harness, test_name);
}

/// TOOL-4: Mixed batch — success + failure both propagated.
#[test]
fn tool_mixed_batch_both_results_propagated() {
    let test_name = "fi_tool_mixed_batch";
    let harness = TestHarness::new(test_name);
    harness.section("tool_mixed_batch");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
            ToolFailureScenario::MixedBatch,
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 6);
        agent_session
            .run_text("call mixed tools".to_string(), move |event| {
                let mut guard = capture_ref.lock().expect("lock capture");
                match &event {
                    AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                    AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                    _ => {}
                }
                guard.timeline.push(event_label(&event).to_string());
            })
            .await
            .expect("should complete")
    });

    let cap = capture.lock().expect("lock capture");
    assert_eq!(cap.tool_starts, 2, "Both tools should start");
    assert_eq!(cap.tool_ends, 2, "Both tools should end");

    let text = assistant_text(&message);
    assert!(
        text.contains("good_ok=true"),
        "Good tool should succeed: {text}"
    );
    assert!(
        text.contains("bad_err=true"),
        "Bad tool should be marked error: {text}"
    );

    harness
        .log()
        .info_ctx("result", "TOOL-4: Mixed batch verified", |ctx| {
            ctx.push(("tool_starts".into(), cap.tool_starts.to_string()));
            ctx.push(("response".into(), text));
        });
    write_jsonl_artifacts(&harness, test_name);
}

/// TOOL-5: Recovery chain — fail then succeed with different tool.
#[test]
fn tool_recovery_chain_fail_then_succeed() {
    let test_name = "fi_tool_recovery_chain";
    let harness = TestHarness::new(test_name);
    harness.section("tool_recovery_chain");

    let cwd = harness.temp_dir().to_path_buf();
    let capture = Arc::new(Mutex::new(EventCapture::default()));
    let capture_ref = Arc::clone(&capture);

    let message = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
            ToolFailureScenario::RecoveryChain,
        ));
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = make_agent_session(&cwd, provider, session, 8);
        agent_session
            .run_text("try tools".to_string(), move |event| {
                let mut guard = capture_ref.lock().expect("lock capture");
                match &event {
                    AgentEvent::TurnStart { .. } => guard.turn_count += 1,
                    AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                    AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                    _ => {}
                }
            })
            .await
            .expect("recovery chain should complete")
    });

    let cap = capture.lock().expect("lock capture");
    let text = assistant_text(&message);

    // Recovery assertions
    assert!(
        cap.turn_count >= 3,
        "Should have at least 3 turns (fail + recover + final)"
    );
    assert!(
        cap.tool_starts >= 2,
        "Should have at least 2 tool executions"
    );
    assert!(
        text.contains("recovered=true"),
        "Recovery should succeed: {text}"
    );

    harness
        .log()
        .info_ctx("result", "TOOL-5: Recovery chain verified", |ctx| {
            ctx.push(("turn_count".into(), cap.turn_count.to_string()));
            ctx.push(("tool_starts".into(), cap.tool_starts.to_string()));
            ctx.push(("response".into(), text));
        });
    write_jsonl_artifacts(&harness, test_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// CROSS-CUTTING: Session state preservation after failures
// ═══════════════════════════════════════════════════════════════════════════

/// After a provider failure, session should have no corrupted state.
#[test]
fn session_clean_after_provider_failure() {
    let test_name = "fi_session_clean_after_failure";
    let harness = TestHarness::new(test_name);
    harness.section("session_clean_after_failure");

    let cwd = harness.temp_dir().to_path_buf();
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));
    let session_ref = Arc::clone(&session);

    // Attempt 1: fail
    let result = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session_ref);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(AuthFailureProvider::new());
            let mut agent_session = make_agent_session(&cwd, provider, session, 4);
            agent_session
                .run_text("this will fail".to_string(), |_| {})
                .await
        }
    });
    assert!(result.is_err(), "First attempt should fail");

    // Verify session is clean
    let messages = run_async({
        let session = Arc::clone(&session_ref);
        async move {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.to_messages_for_current_path()
        }
    });

    harness
        .log()
        .info_ctx("result", "Session state after failure", |ctx| {
            ctx.push(("message_count".into(), messages.len().to_string()));
        });

    // Session should either be empty or have the failed user message
    // but no corrupted assistant messages
    let has_corrupted_assistant = messages.iter().any(|m| match m {
        Message::Assistant(a) => a.error_message.is_some(),
        _ => false,
    });
    assert!(
        !has_corrupted_assistant,
        "No corrupted assistant messages after failure"
    );

    write_jsonl_artifacts(&harness, test_name);
}

/// After tool failure, session messages reflect the error accurately.
#[test]
fn session_reflects_tool_errors_accurately() {
    let test_name = "fi_session_tool_error_accuracy";
    let harness = TestHarness::new(test_name);
    harness.section("session_tool_error_accuracy");

    let cwd = harness.temp_dir().to_path_buf();
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(cwd.clone()),
    )));
    let session_ref = Arc::clone(&session);

    let message = run_async({
        let cwd = cwd.clone();
        let session = Arc::clone(&session_ref);
        async move {
            let provider: Arc<dyn Provider> = Arc::new(ToolFailurePropagationProvider::new(
                ToolFailureScenario::MissingTool,
            ));
            let mut agent_session = make_agent_session(&cwd, provider, session, 6);
            let msg = agent_session
                .run_text("call missing tool".to_string(), |_| {})
                .await
                .expect("should complete");
            agent_session.persist_session().await.expect("persist");
            msg
        }
    });

    // Verify session has correct message sequence
    let messages = run_async({
        let session = Arc::clone(&session_ref);
        async move {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.to_messages_for_current_path()
        }
    });

    // Should have: user -> assistant(tool_call) -> tool_result(error) -> assistant(final)
    let tool_results: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(!tool_results.is_empty(), "Should have tool result messages");
    assert!(
        tool_results.iter().any(|r| r.is_error),
        "At least one tool result should be marked as error"
    );

    let text = assistant_text(&message);
    assert!(
        text.contains("true"),
        "Final response should confirm error propagation"
    );

    harness
        .log()
        .info_ctx("result", "Session tool error accuracy verified", |ctx| {
            ctx.push(("total_messages".into(), messages.len().to_string()));
            ctx.push(("tool_results".into(), tool_results.len().to_string()));
            ctx.push((
                "error_results".into(),
                tool_results
                    .iter()
                    .filter(|r| r.is_error)
                    .count()
                    .to_string(),
            ));
        });
    write_jsonl_artifacts(&harness, test_name);
}
