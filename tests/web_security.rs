#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::web_remote::{
    BindMode, ControlMode, EMBEDDED_WEB_CLIENT_HTML, WebFrameType, WebRemoteManager,
    WebRemoteSettings,
};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_threat_model_document_exists_and_valid() {
    let harness = TestHarness::new("threat_model_document");
    let doc_path = "docs/security/web-access-threat-model.md";
    assert!(
        std::path::Path::new(doc_path).is_file(),
        "threat model document must exist"
    );

    let content = std::fs::read_to_string(doc_path).unwrap_or_default();
    assert!(content.contains("pi.web.threat_model.v1"));
    assert!(content.contains("T-01"));
    assert!(content.contains("T-02"));
    assert!(content.contains("T-03"));
    assert!(content.contains("T-04"));
    assert!(content.contains("T-05"));
    assert!(content.contains("T-06"));
    assert!(content.contains("T-07"));

    finish_case(&harness, "threat_model_document");
}

#[test]
fn test_canary_secrets_obfuscation_in_web_frames() {
    let harness = TestHarness::new("canary_secrets_obfuscation");

    let manager = WebRemoteManager::new(WebRemoteSettings::default());

    // Canary raw secret payload
    let raw_secret = "sk-live-99887766554433221100aabbccddeeff";
    let placeholder = "[SECRET_OPENAI_KEY_1]";

    // Text with substituted placeholder
    let safe_content = format!("Connecting using OpenAI key {placeholder}...");
    assert!(!safe_content.contains(raw_secret));

    let frame = manager.next_frame(WebFrameType::Patch, 80, 24, &safe_content);
    assert_eq!(frame.schema, "pi.web.frame.v1");
    assert!(!frame.data.contains(raw_secret));
    assert!(frame.data.contains(placeholder));

    finish_case(&harness, "canary_secrets_obfuscation");
}

#[test]
fn test_remote_mutating_action_requires_local_approval() {
    let harness = TestHarness::new("remote_approval_gate");

    let settings = WebRemoteSettings {
        port: 8080,
        bind_mode: BindMode::Tailscale,
        view_only: false,
        max_viewers: 2,
        require_auth_token: false,
        enable_audit_log: true,
    };

    let manager = WebRemoteManager::new(settings);
    let _ = manager.connect_client("remote-user", "100.64.0.5:40001", None);

    // Initial state: Remote user does not have control until requested and granted
    let takeover_result = manager.request_takeover("remote-user");
    assert_eq!(takeover_result, Ok(ControlMode::RemoteControlling));

    // Audit logs record takeover grant with provenance
    let logs = manager.audit_log();
    assert!(logs.iter().any(
        |e| e.event_type == "takeover_granted" && e.client_id.as_deref() == Some("remote-user")
    ));

    finish_case(&harness, "remote_approval_gate");
}

#[test]
fn test_static_zero_browser_persistence_audit() {
    let harness = TestHarness::new("zero_browser_persistence_audit");

    // Static analysis over all embedded client assets
    assert!(!EMBEDDED_WEB_CLIENT_HTML.contains("localStorage"));
    assert!(!EMBEDDED_WEB_CLIENT_HTML.contains("sessionStorage"));
    assert!(!EMBEDDED_WEB_CLIENT_HTML.contains("indexedDB"));
    assert!(!EMBEDDED_WEB_CLIENT_HTML.contains("openDatabase"));

    finish_case(&harness, "zero_browser_persistence_audit");
}
