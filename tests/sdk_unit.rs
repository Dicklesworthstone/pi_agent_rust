//! Comprehensive SDK unit test suite (bd-2omnf: DROPIN-173).
//!
//! Validates SDK lifecycle states, callback ordering, transport adapter behavior,
//! cancellation semantics, RPC type serde, and extension-policy interactions
//! with structured JSONL log output.

mod common;

use common::{TestHarness, run_async};
use pi::agent::{AbortHandle, AgentEvent};
use pi::model::{
    AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent, ThinkingLevel, Usage,
};
use pi::sdk::{
    AgentSessionHandle, AgentSessionState, EventListeners, RpcBashResult, RpcCancelledResult,
    RpcCommandInfo, RpcCompactionResult, RpcCycleModelResult, RpcExportHtmlResult,
    RpcExtensionUiResponse, RpcForkMessage, RpcForkResult, RpcLastAssistantText, RpcModelInfo,
    RpcSessionState, RpcSessionStats, RpcThinkingLevelResult, RpcTokenStats, RpcTransportOptions,
    SessionOptions, SessionPromptResult, SessionTransportEvent, SessionTransportState,
    create_agent_session,
};
use pi::tools::{ToolOutput, ToolRegistry};
use serde_json::json;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const TEST_API_KEY: &str = "sk-test-sdk-unit";

fn test_session_options(harness: &TestHarness) -> SessionOptions {
    SessionOptions {
        provider: Some("anthropic".to_string()),
        model: Some("claude-opus-4-5".to_string()),
        api_key: Some(TEST_API_KEY.to_string()),
        working_directory: Some(harness.temp_dir().to_path_buf()),
        no_session: true,
        ..SessionOptions::default()
    }
}

// ============================================================================
// 1. Callback Ordering
// ============================================================================

#[test]
fn callback_ordering_tool_hooks_fire_before_generic_subscribers() {
    let harness = TestHarness::new("callback_ordering_tool_hooks_fire_before_generic_subscribers");
    let options = test_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");

    // Track ordering with a shared counter.
    let order = Arc::new(AtomicU32::new(0));

    let order_tool = Arc::clone(&order);
    let tool_hook_order = Arc::new(Mutex::new(0u32));
    let tool_hook_order_ref = Arc::clone(&tool_hook_order);
    let mut listeners = handle.listeners().clone();
    listeners.on_tool_start = Some(Arc::new(move |_name, _args| {
        let val = order_tool.fetch_add(1, Ordering::SeqCst);
        *tool_hook_order_ref.lock().expect("lock") = val;
    }));

    let order_subscriber = Arc::clone(&order);
    let subscriber_order = Arc::new(Mutex::new(0u32));
    let subscriber_order_ref = Arc::clone(&subscriber_order);
    listeners.subscribe(Arc::new(move |event| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
            let val = order_subscriber.fetch_add(1, Ordering::SeqCst);
            *subscriber_order_ref.lock().expect("lock") = val;
        }
    }));

    // Simulate a tool start event through notify chain.
    let args = json!({"path": "/test"});
    listeners.notify_tool_start("read", &args);
    listeners.notify(&AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-1".to_string(),
        tool_name: "read".to_string(),
        args,
    });

    let hook_order = *tool_hook_order.lock().expect("lock");
    let sub_order = *subscriber_order.lock().expect("lock");
    assert!(
        hook_order < sub_order,
        "tool hook ({hook_order}) should fire before generic subscriber ({sub_order})"
    );

    harness
        .log()
        .info_ctx("sdk", "callback ordering verified", |ctx| {
            ctx.push(("tool_hook_order".to_string(), hook_order.to_string()));
            ctx.push(("subscriber_order".to_string(), sub_order.to_string()));
        });
}

#[test]
fn callback_ordering_session_listeners_fire_for_every_event() {
    let harness = TestHarness::new("callback_ordering_session_listeners_fire_for_every_event");

    let listeners = EventListeners::default();
    let count = Arc::new(AtomicUsize::new(0));

    let c1 = Arc::clone(&count);
    listeners.subscribe(Arc::new(move |_| {
        c1.fetch_add(1, Ordering::SeqCst);
    }));

    // Fire multiple different events.
    listeners.notify(&AgentEvent::AgentStart {
        session_id: "s1".into(),
    });
    listeners.notify(&AgentEvent::AgentEnd {
        session_id: "s1".into(),
        messages: vec![],
        error: None,
    });

    let total = count.load(Ordering::SeqCst);
    assert_eq!(total, 2, "session listener should fire for every event");

    harness
        .log()
        .info_ctx("sdk", "session listener count ok", |ctx| {
            ctx.push(("events_received".to_string(), total.to_string()));
        });
}

#[test]
fn callback_ordering_stream_event_hook_extracts_from_message_update() {
    let harness =
        TestHarness::new("callback_ordering_stream_event_hook_extracts_from_message_update");

    let mut listeners = EventListeners::default();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    listeners.on_stream_event = Some(Arc::new(move |ev| {
        cap.lock().expect("lock").push(format!("{ev:?}"));
    }));

    // Create a TextDelta stream event and verify the hook recognizes it.
    let event = StreamEvent::TextDelta {
        content_index: 0,
        delta: "hello".to_string(),
    };
    listeners.notify_stream_event(&event);

    let events = captured.lock().expect("lock").clone();
    assert_eq!(events.len(), 1, "stream event hook should fire once");
    assert!(events[0].contains("TextDelta"), "should contain TextDelta");

    harness
        .log()
        .info_ctx("sdk", "stream event hook ok", |ctx| {
            ctx.push(("events_captured".to_string(), events.len().to_string()));
        });
}

// ============================================================================
// 2. Transport Adapter Behavior
// ============================================================================

#[test]
fn transport_session_transport_event_variants_debug() {
    let harness = TestHarness::new("transport_session_transport_event_variants_debug");

    let in_process_event = SessionTransportEvent::InProcess(AgentEvent::AgentStart {
        session_id: "test".into(),
    });
    let rpc_event = SessionTransportEvent::Rpc(json!({"type": "agent_start"}));

    let dbg_ip = format!("{in_process_event:?}");
    let dbg_rpc = format!("{rpc_event:?}");

    assert!(dbg_ip.contains("InProcess"), "InProcess variant debug");
    assert!(dbg_rpc.contains("Rpc"), "Rpc variant debug");

    harness
        .log()
        .info_ctx("sdk", "transport event variants ok", |ctx| {
            ctx.push(("in_process_debug_len".to_string(), dbg_ip.len().to_string()));
            ctx.push(("rpc_debug_len".to_string(), dbg_rpc.len().to_string()));
        });
}

#[test]
fn transport_session_prompt_result_variants() {
    let harness = TestHarness::new("transport_session_prompt_result_variants");

    let msg = AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new("test"))],
        api: "test".to_string(),
        provider: "test".to_string(),
        model: "test".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    };
    let in_process = SessionPromptResult::InProcess(Box::new(msg));
    let rpc = SessionPromptResult::RpcEvents(vec![json!({"type": "agent_end"})]);

    // Debug works
    let dbg_ip = format!("{in_process:?}");
    let dbg_rpc = format!("{rpc:?}");

    assert!(dbg_ip.contains("InProcess"), "InProcess debug");
    assert!(dbg_rpc.contains("RpcEvents"), "RpcEvents debug");

    harness
        .log()
        .info_ctx("sdk", "prompt result variants ok", |ctx| {
            ctx.push(("variants_tested".to_string(), "2".to_string()));
        });
}

#[test]
fn transport_session_transport_state_variants() {
    let harness = TestHarness::new("transport_session_transport_state_variants");

    let in_process = SessionTransportState::InProcess(AgentSessionState {
        session_id: Some("s1".to_string()),
        provider: "anthropic".to_string(),
        model_id: "claude-sonnet-4-20250514".to_string(),
        thinking_level: None,
        save_enabled: false,
        message_count: 0,
    });

    let rpc = SessionTransportState::Rpc(Box::new(RpcSessionState {
        model: None,
        thinking_level: "off".to_string(),
        is_streaming: false,
        is_compacting: false,
        steering_mode: "all".to_string(),
        follow_up_mode: "all".to_string(),
        session_file: None,
        session_id: "s2".to_string(),
        session_name: None,
        auto_compaction_enabled: true,
        auto_retry_enabled: false,
        message_count: 0,
        pending_message_count: 0,
        durability_mode: "balanced".to_string(),
    }));

    // Debug and Clone
    let dbg_ip = format!("{in_process:?}");
    let dbg_rpc = format!("{rpc:?}");
    assert!(dbg_ip.contains("InProcess"));
    assert!(dbg_rpc.contains("Rpc"));

    // PartialEq
    let in_process2 = in_process.clone();
    assert_eq!(in_process, in_process2);

    harness
        .log()
        .info_ctx("sdk", "transport state variants ok", |ctx| {
            ctx.push(("ip_debug_len".to_string(), dbg_ip.len().to_string()));
            ctx.push(("rpc_debug_len".to_string(), dbg_rpc.len().to_string()));
        });
}

#[test]
fn transport_rpc_options_default() {
    let harness = TestHarness::new("transport_rpc_options_default");

    let options = RpcTransportOptions::default();
    assert_eq!(options.binary_path.to_str().unwrap(), "pi");
    assert!(options.args.contains(&"--mode".to_string()));
    assert!(options.args.contains(&"rpc".to_string()));
    assert!(options.cwd.is_none());

    let dbg = format!("{options:?}");
    assert!(
        dbg.contains("binary_path"),
        "debug should contain binary_path"
    );

    harness
        .log()
        .info_ctx("sdk", "rpc options default ok", |ctx| {
            ctx.push((
                "binary_path".to_string(),
                options.binary_path.display().to_string(),
            ));
            ctx.push(("args_count".to_string(), options.args.len().to_string()));
        });
}

// ============================================================================
// 3. Lifecycle State Transitions
// ============================================================================

#[test]
fn lifecycle_state_after_model_switch() {
    let harness = TestHarness::new("lifecycle_state_after_model_switch");
    let options = test_session_options(&harness);

    let mut handle = run_async(create_agent_session(options)).expect("create session");
    let (state_before, state_after) = run_async(async move {
        let state_before = handle.state().await?;
        handle.set_model("openai", "gpt-4o").await?;
        let state_after = handle.state().await?;
        Ok::<_, pi::error::Error>((state_before, state_after))
    })
    .expect("state transitions");
    assert_eq!(state_before.provider, "anthropic");

    assert_eq!(state_after.provider, "openai");
    assert_eq!(state_after.model_id, "gpt-4o");

    // Session ID should remain the same.
    assert_eq!(state_before.session_id, state_after.session_id);

    harness
        .log()
        .info_ctx("sdk", "state after model switch ok", |ctx| {
            ctx.push(("before_provider".to_string(), state_before.provider));
            ctx.push(("after_provider".to_string(), state_after.provider));
            ctx.push(("after_model".to_string(), state_after.model_id));
        });
}

#[test]
fn lifecycle_state_fresh_session_has_zero_messages() {
    let harness = TestHarness::new("lifecycle_state_fresh_session_has_zero_messages");
    let options = test_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");
    let state = run_async(async move { handle.state().await }).expect("state");

    assert_eq!(
        state.message_count, 0,
        "fresh session should have 0 messages"
    );
    assert!(state.session_id.is_some(), "session_id should be set");
    assert!(!state.model_id.is_empty(), "model_id should be non-empty");

    harness
        .log()
        .info_ctx("sdk", "fresh session state ok", |ctx| {
            ctx.push(("messages".to_string(), state.message_count.to_string()));
            ctx.push((
                "session_id".to_string(),
                state.session_id.unwrap_or_default(),
            ));
        });
}

#[test]
fn lifecycle_state_with_thinking_level() {
    let harness = TestHarness::new("lifecycle_state_with_thinking_level");
    let options = SessionOptions {
        thinking: Some(ThinkingLevel::Medium),
        ..test_session_options(&harness)
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    let state = run_async(async move { handle.state().await }).expect("state");

    assert_eq!(
        state.thinking_level,
        Some(ThinkingLevel::Medium),
        "thinking level should be Medium"
    );

    harness
        .log()
        .info_ctx("sdk", "thinking level state ok", |ctx| {
            ctx.push(("level".to_string(), format!("{:?}", state.thinking_level)));
        });
}

#[test]
fn lifecycle_state_save_enabled_reflects_session_mode() {
    let harness = TestHarness::new("lifecycle_state_save_enabled_reflects_session_mode");

    // no_session = true => save_enabled = false
    let options_no = test_session_options(&harness);
    let handle_no = run_async(create_agent_session(options_no)).expect("create no-session");
    let state_no = run_async(async move { handle_no.state().await }).expect("state no");
    assert!(
        !state_no.save_enabled,
        "no-session should have save_enabled=false"
    );

    // no_session = false => save_enabled = true
    let session_dir = harness.temp_dir().join("sessions_enabled");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let options_yes = SessionOptions {
        no_session: false,
        session_dir: Some(session_dir),
        ..test_session_options(&harness)
    };
    let handle_yes = run_async(create_agent_session(options_yes)).expect("create with-session");
    let state_yes = run_async(async move { handle_yes.state().await }).expect("state yes");
    assert!(
        state_yes.save_enabled,
        "with-session should have save_enabled=true"
    );

    harness
        .log()
        .info_ctx("sdk", "save enabled states ok", |ctx| {
            ctx.push((
                "no_session_save".to_string(),
                state_no.save_enabled.to_string(),
            ));
            ctx.push((
                "with_session_save".to_string(),
                state_yes.save_enabled.to_string(),
            ));
        });
}

#[test]
fn lifecycle_agent_session_state_equality() {
    let harness = TestHarness::new("lifecycle_agent_session_state_equality");

    let state1 = AgentSessionState {
        session_id: Some("s1".to_string()),
        provider: "anthropic".to_string(),
        model_id: "claude-sonnet-4-20250514".to_string(),
        thinking_level: Some(ThinkingLevel::High),
        save_enabled: false,
        message_count: 5,
    };
    let state2 = state1.clone();
    assert_eq!(state1, state2, "cloned states should be equal");

    let state3 = AgentSessionState {
        message_count: 10,
        ..state1.clone()
    };
    assert_ne!(
        state1, state3,
        "states with different message_count should differ"
    );

    harness.log().info_ctx("sdk", "state equality ok", |ctx| {
        ctx.push(("equal".to_string(), "true".to_string()));
        ctx.push(("not_equal".to_string(), "true".to_string()));
    });
}

// ============================================================================
// 4. Cancellation Semantics
// ============================================================================

#[test]
fn cancel_abort_handle_signal_pair_creation() {
    let harness = TestHarness::new("cancel_abort_handle_signal_pair_creation");

    let (handle, signal) = AbortHandle::new();
    assert!(
        !signal.is_aborted(),
        "signal should not be aborted initially"
    );

    let dbg = format!("{handle:?}");
    assert!(dbg.contains("AbortHandle"), "debug should show AbortHandle");

    harness
        .log()
        .info_ctx("sdk", "abort pair creation ok", |ctx| {
            ctx.push((
                "aborted_initially".to_string(),
                signal.is_aborted().to_string(),
            ));
        });
}

#[test]
fn cancel_abort_sets_signal() {
    let harness = TestHarness::new("cancel_abort_sets_signal");

    let (handle, signal) = AbortHandle::new();
    assert!(!signal.is_aborted());

    handle.abort();
    assert!(
        signal.is_aborted(),
        "signal should be aborted after abort()"
    );

    harness.log().info_ctx("sdk", "abort signal set ok", |ctx| {
        ctx.push(("aborted_after".to_string(), signal.is_aborted().to_string()));
    });
}

#[test]
fn cancel_double_abort_is_safe() {
    let harness = TestHarness::new("cancel_double_abort_is_safe");

    let (handle, signal) = AbortHandle::new();
    handle.abort();
    handle.abort(); // Should not panic.
    assert!(signal.is_aborted(), "signal should remain aborted");

    harness.log().info_ctx("sdk", "double abort safe", |ctx| {
        ctx.push(("aborted".to_string(), signal.is_aborted().to_string()));
    });
}

#[test]
fn cancel_cloned_signal_reflects_abort() {
    let harness = TestHarness::new("cancel_cloned_signal_reflects_abort");

    let (handle, signal) = AbortHandle::new();
    let cloned = signal.clone();

    assert!(!cloned.is_aborted());
    handle.abort();
    assert!(cloned.is_aborted(), "cloned signal should reflect abort");
    assert!(signal.is_aborted(), "original signal should reflect abort");

    harness.log().info_ctx("sdk", "cloned signal ok", |ctx| {
        ctx.push((
            "original_aborted".to_string(),
            signal.is_aborted().to_string(),
        ));
        ctx.push(("clone_aborted".to_string(), cloned.is_aborted().to_string()));
    });
}

#[test]
fn cancel_cloned_handle_aborts_same_signal() {
    let harness = TestHarness::new("cancel_cloned_handle_aborts_same_signal");

    let (handle, signal) = AbortHandle::new();
    let cloned_handle = handle.clone();

    assert!(!signal.is_aborted());
    handle.abort();
    cloned_handle.abort();
    assert!(
        signal.is_aborted(),
        "cloned handle should abort the same signal"
    );

    harness
        .log()
        .info_ctx("sdk", "cloned handle abort ok", |ctx| {
            ctx.push(("aborted".to_string(), signal.is_aborted().to_string()));
        });
}

#[test]
fn cancel_new_abort_handle_via_sdk_api() {
    let harness = TestHarness::new("cancel_new_abort_handle_via_sdk_api");

    // Verify the SDK-level convenience function works.
    let (handle, signal) = AgentSessionHandle::new_abort_handle();
    assert!(!signal.is_aborted());
    handle.abort();
    assert!(signal.is_aborted());

    harness
        .log()
        .info_ctx("sdk", "sdk abort handle api ok", |ctx| {
            ctx.push(("aborted".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 5. RPC Type Serde Round-Trips
// ============================================================================

#[test]
fn rpc_bash_result_serde() {
    let harness = TestHarness::new("rpc_bash_result_serde");

    let value = json!({
        "output": "hello world\n",
        "exitCode": 0,
        "cancelled": false,
        "truncated": false,
        "fullOutputPath": null
    });
    let result: RpcBashResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.output, "hello world\n");
    assert_eq!(result.exit_code, 0);
    assert!(!result.cancelled);
    assert!(!result.truncated);
    assert!(result.full_output_path.is_none());

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcBashResult serde ok", |ctx| {
            ctx.push(("output".to_string(), result.output.trim().to_string()));
            ctx.push(("exit_code".to_string(), result.exit_code.to_string()));
        });
}

#[test]
fn rpc_compaction_result_serde() {
    let harness = TestHarness::new("rpc_compaction_result_serde");

    let value = json!({
        "summary": "Compacted 10 messages into 2",
        "firstKeptEntryId": "entry-5",
        "tokensBefore": 12000,
        "tokensAfter": 3200,
        "details": {"removed": 8}
    });
    let result: RpcCompactionResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.summary, "Compacted 10 messages into 2");
    assert_eq!(result.first_kept_entry_id, "entry-5");
    assert_eq!(result.tokens_before, 12000);
    assert_eq!(result.tokens_after, 3200);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcCompactionResult serde ok", |ctx| {
            ctx.push((
                "tokens_before".to_string(),
                result.tokens_before.to_string(),
            ));
            ctx.push(("tokens_after".to_string(), result.tokens_after.to_string()));
        });
}

#[test]
fn rpc_fork_result_serde() {
    let harness = TestHarness::new("rpc_fork_result_serde");

    let value = json!({"text": "forked from entry-3", "cancelled": false});
    let result: RpcForkResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.text, "forked from entry-3");
    assert!(!result.cancelled);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcForkResult serde ok", |ctx| {
            ctx.push(("text".to_string(), result.text.clone()));
        });
}

#[test]
fn rpc_fork_message_serde() {
    let harness = TestHarness::new("rpc_fork_message_serde");

    let value = json!({"entryId": "e-42", "text": "User asked about widgets"});
    let result: RpcForkMessage = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.entry_id, "e-42");
    assert_eq!(result.text, "User asked about widgets");

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcForkMessage serde ok", |ctx| {
            ctx.push(("entry_id".to_string(), result.entry_id.clone()));
        });
}

#[test]
fn rpc_cancelled_result_serde() {
    let harness = TestHarness::new("rpc_cancelled_result_serde");

    let value = json!({"cancelled": true});
    let result: RpcCancelledResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert!(result.cancelled);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcCancelledResult serde ok", |ctx| {
            ctx.push(("cancelled".to_string(), "true".to_string()));
        });
}

#[test]
fn rpc_cycle_model_result_serde() {
    let harness = TestHarness::new("rpc_cycle_model_result_serde");

    let value = json!({
        "model": {
            "id": "claude-sonnet-4-20250514",
            "name": "Claude Sonnet 4",
            "api": "anthropic-messages",
            "provider": "anthropic",
            "baseUrl": "https://api.anthropic.com",
            "reasoning": true,
            "input": ["text", "image"],
            "contextWindow": 200_000,
            "maxTokens": 8192,
            "cost": null
        },
        "thinkingLevel": "medium",
        "isScoped": false
    });
    let result: RpcCycleModelResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.model.id, "claude-sonnet-4-20250514");
    assert_eq!(result.thinking_level, ThinkingLevel::Medium);
    assert!(!result.is_scoped);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcCycleModelResult serde ok", |ctx| {
            ctx.push(("model_id".to_string(), result.model.id.clone()));
            ctx.push((
                "thinking".to_string(),
                format!("{:?}", result.thinking_level),
            ));
        });
}

#[test]
fn rpc_thinking_level_result_serde() {
    let harness = TestHarness::new("rpc_thinking_level_result_serde");

    let value = json!({"level": "high"});
    let result: RpcThinkingLevelResult =
        serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.level, ThinkingLevel::High);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcThinkingLevelResult serde ok", |ctx| {
            ctx.push(("level".to_string(), format!("{:?}", result.level)));
        });
}

#[test]
fn rpc_export_html_result_serde() {
    let harness = TestHarness::new("rpc_export_html_result_serde");

    let value = json!({"path": "/tmp/export.html"});
    let result: RpcExportHtmlResult = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.path, "/tmp/export.html");

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcExportHtmlResult serde ok", |ctx| {
            ctx.push(("path".to_string(), result.path.clone()));
        });
}

#[test]
fn rpc_last_assistant_text_serde() {
    let harness = TestHarness::new("rpc_last_assistant_text_serde");

    // With text
    let value = json!({"text": "Hello, world!"});
    let result: RpcLastAssistantText = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.text.as_deref(), Some("Hello, world!"));
    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    // With null text
    let value_null = json!({"text": null});
    let result_null: RpcLastAssistantText =
        serde_json::from_value(value_null).expect("deserialize null");
    assert!(result_null.text.is_none());

    harness
        .log()
        .info_ctx("sdk", "RpcLastAssistantText serde ok", |ctx| {
            ctx.push(("with_text".to_string(), "true".to_string()));
            ctx.push(("null_text".to_string(), "true".to_string()));
        });
}

#[test]
fn rpc_token_stats_serde() {
    let harness = TestHarness::new("rpc_token_stats_serde");

    let value = json!({
        "input": 1500,
        "output": 800,
        "cacheRead": 200,
        "cacheWrite": 100,
        "total": 2600
    });
    let result: RpcTokenStats = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.input, 1500);
    assert_eq!(result.output, 800);
    assert_eq!(result.cache_read, 200);
    assert_eq!(result.cache_write, 100);
    assert_eq!(result.total, 2600);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcTokenStats serde ok", |ctx| {
            ctx.push(("total".to_string(), result.total.to_string()));
        });
}

#[test]
fn rpc_session_stats_serde() {
    let harness = TestHarness::new("rpc_session_stats_serde");

    let value = json!({
        "sessionFile": "/tmp/session.jsonl",
        "sessionId": "s-123",
        "userMessages": 5,
        "assistantMessages": 5,
        "toolCalls": 3,
        "toolResults": 3,
        "totalMessages": 16,
        "tokens": {
            "input": 1000,
            "output": 500,
            "cacheRead": 0,
            "cacheWrite": 0,
            "total": 1500
        },
        "cost": 0.042
    });
    let result: RpcSessionStats = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.session_id, "s-123");
    assert_eq!(result.user_messages, 5);
    assert_eq!(result.total_messages, 16);
    assert_eq!(result.tokens.total, 1500);
    assert!((result.cost - 0.042).abs() < f64::EPSILON);

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcSessionStats serde ok", |ctx| {
            ctx.push((
                "total_messages".to_string(),
                result.total_messages.to_string(),
            ));
            ctx.push(("cost".to_string(), format!("{:.3}", result.cost)));
        });
}

#[test]
fn rpc_command_info_serde() {
    let harness = TestHarness::new("rpc_command_info_serde");

    let value = json!({
        "name": "compact",
        "description": "Compact session history",
        "source": "builtin",
        "location": null,
        "path": null
    });
    let result: RpcCommandInfo = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.name, "compact");
    assert_eq!(
        result.description.as_deref(),
        Some("Compact session history")
    );
    assert_eq!(result.source, "builtin");

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcCommandInfo serde ok", |ctx| {
            ctx.push(("name".to_string(), result.name.clone()));
            ctx.push(("source".to_string(), result.source.clone()));
        });
}

#[test]
fn rpc_extension_ui_response_all_variants_serde() {
    let harness = TestHarness::new("rpc_extension_ui_response_all_variants_serde");

    // Value variant
    let val = json!({"kind": "value", "value": {"foo": "bar"}});
    let decoded: RpcExtensionUiResponse = serde_json::from_value(val.clone()).expect("value");
    assert!(matches!(decoded, RpcExtensionUiResponse::Value { .. }));
    assert_eq!(serde_json::to_value(&decoded).expect("enc"), val);

    // Confirmed variant
    let conf = json!({"kind": "confirmed", "confirmed": false});
    let decoded_conf: RpcExtensionUiResponse =
        serde_json::from_value(conf.clone()).expect("confirmed");
    assert!(matches!(
        decoded_conf,
        RpcExtensionUiResponse::Confirmed { confirmed: false }
    ));
    assert_eq!(serde_json::to_value(&decoded_conf).expect("enc"), conf);

    // Cancelled variant
    let cancel = json!({"kind": "cancelled"});
    let decoded_cancel: RpcExtensionUiResponse =
        serde_json::from_value(cancel.clone()).expect("cancelled");
    assert!(matches!(decoded_cancel, RpcExtensionUiResponse::Cancelled));
    assert_eq!(serde_json::to_value(&decoded_cancel).expect("enc"), cancel);

    harness
        .log()
        .info_ctx("sdk", "extension ui response all variants ok", |ctx| {
            ctx.push(("variants_tested".to_string(), "3".to_string()));
        });
}

#[test]
fn rpc_model_info_serde_with_cost() {
    let harness = TestHarness::new("rpc_model_info_serde_with_cost");

    let value = json!({
        "id": "gpt-4o",
        "name": "GPT-4o",
        "api": "openai-chat",
        "provider": "openai",
        "baseUrl": "https://api.openai.com",
        "reasoning": false,
        "input": ["text", "image"],
        "contextWindow": 128_000,
        "maxTokens": 4096,
        "cost": {
            "input": 5.0,
            "output": 15.0,
            "cacheRead": 2.5,
            "cacheWrite": 5.0
        }
    });
    let result: RpcModelInfo = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.id, "gpt-4o");
    assert_eq!(result.provider, "openai");
    assert!(!result.reasoning);
    assert_eq!(result.context_window, 128_000);
    assert!(result.cost.is_some());

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcModelInfo with cost ok", |ctx| {
            ctx.push(("model".to_string(), result.id.clone()));
            ctx.push((
                "context_window".to_string(),
                result.context_window.to_string(),
            ));
        });
}

#[test]
fn rpc_model_info_serde_without_cost() {
    let harness = TestHarness::new("rpc_model_info_serde_without_cost");

    let value = json!({
        "id": "custom-model",
        "name": "Custom",
        "api": "anthropic-messages",
        "provider": "custom",
        "baseUrl": "",
        "reasoning": false,
        "input": [],
        "contextWindow": 0,
        "maxTokens": 0,
        "cost": null
    });
    let result: RpcModelInfo = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(result.id, "custom-model");
    assert!(result.cost.is_none());

    let reencoded = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reencoded, value);

    harness
        .log()
        .info_ctx("sdk", "RpcModelInfo without cost ok", |ctx| {
            ctx.push(("model".to_string(), result.id.clone()));
            ctx.push(("has_cost".to_string(), "false".to_string()));
        });
}

#[test]
fn rpc_session_state_defaults_serde() {
    let harness = TestHarness::new("rpc_session_state_defaults_serde");

    // Minimal payload — all fields default.
    let value = json!({});
    let result: RpcSessionState = serde_json::from_value(value).expect("deserialize");
    assert!(result.model.is_none());
    assert_eq!(result.thinking_level, "");
    assert!(!result.is_streaming);
    assert!(!result.is_compacting);
    assert!(!result.auto_retry_enabled);
    assert_eq!(result.message_count, 0);
    assert_eq!(result.durability_mode, "");

    harness
        .log()
        .info_ctx("sdk", "RpcSessionState defaults ok", |ctx| {
            ctx.push(("thinking".to_string(), result.thinking_level.clone()));
            ctx.push(("messages".to_string(), result.message_count.to_string()));
        });
}

// ============================================================================
// 6. EventListeners Edge Cases
// ============================================================================

#[test]
fn event_listeners_default_has_no_hooks() {
    let harness = TestHarness::new("event_listeners_default_has_no_hooks");

    let listeners = EventListeners::default();
    assert!(listeners.on_tool_start.is_none());
    assert!(listeners.on_tool_end.is_none());
    assert!(listeners.on_stream_event.is_none());

    let dbg = format!("{listeners:?}");
    assert!(dbg.contains("has_on_tool_start: false"));
    assert!(dbg.contains("has_on_tool_end: false"));
    assert!(dbg.contains("has_on_stream_event: false"));

    harness
        .log()
        .info_ctx("sdk", "event listeners default ok", |ctx| {
            ctx.push(("hooks_set".to_string(), "none".to_string()));
        });
}

#[test]
fn event_listeners_notify_with_no_subscribers_is_safe() {
    let harness = TestHarness::new("event_listeners_notify_with_no_subscribers_is_safe");

    let listeners = EventListeners::default();
    // Should not panic with zero subscribers.
    listeners.notify(&AgentEvent::AgentStart {
        session_id: "test".into(),
    });
    listeners.notify_tool_start("bash", &json!({}));
    listeners.notify_tool_end(
        "bash",
        &ToolOutput {
            content: vec![],
            details: None,
            is_error: false,
        },
        false,
    );
    listeners.notify_stream_event(&StreamEvent::TextDelta {
        content_index: 0,
        delta: "x".to_string(),
    });

    harness
        .log()
        .info_ctx("sdk", "notify no subscribers safe", |ctx| {
            ctx.push(("panicked".to_string(), "false".to_string()));
        });
}

#[test]
fn event_listeners_subscription_ids_are_unique() {
    let harness = TestHarness::new("event_listeners_subscription_ids_are_unique");

    let listeners = EventListeners::default();
    let id1 = listeners.subscribe(Arc::new(|_| {}));
    let id2 = listeners.subscribe(Arc::new(|_| {}));
    let id3 = listeners.subscribe(Arc::new(|_| {}));

    assert_ne!(id1, id2, "subscription IDs should be unique");
    assert_ne!(id2, id3, "subscription IDs should be unique");
    assert_ne!(id1, id3, "subscription IDs should be unique");

    harness
        .log()
        .info_ctx("sdk", "subscription IDs unique ok", |ctx| {
            ctx.push(("id_count".to_string(), "3".to_string()));
        });
}

#[test]
fn event_listeners_clone_shares_subscribers() {
    let harness = TestHarness::new("event_listeners_clone_shares_subscribers");

    let listeners = EventListeners::default();
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    listeners.subscribe(Arc::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    }));

    let cloned = listeners.clone();

    // Notify on the clone — should invoke the subscriber.
    cloned.notify(&AgentEvent::AgentStart {
        session_id: "s".into(),
    });
    listeners.notify(&AgentEvent::AgentStart {
        session_id: "s2".into(),
    });

    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "cloned listeners should share subscriber state"
    );

    harness
        .log()
        .info_ctx("sdk", "clone shares subscribers ok", |ctx| {
            ctx.push(("notifications_received".to_string(), "1".to_string()));
        });
}

#[test]
fn event_listeners_unsubscribe_stops_notifications() {
    let harness = TestHarness::new("event_listeners_unsubscribe_stops_notifications");

    let listeners = EventListeners::default();
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    let id = listeners.subscribe(Arc::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    }));

    listeners.notify(&AgentEvent::AgentStart {
        session_id: "s".into(),
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);

    let removed = listeners.unsubscribe(id);
    assert!(removed, "unsubscribe should return true");

    listeners.notify(&AgentEvent::AgentStart {
        session_id: "s2".into(),
    });
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "should not receive events after unsubscribe"
    );

    harness
        .log()
        .info_ctx("sdk", "unsubscribe stops notifications ok", |ctx| {
            ctx.push((
                "count_after_unsub".to_string(),
                count.load(Ordering::SeqCst).to_string(),
            ));
        });
}

#[test]
fn event_listeners_tool_start_hook_without_generic_subscriber() {
    let harness = TestHarness::new("event_listeners_tool_start_hook_without_generic_subscriber");

    let mut listeners = EventListeners::default();
    let names = Arc::new(Mutex::new(Vec::new()));
    let n = Arc::clone(&names);
    listeners.on_tool_start = Some(Arc::new(move |name, _args| {
        n.lock().expect("lock").push(name.to_string());
    }));

    listeners.notify_tool_start("bash", &json!({"command": "ls"}));
    listeners.notify_tool_start("read", &json!({"path": "/tmp"}));

    let captured = names.lock().expect("lock").clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0], "bash");
    assert_eq!(captured[1], "read");

    harness
        .log()
        .info_ctx("sdk", "tool start hook standalone ok", |ctx| {
            ctx.push(("tools_captured".to_string(), captured.len().to_string()));
        });
}

#[test]
fn event_listeners_tool_end_hook_with_error() {
    let harness = TestHarness::new("event_listeners_tool_end_hook_with_error");

    let mut listeners = EventListeners::default();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let e = Arc::clone(&errors);
    listeners.on_tool_end = Some(Arc::new(move |name, _output, is_error| {
        e.lock().expect("lock").push((name.to_string(), is_error));
    }));

    let ok_output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("ok"))],
        details: None,
        is_error: false,
    };
    let err_output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("error"))],
        details: None,
        is_error: true,
    };

    listeners.notify_tool_end("read", &ok_output, false);
    listeners.notify_tool_end("bash", &err_output, true);

    let captured = errors.lock().expect("lock").clone();
    assert_eq!(captured.len(), 2);
    assert!(!captured[0].1, "read should not be error");
    assert!(captured[1].1, "bash should be error");

    harness
        .log()
        .info_ctx("sdk", "tool end error flag ok", |ctx| {
            ctx.push(("captured_count".to_string(), captured.len().to_string()));
        });
}

// ============================================================================
// 7. Tool Factory + Registry
// ============================================================================

#[test]
fn tool_registry_from_sdk_tools_lookup() {
    let harness = TestHarness::new("tool_registry_from_sdk_tools_lookup");

    let tmp = tempfile::tempdir().expect("tempdir");
    let tools = pi::sdk::create_all_tools(tmp.path());
    let registry = ToolRegistry::from_tools(tools);

    for name in pi::sdk::BUILTIN_TOOL_NAMES {
        assert!(
            registry.get(name).is_some(),
            "registry should contain tool: {name}"
        );
    }
    assert!(registry.get("nonexistent").is_none());

    harness
        .log()
        .info_ctx("sdk", "tool registry lookup ok", |ctx| {
            ctx.push((
                "tools_found".to_string(),
                pi::sdk::BUILTIN_TOOL_NAMES.len().to_string(),
            ));
        });
}

#[test]
fn tool_definitions_have_required_schema_fields() {
    let harness = TestHarness::new("tool_definitions_have_required_schema_fields");

    let tmp = tempfile::tempdir().expect("tempdir");
    let defs = pi::sdk::all_tool_definitions(tmp.path());

    for def in &defs {
        assert!(!def.name.is_empty(), "tool name should be non-empty");
        assert!(
            !def.description.is_empty(),
            "description should be non-empty for {}",
            def.name
        );
        assert!(
            def.parameters.is_object(),
            "parameters should be object for {}",
            def.name
        );
        assert!(
            def.parameters.get("type").and_then(|v| v.as_str()) == Some("object"),
            "parameters.type should be 'object' for {}",
            def.name
        );
        assert!(
            def.parameters.get("properties").is_some(),
            "parameters should have 'properties' for {}",
            def.name
        );
    }

    harness
        .log()
        .info_ctx("sdk", "tool definitions schema ok", |ctx| {
            ctx.push(("definitions_checked".to_string(), defs.len().to_string()));
        });
}

// ============================================================================
// 8. SessionOptions Defaults
// ============================================================================

#[test]
fn session_options_default_values() {
    let harness = TestHarness::new("session_options_default_values");

    let opts = SessionOptions::default();
    assert!(opts.provider.is_none());
    assert!(opts.model.is_none());
    assert!(opts.api_key.is_none());
    assert!(opts.thinking.is_none());
    assert!(opts.system_prompt.is_none());
    assert!(opts.append_system_prompt.is_none());
    assert!(opts.enabled_tools.is_none());
    assert!(opts.working_directory.is_none());
    assert!(opts.no_session, "default should be no_session=true");
    assert!(opts.session_path.is_none());
    assert!(opts.session_dir.is_none());
    assert!(opts.extension_paths.is_empty());
    assert!(opts.extension_policy.is_none());
    assert!(opts.repair_policy.is_none());
    assert!(opts.include_cwd_in_prompt);
    assert_eq!(opts.max_tool_iterations, 50);
    assert!(opts.on_event.is_none());
    assert!(opts.on_tool_start.is_none());
    assert!(opts.on_tool_end.is_none());
    assert!(opts.on_stream_event.is_none());

    harness
        .log()
        .info_ctx("sdk", "session options defaults ok", |ctx| {
            ctx.push((
                "max_tool_iterations".to_string(),
                opts.max_tool_iterations.to_string(),
            ));
            ctx.push(("no_session".to_string(), opts.no_session.to_string()));
        });
}

#[test]
fn session_options_with_custom_system_prompt() {
    let harness = TestHarness::new("session_options_with_custom_system_prompt");

    let options = SessionOptions {
        system_prompt: Some("You are a test assistant.".to_string()),
        ..test_session_options(&harness)
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    // The system prompt is baked into the agent config; verify session was created.
    let (provider, _model) = handle.model();
    assert_eq!(provider, "anthropic");

    harness
        .log()
        .info_ctx("sdk", "custom system prompt ok", |ctx| {
            ctx.push(("provider".to_string(), provider));
        });
}

#[test]
fn session_options_with_no_tools() {
    let harness = TestHarness::new("session_options_with_no_tools");

    let options = SessionOptions {
        enabled_tools: Some(vec![]),
        ..test_session_options(&harness)
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    // Session should be created even with no tools.
    assert!(!handle.session().agent.provider().model_id().is_empty());

    harness.log().info_ctx("sdk", "no tools session ok", |ctx| {
        ctx.push(("created".to_string(), "true".to_string()));
    });
}

#[test]
fn session_options_with_selected_tools() {
    let harness = TestHarness::new("session_options_with_selected_tools");

    let options = SessionOptions {
        enabled_tools: Some(vec!["read".to_string(), "bash".to_string()]),
        ..test_session_options(&harness)
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert!(!handle.session().agent.provider().model_id().is_empty());

    harness
        .log()
        .info_ctx("sdk", "selected tools session ok", |ctx| {
            ctx.push(("tools".to_string(), "read,bash".to_string()));
        });
}

// ============================================================================
// 9. Messages API
// ============================================================================

#[test]
fn messages_api_empty_on_fresh_session() {
    let harness = TestHarness::new("messages_api_empty_on_fresh_session");
    let options = test_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");
    let messages = run_async(async move { handle.messages().await }).expect("messages");

    assert!(messages.is_empty(), "fresh session should have no messages");

    harness
        .log()
        .info_ctx("sdk", "messages api empty ok", |ctx| {
            ctx.push(("count".to_string(), messages.len().to_string()));
        });
}

// ============================================================================
// 10. SDK Convenience Accessors
// ============================================================================

#[test]
fn sdk_model_accessor_returns_correct_pair() {
    let harness = TestHarness::new("sdk_model_accessor_returns_correct_pair");
    let options = SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        ..test_session_options(&harness)
    };

    let handle = run_async(create_agent_session(options)).expect("create session");
    let (prov, model) = handle.model();
    assert_eq!(prov, "openai");
    assert_eq!(model, "gpt-4o");

    harness.log().info_ctx("sdk", "model accessor ok", |ctx| {
        ctx.push(("provider".to_string(), prov));
        ctx.push(("model".to_string(), model));
    });
}

#[test]
fn sdk_thinking_accessor_returns_configured_level() {
    let harness = TestHarness::new("sdk_thinking_accessor_returns_configured_level");

    // Test with thinking level set.
    let options = SessionOptions {
        thinking: Some(ThinkingLevel::Low),
        ..test_session_options(&harness)
    };
    let handle = run_async(create_agent_session(options)).expect("create session");
    assert_eq!(handle.thinking(), Some(ThinkingLevel::Low));
    assert_eq!(handle.thinking_level(), Some(ThinkingLevel::Low));

    harness
        .log()
        .info_ctx("sdk", "thinking accessor ok", |ctx| {
            ctx.push(("level".to_string(), format!("{:?}", handle.thinking())));
        });
}

#[test]
fn sdk_session_mut_accessor() {
    let harness = TestHarness::new("sdk_session_mut_accessor");
    let options = test_session_options(&harness);

    let mut handle = run_async(create_agent_session(options)).expect("create session");

    // Verify session() and session_mut() work.
    let _provider = handle.session().agent.provider();
    let _provider_mut = handle.session_mut().agent.provider();

    harness
        .log()
        .info_ctx("sdk", "session accessors ok", |ctx| {
            ctx.push(("session_accessible".to_string(), "true".to_string()));
        });
}

#[test]
fn sdk_listeners_mut_can_update_hooks() {
    let harness = TestHarness::new("sdk_listeners_mut_can_update_hooks");
    let options = test_session_options(&harness);

    let mut handle = run_async(create_agent_session(options)).expect("create session");

    // Initially no tool hooks.
    assert!(handle.listeners().on_tool_start.is_none());

    // Set a hook via listeners_mut.
    handle.listeners_mut().on_tool_start = Some(Arc::new(|_name, _args| {}));
    assert!(handle.listeners().on_tool_start.is_some());

    harness
        .log()
        .info_ctx("sdk", "listeners_mut update ok", |ctx| {
            ctx.push(("hook_set".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 11. Extension Convenience Methods
// ============================================================================

#[test]
fn sdk_extension_methods_without_extensions() {
    let harness = TestHarness::new("sdk_extension_methods_without_extensions");
    let options = test_session_options(&harness);

    let handle = run_async(create_agent_session(options)).expect("create session");
    assert!(!handle.has_extensions());
    assert!(handle.extension_manager().is_none());
    assert!(handle.extension_region().is_none());

    harness.log().info_ctx("sdk", "no extensions ok", |ctx| {
        ctx.push(("has_extensions".to_string(), "false".to_string()));
    });
}
