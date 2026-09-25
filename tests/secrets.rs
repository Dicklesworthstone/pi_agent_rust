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
use pi::tools::{ToolOutput, ToolRegistry};
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
            use std::fmt::Write as _;
            let _ = write!(payload, "{message:?}"); // ubs:ignore capture loop in a stub provider
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
            message.content.push(ContentBlock::Text(TextContent::new("key copied")));
        }
        Ok(Box::pin(futures::stream::iter(vec![Ok(StreamEvent::Done {
            reason: message.stop_reason,
            message,
        })])))
    }
}

#[test]
fn private_key_placeholder_executes_real_write_and_read_without_cloud_disclosure() {
    let case = "private_key_real_tool_round_trip";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let capture = Arc::new(Mutex::new(Capture::default()));
    let provider = Arc::new(PrivateKeyToolProvider { capture: Arc::clone(&capture) });
    let tools = ToolRegistry::new(&["write", "read"], &root, None);
    let mut agent = Agent::new(provider, tools, AgentConfig::default());
    let key = private_key_fixture();
    let completed_tools = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&completed_tools);
    let result = block_on_local(agent.run(
        format!("Copy this private key, then read the copy:\n{key}"),
        move |event| {
            if let pi::agent::AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } = event {
                recorded.lock().expect("tool events").push((tool_name, is_error));
            }
        },
    ))
    .expect("real tool round trip");
    assert_eq!(result.stop_reason, pi::model::StopReason::Stop);
    assert_eq!(std::fs::read_to_string(root.join("copied.pem")).expect("written key"), key);
    assert_eq!(
        *completed_tools.lock().expect("tool events"),
        vec![("write".to_string(), false), ("read".to_string(), false)]
    );
    let capture = capture.lock().expect("capture");
    assert_eq!(capture.payloads.len(), 3);
    for payload in &capture.payloads {
        assert!(payload.contains("<pi-secret:"));
        assert!(!payload.contains(PEM_BODY), "private body reached the provider");
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
    let (mut agent, capture) = build_agent(&root, Some(SecretsSettings {
        mode: Some("block".to_string()),
        extra_patterns: None,
    }));
    let result = block_on_local(agent.run(
        format!("-----BEGIN PRIVATE KEY-----\n{PEM_BODY}"),
        |_| {},
    ));
    let error = result.expect_err("block mode refuses before provider entry").to_string();
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
    let (mut agent, capture) = build_agent(&root, Some(SecretsSettings {
        mode: Some("obfuscate".to_string()),
        extra_patterns: Some(vec!["abcde".to_string(), "defgh".to_string(), "ghij".to_string()]),
    }));
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
    block_on_local(agent.run(format!("echoed value: {OPAQUE_SECRET}"), |_| {})).expect("later turn");
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
    let records = agent.messages().iter().map(|message| {
        serde_json::to_string(message).expect("serialize local transcript")
    }).collect::<Vec<_>>();
    let original = format!("{}\r\n", records.join("\r\n"));
    assert!(original.contains(PEM_BODY), "local input is deliberately not an export");
    let exported = agent.mask_secrets_text(&original);
    assert!(!exported.contains(PEM_BODY));
    assert!(exported.contains("<pi-secret:000001>"));
    assert!(exported.ends_with("\r\n"));
    let decoded = exported.lines().map(|line| {
        serde_json::from_str::<serde_json::Value>(line).expect("screened record remains valid JSON")
    }).collect::<Vec<_>>();
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
        let (mut agent, capture) = build_agent(&root, Some(SecretsSettings {
            mode: Some(mode.to_string()),
            extra_patterns: None,
        }));
        let result = block_on_local(agent.run(
            json!({"password": OPAQUE_SECRET}).to_string(),
            |_| {},
        ));
        let capture = capture.lock().expect("capture");
        if mode == "block" {
            let error = result.expect_err("quoted keys must not bypass block mode").to_string();
            assert!(error.contains("PI_SECRET_BLOCK"));
            assert!(!error.contains(OPAQUE_SECRET));
            assert!(capture.payloads.is_empty());
        } else {
            result.expect("off mode retains ordinary provider behavior");
            assert_eq!(capture.payloads.len(), 1);
            assert!(capture.payloads[0].contains(OPAQUE_SECRET));
        }
    }
    finish_case(&harness, case);
}
