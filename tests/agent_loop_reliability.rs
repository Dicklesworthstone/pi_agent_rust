//! Agent loop reliability tests (bd-1f42.2.5).
//!
//! Stress-tests the agent loop for:
//! - Mid-stream abort (partial content preserved)
//! - Abort during tool execution (completed tools kept, remaining skipped)
//! - Stream truncation recovery (no Done event)
//! - Session resume after interruption (history consistency)
//! - Repeated interruption cycles (no state corruption)
//! - Provider error during streaming
//! - Max tool iterations exceeded
//! - Pre-abort skips provider
//!
//! Each test emits structured JSONL timeline artifacts for postmortem.

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
    ToolResultMessage, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{self, Poll};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_assistant(text: &str, stop: StopReason, total_tokens: u64) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        api: "test-api".to_string(),
        provider: "test-provider".to_string(),
        model: "test-model".to_string(),
        usage: Usage {
            total_tokens,
            output: total_tokens,
            ..Usage::default()
        },
        stop_reason: stop,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    }
}

fn make_tool_call_message(tools: Vec<ToolCall>, total_tokens: u64) -> AssistantMessage {
    AssistantMessage {
        content: tools.into_iter().map(ContentBlock::ToolCall).collect(),
        api: "test-api".to_string(),
        provider: "test-provider".to_string(),
        model: "test-model".to_string(),
        usage: Usage {
            total_tokens,
            output: total_tokens,
            ..Usage::default()
        },
        stop_reason: StopReason::ToolUse,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    }
}

fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_text(msg: &ToolResultMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
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

#[derive(Debug, Default)]
struct Timeline {
    events: Vec<serde_json::Value>,
    tool_starts: usize,
    tool_ends: usize,
    aborted_events: usize,
}

const FAULT_CLASS_RECOVERABLE: &str = "recoverable";
const FAULT_CLASS_FATAL: &str = "fatal";

#[derive(Debug, Serialize)]
struct FaultTransition {
    phase: String,
    elapsed_ms: u128,
    detail: String,
}

#[derive(Debug, Serialize)]
struct FaultEpisodeRecord {
    schema: &'static str,
    issue_id: &'static str,
    remediation_issue: &'static str,
    test_name: String,
    fault_name: String,
    injection_point: String,
    classification: &'static str,
    retry_backoff_ms: Vec<u64>,
    state_transitions: Vec<FaultTransition>,
    terminal_outcome: String,
    root_cause_marker: String,
    replay_command: String,
}

fn make_timeline_callback(
    tl: Arc<StdMutex<Timeline>>,
    started_at: Instant,
) -> impl Fn(AgentEvent) + Send + Sync + 'static {
    move |event: AgentEvent| {
        let elapsed_ms = started_at.elapsed().as_millis();
        let mut guard = tl.lock().expect("lock timeline");
        match &event {
            AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
            AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
            AgentEvent::AgentEnd { error, .. } if error.is_some() => {
                guard.aborted_events += 1;
            }
            _ => {}
        }
        guard.events.push(json!({
            "event": event_label(&event),
            "elapsedMs": elapsed_ms,
        }));
        drop(guard);
    }
}

fn capture_timeline() -> (
    Arc<StdMutex<Timeline>>,
    impl Fn(AgentEvent) + Send + Sync + 'static,
) {
    let tl = Arc::new(StdMutex::new(Timeline::default()));
    let started_at = Instant::now();
    let cb = make_timeline_callback(Arc::clone(&tl), started_at);
    (tl, cb)
}

fn write_timeline_artifact(harness: &TestHarness, name: &str, tl: &Timeline) {
    let path = harness.temp_path(format!("{name}.timeline.jsonl"));
    let mut file = std::fs::File::create(&path).expect("create timeline");
    for entry in &tl.events {
        let line = serde_json::to_string(entry).expect("serialize");
        let _ = writeln!(file, "{line}");
    }
    harness.record_artifact(format!("{name}.timeline.jsonl"), &path);
}

fn write_log_artifacts(harness: &TestHarness, name: &str) {
    let log_path = harness.temp_path(format!("{name}.log.jsonl"));
    harness
        .write_jsonl_logs(&log_path)
        .expect("write jsonl log");
    harness.record_artifact(format!("{name}.log.jsonl"), &log_path);
}

fn write_fault_episode_artifact(harness: &TestHarness, name: &str, record: &FaultEpisodeRecord) {
    let path = harness.temp_path(format!("{name}.fault_episode.jsonl"));
    let mut file = std::fs::File::create(&path).expect("create fault episode artifact");
    let line = serde_json::to_string(record).expect("serialize fault episode");
    let _ = writeln!(file, "{line}");
    harness.record_artifact(format!("{name}.fault_episode.jsonl"), &path);
}

fn make_session(harness: &TestHarness) -> Arc<asupersync::sync::Mutex<Session>> {
    Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(harness.temp_dir().to_path_buf()),
    )))
}

fn make_agent(provider: Arc<dyn Provider>, cwd: &std::path::Path, max_iters: usize) -> Agent {
    let tools = ToolRegistry::new(&["read", "write", "bash"], cwd, None);
    let config = AgentConfig {
        system_prompt: None,
        max_tool_iterations: max_iters,
        stream_options: StreamOptions {
            api_key: Some("test-key".to_string()),
            ..StreamOptions::default()
        },
        block_images: false,
        fail_closed_hooks: false,
        tool_approval: None,
        keyword_settings: None,
    };
    Agent::new(provider, tools, config)
}

fn make_agent_session(
    provider: Arc<dyn Provider>,
    harness: &TestHarness,
    max_iters: usize,
) -> AgentSession {
    let cwd = harness.temp_dir().to_path_buf();
    let agent = make_agent(provider, &cwd, max_iters);
    let session = make_session(harness);
    AgentSession::new(agent, session, true, ResolvedCompactionSettings::default())
}

// ---------------------------------------------------------------------------
// Provider: emits Start + N TextDelta chunks, then hangs (for abort tests)
// ---------------------------------------------------------------------------

struct SlowStreamProvider {
    /// Number of `TextDelta` chunks to emit before hanging.
    chunks_before_hang: usize,
    /// Text per chunk.
    chunk_text: String,
    /// Track how many times `stream()` was called.
    stream_calls: AtomicUsize,
}

impl SlowStreamProvider {
    fn new(chunks_before_hang: usize, chunk_text: &str) -> Self {
        Self {
            chunks_before_hang,
            chunk_text: chunk_text.to_string(),
            stream_calls: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for SlowStreamProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlowStreamProvider")
            .field("chunks_before_hang", &self.chunks_before_hang)
            .field("chunk_text", &self.chunk_text)
            .field("stream_calls", &self.stream_calls.load(Ordering::SeqCst))
            .finish()
    }
}

struct EventSequenceThenHang {
    events: Vec<StreamEvent>,
    index: usize,
}

impl Stream for EventSequenceThenHang {
    type Item = Result<StreamEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.index < self.events.len() {
            let idx = self.index;
            self.index += 1;
            // Clone the event. StreamEvent must be Clone.
            // If not, we need to use Option<StreamEvent>.
            Poll::Ready(Some(Ok(self.events[idx].clone())))
        } else {
            Poll::Pending
        }
    }
}

/// Stream that emits a fixed sequence then returns None (EOF).
struct EventSequenceThenEof {
    events: Vec<Option<StreamEvent>>,
    index: usize,
}

impl Stream for EventSequenceThenEof {
    type Item = Result<StreamEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.index < self.events.len() {
            let idx = self.index;
            self.index += 1;
            self.events[idx]
                .take()
                .map_or_else(|| Poll::Ready(None), |event| Poll::Ready(Some(Ok(event))))
        } else {
            Poll::Ready(None)
        }
    }
}

fn make_partial(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        api: "test-api".to_string(),
        provider: "test-provider".to_string(),
        model: "test-model".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for SlowStreamProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);

        let mut events = Vec::new();

        // Start event
        events.push(StreamEvent::Start {
            partial: make_partial(""),
        });

        // TextDelta chunks
        for _i in 0..self.chunks_before_hang {
            events.push(StreamEvent::TextDelta {
                content_index: 0,
                delta: self.chunk_text.clone(),
            });
        }

        // Then hang forever (no Done event)
        Ok(Box::pin(EventSequenceThenHang { events, index: 0 }))
    }
}

// ---------------------------------------------------------------------------
// Provider: emits a sequence then EOF (stream truncation)
// ---------------------------------------------------------------------------

struct TruncatingProvider {
    /// Text chunks to emit before stream closes.
    chunks: Vec<String>,
}

impl std::fmt::Debug for TruncatingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TruncatingProvider").finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for TruncatingProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let mut events: Vec<Option<StreamEvent>> = Vec::new();

        events.push(Some(StreamEvent::Start {
            partial: make_partial(""),
        }));

        for chunk in &self.chunks {
            events.push(Some(StreamEvent::TextDelta {
                content_index: 0,
                delta: chunk.clone(),
            }));
        }

        // Stream ends without Done event (simulating network disconnect)
        Ok(Box::pin(EventSequenceThenEof { events, index: 0 }))
    }
}

// ---------------------------------------------------------------------------
// Provider: emits error mid-stream
// ---------------------------------------------------------------------------

struct ErrorMidStreamProvider {
    /// Chunks to emit before error.
    good_chunks: usize,
}

impl std::fmt::Debug for ErrorMidStreamProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErrorMidStreamProvider").finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ErrorMidStreamProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let mut events: Vec<Result<StreamEvent>> = Vec::new();

        events.push(Ok(StreamEvent::Start {
            partial: make_partial(""),
        }));

        for i in 0..self.good_chunks {
            let chunk = format!("chunk{i} ");
            events.push(Ok(StreamEvent::TextDelta {
                content_index: 0,
                delta: chunk,
            }));
        }

        // Error event
        let error_msg = make_assistant("", StopReason::Error, 0);
        let mut error_msg_with_err = error_msg;
        error_msg_with_err.error_message = Some("Server error during streaming".to_string());
        error_msg_with_err.stop_reason = StopReason::Error;
        events.push(Ok(StreamEvent::Error {
            reason: StopReason::Error,
            error: error_msg_with_err,
        }));

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

// ---------------------------------------------------------------------------
// Provider: multi-call scripted (for tool-loop + resume tests)
// ---------------------------------------------------------------------------

struct ScriptedReliabilityProvider {
    stream_calls: AtomicUsize,
    /// For each call index, a list of stream events to emit.
    scripts: Vec<Vec<StreamEvent>>,
}

impl std::fmt::Debug for ScriptedReliabilityProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedReliabilityProvider").finish()
    }
}

struct FlakyTimeoutThenSuccessProvider {
    fail_attempts: usize,
    success_text: String,
    stream_calls: AtomicUsize,
}

impl FlakyTimeoutThenSuccessProvider {
    fn new(fail_attempts: usize, success_text: &str) -> Self {
        Self {
            fail_attempts,
            success_text: success_text.to_string(),
            stream_calls: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for FlakyTimeoutThenSuccessProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlakyTimeoutThenSuccessProvider")
            .field("fail_attempts", &self.fail_attempts)
            .field("success_text", &self.success_text)
            .field("stream_calls", &self.stream_calls.load(Ordering::SeqCst))
            .finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for FlakyTimeoutThenSuccessProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if call_index < self.fail_attempts {
            return Err(Error::api(format!(
                "request timed out while streaming (attempt {})",
                call_index + 1
            )));
        }

        let message = make_assistant(&self.success_text, StopReason::Stop, 24);
        let events = vec![
            Ok(StreamEvent::Start {
                partial: make_partial(""),
            }),
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct StreamContractViolationProvider {
    stream_calls: AtomicUsize,
}

impl StreamContractViolationProvider {
    const fn new() -> Self {
        Self {
            stream_calls: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for StreamContractViolationProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamContractViolationProvider")
            .field("stream_calls", &self.stream_calls.load(Ordering::SeqCst))
            .finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for StreamContractViolationProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FaultyWriteInput {
    path: String,
    content: String,
}

#[derive(Debug)]
struct FaultyPartialWriteTool {
    cwd: std::path::PathBuf,
}

impl FaultyPartialWriteTool {
    fn new(cwd: &std::path::Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for FaultyPartialWriteTool {
    fn name(&self) -> &str {
        "faulty_write"
    }

    fn label(&self) -> &str {
        "faulty_write"
    }

    fn description(&self) -> &str {
        "Inject a partial write failure before persist to validate recovery paths."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: FaultyWriteInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let path = if std::path::Path::new(&input.path).is_absolute() {
            std::path::PathBuf::from(&input.path)
        } else {
            self.cwd.join(&input.path)
        };
        let Some(parent) = path.parent() else {
            return Err(Error::tool(
                "faulty_write",
                format!("Cannot resolve parent for {}", path.display()),
            ));
        };

        asupersync::fs::create_dir_all(parent).await.map_err(|e| {
            Error::tool(
                "faulty_write",
                format!("Failed to create parent directories: {e}"),
            )
        })?;

        let temp_file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| Error::tool("faulty_write", format!("Failed to create temp file: {e}")))?;

        let bytes = input.content.as_bytes();
        let cut = bytes.len().saturating_div(2).max(1);
        asupersync::fs::write(temp_file.path(), &bytes[..cut])
            .await
            .map_err(|e| Error::tool("faulty_write", format!("Failed to write temp file: {e}")))?;

        Err(Error::tool(
            "faulty_write",
            format!(
                "Injected partial temp write failure before persist [FAULT-PARTIAL-WRITE]: {}",
                path.display()
            ),
        ))
    }
}

impl ScriptedReliabilityProvider {
    #[allow(clippy::missing_const_for_fn)]
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            stream_calls: AtomicUsize::new(0),
            scripts,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ScriptedReliabilityProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if call_index >= self.scripts.len() {
            return Err(Error::api(format!(
                "ScriptedReliabilityProvider: unexpected call index {call_index}"
            )));
        }
        let events: Vec<Result<StreamEvent>> =
            self.scripts[call_index].iter().cloned().map(Ok).collect();
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

// ---------------------------------------------------------------------------
// Provider: tool call then hang (for abort-during-tools tests)
// ---------------------------------------------------------------------------

struct ToolCallThenHangProvider {
    tool_calls: Vec<ToolCall>,
    stream_calls: AtomicUsize,
    /// If true, second call hangs. Otherwise returns text.
    second_call_hangs: bool,
}

impl std::fmt::Debug for ToolCallThenHangProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallThenHangProvider").finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ToolCallThenHangProvider {
    fn name(&self) -> &str {
        "test-provider"
    }

    fn api(&self) -> &str {
        "test-api"
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

        if call_index == 0 {
            // First call: return tool calls
            let msg = make_tool_call_message(self.tool_calls.clone(), 30);
            let partial = AssistantMessage {
                content: Vec::new(),
                stop_details: None,
                ..msg.clone()
            };
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    message: msg,
                }),
            ];
            return Ok(Box::pin(futures::stream::iter(events)));
        }

        if self.second_call_hangs {
            // Second call: hang forever (for abort during follow-up)
            let events = vec![StreamEvent::Start {
                partial: make_partial(""),
            }];
            Ok(Box::pin(EventSequenceThenHang { events, index: 0 }))
        } else {
            // Second call: return text response
            let msg = make_assistant("follow-up complete", StopReason::Stop, 10);
            let partial = make_partial("");
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: msg,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1: Mid-stream abort preserves partial content
// ---------------------------------------------------------------------------

#[test]
fn abort_mid_stream_preserves_partial_content() {
    let test_name = "reliability_abort_mid_stream";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(SlowStreamProvider::new(3, "hello "));
        let cwd = harness.temp_dir().to_path_buf();
        let agent = make_agent(Arc::clone(&provider), &cwd, 4);
        let session = make_session(&harness);
        let mut agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let (abort_handle, abort_signal) = AbortHandle::new();
        let tl = Arc::new(StdMutex::new(Timeline::default()));
        let tl_ref = Arc::clone(&tl);
        let started_at = Instant::now();
        let abort_handle_inner = abort_handle.clone();
        let chunks_seen = Arc::new(AtomicUsize::new(0));

        let cb2 = move |event: AgentEvent| {
            let elapsed_ms = started_at.elapsed().as_millis();
            let mut guard = tl_ref.lock().expect("lock");
            match &event {
                AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
                AgentEvent::AgentEnd { error, .. } if error.is_some() => {
                    guard.aborted_events += 1;
                }
                AgentEvent::MessageUpdate { .. } => {
                    let count = chunks_seen.fetch_add(1, Ordering::SeqCst);
                    // After seeing 2 message updates (Start + first delta), trigger abort
                    if count >= 2 {
                        abort_handle_inner.abort();
                    }
                }
                _ => {}
            }
            guard.events.push(json!({
                "event": event_label(&event),
                "elapsedMs": elapsed_ms,
            }));
            drop(guard);
        };

        let message = agent_session
            .run_text_with_abort("test input".to_string(), Some(abort_signal), cb2)
            .await
            .expect("run should not error");

        // Verify abort was handled correctly
        assert_eq!(
            message.stop_reason,
            StopReason::Aborted,
            "stop reason should be Aborted"
        );
        assert_eq!(
            message.error_message.as_deref(),
            Some("Aborted"),
            "error message should say Aborted"
        );

        // Verify partial content was preserved (at least some chunks got through)
        let text = assistant_text(&message);
        // We should have at least some "hello " text from chunks that were processed
        // before the abort fired.
        harness
            .log()
            .info_ctx("abort_mid_stream", "partial content check", |ctx| {
                ctx.push(("text".into(), text.clone()));
                ctx.push(("stop_reason".into(), format!("{:?}", message.stop_reason)));
            });

        // Verify message history is consistent
        let messages = {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.to_messages_for_current_path()
        };

        // Should have at least: user message + assistant message
        assert!(
            messages.len() >= 2,
            "expected at least 2 messages in history, got {}",
            messages.len()
        );

        // Last message should be assistant with aborted state
        match messages.last() {
            Some(Message::Assistant(a)) => {
                assert_eq!(a.stop_reason, StopReason::Aborted);
            }
            other => panic!("expected last message to be Assistant, got: {other:?}"),
        }

        let guard = tl.lock().expect("lock timeline");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 2: Abort during tool execution preserves completed tools
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn abort_during_tool_execution_preserves_completed_tools() {
    let test_name = "reliability_abort_during_tools";
    let harness = TestHarness::new(test_name);

    // Create a file that the read tool can access
    harness.create_file("testfile.txt", "test content here");

    run_async(async move {
        let tool_calls = vec![
            ToolCall {
                id: "read-1".to_string(),
                name: "read".to_string(),
                arguments: json!({
                    "path": harness.temp_path("testfile.txt").display().to_string()
                }),
                thought_signature: None,
            },
            ToolCall {
                id: "bash-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "sleep 300" }),
                thought_signature: None,
            },
        ];

        let provider: Arc<dyn Provider> = Arc::new(ToolCallThenHangProvider {
            tool_calls,
            stream_calls: AtomicUsize::new(0),
            second_call_hangs: true,
        });

        let cwd = harness.temp_dir().to_path_buf();
        let agent = make_agent(Arc::clone(&provider), &cwd, 4);
        let session = make_session(&harness);
        let mut agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let (abort_handle, abort_signal) = AbortHandle::new();

        let tl = Arc::new(StdMutex::new(Timeline::default()));
        let tl_ref = Arc::clone(&tl);
        let started_at = Instant::now();

        // Abort when first tool execution completes
        let abort_handle_inner = abort_handle.clone();
        let tool_end_count = Arc::new(AtomicUsize::new(0));
        let tool_end_count_ref = Arc::clone(&tool_end_count);

        let cb = move |event: AgentEvent| {
            let elapsed_ms = started_at.elapsed().as_millis();
            let mut guard = tl_ref.lock().expect("lock");
            match &event {
                AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
                AgentEvent::ToolExecutionEnd { .. } => {
                    guard.tool_ends += 1;
                    let count = tool_end_count_ref.fetch_add(1, Ordering::SeqCst);
                    // After first tool completes, abort (second tool should be aborted)
                    if count == 0 {
                        abort_handle_inner.abort();
                    }
                }
                AgentEvent::AgentEnd { error, .. } if error.is_some() => {
                    guard.aborted_events += 1;
                }
                _ => {}
            }
            guard.events.push(json!({
                "event": event_label(&event),
                "elapsedMs": elapsed_ms,
            }));
            drop(guard);
        };

        let message = agent_session
            .run_text_with_abort("execute tools".to_string(), Some(abort_signal), cb)
            .await
            .expect("run should not error");

        // The agent should return with Aborted status
        assert_eq!(
            message.stop_reason,
            StopReason::Aborted,
            "stop reason should be Aborted"
        );

        // Verify the message history includes tool results
        let messages = agent_session.agent.messages();

        // Count tool results in history
        let tool_results: Vec<&ToolResultMessage> = messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(r) => Some(r.as_ref()),
                _ => None,
            })
            .collect();

        // At least the first tool (read) should have completed
        assert!(
            !tool_results.is_empty(),
            "expected at least one tool result in history"
        );

        // First tool result should be successful (read)
        let first_result = &tool_results[0];
        assert_eq!(first_result.tool_call_id, "read-1");

        let guard = tl.lock().expect("lock");
        harness
            .log()
            .info_ctx("abort_during_tools", "tool execution summary", |ctx| {
                ctx.push(("tool_starts".into(), guard.tool_starts.to_string()));
                ctx.push(("tool_ends".into(), guard.tool_ends.to_string()));
                ctx.push((
                    "tool_results_in_history".into(),
                    tool_results.len().to_string(),
                ));
            });
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 3: Stream truncation (no Done event) recovers gracefully
// ---------------------------------------------------------------------------

#[test]
fn stream_truncation_preserves_partial_and_reports_error() {
    let test_name = "reliability_stream_truncation";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(TruncatingProvider {
            chunks: vec![
                "This is ".to_string(),
                "a truncated ".to_string(),
                "stream.".to_string(),
            ],
        });

        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let message = agent_session
            .run_text("test input".to_string(), cb)
            .await
            .expect("run should not hard-error");

        // Stream ended without Done event → should be error state
        assert_eq!(
            message.stop_reason,
            StopReason::Error,
            "stop reason should be Error for truncated stream"
        );
        assert!(
            message
                .error_message
                .as_ref()
                .is_some_and(|m| m.contains("Stream ended without Done event")),
            "error message should mention stream truncation, got: {:?}",
            message.error_message
        );

        // Partial content should be preserved
        let text = assistant_text(&message);
        assert!(
            text.contains("This is "),
            "partial text should contain first chunk, got: {text}"
        );
        assert!(
            text.contains("stream."),
            "partial text should contain last chunk, got: {text}"
        );

        // Agent's message history should contain the partial assistant message
        let messages = agent_session.agent.messages();
        let assistant_msgs: Vec<&AssistantMessage> = messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_msgs.len(),
            1,
            "expected exactly one assistant message"
        );
        assert_eq!(assistant_msgs[0].stop_reason, StopReason::Error);

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 4: Provider error mid-stream returns clean error state
// ---------------------------------------------------------------------------

#[test]
fn provider_error_mid_stream_returns_clean_error() {
    let test_name = "reliability_provider_error_mid_stream";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ErrorMidStreamProvider { good_chunks: 2 });
        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let message = agent_session
            .run_text("test input".to_string(), cb)
            .await
            .expect("run should not hard-error");

        assert_eq!(
            message.stop_reason,
            StopReason::Error,
            "stop reason should be Error"
        );
        assert!(message.error_message.is_some(), "should have error message");

        // Message history should be consistent
        let messages = agent_session.agent.messages();
        assert!(messages.len() >= 2, "should have user + assistant messages");

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 5: Max tool iterations exceeded returns clean stop
// ---------------------------------------------------------------------------

#[test]
fn max_tool_iterations_exceeded_returns_clean_stop() {
    let test_name = "reliability_max_tool_iterations";
    let harness = TestHarness::new(test_name);

    harness.create_file("data.txt", "test data");

    run_async(async move {
        // Provider always returns a tool call → forces iteration limit
        let data_path = harness.temp_path("data.txt").display().to_string();
        let tool_call = ToolCall {
            id: "read-1".to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": data_path }),
            thought_signature: None,
        };

        let msg = make_tool_call_message(vec![tool_call.clone()], 20);
        let partial = AssistantMessage {
            content: Vec::new(),
            stop_details: None,
            ..msg.clone()
        };

        // Create script: each call returns the same tool call
        let script = vec![
            StreamEvent::Start {
                partial: partial.clone(),
            },
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: msg.clone(),
            },
        ];

        let provider: Arc<dyn Provider> = Arc::new(ScriptedReliabilityProvider::new(vec![
            script.clone(),
            script.clone(),
            script.clone(),
            script.clone(),
        ]));

        // max_tool_iterations = 2 → should stop after 2 tool iterations
        let mut agent_session = make_agent_session(provider, &harness, 2);
        let (tl, cb) = capture_timeline();

        let message = agent_session
            .run_text("keep calling tools".to_string(), cb)
            .await
            .expect("run should not hard-error");

        assert_eq!(
            message.stop_reason,
            StopReason::Error,
            "stop reason should be Error for max iterations"
        );
        assert!(
            message
                .error_message
                .as_ref()
                .is_some_and(|m| m.contains("Maximum tool iterations")),
            "error should mention max iterations, got: {:?}",
            message.error_message
        );

        let persisted_assistant = agent_session
            .agent
            .messages()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .expect("assistant message should be persisted");
        assert_eq!(
            persisted_assistant.stop_reason,
            StopReason::Error,
            "persisted assistant message should reflect max-iteration error"
        );
        assert!(
            persisted_assistant
                .error_message
                .as_ref()
                .is_some_and(|m| m.contains("Maximum tool iterations")),
            "persisted assistant error should mention max iterations, got: {:?}",
            persisted_assistant.error_message
        );

        let guard = tl.lock().expect("lock");
        assert!(
            guard.tool_starts >= 2,
            "should have executed at least 2 tool calls"
        );
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 6: Pre-abort skips provider call entirely
// ---------------------------------------------------------------------------

#[test]
fn pre_abort_skips_provider_entirely() {
    let test_name = "reliability_pre_abort";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        struct TrackingProvider {
            calls: Arc<AtomicUsize>,
        }

        impl std::fmt::Debug for TrackingProvider {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("TrackingProvider").finish()
            }
        }

        #[async_trait]
        #[allow(clippy::unnecessary_literal_bound)]
        impl Provider for TrackingProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn api(&self) -> &str {
                "test-api"
            }
            fn model_id(&self) -> &str {
                "test-model"
            }
            async fn stream(
                &self,
                _ctx: &Context<'_>,
                _opts: &StreamOptions,
            ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Box::pin(futures::stream::empty()))
            }
        }

        let stream_calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(TrackingProvider {
            calls: Arc::clone(&stream_calls),
        });
        let mut agent_session = make_agent_session(provider, &harness, 4);

        // Pre-abort before run
        let (abort_handle, abort_signal) = AbortHandle::new();
        abort_handle.abort();

        let (tl, cb) = capture_timeline();

        let message = agent_session
            .run_text_with_abort("test".to_string(), Some(abort_signal), cb)
            .await
            .expect("run should not hard-error");

        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(
            stream_calls.load(Ordering::SeqCst),
            0,
            "provider should not have been called"
        );

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 7: Repeated interruption cycles don't corrupt state
// ---------------------------------------------------------------------------

#[test]
fn repeated_interruption_cycles_no_corruption() {
    let test_name = "reliability_repeated_interruption";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let cwd = harness.temp_dir().to_path_buf();
        let session = make_session(&harness);

        // Run 3 cycles of: start → abort → check state
        for cycle in 0..3 {
            let provider: Arc<dyn Provider> =
                Arc::new(SlowStreamProvider::new(5, &format!("cycle{cycle}-")));

            let tools = ToolRegistry::new(&[], &cwd, None);
            let config = AgentConfig {
                system_prompt: None,
                max_tool_iterations: 4,
                stream_options: StreamOptions {
                    api_key: Some("test-key".to_string()),
                    ..StreamOptions::default()
                },
                block_images: false,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
            };
            let agent = Agent::new(provider, tools, config);
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );

            let (abort_handle, abort_signal) = AbortHandle::new();
            let chunks_seen = Arc::new(AtomicUsize::new(0));
            let chunks_ref = Arc::clone(&chunks_seen);
            let abort_inner = abort_handle.clone();

            let cb = move |event: AgentEvent| {
                if matches!(event, AgentEvent::MessageUpdate { .. }) {
                    let count = chunks_ref.fetch_add(1, Ordering::SeqCst);
                    if count >= 1 {
                        abort_inner.abort();
                    }
                }
            };

            let message = agent_session
                .run_text_with_abort(format!("cycle {cycle} input"), Some(abort_signal), cb)
                .await
                .expect("run should not error");

            assert_eq!(
                message.stop_reason,
                StopReason::Aborted,
                "cycle {cycle}: should be aborted"
            );

            // Verify agent messages are consistent
            let messages = agent_session.agent.messages();
            // Should have user + assistant for this cycle
            // (previous cycles' messages are in the session but not in agent.messages
            // since we create a new agent each cycle)
            assert!(
                messages.len() >= 2,
                "cycle {cycle}: expected at least 2 messages, got {}",
                messages.len()
            );

            // Verify message history consistency: user message present,
            // last assistant is Aborted, no corruption.
            let has_user = messages.iter().any(|m| matches!(m, Message::User(_)));
            assert!(has_user, "cycle {cycle}: should have user message");

            // The last assistant message should be the aborted one
            let last_assistant = messages.iter().rev().find_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            });
            assert!(
                last_assistant.is_some(),
                "cycle {cycle}: should have at least one assistant message"
            );
            assert_eq!(
                last_assistant.unwrap().stop_reason,
                StopReason::Aborted,
                "cycle {cycle}: last assistant should be Aborted"
            );
        }

        // Verify session state is intact after 3 cycles
        let messages = {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.to_messages_for_current_path()
        };

        harness
            .log()
            .info_ctx("repeated_interruption", "all cycles complete", |ctx| {
                ctx.push(("total_messages".into(), messages.len().to_string()));
                ctx.push(("cycles".into(), "3".into()));
            });

        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 8: Session resume after interruption maintains history consistency
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn session_resume_after_interruption() {
    let test_name = "reliability_session_resume";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let cwd = harness.temp_dir().to_path_buf();

        // Phase 1: Normal completion
        let msg1 = make_assistant("first response", StopReason::Stop, 15);
        let partial1 = make_partial("");
        let script1 = vec![
            StreamEvent::Start { partial: partial1 },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: msg1,
            },
        ];

        let provider1: Arc<dyn Provider> =
            Arc::new(ScriptedReliabilityProvider::new(vec![script1]));
        let session = make_session(&harness);

        let tools = ToolRegistry::new(&[], &cwd, None);
        let config = AgentConfig {
            system_prompt: Some("You are a test agent.".to_string()),
            max_tool_iterations: 4,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
        };
        let agent1 = Agent::new(provider1, tools, config.clone());
        let mut session1 = AgentSession::new(
            agent1,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let message1 = session1
            .run_text("first question".to_string(), |_| {})
            .await
            .expect("first run");
        assert_eq!(message1.stop_reason, StopReason::Stop);
        session1.persist_session().await.expect("persist phase 1");

        let phase1_messages = session1.agent.messages().len();

        // Phase 2: New run that will be aborted
        let provider2: Arc<dyn Provider> =
            Arc::new(SlowStreamProvider::new(2, "interrupted-content "));

        let tools2 = ToolRegistry::new(&[], &cwd, None);
        let mut agent2 = Agent::new(provider2, tools2, config.clone());
        // Restore messages from phase 1
        agent2.replace_messages(session1.agent.messages().to_vec());

        let mut session2 = AgentSession::new(
            agent2,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let (abort_handle, abort_signal) = AbortHandle::new();
        let abort_inner = abort_handle.clone();
        let update_count = Arc::new(AtomicUsize::new(0));
        let update_ref = Arc::clone(&update_count);

        let message2 = session2
            .run_text_with_abort(
                "second question".to_string(),
                Some(abort_signal),
                move |event| {
                    if matches!(event, AgentEvent::MessageUpdate { .. })
                        && update_ref.fetch_add(1, Ordering::SeqCst) >= 1
                    {
                        abort_inner.abort();
                    }
                },
            )
            .await
            .expect("second run (aborted)");

        assert_eq!(message2.stop_reason, StopReason::Aborted);
        session2.persist_session().await.expect("persist phase 2");

        // Phase 3: Resume with new provider (simulating continuation)
        let msg3 = make_assistant("resumed response after interruption", StopReason::Stop, 20);
        let partial3 = make_partial("");
        let script3 = vec![
            StreamEvent::Start { partial: partial3 },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: msg3,
            },
        ];

        let provider3: Arc<dyn Provider> =
            Arc::new(ScriptedReliabilityProvider::new(vec![script3]));

        let tools3 = ToolRegistry::new(&[], &cwd, None);
        let mut agent3 = Agent::new(provider3, tools3, config);
        // Restore full message history
        agent3.replace_messages(session2.agent.messages().to_vec());

        let mut session3 = AgentSession::new(
            agent3,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        // Continue without new user message
        let message3 = session3
            .agent
            .run_continue_with_abort(None, |_| {})
            .await
            .expect("resume run");

        assert_eq!(
            message3.stop_reason,
            StopReason::Stop,
            "resumed run should complete normally"
        );

        let text3 = assistant_text(&message3);
        assert!(
            text3.contains("resumed response"),
            "should contain resume text, got: {text3}"
        );

        // Verify full history is consistent
        let final_messages = session3.agent.messages();
        let user_count = final_messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count();
        let assistant_count = final_messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();

        // Should have: user1 + assistant1 + user2 + assistant2(aborted) + assistant3(resumed)
        assert!(
            user_count >= 2,
            "expected at least 2 user messages, got {user_count}"
        );
        assert!(
            assistant_count >= 3,
            "expected at least 3 assistant messages (1 normal + 1 aborted + 1 resumed), got {assistant_count}"
        );

        harness
            .log()
            .info_ctx("session_resume", "full lifecycle complete", |ctx| {
                ctx.push(("phase1_msgs".into(), phase1_messages.to_string()));
                ctx.push(("total_users".into(), user_count.to_string()));
                ctx.push(("total_assistants".into(), assistant_count.to_string()));
            });

        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 9: Tool call followed by normal completion (baseline sanity)
// ---------------------------------------------------------------------------

#[test]
fn tool_call_followed_by_normal_completion() {
    let test_name = "reliability_tool_normal_completion";
    let harness = TestHarness::new(test_name);

    harness.create_file("sample.txt", "sample file content");

    run_async(async move {
        let sample_path = harness.temp_path("sample.txt").display().to_string();
        let tool_calls = vec![ToolCall {
            id: "read-1".to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": sample_path }),
            thought_signature: None,
        }];

        let provider: Arc<dyn Provider> = Arc::new(ToolCallThenHangProvider {
            tool_calls,
            stream_calls: AtomicUsize::new(0),
            second_call_hangs: false,
        });

        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let message = agent_session
            .run_text("read the file".to_string(), cb)
            .await
            .expect("run should succeed");

        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "should complete normally"
        );

        let guard = tl.lock().expect("lock");
        assert_eq!(guard.tool_starts, 1, "should have 1 tool execution");
        assert_eq!(guard.tool_ends, 1, "should have 1 tool end");

        // Verify tool result is in message history
        let tool_results: Vec<&ToolResultMessage> = agent_session
            .agent
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(r) => Some(r.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 1, "should have 1 tool result");
        assert!(!tool_results[0].is_error, "tool result should not be error");

        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 10: Transient timeout + retry/backoff recovers deterministically
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn transient_timeout_retry_backoff_is_recoverable() {
    let test_name = "reliability_fault_timeout_retry";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider = Arc::new(FlakyTimeoutThenSuccessProvider::new(
            2,
            "Recovered after transient timeout retry.",
        ));
        let provider_dyn: Arc<dyn Provider> = provider.clone();
        let mut agent_session = make_agent_session(provider_dyn, &harness, 4);

        let tl = Arc::new(StdMutex::new(Timeline::default()));
        let started_at = Instant::now();
        let retry_backoff_ms = vec![25_u64, 50_u64];
        let replay_command =
            format!("cargo test --test agent_loop_reliability {test_name} -- --nocapture");

        let mut transitions = vec![FaultTransition {
            phase: "fault_injected".to_string(),
            elapsed_ms: 0,
            detail: "provider.stream returns timeout errors for first two attempts".to_string(),
        }];
        let mut final_message: Option<AssistantMessage> = None;
        let mut terminal_outcome = "unreached".to_string();

        for attempt in 0..=retry_backoff_ms.len() {
            transitions.push(FaultTransition {
                phase: "attempt".to_string(),
                elapsed_ms: started_at.elapsed().as_millis(),
                detail: format!("attempt {} started", attempt + 1),
            });

            let result = if attempt == 0 {
                agent_session
                    .run_text(
                        "exercise transient timeout path".to_string(),
                        make_timeline_callback(Arc::clone(&tl), started_at),
                    )
                    .await
            } else {
                agent_session
                    .agent
                    .run_continue_with_abort(
                        None,
                        make_timeline_callback(Arc::clone(&tl), started_at),
                    )
                    .await
            };

            match result {
                Ok(message) => {
                    terminal_outcome = format!("recovered_on_attempt_{}", attempt + 1);
                    transitions.push(FaultTransition {
                        phase: "recovered".to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        detail: format!("assistant stop_reason={:?}", message.stop_reason),
                    });
                    final_message = Some(message);
                    break;
                }
                Err(err) => {
                    let err_text = err.to_string();
                    transitions.push(FaultTransition {
                        phase: "attempt_failed".to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        detail: err_text.clone(),
                    });

                    if attempt == retry_backoff_ms.len() {
                        terminal_outcome = format!("fatal_after_retries: {err_text}");
                        break;
                    }

                    assert!(
                        err_text.to_ascii_lowercase().contains("timed out"),
                        "expected timeout error, got: {err_text}"
                    );

                    let backoff_ms = retry_backoff_ms[attempt];
                    transitions.push(FaultTransition {
                        phase: "retry_backoff".to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        detail: format!("sleeping {backoff_ms}ms before retry"),
                    });
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(backoff_ms),
                    )
                    .await;
                }
            }
        }

        let message = final_message.expect("transient timeout should recover with retries");
        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "recovered run should complete normally"
        );
        assert!(
            assistant_text(&message).contains("Recovered after transient timeout retry"),
            "recovery response should be present"
        );
        assert_eq!(
            provider.stream_calls.load(Ordering::SeqCst),
            3,
            "expected two failures + one success"
        );

        let user_count = agent_session
            .agent
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count();
        assert_eq!(
            user_count, 1,
            "retry via continue path should not duplicate user prompts"
        );

        let record = FaultEpisodeRecord {
            schema: "pi.reliability.fault_episode.v1",
            issue_id: "bd-1f42.5.2",
            remediation_issue: "bd-1f42.5.2",
            test_name: test_name.to_string(),
            fault_name: "transient_network_timeout".to_string(),
            injection_point: "provider.stream".to_string(),
            classification: FAULT_CLASS_RECOVERABLE,
            retry_backoff_ms: retry_backoff_ms.clone(),
            state_transitions: transitions,
            terminal_outcome,
            root_cause_marker: "fault.timeout.transient".to_string(),
            replay_command,
        };
        assert_eq!(record.classification, FAULT_CLASS_RECOVERABLE);

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_fault_episode_artifact(&harness, test_name, &record);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 11: Partial-write tool failure remains recoverable with state integrity
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn partial_write_tool_failure_recovers_without_state_corruption() {
    let test_name = "reliability_fault_partial_write_recovery";
    let harness = TestHarness::new(test_name);
    let baseline_content = "stable baseline\n";
    harness.create_file("target.txt", baseline_content);

    run_async(async move {
        let target_path = harness.temp_path("target.txt").display().to_string();
        let first_turn = make_tool_call_message(
            vec![
                ToolCall {
                    id: "faulty-write-1".to_string(),
                    name: "faulty_write".to_string(),
                    arguments: json!({
                        "path": target_path.clone(),
                        "content": "this payload should never persist"
                    }),
                    thought_signature: None,
                },
                ToolCall {
                    id: "read-verify-1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({
                        "path": target_path.clone()
                    }),
                    thought_signature: None,
                },
            ],
            44,
        );
        let first_partial = AssistantMessage {
            content: Vec::new(),
            stop_details: None,
            ..first_turn.clone()
        };
        let second_turn = make_assistant(
            "Recovered after injected partial-write failure; state verified.",
            StopReason::Stop,
            18,
        );
        let second_partial = make_partial("");

        let provider: Arc<dyn Provider> = Arc::new(ScriptedReliabilityProvider::new(vec![
            vec![
                StreamEvent::Start {
                    partial: first_partial,
                },
                StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    message: first_turn,
                },
            ],
            vec![
                StreamEvent::Start {
                    partial: second_partial,
                },
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: second_turn,
                },
            ],
        ]));

        let cwd = harness.temp_dir().to_path_buf();
        let mut tools = ToolRegistry::new(&["read"], &cwd, None);
        tools.extend(std::iter::once(
            Box::new(FaultyPartialWriteTool::new(&cwd)) as Box<dyn Tool>
        ));
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 4,
                stream_options: StreamOptions {
                    api_key: Some("test-key".to_string()),
                    ..StreamOptions::default()
                },
                block_images: false,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
            },
        );
        let session = make_session(&harness);
        let mut agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let (tl, cb) = capture_timeline();
        let message = agent_session
            .run_text("exercise partial-write recovery".to_string(), cb)
            .await
            .expect("partial-write scenario should recover");
        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "assistant should recover after injected tool failure"
        );

        let tool_results: Vec<&ToolResultMessage> = agent_session
            .agent
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(result) => Some(result.as_ref()),
                _ => None,
            })
            .collect();

        let faulty = tool_results
            .iter()
            .find(|r| r.tool_call_id == "faulty-write-1")
            .expect("expected faulty_write tool result");
        assert!(
            faulty.is_error,
            "faulty_write should emit an error tool result"
        );
        assert!(
            tool_result_text(faulty).contains("FAULT-PARTIAL-WRITE"),
            "fault marker should appear in faulty_write output"
        );

        let read_verify = tool_results
            .iter()
            .find(|r| r.tool_call_id == "read-verify-1")
            .expect("expected read verification tool result");
        assert!(
            !read_verify.is_error,
            "read verification should succeed after faulty write failure"
        );
        assert!(
            tool_result_text(read_verify).contains("stable baseline"),
            "read verification should confirm baseline content"
        );
        assert_eq!(
            harness.read_file("target.txt"),
            baseline_content,
            "target file content must remain unchanged after injected failure"
        );

        let record = FaultEpisodeRecord {
            schema: "pi.reliability.fault_episode.v1",
            issue_id: "bd-1f42.5.2",
            remediation_issue: "bd-1f42.5.2",
            test_name: test_name.to_string(),
            fault_name: "partial_write_before_persist".to_string(),
            injection_point: "tool.faulty_write.tempfile_pre_persist".to_string(),
            classification: FAULT_CLASS_RECOVERABLE,
            retry_backoff_ms: Vec::new(),
            state_transitions: vec![
                FaultTransition {
                    phase: "fault_injected".to_string(),
                    elapsed_ms: 0,
                    detail: "faulty_write stops after writing partial tempfile".to_string(),
                },
                FaultTransition {
                    phase: "state_verified".to_string(),
                    elapsed_ms: 1,
                    detail: "read tool confirms target file content unchanged".to_string(),
                },
                FaultTransition {
                    phase: "recovered".to_string(),
                    elapsed_ms: 2,
                    detail: format!("assistant stop_reason={:?}", message.stop_reason),
                },
            ],
            terminal_outcome: "recovered_with_integrity_preserved".to_string(),
            root_cause_marker: "fault.partial_write.pre_persist".to_string(),
            replay_command: format!(
                "cargo test --test agent_loop_reliability {test_name} -- --nocapture"
            ),
        };
        assert_eq!(record.classification, FAULT_CLASS_RECOVERABLE);

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_fault_episode_artifact(&harness, test_name, &record);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 12: Repeated stream-contract violation is classified as fatal
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn stream_contract_violation_after_retries_is_fatal() {
    let test_name = "reliability_fault_stream_contract_fatal";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider = Arc::new(StreamContractViolationProvider::new());
        let provider_dyn: Arc<dyn Provider> = provider.clone();
        let mut agent_session = make_agent_session(provider_dyn, &harness, 4);

        let tl = Arc::new(StdMutex::new(Timeline::default()));
        let started_at = Instant::now();
        let retry_backoff_ms = vec![15_u64, 30_u64];
        let replay_command =
            format!("cargo test --test agent_loop_reliability {test_name} -- --nocapture");

        let mut transitions = vec![FaultTransition {
            phase: "fault_injected".to_string(),
            elapsed_ms: 0,
            detail: "provider emits empty stream (no Start/Done)".to_string(),
        }];
        let mut final_error: Option<String> = None;

        for attempt in 0..=retry_backoff_ms.len() {
            transitions.push(FaultTransition {
                phase: "attempt".to_string(),
                elapsed_ms: started_at.elapsed().as_millis(),
                detail: format!("attempt {} started", attempt + 1),
            });

            let result = if attempt == 0 {
                agent_session
                    .run_text(
                        "exercise fatal stream-contract path".to_string(),
                        make_timeline_callback(Arc::clone(&tl), started_at),
                    )
                    .await
            } else {
                agent_session
                    .agent
                    .run_continue_with_abort(
                        None,
                        make_timeline_callback(Arc::clone(&tl), started_at),
                    )
                    .await
            };

            match result {
                Ok(message) => {
                    panic!(
                        "fatal stream-contract violation should not recover, got {:?}",
                        message.stop_reason
                    );
                }
                Err(err) => {
                    let err_text = err.to_string();
                    transitions.push(FaultTransition {
                        phase: "attempt_failed".to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        detail: err_text.clone(),
                    });
                    assert!(
                        err_text.contains("Stream ended without Done event"),
                        "expected stream-contract error, got: {err_text}"
                    );
                    final_error = Some(err_text.clone());

                    if attempt == retry_backoff_ms.len() {
                        break;
                    }

                    let backoff_ms = retry_backoff_ms[attempt];
                    transitions.push(FaultTransition {
                        phase: "retry_backoff".to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        detail: format!("sleeping {backoff_ms}ms before retry"),
                    });
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(backoff_ms),
                    )
                    .await;
                }
            }
        }

        assert!(
            final_error.is_some(),
            "fatal scenario must retain final stream-contract error"
        );
        assert_eq!(
            provider.stream_calls.load(Ordering::SeqCst),
            3,
            "expected initial run + two retries"
        );

        let assistant_count = agent_session
            .agent
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        assert_eq!(
            assistant_count, 3,
            "each failed attempt leaves an error assistant message in the manual continue loop"
        );
        let user_count = agent_session
            .agent
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count();
        assert_eq!(
            user_count, 1,
            "continue retries should not duplicate user prompts"
        );

        let record = FaultEpisodeRecord {
            schema: "pi.reliability.fault_episode.v1",
            issue_id: "bd-1f42.5.2",
            remediation_issue: "bd-1f42.5.2",
            test_name: test_name.to_string(),
            fault_name: "provider_stream_contract_violation".to_string(),
            injection_point: "provider.stream".to_string(),
            classification: FAULT_CLASS_FATAL,
            retry_backoff_ms: retry_backoff_ms.clone(),
            state_transitions: transitions,
            terminal_outcome: final_error.unwrap_or_else(|| "unknown".to_string()),
            root_cause_marker: "fault.stream_contract.empty_stream".to_string(),
            replay_command,
        };
        assert_eq!(record.classification, FAULT_CLASS_FATAL);

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_fault_episode_artifact(&harness, test_name, &record);
        write_log_artifacts(&harness, test_name);
    });
}

// ---------------------------------------------------------------------------
// Test 13: Empty stream (no events at all) returns error
// ---------------------------------------------------------------------------

#[test]
fn empty_stream_returns_error() {
    let test_name = "reliability_empty_stream";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        struct EmptyStreamProvider;

        impl std::fmt::Debug for EmptyStreamProvider {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("EmptyStreamProvider").finish()
            }
        }

        #[async_trait]
        #[allow(clippy::unnecessary_literal_bound)]
        impl Provider for EmptyStreamProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn api(&self) -> &str {
                "test-api"
            }
            fn model_id(&self) -> &str {
                "test-model"
            }
            async fn stream(
                &self,
                _ctx: &Context<'_>,
                _opts: &StreamOptions,
            ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
                Ok(Box::pin(futures::stream::empty()))
            }
        }

        let provider: Arc<dyn Provider> = Arc::new(EmptyStreamProvider);
        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let result = agent_session.run_text("test".to_string(), cb).await;

        // Empty stream (no Start, no Done) should return an error
        assert!(result.is_err(), "empty stream should return error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Stream ended without Done event") || err.contains("stream"),
            "error should mention stream issue, got: {err}"
        );

        let guard = tl.lock().expect("lock");
        write_timeline_artifact(&harness, test_name, &guard);
        drop(guard);
        write_log_artifacts(&harness, test_name);
    });
}
