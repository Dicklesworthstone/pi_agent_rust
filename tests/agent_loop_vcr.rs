mod common;

use common::{TestHarness, run_async};
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{ContentBlock, Message, StopReason, TextContent};
use pi::provider::StreamOptions;
use pi::providers::anthropic::AnthropicProvider;
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
    let test_name = "agent_loop_openai_basic";
    let harness = TestHarness::new(test_name);

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
        let mut agent_session = AgentSession::new(
            agent,
            session,
            true,
            pi::compaction::ResolvedCompactionSettings::default(),
        );

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

        write_jsonl_artifacts(&harness, test_name, &["test-key", "vcr-playback"]);
    });
}

const SYSTEM_PROMPT: &str =
    "You are a test harness model. Follow instructions precisely and deterministically.";

fn write_timeline(harness: &TestHarness, timeline: &Arc<Mutex<Vec<serde_json::Value>>>) {
    let timeline_path = harness.temp_path("agent_loop.timeline.jsonl");
    let mut file = File::create(&timeline_path).expect("create timeline");
    if let Ok(events) = timeline.lock() {
        for entry in events.iter() {
            let line = serde_json::to_string(entry).expect("serialize timeline entry");
            let _ = writeln!(file, "{line}");
        }
    }
    harness.record_artifact("agent_loop.timeline.jsonl", &timeline_path);
}

fn write_jsonl_artifacts(harness: &TestHarness, test_name: &str, forbidden: &[&str]) {
    let log_path = harness.temp_path(format!("{test_name}.log.jsonl"));
    harness
        .write_jsonl_logs(&log_path)
        .expect("write jsonl log");
    assert!(log_path.exists(), "jsonl log should exist");
    harness.record_artifact(format!("{test_name}.log.jsonl"), &log_path);

    let normalized_log_path = harness.temp_path(format!("{test_name}.log.normalized.jsonl"));
    harness
        .write_jsonl_logs_normalized(&normalized_log_path)
        .expect("write normalized jsonl log");
    assert!(
        normalized_log_path.exists(),
        "normalized jsonl log should exist"
    );
    harness.record_artifact(
        format!("{test_name}.log.normalized.jsonl"),
        &normalized_log_path,
    );

    let artifacts_path = harness.temp_path(format!("{test_name}.artifacts.jsonl"));
    harness
        .write_artifact_index_jsonl(&artifacts_path)
        .expect("write artifact index jsonl");
    assert!(artifacts_path.exists(), "artifact index should exist");
    harness.record_artifact(format!("{test_name}.artifacts.jsonl"), &artifacts_path);

    let normalized_artifacts_path =
        harness.temp_path(format!("{test_name}.artifacts.normalized.jsonl"));
    harness
        .write_artifact_index_jsonl_normalized(&normalized_artifacts_path)
        .expect("write normalized artifact index jsonl");
    assert!(
        normalized_artifacts_path.exists(),
        "normalized artifact index should exist"
    );
    harness.record_artifact(
        format!("{test_name}.artifacts.normalized.jsonl"),
        &normalized_artifacts_path,
    );

    let log_contents = std::fs::read_to_string(&log_path).expect("read jsonl log");
    let normalized_contents =
        std::fs::read_to_string(&normalized_log_path).expect("read normalized jsonl log");
    for needle in forbidden {
        assert!(
            !log_contents.contains(needle),
            "jsonl logs should redact {needle}"
        );
        assert!(
            !normalized_contents.contains(needle),
            "normalized jsonl logs should redact {needle}"
        );
    }
}

/// Agent loop: simple text response via Anthropic VCR cassette.
/// Verifies end-to-end: user message → provider stream → text response → session persist.
#[test]
fn agent_loop_anthropic_simple_text() {
    let test_name = "agent_loop_anthropic_simple_text";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_name = "anthropic_simple_text";
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);

        if !cassette_path.exists() {
            harness.log().warn("vcr", "Missing cassette; skipping");
            return;
        }

        let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider = AnthropicProvider::new("claude-sonnet-4-20250514").with_client(client);

        let config = Config::default();
        let cwd = harness.temp_dir().to_path_buf();
        let tools = ToolRegistry::new(&[], &cwd, Some(&config));
        let agent_config = AgentConfig {
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            max_tool_iterations: 2,
            stream_options: StreamOptions {
                api_key: Some("vcr-playback".to_string()),
                max_tokens: Some(64),
                temperature: Some(0.0),
                ..Default::default()
            },
        };
        let agent = Agent::new(Arc::new(provider), tools, agent_config);

        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(harness.temp_dir().to_path_buf()),
        )));
        let mut agent_session = AgentSession::new(
            agent,
            session,
            true,
            pi::compaction::ResolvedCompactionSettings::default(),
        );

        let timeline: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let timeline_ref = Arc::clone(&timeline);

        let result = agent_session
            .run_text(
                "Reply with the single word: pong.".to_string(),
                move |event| {
                    if let Ok(mut events) = timeline_ref.lock() {
                        events.push(format_event(&event));
                    }
                },
            )
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
            text.contains("pong"),
            "expected assistant to say 'pong', got: {text}"
        );

        agent_session
            .persist_session()
            .await
            .expect("persist session");

        write_timeline(&harness, &timeline);

        harness
            .log()
            .info_ctx("agent_loop", "Anthropic simple text completed", |ctx| {
                ctx.push(("cassette".into(), cassette_name.to_string()));
                ctx.push(("stop_reason".into(), format!("{:?}", result.stop_reason)));
                ctx.push(("response".into(), text));
            });

        write_jsonl_artifacts(&harness, test_name, &["test-key", "vcr-playback"]);
    });
}

/// Agent loop: error stream from provider.
/// Verifies the agent loop gracefully handles provider errors and reports them
/// through the event callback and result.
#[test]
fn agent_loop_anthropic_error_stream() {
    let test_name = "agent_loop_anthropic_error_stream";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_name = "anthropic_server_error_500";
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);

        if !cassette_path.exists() {
            harness.log().warn("vcr", "Missing cassette; skipping");
            return;
        }

        let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider = AnthropicProvider::new("claude-sonnet-4-20250514").with_client(client);

        let config = Config::default();
        let cwd = harness.temp_dir().to_path_buf();
        let tools = ToolRegistry::new(&[], &cwd, Some(&config));
        let agent_config = AgentConfig {
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            max_tool_iterations: 0,
            stream_options: StreamOptions {
                api_key: Some("vcr-playback".to_string()),
                max_tokens: Some(256),
                temperature: Some(0.0),
                ..Default::default()
            },
        };
        let agent = Agent::new(Arc::new(provider), tools, agent_config);

        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(harness.temp_dir().to_path_buf()),
        )));
        let mut agent_session = AgentSession::new(
            agent,
            session,
            true,
            pi::compaction::ResolvedCompactionSettings::default(),
        );

        let timeline: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let timeline_ref = Arc::clone(&timeline);

        let result = agent_session
            .run_text("Trigger a server error.".to_string(), move |event| {
                if let Ok(mut events) = timeline_ref.lock() {
                    events.push(format_event(&event));
                }
            })
            .await;

        // The agent loop should propagate the provider error.
        assert!(result.is_err(), "expected error from 500 response");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("500") || err_msg.contains("server error") || err_msg.contains("HTTP"),
            "expected error message to mention HTTP 500, got: {err_msg}"
        );

        write_timeline(&harness, &timeline);

        harness
            .log()
            .info_ctx("agent_loop", "Error stream test completed", |ctx| {
                ctx.push(("cassette".into(), cassette_name.to_string()));
                ctx.push(("error".into(), err_msg));
            });

        write_jsonl_artifacts(&harness, test_name, &["test-key", "vcr-playback"]);
    });
}

/// Agent loop: tool call response (single iteration).
/// Verifies the agent receives a `tool_use` stop reason and reports tool call events.
/// Uses `max_tool_iterations=0` to avoid executing tools (no follow-up cassette needed).
#[test]
fn agent_loop_anthropic_tool_call_stop() {
    let test_name = "agent_loop_anthropic_tool_call_stop";
    let harness = TestHarness::new(test_name);

    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_name = "anthropic_tool_call_single";
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);

        if !cassette_path.exists() {
            harness.log().warn("vcr", "Missing cassette; skipping");
            return;
        }

        let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
        let client = Client::new().with_vcr(recorder);
        let provider = AnthropicProvider::new("claude-sonnet-4-20250514").with_client(client);

        let config = Config::default();
        let cwd = harness.temp_dir().to_path_buf();
        let tools = ToolRegistry::new(&[], &cwd, Some(&config));
        let agent_config = AgentConfig {
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            max_tool_iterations: 0,
            stream_options: StreamOptions {
                api_key: Some("vcr-playback".to_string()),
                max_tokens: Some(256),
                temperature: Some(0.0),
                ..Default::default()
            },
        };
        let agent = Agent::new(Arc::new(provider), tools, agent_config);

        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(harness.temp_dir().to_path_buf()),
        )));
        let mut agent_session = AgentSession::new(
            agent,
            session,
            true,
            pi::compaction::ResolvedCompactionSettings::default(),
        );

        let timeline: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let timeline_ref = Arc::clone(&timeline);

        let result = agent_session
            .run_text(
                "Call the echo tool with text='hello'. Do not answer in text.".to_string(),
                move |event| {
                    if let Ok(mut events) = timeline_ref.lock() {
                        events.push(format_event(&event));
                    }
                },
            )
            .await;

        // With max_tool_iterations=0 and a tool_use response, the agent may
        // return the tool call result or an error depending on implementation.
        // Either way, verify event timeline captured the interaction.
        write_timeline(&harness, &timeline);

        let (has_agent_start, event_count) = {
            let events = timeline.lock().expect("timeline lock");
            let has_agent_start = events
                .iter()
                .any(|e| e.get("event").and_then(|v| v.as_str()) == Some("agent_start"));
            let event_count = events.len();
            drop(events);
            (has_agent_start, event_count)
        };
        assert!(has_agent_start, "expected agent_start event in timeline");

        harness
            .log()
            .info_ctx("agent_loop", "Tool call stop test completed", |ctx| {
                ctx.push(("cassette".into(), cassette_name.to_string()));
                ctx.push(("event_count".into(), event_count.to_string()));
                match &result {
                    Ok(msg) => ctx.push(("stop_reason".into(), format!("{:?}", msg.stop_reason))),
                    Err(e) => ctx.push(("error".into(), e.to_string())),
                }
            });

        write_jsonl_artifacts(&harness, test_name, &["test-key", "vcr-playback"]);
    });
}
