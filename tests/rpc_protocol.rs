#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]

mod common;

use common::TestHarness;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{
    AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall, Usage, UserContent,
};
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
    let recorder = VcrRecorder::new_with("rpc_protocol", VcrMode::Playback, cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider: Arc<dyn Provider> = Arc::new(OpenAIProvider::new(model).with_client(client));
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    let config = AgentConfig::default();
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    AgentSession::new(agent, session, false)
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

#[test]
fn rpc_rejects_invalid_json_and_missing_type() {
    let harness = TestHarness::new("rpc_rejects_invalid_json_and_missing_type");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);

        let auth = AuthStorage::load(harness.temp_path("auth.json")).expect("load auth storage");
        let options = RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            auth,
            runtime_handle: handle.clone(),
        };

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, "{".to_string())
            .await
            .expect("send invalid json");
        let invalid_line = recv_line(&out_rx, "invalid json response")
            .await
            .expect("recv invalid json response");

        in_tx
            .send(&cx, r#"{"id":"1"}"#.to_string())
            .await
            .expect("send missing type");
        let missing_type_line = recv_line(&out_rx, "missing type response")
            .await
            .expect("recv missing type response");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server returned error: {result:?}");

        let resp: Value = serde_json::from_str(invalid_line.trim()).unwrap();
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "parse");
        assert_eq!(resp["success"], false);
        assert!(resp.get("id").is_none());
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|s| s.contains("Failed to parse command")),
            "unexpected parse error: {resp}"
        );

        let resp: Value = serde_json::from_str(missing_type_line.trim()).unwrap();
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "parse");
        assert_eq!(resp["success"], false);
        assert!(resp.get("id").is_none());
        assert_eq!(resp["error"], "Missing command type");
    });
}

#[test]
fn rpc_errors_on_unknown_command_and_missing_params() {
    let harness = TestHarness::new("rpc_errors_on_unknown_command_and_missing_params");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);

        let auth = AuthStorage::load(harness.temp_path("auth.json")).expect("load auth storage");
        let options = RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            auth,
            runtime_handle: handle.clone(),
        };

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();

        in_tx
            .send(&cx, r#"{"id":"1","type":"nope"}"#.to_string())
            .await
            .expect("send unknown command");
        let unknown_line = recv_line(&out_rx, "unknown command response")
            .await
            .expect("recv unknown command response");

        in_tx
            .send(&cx, r#"{"id":"2","type":"prompt"}"#.to_string())
            .await
            .expect("send prompt missing message");
        let prompt_missing_line = recv_line(&out_rx, "prompt missing message response")
            .await
            .expect("recv prompt missing message response");

        in_tx
            .send(
                &cx,
                r#"{"id":"3","type":"set_model","modelId":"x"}"#.to_string(),
            )
            .await
            .expect("send set_model missing provider");
        let set_model_missing_line = recv_line(&out_rx, "set_model missing provider response")
            .await
            .expect("recv set_model missing provider response");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server returned error: {result:?}");

        let resp: Value = serde_json::from_str(unknown_line.trim()).unwrap();
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "nope");
        assert_eq!(resp["success"], false);
        assert_eq!(resp["id"], "1");
        assert_eq!(resp["error"], "Unknown command: nope");

        let resp: Value = serde_json::from_str(prompt_missing_line.trim()).unwrap();
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "prompt");
        assert_eq!(resp["success"], false);
        assert_eq!(resp["id"], "2");
        assert_eq!(resp["error"], "Missing message");

        let resp: Value = serde_json::from_str(set_model_missing_line.trim()).unwrap();
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "set_model");
        assert_eq!(resp["success"], false);
        assert_eq!(resp["id"], "3");
        assert_eq!(resp["error"], "Missing provider");
    });
}

#[test]
fn rpc_get_messages_preserves_tool_call_identity_and_args() {
    let harness = TestHarness::new("rpc_get_messages_preserves_tool_call_identity_and_args");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hi".to_string()),
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
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage {
                    input: 2,
                    output: 3,
                    total_tokens: 5,
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
            content: vec![ContentBlock::Text(TextContent::new("ok"))],
            details: None,
            is_error: false,
            timestamp: Some(now),
        });

        let agent_session = build_agent_session(session, &cassette_dir);

        let auth = AuthStorage::load(harness.temp_path("auth.json")).expect("load auth storage");
        let options = RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            auth,
            runtime_handle: handle.clone(),
        };

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(&cx, r#"{"id":"1","type":"get_messages"}"#.to_string())
            .await
            .expect("send get_messages");

        let get_messages_line = recv_line(&out_rx, "get_messages response")
            .await
            .expect("recv get_messages response");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server returned error: {result:?}");

        let resp: Value = serde_json::from_str(get_messages_line.trim()).unwrap();
        harness
            .log()
            .info("rpc", format!("get_messages response: {resp}"));
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_messages");
        assert_eq!(resp["success"], true);

        let messages = resp["data"]["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3, "unexpected message count: {messages:?}");

        let assistant = messages
            .iter()
            .find(|msg| msg.get("role").and_then(Value::as_str) == Some("assistant"))
            .expect("assistant message");
        let tool_call = assistant["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|block| block.get("type").and_then(Value::as_str) == Some("toolCall"))
            })
            .expect("toolCall content block");
        assert_eq!(tool_call["id"], "tc1");
        assert_eq!(tool_call["name"], "read");
        assert_eq!(tool_call["arguments"]["path"], "test.txt");

        let tool_result = messages
            .iter()
            .find(|msg| msg.get("role").and_then(Value::as_str) == Some("toolResult"))
            .expect("toolResult message");
        let tool_call_id = tool_result
            .get("toolCallId")
            .or_else(|| tool_result.get("tool_call_id"))
            .and_then(Value::as_str)
            .expect("toolCallId/tool_call_id");
        assert_eq!(tool_call_id, "tc1");
        let tool_name = tool_result
            .get("toolName")
            .or_else(|| tool_result.get("tool_name"))
            .and_then(Value::as_str)
            .expect("toolName/tool_name");
        assert_eq!(tool_name, "read");
        assert_eq!(tool_result["content"][0]["type"], "text");
        assert_eq!(tool_result["content"][0]["text"], "ok");
    });
}
