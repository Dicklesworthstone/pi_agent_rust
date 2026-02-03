#![allow(clippy::similar_names)]
#![allow(clippy::unnecessary_literal_bound)]
#![allow(clippy::too_many_lines)]

use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall, Usage};
use pi::provider::{Context, Provider, StreamOptions};
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, run};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

struct MockProvider;

#[async_trait::async_trait]
impl Provider for MockProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "mock"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn api(&self) -> &str {
        "mock"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn stream(
        &self,
        _context: &Context,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>>,
    > {
        let now = chrono::Utc::now().timestamp_millis();
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("hello"))],
            api: "mock".to_string(),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now,
        };

        let events = vec![Ok(pi::model::StreamEvent::Done {
            reason: StopReason::Stop,
            message,
        })];

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rpc_get_state_and_prompt() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    let mut config = AgentConfig::default();
    config.stream_options.api_key = Some("test-key".to_string());
    let agent = Agent::new(provider, tools, config);

    let mut session = Session::in_memory();
    session.header.provider = Some("mock".to_string());
    session.header.model_id = Some("mock-model".to_string());
    session.header.thinking_level = Some("off".to_string());

    let agent_session = AgentSession::new(agent, session, false);

    let auth_dir = tempfile::tempdir().unwrap();
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).unwrap();

    let options = RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models: Vec::new(),
        scoped_models: Vec::new(),
        auth,
    };

    let (stdin_client, stdin_server) = tokio::io::duplex(1024);
    let (stdout_server, stdout_client) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        run(
            agent_session,
            options,
            tokio::io::BufReader::new(stdin_server),
            tokio::io::BufWriter::new(stdout_server),
        )
        .await
        .unwrap();
    });

    let mut stdin_client = tokio::io::BufWriter::new(stdin_client);
    let mut stdout_client = tokio::io::BufReader::new(stdout_client);

    // get_state
    stdin_client
        .write_all(br#"{"id":"1","type":"get_state"}"#)
        .await
        .unwrap();
    stdin_client.write_all(b"\n").await.unwrap();
    stdin_client.flush().await.unwrap();

    let mut line = String::new();
    stdout_client.read_line(&mut line).await.unwrap();
    let get_state_response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(get_state_response["type"], "response");
    assert_eq!(get_state_response["command"], "get_state");
    assert_eq!(get_state_response["success"], true);
    let get_state_data = get_state_response["data"].as_object().unwrap();
    assert!(get_state_data.get("sessionFile").is_none());
    assert!(get_state_data.get("sessionName").is_none());
    assert!(get_state_data.get("model").is_some());
    assert!(get_state_response["data"]["model"].is_null());

    // prompt
    line.clear();
    stdin_client
        .write_all(br#"{"id":"2","type":"prompt","message":"hi"}"#)
        .await
        .unwrap();
    stdin_client.write_all(b"\n").await.unwrap();
    stdin_client.flush().await.unwrap();

    stdout_client.read_line(&mut line).await.unwrap();
    let prompt_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(prompt_resp["type"], "response");
    assert_eq!(prompt_resp["command"], "prompt");
    assert_eq!(prompt_resp["success"], true);

    // Collect events until agent_end.
    let mut saw_agent_end = false;
    let mut message_end_count = 0usize;
    for _ in 0..10 {
        line.clear();
        if stdout_client.read_line(&mut line).await.unwrap() == 0 {
            break;
        }
        let event: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        if event["type"] == "message_end" {
            message_end_count += 1;
        }
        if event["type"] == "agent_end" {
            saw_agent_end = true;
            break;
        }
    }
    assert!(saw_agent_end, "did not receive agent_end event");
    assert!(
        message_end_count >= 2,
        "expected at least user+assistant message_end events"
    );

    // get_session_stats
    line.clear();
    stdin_client
        .write_all(br#"{"id":"3","type":"get_session_stats"}"#)
        .await
        .unwrap();
    stdin_client.write_all(b"\n").await.unwrap();
    stdin_client.flush().await.unwrap();

    stdout_client.read_line(&mut line).await.unwrap();
    let get_stats_response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(get_stats_response["type"], "response");
    assert_eq!(get_stats_response["command"], "get_session_stats");
    assert_eq!(get_stats_response["success"], true);
    let get_stats_data = get_stats_response["data"].as_object().unwrap();
    assert!(get_stats_data.get("sessionFile").is_none());
    assert_eq!(get_stats_response["data"]["userMessages"], 1);
    assert_eq!(get_stats_response["data"]["assistantMessages"], 1);
    assert_eq!(get_stats_response["data"]["toolCalls"], 0);
    assert_eq!(get_stats_response["data"]["toolResults"], 0);
    assert_eq!(get_stats_response["data"]["totalMessages"], 2);
    assert_eq!(get_stats_response["data"]["tokens"]["input"], 10);
    assert_eq!(get_stats_response["data"]["tokens"]["output"], 5);
    assert_eq!(get_stats_response["data"]["tokens"]["total"], 15);

    // Shut down by closing stdin.
    drop(stdin_client);
    let _ = server.await;
}

#[tokio::test]
async fn rpc_session_stats_counts_tool_calls_and_results() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    let mut config = AgentConfig::default();
    config.stream_options.api_key = Some("test-key".to_string());
    let agent = Agent::new(provider, tools, config);

    let now = chrono::Utc::now().timestamp_millis();
    let mut session = Session::in_memory();
    session.header.provider = Some("mock".to_string());
    session.header.model_id = Some("mock-model".to_string());
    session.header.thinking_level = Some("off".to_string());
    session.append_message(SessionMessage::User {
        content: pi::model::UserContent::Text("hi".to_string()),
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
            api: "mock".to_string(),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
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

    let agent_session = AgentSession::new(agent, session, false);

    let auth_dir = tempfile::tempdir().unwrap();
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).unwrap();

    let options = RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models: Vec::new(),
        scoped_models: Vec::new(),
        auth,
    };

    let (stdin_client, stdin_server) = tokio::io::duplex(1024);
    let (stdout_server, stdout_client) = tokio::io::duplex(4096);

    let server = tokio::spawn(async move {
        run(
            agent_session,
            options,
            tokio::io::BufReader::new(stdin_server),
            tokio::io::BufWriter::new(stdout_server),
        )
        .await
        .unwrap();
    });

    let mut stdin_client = tokio::io::BufWriter::new(stdin_client);
    let mut stdout_client = tokio::io::BufReader::new(stdout_client);

    stdin_client
        .write_all(br#"{"id":"1","type":"get_session_stats"}"#)
        .await
        .unwrap();
    stdin_client.write_all(b"\n").await.unwrap();
    stdin_client.flush().await.unwrap();

    let mut line = String::new();
    stdout_client.read_line(&mut line).await.unwrap();
    let stats_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(stats_resp["type"], "response");
    assert_eq!(stats_resp["command"], "get_session_stats");
    assert_eq!(stats_resp["success"], true);
    let stats_data = stats_resp["data"].as_object().unwrap();
    assert!(stats_data.get("sessionFile").is_none());
    assert_eq!(stats_resp["data"]["userMessages"], 1);
    assert_eq!(stats_resp["data"]["assistantMessages"], 1);
    assert_eq!(stats_resp["data"]["toolCalls"], 1);
    assert_eq!(stats_resp["data"]["toolResults"], 1);
    assert_eq!(stats_resp["data"]["totalMessages"], 3);
    assert_eq!(stats_resp["data"]["tokens"]["total"], 5);

    drop(stdin_client);
    let _ = server.await;
}
