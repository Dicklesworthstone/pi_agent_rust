#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::web_remote::{
    BindMode, ControlMode, EMBEDDED_WEB_CLIENT_HTML, EMBEDDED_WEB_CLIENT_JS, TokenKind,
    WebRemoteBridge, WebRemoteManager, WebRemoteSettings,
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

    let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
    let raw_secret = "sk-live-99887766554433221100aabbccddeeff";
    bridge.set_secrets(vec![raw_secret.to_string()]);
    let pairing_token = bridge.manager().generate_token(TokenKind::View).unwrap();
    assert!(bridge.publish_text(
        120,
        24,
        format!("Connecting using OpenAI key {raw_secret}...\nPairing {pairing_token}")
    ));
    let frame = bridge.frame_after(0).expect("sanitized published frame");
    assert_eq!(frame.schema, "pi.web.frame.v1");
    assert!(!frame.data.contains(raw_secret));
    assert!(!frame.data.contains(&pairing_token));
    assert!(frame.data.contains("Connecting using OpenAI key"));
    assert!(frame.data.contains('*'));
    let wire = serde_json::to_string(&*frame).unwrap();
    assert!(!wire.contains(raw_secret));
    assert!(!wire.contains(&pairing_token));

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
        require_auth_token: true,
        enable_audit_log: true,
    };

    let manager = WebRemoteManager::new(settings);
    let token = manager.generate_token(TokenKind::Steer).unwrap();
    manager
        .connect_client("remote-user", "100.64.0.5:40001", Some(&token))
        .unwrap();

    // A browser request cannot grant itself the first control lease.
    let takeover_result = manager.request_takeover("remote-user");
    assert_eq!(takeover_result, Ok(ControlMode::TakeoverPendingApproval));
    assert!(manager.authorize_input("remote-user", 1).is_err());
    assert!(
        manager
            .audit_log()
            .iter()
            .all(|entry| entry.event_type != "takeover_granted")
    );

    assert_eq!(
        manager.approve_takeover("remote-user"),
        Ok(ControlMode::RemoteControlling)
    );
    let epoch = manager.authorize_input("remote-user", 1).unwrap();
    assert!(manager.input_authorized("remote-user", epoch));
    assert!(manager.authorize_input("remote-user", 1).is_err());
    manager.revoke_control();
    assert!(!manager.input_authorized("remote-user", epoch));
    assert!(manager.authorize_input("remote-user", 2).is_err());

    // Audit logs record takeover grant with provenance
    let logs = manager.audit_log();
    assert!(logs.iter().any(|e| e.event_type == "takeover_granted"
        && e.client_id.as_deref() == Some("remote-user")
        && e.details.get("actor").map(String::as_str) == Some("local_operator")));
    assert!(!serde_json::to_string(&logs).unwrap().contains(&token));
    assert!(!format!("{manager:?}").contains(&token));

    finish_case(&harness, "remote_approval_gate");
}

#[test]
fn test_static_zero_browser_persistence_audit() {
    let harness = TestHarness::new("zero_browser_persistence_audit");

    // Static analysis over all embedded client assets
    for asset in [EMBEDDED_WEB_CLIENT_HTML, EMBEDDED_WEB_CLIENT_JS] {
        assert!(!asset.contains("localStorage"));
        assert!(!asset.contains("sessionStorage"));
        assert!(!asset.contains("indexedDB"));
        assert!(!asset.contains("openDatabase"));
    }

    finish_case(&harness, "zero_browser_persistence_audit");
}
