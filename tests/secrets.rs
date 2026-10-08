// Product and vendor names appear throughout these docs (Alibaba BaiLian,
// OpenAI and friends) and `doc_markdown` reads their capitalisation as
// un-backticked code items. Backticking a brand renders it as code, which is
// worse than the warning. Same allow that 30 other files in tests/ already
// carry.
#![allow(clippy::doc_markdown)]

//! Integration tests for the secrets obfuscation vault (bd-cv653.7.9).
//!
//! Acceptance coverage:
//! 1. Fixture secret in context → the recorded provider payload contains
//!    placeholders, zero raw secrets (canary assertions).
//! 2. Model echoes a placeholder into a write → file on disk gets the REAL
//!    value; a tool echo of the value is masked in outbound provider context.
//! 3. Block mode refuses the send with a named `PI_SECRET_BLOCK` error.
//! 4. Explicit export screening masks known secret values; the local user
//!    transcript is not claimed to be a redacted export.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::agent::{Agent, AgentConfig};
use pi::model::StreamEvent;
use pi::provider::{Context, StreamOptions};
use pi::secrets::SecretsSettings;
use pi::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
use serde_json::json;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    // ubs:ignore harness pattern (single-line chains keep the marker on the flagged line)
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
    runtime.block_on(Box::pin(future))
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

/// Records the provider-visible payload text and advertised tool names;
/// replies with a text turn.
#[derive(Default)]
struct Capture {
    payloads: Vec<String>,
    tools: Vec<String>,
    messages: Vec<Vec<pi::model::Message>>,
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
            use std::fmt::Write as _;
            let _ = write!(payload, "{message:?}"); // ubs:ignore capture loop in a stub provider
            payload.push('\n');
        }
        let mut capture = self.capture.lock().expect("capture"); // ubs:ignore test capture
        capture.payloads.push(payload);
        capture.messages.push(context.messages.to_vec());
        capture
            .tools
            .extend(context.tools.iter().map(|tool| tool.name.clone()));
        drop(capture);
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::TextDelta {
                content_index: 0,
                delta: "ack".to_string(),
            },
        )])))
    }
}

fn build_agent(root: &Path, secrets: Option<SecretsSettings>) -> (Agent, Arc<Mutex<Capture>>) {
    build_agent_with_tools(root, secrets, Vec::new())
}

fn build_agent_with_tools(
    root: &Path,
    secrets: Option<SecretsSettings>,
    extra_tools: Vec<Box<dyn Tool>>,
) -> (Agent, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(CaptureProvider {
        capture: Arc::clone(&capture),
    });
    let mut tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    tools.extend(extra_tools);
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

/// gh #211: a dotted OpenAI-compatible key (Alibaba BaiLian `sk-sp-…`)
/// must take the same path as a plain one — vaulted outbound, restored
/// inbound, re-masked in tool output — and the placeholder must survive a
/// second outbound pass unchanged (it is not itself credential-shaped).
#[test]
fn dotted_key_round_trips_through_the_agent() {
    const DOTTED: &str = "sk-sp-H.EEDDM.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV.wXyZ01234";
    let case = "dotted_key_round_trips_through_the_agent";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    // Two turns: the second carries the placeholder from the first back
    // through the outbound transform (assistant/user history is re-scanned
    // every turn).
    block_on_local(agent.run(format!("\"apiKey\": \"{DOTTED}\"."), |_| {})).expect("run"); // ubs:ignore test run
    block_on_local(agent.run("and again?", |_| {})).expect("run"); // ubs:ignore test run
    let payloads = capture.lock().expect("capture").payloads.clone(); // ubs:ignore test capture
    let joined = payloads.join("\n");
    harness.log().info(
        "verify",
        format!("payloads: {}", joined.chars().take(400).collect::<String>()),
    );
    assert_eq!(payloads.len(), 2);
    assert!(
        joined.contains("<pi-secret:000001>\\\"."),
        "trailing period must stay outside the placeholder: {joined}"
    );
    assert!(!joined.contains(DOTTED), "raw dotted key leaked: {joined}");
    assert!(
        !joined.contains("<pi-secret:000002>"),
        "placeholder must not be re-vaulted on the second turn: {joined}"
    );

    let restored = agent.restore_secrets_inbound(pi::model::ToolCall {
        id: "t1".to_string(),
        name: "bash".to_string(),
        arguments: json!({ "command": "curl -H 'Authorization: Bearer <pi-secret:000001>'" }),
        thought_signature: None,
    });
    let args = serde_json::to_string(&restored.arguments).expect("args");
    assert!(
        args.contains(DOTTED),
        "restore must substitute the dotted key: {args}"
    );

    let mut output = ToolOutput {
        content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
            format!("OPENAI_API_KEY={DOTTED}\n"),
        ))],
        details: None,
        is_error: false,
    };
    agent.mask_secrets_in_output(&mut output);
    let masked = first_text(&output);
    assert_eq!(masked, "OPENAI_API_KEY=<pi-secret:000001>\n", "{masked}");
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

const PEM_BODY: &str = "U1lOVEhFVElDLVBSSVZBVEUtS0VZLUJPRFktQ0FOQVJZ";

fn private_key_fixture() -> String {
    format!("-----BEGIN PRIVATE KEY-----\n{PEM_BODY}\n-----END PRIVATE KEY-----")
}

#[test]
fn complete_and_truncated_private_keys_protect_the_body_at_the_provider_boundary() {
    let case = "private_key_body_provider_boundary";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    for key in [
        private_key_fixture(),
        format!("-----BEGIN RSA PRIVATE KEY-----\n{PEM_BODY}"),
        format!(
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\r\nProc-Type: 4,ENCRYPTED\r\n{PEM_BODY}\r\n-----END ENCRYPTED PRIVATE KEY-----"
        ),
    ] {
        let (mut agent, capture) = build_agent(&root, None);
        block_on_local(agent.run(format!("inspect this key:\n{key}"), |_| {})).expect("run");
        let capture = capture.lock().expect("capture");
        assert_eq!(capture.payloads.len(), 1);
        assert!(capture.payloads[0].contains("<pi-secret:"));
        assert!(!capture.payloads[0].contains(PEM_BODY));
        assert!(!capture.payloads[0].contains("Proc-Type"));
        assert!(!capture.payloads[0].contains("-----END"));
        drop(capture);
    }
    finish_case(&harness, case);
}

/// Only the remote model is replaced here. The Agent's outbound transform,
/// inbound argument restoration, and actual write/read tools all execute.
struct PrivateKeyToolProvider {
    capture: Arc<Mutex<Capture>>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for PrivateKeyToolProvider {
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
        use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall};

        let payload = serde_json::to_string(context.messages.as_ref()).expect("provider payload");
        let step = {
            let mut capture = self.capture.lock().expect("capture");
            let step = capture.payloads.len();
            capture.payloads.push(payload.clone());
            step
        };
        let mut message = AssistantMessage {
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            ..AssistantMessage::default()
        };
        let call = match step {
            0 => {
                let start = payload.find("<pi-secret:").expect("outbound placeholder");
                let end = start + payload[start..].find('>').expect("placeholder end") + 1;
                Some((
                    "write",
                    json!({"path": "copied.pem", "content": &payload[start..end]}),
                ))
            }
            1 => Some(("read", json!({"path": "copied.pem"}))),
            2 => None,
            _ => panic!("unexpected extra provider request"),
        };
        if let Some((name, arguments)) = call {
            message.stop_reason = StopReason::ToolUse;
            message.content.push(ContentBlock::ToolCall(ToolCall {
                id: format!("key-tool-{step}"),
                name: name.to_string(),
                arguments,
                thought_signature: None,
            }));
        } else {
            message
                .content
                .push(ContentBlock::Text(TextContent::new("key copied")));
        }
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::Done {
                reason: message.stop_reason,
                message,
            },
        )])))
    }
}

#[test]
fn private_key_placeholder_executes_real_write_and_read_without_cloud_disclosure() {
    let case = "private_key_real_tool_round_trip";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(PrivateKeyToolProvider {
        capture: Arc::clone(&capture),
    });
    let tools = ToolRegistry::new(&["write", "read"], &root, None);
    let mut agent = Agent::new(provider, tools, AgentConfig::default());
    let key = private_key_fixture();
    let completed_tools = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&completed_tools);
    let result = block_on_local(agent.run(
        format!("Copy this private key, then read the copy:\n{key}"),
        move |event| {
            if let pi::agent::AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                ..
            } = event
            {
                recorded
                    .lock()
                    .expect("tool events")
                    .push((tool_name, is_error));
            }
        },
    ))
    .expect("real tool round trip");
    assert_eq!(result.stop_reason, pi::model::StopReason::Stop);
    assert_eq!(
        std::fs::read_to_string(root.join("copied.pem")).expect("written key"),
        key
    );
    assert_eq!(
        *completed_tools.lock().expect("tool events"),
        vec![("write".to_string(), false), ("read".to_string(), false)]
    );
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 3);
    for payload in &capture.payloads {
        assert!(payload.contains("<pi-secret:"));
        assert!(
            !payload.contains(PEM_BODY),
            "private body reached the provider"
        );
        assert!(!payload.contains("-----BEGIN"));
        assert!(!payload.contains("-----END"));
    }
    drop(capture);
    finish_case(&harness, case);
}

#[test]
fn truncated_private_key_block_mode_never_calls_the_provider() {
    let case = "private_key_block_before_provider";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(
        &root,
        Some(SecretsSettings {
            mode: Some("block".to_string()),
            extra_patterns: None,
        }),
    );
    let result =
        block_on_local(agent.run(format!("-----BEGIN PRIVATE KEY-----\n{PEM_BODY}"), |_| {}));
    let error = result
        .expect_err("block mode refuses before provider entry")
        .to_string();
    assert!(error.contains("PI_SECRET_BLOCK"));
    assert!(!error.contains(PEM_BODY));
    assert!(capture.lock().expect("capture").payloads.is_empty());
    finish_case(&harness, case);
}

#[test]
fn overlapping_custom_rules_cover_the_full_secret_in_real_agent_context() {
    let case = "overlapping_rules_provider_boundary";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(
        &root,
        Some(SecretsSettings {
            mode: Some("obfuscate".to_string()),
            extra_patterns: Some(vec![
                "abcde".to_string(),
                "defgh".to_string(),
                "ghij".to_string(),
            ]),
        }),
    );
    block_on_local(agent.run("safe abcdefghij safe", |_| {})).expect("run");
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 1);
    assert!(capture.payloads[0].contains("safe <pi-secret:000001> safe"));
    assert!(!capture.payloads[0].contains("fghij"));
    drop(capture);
    finish_case(&harness, case);
}

const OPAQUE_SECRET: &str = "hunter2hunter2hunter2";

#[test]
fn json_credentials_stay_protected_after_the_assignment_leaves_history() {
    let case = "remembered_credential_after_history_change";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    let input = json!({"password": OPAQUE_SECRET}).to_string();
    block_on_local(agent.run(input, |_| {})).expect("first turn");
    // Keep the session's vault, but remove the original KEY=value hint.
    // This tests the loss of context, not a synthetic second detector call.
    agent.clear_messages();
    block_on_local(agent.run(format!("echoed value: {OPAQUE_SECRET}"), |_| {}))
        .expect("later turn");
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 2);
    for payload in &capture.payloads {
        assert!(!payload.contains(OPAQUE_SECRET));
        assert!(payload.contains("<pi-secret:000001>"));
    }
    drop(capture);
    let side_context = agent
        .secrets_transform_outbound_text(&format!("side question quotes {OPAQUE_SECRET}"))
        .expect("auxiliary outbound screening");
    assert_eq!(side_context, "side question quotes <pi-secret:000001>");
    let call = agent.restore_secrets_inbound(pi::model::ToolCall {
        id: "remembered-value".to_string(),
        name: "write".to_string(),
        arguments: json!({"path": "key.txt", "content": "<pi-secret:000001>"}),
        thought_signature: None,
    });
    assert_eq!(call.arguments["content"], OPAQUE_SECRET);
    finish_case(&harness, case);
}

#[test]
fn a_bare_echo_before_its_first_assignment_does_not_leak_to_the_provider() {
    let case = "same_prompt_bare_echo_screening";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    block_on_local(agent.run(
        format!("earlier {OPAQUE_SECRET}; password={OPAQUE_SECRET}; later {OPAQUE_SECRET}"),
        |_| {},
    ))
    .expect("run");
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 1);
    assert!(!capture.payloads[0].contains(OPAQUE_SECRET));
    assert_eq!(capture.payloads[0].matches("<pi-secret:000001>").count(), 3);
    drop(capture);
    finish_case(&harness, case);
}

#[test]
fn multiline_credentials_are_masked_inside_nested_tool_result_details() {
    let case = "multiline_tool_result_details";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, _) = build_agent(&root, None);
    let key = private_key_fixture();
    block_on_local(agent.run(key.clone(), |_| {})).expect("establish vault");
    let mut output = ToolOutput {
        content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
            format!("copied:\n{key}"),
        ))],
        details: Some(json!({
            "credential": key,
            "nested": [{"echo": key, "safe": true}],
            "count": 7,
        })),
        is_error: false,
    };
    agent.mask_secrets_in_output(&mut output);
    assert_eq!(first_text(&output), "copied:\n<pi-secret:000001>");
    let details = output.details.as_ref().expect("details retained");
    assert_eq!(details["credential"], "<pi-secret:000001>");
    assert_eq!(details["nested"][0]["echo"], "<pi-secret:000001>");
    assert_eq!(details["nested"][0]["safe"], true);
    assert_eq!(details["count"], 7);
    assert!(!serde_json::to_string(details).unwrap().contains(PEM_BODY));
    finish_case(&harness, case);
}

#[test]
fn serialized_transcript_export_masks_multiline_keys_without_breaking_jsonl() {
    let case = "multiline_transcript_export";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let (mut agent, _) = build_agent(&root, None);
    block_on_local(agent.run(private_key_fixture(), |_| {})).expect("establish vault");
    let records = agent
        .messages()
        .iter()
        .map(|message| serde_json::to_string(message).expect("serialize local transcript"))
        .collect::<Vec<_>>();
    let original = format!("{}\r\n", records.join("\r\n"));
    assert!(
        original.contains(PEM_BODY),
        "local input is deliberately not an export"
    );
    let exported = agent.mask_secrets_text(&original);
    assert!(!exported.contains(PEM_BODY));
    assert!(exported.contains("<pi-secret:000001>"));
    assert!(exported.ends_with("\r\n"));
    let decoded = exported
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("screened record remains valid JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(decoded.len(), records.len());
    for (before, after) in records.iter().zip(&decoded) {
        let before: serde_json::Value = serde_json::from_str(before).unwrap();
        assert_eq!(before["role"], after["role"]);
    }
    assert_eq!(agent.mask_secrets_text(&exported), exported);
    finish_case(&harness, case);
}

#[test]
fn quoted_generic_credentials_obey_block_and_off_modes_at_provider_entry() {
    let case = "quoted_credential_mode_boundaries";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    for mode in ["block", "off"] {
        let (mut agent, capture) = build_agent(
            &root,
            Some(SecretsSettings {
                mode: Some(mode.to_string()),
                extra_patterns: None,
            }),
        );
        let result =
            block_on_local(agent.run(json!({"password": OPAQUE_SECRET}).to_string(), |_| {}));
        let capture = capture.lock().expect("capture");
        if mode == "block" {
            let error = result
                .expect_err("quoted keys must not bypass block mode")
                .to_string();
            assert!(error.contains("PI_SECRET_BLOCK"));
            assert!(!error.contains(OPAQUE_SECRET));
            assert!(capture.payloads.is_empty());
        } else {
            result.expect("off mode retains ordinary provider behavior");
            assert_eq!(capture.payloads.len(), 1);
            assert!(capture.payloads[0].contains(OPAQUE_SECRET));
        }
        drop(capture);
    }
    finish_case(&harness, case);
}

#[test]
fn structured_custom_and_tool_arguments_are_screened_request_wide() {
    const OPAQUE: &str = "opaqueCredentialValue1234567890";
    let harness =
        TestHarness::new("structured_custom_and_tool_arguments_are_screened_request_wide");
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    let custom = pi::model::Message::Custom(pi::model::CustomMessage {
        content: format!("earlier bare echo: {OPAQUE}"),
        custom_type: "fixture".to_string(),
        display: true,
        details: Some(json!({"api_key": OPAQUE, "ordinary": true})),
        timestamp: 0,
    });
    let assistant = pi::model::Message::Assistant(Arc::new(pi::model::AssistantMessage {
        content: vec![pi::model::ContentBlock::ToolCall(pi::model::ToolCall {
            id: "history-call".to_string(),
            name: "fixture".to_string(),
            arguments: json!({
                "echo": OPAQUE,
                "nested": {"api_key": OPAQUE},
            }),
            thought_signature: None,
        })],
        stop_reason: pi::model::StopReason::Stop,
        timestamp: 0,
        ..pi::model::AssistantMessage::default()
    }));

    block_on_local(agent.run_with_messages_with_abort(vec![custom, assistant], None, |_| {}))
        .expect("screened request should reach provider");

    let joined = capture.lock().expect("capture").payloads.join("\n");
    assert!(
        !joined.contains(OPAQUE),
        "opaque credential leaked: {joined}"
    );
    assert!(
        joined.matches("<pi-secret:").count() >= 4,
        "content, details and structured arguments should all be protected: {joined}"
    );

    let restored = agent.restore_secrets_inbound(pi::model::ToolCall {
        id: "restore".to_string(),
        name: "fixture".to_string(),
        arguments: json!({"value": "<pi-secret:000001>"}),
        thought_signature: None,
    });
    assert_eq!(restored.arguments["value"], OPAQUE);
}

fn user_text(text: &str) -> pi::model::Message {
    pi::model::Message::User(pi::model::UserMessage {
        content: pi::model::UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn assistant(
    content: Vec<pi::model::ContentBlock>,
    stop_reason: pi::model::StopReason,
) -> pi::model::Message {
    pi::model::Message::Assistant(Arc::new(pi::model::AssistantMessage {
        content,
        stop_reason,
        api: "capture-api".to_string(),
        provider: "capture".to_string(),
        model: "capture-model".to_string(),
        timestamp: 0,
        ..pi::model::AssistantMessage::default()
    }))
}

fn tool_call(
    id: &str,
    arguments: serde_json::Value,
    thought_signature: Option<&str>,
) -> pi::model::ContentBlock {
    pi::model::ContentBlock::ToolCall(pi::model::ToolCall {
        id: id.to_string(),
        name: "fixture".to_string(),
        arguments,
        thought_signature: thought_signature.map(ToString::to_string),
    })
}

fn block_mode() -> SecretsSettings {
    SecretsSettings {
        mode: Some("block".to_string()),
        extra_patterns: None,
    }
}

/// Advertised to the provider under a fixed name; never executed.
struct NamedTool(&'static str);

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }

    fn label(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "fixture tool that is advertised but never executed"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> pi::error::Result<ToolOutput> {
        Err(pi::error::Error::tool(
            self.0,
            "fixture tool is never executed",
        ))
    }
}

#[test]
fn late_refusal_rolls_back_the_entire_request_vault() {
    const EARLY: &str = "sk-aaaaaaaaaaaaaaaaaaaaaaaa";
    let harness = TestHarness::new("late_refusal_rolls_back_the_entire_request_vault");
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    // Numeric credentials from another provider cannot use the model-origin
    // replay exception. Refuse without changing their JSON type or learning
    // any earlier secret from this request into the live vault.
    let mut numeric = assistant(
        vec![tool_call(
            "numeric-call",
            json!({"token": 123_456_789_012_345_678_u64}),
            None,
        )],
        pi::model::StopReason::ToolUse,
    );
    let pi::model::Message::Assistant(message) = &mut numeric else {
        unreachable!()
    };
    Arc::make_mut(message).provider = "other-provider".to_string();
    let error = block_on_local(agent.run_with_messages_with_abort(
        vec![user_text(&format!("remember {EARLY}")), numeric],
        None,
        |_| {},
    ))
    .expect_err("a numeric credential must not change type");
    assert!(
        error.to_string().contains("PI_SECRET_JSON_PRIMITIVE"),
        "{error}"
    );
    assert!(
        capture.lock().expect("capture").payloads.is_empty(),
        "provider must not be invoked after a screening refusal"
    );

    // The refused request learned EARLY and the numeric value in its staged
    // vault. Neither may survive into the live session: the numeric value is
    // not remembered, and EARLY gets the first placeholder identity afresh.
    assert_eq!(
        agent
            .secrets_transform_outbound_text("123456789012345678")
            .expect("screen numeric value after rollback"),
        "123456789012345678"
    );
    assert_eq!(
        agent
            .secrets_transform_outbound_text(EARLY)
            .expect("screen early credential after rollback"),
        "<pi-secret:000001>"
    );
}

#[test]
fn unsigned_numeric_tool_arguments_survive_same_origin_continuations() {
    use pi::model::{ContentBlock, Message, StopReason, TextContent, ToolResultMessage};

    const NUMERIC: u64 = 123_456_789_012_345_678;
    let harness =
        TestHarness::new("unsigned_numeric_tool_arguments_survive_same_origin_continuations");
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);
    let mut arguments = json!({
        "token": NUMERIC,
        "nested": [true, false, null, {"echo": NUMERIC.to_string(), "api_key": SECRET}],
    });
    arguments
        .as_object_mut()
        .expect("arguments object")
        .insert(SECRET.to_string(), json!("ordinary"));
    let history = vec![
        user_text("page through the results"),
        assistant(
            vec![tool_call("unsigned-page", arguments.clone(), None)],
            StopReason::ToolUse,
        ),
        Message::tool_result(ToolResultMessage {
            tool_call_id: "unsigned-page".to_string(),
            tool_name: "fixture".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("page ready"))],
            details: None,
            is_error: false,
            timestamp: 0,
        }),
    ];
    let original = serde_json::to_value(&history).expect("original history");
    agent.replace_messages(history.clone());

    block_on_local(agent.run_continue_with_abort(None, |_| {}))
        .expect("unsigned numeric history must allow the continuation");
    block_on_local(agent.run("next page".to_string(), |_| {}))
        .expect("later prompts must also remain usable");

    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 2);
    for (payload, messages) in capture.payloads.iter().zip(&capture.messages) {
        let replayed = messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(message) => Some(&message.content),
                _ => None,
            })
            .flatten()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) if call.id == "unsigned-page" => Some(call),
                _ => None,
            })
            .expect("replayed unsigned tool call");
        assert!(replayed.thought_signature.is_none());
        assert_eq!(replayed.arguments["token"].as_u64(), Some(NUMERIC));
        assert_eq!(replayed.arguments["nested"][0], true);
        assert_eq!(replayed.arguments["nested"][1], false);
        assert!(replayed.arguments["nested"][2].is_null());
        assert_ne!(replayed.arguments["nested"][3]["echo"], NUMERIC.to_string());
        assert!(
            !payload.contains(SECRET),
            "strings and keys still need protection"
        );
        assert_eq!(
            agent.restore_secrets_inbound(replayed.clone()).arguments,
            arguments,
            "string placeholders still restore without changing primitive types"
        );
    }
    assert_eq!(
        serde_json::to_value(&agent.messages()[..history.len()]).expect("retained history"),
        original,
        "the local transcript remains unchanged by outbound projection"
    );
}

#[test]
fn numeric_tool_replay_requires_exact_origin_and_never_bypasses_block_mode() {
    use pi::model::{Message, StopReason};

    let harness =
        TestHarness::new("numeric_tool_replay_requires_exact_origin_and_never_bypasses_block_mode");
    let root = harness.temp_path(".");
    for mismatch in ["same", "api", "provider", "model", "unknown"] {
        for mode in ["obfuscate", "block"] {
            if mismatch == "same" && mode == "obfuscate" {
                continue;
            }
            let (mut agent, capture) = build_agent(
                &root,
                Some(SecretsSettings {
                    mode: Some(mode.to_string()),
                    extra_patterns: None,
                }),
            );
            let mut numeric = assistant(
                vec![tool_call(
                    "numeric-origin",
                    json!({"token": 123_456_789_012_345_678_u64}),
                    None,
                )],
                StopReason::ToolUse,
            );
            let Message::Assistant(message) = &mut numeric else {
                unreachable!()
            };
            let message = Arc::make_mut(message);
            match mismatch {
                "api" => message.api = "other-api".to_string(),
                "provider" => message.provider = "other-provider".to_string(),
                "model" => message.model = "other-model".to_string(),
                "unknown" => {
                    message.api.clear();
                    message.provider.clear();
                    message.model.clear();
                }
                _ => {}
            }
            agent.replace_messages(vec![user_text("continue the page"), numeric]);
            let error = block_on_local(agent.run_continue_with_abort(None, |_| {}))
                .expect_err("protected numeric history must refuse before provider entry");
            assert!(
                error.to_string().contains(if mode == "block" {
                    "PI_SECRET_BLOCK"
                } else {
                    "PI_SECRET_JSON_PRIMITIVE"
                }),
                "{mismatch} {mode}: {error}"
            );
            assert!(
                capture.lock().expect("capture").payloads.is_empty(),
                "{mismatch} {mode} must not invoke the provider"
            );
        }
    }
}

#[test]
fn user_and_tool_result_primitives_do_not_inherit_assistant_replay_exceptions() {
    use pi::model::{
        ContentBlock, Message, TextContent, ToolResultMessage, UserContent, UserMessage,
    };

    let harness = TestHarness::new(
        "user_and_tool_result_primitives_do_not_inherit_assistant_replay_exceptions",
    );
    let root = harness.temp_path(".");
    let arguments = json!({"token": 123_456_789_012_345_678_u64});
    let messages = [
        Message::User(UserMessage {
            content: UserContent::Blocks(vec![tool_call("user-call", arguments.clone(), None)]),
            timestamp: 0,
        }),
        Message::tool_result(ToolResultMessage {
            tool_call_id: "result-call".to_string(),
            tool_name: "fixture".to_string(),
            content: vec![tool_call("nested-call", arguments.clone(), None)],
            details: None,
            is_error: false,
            timestamp: 0,
        }),
        Message::tool_result(ToolResultMessage {
            tool_call_id: "details-call".to_string(),
            tool_name: "fixture".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("page ready"))],
            details: Some(arguments),
            is_error: false,
            timestamp: 0,
        }),
    ];
    for message in messages {
        let (mut agent, capture) = build_agent(&root, None);
        let error = block_on_local(agent.run_with_message_with_abort(message, None, |_| {}))
            .expect_err("only exact-origin assistant arguments may preserve detected primitives");
        assert!(
            error.to_string().contains("PI_SECRET_JSON_PRIMITIVE"),
            "{error}"
        );
        assert!(capture.lock().expect("capture").payloads.is_empty());
    }
}

#[test]
fn signed_tool_calls_with_numeric_credentials_do_not_wedge_the_default_session() {
    // OpenAI Responses tool calls always carry an `fc_` id. Rewriting a
    // numeric credential would change its JSON type and refuse the request,
    // and every later one; the signed call is replayed as the model made it.
    let harness = TestHarness::new(
        "signed_tool_calls_with_numeric_credentials_do_not_wedge_the_default_session",
    );
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    let history = vec![
        user_text("page through the results"),
        assistant(
            vec![tool_call(
                "call_page",
                json!({"token": 123_456_789_012_345_678_u64}),
                Some("fc_0123456789abcdef"),
            )],
            pi::model::StopReason::ToolUse,
        ),
    ];
    block_on_local(agent.run_with_messages_with_abort(history, None, |_| {}))
        .expect("a signed numeric argument must not refuse the request");
    block_on_local(agent.run("next page".to_string(), |_| {})).expect("nor any request after it");
    assert_eq!(capture.lock().expect("capture").payloads.len(), 2);
}

#[test]
fn signature_fields_outside_assistant_output_do_not_exempt_screening() {
    // Only a provider signs its own output. A signature field on user or
    // tool-result content (any extension can set one) is not honored.
    let harness =
        TestHarness::new("signature_fields_outside_assistant_output_do_not_exempt_screening");
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    let forged = pi::model::Message::User(pi::model::UserMessage {
        content: pi::model::UserContent::Blocks(vec![pi::model::ContentBlock::Text(
            pi::model::TextContent {
                text: format!("deploy with {SECRET}"),
                text_signature: Some("forged".to_string()),
            },
        )]),
        timestamp: 0,
    });
    block_on_local(agent.run_with_message_with_abort(forged, None, |_| {}))
        .expect("screened request should reach provider");
    let payloads = capture.lock().expect("capture").payloads.clone();
    assert_eq!(payloads.len(), 1);
    assert!(!payloads[0].contains(SECRET), "{}", payloads[0]);
    assert!(payloads[0].contains("<pi-secret:"), "{}", payloads[0]);
}

#[test]
fn openai_item_ids_and_identifier_text_do_not_wedge_the_default_session() {
    // Regression: OpenAI Responses keeps plain item ids (`fc_`/`msg_`) in the
    // signature fields, and `sk-` matched inside `task-management-service`.
    // The second request below, and every one after it, used to fail with
    // PI_SECRET_SIGNED_CONTENT.
    let harness =
        TestHarness::new("openai_item_ids_and_identifier_text_do_not_wedge_the_default_session");
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, None);

    let history = vec![
        user_text("create the service directory"),
        assistant(
            vec![
                pi::model::ContentBlock::Text(pi::model::TextContent {
                    text: "Creating task-management-service now.".to_string(),
                    text_signature: Some("msg_0123456789abcdef".to_string()),
                }),
                tool_call(
                    "call_mkdir",
                    json!({"command": "mkdir task-management-service"}),
                    Some("fc_0123456789abcdef"),
                ),
            ],
            pi::model::StopReason::ToolUse,
        ),
    ];
    block_on_local(agent.run_with_messages_with_abort(history, None, |_| {}))
        .expect("history with item-id signatures must reach the provider");
    block_on_local(agent.run("next step".to_string(), |_| {}))
        .expect("the following request must not be refused either");

    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 2);
    for payload in &capture.payloads {
        assert!(
            payload.contains("mkdir task-management-service"),
            "{payload}"
        );
        assert!(!payload.contains("<pi-secret:"), "{payload}");
    }
    drop(capture);
}

#[test]
fn mcp_tool_names_are_advertised_unchanged_and_refused_only_in_block_mode() {
    // Regression: `sk-` matched inside `task-master`, and any tool name the
    // detector would change was refused with PI_SECRET_IDENTIFIER on every
    // request, in the default mode too.
    const MCP_TOOL: &str = "mcp__task-master-ai__get_tasks";
    const KEYED_TOOL: &str = "deploy-sk-0123456789abcdefghijklmnop";
    let harness =
        TestHarness::new("mcp_tool_names_are_advertised_unchanged_and_refused_only_in_block_mode");
    let root = harness.temp_path(".");

    let (mut agent, capture) = build_agent_with_tools(
        &root,
        None,
        vec![
            Box::new(NamedTool(MCP_TOOL)),
            Box::new(NamedTool(KEYED_TOOL)),
        ],
    );
    block_on_local(agent.run("list my tasks".to_string(), |_| {})).expect("first request");
    block_on_local(agent.run("and again".to_string(), |_| {})).expect("second request");
    let advertised = capture.lock().expect("capture").tools.clone();
    assert_eq!(
        advertised.iter().filter(|name| *name == MCP_TOOL).count(),
        2
    );
    // Names are routing identifiers: never rewritten, even when detected.
    assert_eq!(
        advertised.iter().filter(|name| *name == KEYED_TOOL).count(),
        2
    );

    let (mut agent, _capture) = build_agent_with_tools(
        &root,
        Some(block_mode()),
        vec![Box::new(NamedTool(MCP_TOOL))],
    );
    block_on_local(agent.run("list my tasks".to_string(), |_| {}))
        .expect("an MCP server key is not a credential, even in block mode");

    let (mut agent, capture) = build_agent_with_tools(
        &root,
        Some(block_mode()),
        vec![Box::new(NamedTool(KEYED_TOOL))],
    );
    let error = block_on_local(agent.run("list my tasks".to_string(), |_| {}))
        .expect_err("block mode refuses a credential-shaped tool name");
    let error = error.to_string();
    assert!(
        error.contains("PI_SECRET_BLOCK") || error.contains("PI_SECRET_IDENTIFIER"),
        "{error}"
    );
    assert!(capture.lock().expect("capture").payloads.is_empty());
}

#[test]
fn signed_blocks_and_paused_turns_replay_verbatim_unless_block_mode_refuses() {
    // Signed content must stay byte-stable for provider replay. The default
    // mode sends it as the provider produced it (refusing wedged the session);
    // block mode still refuses on any detection.
    const SECRET_IN_SIGNED_CONTENT: &str = "sk-cccccccccccccccccccccccc";
    let harness = TestHarness::new(
        "signed_blocks_and_paused_turns_replay_verbatim_unless_block_mode_refuses",
    );
    let root = harness.temp_path(".");

    let messages = || {
        [
            assistant(
                vec![pi::model::ContentBlock::Text(pi::model::TextContent {
                    text: SECRET_IN_SIGNED_CONTENT.to_string(),
                    text_signature: Some("text-signature".to_string()),
                })],
                pi::model::StopReason::Stop,
            ),
            assistant(
                vec![pi::model::ContentBlock::Thinking(
                    pi::model::ThinkingContent {
                        thinking: SECRET_IN_SIGNED_CONTENT.to_string(),
                        thinking_signature: Some("thinking-signature".to_string()),
                    },
                )],
                pi::model::StopReason::Stop,
            ),
            assistant(
                vec![tool_call(
                    "signed-call",
                    json!({"token": SECRET_IN_SIGNED_CONTENT}),
                    Some("provider-signature"),
                )],
                pi::model::StopReason::ToolUse,
            ),
            assistant(
                vec![tool_call(
                    "server-tool",
                    json!({"api_key": SECRET_IN_SIGNED_CONTENT}),
                    Some("server-signature"),
                )],
                pi::model::StopReason::PauseTurn,
            ),
        ]
    };

    for message in messages() {
        let (mut agent, capture) = build_agent(&root, None);
        let original = serde_json::to_value(&message).expect("original signed history");
        block_on_local(agent.run_with_message_with_abort(message, None, |_| {}))
            .expect("signed content must not wedge the default mode");
        let capture = capture.lock().expect("capture");
        assert_eq!(capture.payloads.len(), 1);
        assert!(
            capture.payloads[0].contains(SECRET_IN_SIGNED_CONTENT),
            "{}",
            capture.payloads[0]
        );
        assert_eq!(
            serde_json::to_value(&capture.messages[0][0]).expect("replayed signed history"),
            original,
            "same-origin signed or paused content must replay verbatim"
        );
    }
    for message in messages() {
        let (mut agent, capture) = build_agent(&root, Some(block_mode()));
        let error = block_on_local(agent.run_with_message_with_abort(message, None, |_| {}))
            .expect_err("block mode refuses detected secrets in signed content");
        assert!(error.to_string().contains("PI_SECRET_BLOCK"), "{error}");
        assert!(capture.lock().expect("capture").payloads.is_empty());
    }
}

#[test]
fn foreign_signed_and_paused_history_is_screened_without_mutating_the_session() {
    use pi::model::{ContentBlock, Message, StopReason, TextContent, ThinkingContent};

    const OPAQUE: &str = "opaqueCredentialValue1234567890";
    let harness = TestHarness::new(
        "foreign_signed_and_paused_history_is_screened_without_mutating_the_session",
    );
    let root = harness.temp_path(".");

    for mismatch in ["api", "provider", "model", "unknown"] {
        for reason in [StopReason::ToolUse, StopReason::PauseTurn] {
            let (mut agent, capture) = build_agent(&root, None);
            let mut source = assistant(
                vec![
                    ContentBlock::Text(TextContent {
                        text: OPAQUE.to_string(),
                        text_signature: Some("msg_source".to_string()),
                    }),
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: OPAQUE.to_string(),
                        thinking_signature: Some(format!("signature-{OPAQUE}")),
                    }),
                    tool_call("call_source", json!({"echo": OPAQUE}), Some("fc_source")),
                ],
                reason,
            );
            let Message::Assistant(message) = &mut source else {
                unreachable!()
            };
            let message = Arc::make_mut(message);
            match mismatch {
                "api" => message.api = "other-api".to_string(),
                "provider" => message.provider = "other-provider".to_string(),
                "model" => message.model = "other-model".to_string(),
                _ => {
                    message.api.clear();
                    message.provider.clear();
                    message.model.clear();
                }
            }
            let original = serde_json::to_value(&source).expect("original history");

            // The later assignment teaches the vault about an earlier bare
            // echo, including one that was signed by a previous provider.
            block_on_local(agent.run_with_messages_with_abort(
                vec![source, user_text(&format!("api_key={OPAQUE}"))],
                None,
                |_| {},
            ))
            .expect("screened foreign history reaches the provider");

            let capture = capture.lock().expect("capture");
            assert_eq!(capture.payloads.len(), 1);
            assert!(
                !capture.payloads[0].contains(OPAQUE),
                "{mismatch} {reason:?}"
            );
            let Message::Assistant(screened) = &capture.messages[0][0] else {
                panic!("expected screened assistant history")
            };
            assert_eq!(screened.stop_reason, reason);
            assert!(matches!(&screened.content[0], ContentBlock::Text(text)
                if text.text == "<pi-secret:000001>" && text.text_signature.is_none()));
            assert!(
                matches!(&screened.content[1], ContentBlock::Thinking(thinking)
                if thinking.thinking == "<pi-secret:000001>" && thinking.thinking_signature.is_none())
            );
            assert!(matches!(&screened.content[2], ContentBlock::ToolCall(call)
                if call.id == "call_source" && call.name == "fixture"
                    && call.arguments == json!({"echo": "<pi-secret:000001>"})
                    && call.thought_signature.is_none()));
            assert_eq!(
                serde_json::to_value(&agent.messages()[0]).expect("session history"),
                original,
                "outbound screening must retain original history for a switch back"
            );
        }
    }
}

fn attachment_message(
    content: Vec<pi::model::ContentBlock>,
    as_tool_result: bool,
) -> pi::model::Message {
    if as_tool_result {
        pi::model::Message::tool_result(pi::model::ToolResultMessage {
            tool_call_id: "attachment-result".to_string(),
            tool_name: "fixture".to_string(),
            content,
            details: None,
            is_error: false,
            timestamp: 0,
        })
    } else {
        pi::model::Message::User(pi::model::UserMessage {
            content: pi::model::UserContent::Blocks(content),
            timestamp: 0,
        })
    }
}

#[test]
fn media_names_are_discovered_before_earlier_fields_are_screened() {
    use pi::model::{ContentBlock, MediaContent, Message, TextContent, UserContent};

    let harness = TestHarness::new("media_names_are_discovered_before_earlier_fields_are_screened");
    let root = harness.temp_path(".");
    for as_tool_result in [false, true] {
        for mode in ["obfuscate", "block"] {
            let (mut agent, capture) = build_agent(
                &root,
                Some(SecretsSettings {
                    mode: Some(mode.to_string()),
                    extra_patterns: None,
                }),
            );
            let message = attachment_message(
                vec![
                    ContentBlock::Text(TextContent::new(format!("earlier echo: {OPAQUE_SECRET}"))),
                    ContentBlock::Media(MediaContent {
                        data: "cGF5bG9hZA==".to_string(),
                        mime_type: "audio/wav".to_string(),
                        name: Some(SECRET.to_string()),
                    }),
                    ContentBlock::Media(MediaContent {
                        data: "cGF5bG9hZA==".to_string(),
                        mime_type: "video/mp4".to_string(),
                        name: Some(format!("password={OPAQUE_SECRET}")),
                    }),
                ],
                as_tool_result,
            );
            let original = serde_json::to_value(&message).expect("original attachment message");
            let outcome = block_on_local(agent.run_with_message_with_abort(message, None, |_| {}));
            let capture = capture.lock().expect("capture");
            if mode == "block" {
                let error = outcome
                    .expect_err("metadata must obey block mode")
                    .to_string();
                assert!(error.contains("PI_SECRET_BLOCK"), "{error}");
                assert!(!error.contains(SECRET));
                assert!(!error.contains(OPAQUE_SECRET));
                assert!(capture.payloads.is_empty());
            } else {
                outcome.expect("screened names must reach the provider");
                assert_eq!(capture.payloads.len(), 1);
                assert!(!capture.payloads[0].contains(SECRET));
                assert!(!capture.payloads[0].contains(OPAQUE_SECRET));
                let blocks = match &capture.messages[0][0] {
                    Message::User(user) => match &user.content {
                        UserContent::Blocks(blocks) => blocks,
                        UserContent::Text(_) => panic!("attachment blocks expected"),
                    },
                    Message::ToolResult(result) => &result.content,
                    _ => panic!("user or tool-result attachment expected"),
                };
                assert_eq!(blocks.len(), 3);
                for block in &blocks[1..] {
                    let ContentBlock::Media(media) = block else {
                        panic!("media must be retained")
                    };
                    assert_eq!(media.data, "cGF5bG9hZA==");
                    assert!(matches!(
                        media.mime_type.as_str(),
                        "audio/wav" | "video/mp4"
                    ));
                    assert!(media.name.as_deref().unwrap().contains("<pi-secret:"));
                    assert!(!media.placeholder().contains(SECRET));
                    assert!(!media.placeholder().contains(OPAQUE_SECRET));
                }
            }
            assert_eq!(
                serde_json::to_value(&agent.messages()[0]).expect("local transcript"),
                original,
                "screening names must not alter retained media or text"
            );
        }
    }
}

#[test]
fn remembered_and_configured_media_names_are_protected_at_provider_entry() {
    use pi::model::{ContentBlock, MediaContent};

    let harness =
        TestHarness::new("remembered_and_configured_media_names_are_protected_at_provider_entry");
    let root = harness.temp_path(".");
    for as_tool_result in [false, true] {
        let (mut agent, capture) = build_agent(&root, None);
        agent
            .secrets_transform_outbound_text(&format!("password={OPAQUE_SECRET}"))
            .expect("remember an opaque credential");
        let media = ContentBlock::Media(MediaContent {
            data: "cGF5bG9hZA==".to_string(),
            mime_type: "audio/wav".to_string(),
            name: Some(OPAQUE_SECRET.to_string()),
        });
        block_on_local(agent.run_with_message_with_abort(
            attachment_message(vec![media], as_tool_result),
            None,
            |_| {},
        ))
        .expect("remembered names are obfuscated");
        let capture = capture.lock().expect("capture");
        assert_eq!(capture.payloads.len(), 1);
        assert!(!capture.payloads[0].contains(OPAQUE_SECRET));
        assert!(capture.payloads[0].contains("<pi-secret:"));
        drop(capture);

        let (mut blocked, blocked_capture) = build_agent(
            &root,
            Some(SecretsSettings {
                mode: Some("block".to_string()),
                extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
            }),
        );
        let error = block_on_local(blocked.run_with_message_with_abort(
            attachment_message(
                vec![ContentBlock::Media(MediaContent {
                    data: "cGF5bG9hZA==".to_string(),
                    mime_type: "audio/wav".to_string(),
                    name: Some("ACME-123456".to_string()),
                })],
                as_tool_result,
            ),
            None,
            |_| {},
        ))
        .expect_err("custom patterns scan the original name field");
        assert!(error.to_string().contains("PI_SECRET_BLOCK"), "{error}");
        assert!(blocked_capture.lock().expect("capture").payloads.is_empty());
    }
}

#[test]
fn secret_mime_types_refuse_without_changing_transport_or_learning_other_fields() {
    use pi::model::{ContentBlock, ImageContent, MediaContent, TextContent};

    const EARLY: &str = "sk-aaaaaaaaaaaaaaaaaaaaaaaa";
    let harness = TestHarness::new(
        "secret_mime_types_refuse_without_changing_transport_or_learning_other_fields",
    );
    let root = harness.temp_path(".");
    for image in [false, true] {
        for as_tool_result in [false, true] {
            for mode in ["obfuscate", "block", "off"] {
                let (mut agent, capture) = build_agent(
                    &root,
                    Some(SecretsSettings {
                        mode: Some(mode.to_string()),
                        extra_patterns: None,
                    }),
                );
                let mime = format!("{}/{SECRET}", if image { "image" } else { "audio" });
                let block = if image {
                    ContentBlock::Image(ImageContent {
                        data: "cGF5bG9hZA==".to_string(),
                        mime_type: mime.clone(),
                    })
                } else {
                    ContentBlock::Media(MediaContent {
                        data: "cGF5bG9hZA==".to_string(),
                        mime_type: mime.clone(),
                        name: Some("recording".to_string()),
                    })
                };
                let message = attachment_message(
                    vec![
                        ContentBlock::Text(TextContent::new(if mode == "block" {
                            "ordinary"
                        } else {
                            EARLY
                        })),
                        block,
                    ],
                    as_tool_result,
                );
                let original = serde_json::to_value(&message).expect("original MIME");
                let outcome =
                    block_on_local(agent.run_with_message_with_abort(message, None, |_| {}));
                let capture = capture.lock().expect("capture");
                if mode == "off" {
                    outcome.expect("off mode keeps ordinary transport behavior");
                    assert_eq!(capture.payloads.len(), 1);
                    assert!(capture.payloads[0].contains(&mime));
                } else {
                    let error = outcome.expect_err("secret MIME cannot be sent").to_string();
                    assert!(
                        error.contains(if mode == "block" {
                            "PI_SECRET_BLOCK"
                        } else {
                            "PI_SECRET_MEDIA_MIME"
                        }),
                        "{error}"
                    );
                    assert!(!error.contains(SECRET));
                    assert!(!error.contains(EARLY));
                    assert!(capture.payloads.is_empty());
                    if mode == "obfuscate" {
                        assert_eq!(
                            agent
                                .secrets_transform_outbound_text(EARLY)
                                .expect("rolled-back vault"),
                            "<pi-secret:000001>",
                            "the MIME refusal must roll back discovery of every earlier field"
                        );
                    }
                }
                assert_eq!(
                    serde_json::to_value(&agent.messages()[0]).expect("retained MIME"),
                    original
                );
            }
        }
    }
}

#[test]
fn encoded_image_and_media_payloads_remain_opaque_to_secret_screening() {
    use pi::model::{ContentBlock, ImageContent, MediaContent};

    // This is valid base64 whose bytes also match a built-in AWS-key rule.
    // Inspecting the encoded payload as text would either corrupt it or refuse.
    const PAYLOAD: &str = "AKIA1234567890ABCDEF";
    assert!(pi::secrets::contains_secret(PAYLOAD, &[]));
    let harness =
        TestHarness::new("encoded_image_and_media_payloads_remain_opaque_to_secret_screening");
    let root = harness.temp_path(".");
    for mode in ["obfuscate", "block"] {
        let (mut agent, capture) = build_agent(
            &root,
            Some(SecretsSettings {
                mode: Some(mode.to_string()),
                extra_patterns: None,
            }),
        );
        let blocks = vec![
            ContentBlock::Image(ImageContent {
                data: PAYLOAD.to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Media(MediaContent {
                data: PAYLOAD.to_string(),
                mime_type: "audio/wav".to_string(),
                name: Some("recording".to_string()),
            }),
        ];
        let messages = vec![
            attachment_message(blocks.clone(), false),
            attachment_message(blocks, true),
        ];
        let original = serde_json::to_value(&messages).expect("original encoded payloads");
        block_on_local(agent.run_with_messages_with_abort(messages, None, |_| {}))
            .expect("encoded payloads are not text metadata");
        let capture = capture.lock().expect("capture");
        assert_eq!(capture.payloads.len(), 1);
        assert_eq!(
            serde_json::to_value(&capture.messages[0]).unwrap(),
            original
        );
        assert_eq!(
            serde_json::to_value(&agent.messages()[..2]).unwrap(),
            original
        );
    }
}

#[test]
fn block_mode_paused_replay_keeps_payloads_opaque_but_screens_content_and_names() {
    use pi::model::{
        ContentBlock, ImageContent, MediaContent, StopReason, TextContent, ThinkingContent,
    };

    const PAYLOAD: &str = "AKIA1234567890ABCDEF";
    let harness = TestHarness::new(
        "block_mode_paused_replay_keeps_payloads_opaque_but_screens_content_and_names",
    );
    let root = harness.temp_path(".");
    let (mut agent, capture) = build_agent(&root, Some(block_mode()));
    let media = MediaContent {
        data: PAYLOAD.to_string(),
        mime_type: "audio/wav".to_string(),
        name: Some("recording".to_string()),
    };
    let paused = assistant(
        vec![
            ContentBlock::Media(media.clone()),
            ContentBlock::Image(ImageContent {
                data: PAYLOAD.to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "check recording".to_string(),
                thinking_signature: Some(PAYLOAD.to_string()),
            }),
            ContentBlock::Text(TextContent {
                text: "recording received".to_string(),
                text_signature: Some(PAYLOAD.to_string()),
            }),
            tool_call("paused-call", json!({"status": "ready"}), Some(PAYLOAD)),
        ],
        StopReason::PauseTurn,
    );
    let original = serde_json::to_value(&paused).expect("original paused response");
    block_on_local(agent.run_with_message_with_abort(paused, None, |_| {}))
        .expect("opaque paused payloads and signatures must not be screened as text");
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 1);
    assert_eq!(
        serde_json::to_value(&capture.messages[0][0]).unwrap(),
        original
    );
    assert_eq!(
        serde_json::to_value(&agent.messages()[0]).unwrap(),
        original
    );
    drop(capture);

    let protected_blocks = [
        ContentBlock::Media(MediaContent {
            name: Some(SECRET.to_string()),
            ..media
        }),
        tool_call("protected-call", json!({"api_key": SECRET}), Some(PAYLOAD)),
        ContentBlock::Text(TextContent {
            text: SECRET.to_string(),
            text_signature: Some(PAYLOAD.to_string()),
        }),
        ContentBlock::Thinking(ThinkingContent {
            thinking: SECRET.to_string(),
            thinking_signature: Some(PAYLOAD.to_string()),
        }),
    ];
    for block in protected_blocks {
        let (mut blocked, blocked_capture) = build_agent(&root, Some(block_mode()));
        let error = block_on_local(blocked.run_with_message_with_abort(
            assistant(vec![block], StopReason::PauseTurn),
            None,
            |_| {},
        ))
        .expect_err("paused replay never exempts readable content from block mode");
        assert!(error.to_string().contains("PI_SECRET_BLOCK"), "{error}");
        assert!(!error.to_string().contains(SECRET));
        assert!(blocked_capture.lock().expect("capture").payloads.is_empty());
    }
}
