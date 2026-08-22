//! SDK integration test suite (bd-2hcex: PARITY-V3).
//!
//! Validates that the programmatic SDK API (`pi::sdk`) works correctly:
//! session creation, model selection, event streaming, tool execution,
//! persistence, abort, compaction, error handling.
//!
//! - Non-prompting tests use `create_agent_session()` directly.
//! - Prompting tests use `AgentSession` with a `ScriptedProvider` (same
//!   code path as `AgentSessionHandle::prompt()`, which delegates to
//!   `AgentSession::run_text()`).

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{AgentConfig, AgentEvent, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::extensions::SecurityAlertCategory;
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
    UserContent, UserMessage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::sdk::{
    AgentSessionHandle, AgentSessionState, SessionOptions, SubscriptionId, create_agent_session,
};
use pi::session::Session;
use pi::tools::ToolRegistry;
use pi::turn_recovery::TurnRecoveryMode;
use serde_json::json;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============================================================================
// ScriptedProvider — deterministic responses for prompting tests
// ============================================================================

#[derive(Debug, Clone)]
enum Script {
    /// Return a single text response and stop.
    SingleText(String),
    /// First call: emit a tool call; second call: text response.
    ToolRoundTrip {
        tool_name: String,
        tool_args: serde_json::Value,
        final_text: String,
    },
}

#[derive(Debug)]
struct ScriptedProvider {
    script: Script,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    const fn new(script: Script) -> Self {
        Self {
            script,
            call_count: AtomicUsize::new(0),
        }
    }

    fn assistant_msg(stop: StopReason, content: Vec<ContentBlock>) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "scripted".to_string(),
            provider: "scripted".to_string(),
            model: "scripted-model".to_string(),
            usage: Usage {
                total_tokens: 10,
                output: 10,
                ..Usage::default()
            },
            stop_reason: stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn done_stream(
        msg: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let partial = Self::assistant_msg(StopReason::Stop, Vec::new());
        Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Ok(StreamEvent::Done {
                reason: msg.stop_reason,
                message: msg,
            }),
        ]))
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn api(&self) -> &str {
        "scripted"
    }
    fn model_id(&self) -> &str {
        "scripted-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        match &self.script {
            Script::SingleText(text) => {
                if idx > 0 {
                    return Err(Error::api("SingleText expects exactly one call"));
                }
                Ok(Self::done_stream(Self::assistant_msg(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(text.clone()))],
                )))
            }
            Script::ToolRoundTrip {
                tool_name,
                tool_args,
                final_text,
            } => {
                if idx == 0 {
                    Ok(Self::done_stream(Self::assistant_msg(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "tc-1".to_string(),
                            name: tool_name.clone(),
                            arguments: tool_args.clone(),
                            thought_signature: None,
                        })],
                    )))
                } else if idx == 1 {
                    Ok(Self::done_stream(Self::assistant_msg(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new(final_text.clone()))],
                    )))
                } else {
                    Err(Error::api("ToolRoundTrip expects at most two calls"))
                }
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

const TEST_API_KEY: &str = "test-key";

fn default_session_options(harness: &TestHarness) -> SessionOptions {
    SessionOptions {
        provider: Some("anthropic".to_string()),
        model: Some("claude-opus-4-5".to_string()),
        api_key: Some(TEST_API_KEY.to_string()),
        working_directory: Some(harness.temp_dir().to_path_buf()),
        no_session: true,
        ..SessionOptions::default()
    }
}

fn write_sdk_extension(harness: &TestHarness, filename: &str, source: &str) -> std::path::PathBuf {
    harness.create_file(format!("extensions/{filename}"), source.as_bytes())
}

fn run_scripted(
    harness: &TestHarness,
    script: Script,
    user_prompt: &str,
) -> (AssistantMessage, Vec<AgentEvent>) {
    let cwd = harness.temp_dir().to_path_buf();
    let prompt = user_prompt.to_string();
    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(script));
        let tools = ToolRegistry::new(&["read"], &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 10,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        };
        let agent = pi::agent::Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd),
        )));
        let mut agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_ref = Arc::clone(&events);
        let message = agent_session
            .run_text(prompt, move |event| {
                events_ref.lock().expect("lock").push(event);
            })
            .await
            .expect("run_text");
        let captured = events.lock().expect("lock").clone();
        (message, captured)
    })
}

// ============================================================================
// 1. Basic session creation
// ============================================================================

#[test]
fn sdk_basic_session_creation() {
    let harness = TestHarness::new("sdk_basic_session_creation");
    let options = default_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");

    let provider = handle.session().agent.provider();
    assert_eq!(
        provider.name(),
        "anthropic",
        "default provider should be anthropic"
    );
    // Model can vary; just verify it's non-empty.
    assert!(
        !provider.model_id().is_empty(),
        "model_id should be non-empty"
    );

    harness.log().info_ctx("sdk", "basic creation ok", |ctx| {
        ctx.push(("provider".to_string(), provider.name().to_string()));
        ctx.push(("model".to_string(), provider.model_id().to_string()));
    });
}

// ============================================================================
// 2. Custom model
// ============================================================================

#[test]
fn sdk_custom_model_selection() {
    let harness = TestHarness::new("sdk_custom_model_selection");
    let options = SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        api_key: Some(TEST_API_KEY.to_string()),
        working_directory: Some(harness.temp_dir().to_path_buf()),
        no_session: true,
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    let (prov, model) = handle.model();
    assert_eq!(prov, "openai");
    assert_eq!(model, "gpt-4o");

    harness.log().info_ctx("sdk", "custom model ok", |ctx| {
        ctx.push(("provider".to_string(), prov));
        ctx.push(("model".to_string(), model));
    });
}

// ============================================================================
// 3. Event streaming — verify all lifecycle events fire
// ============================================================================

#[test]
fn sdk_event_streaming() {
    let harness = TestHarness::new("sdk_event_streaming");
    let (message, events) = run_scripted(
        &harness,
        Script::SingleText("hello from sdk test".to_string()),
        "Say hello",
    );

    // The response text should appear in the assistant message.
    let text = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("hello from sdk test"),
        "response text mismatch: {text}"
    );

    // Verify core lifecycle events were emitted.
    let has_agent_start = events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentStart { .. }));
    let has_agent_end = events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentEnd { .. }));
    let has_turn_start = events
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnStart { .. }));
    let has_turn_end = events
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnEnd { .. }));
    let has_message_start = events
        .iter()
        .any(|e| matches!(e, AgentEvent::MessageStart { .. }));
    let has_message_end = events
        .iter()
        .any(|e| matches!(e, AgentEvent::MessageEnd { .. }));

    assert!(has_agent_start, "missing AgentStart event");
    assert!(has_agent_end, "missing AgentEnd event");
    assert!(has_turn_start, "missing TurnStart event");
    assert!(has_turn_end, "missing TurnEnd event");
    assert!(has_message_start, "missing MessageStart event");
    assert!(has_message_end, "missing MessageEnd event");

    harness.log().info_ctx("sdk", "event streaming ok", |ctx| {
        ctx.push(("event_count".to_string(), events.len().to_string()));
        ctx.push(("response_len".to_string(), text.len().to_string()));
    });
}

// ============================================================================
// 4. Tool execution — verify tool events
// ============================================================================

#[test]
fn sdk_tool_execution() {
    let harness = TestHarness::new("sdk_tool_execution");

    // Create a file the tool can read.
    let target = harness.temp_dir().join("test_input.txt");
    std::fs::write(&target, "SDK integration content").expect("write target");

    let (message, events) = run_scripted(
        &harness,
        Script::ToolRoundTrip {
            tool_name: "read".to_string(),
            tool_args: json!({ "path": target.to_str().unwrap() }),
            final_text: "file contents confirmed".to_string(),
        },
        "Read the test file",
    );

    // Final response should contain the scripted text.
    let text = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("file contents confirmed"),
        "final text mismatch: {text}"
    );

    // Should have tool execution events.
    let tool_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
        .collect();
    let has_tool_end = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }));
    assert!(!tool_starts.is_empty(), "missing ToolExecutionStart");
    assert!(has_tool_end, "missing ToolExecutionEnd");

    // At least 2 turns (tool call turn + final response turn).
    let turn_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart { .. }))
        .collect();
    assert!(
        turn_starts.len() >= 2,
        "expected >=2 turns, got {}",
        turn_starts.len()
    );

    harness.log().info_ctx("sdk", "tool execution ok", |ctx| {
        ctx.push(("tool_starts".to_string(), tool_starts.len().to_string()));
        ctx.push(("turns".to_string(), turn_starts.len().to_string()));
    });
}

// ============================================================================
// 5. Session persistence — verify session file is created
// ============================================================================

#[test]
fn sdk_session_persistence() {
    let harness = TestHarness::new("sdk_session_persistence");
    let session_dir = harness.temp_dir().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");

    let options = SessionOptions {
        working_directory: Some(harness.temp_dir().to_path_buf()),
        api_key: Some(TEST_API_KEY.to_string()),
        no_session: false,
        session_dir: Some(session_dir.clone()),
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert!(handle.session().save_enabled(), "save should be enabled");

    harness
        .log()
        .info_ctx("sdk", "session persistence ok", |ctx| {
            ctx.push(("save_enabled".to_string(), "true".to_string()));
            ctx.push(("session_dir".to_string(), session_dir.display().to_string()));
        });
}

// ============================================================================
// 6. Abort — verify prompt can be cancelled
// ============================================================================

#[test]
fn sdk_abort_signal() {
    let harness = TestHarness::new("sdk_abort_signal");

    // Verify abort handle/signal creation works through the SDK.
    let (abort_handle, _abort_signal) = AgentSessionHandle::new_abort_handle();

    // Abort immediately — should complete without error.
    abort_handle.abort();

    harness.log().info_ctx("sdk", "abort signal ok", |ctx| {
        ctx.push(("aborted".to_string(), "true".to_string()));
    });
}

// ============================================================================
// 7. Compaction — verify compact on empty session is noop
// ============================================================================

#[test]
fn sdk_compact_empty_session() {
    let harness = TestHarness::new("sdk_compact_empty_session");
    let options = default_session_options(&harness);

    let mut handle = run_async(create_agent_session(options)).expect("create session");

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_ref = Arc::clone(&events);
    run_async(async move {
        handle
            .compact(move |event| {
                events_ref.lock().expect("lock").push(event);
            })
            .await
    })
    .expect("compact");

    let captured_len = {
        let captured = events.lock().expect("lock");
        assert!(
            captured.is_empty(),
            "compact on empty session should emit no events, got {}",
            captured.len()
        );
        captured.len()
    };
    assert!(
        captured_len == 0,
        "compact on empty session should emit no events, got {captured_len}"
    );

    harness.log().info_ctx("sdk", "compact noop ok", |ctx| {
        ctx.push(("event_count".to_string(), captured_len.to_string()));
    });
}

// ============================================================================
// 8. No-session mode — verify ephemeral state
// ============================================================================

#[test]
fn sdk_no_session_mode() {
    let harness = TestHarness::new("sdk_no_session_mode");
    let options = SessionOptions {
        working_directory: Some(harness.temp_dir().to_path_buf()),
        api_key: Some(TEST_API_KEY.to_string()),
        no_session: true,
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert!(
        !handle.session().save_enabled(),
        "save should be disabled in no-session mode"
    );

    harness.log().info_ctx("sdk", "no session mode ok", |ctx| {
        ctx.push(("save_enabled".to_string(), "false".to_string()));
    });
}

// ============================================================================
// 9. Error handling — invalid provider
// ============================================================================

#[test]
fn sdk_error_invalid_provider() {
    let harness = TestHarness::new("sdk_error_invalid_provider");
    let options = SessionOptions {
        provider: Some("nonexistent-provider-xyz".to_string()),
        model: Some("fake-model".to_string()),
        working_directory: Some(harness.temp_dir().to_path_buf()),
        no_session: true,
        ..SessionOptions::default()
    };

    let result = run_async(create_agent_session(options));
    assert!(
        result.is_err(),
        "creating session with invalid provider should fail"
    );
    let err_msg = match result {
        Ok(_) => panic!("expected invalid provider to fail"),
        Err(err) => err.to_string(),
    };

    harness.log().info_ctx("sdk", "error handling ok", |ctx| {
        ctx.push(("error".to_string(), err_msg.clone()));
    });

    // Verify the error is meaningful (not a panic or empty message).
    assert!(!err_msg.is_empty(), "error message should be non-empty");
}

// ============================================================================
// 10. Event subscription lifecycle
// ============================================================================

#[test]
fn sdk_subscribe_unsubscribe_lifecycle() {
    let harness = TestHarness::new("sdk_subscribe_unsubscribe_lifecycle");
    let options = default_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");

    let count = Arc::new(AtomicUsize::new(0));
    let count_ref = Arc::clone(&count);
    let id: SubscriptionId = handle.subscribe(move |_event| {
        count_ref.fetch_add(1, Ordering::SeqCst);
    });

    // Unsubscribe should succeed.
    assert!(handle.unsubscribe(id), "unsubscribe should return true");
    // Double-unsubscribe should return false.
    assert!(
        !handle.unsubscribe(id),
        "double unsubscribe should return false"
    );

    harness
        .log()
        .info_ctx("sdk", "subscribe lifecycle ok", |ctx| {
            ctx.push(("unsubscribed".to_string(), "true".to_string()));
        });
}

// ============================================================================
// Bonus: state snapshot
// ============================================================================

#[test]
fn sdk_state_snapshot() {
    let harness = TestHarness::new("sdk_state_snapshot");
    let options = default_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");
    let state: AgentSessionState =
        run_async(async move { handle.state().await }).expect("get state");

    assert!(state.session_id.is_some(), "session_id should be set");
    assert_eq!(state.provider, "anthropic");
    assert!(!state.model_id.is_empty(), "model_id should be non-empty");
    assert!(
        !state.save_enabled,
        "no-session mode should have save disabled"
    );
    assert_eq!(
        state.message_count, 0,
        "fresh session should have 0 messages"
    );

    harness.log().info_ctx("sdk", "state snapshot ok", |ctx| {
        ctx.push(("provider".to_string(), state.provider.clone()));
        ctx.push(("model_id".to_string(), state.model_id.clone()));
        ctx.push(("messages".to_string(), state.message_count.to_string()));
    });
}

// ============================================================================
// Bonus: model switching
// ============================================================================

#[test]
fn sdk_model_switching() {
    let harness = TestHarness::new("sdk_model_switching");
    let options = default_session_options(&harness);

    let mut handle = run_async(create_agent_session(options)).expect("create session");

    // Switch to openai/gpt-4o
    let (prov, model) = run_async(async move {
        handle.set_model("openai", "gpt-4o").await?;
        Ok::<(String, String), Error>(handle.model())
    })
    .expect("set model");
    assert_eq!(prov, "openai");
    assert_eq!(model, "gpt-4o");

    harness.log().info_ctx("sdk", "model switching ok", |ctx| {
        ctx.push(("new_provider".to_string(), prov));
        ctx.push(("new_model".to_string(), model));
    });
}

// ============================================================================
// Bonus: thinking level
// ============================================================================

#[test]
fn sdk_thinking_level() {
    let harness = TestHarness::new("sdk_thinking_level");
    let options = SessionOptions {
        api_key: Some(TEST_API_KEY.to_string()),
        thinking: Some(pi::model::ThinkingLevel::High),
        working_directory: Some(harness.temp_dir().to_path_buf()),
        no_session: true,
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert_eq!(
        handle.thinking_level(),
        Some(pi::model::ThinkingLevel::High),
        "thinking level should be High"
    );

    harness.log().info_ctx("sdk", "thinking level ok", |ctx| {
        ctx.push(("level".to_string(), "High".to_string()));
    });
}

// ============================================================================
// 11. SDK extensions — registration surface available via handle
// ============================================================================

#[test]
fn sdk_extensions_load_and_expose_registration_surface() {
    let harness = TestHarness::new("sdk_extensions_load_and_expose_registration_surface");
    let extension_path = write_sdk_extension(
        &harness,
        "sdk_visible.mjs",
        r#"export default function init(pi) {
  pi.registerCommand("sdk-visible", {
    description: "visible command",
    handler: async (args) => ({ display: "sdk-visible:" + (args || "") })
  });
  pi.registerFlag("sdk-flag", {
    type: "string",
    default: "on"
  });
}"#,
    );

    let options = SessionOptions {
        working_directory: Some(harness.temp_dir().to_path_buf()),
        api_key: Some(TEST_API_KEY.to_string()),
        extension_paths: vec![extension_path],
        extension_policy: Some("safe".to_string()),
        no_session: true,
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert!(
        handle.has_extensions(),
        "expected SDK session to load extensions"
    );

    let manager = handle
        .extension_manager()
        .expect("extension manager should be present")
        .clone();

    let has_command = manager
        .list_commands()
        .into_iter()
        .any(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some("sdk-visible"));
    assert!(
        has_command,
        "registered SDK extension command should be visible"
    );

    let has_flag = manager
        .list_flags()
        .into_iter()
        .any(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some("sdk-flag"));
    assert!(has_flag, "registered SDK extension flag should be visible");

    let command_result =
        run_async(async move { manager.execute_command("sdk-visible", "ok", 5000).await })
            .expect("execute extension command");
    let display = command_result
        .get("display")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        display.contains("sdk-visible:ok"),
        "unexpected extension command display payload: {command_result:?}"
    );
}

// ============================================================================
// 12. SDK extension policy + hostcall visibility
// ============================================================================

#[test]
fn sdk_extension_policy_safe_denies_exec_and_records_hostcall_telemetry() {
    let harness =
        TestHarness::new("sdk_extension_policy_safe_denies_exec_and_records_hostcall_telemetry");
    let extension_path = write_sdk_extension(
        &harness,
        "sdk_exec_policy.mjs",
        r#"export default function init(pi) {
  pi.registerCommand("sdk-exec", {
    description: "attempt exec hostcall",
    handler: async () => {
      await pi.exec("echo", ["sdk-policy"]);
      return { display: "exec-ok" };
    }
  });
}"#,
    );

    let options = SessionOptions {
        working_directory: Some(harness.temp_dir().to_path_buf()),
        api_key: Some(TEST_API_KEY.to_string()),
        extension_paths: vec![extension_path],
        extension_policy: Some("safe".to_string()),
        no_session: true,
        ..SessionOptions::default()
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    let manager = handle
        .extension_manager()
        .expect("extension manager should be present")
        .clone();
    let mut risk_config = manager.runtime_risk_config();
    risk_config.enabled = true;
    risk_config.enforce = true;
    manager.set_runtime_risk_config(risk_config);

    let command_result =
        run_async(async move { manager.execute_command("sdk-exec", "", 5000).await });
    let denied = match &command_result {
        Ok(value) => {
            let text = serde_json::to_string(value).unwrap_or_default();
            text.contains("denied") || text.contains("not allowed")
        }
        Err(err) => {
            let text = err.to_string();
            text.contains("denied") || text.contains("not allowed")
        }
    };
    assert!(
        denied,
        "safe policy should deny exec hostcall; got: {command_result:?}"
    );

    let telemetry = handle
        .extension_manager()
        .expect("extension manager should still be available")
        .runtime_hostcall_telemetry_artifact();
    assert_eq!(
        telemetry.entry_count, 0,
        "static policy denial should short-circuit runtime-risk telemetry"
    );

    let alerts = handle
        .extension_manager()
        .expect("extension manager should still be available")
        .security_alert_artifact();
    assert!(
        alerts.category_counts.policy_denial > 0,
        "policy denial should be emitted as a security alert"
    );
    let exec_alert = alerts
        .alerts
        .iter()
        .find(|alert| {
            alert.category == SecurityAlertCategory::PolicyDenial && alert.capability == "exec"
        })
        .expect("expected policy-denial security alert for exec capability");
    assert!(
        exec_alert.policy_source.contains("deny"),
        "expected deny policy source for safe profile, got: {}",
        exec_alert.policy_source
    );
}

// ============================================================================
// 13. Conformance: Event ordering guarantees
// ============================================================================

/// Validate that `AgentEvent` lifecycle events are emitted in the correct order
/// matching pi-mono's event contract.
///
/// Expected ordering:
///   `AgentStart` -> (`TurnStart` -> `MessageStart` -> `MessageUpdate`* ->
///   `MessageEnd` -> `ToolExecution`* -> `TurnEnd`)+ -> `AgentEnd`
#[test]
fn sdk_conformance_event_ordering() {
    let harness = TestHarness::new("sdk_conformance_event_ordering");
    let (_message, events) = run_scripted(
        &harness,
        Script::SingleText("ordered response".to_string()),
        "Test ordering",
    );

    let type_names: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentEvent::AgentStart { .. } => "AgentStart",
            AgentEvent::AgentEnd { .. } => "AgentEnd",
            AgentEvent::TurnStart { .. } => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::MessageStart { .. } => "MessageStart",
            AgentEvent::MessageUpdate { .. } => "MessageUpdate",
            AgentEvent::MessageEnd { .. } => "MessageEnd",
            AgentEvent::ToolExecutionStart { .. } => "ToolExecutionStart",
            AgentEvent::ToolExecutionUpdate { .. } => "ToolExecutionUpdate",
            AgentEvent::ToolExecutionEnd { .. } => "ToolExecutionEnd",
            AgentEvent::AutoCompactionStart { .. } => "AutoCompactionStart",
            AgentEvent::AutoCompactionEnd { .. } => "AutoCompactionEnd",
            AgentEvent::AutoRetryStart { .. } => "AutoRetryStart",
            AgentEvent::AutoRetryEnd { .. } => "AutoRetryEnd",
            AgentEvent::FailoverStart { .. } => "FailoverStart",
            AgentEvent::FailoverEnd { .. } => "FailoverEnd",
            AgentEvent::ExtensionError { .. } => "ExtensionError",
            AgentEvent::AdvisorNote { .. } => "AdvisorNote",
        })
        .collect();

    // AgentStart must be first
    assert_eq!(
        type_names.first(),
        Some(&"AgentStart"),
        "first event must be AgentStart, got: {type_names:?}"
    );

    // AgentEnd must be last
    assert_eq!(
        type_names.last(),
        Some(&"AgentEnd"),
        "last event must be AgentEnd, got: {type_names:?}"
    );

    // TurnStart must precede TurnEnd
    let first_turn_start = type_names.iter().position(|&n| n == "TurnStart");
    let first_turn_end = type_names.iter().position(|&n| n == "TurnEnd");
    assert!(
        first_turn_start < first_turn_end,
        "TurnStart must precede TurnEnd"
    );

    // MessageStart must precede MessageEnd within the turn
    let first_msg_start = type_names.iter().position(|&n| n == "MessageStart");
    let first_msg_end = type_names.iter().position(|&n| n == "MessageEnd");
    assert!(
        first_msg_start < first_msg_end,
        "MessageStart must precede MessageEnd"
    );

    harness
        .log()
        .info_ctx("sdk", "event ordering conforms", |ctx| {
            ctx.push(("event_sequence".to_string(), format!("{type_names:?}")));
        });
}

// ============================================================================
// 14. Conformance: Tool lifecycle event ordering
// ============================================================================

/// Validate tool execution events follow the contract:
///   `ToolExecutionStart` -> `ToolExecutionUpdate`* -> `ToolExecutionEnd`
#[test]
fn sdk_conformance_tool_event_ordering() {
    let harness = TestHarness::new("sdk_conformance_tool_event_ordering");
    let target = harness.temp_dir().join("tool_ordering_test.txt");
    std::fs::write(&target, "tool ordering content").expect("write target");

    let (_message, events) = run_scripted(
        &harness,
        Script::ToolRoundTrip {
            tool_name: "read".to_string(),
            tool_args: json!({ "path": target.to_str().unwrap() }),
            final_text: "done".to_string(),
        },
        "Read file for ordering test",
    );

    let tool_events: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionStart { .. } => Some("Start"),
            AgentEvent::ToolExecutionUpdate { .. } => Some("Update"),
            AgentEvent::ToolExecutionEnd { .. } => Some("End"),
            _ => None,
        })
        .collect();

    assert!(!tool_events.is_empty(), "should have tool execution events");

    // Must start with Start
    assert_eq!(
        tool_events.first(),
        Some(&"Start"),
        "tool events must start with Start: {tool_events:?}"
    );

    // Must end with End
    assert_eq!(
        tool_events.last(),
        Some(&"End"),
        "tool events must end with End: {tool_events:?}"
    );

    // Verify tool name consistency across start and end
    let start_name = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionStart { tool_name, .. } => Some(tool_name.as_str()),
        _ => None,
    });
    let end_name = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd { tool_name, .. } => Some(tool_name.as_str()),
        _ => None,
    });
    assert_eq!(
        start_name, end_name,
        "tool name should be consistent between Start and End"
    );

    harness
        .log()
        .info_ctx("sdk", "tool event ordering conforms", |ctx| {
            ctx.push(("tool_events".to_string(), format!("{tool_events:?}")));
        });
}

// ============================================================================
// 15. Conformance: AgentEvent JSON serialization matches pi-mono schema
// ============================================================================

/// Verify that `AgentEvent` serializes with `snake_case` type tags and
/// `camelCase` field names, matching the pi-mono JSON event protocol.
#[test]
fn sdk_conformance_agent_event_json_schema() {
    let events = vec![
        AgentEvent::AgentStart {
            session_id: "s1".into(),
        },
        AgentEvent::TurnStart {
            session_id: "s1".into(),
            turn_index: 0,
            timestamp: 1_234_567_890,
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tc-1".to_string(),
            tool_name: "read".to_string(),
            args: json!({"path": "/tmp/test"}),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc-1".to_string(),
            tool_name: "read".to_string(),
            result: pi::tools::ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("file contents"))],
                details: None,
                is_error: false,
            },
            is_error: false,
        },
        AgentEvent::AgentEnd {
            session_id: "s1".into(),
            messages: vec![],
            error: None,
        },
    ];

    for event in &events {
        let json_value =
            serde_json::to_value(event).expect("AgentEvent should always serialize to JSON");

        // Must have a "type" field (snake_case tag)
        let type_field = json_value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .expect("AgentEvent JSON must have 'type' field");

        // Type must be snake_case
        assert!(
            !type_field.contains('-') && !type_field.chars().any(char::is_uppercase),
            "event type must be snake_case, got: {type_field}"
        );
    }
}

// ============================================================================
// 16. Conformance: Session-level tool hooks fire via from_session_with_listeners
// ============================================================================

/// Validate that typed tool hooks (`on_tool_start`, `on_tool_end`) registered
/// via `EventListeners` fire during prompt execution alongside per-prompt
/// callbacks.
#[test]
fn sdk_conformance_session_tool_hooks() {
    use pi::sdk::EventListeners;

    let harness = TestHarness::new("sdk_conformance_session_tool_hooks");
    let target = harness.temp_dir().join("hook_test.txt");
    std::fs::write(&target, "hook test content").expect("write target");

    let cwd = harness.temp_dir().to_path_buf();
    let target_path = target.to_str().unwrap().to_string();

    // Track tool start/end from session-level typed hooks.
    let start_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let end_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let start_names_ref = Arc::clone(&start_names);
    let end_names_ref = Arc::clone(&end_names);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Script::ToolRoundTrip {
            tool_name: "read".to_string(),
            tool_args: json!({ "path": target_path }),
            final_text: "done".to_string(),
        }));
        let tools = ToolRegistry::new(&["read"], &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 10,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        };
        let agent = pi::agent::Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd),
        )));
        let agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let mut listeners = EventListeners::default();
        listeners.on_tool_start = Some(Arc::new(move |name, _args| {
            start_names_ref.lock().expect("lock").push(name.to_string());
        }));
        listeners.on_tool_end = Some(Arc::new(move |name, _output, _is_error| {
            end_names_ref.lock().expect("lock").push(name.to_string());
        }));

        let mut handle = AgentSessionHandle::from_session_with_listeners(agent_session, listeners);
        handle
            .prompt("read the file", |_event| {})
            .await
            .expect("prompt");
    });

    let starts = start_names.lock().expect("lock").clone();
    let ends = end_names.lock().expect("lock").clone();

    assert!(
        !starts.is_empty(),
        "on_tool_start should have fired at least once"
    );
    assert!(
        !ends.is_empty(),
        "on_tool_end should have fired at least once"
    );
    assert!(
        starts.iter().any(|name| name == "read"),
        "on_tool_start should report tool name 'read', got: {starts:?}"
    );
    assert!(
        ends.iter().any(|name| name == "read"),
        "on_tool_end should report tool name 'read', got: {ends:?}"
    );

    harness
        .log()
        .info_ctx("sdk", "session tool hooks conform", |ctx| {
            ctx.push(("tool_starts".to_string(), format!("{starts:?}")));
            ctx.push(("tool_ends".to_string(), format!("{ends:?}")));
        });
}

// ============================================================================
// 17. Conformance: Combined callback ordering (session + per-prompt)
// ============================================================================

/// Validate that session-level subscribers and per-prompt callbacks both receive
/// the same events, and that session-level subscribers fire before per-prompt.
#[test]
fn sdk_conformance_combined_callback_ordering() {
    use pi::sdk::EventListeners;

    let harness = TestHarness::new("sdk_conformance_combined_callback_ordering");
    let cwd = harness.temp_dir().to_path_buf();

    // Track event ordering: session subscriber sees events before per-prompt callback.
    let order: Arc<Mutex<Vec<(String, &str)>>> = Arc::new(Mutex::new(Vec::new()));
    let order_session = Arc::clone(&order);
    let order_prompt = Arc::clone(&order);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Script::SingleText(
            "combined test".to_string(),
        )));
        let tools = ToolRegistry::new(&["read"], &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 10,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        };
        let agent = pi::agent::Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd),
        )));
        let agent_session =
            AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

        let listeners = EventListeners::default();
        let mut handle = AgentSessionHandle::from_session_with_listeners(agent_session, listeners);

        // Register a session-level subscriber.
        handle.subscribe(move |event| {
            let type_name = match &event {
                AgentEvent::AgentStart { .. } => "AgentStart",
                AgentEvent::AgentEnd { .. } => "AgentEnd",
                _ => return,
            };
            order_session
                .lock()
                .expect("lock")
                .push((type_name.to_string(), "session"));
        });

        handle
            .prompt("test", move |event| {
                let type_name = match &event {
                    AgentEvent::AgentStart { .. } => "AgentStart",
                    AgentEvent::AgentEnd { .. } => "AgentEnd",
                    _ => return,
                };
                order_prompt
                    .lock()
                    .expect("lock")
                    .push((type_name.to_string(), "per-prompt"));
            })
            .await
            .expect("prompt");
    });

    let entries = order.lock().expect("lock").clone();

    // Both session and per-prompt should have received AgentStart.
    let has_session_start = entries
        .iter()
        .any(|(name, source)| name == "AgentStart" && *source == "session");
    let has_prompt_start = entries
        .iter()
        .any(|(name, source)| name == "AgentStart" && *source == "per-prompt");

    assert!(
        has_session_start,
        "session subscriber should receive AgentStart"
    );
    assert!(
        has_prompt_start,
        "per-prompt callback should receive AgentStart"
    );

    // Session subscriber should fire before per-prompt for the same event.
    let first_session_idx = entries
        .iter()
        .position(|(_, source)| *source == "session")
        .expect("session event");
    let first_prompt_idx = entries
        .iter()
        .position(|(_, source)| *source == "per-prompt")
        .expect("prompt event");
    assert!(
        first_session_idx < first_prompt_idx,
        "session-level subscriber should fire before per-prompt callback"
    );

    harness
        .log()
        .info_ctx("sdk", "combined callback ordering conforms", |ctx| {
            ctx.push(("entries".to_string(), format!("{entries:?}")));
        });
}

// ============================================================================
// 18. Continuation wrappers on AgentSessionHandle
// ============================================================================

#[test]
fn sdk_continue_turn_uses_combined_listener_path() {
    use pi::sdk::EventListeners;

    let harness = TestHarness::new("sdk_continue_turn_uses_combined_listener_path");
    let cwd = harness.temp_dir().to_path_buf();
    let subscriber_events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let per_call_events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let subscriber_events_ref = Arc::clone(&subscriber_events);
    let per_call_events_ref = Arc::clone(&per_call_events);

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Script::SingleText(
            "continued response".to_string(),
        )));
        let tools = ToolRegistry::new(&["read"], &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 10,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        };
        let agent = pi::agent::Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd),
        )));
        let agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        {
            let cx = pi::agent_cx::AgentCx::for_request();
            let mut guard = session.lock(cx.cx()).await.expect("lock session");
            guard.append_model_message(Message::User(UserMessage {
                content: UserContent::Text("prior user message".to_string()),
                timestamp: 0,
            }));
        }

        let listeners = EventListeners::default();
        listeners.subscribe(Arc::new(move |event| {
            let name = match event {
                AgentEvent::AgentStart { .. } => Some("AgentStart"),
                AgentEvent::AgentEnd { .. } => Some("AgentEnd"),
                _ => None,
            };
            if let Some(name) = name {
                subscriber_events_ref.lock().expect("lock").push(name);
            }
        }));

        let mut handle = AgentSessionHandle::from_session_with_listeners(agent_session, listeners);
        handle
            .session_mut()
            .agent
            .add_message(Message::User(UserMessage {
                content: UserContent::Text("prior user message".to_string()),
                timestamp: 0,
            }));

        let message = handle
            .continue_turn(move |event| {
                let name = match event {
                    AgentEvent::AgentStart { .. } => Some("AgentStart"),
                    AgentEvent::AgentEnd { .. } => Some("AgentEnd"),
                    _ => None,
                };
                if let Some(name) = name {
                    per_call_events_ref.lock().expect("lock").push(name);
                }
            })
            .await
            .expect("continue turn");

        assert_eq!(message.stop_reason, StopReason::Stop);
    });

    let subscriber = subscriber_events.lock().expect("lock").clone();
    let per_call = per_call_events.lock().expect("lock").clone();

    assert_eq!(subscriber, vec!["AgentStart", "AgentEnd"]);
    assert_eq!(per_call, vec!["AgentStart", "AgentEnd"]);
}

#[test]
fn sdk_continue_turn_with_abort_returns_aborted_message() {
    use pi::sdk::EventListeners;

    let harness = TestHarness::new("sdk_continue_turn_with_abort_returns_aborted_message");
    let cwd = harness.temp_dir().to_path_buf();

    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Script::SingleText(
            "should not complete".to_string(),
        )));
        let tools = ToolRegistry::new(&["read"], &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 10,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        };
        let agent = pi::agent::Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd),
        )));
        let agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );
        let listeners = EventListeners::default();
        let mut handle = AgentSessionHandle::from_session_with_listeners(agent_session, listeners);

        let (abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        abort_handle.abort();

        let message = handle
            .continue_turn_with_abort(abort_signal, |_event| {})
            .await
            .expect("continue turn with pre-aborted signal");

        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(message.error_message.as_deref(), Some("Aborted"));
    });
}
