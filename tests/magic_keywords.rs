//! Integration tests for magic keywords (bd-cv653.3.6).
//!
//! Acceptance coverage:
//! 1. `please ultrathink this design` → the outbound request runs at
//!    `ThinkingLevel::Max` (clamped downstream), proven via a capture
//!    provider.
//! 2. `orchestrate`/`workflowz` inject the correct directive exactly once per
//!    turn (system prompt contains it once).
//! 3. Settings disable each keyword (no level change, no directive injected).
//! 4. Code/fence/XML/path occurrences leave the request untouched.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::agent::{Agent, AgentConfig};
use pi::model::{StreamEvent, ThinkingLevel};
use pi::provider::{Context, StreamOptions};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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

/// Records the options + system prompt of every request, then streams a
/// one-line assistant reply.
#[derive(Default, Clone)]
struct Capture {
    thinking: Vec<Option<ThinkingLevel>>,
    system_prompts: Vec<Option<String>>,
}

struct CaptureProvider {
    capture: Arc<Mutex<Capture>>,
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
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                content_index: 0,
                delta: "done".to_string(),
            }),
            Ok(StreamEvent::TextEnd {
                content_index: 0,
                content: "done".to_string(),
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
    });
    let tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    let config = AgentConfig {
        system_prompt: Some("base prompt".to_string()),
        keyword_settings: keywords,
        ..AgentConfig::default()
    };
    (Agent::new(provider, tools, config), capture)
}

#[test]
fn ultrathink_raises_turn_to_max() {
    let case = "ultrathink_raises_turn_to_max";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    block_on_local(agent.run("please ultrathink this design", |_| {})).expect("run");
    let capture = capture.lock().expect("capture").clone();
    harness.log().info(
        "verify",
        format!("captured thinking levels: {:?}", capture.thinking),
    );
    assert_eq!(
        capture.thinking.first(),
        Some(&Some(ThinkingLevel::Max)),
        "ultrathink must raise the turn to max: {:?}",
        capture.thinking
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

    block_on_local(agent.run("ultrathink and orchestrate this", |_| {})).expect("run");
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
    assert!(
        !matches!(capture.thinking.first(), Some(&Some(ThinkingLevel::Max))),
        "disabled ultrathink must not raise thinking: {:?}",
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

    block_on_local(agent.run(
        "see `ultrathink` and /tmp/orchestrate plus <think>workflowz</think>",
        |_| {},
    ))
    .expect("run");
    let capture = capture.lock().expect("capture").clone();
    assert!(
        !matches!(capture.thinking.first(), Some(&Some(ThinkingLevel::Max))),
        "code-span ultrathink must not raise: {:?}",
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
fn keyword_activation_lands_in_ledger() {
    let case = "keyword_activation_lands_in_ledger";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, _capture) = build_agent(&root, None);

    block_on_local(agent.run("ultrathink this", |_| {})).expect("run");
    let activations = agent.drain_keyword_ledger();
    harness
        .log()
        .info("verify", format!("activations: {activations:?}"));
    assert_eq!(activations.len(), 1);
    assert_eq!(activations[0].word, "ultrathink");
    assert_eq!(activations[0].action, "ultrathink");
    // Drained: a second drain is empty.
    assert!(agent.drain_keyword_ledger().is_empty());
    let _ = json!({"schema": "pi.magic_keyword.v1"});
    finish_case(&harness, case);
}
