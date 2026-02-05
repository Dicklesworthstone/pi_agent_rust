mod common;

use common::{TestHarness, run_async};
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{ContentBlock, Message, StopReason, TextContent};
use pi::provider::StreamOptions;
use pi::providers::openai::OpenAIProvider;
use pi::session::Session;
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::json;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn cassette_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr")
}

const fn message_role(message: &Message) -> &'static str {
    match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "tool_result",
        Message::Custom(_) => "custom",
    }
}

fn format_event(event: &AgentEvent) -> serde_json::Value {
    match event {
        AgentEvent::AgentStart { .. } => json!({ "event": "agent_start" }),
        AgentEvent::AgentEnd { error, .. } => {
            json!({ "event": "agent_end", "error": error })
        }
        AgentEvent::TurnStart { .. } => json!({ "event": "turn_start" }),
        AgentEvent::TurnEnd { .. } => json!({ "event": "turn_end" }),
        AgentEvent::MessageStart { message } => {
            json!({ "event": "message_start", "role": message_role(message) })
        }
        AgentEvent::MessageUpdate { .. } => json!({ "event": "message_update" }),
        AgentEvent::MessageEnd { message } => {
            json!({ "event": "message_end", "role": message_role(message) })
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
        } => json!({
            "event": "tool_exec_start",
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            ..
        } => json!({
            "event": "tool_exec_update",
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            ..
        } => json!({
            "event": "tool_exec_end",
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "is_error": is_error,
        }),
    }
}

#[test]
fn agent_loop_openai_vcr_basic() {
    let harness = TestHarness::new("agent_loop_openai_basic");

    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_name = "rpc_prompt";
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);

        if !cassette_path.exists() {
            harness.log().warn(
                "vcr",
                format!(
                    "Missing cassette {}; skipping test",
                    cassette_path.display()
                ),
            );
            return;
        }

        let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider = OpenAIProvider::new("gpt-4o-mini").with_client(client);

        let config = Config::default();
        let cwd = harness.temp_dir().to_path_buf();
        let tools = ToolRegistry::new(&[], &cwd, Some(&config));
        let agent_config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 2,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        };
        let agent = Agent::new(Arc::new(provider), tools, agent_config);

        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(harness.temp_dir().to_path_buf()),
        )));
        let mut agent_session = AgentSession::new(agent, session, true);

        let timeline: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let timeline_ref = Arc::clone(&timeline);

        let result = agent_session
            .run_text("hi".to_string(), move |event| {
                if let Ok(mut events) = timeline_ref.lock() {
                    events.push(format_event(&event));
                }
            })
            .await
            .expect("agent loop run");

        assert_eq!(result.stop_reason, StopReason::Stop);
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.contains("hello"),
            "expected assistant to say 'hello', got: {text}"
        );

        agent_session
            .persist_session()
            .await
            .expect("persist session");

        let session_path = {
            let cx = asupersync::Cx::for_testing();
            let guard = agent_session.session.lock(&cx).await.expect("lock session");
            guard.path.clone()
        };
        if let Some(path) = session_path {
            harness.record_artifact("session.jsonl", &path);
        }

        let timeline_path = harness.temp_path("agent_loop.timeline.jsonl");
        let mut file = File::create(&timeline_path).expect("create timeline");
        if let Ok(events) = timeline.lock() {
            for entry in events.iter() {
                let line = serde_json::to_string(entry).expect("serialize timeline entry");
                let _ = writeln!(file, "{line}");
            }
        }
        harness.record_artifact("agent_loop.timeline.jsonl", &timeline_path);

        harness
            .log()
            .info_ctx("agent_loop", "Completed VCR run", |ctx| {
                ctx.push(("cassette".into(), cassette_name.to_string()));
                ctx.push(("response".into(), text));
            });
    });
}
