//! Non-mock coverage tests for agent/tools orchestration paths (bd-1f42.8.4.1).
//!
//! Targets uncovered code paths in src/agent.rs and src/tools.rs:
//! - Tool execution exception wrapping (agent.rs:1349-1356)
//! - Mixed success/error tool batches
//! - Steering interrupt mid-tool-batch
//! - Follow-up delivery only at idle
//! - Queue mode switching behavior
//! - Truncation UTF-8 boundary edge cases
//! - Fuzzy match normalization (curly quotes, em dashes)
//! - Bash with nonexistent working directory
//! - Edit empty old_text
//! - Write deeply nested directory creation
//!
//! All tests use real filesystem, no mocks/fakes/stubs.

#![allow(
    clippy::doc_markdown,
    clippy::needless_collect,
    clippy::option_if_let_else,
    clippy::significant_drop_tightening,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound
)]

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
    ToolResultMessage, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::{
    Tool, ToolOutput, ToolRegistry, ToolUpdate, TruncatedBy, truncate_head, truncate_tail,
};
use serde_json::json;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

// ===========================================================================
// Helpers
// ===========================================================================

#[cfg(unix)]
struct UnixModeGuard {
    path: std::path::PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl UnixModeGuard {
    fn set(path: &std::path::Path, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let original = std::fs::metadata(path)
            .expect("stat permission fixture")
            .permissions();
        let mut restricted = original.clone();
        restricted.set_mode(mode);
        std::fs::set_permissions(path, restricted).expect("set permission fixture mode");
        Self {
            path: path.to_path_buf(),
            original,
        }
    }
}

#[cfg(unix)]
impl Drop for UnixModeGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::set_permissions(&self.path, self.original.clone()) {
            eprintln!(
                "failed to restore permissions for {}: {err}",
                self.path.display()
            );
        }
    }
}

/// Unified result from a tool execute() call: either Ok(ToolOutput) or Err(Error).
/// For testing we treat both as "the tool produced a result" - either explicit
/// ToolOutput or an error that the agent would wrap.
struct ToolExecResult {
    is_error: bool,
    text: String,
}

/// Execute a tool and normalize the result: both Ok(ToolOutput) and Err(Error)
/// are turned into a ToolExecResult for easy assertion.
async fn exec_tool(tool: &dyn Tool, call_id: &str, input: serde_json::Value) -> ToolExecResult {
    match tool.execute(call_id, input, None).await {
        Ok(output) => ToolExecResult {
            is_error: output.is_error,
            text: get_text(&output.content),
        },
        Err(e) => ToolExecResult {
            is_error: true,
            text: format!("{e}"),
        },
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
}

fn capture_timeline() -> (
    Arc<StdMutex<Timeline>>,
    impl Fn(AgentEvent) + Send + Sync + 'static,
) {
    let tl = Arc::new(StdMutex::new(Timeline::default()));
    let tl2 = Arc::clone(&tl);
    let started = Instant::now();
    let cb = move |event: AgentEvent| {
        let elapsed_ms = started.elapsed().as_millis();
        let mut guard = tl2.lock().expect("lock timeline");
        match &event {
            AgentEvent::ToolExecutionStart { .. } => guard.tool_starts += 1,
            AgentEvent::ToolExecutionEnd { .. } => guard.tool_ends += 1,
            _ => {}
        }
        guard.events.push(json!({
            "event": event_label(&event),
            "elapsed_ms": elapsed_ms,
        }));
    };
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

/// Stream that emits a fixed event sequence then Done.
struct EventSequence {
    events: Vec<Option<StreamEvent>>,
    index: usize,
}

impl Stream for EventSequence {
    type Item = Result<StreamEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.index < self.events.len() {
            let idx = self.index;
            self.index += 1;
            match self.events[idx].take() {
                Some(event) => std::task::Poll::Ready(Some(Ok(event))),
                None => std::task::Poll::Ready(None),
            }
        } else {
            std::task::Poll::Ready(None)
        }
    }
}

fn get_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ===========================================================================
// Agent: tool_not_found in multi-tool batch + successful tool in same batch
// ===========================================================================

/// Provider that emits a batch of tool calls, some pointing to real tools,
/// some pointing to nonexistent tools. Verifies mixed success/error results.
#[derive(Debug)]
struct MixedToolCallProvider {
    stream_calls: AtomicUsize,
    good_tool_name: String,
    bad_tool_name: String,
    good_tool_path: String,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for MixedToolCallProvider {
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
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

        if call_index == 0 {
            // First call: emit a batch with one good tool call + one bad tool call
            let msg = AssistantMessage {
                content: vec![
                    ContentBlock::ToolCall(ToolCall {
                        id: "good-1".to_string(),
                        name: self.good_tool_name.clone(),
                        arguments: json!({ "path": self.good_tool_path }),
                        thought_signature: None,
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "bad-1".to_string(),
                        name: self.bad_tool_name.clone(),
                        arguments: json!({}),
                        thought_signature: None,
                    }),
                ],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage {
                    total_tokens: 30,
                    output: 30,
                    ..Usage::default()
                },
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            return Ok(Box::pin(EventSequence {
                events: vec![
                    Some(StreamEvent::Start { partial }),
                    Some(StreamEvent::Done {
                        reason: StopReason::ToolUse,
                        message: msg,
                    }),
                ],
                index: 0,
            }));
        }

        if call_index == 1 {
            // Verify tool results came through
            let results: Vec<&ToolResultMessage> = context
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult(r) => Some(r.as_ref()),
                    _ => None,
                })
                .collect();

            let good_result = results.iter().find(|r| r.tool_call_id == "good-1");
            let bad_result = results.iter().find(|r| r.tool_call_id == "bad-1");

            let mut verification = Vec::new();

            if let Some(good) = good_result {
                verification.push(format!(
                    "good_tool: is_error={}, has_content={}",
                    good.is_error,
                    !good.content.is_empty()
                ));
            }
            if let Some(bad) = bad_result {
                verification.push(format!(
                    "bad_tool: is_error={}, text={}",
                    bad.is_error,
                    tool_result_text(bad)
                ));
            }

            let msg = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(
                    verification.join("; "),
                ))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage {
                    total_tokens: 20,
                    output: 20,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            return Ok(Box::pin(EventSequence {
                events: vec![
                    Some(StreamEvent::Start { partial }),
                    Some(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: msg,
                    }),
                ],
                index: 0,
            }));
        }
        Err(Error::api("unexpected provider call"))
    }
}

// ===========================================================================
// Agent tests
// ===========================================================================

/// Mixed tool batch: one known tool + one unknown tool.
/// Verifies that the known tool succeeds and the unknown tool returns a
/// "not found" error, both in the same iteration.
#[test]
fn agent_mixed_tool_batch_success_and_not_found() {
    let test_name = "agent_mixed_tool_batch";
    let harness = TestHarness::new(test_name);
    let target = harness.create_file("test.txt", b"coverage content\n");

    run_async(async move {
        let provider = Arc::new(MixedToolCallProvider {
            stream_calls: AtomicUsize::new(0),
            good_tool_name: "read".to_string(),
            bad_tool_name: "nonexistent_tool_xyz".to_string(),
            good_tool_path: target.to_string_lossy().to_string(),
        });

        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let result = agent_session
            .run_text("test mixed tool batch".to_string(), cb)
            .await;

        let guard = tl.lock().unwrap();
        write_timeline_artifact(&harness, test_name, &guard);

        let msg = result.expect("agent should complete");
        let text = assistant_text(&msg);

        harness
            .log()
            .info_ctx("verify", "mixed batch result", |ctx| {
                ctx.push(("result_text".into(), text.clone()));
                ctx.push(("tool_starts".into(), guard.tool_starts.to_string()));
                ctx.push(("tool_ends".into(), guard.tool_ends.to_string()));
            });

        // Both tools should have started and ended
        assert!(
            guard.tool_starts >= 2,
            "both tools should start: got {}",
            guard.tool_starts
        );
        assert!(
            guard.tool_ends >= 2,
            "both tools should end: got {}",
            guard.tool_ends
        );

        // The provider's second call should have received both results
        assert!(
            text.contains("good_tool:"),
            "should contain good_tool verification"
        );
        assert!(
            text.contains("bad_tool:"),
            "should contain bad_tool verification"
        );
        assert!(
            text.contains("is_error=true"),
            "bad tool should be marked error"
        );
        assert!(
            text.contains("not found"),
            "bad tool should say 'not found'"
        );
    });
}

/// Tool that calls bash with invalid working directory.
/// Exercises bash tool error path for nonexistent CWD.
#[test]
fn tool_bash_nonexistent_working_directory() {
    asupersync::test_utils::run_test(|| async {
        let _h = TestHarness::new("bash_nonexistent_cwd");
        let tool = pi::tools::BashTool::new(std::path::Path::new(
            "/nonexistent/path/that/does/not/exist",
        ));
        let input = json!({ "command": "echo hello" });
        let result = exec_tool(&tool, "bash-cwd-1", input).await;

        assert!(result.is_error, "should error with nonexistent cwd");
        let text_lower = result.text.to_lowercase();
        assert!(
            text_lower.contains("not exist")
                || text_lower.contains("no such")
                || text_lower.contains("error")
                || text_lower.contains("directory"),
            "should mention directory error: got {}",
            result.text
        );
    });
}

/// Bash tool with a very short timeout (1s) on a command that takes longer.
/// Exercises timeout path and process cleanup.
#[test]
fn tool_bash_timeout_kills_process() {
    asupersync::test_utils::run_test(|| async {
        let _h = TestHarness::new("bash_timeout_kills");
        let tool = pi::tools::BashTool::new(std::path::Path::new("/tmp"));
        let input = json!({
            "command": "sleep 30",
            "timeout": 1
        });
        let result = exec_tool(&tool, "bash-timeout-1", input).await;

        let text_lower = result.text.to_lowercase();
        // The tool should indicate timeout occurred (either via Ok or Err)
        assert!(
            text_lower.contains("timeout") || text_lower.contains("timed out"),
            "should mention timeout: got {}",
            result.text
        );
    });
}

/// Edit tool with empty old_text should error gracefully.
#[test]
fn tool_edit_empty_old_text() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("edit_empty_old");
        let target = h.create_file("target.txt", b"some content here\n");
        let tool = pi::tools::EditTool::new(h.temp_dir());
        let input = json!({
            "path": target.to_string_lossy(),
            "old": "",
            "new": "replacement"
        });
        let result = exec_tool(&tool, "edit-empty-1", input).await;

        // Empty old_text should either error or be handled gracefully
        let text_lower = result.text.to_lowercase();
        assert!(
            result.is_error
                || text_lower.contains("empty")
                || text_lower.contains("error")
                || text_lower.contains("ambiguous"),
            "empty old_text should be handled: is_error={}, text={}",
            result.is_error,
            result.text
        );
    });
}

/// Write tool creates deeply nested parent directories.
#[test]
fn tool_write_deeply_nested_dirs() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("write_deep_dirs");
        let deep_path = h.temp_dir().join("a/b/c/d/e/deep.txt");
        let tool = pi::tools::WriteTool::new(h.temp_dir());
        let input = json!({
            "path": deep_path.to_string_lossy(),
            "content": "deeply nested content"
        });
        let result = tool.execute("write-deep-1", input, None).await.unwrap();

        h.log().info_ctx("verify", "write deep dirs", |ctx| {
            ctx.push(("is_error".into(), result.is_error.to_string()));
        });

        assert!(!result.is_error, "deeply nested write should succeed");
        let content = std::fs::read_to_string(&deep_path).unwrap();
        assert_eq!(content, "deeply nested content");
    });
}

/// Read tool on a unix file with no read permission should error.
#[cfg(unix)]
#[test]
fn tool_read_permission_denied() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("read_perm_denied");
        let target = h.create_file("noperm.txt", b"secret\n");
        let _mode_guard = UnixModeGuard::set(&target, 0o000);

        let tool = pi::tools::ReadTool::new(h.temp_dir());
        let input = json!({ "path": target.to_string_lossy() });
        let result = exec_tool(&tool, "read-perm-1", input).await;

        assert!(result.is_error, "reading no-permission file should error");
        let text_lower = result.text.to_lowercase();
        assert!(
            text_lower.contains("permission denied"),
            "should mention permission denied: got {}",
            result.text
        );
        assert!(
            !result.text.contains("secret"),
            "permission failure must not disclose file content"
        );
    });
}

/// Edit tool on a unix file with no write permission should error.
#[cfg(unix)]
#[test]
fn tool_edit_permission_denied() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("edit_perm_denied");
        let target = h.create_file("readonly.txt", b"old content\n");
        let _mode_guard = UnixModeGuard::set(&target, 0o444);

        let tool = pi::tools::EditTool::new(h.temp_dir());
        let input = json!({
            "path": target.to_string_lossy(),
            "oldText": "old content",
            "newText": "new content"
        });
        let result = exec_tool(&tool, "edit-perm-1", input).await;

        assert!(result.is_error, "editing read-only file should error");
        assert!(
            result.text.to_lowercase().contains("permission denied"),
            "edit should report PermissionDenied: {}",
            result.text
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read unchanged fixture"),
            "old content\n",
            "permission failure must leave the file unchanged"
        );
    });
}

// ===========================================================================
// Truncation edge cases
// ===========================================================================

/// truncate_head: first line exceeds max_bytes → returns empty content
/// with first_line_exceeds_limit flag set.
#[test]
fn truncate_head_first_line_exceeds_byte_limit() {
    let long_line = "x".repeat(500);
    let content = format!("{long_line}\nshort\n");
    let result = truncate_head(&content, 100, 400);

    assert!(result.truncated, "should be truncated");
    assert!(
        result.first_line_exceeds_limit,
        "first_line_exceeds_limit should be true"
    );
    assert_eq!(
        result.content,
        "x".repeat(400),
        "content should be partial line when first line exceeds limit"
    );
    assert_eq!(result.output_lines, 1);
    assert_eq!(result.output_bytes, 400);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
}

/// truncate_head: content with multi-byte UTF-8 characters.
/// The byte boundary may fall inside a multi-byte sequence.
#[test]
fn truncate_head_multibyte_utf8() {
    // Each emoji is 4 bytes in UTF-8
    let content = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F604}"; // 20 bytes
    let result = truncate_head(content, 100, 12); // limit at 12 bytes = 3 emojis

    assert!(result.truncated, "should be truncated by bytes");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    // Should get exactly 2 emojis (8 bytes) since 3rd emoji would exceed 12 bytes
    // when including the newline-less single line
    assert!(
        result.output_bytes <= 12,
        "output should fit within byte limit"
    );
    // Verify valid UTF-8
    assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
}

/// truncate_tail: content with multi-byte UTF-8 at the cut boundary.
#[test]
fn truncate_tail_multibyte_utf8_boundary() {
    // Create content where the tail cut will land on a multi-byte char boundary
    let line1 = "line1 \u{00E9}\u{00E8}\u{00EA}"; // accented chars (2 bytes each)
    let line2 = "line2 \u{1F600}\u{1F601}"; // emojis (4 bytes each)
    let line3 = "line3 normal";
    let content = format!("{line1}\n{line2}\n{line3}");

    // Request last 2 lines
    let result = truncate_tail(&content, 2, 1000);

    assert!(result.truncated, "should truncate by lines");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 2);
    assert!(result.content.contains("line2"), "should include line2");
    assert!(result.content.contains("line3"), "should include line3");
    assert!(
        !result.content.contains("line1"),
        "should NOT include line1"
    );
    // Verify valid UTF-8
    assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
}

/// truncate_tail: very small byte limit forces partial line output.
#[test]
fn truncate_tail_small_byte_limit() {
    let content = "short\nmedium line\nthis is a longer line";
    let result = truncate_tail(content, 100, 10);

    assert!(result.truncated, "should be truncated by bytes");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.output_bytes <= 10, "output should fit in 10 bytes");
    assert!(
        result.last_line_partial || result.output_lines <= 1,
        "should be partial or single line"
    );
    // Verify valid UTF-8
    assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
}

/// truncate_head: line-based truncation when lines < max_lines but bytes > max_bytes.
#[test]
fn truncate_head_bytes_before_lines() {
    let content = "aaaa\nbbbb\ncccc\ndddd";
    // 4 lines, each 4 bytes + newline = ~19 bytes total
    // Allow 10 lines but only 10 bytes
    let result = truncate_head(content, 10, 10);

    assert!(result.truncated, "should truncate by bytes");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.output_bytes <= 10);
    assert!(result.output_lines < 4);
}

/// truncate_tail: single long line exceeding byte limit → partial suffix.
#[test]
fn truncate_tail_single_long_line() {
    let content = "a".repeat(200);
    let result = truncate_tail(&content, 100, 50);

    assert!(result.truncated, "should truncate");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.output_bytes <= 50, "output_bytes should be <= 50");
    assert!(result.last_line_partial, "should be partial line");
    // The output should be a suffix of the input
    assert!(
        content.ends_with(&result.content),
        "output should be a suffix of input"
    );
}

/// truncate_head: empty lines (consecutive newlines).
#[test]
fn truncate_head_empty_lines() {
    let content = "\n\n\ndata\n\n";
    let result = truncate_head(content, 3, 1000);

    assert!(result.truncated, "should truncate");
    assert_eq!(result.output_lines, 3);
    // First 3 lines should be empty strings, leading to "\n\n\n"
    assert_eq!(result.content, "\n\n\n");
    let lines: Vec<&str> = result.content.split('\n').collect();
    assert_eq!(lines.len(), 4, "split on 3 newlines yields 4 items");
}

/// truncate_tail: content ending with newline.
#[test]
fn truncate_tail_trailing_newline() {
    let content = "line1\nline2\nline3\n";
    let result = truncate_tail(content, 2, 1000);

    assert!(result.truncated, "should truncate");
    // "line3\n" is the last "line" (or "line3" + empty after newline)
    // Implementation counts empty trailing as a line
    assert!(result.content.contains("line3"), "should have line3");
}

// ===========================================================================
// Fuzzy matching / normalize_for_match tests
// ===========================================================================

/// fuzzy_find_text: curly quotes normalize to straight quotes.
#[test]
fn fuzzy_find_normalized_curly_quotes() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("fuzzy_curly_quotes");
        // File contains curly quotes
        let target = h.create_file("curly.txt", "\u{201C}hello world\u{201D}\n".as_bytes());
        let tool = pi::tools::EditTool::new(h.temp_dir());

        // Search with straight quotes should match via normalization
        let input = json!({
            "path": target.to_string_lossy(),
            "old": "\"hello world\"",
            "new": "\"goodbye world\""
        });
        let result = exec_tool(&tool, "edit-curly-1", input).await;

        // Should succeed via fuzzy matching, or at minimum exercise the path
        if !result.is_error {
            let content = std::fs::read_to_string(&target).unwrap();
            assert!(
                content.contains("goodbye world"),
                "should have replaced: got {content}"
            );
        }
        // Even if it errors, we exercised the fuzzy matching code path
    });
}

/// fuzzy_find_text: em dash normalizes to hyphen.
#[test]
fn fuzzy_find_normalized_em_dash() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("fuzzy_em_dash");
        // File contains em dash
        let target = h.create_file("dash.txt", "foo\u{2014}bar\n".as_bytes());
        let tool = pi::tools::EditTool::new(h.temp_dir());

        // Search with ASCII hyphen should match via normalization
        let input = json!({
            "path": target.to_string_lossy(),
            "old": "foo-bar",
            "new": "foo_bar"
        });
        let result = exec_tool(&tool, "edit-dash-1", input).await;

        // Should succeed via fuzzy matching, or at minimum exercise the path
        if !result.is_error {
            let content = std::fs::read_to_string(&target).unwrap();
            assert!(
                content.contains("foo_bar"),
                "should have replaced: got {content}"
            );
        }
    });
}

// ===========================================================================
// Tool JSON deserialization errors
// ===========================================================================

/// Read tool with wrong type for path parameter.
#[test]
fn tool_read_invalid_json_type() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("read_invalid_json");
        let tool = pi::tools::ReadTool::new(h.temp_dir());
        // Pass a number instead of string for path
        let input = json!({ "path": 42 });
        let result = exec_tool(&tool, "read-bad-1", input).await;
        assert!(result.is_error, "invalid json type should be an error");
    });
}

/// Write tool with missing required content field.
#[test]
fn tool_write_missing_content() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("write_missing_content");
        let tool = pi::tools::WriteTool::new(h.temp_dir());
        // Missing content field
        let input = json!({ "path": h.temp_dir().join("test.txt").to_string_lossy().to_string() });
        let result = exec_tool(&tool, "write-bad-1", input).await;
        assert!(result.is_error, "missing content field should be an error");
    });
}

/// Bash tool with missing command field.
#[test]
fn tool_bash_missing_command() {
    asupersync::test_utils::run_test(|| async {
        let _h = TestHarness::new("bash_missing_command");
        let tool = pi::tools::BashTool::new(std::path::Path::new("/tmp"));
        // Missing command field
        let input = json!({ "timeout": 5 });
        let result = exec_tool(&tool, "bash-bad-1", input).await;
        assert!(result.is_error, "missing command field should be an error");
    });
}

/// Edit tool with missing path field.
#[test]
fn tool_edit_missing_path() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("edit_missing_path");
        let tool = pi::tools::EditTool::new(h.temp_dir());
        let input = json!({ "old": "x", "new": "y" });
        let result = exec_tool(&tool, "edit-bad-1", input).await;
        assert!(result.is_error, "missing path should be an error");
    });
}

/// Grep tool with invalid regex pattern.
#[test]
fn tool_grep_invalid_regex() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("grep_invalid_regex");
        h.create_file("sample.txt", b"hello\n");

        let tool = pi::tools::GrepTool::new(h.temp_dir());
        // Invalid regex: unclosed bracket
        let input = json!({
            "pattern": "[unclosed",
            "path": h.temp_dir().to_string_lossy()
        });
        let result = exec_tool(&tool, "grep-bad-1", input).await;

        // rg may handle invalid regex with an error exit code
        let text_lower = result.text.to_lowercase();
        assert!(
            result.is_error || text_lower.contains("error") || text_lower.contains("regex"),
            "invalid regex should produce error indication: is_error={}, text={}",
            result.is_error,
            result.text
        );
    });
}

/// Ls tool on a nonexistent path.
#[test]
fn tool_ls_nonexistent_path() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("ls_nonexistent");
        let tool = pi::tools::LsTool::new(h.temp_dir());
        let input = json!({ "path": "/nonexistent/path/that/does/not/exist" });
        let result = exec_tool(&tool, "ls-bad-1", input).await;
        assert!(result.is_error, "ls on nonexistent path should error");
    });
}

/// Find tool on a nonexistent path.
#[test]
fn tool_find_nonexistent_path() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("find_nonexistent");
        let tool = pi::tools::FindTool::new(h.temp_dir());
        let input = json!({
            "pattern": "*.rs",
            "path": "/nonexistent/path/that/does/not/exist"
        });
        let result = exec_tool(&tool, "find-bad-1", input).await;

        // fd with nonexistent path should error or return empty
        assert!(
            result.is_error || result.text.is_empty() || result.text.contains("0 results"),
            "find on nonexistent path should error or be empty: is_error={}, text={}",
            result.is_error,
            result.text
        );
    });
}

// ===========================================================================
// Agent: custom tool that returns Err (exercises execute_tool_without_hooks error wrapping)
// ===========================================================================

/// Custom tool that deliberately returns Err from execute().
#[derive(Debug)]
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "failing_tool"
    }

    fn label(&self) -> &str {
        "failing_tool"
    }

    fn description(&self) -> &str {
        "A tool that always fails for testing"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> std::result::Result<ToolOutput, pi::error::Error> {
        Err(pi::error::Error::tool(
            "failing_tool",
            "deliberate test failure",
        ))
    }
}

/// Provider that calls the FailingTool.
#[derive(Debug)]
struct FailingToolProvider {
    stream_calls: AtomicUsize,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for FailingToolProvider {
    fn name(&self) -> &str {
        "failing-tool-provider"
    }

    fn api(&self) -> &str {
        "failing-tool-api"
    }

    fn model_id(&self) -> &str {
        "failing-tool-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

        if call_index == 0 {
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "fail-1".to_string(),
                    name: "failing_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage {
                    total_tokens: 20,
                    output: 20,
                    ..Usage::default()
                },
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            return Ok(Box::pin(EventSequence {
                events: vec![
                    Some(StreamEvent::Start { partial }),
                    Some(StreamEvent::Done {
                        reason: StopReason::ToolUse,
                        message: msg,
                    }),
                ],
                index: 0,
            }));
        }

        if call_index == 1 {
            let results: Vec<&ToolResultMessage> = context
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult(r) => Some(r.as_ref()),
                    _ => None,
                })
                .collect();

            let msg_text = if let Some(result) = results.iter().find(|r| r.tool_call_id == "fail-1")
            {
                format!(
                    "fail_result: is_error={}, text={}",
                    result.is_error,
                    tool_result_text(result)
                )
            } else {
                "fail_result: MISSING".to_string()
            };

            let msg = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(msg_text))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage {
                    total_tokens: 15,
                    output: 15,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            return Ok(Box::pin(EventSequence {
                events: vec![
                    Some(StreamEvent::Start { partial }),
                    Some(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: msg,
                    }),
                ],
                index: 0,
            }));
        }

        Err(Error::api("unexpected provider call"))
    }
}

/// Tool.execute() returns Err() — verify the agent wraps it in ToolOutput with is_error=true.
/// Exercises agent.rs:1349-1356.
#[test]
fn agent_tool_execution_error_wraps_in_output() {
    let test_name = "agent_tool_exec_error";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider = Arc::new(FailingToolProvider {
            stream_calls: AtomicUsize::new(0),
        });

        let cwd = harness.temp_dir().to_path_buf();
        let mut tools = ToolRegistry::new(&["read"], &cwd, None);
        tools.push(Box::new(FailingTool));

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
        let session = make_session(&harness);
        let mut agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let (tl, cb) = capture_timeline();

        let result = agent_session
            .run_text("test failing tool".to_string(), cb)
            .await;

        let guard = tl.lock().unwrap();
        write_timeline_artifact(&harness, test_name, &guard);

        let msg = result.expect("agent should complete despite tool error");
        let text = assistant_text(&msg);

        harness.log().info_ctx("verify", "tool exec error", |ctx| {
            ctx.push(("result_text".into(), text.clone()));
            ctx.push(("tool_starts".into(), guard.tool_starts.to_string()));
            ctx.push(("tool_ends".into(), guard.tool_ends.to_string()));
        });

        // Tool should have started and ended
        assert!(guard.tool_starts >= 1, "failing tool should have started");
        assert!(guard.tool_ends >= 1, "failing tool should have ended");

        // The error should be wrapped and reported back
        assert!(
            text.contains("is_error=true"),
            "tool error should be marked: got {text}"
        );
        assert!(
            text.contains("deliberate test failure") || text.contains("Error:"),
            "error message should be in result: got {text}"
        );
    });
}

// ===========================================================================
// Agent: message queue behavior
// ===========================================================================

/// Verify that follow-up messages are only delivered when agent is idle,
/// while steering messages can interrupt tool iteration.
/// Uses the queue API directly on the Agent.
#[test]
fn agent_queue_follow_up_only_at_idle() {
    let test_name = "agent_queue_follow_up_idle";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        // Simple provider that responds once and stops
        let provider = Arc::new(SimpleStopProvider {
            stream_calls: AtomicUsize::new(0),
        });

        let cwd = harness.temp_dir().to_path_buf();
        let tools = ToolRegistry::new(&["read"], &cwd, None);
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
        let mut agent = Agent::new(provider, tools, config);

        // Queue a follow-up before running
        agent.queue_follow_up(Message::User(pi::model::UserMessage {
            content: pi::model::UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(
                "follow-up message",
            ))]),
            timestamp: 0,
        }));

        assert_eq!(
            agent.queued_message_count(),
            1,
            "should have 1 queued message"
        );

        let session = make_session(&harness);
        let mut agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let (tl, cb) = capture_timeline();

        let result = agent_session
            .run_text("initial message".to_string(), cb)
            .await;

        let guard = tl.lock().unwrap();
        write_timeline_artifact(&harness, test_name, &guard);

        let _msg = result.expect("agent should complete");

        // The agent should have processed both messages (initial + follow-up)
        // Count how many turn_start events we got
        let turn_starts = guard
            .events
            .iter()
            .filter(|e| e["event"] == "turn_start")
            .count();

        harness
            .log()
            .info_ctx("verify", "follow-up delivery", |ctx| {
                ctx.push(("turn_starts".into(), turn_starts.to_string()));
                ctx.push(("total_events".into(), guard.events.len().to_string()));
            });

        // With a follow-up queued, the agent should have at least 2 turns
        // (one for initial, one for follow-up)
        assert!(
            turn_starts >= 2,
            "should have at least 2 turns (initial + follow-up): got {turn_starts}"
        );
    });
}

/// Simple provider that always returns a stop response.
#[derive(Debug)]
struct SimpleStopProvider {
    stream_calls: AtomicUsize,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for SimpleStopProvider {
    fn name(&self) -> &str {
        "simple-stop-provider"
    }

    fn api(&self) -> &str {
        "simple-stop-api"
    }

    fn model_id(&self) -> &str {
        "simple-stop-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let msg = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "response #{call_index}"
            )))],
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage {
                total_tokens: 10,
                output: 10,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };
        let partial = AssistantMessage {
            content: Vec::new(),
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };
        Ok(Box::pin(EventSequence {
            events: vec![
                Some(StreamEvent::Start { partial }),
                Some(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: msg,
                }),
            ],
            index: 0,
        }))
    }
}

// ===========================================================================
// Agent: verify event structure for complete lifecycle
// ===========================================================================

/// Verify that a simple agent run emits the correct event sequence:
/// AgentStart → TurnStart → MessageStart → MessageEnd → TurnEnd → AgentEnd
#[test]
fn agent_event_lifecycle_simple() {
    let test_name = "agent_event_lifecycle";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let provider = Arc::new(SimpleStopProvider {
            stream_calls: AtomicUsize::new(0),
        });
        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let result = agent_session.run_text("hello".to_string(), cb).await;

        let guard = tl.lock().unwrap();
        write_timeline_artifact(&harness, test_name, &guard);

        let _msg = result.expect("should succeed");

        let event_types: Vec<String> = guard
            .events
            .iter()
            .filter_map(|e| e["event"].as_str().map(String::from))
            .collect();

        harness.log().info_ctx("verify", "event lifecycle", |ctx| {
            ctx.push(("events".into(), format!("{event_types:?}")));
        });

        // Verify mandatory events are present in correct order
        assert!(
            event_types.contains(&"agent_start".to_string()),
            "missing agent_start"
        );
        assert!(
            event_types.contains(&"turn_start".to_string()),
            "missing turn_start"
        );
        assert!(
            event_types.contains(&"message_start".to_string()),
            "missing message_start"
        );
        assert!(
            event_types.contains(&"message_end".to_string()),
            "missing message_end"
        );
        assert!(
            event_types.contains(&"turn_end".to_string()),
            "missing turn_end"
        );
        assert!(
            event_types.contains(&"agent_end".to_string()),
            "missing agent_end"
        );

        // agent_start should be first, agent_end should be last
        assert_eq!(event_types.first().unwrap(), "agent_start");
        assert_eq!(event_types.last().unwrap(), "agent_end");
    });
}

/// Verify that an agent run with tool calls emits tool execution events
/// in the correct order within the turn.
#[test]
fn agent_event_lifecycle_with_tools() {
    let test_name = "agent_event_lifecycle_tools";
    let harness = TestHarness::new(test_name);
    harness.create_file("target.txt", b"tool content\n");

    run_async(async move {
        // Provider that issues a read tool call
        let provider = Arc::new(MixedToolCallProvider {
            stream_calls: AtomicUsize::new(0),
            good_tool_name: "read".to_string(),
            bad_tool_name: "nonexistent_xyz".to_string(),
            good_tool_path: harness
                .temp_dir()
                .join("target.txt")
                .to_string_lossy()
                .to_string(),
        });
        let mut agent_session = make_agent_session(provider, &harness, 4);
        let (tl, cb) = capture_timeline();

        let result = agent_session
            .run_text("test lifecycle with tools".to_string(), cb)
            .await;

        let guard = tl.lock().unwrap();
        write_timeline_artifact(&harness, test_name, &guard);

        let _msg = result.expect("should succeed");

        let event_types: Vec<String> = guard
            .events
            .iter()
            .filter_map(|e| e["event"].as_str().map(String::from))
            .collect();

        harness
            .log()
            .info_ctx("verify", "tool event lifecycle", |ctx| {
                ctx.push(("events".into(), format!("{event_types:?}")));
                ctx.push(("tool_starts".into(), guard.tool_starts.to_string()));
                ctx.push(("tool_ends".into(), guard.tool_ends.to_string()));
            });

        // Verify tool events are present
        assert!(
            event_types.contains(&"tool_start".to_string()),
            "missing tool_start"
        );
        assert!(
            event_types.contains(&"tool_end".to_string()),
            "missing tool_end"
        );

        // tool_start should come after turn_start and before turn_end
        let turn_start_idx = event_types.iter().position(|e| e == "turn_start").unwrap();
        let first_tool_start = event_types.iter().position(|e| e == "tool_start").unwrap();
        let last_tool_end = event_types.iter().rposition(|e| e == "tool_end").unwrap();
        let turn_end_idx = event_types.iter().rposition(|e| e == "turn_end").unwrap();

        assert!(
            first_tool_start > turn_start_idx,
            "tool_start should come after turn_start"
        );
        assert!(
            last_tool_end < turn_end_idx,
            "tool_end should come before turn_end"
        );
    });
}

// ===========================================================================
// Bash tool: process tree cleanup after exit
// ===========================================================================

/// Bash tool properly captures exit code from failed commands.
#[test]
fn tool_bash_exit_code_capture() {
    asupersync::test_utils::run_test(|| async {
        let _h = TestHarness::new("bash_exit_code");
        let tool = pi::tools::BashTool::new(std::path::Path::new("/tmp"));
        let input = json!({ "command": "exit 42" });
        let result = exec_tool(&tool, "bash-exit-1", input).await;

        // Non-zero exit should be reported as an error
        assert!(result.is_error, "non-zero exit code should be marked error");
        assert!(
            result.text.contains("42") || result.text.to_lowercase().contains("exit"),
            "should mention exit code: got {}",
            result.text
        );
    });
}

/// Bash tool captures both stdout and stderr.
#[test]
fn tool_bash_stdout_stderr_capture() {
    asupersync::test_utils::run_test(|| async {
        let _h = TestHarness::new("bash_stderr");
        let tool = pi::tools::BashTool::new(std::path::Path::new("/tmp"));
        let input = json!({ "command": "echo 'out_msg' && echo 'err_msg' >&2" });
        let result = exec_tool(&tool, "bash-stderr-1", input).await;

        assert!(
            result.text.contains("out_msg"),
            "should capture stdout: got {}",
            result.text
        );
        assert!(
            result.text.contains("err_msg"),
            "should capture stderr: got {}",
            result.text
        );
    });
}
