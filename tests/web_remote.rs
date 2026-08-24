#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::web_remote::{
    BindMode, ControlMode, EMBEDDED_WEB_CLIENT_HTML, TokenKind, WebFrameType, WebRemoteManager,
    WebRemoteSettings, render_half_block_qr,
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
fn test_web_frame_protocol_and_schema() {
    let harness = TestHarness::new("web_frame_protocol");

    let manager = WebRemoteManager::new(WebRemoteSettings::default());

    let kf = manager.next_frame(WebFrameType::Keyframe, 120, 36, "Header\nBody line");
    assert_eq!(kf.schema, "pi.web.frame.v1");
    assert_eq!(kf.seq, 1);
    assert_eq!(kf.frame_type, WebFrameType::Keyframe);
    assert_eq!(kf.width, 120);
    assert_eq!(kf.height, 36);
    assert_eq!(kf.data, "Header\nBody line");

    let patch = manager.next_frame(WebFrameType::Patch, 120, 36, "Diff chunk");
    assert_eq!(patch.schema, "pi.web.frame.v1");
    assert_eq!(patch.seq, 2);
    assert_eq!(patch.frame_type, WebFrameType::Patch);

    finish_case(&harness, "web_frame_protocol");
}

#[test]
fn test_web_client_authentication_and_capacity() {
    let harness = TestHarness::new("web_client_auth_capacity");

    let settings = WebRemoteSettings {
        port: 9090,
        bind_mode: BindMode::Loopback,
        view_only: false,
        max_viewers: 3,
        require_auth_token: true,
        enable_audit_log: true,
    };

    let manager = WebRemoteManager::new(settings);
    manager.issue_token("tok-alpha", TokenKind::Steer);
    manager.issue_token("tok-beta", TokenKind::View);
    manager.issue_token("tok-gamma", TokenKind::Steer);
    manager.issue_token("tok-delta", TokenKind::View);

    // Connect viewer 1
    let v1 = manager.connect_client("viewer-1", "127.0.0.1:4001", Some("tok-alpha"));
    assert!(v1.is_ok());
    assert!(!v1.as_ref().map_or(true, |c| c.is_view_only));

    // Reuse of tok-alpha is rejected
    let v1_replay = manager.connect_client("viewer-1-replay", "127.0.0.1:4002", Some("tok-alpha"));
    assert!(v1_replay.is_err());

    // Connect viewers 2 (view only) and 3
    let v2 = manager.connect_client("viewer-2", "127.0.0.1:4003", Some("tok-beta"));
    let v3 = manager.connect_client("viewer-3", "127.0.0.1:4004", Some("tok-gamma"));
    assert!(v2.is_ok());
    assert!(v2.as_ref().map_or(false, |c| c.is_view_only));
    assert!(v3.is_ok());

    // Viewer 2 cannot request control because it is view-only
    let v2_control = manager.request_takeover("viewer-2");
    assert!(v2_control.is_err());

    // 4th viewer rejected due to max_viewers = 3
    let v4 = manager.connect_client("viewer-4", "127.0.0.1:4005", Some("tok-delta"));
    assert!(v4.is_err());
    assert!(
        v4.as_ref()
            .err()
            .map_or(false, |e| e.contains("maximum viewer capacity"))
    );

    finish_case(&harness, "web_client_auth_capacity");
}

#[test]
fn test_qr_console_pairing_render() {
    let harness = TestHarness::new("qr_console_pairing");

    let steer_url = "http://100.64.0.1:8080/#t=steer_tok_123456";
    let view_url = "http://100.64.0.1:8080/#t=view_tok_654321";

    let steer_qr = render_half_block_qr(steer_url);
    let view_qr = render_half_block_qr(view_url);

    assert!(steer_qr.contains("▀"));
    assert!(steer_qr.contains("▄"));
    assert!(steer_qr.contains("█"));
    assert!(steer_qr.contains("steer_tok_123456"));

    assert!(view_qr.contains("▀"));
    assert!(view_qr.contains("view_tok_654321"));

    finish_case(&harness, "qr_console_pairing");
}

#[test]
fn test_input_arbitration_and_audit_ledger() {
    let harness = TestHarness::new("input_arbitration_audit");

    let settings = WebRemoteSettings {
        port: 8080,
        bind_mode: BindMode::Tailscale,
        view_only: false,
        max_viewers: 4,
        require_auth_token: false,
        enable_audit_log: true,
    };

    let manager = WebRemoteManager::new(settings);
    let _ = manager.connect_client("remote-peer-1", "100.64.0.1:52110", None);

    // Peer 1 takes control
    let control_1 = manager.request_takeover("remote-peer-1");
    assert_eq!(control_1, Ok(ControlMode::RemoteControlling));

    let _ = manager.connect_client("remote-peer-2", "100.64.0.2:52112", None);

    // Peer 2 requests control while Peer 1 is active -> pending approval
    let control_2 = manager.request_takeover("remote-peer-2");
    assert_eq!(control_2, Ok(ControlMode::TakeoverPendingApproval));

    // Peer 1 releases control
    manager.release_control("remote-peer-1");

    // Peer 2 requests control again -> granted
    let control_2_now = manager.request_takeover("remote-peer-2");
    assert_eq!(control_2_now, Ok(ControlMode::RemoteControlling));

    // Verify audit log has proper schema and records
    let logs = manager.audit_log();
    assert!(logs.len() >= 4);
    for entry in &logs {
        assert_eq!(entry.schema, "pi.web.audit.v1");
        assert!(entry.timestamp_ms > 0);
    }

    finish_case(&harness, "input_arbitration_audit");
}

#[test]
fn test_embedded_web_client_security_posture() {
    let harness = TestHarness::new("web_client_security");

    assert!(
        !EMBEDDED_WEB_CLIENT_HTML.is_empty(),
        "embedded client HTML must not be empty"
    );

    // Verify CSP header presence and strict rules
    assert!(
        EMBEDDED_WEB_CLIENT_HTML.contains("Content-Security-Policy"),
        "must define strict Content-Security-Policy"
    );
    assert!(
        EMBEDDED_WEB_CLIENT_HTML.contains("default-src 'self'"),
        "CSP must restrict default sources to self"
    );

    // Verify zero persistent storage usage (no localStorage, indexedDB)
    assert!(
        !EMBEDDED_WEB_CLIENT_HTML.contains("localStorage"),
        "client must not store session content in localStorage"
    );
    assert!(
        !EMBEDDED_WEB_CLIENT_HTML.contains("indexedDB"),
        "client must not store session content in indexedDB"
    );

    finish_case(&harness, "web_client_security");
}
