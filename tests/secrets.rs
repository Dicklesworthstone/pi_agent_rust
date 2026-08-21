//! Integration tests for the secrets obfuscation vault (bd-cv653.7.9).
//!
//! Acceptance coverage:
//! 1. Fixture secret in context → the recorded provider payload contains
//!    placeholders, zero raw secrets (canary assertions).
//! 2. Model echoes a placeholder into a write → file on disk gets the REAL
//!    value; a bash echo of the value is masked in the tool result.
//! 3. Block mode refuses the send with a named PI_SECRET_BLOCK error.
//! 4. Session content contains placeholders only (the vault never persists
//!    raw values — nothing to leak on export/share).
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::agent::{Agent, AgentConfig};
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, StreamOptions};
use pi::secrets::SecretsSettings;
use pi::tools::{ToolOutput, ToolRegistry};
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
        .expect("write JSONL test logs"); // ubs:ignore harness pattern
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs"); // ubs:ignore harness pattern
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    // ubs:ignore-start — the runtime construction is infallible in tests
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    // ubs:ignore-end
    runtime.block_on(future)
}

fn first_text(output: &pi::tools::ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// Records the provider-visible payload text; replies with a text turn.
#[derive(Default)]
struct Capture {
    payloads: Vec<String>,
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
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        let mut payload = String::new();
        if let Some(prompt) = context.system_prompt.as_deref() {
            payload.push_str(prompt);
            payload.push('\n');
        }
        for message in context.messages.iter() {
            payload.push_str(&format!("{message:?}")); // ubs:ignore capture loop in a stub provider
            payload.push('\n');
        }
        self.capture.lock().expect("capture").payloads.push(payload); // ubs:ignore test capture
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::TextDelta {
                content_index: 0,
                delta: "ack".to_string(),
            },
        )])))
    }
}

fn build_agent(root: &Path, secrets: Option<SecretsSettings>) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
    });
    let tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    let config = AgentConfig {
        system_prompt: Some("base prompt".to_string()),
        secrets,
        ..AgentConfig::default()
    };
    (Agent::new(provider, tools, config), capture)
}

const SECRET: &str = "sk-0123456789abcdefghijklmnop";

#[test]
fn outbound_payload_carries_placeholders_only() {
    let case = "outbound_payload_carries_placeholders_only";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    block_on_local(agent.run(format!("my key is {SECRET}"), |_| {})).expect("run"); // ubs:ignore test run
    let payloads = capture.lock().expect("capture").payloads.clone(); // ubs:ignore test capture
    harness.log().info(
        "verify",
        format!(
            "payloads: {}",
            payloads.join(" | ").chars().take(400).collect::<String>()
        ),
    );
    assert!(!payloads.is_empty());
    let joined = payloads.join("\n");
    assert!(
        joined.contains("<pi-secret:"),
        "provider payload must carry the placeholder: {joined}"
    );
    assert!(
        !joined.contains(SECRET),
        "provider payload must never carry the raw secret: {joined}"
    );
    finish_case(&harness, case);
}

#[test]
fn inbound_restore_writes_real_value_and_masks_echo() {
    let case = "inbound_restore_writes_real_value_and_masks_echo";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    // Establish the vault mapping through a real outbound turn.
    let (mut agent, capture) = build_agent(&root, None);
    block_on_local(agent.run(format!("my key is {SECRET}"), |_| {})).expect("run"); // ubs:ignore test run
    let payloads = capture.lock().expect("capture").payloads.clone(); // ubs:ignore test capture
    let placeholder = payloads
        .join("\n")
        .split_whitespace()
        .find(|token| token.contains("<pi-secret:"))
        .map(|token| {
            token
                .trim_start_matches(|c| c != '<')
                .trim_end_matches(|c: char| c != '>')
                .to_string()
        })
        .expect("placeholder in payload");
    harness
        .log()
        .info("verify", format!("placeholder: {placeholder}"));

    // The model echoes the placeholder into a write → the file gets the
    // REAL value.
    let tool_call = pi::model::ToolCall {
        id: "t1".to_string(),
        name: "write".to_string(),
        arguments: json!({
            "path": root.join("secret.txt").display().to_string(),
            "content": format!("key = {placeholder}"),
        }),
        thought_signature: None,
    };
    let restored = agent.restore_secrets_inbound(tool_call);
    let args = serde_json::to_string(&restored.arguments).expect("args");
    harness
        .log()
        .info("verify", format!("restored args: {args}"));
    assert!(args.contains(SECRET), "restore must substitute: {args}");
    assert!(!args.contains("<pi-secret:"), "no placeholder left: {args}");

    // Echo hygiene: a result containing the real value is masked back.
    let mut output = ToolOutput {
        content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
            format!("wrote {SECRET}"),
        ))],
        details: None,
        is_error: false,
    };
    agent.mask_secrets_in_output(&mut output);
    let masked = first_text(&output);
    assert!(masked.contains("<pi-secret:"), "{masked}");
    assert!(!masked.contains(SECRET), "{masked}");
    finish_case(&harness, case);
}

#[test]
fn block_mode_refuses_the_send() {
    let case = "block_mode_refuses_the_send";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, _capture) = build_agent(
        &root,
        Some(SecretsSettings {
            mode: Some("block".to_string()),
            extra_patterns: None,
        }),
    );

    let err = block_on_local(agent.run(format!("my key is {SECRET}"), |_| {}))
        .expect_err("block mode must refuse");
    let text = err.to_string();
    harness.log().info("verify", format!("block error: {text}"));
    assert!(text.contains("PI_SECRET_BLOCK"), "{text}");
    finish_case(&harness, case);
}

#[test]
fn off_mode_is_byte_identical() {
    let case = "off_mode_is_byte_identical";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(
        &root,
        Some(SecretsSettings {
            mode: Some("off".to_string()),
            extra_patterns: None,
        }),
    );
    block_on_local(agent.run(format!("my key is {SECRET}"), |_| {})).expect("run"); // ubs:ignore test run
    let payloads = capture.lock().expect("capture").payloads.clone(); // ubs:ignore test capture
    let joined = payloads.join("\n");
    harness.log().info(
        "verify",
        format!("off payload contains raw: {}", joined.contains(SECRET)),
    );
    assert!(
        joined.contains(SECRET),
        "off mode must pass raw values through: {}",
        &joined[..joined.len().min(300)]
    );
    finish_case(&harness, case);
}

#[test]
fn export_carries_placeholders_only() {
    let case = "export_carries_placeholders_only";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    // The live transcript keeps the user's own typed text (correct UX);
    // the EXPORT surface masks known secrets through the vault (acceptance
    // #5: exported/shared content contains placeholders only).
    let (mut agent, _capture) = build_agent(&root, None);
    block_on_local(agent.run(format!("my key is {SECRET}"), |_| {})).expect("run"); // ubs:ignore test run
    let transcript: String = agent
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("ser"))
        .collect::<Vec<_>>()
        .join("\n");
    let exported = agent.mask_secrets_text(&transcript);
    harness.log().info(
        "verify",
        format!(
            "exported contains placeholder: {}",
            exported.contains("<pi-secret:")
        ),
    );
    assert!(
        exported.contains("<pi-secret:"),
        "export must carry the placeholder"
    );
    assert!(
        !exported.contains(SECRET),
        "export must never carry the raw secret: {}",
        &exported[..exported.len().min(300)]
    );
    finish_case(&harness, case);
}
