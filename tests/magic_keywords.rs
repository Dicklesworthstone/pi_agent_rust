//! Integration tests for magic keywords (bd-cv653.3.6).
//!
//! Acceptance coverage:
//! 1. `please ultrathink this design` → the outbound request runs at the
//!    active model's clamped maximum, proven via a capture provider.
//! 2. `orchestrate`/`workflowz` inject the correct directive exactly once per
//!    turn (system prompt contains it once).
//! 3. Settings disable each keyword (no level change, no directive injected).
//! 4. Code/fence/XML/path occurrences leave the request untouched.
//! 5. Block-content prompts activate and persist replayable telemetry.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use asupersync::sync::Mutex as AsyncMutex;
use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::compaction::ResolvedCompactionSettings;
use pi::config::Config;
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingLevel,
    UserContent, UserMessage,
};
use pi::models::{ModelEntry, ModelRegistry};
use pi::provider::{Context, InputType, Model, ModelCost, StreamOptions};
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, run as run_rpc};
use pi::session::{Session, SessionEntry};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

async fn recv_rpc_line(rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Result<String, String> {
    let started = Instant::now();
    loop {
        let recv_result = match rx.lock() {
            Ok(receiver) => receiver.try_recv(),
            Err(poisoned) => poisoned.into_inner().try_recv(),
        };
        match recv_result {
            Ok(line) => return Ok(line),
            Err(TryRecvError::Disconnected) => {
                return Err(format!("{label}: RPC output disconnected"));
            }
            Err(TryRecvError::Empty) => {}
        }
        if started.elapsed() >= Duration::from_secs(10) {
            return Err(format!("{label}: timed out waiting for RPC output"));
        }
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

/// Records the options + system prompt of every request, then streams a
/// one-line assistant reply.
#[derive(Default, Clone)]
struct Capture {
    thinking: Vec<Option<ThinkingLevel>>,
    system_prompts: Vec<Option<String>>,
}

struct CaptureProvider {
    capture: Arc<Mutex<Capture>>,
    first_call_entered: Mutex<Option<asupersync::channel::oneshot::Sender<()>>>,
    first_call_gate: Mutex<Option<asupersync::channel::oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for CaptureProvider {
    fn name(&self) -> &str {
        "capture"
    }

    fn api(&self) -> &str {
        "capture-api"
    }

    fn model_id(&self) -> &str {
        "capture-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        {
            let mut capture = self.capture.lock().expect("capture");
            capture.thinking.push(options.thinking_level);
            capture
                .system_prompts
                .push(context.system_prompt.as_ref().map(ToString::to_string));
        }
        let first_call_entered = self
            .first_call_entered
            .lock()
            .expect("first call entered signal")
            .take();
        let first_call_gate = self
            .first_call_gate
            .lock()
            .expect("first call gate")
            .take();
        if let Some(entered) = first_call_entered {
            let cx = asupersync::Cx::for_testing();
            entered.send(&cx, ()).expect("signal first provider call");
        }
        if let Some(mut gate) = first_call_gate {
            let cx = asupersync::Cx::for_testing();
            gate.recv(&cx)
                .await
                .expect("release first provider call");
        }
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                content_index: 0,
                delta: "done".to_string(),
            }),
            Ok(StreamEvent::TextEnd {
                content_index: 0,
                content: "done".to_string(),
            }),
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("done"))],
                    api: "capture-api".to_string(),
                    provider: "capture".to_string(),
                    model: "capture-model".to_string(),
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                },
            }),
        ])))
    }
}

fn build_agent(
    root: &Path,
    keywords: Option<pi::magic_keywords::KeywordSettings>,
) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
        first_call_entered: Mutex::new(None),
        first_call_gate: Mutex::new(None),
    });
    let tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    let config = AgentConfig {
        system_prompt: Some("base prompt".to_string()),
        keyword_settings: keywords,
        ..AgentConfig::default()
    };
    (Agent::new(provider, tools, config), capture)
}

fn build_gated_agent(
    root: &Path,
    first_call_entered: asupersync::channel::oneshot::Sender<()>,
    first_call_gate: asupersync::channel::oneshot::Receiver<()>,
) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
        first_call_entered: Mutex::new(Some(first_call_entered)),
        first_call_gate: Mutex::new(Some(first_call_gate)),
    });
    let tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    let config = AgentConfig {
        system_prompt: Some("base prompt".to_string()),
        ..AgentConfig::default()
    };
    (Agent::new(provider, tools, config), capture)
}

fn capture_model_entry() -> ModelEntry {
    ModelEntry {
        model: Model {
            id: "capture-model".to_string(),
            name: "Capture Model".to_string(),
            api: "capture-api".to_string(),
            provider: "capture".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            reasoning: true,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn capture_model_registry(auth: &AuthStorage) -> (ModelRegistry, ThinkingLevel) {
    let entry = capture_model_entry();
    let expected_max = entry.clamp_thinking_level(ThinkingLevel::Max);
    assert_eq!(
        expected_max,
        ThinkingLevel::High,
        "capture fixture must exercise a real Max-to-High clamp"
    );
    let mut registry = ModelRegistry::load(auth, None);
    registry.merge_entries(vec![entry]);
    (registry, expected_max)
}

#[test]
fn ultrathink_uses_model_clamped_max() {
    let case = "ultrathink_uses_model_clamped_max";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (agent, capture) = build_agent(&root, None);
    let auth_dir = tempfile::tempdir().expect("auth tempdir");
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).expect("auth storage");
    let (registry, expected_max) = capture_model_registry(&auth);
    let session = Arc::new(AsyncMutex::new(Session::in_memory()));
    let mut agent_session =
        AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
    agent_session.set_model_registry(registry);

    let response =
        block_on_local(agent_session.run_text("please ultrathink this design".to_string(), |_| {}))
            .expect("run");
    assert_eq!(response.stop_reason, StopReason::Stop);
    let capture = capture.lock().expect("capture").clone();
    harness.log().info(
        "verify",
        format!("captured thinking levels: {:?}", capture.thinking),
    );
    assert_eq!(
        capture.thinking.first(),
        Some(&Some(expected_max)),
        "ultrathink must use the active model's clamped max: {:?}",
        capture.thinking
    );
    finish_case(&harness, case);
}

#[test]
fn ultrathink_does_not_guess_capabilities_for_unknown_model() {
    let case = "ultrathink_does_not_guess_capabilities_for_unknown_model";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    block_on_local(agent.run("please ultrathink this design", |_| {})).expect("run");
    let capture = capture.lock().expect("capture").clone();
    assert_eq!(
        capture.thinking.first(),
        Some(&Some(ThinkingLevel::Off)),
        "an unregistered provider must fail closed instead of receiving raw Max"
    );
    finish_case(&harness, case);
}

#[test]
fn block_content_text_activates_keywords() {
    let case = "block_content_text_activates_keywords";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    agent.set_keyword_max_thinking_level(ThinkingLevel::High);

    block_on_local(agent.run_with_content(
        vec![ContentBlock::Text(TextContent::new(
            "please ultrathink and orchestrate this".to_string(),
        ))],
        |_| {},
    ))
    .expect("run block content");
    let capture = capture.lock().expect("capture").clone();
    assert_eq!(capture.thinking.first(), Some(&Some(ThinkingLevel::High)));
    assert!(
        capture.system_prompts[0]
            .as_deref()
            .is_some_and(|prompt| prompt.contains("`orchestrate` for this turn")),
        "block text must receive the same directive handling as plain text"
    );
    finish_case(&harness, case);
}

#[test]
fn directives_injected_exactly_once() {
    let case = "directives_injected_exactly_once";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    block_on_local(agent.run("orchestrate the migration then workflowz it", |_| {})).expect("run");
    let capture = capture.lock().expect("capture").clone();
    let prompt = capture.system_prompts[0].clone().expect("system prompt");
    harness.log().info(
        "verify",
        format!(
            "prompt tail: {}",
            &prompt[prompt.len().saturating_sub(400)..]
        ),
    );
    assert!(
        prompt.contains("`orchestrate` for this turn"),
        "orchestrate directive missing"
    );
    assert!(
        prompt.contains("`workflowz` for this turn"),
        "workflowz directive missing"
    );
    assert_eq!(
        prompt.matches("`orchestrate` for this turn").count(),
        1,
        "orchestrate directive must appear exactly once"
    );
    assert_eq!(
        prompt.matches("`workflowz` for this turn").count(),
        1,
        "workflowz directive must appear exactly once"
    );
    finish_case(&harness, case);
}

#[test]
fn directive_is_injected_once_across_multiple_user_prompts() {
    let case = "directive_is_injected_once_across_multiple_user_prompts";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    let prompts = [
        "orchestrate the first slice",
        "orchestrate the second slice",
    ]
    .into_iter()
    .map(|text| {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    })
    .collect();

    block_on_local(agent.run_with_messages_with_abort(prompts, None, |_| {}))
        .expect("run multiple prompts");
    let capture = capture.lock().expect("capture").clone();
    let prompt = capture.system_prompts[0].as_deref().expect("system prompt");
    assert_eq!(
        prompt.matches("`orchestrate` for this turn").count(),
        1,
        "the directive must remain idempotent across the entire turn"
    );
    finish_case(&harness, case);
}

#[test]
fn settings_disable_keywords() {
    let case = "settings_disable_keywords";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let settings = pi::magic_keywords::KeywordSettings {
        ultrathink: Some(false),
        orchestrate: Some(false),
        workflowz: Some(false),
        ..Default::default()
    };
    let (mut agent, capture) = build_agent(&root, Some(settings));
    agent.set_keyword_max_thinking_level(ThinkingLevel::High);

    block_on_local(agent.run("ultrathink, orchestrate, and workflowz this", |_| {})).expect("run");
    let capture = capture.lock().expect("capture").clone();
    harness.log().info(
        "verify",
        format!(
            "thinking: {:?} prompt has directive: {}",
            capture.thinking,
            capture.system_prompts[0]
                .as_ref()
                .is_some_and(|p| p.contains("for this turn"))
        ),
    );
    assert_eq!(
        capture.thinking.first(),
        Some(&None),
        "disabled ultrathink must preserve the unset baseline: {:?}",
        capture.thinking
    );
    let prompt = capture.system_prompts[0].clone().expect("prompt");
    assert!(
        !prompt.contains("for this turn"),
        "disabled orchestrate must not inject: {prompt}"
    );
    finish_case(&harness, case);
}

#[test]
fn code_and_paths_leave_request_untouched() {
    let case = "code_and_paths_leave_request_untouched";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    agent.set_keyword_max_thinking_level(ThinkingLevel::High);

    block_on_local(agent.run(
        "see `ultrathink`, /tmp/orchestrate, <think>workflowz</think>, and https://example.test/?ultrathink",
        |_| {},
    ))
    .expect("run");
    let capture = capture.lock().expect("capture").clone();
    assert_eq!(
        capture.thinking.first(),
        Some(&None),
        "excluded ultrathink occurrences must preserve the unset baseline: {:?}",
        capture.thinking
    );
    let prompt = capture.system_prompts[0].clone().expect("prompt");
    assert!(
        !prompt.contains("for this turn"),
        "path/XML keywords must not inject: {prompt}"
    );
    finish_case(&harness, case);
}

#[test]
fn block_keyword_activation_persists_in_session_custom_entry() {
    let case = "block_keyword_activation_persists_in_session_custom_entry";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (agent, _capture) = build_agent(&root, None);
    let session = Arc::new(AsyncMutex::new(Session::create_with_dir(Some(
        harness.temp_path("sessions"),
    ))));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        true,
        ResolvedCompactionSettings::default(),
    );

    block_on_local(agent_session.run_with_content(
        vec![ContentBlock::Text(TextContent::new(
            "ultrathink this".to_string(),
        ))],
        |_| {},
    ))
    .expect("run block content through session wrapper");

    let guard = session.try_lock().expect("session lock");
    let telemetry = guard
        .entries_for_current_path()
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == "magic_keyword" => {
                custom.data.as_ref()
            }
            _ => None,
        })
        .expect("magic keyword Custom telemetry entry");
    harness
        .log()
        .info("verify", format!("telemetry: {telemetry}"));
    assert_eq!(telemetry["schema"], json!("pi.magic_keyword.v1"));
    assert_eq!(telemetry["word"], json!("ultrathink"));
    assert_eq!(telemetry["action"], json!("ultrathink"));
    let persisted_path = guard.path.clone().expect("autosave created session file");
    drop(guard);

    let reopened = block_on_local(Session::open(persisted_path.to_string_lossy().as_ref()))
        .expect("reopen autosaved session");
    assert!(
        reopened
            .entries_for_current_path()
            .into_iter()
            .any(|entry| {
                matches!(
                    entry,
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "magic_keyword"
                            && custom.data.as_ref().is_some_and(|data| {
                                data["schema"] == json!("pi.magic_keyword.v1")
                                    && data["word"] == json!("ultrathink")
                                    && data["action"] == json!("ultrathink")
                            })
                )
            })
    );
    finish_case(&harness, case);
}

#[test]
#[allow(clippy::too_many_lines)]
fn rpc_prompt_observes_clamped_thinking_directive_and_telemetry() {
    let case = "rpc_prompt_observes_clamped_thinking_directive_and_telemetry";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (agent, capture) = build_agent(&root, None);

    let session = Arc::new(AsyncMutex::new(Session::in_memory()));
    {
        let mut guard = session.try_lock().expect("session header lock");
        guard.header.provider = Some("capture".to_string());
        guard.header.model_id = Some("capture-model".to_string());
        guard.header.thinking_level = Some("medium".to_string());
    }
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        false,
        ResolvedCompactionSettings::default(),
    );
    let auth_dir = tempfile::tempdir().expect("auth tempdir");
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).expect("auth storage");
    let (registry, expected_max) = capture_model_registry(&auth);
    agent_session.set_model_registry(registry);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("RPC test runtime");
    let handle = runtime.handle();

    runtime.block_on(async {
        let options = RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            cli_api_key: None,
            auth,
            runtime_handle: handle.clone(),
            ask_tool: None,
        };
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(256);
        let out_rx = Arc::new(Mutex::new(out_rx));
        let server =
            handle.spawn(async move { run_rpc(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"prompt","message":"please ultrathink and orchestrate this"}"#
                    .to_string(),
            )
            .await
            .expect("send RPC prompt");

        let ack: serde_json::Value = serde_json::from_str(
            recv_rpc_line(&out_rx, "RPC prompt acknowledgment")
                .await
                .expect("receive RPC prompt acknowledgment")
                .trim(),
        )
        .expect("parse RPC prompt acknowledgment");
        assert_eq!(ack["type"], "response");
        assert_eq!(ack["command"], "prompt");
        assert!(ack["success"].as_bool().unwrap_or(false));

        let mut saw_agent_end = false;
        for _ in 0..100 {
            let event: serde_json::Value = serde_json::from_str(
                recv_rpc_line(&out_rx, "RPC magic-keyword event")
                    .await
                    .expect("receive RPC magic-keyword event")
                    .trim(),
            )
            .expect("parse RPC event");
            if event["type"] == "agent_end" {
                assert!(
                    event["error"].is_null(),
                    "RPC agent turn ended with an error: {event}"
                );
                saw_agent_end = true;
                break;
            }
        }
        assert!(saw_agent_end, "RPC prompt never reached agent_end");
        drop(in_tx);
        server.await.expect("RPC server task join");
    });

    let captured = capture.lock().expect("capture").clone();
    assert_eq!(captured.thinking.first(), Some(&Some(expected_max)));
    assert!(
        captured.system_prompts[0]
            .as_deref()
            .is_some_and(|prompt| prompt.contains("`orchestrate` for this turn")),
        "RPC outbound request must contain the orchestration directive"
    );
    let guard = session.try_lock().expect("session telemetry lock");
    assert!(guard.entries_for_current_path().into_iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::Custom(custom)
                if custom.custom_type == "magic_keyword"
                    && custom.data.as_ref().is_some_and(|data| {
                        data["schema"] == json!("pi.magic_keyword.v1")
                            && data["word"] == json!("ultrathink")
                    })
        )
    }));
    drop(guard);
    finish_case(&harness, case);
}

#[test]
#[allow(clippy::too_many_lines)]
fn rpc_queued_steering_and_follow_up_keyword_provenance() {
    let case = "rpc_queued_steering_and_follow_up_keyword_provenance";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (first_call_entered, mut wait_for_first_call) =
        asupersync::channel::oneshot::channel::<()>();
    let (release_first_call, first_call_gate) =
        asupersync::channel::oneshot::channel::<()>();
    let (agent, capture) = build_gated_agent(&root, first_call_entered, first_call_gate);

    let session = Arc::new(AsyncMutex::new(Session::in_memory()));
    {
        let mut guard = session.try_lock().expect("session header lock");
        guard.header.provider = Some("capture".to_string());
        guard.header.model_id = Some("capture-model".to_string());
        guard.header.thinking_level = Some("off".to_string());
    }
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        false,
        ResolvedCompactionSettings::default(),
    );
    let auth_dir = tempfile::tempdir().expect("auth tempdir");
    let auth = AuthStorage::load(auth_dir.path().join("auth.json")).expect("auth storage");
    let (registry, expected_max) = capture_model_registry(&auth);
    agent_session.set_model_registry(registry);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("RPC test runtime");
    let handle = runtime.handle();

    runtime.block_on(async {
        let options = RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            cli_api_key: None,
            auth,
            runtime_handle: handle.clone(),
            ask_tool: None,
        };
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(256);
        let out_rx = Arc::new(Mutex::new(out_rx));
        let server =
            handle.spawn(async move { run_rpc(agent_session, options, in_rx, out_tx).await });
        let cx = asupersync::Cx::for_testing();

        // Keep prompt 1 inside the provider while both queued messages arrive.
        in_tx
            .send(
                &cx,
                r#"{"id":"1","type":"prompt","message":"turn 1 normal"}"#.to_string(),
            )
            .await
            .expect("send RPC prompt 1");

        let ack1: serde_json::Value = serde_json::from_str(
            recv_rpc_line(&out_rx, "RPC prompt 1 acknowledgment")
                .await
                .expect("receive RPC prompt 1 acknowledgment")
                .trim(),
        )
        .expect("parse ack 1");
        assert_eq!(ack1["type"], "response");
        assert_eq!(ack1["command"], "prompt");
        assert_eq!(ack1["success"], true);

        let entered_cx = asupersync::Cx::for_testing();
        wait_for_first_call
            .recv(&entered_cx)
            .await
            .expect("first provider call entered");

        // Queue steering while prompt 1 is provably still streaming.
        in_tx
            .send(
                &cx,
                r#"{"id":"2","type":"prompt","message":"please ultrathink and workflowz this steering input","streamingBehavior":"steer"}"#.to_string(),
            )
            .await
            .expect("send RPC steer");

        let ack2: serde_json::Value = serde_json::from_str(
            recv_rpc_line(&out_rx, "RPC steer acknowledgment")
                .await
                .expect("receive RPC steer acknowledgment")
                .trim(),
        )
        .expect("parse ack 2");
        assert_eq!(ack2["type"], "response");
        assert_eq!(ack2["command"], "prompt");
        assert_eq!(ack2["success"], true);

        // Queue a distinct follow-up before releasing the first provider call.
        in_tx
            .send(
                &cx,
                r#"{"id":"3","type":"follow_up","message":"please orchestrate this follow-up"}"#.to_string(),
            )
            .await
            .expect("send RPC follow-up");

        let ack3: serde_json::Value = serde_json::from_str(
            recv_rpc_line(&out_rx, "RPC follow-up acknowledgment")
                .await
                .expect("receive RPC follow-up acknowledgment")
                .trim(),
        )
        .expect("parse ack 3");
        assert_eq!(ack3["type"], "response");
        assert_eq!(ack3["command"], "follow_up");
        assert_eq!(ack3["success"], true);

        let release_cx = asupersync::Cx::for_testing();
        release_first_call
            .send(&release_cx, ())
            .expect("release first provider call");

        let mut saw_agent_end = false;
        for _ in 0..100 {
            let event: serde_json::Value = serde_json::from_str(
                recv_rpc_line(&out_rx, "RPC queued turn event")
                    .await
                    .expect("receive RPC queued turn event")
                    .trim(),
            )
            .expect("parse queued turn event");
            if event["type"] == "agent_end" {
                assert!(
                    event["error"].is_null(),
                    "RPC queued turn ended with an error: {event}"
                );
                saw_agent_end = true;
                break;
            }
        }
        assert!(saw_agent_end, "queued steering/follow-up turn completed");

        drop(in_tx);
        server.await.expect("RPC server task join");
    });

    let captured = capture.lock().expect("capture").clone();
    assert_eq!(captured.thinking.len(), 3);
    assert_eq!(captured.thinking[0], Some(ThinkingLevel::Off));
    assert_eq!(captured.thinking.get(1), Some(&Some(expected_max)));
    assert_eq!(captured.thinking[2], Some(ThinkingLevel::Off));
    assert!(
        captured.system_prompts[1]
            .as_deref()
            .is_some_and(|prompt| prompt.contains("`workflowz` for this turn")),
        "RPC turn 2 must observe queued steering workflowz directive"
    );
    assert!(
        captured.system_prompts[2]
            .as_deref()
            .is_some_and(|prompt| prompt.contains("`orchestrate` for this turn")),
        "RPC turn 3 must observe the queued follow-up orchestrate directive"
    );
    assert!(
        captured.system_prompts[1]
            .as_deref()
            .is_none_or(|prompt| !prompt.contains("`orchestrate` for this turn")),
        "follow-up directives must not leak into the steering request"
    );

    let guard = session.try_lock().expect("session telemetry lock");
    let activation_words = guard
        .entries_for_current_path()
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == "magic_keyword" => custom
                .data
                .as_ref()
                .and_then(|data| data["word"].as_str())
                .map(ToString::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(activation_words.iter().any(|word| word == "ultrathink"));
    assert!(activation_words.iter().any(|word| word == "workflowz"));
    assert!(activation_words.iter().any(|word| word == "orchestrate"));
    drop(guard);

    finish_case(&harness, case);
}
