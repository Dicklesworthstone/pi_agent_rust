//! bd-kh2.2: E2E RPC extension session connector scenarios.
//!
//! Tests the RPC protocol's session-management commands end-to-end:
//! `get_state`, `get_messages`, `set_session_name`, `set_thinking_level`,
//! `get_session_stats`, and `get_last_assistant_text`.
//!
//! These tests spin up a real RPC server over async channels (no network),
//! pre-populate sessions with deterministic data, and verify that each
//! command returns spec-compliant responses.

#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::type_complexity)]

mod common;

use common::TestHarness;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::model::{
    AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall, Usage, UserContent,
};
use pi::provider::Provider;
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, run};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Receive a single JSON line from the RPC output channel with timeout.
async fn recv_line(rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Result<String, String> {
    let start = Instant::now();
    loop {
        let recv_result = {
            let rx = rx.lock().expect("lock rpc output receiver");
            rx.try_recv()
        };

        match recv_result {
            Ok(line) => return Ok(line),
            Err(TryRecvError::Disconnected) => {
                return Err(format!("{label}: output channel disconnected"));
            }
            Err(TryRecvError::Empty) => {}
        }

        if start.elapsed() > Duration::from_secs(20) {
            return Err(format!("{label}: timed out waiting for output"));
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

/// Send a JSON command and receive the response.
async fn send_recv(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    cmd: &str,
    label: &str,
) -> Value {
    let cx = asupersync::Cx::for_testing();
    in_tx
        .send(&cx, cmd.to_string())
        .await
        .expect("send command");
    let line = recv_line(out_rx, label).await.expect(label);
    serde_json::from_str(line.trim()).expect("parse JSON response")
}

/// Create a minimal Agent (provider won't be called for session ops).
fn dummy_agent() -> Agent {
    let provider: Arc<dyn Provider> =
        Arc::new(OpenAIProvider::new("gpt-4o-mini".to_string()));
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    Agent::new(provider, tools, AgentConfig::default())
}

/// Build an `AgentSession` + `RpcOptions` and RPC channels.
/// Returns (`in_tx`, `out_rx`, `server_task`).
fn setup_rpc(
    session: Session,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> (
    asupersync::channel::mpsc::Sender<String>,
    Arc<Mutex<Receiver<String>>>,
    asupersync::runtime::JoinHandle<pi::error::Result<()>>,
) {
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    let agent_session = AgentSession::new(
        dummy_agent(),
        session,
        false,
        pi::compaction::ResolvedCompactionSettings::default(),
    );

    let auth_dir = tempfile::tempdir().unwrap();
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).unwrap();
    let options = RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models: Vec::new(),
        scoped_models: Vec::new(),
        auth,
        runtime_handle: runtime_handle.clone(),
    };

    let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
    let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
    let out_rx = Arc::new(Mutex::new(out_rx));

    let server = runtime_handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

    (in_tx, out_rx, server)
}

/// Create a session pre-populated with a user message and an assistant reply.
fn prepopulated_session() -> Session {
    let now = chrono::Utc::now().timestamp_millis();
    let mut session = Session::in_memory();
    session.header.provider = Some("openai".to_string());
    session.header.model_id = Some("gpt-4o-mini".to_string());
    session.header.thinking_level = Some("off".to_string());

    session.append_message(SessionMessage::User {
        content: UserContent::Text("Hello, world!".to_string()),
        timestamp: Some(now),
    });
    session.append_message(SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Hi there!"))],
            api: "test".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now,
        },
    });
    session
}

// ─── get_state tests ─────────────────────────────────────────────────────────

#[test]
fn rpc_get_state_fresh_session() {
    let harness = TestHarness::new("rpc_get_state_fresh");
    let logger = harness.log();

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(&in_tx, &out_rx, r#"{"id":"1","type":"get_state"}"#, "get_state").await;

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_state");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["id"], "1");

        let data = resp["data"].as_object().expect("data must be object");

        // Required fields per spec.
        let required_keys = [
            "sessionFile",
            "sessionId",
            "sessionName",
            "model",
            "messageCount",
            "isStreaming",
        ];
        for key in &required_keys {
            assert!(data.contains_key(*key), "get_state missing key: {key}");
        }

        logger.info_ctx("rpc", "get_state verified", |ctx| {
            ctx.push(("keys".into(), format!("{:?}", data.keys().collect::<Vec<_>>())));
        });

        // In-memory session has null sessionFile.
        assert!(resp["data"]["sessionFile"].is_null());

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "server error: {result:?}");
    });
}

#[test]
fn rpc_get_state_with_prepopulated_session() {
    let _harness = TestHarness::new("rpc_get_state_prepopulated");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(prepopulated_session(), &handle);

        let resp = send_recv(&in_tx, &out_rx, r#"{"id":"1","type":"get_state"}"#, "get_state").await;

        assert_eq!(resp["success"], true);
        // Prepopulated session has 2 messages.
        let count = resp["data"]["messageCount"].as_u64().unwrap_or(0);
        assert!(count >= 2, "expected at least 2 messages, got {count}");

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── set_session_name tests ──────────────────────────────────────────────────

#[test]
fn rpc_set_session_name_then_get_state() {
    let _harness = TestHarness::new("rpc_set_session_name");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // Set the name.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name","name":"My RPC Session"}"#,
            "set_session_name",
        )
        .await;
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "set_session_name");
        assert_eq!(resp["success"], true);

        // Verify via get_state.
        let state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_state"}"#,
            "get_state after name",
        )
        .await;
        assert_eq!(state["data"]["sessionName"], "My RPC Session");

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_session_name_missing_name_returns_error() {
    let _harness = TestHarness::new("rpc_set_session_name_error");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // Missing "name" field.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name"}"#,
            "set_session_name missing name",
        )
        .await;
        assert_eq!(resp["success"], false);
        assert!(resp["error"].as_str().is_some(), "should have error message");

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_session_name_overwrite() {
    let _harness = TestHarness::new("rpc_set_session_name_overwrite");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // First name.
        send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name","name":"First"}"#,
            "set name 1",
        )
        .await;

        // Second name overwrites.
        send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_session_name","name":"Second"}"#,
            "set name 2",
        )
        .await;

        let state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"get_state"}"#,
            "get_state after overwrite",
        )
        .await;
        assert_eq!(state["data"]["sessionName"], "Second");

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── set_thinking_level tests ────────────────────────────────────────────────

#[test]
fn rpc_set_thinking_level_no_model_succeeds() {
    let _harness = TestHarness::new("rpc_set_thinking_level");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level","level":"high"}"#,
            "set_thinking_level",
        )
        .await;
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "set_thinking_level");
        // May succeed or fail depending on model availability.
        // Without a model entry, level should still be set on agent options.
        // Note: set_thinking_level applies directly to agent stream options.
        assert_eq!(resp["success"], true);

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_thinking_level_missing_level_returns_error() {
    let _harness = TestHarness::new("rpc_set_thinking_level_error");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level"}"#,
            "set_thinking_level missing level",
        )
        .await;
        assert_eq!(resp["success"], false);

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_thinking_level_invalid_level_returns_error() {
    let _harness = TestHarness::new("rpc_set_thinking_level_invalid");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level","level":"super_mega_think"}"#,
            "set_thinking_level invalid",
        )
        .await;
        assert_eq!(resp["success"], false);

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── get_messages tests ──────────────────────────────────────────────────────

#[test]
fn rpc_get_messages_empty_session() {
    let _harness = TestHarness::new("rpc_get_messages_empty");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages empty",
        )
        .await;
        assert_eq!(resp["success"], true);
        let messages = resp["data"]["messages"].as_array().expect("messages array");
        assert!(messages.is_empty(), "empty session should have no messages");

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_get_messages_with_content() {
    let _harness = TestHarness::new("rpc_get_messages_content");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(prepopulated_session(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages with content",
        )
        .await;
        assert_eq!(resp["success"], true);
        let messages = resp["data"]["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "expected user + assistant messages");

        // First message should be user.
        assert_eq!(messages[0]["role"], "user");
        // Second should be assistant.
        assert_eq!(messages[1]["role"], "assistant");

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── get_session_stats tests ─────────────────────────────────────────────────

#[test]
fn rpc_get_session_stats_empty() {
    let _harness = TestHarness::new("rpc_get_session_stats_empty");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats empty",
        )
        .await;
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["userMessages"], 0);
        assert_eq!(resp["data"]["assistantMessages"], 0);
        assert_eq!(resp["data"]["totalMessages"], 0);
        assert_eq!(resp["data"]["toolCalls"], 0);
        assert_eq!(resp["data"]["toolResults"], 0);

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_get_session_stats_with_messages() {
    let _harness = TestHarness::new("rpc_get_session_stats_msgs");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(prepopulated_session(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats",
        )
        .await;
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["userMessages"], 1);
        assert_eq!(resp["data"]["assistantMessages"], 1);
        assert_eq!(resp["data"]["totalMessages"], 2);
        assert_eq!(resp["data"]["tokens"]["input"], 10);
        assert_eq!(resp["data"]["tokens"]["output"], 5);
        assert_eq!(resp["data"]["tokens"]["total"], 15);

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_get_session_stats_with_tool_calls() {
    let _harness = TestHarness::new("rpc_get_session_stats_tools");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = chrono::Utc::now().timestamp_millis();
        let mut session = Session::in_memory();
        session.header.provider = Some("openai".to_string());
        session.header.model_id = Some("gpt-4o-mini".to_string());
        session.header.thinking_level = Some("off".to_string());

        // User, assistant with tool call, tool result.
        session.append_message(SessionMessage::User {
            content: UserContent::Text("read the file".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "tc1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "test.txt" }),
                    thought_signature: None,
                })],
                api: "test".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                usage: Usage {
                    input: 20,
                    output: 10,
                    total_tokens: 30,
                    ..Usage::default()
                },
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: now,
            },
        });
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "tc1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
            is_error: false,
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("Here are the contents."))],
                api: "test".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                usage: Usage {
                    input: 15,
                    output: 8,
                    total_tokens: 23,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now,
            },
        });

        let (in_tx, out_rx, server) = setup_rpc(session, &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats tools",
        )
        .await;
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["userMessages"], 1);
        assert_eq!(resp["data"]["assistantMessages"], 2);
        assert_eq!(resp["data"]["toolCalls"], 1);
        assert_eq!(resp["data"]["toolResults"], 1);
        assert_eq!(resp["data"]["totalMessages"], 4);
        assert_eq!(resp["data"]["tokens"]["input"], 35);
        assert_eq!(resp["data"]["tokens"]["output"], 18);
        assert_eq!(resp["data"]["tokens"]["total"], 53);

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── get_last_assistant_text tests ───────────────────────────────────────────

#[test]
fn rpc_get_last_assistant_text_empty() {
    let _harness = TestHarness::new("rpc_last_text_empty");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text empty",
        )
        .await;
        assert_eq!(resp["success"], true);
        // No assistant messages → null or empty text.
        let text = &resp["data"]["text"];
        assert!(
            text.is_null() || text.as_str() == Some(""),
            "expected null or empty, got {text}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_get_last_assistant_text_with_content() {
    let _harness = TestHarness::new("rpc_last_text_content");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(prepopulated_session(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text",
        )
        .await;
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["text"], "Hi there!");

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── set_model error tests ───────────────────────────────────────────────────

#[test]
fn rpc_set_model_missing_provider_returns_error() {
    let _harness = TestHarness::new("rpc_set_model_no_provider");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","modelId":"gpt-4o"}"#,
            "set_model missing provider",
        )
        .await;
        assert_eq!(resp["success"], false);
        assert!(
            resp["error"]
                .as_str()
                .unwrap_or_default()
                .contains("provider"),
            "error should mention missing provider"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_model_missing_model_id_returns_error() {
    let _harness = TestHarness::new("rpc_set_model_no_model_id");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","provider":"openai"}"#,
            "set_model missing modelId",
        )
        .await;
        assert_eq!(resp["success"], false);

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_set_model_unknown_model_returns_error() {
    let _harness = TestHarness::new("rpc_set_model_unknown");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // No available_models configured → any model is "not found".
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","provider":"openai","modelId":"gpt-4o"}"#,
            "set_model unknown model",
        )
        .await;
        assert_eq!(resp["success"], false);
        assert!(
            resp["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not found"),
            "error should say model not found"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── Unknown command tests ───────────────────────────────────────────────────

#[test]
fn rpc_unknown_command_returns_error() {
    let _harness = TestHarness::new("rpc_unknown_command");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"nonexistent_command"}"#,
            "unknown command",
        )
        .await;
        assert_eq!(resp["success"], false);

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── Multi-command session lifecycle ─────────────────────────────────────────

#[test]
fn rpc_session_lifecycle_multi_command() {
    let harness = TestHarness::new("rpc_session_lifecycle");
    let logger = harness.log();

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(prepopulated_session(), &handle);

        // 1. Get initial state.
        let state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_state"}"#,
            "initial get_state",
        )
        .await;
        assert_eq!(state["success"], true);
        let initial_count = state["data"]["messageCount"].as_u64().unwrap_or(0);
        logger.info_ctx("rpc", "Initial state", |ctx| {
            ctx.push(("messageCount".into(), initial_count.to_string()));
        });

        // 2. Set session name.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_session_name","name":"Lifecycle Test"}"#,
            "set name",
        )
        .await;
        assert_eq!(resp["success"], true);

        // 3. Get messages.
        let msgs = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"get_messages"}"#,
            "get messages",
        )
        .await;
        assert_eq!(msgs["success"], true);
        let msg_array = msgs["data"]["messages"].as_array().expect("messages");
        assert_eq!(msg_array.len(), 2);
        logger.info_ctx("rpc", "Messages retrieved", |ctx| {
            ctx.push(("count".into(), msg_array.len().to_string()));
        });

        // 4. Get stats.
        let stats = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"4","type":"get_session_stats"}"#,
            "get stats",
        )
        .await;
        assert_eq!(stats["success"], true);
        assert_eq!(stats["data"]["totalMessages"], 2);

        // 5. Get last assistant text.
        let text = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"5","type":"get_last_assistant_text"}"#,
            "last text",
        )
        .await;
        assert_eq!(text["success"], true);
        assert_eq!(text["data"]["text"], "Hi there!");

        // 6. Verify name persisted in state.
        let final_state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"6","type":"get_state"}"#,
            "final get_state",
        )
        .await;
        assert_eq!(final_state["data"]["sessionName"], "Lifecycle Test");

        logger.info("rpc", "Session lifecycle test complete");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "server error: {result:?}");
    });
}

// ─── Response ID echo tests ─────────────────────────────────────────────────

#[test]
fn rpc_response_echoes_request_id() {
    let _harness = TestHarness::new("rpc_response_id_echo");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // Various IDs.
        for (idx, test_id) in ["abc-123", "42", "request-uuid-test"].iter().enumerate() {
            let cmd = format!(r#"{{"id":"{test_id}","type":"get_state"}}"#);
            let resp = send_recv(
                &in_tx,
                &out_rx,
                &cmd,
                &format!("id echo {idx}"),
            )
            .await;
            assert_eq!(
                resp["id"].as_str().unwrap_or_default(),
                *test_id,
                "response should echo request id"
            );
        }

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── Malformed JSON handling ─────────────────────────────────────────────────

#[test]
fn rpc_malformed_json_does_not_crash_server() {
    let _harness = TestHarness::new("rpc_malformed_json");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (in_tx, out_rx, server) = setup_rpc(Session::in_memory(), &handle);

        // Send malformed JSON.
        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(&cx, "this is not json".to_string())
            .await
            .expect("send malformed");

        // Server should send an error response for malformed JSON.
        let error_resp = recv_line(&out_rx, "malformed error response")
            .await
            .expect("should get error for malformed JSON");
        let error_val: Value =
            serde_json::from_str(error_resp.trim()).expect("error response is valid JSON");
        assert_eq!(error_val["success"], false);

        // Server should still accept valid commands after malformed input.
        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(&cx, r#"{"id":"1","type":"get_state"}"#.to_string())
            .await
            .expect("send get_state after malformed");

        let mut resp = None;
        for _ in 0..8 {
            let line = recv_line(&out_rx, "get_state after malformed")
                .await
                .expect("get_state after malformed");
            let value: Value =
                serde_json::from_str(line.trim()).expect("parse response after malformed");
            if value["id"] == "1" {
                resp = Some(value);
                break;
            }
        }
        let resp = resp.expect("did not receive get_state response with id=1");
        assert_eq!(resp["success"], true);

        drop(in_tx);
        let _ = server.await;
    });
}
