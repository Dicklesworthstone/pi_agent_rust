// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; asupersync 0.5.0 nests its runtime future types deeply
// enough that proving `Send` exceeds the default 128.
#![recursion_limit = "256"]

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
use pi::models::{CompatConfig, ModelEntry, ModelRegistry};
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
    recv_rpc_line_before(rx, label, Instant::now() + Duration::from_secs(10)).await
}

async fn recv_rpc_line_before(
    rx: &Arc<Mutex<Receiver<String>>>,
    label: &str,
    deadline: Instant,
) -> Result<String, String> {
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{label}: timed out waiting for RPC output"));
        }
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
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

async fn recv_rpc_command_response(
    rx: &Arc<Mutex<Receiver<String>>>,
    expected_id: &str,
    label: &str,
    observed_events: &mut Vec<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let line = recv_rpc_line_before(rx, label, deadline).await?;
        let value = serde_json::from_str::<serde_json::Value>(line.trim()).map_err(|err| {
            format!("{label}: invalid RPC JSON while waiting for response id {expected_id}: {err}")
        })?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some("response") {
            if value.get("id").and_then(serde_json::Value::as_str) == Some(expected_id) {
                return Ok(value);
            }
            return Err(format!(
                "{label}: unexpected RPC response while waiting for id {expected_id}: {value}"
            ));
        }
        observed_events.push(value);
    }
}

/// Records the options + system prompt of every request, then streams a
/// one-line assistant reply.
#[derive(Default, Clone)]
struct Capture {
    thinking: Vec<Option<ThinkingLevel>>,
    system_prompts: Vec<Option<String>>,
    messages: Vec<Vec<Message>>,
}

struct CaptureProvider {
    capture: Arc<Mutex<Capture>>,
    first_call_entered: Mutex<Option<asupersync::channel::oneshot::Sender<()>>>,
    first_call_gate: Mutex<Option<asupersync::channel::oneshot::Receiver<()>>>,
    fail_first: bool,
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
        let first_call = {
            let mut capture = self.capture.lock().expect("capture");
            capture.thinking.push(options.thinking_level);
            capture
                .system_prompts
                .push(context.system_prompt.as_ref().map(ToString::to_string));
            capture.messages.push(context.messages.to_vec());
            capture.thinking.len() == 1
        };
        if self.fail_first && first_call {
            return Err(pi::error::Error::api("injected first-request failure"));
        }
        let first_call_entered = self
            .first_call_entered
            .lock()
            .expect("first call entered signal")
            .take();
        let first_call_gate = self.first_call_gate.lock().expect("first call gate").take();
        if let Some(entered) = first_call_entered {
            let cx = asupersync::Cx::for_testing();
            entered.send(&cx, ()).expect("signal first provider call");
        }
        if let Some(mut gate) = first_call_gate {
            let cx = asupersync::Cx::for_testing();
            gate.recv(&cx).await.expect("release first provider call");
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
    build_agent_with_tools(root, keywords, &[])
}

fn build_agent_with_tools(
    root: &Path,
    keywords: Option<pi::magic_keywords::KeywordSettings>,
    enabled_tools: &[&str],
) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
        first_call_entered: Mutex::new(None),
        first_call_gate: Mutex::new(None),
        fail_first: false,
    });
    let tools = ToolRegistry::new(enabled_tools, root, None::<&pi::config::Config>);
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
        fail_first: false,
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
                audio: None,
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

fn adaptive_capture_model_entry() -> ModelEntry {
    let mut entry = capture_model_entry();
    entry.model.api = "anthropic-messages".to_string();
    entry.compat = Some(CompatConfig {
        force_adaptive_thinking: Some(true),
        ..CompatConfig::default()
    });
    entry
}

fn build_failing_adaptive_agent(root: &Path) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
        first_call_entered: Mutex::new(None),
        first_call_gate: Mutex::new(None),
        fail_first: true,
    });
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(&[], root, None::<&Config>),
        AgentConfig {
            stream_options: StreamOptions {
                thinking_level: Some(ThinkingLevel::High),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    agent.set_keyword_model(&adaptive_capture_model_entry());
    (agent, capture)
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
fn chillax_persists_lowered_effort_across_resume_until_changed() {
    let case = "chillax_persists_lowered_effort_across_resume_until_changed";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    agent.set_keyword_model(&adaptive_capture_model_entry());
    agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    let session = Arc::new(AsyncMutex::new(Session::create_with_dir(Some(
        harness.temp_path("sessions"),
    ))));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        true,
        ResolvedCompactionSettings::default(),
    );
    block_on_local(agent_session.run_text("chillax, check this change".to_string(), |_| {}))
        .expect("initial turn");
    assert_eq!(
        agent_session.agent.stream_options().thinking_level,
        Some(ThinkingLevel::Minimal)
    );
    assert_eq!(
        capture.lock().expect("capture").thinking,
        [Some(ThinkingLevel::Minimal)]
    );
    let path = session
        .try_lock()
        .expect("session")
        .path
        .clone()
        .expect("saved session");
    let reopened = block_on_local(Session::open(path.to_string_lossy().as_ref())).expect("reopen");
    assert_eq!(
        reopened
            .effective_thinking_level_for_current_path()
            .as_deref(),
        Some("minimal")
    );
    let telemetry = reopened
        .entries_for_current_path()
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == "magic_keyword" => {
                custom.data.as_ref()
            }
            _ => None,
        })
        .expect("activation telemetry");
    assert_eq!(telemetry["thinkingLevel"], json!("minimal"));

    let (mut resumed_agent, resumed_capture) = build_agent(&root, None);
    resumed_agent.set_keyword_model(&adaptive_capture_model_entry());
    resumed_agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    let mut resumed = AgentSession::new(
        resumed_agent,
        Arc::new(AsyncMutex::new(reopened)),
        true,
        ResolvedCompactionSettings::default(),
    );
    block_on_local(async {
        resumed
            .run_text("check the next change".to_string(), |_| {})
            .await
            .expect("resumed turn");
        resumed
            .set_thinking_level(ThinkingLevel::High)
            .await
            .expect("explicit thinking change");
        resumed
            .run_text("check another change".to_string(), |_| {})
            .await
            .expect("turn after manual change");
    });
    let captured = resumed_capture.lock().expect("capture");
    assert_eq!(
        captured.thinking,
        [Some(ThinkingLevel::Minimal), Some(ThinkingLevel::High)]
    );
    assert!(captured.system_prompts.iter().all(|prompt| {
        !prompt
            .as_deref()
            .unwrap_or_default()
            .contains("invoked `chillax`")
    }));
    finish_case(&harness, case);
}

#[test]
fn chillax_never_enables_thinking_or_guesses_transport_capabilities() {
    let case = "chillax_never_enables_thinking_or_guesses_transport_capabilities";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    for baseline in [None, Some(ThinkingLevel::Off), Some(ThinkingLevel::Minimal)] {
        for input in ["ultrathink then chillax", "chillax then ultrathink"] {
            let (mut agent, capture) = build_agent(&root, None);
            agent.set_keyword_model(&adaptive_capture_model_entry());
            agent.stream_options_mut().thinking_level = baseline;
            block_on_local(agent.run(input, |_| {})).expect("run");
            assert_eq!(capture.lock().expect("capture").thinking, [baseline]);
            assert_eq!(agent.stream_options().thinking_level, baseline);
        }
    }
    let (mut agent, capture) = build_agent(&root, None);
    agent.set_keyword_model(&capture_model_entry());
    agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    block_on_local(agent.run("chillax this time", |_| {})).expect("run");
    let captured = capture.lock().expect("capture");
    assert_eq!(captured.thinking, [Some(ThinkingLevel::High)]);
    assert!(
        captured.system_prompts[0]
            .as_deref()
            .expect("prompt")
            .contains("invoked `chillax`")
    );

    // Raw Agent callers can supply a disabled baseline that AgentSession
    // would normally clamp. Do not turn an omitted wire effort into max.
    let (mut raw, raw_capture) = build_agent(&root, None);
    let mut entry = adaptive_capture_model_entry();
    entry.compat.as_mut().expect("compat").thinking_level_map = Some(
        ["minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .map(|level| {
                (
                    level.to_string(),
                    (level == "minimal").then(|| "max".to_string()),
                )
            })
            .collect(),
    );
    raw.set_keyword_model(&entry);
    raw.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    block_on_local(raw.run("chillax", |_| {})).expect("unclamped raw Agent");
    assert_eq!(
        raw_capture.lock().expect("capture").thinking,
        [Some(ThinkingLevel::High)]
    );
    finish_case(&harness, case);
}

#[test]
fn chillax_steering_wins_this_turn_and_follow_up_can_ultrathink() {
    let case = "chillax_steering_wins_this_turn_and_follow_up_can_ultrathink";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    agent.set_keyword_model(&adaptive_capture_model_entry());
    agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    let message = |text: &str| {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    };
    agent.queue_steering(message("chillax before continuing"));
    agent.queue_follow_up(message("ultrathink on the next task"));
    block_on_local(agent.run("ultrathink on the first task", |_| {})).expect("run");
    let captured = capture.lock().expect("capture");
    assert_eq!(
        captured.thinking,
        [Some(ThinkingLevel::Minimal), Some(ThinkingLevel::High)]
    );
    assert_eq!(
        agent.stream_options().thinking_level,
        Some(ThinkingLevel::Minimal)
    );
    assert!(
        captured.system_prompts[0]
            .as_deref()
            .expect("prompt")
            .contains("invoked `chillax`")
    );
    assert!(
        !captured.system_prompts[1]
            .as_deref()
            .expect("prompt")
            .contains("invoked `chillax`")
    );
    assert_eq!(
        agent
            .drain_keyword_ledger()
            .iter()
            .map(|hit| hit.word.as_str())
            .collect::<Vec<_>>(),
        ["ultrathink", "chillax", "ultrathink"]
    );
    finish_case(&harness, case);
}

#[test]
fn jevify_requires_eval_and_does_not_rescan_generated_text() {
    let case = "jevify_requires_eval_and_does_not_rescan_generated_text";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    block_on_local(agent.run("jevify this list", |_| {})).expect("run without eval");
    assert!(agent.drain_keyword_ledger().is_empty());
    assert_eq!(
        capture.lock().expect("capture").system_prompts,
        [Some("base prompt".to_string())]
    );

    let (mut agent, capture) = build_agent_with_tools(&root, None, &["eval"]);
    agent.set_keyword_model(&adaptive_capture_model_entry());
    agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
    agent.set_magic_keyword_scan_override(Some(String::new()));
    block_on_local(agent.run("chillax and jevify in generated content", |_| {}))
        .expect("generated content");
    assert!(agent.drain_keyword_ledger().is_empty());
    block_on_local(agent.run("jevify the authored request", |_| {})).expect("authored content");
    let captured = capture.lock().expect("capture");
    assert_eq!(captured.thinking, [Some(ThinkingLevel::High); 2]);
    assert_eq!(captured.system_prompts[0].as_deref(), Some("base prompt"));
    let prompt = captured.system_prompts[1].as_deref().expect("prompt");
    assert!(prompt.contains("judgeBatch()") && prompt.contains("judge_batch()"));
    assert!(prompt.contains("bounded batches") && prompt.contains("unresolved failures"));
    assert_eq!(agent.drain_keyword_ledger()[0].word, "jevify");
    finish_case(&harness, case);
}

#[test]
fn chillax_retry_uses_refreshed_capabilities_without_duplicate_activation() {
    let case = "chillax_retry_uses_refreshed_capabilities_without_duplicate_activation";
    let harness = TestHarness::new(case);
    let (mut agent, capture) = build_failing_adaptive_agent(&harness.temp_path("."));
    let mut entry = adaptive_capture_model_entry();
    entry.compat.as_mut().expect("compat").thinking_level_map = Some(HashMap::from([
        ("minimal".to_string(), None),
        ("low".to_string(), None),
    ]));
    agent.set_keyword_model(&entry);
    let session = Arc::new(AsyncMutex::new(Session::in_memory()));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        false,
        ResolvedCompactionSettings::default(),
    );
    let result =
        block_on_local(agent_session.run_text("chillax for this task".to_string(), |_| {}));
    assert!(result.is_err() || result.is_ok_and(|reply| reply.stop_reason == StopReason::Error));
    block_on_local(agent_session.revert_incomplete_response()).expect("revert incomplete output");
    agent_session
        .agent
        .set_keyword_model(&adaptive_capture_model_entry());
    block_on_local(agent_session.run_continue_with_abort(None, |_| {})).expect("retry");
    assert_eq!(
        capture.lock().expect("capture").thinking,
        [Some(ThinkingLevel::Medium), Some(ThinkingLevel::Minimal)]
    );
    let stored = session.try_lock().expect("session");
    assert_eq!(
        stored
            .effective_thinking_level_for_current_path()
            .as_deref(),
        Some("minimal")
    );
    let activation_count = stored.entries_for_current_path().into_iter().filter(|entry| {
        matches!(entry, SessionEntry::Custom(custom) if custom.custom_type == "magic_keyword")
    }).count();
    assert_eq!(
        activation_count, 1,
        "retry must not duplicate activation telemetry"
    );
    finish_case(&harness, case);
}

#[test]
fn explicit_thinking_change_supersedes_failed_turn_keywords_on_retry() {
    let case = "explicit_thinking_change_supersedes_failed_turn_keywords_on_retry";
    let harness = TestHarness::new(case);
    let (agent, capture) = build_failing_adaptive_agent(&harness.temp_path("."));
    let session = Arc::new(AsyncMutex::new(Session::in_memory()));
    let mut agent_session =
        AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
    let result =
        block_on_local(agent_session.run_text("ultrathink then chillax".to_string(), |_| {}));
    assert!(result.is_err() || result.is_ok_and(|reply| reply.stop_reason == StopReason::Error));
    block_on_local(async {
        agent_session
            .revert_incomplete_response()
            .await
            .expect("revert");
        agent_session
            .set_thinking_level(ThinkingLevel::Low)
            .await
            .expect("manual setting");
        agent_session
            .run_continue_with_abort(None, |_| {})
            .await
            .expect("retry");
    });
    assert_eq!(
        capture.lock().expect("capture").thinking,
        [Some(ThinkingLevel::Minimal), Some(ThinkingLevel::Low)]
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
        let scenario = async {
            let options = RpcOptions {
                config: Config::default(),
                working_directory: None,
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
            let server = handle.spawn(async move {
                Box::pin(run_rpc(agent_session, options, in_rx, out_tx)).await
            });
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
        };
        asupersync::time::timeout(
            asupersync::time::wall_now(),
            Duration::from_secs(15),
            scenario,
        )
        .await
        .expect("RPC scenario exceeded 15 seconds");
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
    let (release_first_call, first_call_gate) = asupersync::channel::oneshot::channel::<()>();
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
        let scenario = async {
            let options = RpcOptions {
                config: Config::default(),
                working_directory: None,
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
                handle.spawn(async move { Box::pin(run_rpc(agent_session, options, in_rx, out_tx)).await });
            let cx = asupersync::Cx::for_testing();
            let mut observed_events = Vec::new();

            // Keep prompt 1 inside the provider while both queued messages arrive.
            in_tx
                .send(
                    &cx,
                    r#"{"id":"1","type":"prompt","message":"turn 1 normal"}"#.to_string(),
                )
                .await
                .expect("send RPC prompt 1");

            let ack1 = recv_rpc_command_response(
                &out_rx,
                "1",
                "RPC prompt 1 acknowledgment",
                &mut observed_events,
            )
            .await
            .expect("receive RPC prompt 1 acknowledgment");
            assert_eq!(ack1["type"], "response");
            assert_eq!(ack1["id"], "1");
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

            let ack2 = recv_rpc_command_response(
                &out_rx,
                "2",
                "RPC steer acknowledgment",
                &mut observed_events,
            )
            .await
            .expect("receive RPC steer acknowledgment");
            assert_eq!(ack2["type"], "response");
            assert_eq!(ack2["command"], "prompt");
            assert_eq!(ack2["success"], true);

            // Queue a distinct follow-up before releasing the first provider call.
            in_tx
                .send(
                    &cx,
                    r#"{"id":"3","type":"follow_up","message":"please workflowz and orchestrate this follow-up"}"#.to_string(),
                )
                .await
                .expect("send RPC follow-up");

            let ack3 = recv_rpc_command_response(
                &out_rx,
                "3",
                "RPC follow-up acknowledgment",
                &mut observed_events,
            )
            .await
            .expect("receive RPC follow-up acknowledgment");
            assert_eq!(ack3["type"], "response");
            assert_eq!(ack3["command"], "follow_up");
            assert_eq!(ack3["success"], true);

            let release_cx = asupersync::Cx::for_testing();
            release_first_call
                .send(&release_cx, ())
                .expect("release first provider call");

            let mut saw_agent_end = false;
            let event_deadline = Instant::now() + Duration::from_secs(10);
            for _ in 0..100 {
                let event: serde_json::Value = serde_json::from_str(
                    recv_rpc_line_before(&out_rx, "RPC queued turn event", event_deadline)
                        .await
                        .expect("receive RPC queued turn event")
                        .trim(),
                )
                .expect("parse queued turn event");
                observed_events.push(event.clone());
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
            server
                .await
                .expect("RPC server run");
            let remaining = out_rx
                .lock()
                .expect("RPC output lock")
                .try_iter()
                .collect::<Vec<_>>();
            for line in remaining {
                observed_events.push(
                    serde_json::from_str(line.trim()).expect("remaining RPC event JSON"),
                );
            }
            let agent_end_events = observed_events
                .iter()
                .filter(|event| event["type"] == "agent_end")
                .collect::<Vec<_>>();
            assert_eq!(agent_end_events.len(), 1, "unexpected lifecycle events");
            assert!(agent_end_events[0]["error"].is_null());
            assert!(
                observed_events.iter().all(|event| event["type"] != "response"),
                "unexpected or duplicate RPC response escaped exact-id validation"
            );
        };
        asupersync::time::timeout(
            asupersync::time::wall_now(),
            Duration::from_secs(15),
            scenario,
        )
        .await
        .expect("queued steering/follow-up RPC scenario exceeded 15 seconds");
    });

    let captured = capture.lock().expect("capture").clone();
    assert_eq!(captured.thinking.len(), 3);
    assert_eq!(captured.thinking[0], Some(ThinkingLevel::Off));
    assert_eq!(captured.thinking.get(1), Some(&Some(expected_max)));
    assert_eq!(captured.thinking[2], Some(ThinkingLevel::Off));
    let user_texts = |call: usize| {
        captured.messages[call]
            .iter()
            .filter_map(|message| match message {
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        user_texts(0),
        vec!["turn 1 normal"],
        "the initial request must contain exactly its authored prompt"
    );
    assert_eq!(
        user_texts(1),
        vec![
            "turn 1 normal",
            "please ultrathink and workflowz this steering input"
        ],
        "the steering request must append its message exactly once"
    );
    assert_eq!(
        user_texts(2),
        vec![
            "turn 1 normal",
            "please ultrathink and workflowz this steering input",
            "please workflowz and orchestrate this follow-up"
        ],
        "the follow-up request must preserve exact order without duplicates"
    );
    assert_eq!(
        captured.system_prompts[1]
            .as_deref()
            .map_or(0, |prompt| prompt
                .matches("`workflowz` for this turn")
                .count()),
        1,
        "RPC turn 2 must inject the steering workflowz directive exactly once"
    );
    assert_eq!(
        captured.system_prompts[2]
            .as_deref()
            .map_or(0, |prompt| prompt
                .matches("`workflowz` for this turn")
                .count()),
        1,
        "the repeated keyword must activate once in the new logical turn"
    );
    assert_eq!(
        captured.system_prompts[2]
            .as_deref()
            .map_or(0, |prompt| prompt
                .matches("`orchestrate` for this turn")
                .count()),
        1,
        "RPC turn 3 must inject its orchestrate directive exactly once"
    );
    assert_eq!(
        captured.system_prompts[1]
            .as_deref()
            .map_or(0, |prompt| prompt
                .matches("`orchestrate` for this turn")
                .count()),
        0,
        "follow-up directives must not leak into the steering request"
    );

    let guard = session.try_lock().expect("session telemetry lock");
    let activations = guard
        .entries_for_current_path()
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == "magic_keyword" => {
                custom.data.as_ref().map(|data| {
                    (
                        data["schema"].as_str().map(ToString::to_string),
                        data["word"].as_str().map(ToString::to_string),
                        data["action"].as_str().map(ToString::to_string),
                    )
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        activations,
        vec![
            (
                Some("pi.magic_keyword.v1".to_string()),
                Some("ultrathink".to_string()),
                Some("ultrathink".to_string()),
            ),
            (
                Some("pi.magic_keyword.v1".to_string()),
                Some("workflowz".to_string()),
                Some("workflowz".to_string()),
            ),
            (
                Some("pi.magic_keyword.v1".to_string()),
                Some("workflowz".to_string()),
                Some("workflowz".to_string()),
            ),
            (
                Some("pi.magic_keyword.v1".to_string()),
                Some("orchestrate".to_string()),
                Some("orchestrate".to_string()),
            ),
        ],
        "telemetry must contain exactly one ordered activation per authored keyword"
    );
    drop(guard);

    finish_case(&harness, case);
}
