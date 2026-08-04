//! Extended integration tests for RPC protocol edge cases: additional commands,
//! state queries, model/thinking operations, session management, and export.
//!
//! Run:
//! ```bash
//! cargo test --test rpc_edge_cases
//! ```

#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]

mod common;

use common::TestHarness;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::provider::Provider;
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, run};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn cassette_root() -> PathBuf {
    env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn build_agent_session(session: Session, cassette_dir: &Path) -> AgentSession {
    let model = "gpt-4o-mini".to_string();
    let recorder = VcrRecorder::new_with("rpc_edge_cases", VcrMode::Playback, cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider: Arc<dyn Provider> = Arc::new(OpenAIProvider::new(model).with_client(client));
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    let config = AgentConfig::default();
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    AgentSession::new(
        agent,
        session,
        false,
        pi::compaction::ResolvedCompactionSettings::default(),
    )
}

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

        if start.elapsed() > Duration::from_secs(10) {
            return Err(format!("{label}: timed out waiting for output"));
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

fn rpc_output_channel() -> (std::sync::mpsc::SyncSender<String>, Receiver<String>) {
    std::sync::mpsc::sync_channel::<String>(1024)
}

fn make_rpc_options(
    harness: &TestHarness,
    handle: &asupersync::runtime::RuntimeHandle,
) -> RpcOptions {
    let auth = AuthStorage::load(harness.temp_path("auth.json")).expect("load auth storage");
    RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models: Vec::new(),
        scoped_models: Vec::new(),
        cli_api_key: None,
        auth,
        runtime_handle: handle.clone(),
    }
}

fn assert_rpc_success(line: &str, command: &str) -> Value {
    let resp: Value = serde_json::from_str(line.trim()).expect("parse rpc response");
    assert_eq!(resp["type"], "response", "response type: {resp}");
    assert_eq!(resp["command"], command, "response command: {resp}");
    assert_eq!(resp["success"], true, "response success: {resp}");
    resp
}

fn assert_rpc_error(line: &str, command: &str) -> Value {
    let resp: Value = serde_json::from_str(line.trim()).expect("parse rpc response");
    assert_eq!(resp["type"], "response", "response type: {resp}");
    assert_eq!(resp["command"], command, "response command: {resp}");
    assert_eq!(resp["success"], false, "response should be error: {resp}");
    resp
}

// ─── get_state ───────────────────────────────────────────────────────────────

#[test]
fn rpc_get_state_returns_initial_state() {
    let harness = TestHarness::new("rpc_get_state_returns_initial_state");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"get_state"}"#.to_string())
            .await
            .expect("send get_state");
        let line = recv_line(&out_rx, "get_state response")
            .await
            .expect("recv get_state");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_state");
        let data = &resp["data"];
        // State should include some session metadata
        assert!(data.is_object(), "Expected object data: {data}");
    });
}

// ─── get_session_stats ───────────────────────────────────────────────────────

#[test]
fn rpc_get_session_stats_returns_stats() {
    let harness = TestHarness::new("rpc_get_session_stats_returns_stats");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(1_700_000_000_000),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("hi there"))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage {
                    input: 10,
                    output: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 1_700_000_000_000,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"get_session_stats"}"#.to_string())
            .await
            .expect("send get_session_stats");
        let line = recv_line(&out_rx, "get_session_stats response")
            .await
            .expect("recv get_session_stats");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_session_stats");
        let data = &resp["data"];
        // Should have some stats fields
        assert!(data.is_object(), "Expected object data: {data}");
    });
}

// ─── get_available_models ────────────────────────────────────────────────────

#[test]
fn rpc_get_available_models_returns_list() {
    let harness = TestHarness::new("rpc_get_available_models_returns_list");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"get_available_models"}"#.to_string(),
            )
            .await
            .expect("send get_available_models");
        let line = recv_line(&out_rx, "get_available_models response")
            .await
            .expect("recv get_available_models");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_available_models");
        let data = &resp["data"];
        // Should contain a models array
        assert!(
            data.get("models").is_some(),
            "Expected models in data: {data}"
        );
    });
}

// ─── set_session_name ────────────────────────────────────────────────────────

#[test]
fn rpc_set_session_name_success() {
    let harness = TestHarness::new("rpc_set_session_name_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"set_session_name","name":"my-test-session"}"#.to_string(),
            )
            .await
            .expect("send set_session_name");
        let line = recv_line(&out_rx, "set_session_name response")
            .await
            .expect("recv set_session_name");

        drop(in_tx);
        let _ = server.await;

        assert_rpc_success(&line, "set_session_name");
    });
}

// ─── get_last_assistant_text ─────────────────────────────────────────────────

#[test]
fn rpc_get_last_assistant_text_with_messages() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_with_messages");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hi".to_string()),
            timestamp: Some(1_700_000_000_000),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(
                    "Hello! How can I help?",
                ))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 1_700_000_000_000,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"get_last_assistant_text"}"#.to_string(),
            )
            .await
            .expect("send get_last_assistant_text");
        let line = recv_line(&out_rx, "get_last_assistant_text response")
            .await
            .expect("recv get_last_assistant_text");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_last_assistant_text");
        let text = resp["data"]["text"].as_str().expect("text field");
        assert!(
            text.contains("Hello! How can I help?"),
            "Expected assistant text: {text}"
        );
    });
}

#[test]
fn rpc_get_last_assistant_text_empty_session() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"get_last_assistant_text"}"#.to_string(),
            )
            .await
            .expect("send get_last_assistant_text");
        let line = recv_line(&out_rx, "get_last_assistant_text response")
            .await
            .expect("recv get_last_assistant_text");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_last_assistant_text");
        // With empty session, text should be empty or null
        let text = resp["data"]["text"].as_str().unwrap_or("");
        assert!(text.is_empty(), "Expected empty text for empty session");
    });
}

// ─── get_commands ────────────────────────────────────────────────────────────

#[test]
fn rpc_get_commands_returns_command_list() {
    let harness = TestHarness::new("rpc_get_commands_returns_command_list");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"get_commands"}"#.to_string())
            .await
            .expect("send get_commands");
        let line = recv_line(&out_rx, "get_commands response")
            .await
            .expect("recv get_commands");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "get_commands");
        let data = &resp["data"];
        // Should contain commands list
        assert!(data.is_object(), "Expected object data: {data}");
    });
}

// ─── export_html ─────────────────────────────────────────────────────────────

#[test]
fn rpc_export_html_with_messages() {
    let harness = TestHarness::new("rpc_export_html_with_messages");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("test message".to_string()),
            timestamp: Some(1_700_000_000_000),
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"export_html"}"#.to_string())
            .await
            .expect("send export_html");
        let line = recv_line(&out_rx, "export_html response")
            .await
            .expect("recv export_html");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_success(&line, "export_html");
        let path = resp["data"]["path"].as_str().expect("path field");
        assert!(!path.is_empty(), "Expected non-empty path");
    });
}

// ─── set_steering_mode ───────────────────────────────────────────────────────

#[test]
fn rpc_set_steering_mode_accepts_valid_mode() {
    let harness = TestHarness::new("rpc_set_steering_mode_accepts_valid_mode");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#.to_string(),
            )
            .await
            .expect("send set_steering_mode");
        let line = recv_line(&out_rx, "set_steering_mode response")
            .await
            .expect("recv set_steering_mode");

        drop(in_tx);
        let _ = server.await;

        assert_rpc_success(&line, "set_steering_mode");
    });
}

// ─── set_auto_compaction ─────────────────────────────────────────────────────

#[test]
fn rpc_set_auto_compaction_toggle() {
    let harness = TestHarness::new("rpc_set_auto_compaction_toggle");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"set_auto_compaction","enabled":false}"#.to_string(),
            )
            .await
            .expect("send set_auto_compaction");
        let line = recv_line(&out_rx, "set_auto_compaction response")
            .await
            .expect("recv set_auto_compaction");

        drop(in_tx);
        let _ = server.await;

        assert_rpc_success(&line, "set_auto_compaction");
    });
}

// ─── set_auto_retry ──────────────────────────────────────────────────────────

#[test]
fn rpc_set_auto_retry_toggle() {
    let harness = TestHarness::new("rpc_set_auto_retry_toggle");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"set_auto_retry","enabled":true}"#.to_string(),
            )
            .await
            .expect("send set_auto_retry");
        let line = recv_line(&out_rx, "set_auto_retry response")
            .await
            .expect("recv set_auto_retry");

        drop(in_tx);
        let _ = server.await;

        assert_rpc_success(&line, "set_auto_retry");
    });
}

// ─── Multiple sequential commands ────────────────────────────────────────────

#[test]
fn rpc_multiple_commands_preserve_id_ordering() {
    let harness = TestHarness::new("rpc_multiple_commands_preserve_id_ordering");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        // Send 3 quick commands
        in_tx
            .send(&cx, r#"{"id":"a","type":"get_state"}"#.to_string())
            .await
            .expect("send a");
        in_tx
            .send(
                &cx,
                r#"{"id":"b","type":"get_available_models"}"#.to_string(),
            )
            .await
            .expect("send b");
        in_tx
            .send(&cx, r#"{"id":"c","type":"get_commands"}"#.to_string())
            .await
            .expect("send c");

        let line_a = recv_line(&out_rx, "response a").await.expect("recv a");
        let line_b = recv_line(&out_rx, "response b").await.expect("recv b");
        let line_c = recv_line(&out_rx, "response c").await.expect("recv c");

        drop(in_tx);
        let _ = server.await;

        // Verify each response has the correct id
        let resp_a: Value = serde_json::from_str(line_a.trim()).unwrap();
        let resp_b: Value = serde_json::from_str(line_b.trim()).unwrap();
        let resp_c: Value = serde_json::from_str(line_c.trim()).unwrap();

        assert_eq!(resp_a["id"], "a");
        assert_eq!(resp_b["id"], "b");
        assert_eq!(resp_c["id"], "c");
        assert_eq!(resp_a["success"], true);
        assert_eq!(resp_b["success"], true);
        assert_eq!(resp_c["success"], true);
    });
}

// ─── steer command requires message ──────────────────────────────────────────

#[test]
fn rpc_steer_missing_message_returns_error() {
    let harness = TestHarness::new("rpc_steer_missing_message_returns_error");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"steer"}"#.to_string())
            .await
            .expect("send steer without message");
        let line = recv_line(&out_rx, "steer response")
            .await
            .expect("recv steer");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_error(&line, "steer");
        assert!(
            resp["error"].as_str().unwrap_or("").contains("Missing"),
            "Expected 'Missing' in error: {resp}"
        );
    });
}

// ─── follow_up command requires message ──────────────────────────────────────

#[test]
fn rpc_follow_up_missing_message_returns_error() {
    let harness = TestHarness::new("rpc_follow_up_missing_message_returns_error");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"follow_up"}"#.to_string())
            .await
            .expect("send follow_up without message");
        let line = recv_line(&out_rx, "follow_up response")
            .await
            .expect("recv follow_up");

        drop(in_tx);
        let _ = server.await;

        let resp = assert_rpc_error(&line, "follow_up");
        assert!(
            resp["error"].as_str().unwrap_or("").contains("Missing"),
            "Expected 'Missing' in error: {resp}"
        );
    });
}

#[test]
fn rpc_follow_up_aliases_missing_message_return_error() {
    let harness = TestHarness::new("rpc_follow_up_aliases_missing_message_return_error");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        let aliases = [
            ("a", r#"{"id":"a","type":"follow-up"}"#),
            ("b", r#"{"id":"b","type":"queue-follow-up"}"#),
            ("c", r#"{"id":"c","type":"followUp"}"#),
        ];
        for (id, cmd) in aliases {
            in_tx.send(&cx, cmd.to_string()).await.expect("send alias");
            let line = recv_line(&out_rx, "follow_up alias response")
                .await
                .expect("recv alias response");
            let resp = assert_rpc_error(&line, "follow_up");
            assert_eq!(resp["id"], id);
            assert!(
                resp["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Missing message"),
                "Expected missing message error for alias {id}: {resp}"
            );
        }

        drop(in_tx);
        let _ = server.await;
    });
}

#[test]
fn rpc_kebab_and_camel_aliases_dispatch_to_canonical_commands() {
    let harness = TestHarness::new("rpc_kebab_and_camel_aliases_dispatch_to_canonical_commands");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"get-state"}"#.to_string())
            .await
            .expect("send get-state");
        let line = recv_line(&out_rx, "get-state response")
            .await
            .expect("recv get-state");
        assert_rpc_success(&line, "get_state");

        in_tx
            .send(
                &cx,
                r#"{"id":"2","type":"set-steering-mode","mode":"all"}"#.to_string(),
            )
            .await
            .expect("send set-steering-mode");
        let line = recv_line(&out_rx, "set-steering-mode response")
            .await
            .expect("recv set-steering-mode");
        assert_rpc_success(&line, "set_steering_mode");

        in_tx
            .send(
                &cx,
                r#"{"id":"3","type":"setFollowUpMode","mode":"one-at-a-time"}"#.to_string(),
            )
            .await
            .expect("send setFollowUpMode");
        let line = recv_line(&out_rx, "setFollowUpMode response")
            .await
            .expect("recv setFollowUpMode");
        assert_rpc_success(&line, "set_follow_up_mode");

        in_tx
            .send(
                &cx,
                r#"{"id":"4","type":"set-auto-compaction","enabled":false}"#.to_string(),
            )
            .await
            .expect("send set-auto-compaction");
        let line = recv_line(&out_rx, "set-auto-compaction response")
            .await
            .expect("recv set-auto-compaction");
        assert_rpc_success(&line, "set_auto_compaction");

        in_tx
            .send(
                &cx,
                r#"{"id":"5","type":"setAutoRetry","enabled":false}"#.to_string(),
            )
            .await
            .expect("send setAutoRetry");
        let line = recv_line(&out_rx, "setAutoRetry response")
            .await
            .expect("recv setAutoRetry");
        assert_rpc_success(&line, "set_auto_retry");

        in_tx
            .send(&cx, r#"{"id":"6","type":"set-model"}"#.to_string())
            .await
            .expect("send set-model");
        let line = recv_line(&out_rx, "set-model response")
            .await
            .expect("recv set-model");
        let resp = assert_rpc_error(&line, "set_model");
        assert!(
            resp["error"]
                .as_str()
                .unwrap_or("")
                .contains("Missing provider"),
            "Expected Missing provider for set-model alias: {resp}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ─── Empty JSON line ─────────────────────────────────────────────────────────

#[test]
fn rpc_empty_line_is_skipped_gracefully() {
    let harness = TestHarness::new("rpc_empty_line_is_skipped_gracefully");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        // Send empty string, then a valid command
        in_tx.send(&cx, String::new()).await.expect("send empty");
        in_tx
            .send(&cx, r#"{"id":"1","type":"get_state"}"#.to_string())
            .await
            .expect("send get_state");

        // Wait a bit for processing
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(100)).await;

        // We should get a response for get_state (empty string may or may not produce output)
        let mut found_get_state = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let recv_result = {
                let rx = out_rx.lock().expect("lock");
                rx.try_recv()
            };
            match recv_result {
                Ok(line) => {
                    let resp: Value = serde_json::from_str(line.trim()).unwrap_or_default();
                    if resp["id"] == "1" && resp["command"] == "get_state" {
                        found_get_state = true;
                        break;
                    }
                }
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }

        drop(in_tx);
        let _ = server.await;

        assert!(
            found_get_state,
            "Expected get_state response after empty line"
        );
    });
}

// ─── Channel disconnect graceful shutdown ────────────────────────────────────

#[test]
fn rpc_server_exits_cleanly_when_input_channel_closes() {
    let harness = TestHarness::new("rpc_server_exits_cleanly_when_input_channel_closes");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = make_rpc_options(&harness, &handle);

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let _out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Immediately close input
        drop(in_tx);

        let result = server.await;
        assert!(result.is_ok(), "RPC server should exit cleanly: {result:?}");
    });
}
