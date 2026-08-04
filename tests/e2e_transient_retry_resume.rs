//! E2E: auto-retry on a transient connection error must RESUME the turn, not
//! replay it from the user message (`pi_agent_rust#125`).
//!
//! When a transient connection drop kills a mid-turn provider request — after
//! one or more tool calls have already executed — the retry must re-issue only
//! the failed request. It must NOT rewind to the user message and re-run the
//! whole turn, which would:
//!   * re-execute already-completed (side-effecting) tool calls, and
//!   * re-bill the tokens for prior assistant turns.
//!
//! These tests drive the exact sequence the print-mode (`src/main.rs`) and RPC
//! (`src/rpc.rs`) retry drivers use: first attempt via `run_text`, then on a
//! retryable failure `revert_incomplete_response()` + `run_continue_with_abort()`.
//! Before the fix this used `revert_last_user_message()` + a fresh `run_text`,
//! and the tool executed twice.

#![allow(clippy::too_many_lines)]

mod common;

use async_trait::async_trait;
use common::run_async;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const fn tool_names() -> [&'static str; 7] {
    ["read", "write", "edit", "bash", "grep", "find", "ls"]
}

fn make_assistant(stop_reason: StopReason, content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test-api".to_string(),
        provider: "repro".to_string(),
        model: "test-model".to_string(),
        usage: Usage {
            total_tokens: 10,
            output: 10,
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
) -> AgentSession {
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&tool_names(), cwd, None),
        AgentConfig {
            max_tool_iterations: 8,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    AgentSession::new(agent, session, true, ResolvedCompactionSettings::default())
}

/// How the transient failure surfaces once the tool result is present.
#[derive(Clone, Copy)]
enum FailMode {
    /// `provider.stream()` returns `Err` before any streaming (connection
    /// refused / reset before headers). Surfaces as `run_text` -> `Err`.
    BeforeResponse,
    /// Stream starts, then drops mid-body. Surfaces as `run_text` -> `Ok` with
    /// `StopReason::Error` and a partial assistant.
    MidStream,
}

/// Provider that:
///  - emits a `write` tool call whenever it does NOT yet see the tool result in
///    the context (counting each emission — a re-emission means the turn was
///    replayed from the user message), then
///  - once the tool result is present, fails ONCE with a transient connection
///    drop, and
///  - thereafter returns a final answer.
struct StepThenTransientProvider {
    tool_call_emissions: AtomicUsize,
    failed_once: AtomicBool,
    path: String,
    mode: FailMode,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for StepThenTransientProvider {
    fn name(&self) -> &str {
        "repro"
    }
    fn api(&self) -> &str {
        "test-api"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let have_tool_result = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "step1"));

        if !have_tool_result {
            // Model has not yet seen the tool result: request the tool.
            self.tool_call_emissions.fetch_add(1, Ordering::SeqCst);
            let msg = make_assistant(
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "step1".to_string(),
                    name: "write".to_string(),
                    arguments: json!({ "path": self.path, "content": "hello" }),
                    thought_signature: None,
                })],
            );
            return Ok(stream_done(msg));
        }

        // Tool result present: fail once transiently to simulate a mid-turn drop
        // that happens AFTER the tool already executed.
        if !self.failed_once.swap(true, Ordering::SeqCst) {
            let transient = "SSE error: connection reset by peer (transient connection drop)";
            return match self.mode {
                FailMode::BeforeResponse => Err(Error::api(transient)),
                FailMode::MidStream => {
                    let partial = make_assistant(StopReason::Stop, Vec::new());
                    Ok(Box::pin(futures::stream::iter(vec![
                        Ok(StreamEvent::Start { partial }),
                        Err(Error::api(transient)),
                    ])))
                }
            };
        }

        let msg = make_assistant(
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new("done"))],
        );
        Ok(stream_done(msg))
    }
}

/// Outcome of running the shared retry driver used below.
struct DriverOutcome {
    tool_call_emissions: usize,
    tool_execution_starts: usize,
    retries: u32,
    final_ok: bool,
}

/// Faithful mirror of the production retry drivers (`src/main.rs`
/// `run_print_prompt_with_retry` and `src/rpc.rs`): first attempt via
/// `run_text`, and on a retryable failure resume the turn via
/// `revert_incomplete_response()` + `run_continue_with_abort()`.
async fn run_with_retry_driver(
    sess: &mut AgentSession,
    prompt: &str,
    provider: &StepThenTransientProvider,
    tool_starts: &Arc<AtomicUsize>,
) -> DriverOutcome {
    let max_retries: u32 = 4;
    let ev = {
        let tsr = Arc::clone(tool_starts);
        move |event: AgentEvent| {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                tsr.fetch_add(1, Ordering::SeqCst);
            }
        }
    };

    let mut result = sess.run_text(prompt.to_string(), ev.clone()).await;
    let mut retries: u32 = 0;
    loop {
        let retry = match &result {
            Err(err) => {
                let s = err.to_string();
                retries < max_retries
                    && (err.is_transient() || pi::error::is_retryable_error(&s, None, None))
            }
            Ok(msg) => {
                matches!(msg.stop_reason, StopReason::Error)
                    && retries < max_retries
                    && pi::error::is_retryable_error(
                        msg.error_message.as_deref().unwrap_or(""),
                        Some(msg.usage.input),
                        None,
                    )
            }
        };
        if !retry {
            break;
        }
        retries += 1;
        // FIXED driver behavior: resume the turn, do not replay it.
        let _ = sess.revert_incomplete_response().await;
        result = sess.run_continue_with_abort(None, ev.clone()).await;
    }

    DriverOutcome {
        tool_call_emissions: provider.tool_call_emissions.load(Ordering::SeqCst),
        tool_execution_starts: tool_starts.load(Ordering::SeqCst),
        retries,
        final_ok: matches!(&result, Ok(m) if matches!(m.stop_reason, StopReason::Stop)),
    }
}

fn run_case(mode: FailMode) -> DriverOutcome {
    let tmp = std::env::temp_dir().join(format!(
        "pi_retry_resume_{}_{}",
        std::process::id(),
        u8::from(matches!(mode, FailMode::MidStream))
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let cwd = tmp;

    let provider = Arc::new(StepThenTransientProvider {
        tool_call_emissions: AtomicUsize::new(0),
        failed_once: AtomicBool::new(false),
        path: "out.txt".to_string(),
        mode,
    });
    let tool_starts = Arc::new(AtomicUsize::new(0));

    run_async({
        let provider = Arc::clone(&provider);
        let tool_starts = Arc::clone(&tool_starts);
        async move {
            let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(cwd.clone()),
            )));
            let mut sess =
                make_agent_session(&cwd, Arc::clone(&provider) as Arc<dyn Provider>, session);
            run_with_retry_driver(&mut sess, prompt_ref(), &provider, &tool_starts).await
        }
    })
}

const fn prompt_ref() -> &'static str {
    "please write the file"
}

/// A transient failure BEFORE any response (`run_text` -> `Err`) after the tool
/// executed: the retry resumes and does not re-run the tool.
#[test]
fn transient_error_before_response_resumes_without_reexecuting_tools() {
    let out = run_case(FailMode::BeforeResponse);
    assert!(out.final_ok, "turn should eventually complete successfully");
    assert_eq!(out.retries, 1, "exactly one retry should have occurred");
    assert_eq!(
        out.tool_call_emissions, 1,
        "provider must be asked for the tool call ONCE; >1 means the turn was replayed from the user message"
    );
    assert_eq!(
        out.tool_execution_starts, 1,
        "the tool must execute exactly ONCE; a transient retry must not duplicate side effects"
    );
}

/// A transient mid-stream drop (`run_text` -> `Ok` with `StopReason::Error`)
/// after the tool executed: the retry resumes and does not re-run the tool.
#[test]
fn transient_error_midstream_resumes_without_reexecuting_tools() {
    let out = run_case(FailMode::MidStream);
    assert!(out.final_ok, "turn should eventually complete successfully");
    assert_eq!(out.retries, 1, "exactly one retry should have occurred");
    assert_eq!(
        out.tool_call_emissions, 1,
        "provider must be asked for the tool call ONCE; >1 means the turn was replayed from the user message"
    );
    assert_eq!(
        out.tool_execution_starts, 1,
        "the tool must execute exactly ONCE; a transient retry must not duplicate side effects"
    );
}
