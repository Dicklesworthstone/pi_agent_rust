#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]

//! E2E RPC protocol tests — comprehensive command coverage.
//!
//! These tests drive the RPC server in-process via channels, exercising the full
//! JSON-line protocol for commands that are not yet covered by `rpc_mode.rs` or
//! `rpc_protocol.rs`.

mod common;

use common::TestHarness;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::models::ModelEntry;
use pi::provider::{InputType, Model, ModelCost, Provider};
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{run, RpcOptions, RpcScopedModel};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cassette_root() -> PathBuf {
    std::env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn test_model(id: &str, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "anthropic".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        context_window: 200_000,
        max_tokens: 8192,
        headers: HashMap::new(),
    }
}

fn test_entry(id: &str, reasoning: bool) -> ModelEntry {
    ModelEntry {
        model: test_model(id, reasoning),
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn build_agent_session(session: Session, cassette_dir: &Path) -> AgentSession {
    let model = "gpt-4o-mini".to_string();
    let recorder = VcrRecorder::new_with("e2e_rpc_noop", VcrMode::Playback, cassette_dir);
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

fn build_options(
    handle: &asupersync::runtime::RuntimeHandle,
    auth_path: PathBuf,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<RpcScopedModel>,
) -> RpcOptions {
    let auth = AuthStorage::load(auth_path).expect("load auth storage");
    RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models,
        scoped_models,
        auth,
        runtime_handle: handle.clone(),
    }
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

fn parse_response(line: &str) -> Value {
    serde_json::from_str(line.trim()).expect("parse JSON response")
}

/// Send a command and get the response.
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
        .unwrap_or_else(|_| panic!("send {label}"));
    let line = recv_line(out_rx, label)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    parse_response(&line)
}

/// Assert that a response indicates success with the expected command.
fn assert_ok(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(resp["success"], true, "success for {command}: {resp}");
}

/// Assert that a response indicates an error with the expected command.
fn assert_err(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(resp["success"], false, "expected error for {command}: {resp}");
}

// ---------------------------------------------------------------------------
// Tests: Configuration commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_set_steering_mode_valid() {
    let harness = TestHarness::new("rpc_set_steering_mode_valid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to "all"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#,
            "set_steering_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");
        assert_eq!(resp["id"], "1");

        // Set to "one-at-a-time"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"one-at-a-time"}"#,
            "set_steering_mode(one-at-a-time)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_steering_mode_invalid() {
    let harness = TestHarness::new("rpc_set_steering_mode_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode"}"#,
            "set_steering_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"bogus"}"#,
            "set_steering_mode(bogus)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Invalid steering mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_follow_up_mode_valid_and_invalid() {
    let harness = TestHarness::new("rpc_set_follow_up_mode_valid_and_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Valid
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_follow_up_mode","mode":"all"}"#,
            "set_follow_up_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_follow_up_mode");

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_follow_up_mode"}"#,
            "set_follow_up_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_follow_up_mode","mode":"nope"}"#,
            "set_follow_up_mode(nope)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Invalid follow-up mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_auto_compaction_and_retry() {
    let harness = TestHarness::new("rpc_set_auto_compaction_and_retry");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // set_auto_compaction true
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_auto_compaction","enabled":true}"#,
            "set_auto_compaction(true)",
        )
        .await;
        assert_ok(&resp, "set_auto_compaction");

        // set_auto_compaction missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_auto_compaction"}"#,
            "set_auto_compaction(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_compaction");
        assert_eq!(resp["error"], "Missing enabled");

        // set_auto_retry false
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_auto_retry","enabled":false}"#,
            "set_auto_retry(false)",
        )
        .await;
        assert_ok(&resp, "set_auto_retry");

        // set_auto_retry missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"4","type":"set_auto_retry"}"#,
            "set_auto_retry(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_retry");
        assert_eq!(resp["error"], "Missing enabled");

        // abort_retry (always succeeds)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"5","type":"abort_retry"}"#,
            "abort_retry",
        )
        .await;
        assert_ok(&resp, "abort_retry");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Model / Thinking Level commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_available_models_empty() {
    let harness = TestHarness::new("rpc_get_available_models_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert!(models.is_empty(), "expected empty model list");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_available_models_populated() {
    let harness = TestHarness::new("rpc_get_available_models_populated");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let models = vec![
            test_entry("claude-opus-4-6", true),
            test_entry("gpt-4o", false),
        ];
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-opus-4-6");
        assert_eq!(models[0]["reasoning"], true);
        assert_eq!(models[1]["id"], "gpt-4o");
        assert_eq!(models[1]["reasoning"], false);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_success() {
    let harness = TestHarness::new("rpc_set_thinking_level_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to high
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level","level":"high"}"#,
            "set_thinking_level(high)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        // Set to off
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"off"}"#,
            "set_thinking_level(off)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_errors() {
    let harness = TestHarness::new("rpc_set_thinking_level_errors");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level"}"#,
            "set_thinking_level(missing)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert_eq!(resp["error"], "Missing level");

        // Invalid level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"impossible"}"#,
            "set_thinking_level(impossible)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "expected error message"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session data commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_messages_empty_session() {
    let harness = TestHarness::new("rpc_get_messages_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(empty)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert!(messages.is_empty(), "expected empty messages");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_messages_with_user_messages() {
    let harness = TestHarness::new("rpc_get_messages_with_user_messages");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::User {
            content: UserContent::Text("world".to_string()),
            timestamp: Some(now + 1000),
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(with_users)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_empty() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(empty)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert!(resp["data"]["text"].is_null(), "expected null text");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_with_assistant() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_with_assistant");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("Hi there!"))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(with_assistant)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "Hi there!");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_commands_empty() {
    let harness = TestHarness::new("rpc_get_commands_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_commands"}"#,
            "get_commands",
        )
        .await;
        assert_ok(&resp, "get_commands");
        assert!(
            resp["data"]["commands"].is_array(),
            "commands should be an array"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session management commands
// ---------------------------------------------------------------------------

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
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name","name":"Test Session"}"#,
            "set_session_name",
        )
        .await;
        assert_ok(&resp, "set_session_name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_session_name_missing_name() {
    let harness = TestHarness::new("rpc_set_session_name_missing_name");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name"}"#,
            "set_session_name(missing)",
        )
        .await;
        assert_err(&resp, "set_session_name");
        assert_eq!(resp["error"], "Missing name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Bash command
// ---------------------------------------------------------------------------

#[test]
fn rpc_bash_echo() {
    let harness = TestHarness::new("rpc_bash_echo");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"echo hello_rpc"}"#,
            "bash(echo)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 0);
        let output = resp["data"]["output"].as_str().unwrap_or("");
        assert!(
            output.contains("hello_rpc"),
            "bash output should contain hello_rpc, got: {output}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_missing_command() {
    let harness = TestHarness::new("rpc_bash_missing_command");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash"}"#,
            "bash(missing)",
        )
        .await;
        assert_err(&resp, "bash");
        assert_eq!(resp["error"], "Missing command");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_nonzero_exit() {
    let harness = TestHarness::new("rpc_bash_nonzero_exit");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"exit 42"}"#,
            "bash(exit 42)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 42);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Request ID handling
// ---------------------------------------------------------------------------

#[test]
fn rpc_request_id_preserved() {
    let harness = TestHarness::new("rpc_request_id_preserved");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // With string ID
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"custom-id-123","type":"get_state"}"#,
            "get_state(with id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert_eq!(resp["id"], "custom-id-123");

        // With numeric ID (RPC server uses as_str(), so numeric IDs are treated as absent)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":42,"type":"get_state"}"#,
            "get_state(numeric id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "numeric IDs should be treated as absent (parsed via as_str)"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_request_without_id() {
    let harness = TestHarness::new("rpc_request_without_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Request without id field
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"type":"get_state"}"#,
            "get_state(no id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        // id should be null or absent
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "expected no id or null id"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Multiple rapid commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_rapid_sequence_of_sync_commands() {
    let harness = TestHarness::new("rpc_rapid_sequence_of_sync_commands");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let models = vec![test_entry("test-model", false)];
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(32);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();

        // Fire 8 commands rapidly.
        let commands = [
            (r#"{"id":"1","type":"get_state"}"#, "get_state"),
            (
                r#"{"id":"2","type":"get_available_models"}"#,
                "get_available_models",
            ),
            (r#"{"id":"3","type":"get_messages"}"#, "get_messages"),
            (
                r#"{"id":"4","type":"get_session_stats"}"#,
                "get_session_stats",
            ),
            (r#"{"id":"5","type":"get_commands"}"#, "get_commands"),
            (
                r#"{"id":"6","type":"get_last_assistant_text"}"#,
                "get_last_assistant_text",
            ),
            (
                r#"{"id":"7","type":"set_auto_compaction","enabled":true}"#,
                "set_auto_compaction",
            ),
            (
                r#"{"id":"8","type":"set_auto_retry","enabled":false}"#,
                "set_auto_retry",
            ),
        ];

        for (cmd, _label) in &commands {
            in_tx
                .send(&cx, cmd.to_string())
                .await
                .expect("send rapid command");
        }

        // Collect all 8 responses.
        let mut responses = Vec::new();
        for (_, label) in &commands {
            let line = recv_line(&out_rx, label)
                .await
                .unwrap_or_else(|err| panic!("{err}"));
            responses.push(parse_response(&line));
        }

        // Verify each response matches its command.
        for (i, (_, expected_cmd)) in commands.iter().enumerate() {
            assert_ok(&responses[i], expected_cmd);
            assert_eq!(
                responses[i]["id"],
                serde_json::Value::String((i + 1).to_string())
            );
        }

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: State reflection after mutations
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_state_reflects_session_stats() {
    let harness = TestHarness::new("rpc_get_state_reflects_session_stats");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-opus-4-6".to_string());
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("world"))],
                api: "test".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-opus-4-6".to_string(),
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

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // get_session_stats should reflect pre-populated messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats",
        )
        .await;
        assert_ok(&resp, "get_session_stats");
        assert_eq!(resp["data"]["userMessages"], 1);
        assert_eq!(resp["data"]["assistantMessages"], 1);
        assert_eq!(resp["data"]["totalMessages"], 2);
        assert_eq!(resp["data"]["tokens"]["input"], 10);
        assert_eq!(resp["data"]["tokens"]["output"], 5);
        assert_eq!(resp["data"]["tokens"]["total"], 15);

        // get_messages should return the 2 messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_messages"}"#,
            "get_messages",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let msgs = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");

        // get_last_assistant_text should return "world"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "world");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Error path coverage
// ---------------------------------------------------------------------------

#[test]
fn rpc_prompt_missing_message() {
    let harness = TestHarness::new("rpc_prompt_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"prompt"}"#,
            "prompt(missing message)",
        )
        .await;
        assert_err(&resp, "prompt");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_steer_missing_message() {
    let harness = TestHarness::new("rpc_steer_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"steer"}"#,
            "steer(missing message)",
        )
        .await;
        assert_err(&resp, "steer");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_follow_up_missing_message() {
    let harness = TestHarness::new("rpc_follow_up_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"follow_up"}"#,
            "follow_up(missing message)",
        )
        .await;
        assert_err(&resp, "follow_up");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_model_missing_model_id() {
    let harness = TestHarness::new("rpc_set_model_missing_model_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing provider
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","modelId":"x"}"#,
            "set_model(missing provider)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing provider");

        // Missing modelId
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_model","provider":"anthropic"}"#,
            "set_model(missing modelId)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing modelId");

        // Model not found
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_model","provider":"anthropic","modelId":"nonexistent"}"#,
            "set_model(not found)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|s| s.contains("Model not found")),
            "expected model not found error: {resp}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_fork_missing_entry_id() {
    let harness = TestHarness::new("rpc_fork_missing_entry_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"fork"}"#,
            "fork(missing entryId)",
        )
        .await;
        assert_err(&resp, "fork");
        assert_eq!(resp["error"], "Missing entryId");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_export_html_empty_session() {
    let harness = TestHarness::new("rpc_export_html_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let output = harness.temp_path("export.html");
        let cmd = format!(
            r#"{{"id":"1","type":"export_html","outputPath":"{}"}}"#,
            output.display()
        );
        let resp = send_recv(&in_tx, &out_rx, &cmd, "export_html").await;
        assert_ok(&resp, "export_html");
        assert!(resp["data"]["path"].is_string(), "should return path");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: abort (when not streaming)
// ---------------------------------------------------------------------------

#[test]
fn rpc_abort_when_idle() {
    let harness = TestHarness::new("rpc_abort_when_idle");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Abort when nothing is streaming should still succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"abort"}"#,
            "abort(idle)",
        )
        .await;
        assert_ok(&resp, "abort");

        // abort_bash when nothing is running should also succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"abort_bash"}"#,
            "abort_bash(idle)",
        )
        .await;
        assert_ok(&resp, "abort_bash");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}
