#![allow(clippy::similar_names)]
#![allow(clippy::unnecessary_literal_bound)]
#![allow(clippy::too_many_lines)]

mod common;

use common::logging::TestLogger;
use common::{TestEnv, TestHarness};
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall, Usage};
use pi::provider::Provider;
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, run};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn vcr_mode() -> VcrMode {
    match env::var("VCR_MODE")
        .ok()
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("record") => VcrMode::Record,
        Some("auto") => VcrMode::Auto,
        _ => VcrMode::Playback,
    }
}

fn vcr_strict() -> bool {
    env_truthy("VCR_STRICT")
}

fn cassette_root() -> PathBuf {
    env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn openai_test_model() -> String {
    env::var("OPENAI_TEST_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

fn openai_auth_header(mode: VcrMode) -> String {
    let key = match mode {
        VcrMode::Record => {
            env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required for VCR record mode")
        }
        _ => env::var("OPENAI_API_KEY").unwrap_or_else(|_| "test-openai-key".to_string()),
    };
    format!("Bearer {key}")
}

fn log_vcr_context(
    logger: &TestLogger,
    mode: VcrMode,
    cassette_dir: &Path,
    cassette_name: &str,
) -> PathBuf {
    let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
    logger.info_ctx("vcr", "RPC VCR context", |ctx| {
        ctx.push(("mode".into(), format!("{mode:?}")));
        ctx.push(("cassette".into(), cassette_name.to_string()));
        ctx.push(("cassette_path".into(), cassette_path.display().to_string()));
    });
    logger.record_artifact(format!("vcr_cassette:{cassette_name}"), &cassette_path);

    let mut env = TestEnv::new();
    env.set("VCR_MODE", format!("{mode:?}"));
    env.set("VCR_CASSETTE_DIR", cassette_dir.display().to_string());
    env.set("OPENAI_TEST_MODEL", openai_test_model());
    env.log(logger, "env", "RPC test environment");

    cassette_path
}

fn assert_json_eq(
    logger: &TestLogger,
    label: &str,
    actual: &serde_json::Value,
    expected: &serde_json::Value,
) {
    if actual != expected {
        logger.error_ctx("assert", label, |ctx| {
            ctx.push(("expected".into(), expected.to_string()));
            ctx.push(("actual".into(), actual.to_string()));
        });
    }
    assert_eq!(actual, expected, "{label}");
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

        if start.elapsed() > Duration::from_secs(20) {
            return Err(format!("{label}: timed out waiting for output"));
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn rpc_get_state_and_prompt() {
    let harness = TestHarness::new("rpc_get_state_and_prompt");
    let logger = harness.log();
    let cassette_dir = cassette_root();
    let mode = vcr_mode();
    let cassette_name = "rpc_prompt";
    let cassette_path = log_vcr_context(logger, mode, &cassette_dir, cassette_name);
    if mode == VcrMode::Playback && !cassette_path.exists() {
        let message = format!("Missing cassette {}", cassette_path.display());
        assert!(!vcr_strict(), "{message}");
        logger.warn("vcr", message);
        return;
    }

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let model = openai_test_model();
        let auth_header = openai_auth_header(mode);
        let recorder = VcrRecorder::new_with(cassette_name, mode, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider: Arc<dyn Provider> =
            Arc::new(OpenAIProvider::new(model.clone()).with_client(client));
        let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
        let mut config = AgentConfig::default();
        config
            .stream_options
            .headers
            .insert("Authorization".to_string(), auth_header);
        let agent = Agent::new(provider, tools, config);

        let mut session = Session::in_memory();
        session.header.provider = Some("openai".to_string());
        session.header.model_id = Some(model);
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
            runtime_handle: handle.clone(),
        };

        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // get_state
        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(&cx, r#"{"id":"1","type":"get_state"}"#.to_string())
            .await
            .expect("send get_state");

        let line = recv_line(&out_rx, "get_state response")
            .await
            .expect("recv get_state response");
        let get_state_response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let expected_response = serde_json::Value::String("response".to_string());
        let expected_get_state = serde_json::Value::String("get_state".to_string());
        let expected_prompt = serde_json::Value::String("prompt".to_string());
        let expected_stats = serde_json::Value::String("get_session_stats".to_string());
        let expected_true = serde_json::Value::Bool(true);

        assert_json_eq(
            logger,
            "get_state.type",
            &get_state_response["type"],
            &expected_response,
        );
        assert_json_eq(
            logger,
            "get_state.command",
            &get_state_response["command"],
            &expected_get_state,
        );
        assert_json_eq(
            logger,
            "get_state.success",
            &get_state_response["success"],
            &expected_true,
        );
        let get_state_data = get_state_response["data"].as_object().unwrap();
        assert!(get_state_data.get("sessionFile").is_some());
        assert!(get_state_response["data"]["sessionFile"].is_null());
        assert!(get_state_data.get("sessionName").is_some());
        assert!(get_state_response["data"]["sessionName"].is_null());
        assert!(get_state_data.get("model").is_some());
        assert!(get_state_response["data"]["model"].is_null());

        // prompt
        in_tx
            .send(
                &cx,
                r#"{"id":"2","type":"prompt","message":"hi"}"#.to_string(),
            )
            .await
            .expect("send prompt");

        let line = recv_line(&out_rx, "prompt response")
            .await
            .expect("recv prompt response");
        let prompt_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_json_eq(
            logger,
            "prompt.type",
            &prompt_resp["type"],
            &expected_response,
        );
        assert_json_eq(
            logger,
            "prompt.command",
            &prompt_resp["command"],
            &expected_prompt,
        );
        assert_json_eq(
            logger,
            "prompt.success",
            &prompt_resp["success"],
            &expected_true,
        );

        // Collect events until agent_end.
        let mut saw_agent_end = false;
        let mut message_end_count = 0usize;
        let mut event_timeline = Vec::new();
        let log_events = env_truthy("RPC_TEST_LOG");
        for _ in 0..100 {
            let line = recv_line(&out_rx, "event stream")
                .await
                .expect("recv event stream");
            let event: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            if let Some(event_type) = event["type"].as_str() {
                event_timeline.push(event_type.to_string());
            }
            if log_events {
                if event["type"] == "auto_retry_start" {
                    eprintln!(
                        "rpc event: auto_retry_start delayMs={} errorMessage={}",
                        event["delayMs"], event["errorMessage"]
                    );
                } else if event["type"] == "agent_end" {
                    eprintln!("rpc event: agent_end error={}", event["error"]);
                } else {
                    eprintln!("rpc event: {}", event["type"]);
                }
            }
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
        logger.info_ctx("rpc", "Event timeline", |ctx| {
            ctx.push(("events".into(), event_timeline.join(", ")));
            ctx.push(("message_end_count".into(), message_end_count.to_string()));
        });

        // get_session_stats
        in_tx
            .send(&cx, r#"{"id":"3","type":"get_session_stats"}"#.to_string())
            .await
            .expect("send get_session_stats");

        let line = recv_line(&out_rx, "get_session_stats response")
            .await
            .expect("recv get_session_stats response");
        let get_stats_response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        if log_events {
            eprintln!("get_session_stats response: {get_stats_response}");
        }
        assert_json_eq(
            logger,
            "get_session_stats.type",
            &get_stats_response["type"],
            &expected_response,
        );
        assert_json_eq(
            logger,
            "get_session_stats.command",
            &get_stats_response["command"],
            &expected_stats,
        );
        assert_json_eq(
            logger,
            "get_session_stats.success",
            &get_stats_response["success"],
            &expected_true,
        );
        let get_stats_data = get_stats_response["data"].as_object().unwrap();
        assert!(get_stats_data.get("sessionFile").is_some());
        assert!(get_stats_response["data"]["sessionFile"].is_null());
        logger.info_ctx("rpc", "Session stats values", |ctx| {
            ctx.push((
                "userMessages".into(),
                get_stats_response["data"]["userMessages"].to_string(),
            ));
            ctx.push((
                "assistantMessages".into(),
                get_stats_response["data"]["assistantMessages"].to_string(),
            ));
            ctx.push((
                "toolCalls".into(),
                get_stats_response["data"]["toolCalls"].to_string(),
            ));
            ctx.push((
                "toolResults".into(),
                get_stats_response["data"]["toolResults"].to_string(),
            ));
            ctx.push((
                "totalMessages".into(),
                get_stats_response["data"]["totalMessages"].to_string(),
            ));
            ctx.push((
                "tokens.input".into(),
                get_stats_response["data"]["tokens"]["input"].to_string(),
            ));
            ctx.push((
                "tokens.output".into(),
                get_stats_response["data"]["tokens"]["output"].to_string(),
            ));
            ctx.push((
                "tokens.total".into(),
                get_stats_response["data"]["tokens"]["total"].to_string(),
            ));
        });
        assert_eq!(get_stats_response["data"]["userMessages"], 1);
        assert_eq!(get_stats_response["data"]["assistantMessages"], 1);
        assert_eq!(get_stats_response["data"]["toolCalls"], 0);
        assert_eq!(get_stats_response["data"]["toolResults"], 0);
        assert_eq!(get_stats_response["data"]["totalMessages"], 2);
        assert_eq!(get_stats_response["data"]["tokens"]["input"], 10);
        assert_eq!(get_stats_response["data"]["tokens"]["output"], 5);
        assert_eq!(get_stats_response["data"]["tokens"]["total"], 15);

        drop(in_tx);

        let result = server.await;
        assert!(result.is_ok(), "rpc server returned error: {result:?}");
    });
}

#[test]
fn rpc_session_stats_counts_tool_calls_and_results() {
    let harness = TestHarness::new("rpc_session_stats_counts_tool_calls_and_results");
    let logger = harness.log();
    let cassette_dir = cassette_root();
    let mode = vcr_mode();
    let cassette_name = "rpc_session_stats";
    let cassette_path = log_vcr_context(logger, mode, &cassette_dir, cassette_name);
    if mode == VcrMode::Playback && !cassette_path.exists() {
        let message = format!("Missing cassette {}", cassette_path.display());
        assert!(!vcr_strict(), "{message}");
        logger.warn("vcr", message);
        return;
    }
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let model = openai_test_model();
        let auth_header = openai_auth_header(mode);
        let recorder = VcrRecorder::new_with(cassette_name, mode, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider: Arc<dyn Provider> =
            Arc::new(OpenAIProvider::new(model.clone()).with_client(client));
        let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
        let mut config = AgentConfig::default();
        config
            .stream_options
            .headers
            .insert("Authorization".to_string(), auth_header);
        let agent = Agent::new(provider, tools, config);

        let now = chrono::Utc::now().timestamp_millis();
        let mut session = Session::in_memory();
        session.header.provider = Some("openai".to_string());
        session.header.model_id = Some(model);
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

        let agent_session = AgentSession::new(agent, session, false);

        let auth_dir = tempfile::tempdir().unwrap();
        let auth = AuthStorage::load(auth_dir.path().join("auth.json")).unwrap();

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
            .send(&cx, r#"{"id":"1","type":"get_session_stats"}"#.to_string())
            .await
            .expect("send get_session_stats");

        let line = recv_line(&out_rx, "get_session_stats response")
            .await
            .expect("recv get_session_stats response");
        let stats_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let expected_response = serde_json::Value::String("response".to_string());
        let expected_stats = serde_json::Value::String("get_session_stats".to_string());
        let expected_true = serde_json::Value::Bool(true);

        assert_json_eq(
            logger,
            "get_session_stats.type",
            &stats_resp["type"],
            &expected_response,
        );
        assert_json_eq(
            logger,
            "get_session_stats.command",
            &stats_resp["command"],
            &expected_stats,
        );
        assert_json_eq(
            logger,
            "get_session_stats.success",
            &stats_resp["success"],
            &expected_true,
        );
        let stats_data = stats_resp["data"].as_object().unwrap();
        assert!(stats_data.get("sessionFile").is_some());
        assert!(stats_resp["data"]["sessionFile"].is_null());
        logger.info_ctx("rpc", "Session stats values", |ctx| {
            ctx.push((
                "userMessages".into(),
                stats_resp["data"]["userMessages"].to_string(),
            ));
            ctx.push((
                "assistantMessages".into(),
                stats_resp["data"]["assistantMessages"].to_string(),
            ));
            ctx.push((
                "toolCalls".into(),
                stats_resp["data"]["toolCalls"].to_string(),
            ));
            ctx.push((
                "toolResults".into(),
                stats_resp["data"]["toolResults"].to_string(),
            ));
            ctx.push((
                "totalMessages".into(),
                stats_resp["data"]["totalMessages"].to_string(),
            ));
            ctx.push((
                "tokens.total".into(),
                stats_resp["data"]["tokens"]["total"].to_string(),
            ));
        });
        assert_eq!(stats_resp["data"]["userMessages"], 1);
        assert_eq!(stats_resp["data"]["assistantMessages"], 1);
        assert_eq!(stats_resp["data"]["toolCalls"], 1);
        assert_eq!(stats_resp["data"]["toolResults"], 1);
        assert_eq!(stats_resp["data"]["totalMessages"], 3);
        assert_eq!(stats_resp["data"]["tokens"]["total"], 5);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server returned error: {result:?}");
    });
}
