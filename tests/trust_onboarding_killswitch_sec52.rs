//! SEC-5.2 tests: Trust onboarding wizard and emergency kill-switch workflow
//! (bd-ww5br).
//!
//! Validates:
//! - Onboarding flow for quarantined extensions with risk disclosures
//! - Emergency kill-switch (demotion) halts targeted extension activity
//! - Audit trail preserves decision provenance for all trust transitions
//! - Re-onboarding after kill-switch requires fresh operator acknowledgment
//! - Alert generation on emergency demotion events
//! - Multi-extension independent trust tracking
//! - Hostcall gating enforcement after emergency demotion

mod common;

use common::TestHarness;
use pi::extension_preflight::{
    ExtensionTrustState, ExtensionTrustTracker, InstallRecommendation, TRUST_LIFECYCLE_SCHEMA,
    classify_extension_source, initial_trust_state, is_hostcall_allowed_for_trust,
};
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, SECURITY_ALERT_SCHEMA_VERSION, SecurityAlert,
    SecurityAlertAction, SecurityAlertCategory, SecurityAlertFilter, SecurityAlertSeverity,
    query_security_alerts,
};

// ============================================================================
// Helpers
// ============================================================================

fn risky_tracker(ext_id: &str) -> ExtensionTrustTracker {
    ExtensionTrustTracker::new(ext_id, ExtensionTrustState::Quarantined)
}

fn trusted_tracker(ext_id: &str) -> ExtensionTrustTracker {
    ExtensionTrustTracker::new(ext_id, ExtensionTrustState::Trusted)
}

/// Simulate an emergency kill-switch: demote + record alert.
fn emergency_kill_switch(
    tracker: &mut ExtensionTrustTracker,
    manager: &ExtensionManager,
    reason: &str,
) {
    let ext_id = tracker.extension_id().to_string();
    tracker.demote(reason).expect("demotion must succeed");
    // Record a quarantine alert mirroring what the runtime would produce.
    manager.record_security_alert(SecurityAlert::from_quarantine(
        &ext_id, reason, 0.95, // high risk score triggering kill-switch
    ));
}

// ============================================================================
// Test 1: Full onboarding wizard flow — quarantine → restricted → trusted
// ============================================================================

#[test]
fn onboarding_wizard_full_promotion_flow() {
    let harness = TestHarness::new("onboarding_wizard_full_promotion_flow");

    let policy = ExtensionPolicy::default();
    let report = classify_extension_source("ext.wizard", "eval('bad');", &policy);
    assert_eq!(report.recommendation, InstallRecommendation::Block);

    // Starts quarantined due to risky source.
    let state = initial_trust_state(&report);
    assert_eq!(state, ExtensionTrustState::Quarantined);

    let mut tracker = ExtensionTrustTracker::from_risk_report(&report);
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    // Step 1: Operator reviews and promotes to Restricted.
    let event = tracker
        .promote("operator reviewed source code", true, Some(60), None)
        .unwrap();
    assert_eq!(event.to_state, ExtensionTrustState::Restricted);
    assert!(event.operator_acknowledged);

    // Verify hostcall gating: read allowed, dangerous blocked.
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "read"));
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "exec"));

    // Step 2: After observation period, promote to Trusted.
    let event = tracker
        .promote(
            "no anomalies in restricted mode",
            true,
            Some(80),
            Some(InstallRecommendation::Allow),
        )
        .unwrap();
    assert_eq!(event.to_state, ExtensionTrustState::Trusted);

    // Verify hostcall gating: everything allowed.
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "exec"));
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "http"));
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "write"));

    // Verify audit trail completeness.
    let history = tracker.history();
    assert_eq!(history.len(), 2);

    harness.log().info_ctx(
        "onboarding_wizard",
        "full promotion flow complete",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("final_state".into(), format!("{}", tracker.state())));
            ctx_log.push(("history_len".into(), history.len().to_string()));
        },
    );
}

// ============================================================================
// Test 2: Emergency kill-switch halts extension immediately
// ============================================================================

#[test]
fn kill_switch_halts_trusted_extension() {
    let harness = TestHarness::new("kill_switch_halts_trusted_extension");
    let mgr = ExtensionManager::new();

    let mut tracker = trusted_tracker("ext.killswitch");

    // Verify trusted state allows dangerous operations.
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "exec"));
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "http"));

    // Emergency kill-switch.
    emergency_kill_switch(&mut tracker, &mgr, "critical anomaly detected");

    // Verify immediate quarantine.
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);
    assert!(tracker.state().is_quarantined());

    // Verify ALL dangerous hostcalls are blocked.
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "exec"));
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "http"));
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "write"));
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "env"));

    // Registration hostcalls still allowed (extension needs to re-register).
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "register"));
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "log"));

    // Verify alert was generated.
    assert_eq!(mgr.security_alert_count(), 1);
    let artifact = mgr.security_alert_artifact();
    let alert = &artifact.alerts[0];
    assert_eq!(alert.category, SecurityAlertCategory::Quarantine);
    assert_eq!(alert.severity, SecurityAlertSeverity::Critical);
    assert_eq!(alert.action, SecurityAlertAction::Terminate);

    harness
        .log()
        .info_ctx("kill_switch", "trusted extension halted", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("final_state".into(), format!("{}", tracker.state())));
        });
}

// ============================================================================
// Test 3: Audit trail preserves decision provenance
// ============================================================================

#[test]
fn audit_trail_preserves_full_provenance() {
    let harness = TestHarness::new("audit_trail_preserves_full_provenance");

    let mut tracker = risky_tracker("ext.audit");

    // Simulate full lifecycle: promote twice, then kill-switch.
    tracker
        .promote(
            "initial review",
            true,
            Some(60),
            Some(InstallRecommendation::Review),
        )
        .unwrap();
    tracker
        .promote(
            "observation complete",
            true,
            Some(80),
            Some(InstallRecommendation::Allow),
        )
        .unwrap();
    tracker
        .demote("emergency: data exfiltration attempt")
        .unwrap();

    let history = tracker.history();
    assert_eq!(history.len(), 3);

    // Event 0: Quarantined → Restricted (promote).
    assert_eq!(history[0].from_state, ExtensionTrustState::Quarantined);
    assert_eq!(history[0].to_state, ExtensionTrustState::Restricted);
    assert_eq!(history[0].reason, "initial review");
    assert!(history[0].operator_acknowledged);
    assert_eq!(history[0].risk_score, Some(60));
    assert_eq!(
        history[0].recommendation,
        Some(InstallRecommendation::Review)
    );

    // Event 1: Restricted → Trusted (promote).
    assert_eq!(history[1].from_state, ExtensionTrustState::Restricted);
    assert_eq!(history[1].to_state, ExtensionTrustState::Trusted);
    assert_eq!(history[1].reason, "observation complete");

    // Event 2: Trusted → Quarantined (demote/kill-switch).
    assert_eq!(history[2].from_state, ExtensionTrustState::Trusted);
    assert_eq!(history[2].to_state, ExtensionTrustState::Quarantined);
    assert_eq!(history[2].reason, "emergency: data exfiltration attempt");
    assert!(!history[2].operator_acknowledged); // Demotions don't need ack.

    // JSONL export preserves all events.
    let jsonl = tracker.history_jsonl().unwrap();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["schema"], TRUST_LIFECYCLE_SCHEMA);
        assert_eq!(v["extension_id"], "ext.audit");
    }

    // Verify timestamps are monotonically non-decreasing (RFC 3339 strings sort correctly).
    for window in history.windows(2) {
        assert!(
            window[0].timestamp <= window[1].timestamp,
            "timestamps must be monotonic: {} > {}",
            window[0].timestamp,
            window[1].timestamp
        );
    }

    harness
        .log()
        .info_ctx("audit_trail", "full provenance preserved", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("events".into(), history.len().to_string()));
        });
}

// ============================================================================
// Test 4: Re-onboarding after kill-switch requires fresh ack
// ============================================================================

#[test]
fn re_onboarding_after_kill_switch_requires_fresh_ack() {
    let harness = TestHarness::new("re_onboarding_after_kill_switch");

    let mut tracker = trusted_tracker("ext.reonboard");

    // Kill-switch: Trusted → Quarantined.
    tracker.demote("critical vulnerability found").unwrap();
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    // Attempt re-promotion WITHOUT operator ack → must fail.
    let err = tracker
        .promote("auto-recover", false, None, None)
        .unwrap_err();
    assert!(
        matches!(
            err,
            pi::extension_preflight::TrustTransitionError::OperatorAckRequired { .. }
        ),
        "re-promotion without ack must be rejected"
    );
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    // Attempt re-promotion WITH operator ack → succeeds.
    tracker
        .promote("operator re-reviewed after patch", true, Some(70), None)
        .unwrap();
    assert_eq!(tracker.state(), ExtensionTrustState::Restricted);

    // Full history includes: demote + failed attempt (not recorded) + re-promote.
    assert_eq!(tracker.history().len(), 2);

    harness.log().info_ctx(
        "re_onboarding",
        "re-promotion after kill-switch",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("final_state".into(), format!("{}", tracker.state())));
        },
    );
}

// ============================================================================
// Test 5: Alert filtering by extension after kill-switch
// ============================================================================

#[test]
fn alert_filtering_after_kill_switch() {
    let harness = TestHarness::new("alert_filtering_after_kill_switch");
    let mgr = ExtensionManager::new();

    // Simulate alerts from multiple extensions.
    mgr.record_security_alert(SecurityAlert::from_quarantine("ext.a", "anomaly", 0.9));
    mgr.record_security_alert(SecurityAlert::from_quarantine("ext.b", "anomaly", 0.85));
    mgr.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.a",
        "exec",
        "spawn",
        "denied by policy",
        "static_policy",
    ));

    assert_eq!(mgr.security_alert_count(), 3);

    // Filter quarantine alerts for ext.a only.
    let filter = SecurityAlertFilter {
        category: Some(SecurityAlertCategory::Quarantine),
        extension_id: Some("ext.a".to_string()),
        ..Default::default()
    };
    let filtered = query_security_alerts(&mgr, &filter);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].extension_id, "ext.a");
    assert_eq!(filtered[0].category, SecurityAlertCategory::Quarantine);

    // Filter critical severity alerts.
    let crit_filter = SecurityAlertFilter {
        min_severity: Some(SecurityAlertSeverity::Critical),
        ..Default::default()
    };
    let critical = query_security_alerts(&mgr, &crit_filter);
    // Quarantine alerts are Critical severity.
    assert_eq!(critical.len(), 2);

    harness.log().info_ctx(
        "alert_filtering",
        "kill-switch alerts filterable",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push((
                "total_alerts".into(),
                mgr.security_alert_count().to_string(),
            ));
            ctx_log.push(("quarantine_ext_a".into(), filtered.len().to_string()));
            ctx_log.push(("critical_total".into(), critical.len().to_string()));
        },
    );
}

// ============================================================================
// Test 6: Multi-extension independent trust tracking
// ============================================================================

#[test]
fn multi_extension_independent_trust_states() {
    let harness = TestHarness::new("multi_extension_independent_trust_states");

    let mut alpha = risky_tracker("ext.alpha");
    let mut beta = trusted_tracker("ext.beta");
    let gamma = risky_tracker("ext.gamma");

    // Promote alpha to Restricted.
    alpha.promote("reviewed", true, None, None).unwrap();

    // Kill-switch beta.
    beta.demote("compromised dependency").unwrap();

    // gamma stays quarantined (no action).

    // Verify independent states.
    assert_eq!(alpha.state(), ExtensionTrustState::Restricted);
    assert_eq!(beta.state(), ExtensionTrustState::Quarantined);
    assert_eq!(gamma.state(), ExtensionTrustState::Quarantined);

    // Verify hostcall gating is per-extension.
    assert!(is_hostcall_allowed_for_trust(alpha.state(), "read"));
    assert!(!is_hostcall_allowed_for_trust(alpha.state(), "exec"));

    assert!(!is_hostcall_allowed_for_trust(beta.state(), "read"));
    assert!(!is_hostcall_allowed_for_trust(beta.state(), "exec"));

    assert!(!is_hostcall_allowed_for_trust(gamma.state(), "read"));

    // Verify histories are independent.
    assert_eq!(alpha.history().len(), 1);
    assert_eq!(beta.history().len(), 1);
    assert!(gamma.history().is_empty());

    harness.log().info_ctx(
        "multi_extension",
        "independent trust states verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("alpha".into(), format!("{}", alpha.state())));
            ctx_log.push(("beta".into(), format!("{}", beta.state())));
            ctx_log.push(("gamma".into(), format!("{}", gamma.state())));
        },
    );
}

// ============================================================================
// Test 7: Kill-switch alert has correct schema and fields
// ============================================================================

#[test]
fn kill_switch_alert_schema_and_fields() {
    let harness = TestHarness::new("kill_switch_alert_schema_and_fields");

    let alert =
        SecurityAlert::from_quarantine("ext.schema.check", "consecutive unsafe behavior", 0.92);

    // Schema.
    assert_eq!(alert.schema, SECURITY_ALERT_SCHEMA_VERSION);

    // WHO.
    assert_eq!(alert.extension_id, "ext.schema.check");

    // WHAT.
    assert_eq!(alert.category, SecurityAlertCategory::Quarantine);
    assert_eq!(alert.severity, SecurityAlertSeverity::Critical);

    // ACTION.
    assert_eq!(alert.action, SecurityAlertAction::Terminate);

    // WHY.
    assert_ne!(alert.summary, "");

    // CONTEXT.
    assert!(alert.risk_score > 0.9);

    // Serde roundtrip.
    let json = serde_json::to_string_pretty(&alert).expect("serialize");
    let restored: SecurityAlert = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.category, SecurityAlertCategory::Quarantine);
    assert_eq!(restored.action, SecurityAlertAction::Terminate);
    assert_eq!(restored.extension_id, "ext.schema.check");

    harness.log().info_ctx(
        "kill_switch_schema",
        "alert schema and fields verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("schema".into(), alert.schema.clone()));
        },
    );
}

// ============================================================================
// Test 8: Onboarding risk disclosure — promotion records recommendation
// ============================================================================

#[test]
fn onboarding_records_risk_disclosure() {
    let harness = TestHarness::new("onboarding_records_risk_disclosure");

    let mut tracker = risky_tracker("ext.disclosure");

    // Promote with explicit risk disclosure (recommendation field).
    tracker
        .promote(
            "operator acknowledged known risks",
            true,
            Some(45),
            Some(InstallRecommendation::Review),
        )
        .unwrap();

    let event = &tracker.history()[0];

    // Risk disclosure is captured in the event.
    assert_eq!(event.recommendation, Some(InstallRecommendation::Review));
    assert_eq!(event.risk_score, Some(45));
    assert!(event.operator_acknowledged);

    // JSON export includes the recommendation.
    let json = event.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["recommendation"], "review");
    assert_eq!(parsed["risk_score"], 45);
    assert_eq!(parsed["operator_acknowledged"], true);

    harness.log().info_ctx(
        "risk_disclosure",
        "recommendation captured in event",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
        },
    );
}

// ============================================================================
// Test 9: Kill-switch on restricted extension (mid-onboarding)
// ============================================================================

#[test]
fn kill_switch_during_onboarding_restricted_phase() {
    let harness = TestHarness::new("kill_switch_during_onboarding_restricted");
    let mgr = ExtensionManager::new();

    let mut tracker = risky_tracker("ext.midway");

    // Onboarding step 1: promote to Restricted.
    tracker
        .promote("initial review", true, Some(60), None)
        .unwrap();
    assert_eq!(tracker.state(), ExtensionTrustState::Restricted);

    // During restricted observation, anomaly detected → kill-switch.
    emergency_kill_switch(&mut tracker, &mgr, "anomaly during observation");
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    // Verify all hostcalls blocked.
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "read"));
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "exec"));

    // Audit trail shows: promote + demote.
    let history = tracker.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].to_state, ExtensionTrustState::Restricted);
    assert_eq!(history[1].to_state, ExtensionTrustState::Quarantined);
    assert_eq!(history[1].reason, "anomaly during observation");

    // Alert was recorded.
    let artifact = mgr.security_alert_artifact();
    assert_eq!(artifact.alert_count, 1);
    assert_eq!(
        artifact.alerts[0].category,
        SecurityAlertCategory::Quarantine
    );

    harness.log().info_ctx(
        "mid_onboarding_kill",
        "kill-switch during restricted phase",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("history_len".into(), history.len().to_string()));
        },
    );
}

// ============================================================================
// Test 10: Multiple kill-switches on same extension
// ============================================================================

#[test]
fn multiple_kill_switches_accumulate_in_audit_trail() {
    let harness = TestHarness::new("multiple_kill_switches_accumulate");
    let mgr = ExtensionManager::new();

    let mut tracker = risky_tracker("ext.repeat");

    // Cycle 1: promote → kill-switch.
    tracker.promote("review-1", true, None, None).unwrap();
    tracker.promote("full-1", true, None, None).unwrap();
    emergency_kill_switch(&mut tracker, &mgr, "incident-1");

    // Cycle 2: re-onboard → kill-switch again.
    tracker.promote("review-2", true, Some(70), None).unwrap();
    tracker.promote("full-2", true, Some(80), None).unwrap();
    emergency_kill_switch(&mut tracker, &mgr, "incident-2");

    // State is quarantined.
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    // History has 6 events: 2 promotes + demote + 2 promotes + demote.
    let history = tracker.history();
    assert_eq!(history.len(), 6);

    // Demotions at indices 2 and 5.
    assert_eq!(history[2].to_state, ExtensionTrustState::Quarantined);
    assert_eq!(history[2].reason, "incident-1");
    assert_eq!(history[5].to_state, ExtensionTrustState::Quarantined);
    assert_eq!(history[5].reason, "incident-2");

    // Two quarantine alerts recorded.
    assert_eq!(mgr.security_alert_count(), 2);
    let artifact = mgr.security_alert_artifact();
    assert_eq!(artifact.category_counts.quarantine, 2);

    // JSONL export has 6 lines.
    let jsonl = tracker.history_jsonl().unwrap();
    assert_eq!(jsonl.lines().count(), 6);

    harness.log().info_ctx(
        "multiple_kill_switches",
        "repeated incidents tracked",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-ww5br".into()));
            ctx_log.push(("cycles".into(), "2".into()));
            ctx_log.push(("total_events".into(), history.len().to_string()));
            ctx_log.push((
                "total_alerts".into(),
                mgr.security_alert_count().to_string(),
            ));
        },
    );
}

// ============================================================================
// Test 11: Kill-switch alert sequence IDs are monotonic
// ============================================================================

#[test]
fn kill_switch_alert_sequence_ids_monotonic() {
    let mgr = ExtensionManager::new();

    // Record a mix of alert types.
    mgr.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.seq", "exec", "spawn", "blocked", "policy",
    ));
    mgr.record_security_alert(SecurityAlert::from_quarantine("ext.seq", "anomaly", 0.9));
    mgr.record_security_alert(SecurityAlert::from_secret_redaction("ext.seq", "API_KEY"));
    mgr.record_security_alert(SecurityAlert::from_quarantine("ext.seq", "repeat", 0.95));

    let artifact = mgr.security_alert_artifact();
    let ids: Vec<u64> = artifact.alerts.iter().map(|a| a.sequence_id).collect();

    // Sequence IDs must be monotonically increasing.
    for window in ids.windows(2) {
        assert!(
            window[0] < window[1],
            "sequence IDs must be monotonic: {} >= {}",
            window[0],
            window[1]
        );
    }

    // Quarantine alerts should have highest severity.
    let quarantine_alerts: Vec<&SecurityAlert> = artifact
        .alerts
        .iter()
        .filter(|a| a.category == SecurityAlertCategory::Quarantine)
        .collect();
    assert_eq!(quarantine_alerts.len(), 2);
    for alert in &quarantine_alerts {
        assert_eq!(alert.severity, SecurityAlertSeverity::Critical);
    }
}

// ============================================================================
// Test 12: Trust event JSON export includes all required fields
// ============================================================================

#[test]
fn trust_event_json_includes_all_required_fields() {
    let mut tracker = risky_tracker("ext.fields");
    tracker
        .promote(
            "detailed review",
            true,
            Some(55),
            Some(InstallRecommendation::Review),
        )
        .unwrap();

    let event = &tracker.history()[0];
    let json = event.to_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();

    // Required fields for SEC-5.2 audit compliance.
    assert!(v["schema"].is_string(), "schema must be present");
    assert!(
        v["extension_id"].is_string(),
        "extension_id must be present"
    );
    assert!(v["from_state"].is_string(), "from_state must be present");
    assert!(v["to_state"].is_string(), "to_state must be present");
    assert!(v["kind"].is_string(), "kind must be present");
    assert!(v["reason"].is_string(), "reason must be present");
    assert!(
        v["operator_acknowledged"].is_boolean(),
        "operator_acknowledged must be present"
    );
    assert!(v["timestamp"].is_string(), "timestamp must be present");

    // Optional but expected for onboarding.
    assert!(
        v["risk_score"].is_number(),
        "risk_score must be present when provided"
    );
    assert!(
        v["recommendation"].is_string(),
        "recommendation must be present when provided"
    );
}

// ============================================================================
// Test 13: Hostcall gating categories exhaustive coverage
// ============================================================================

#[test]
fn hostcall_gating_covers_all_category_tiers() {
    // Registration-only: always allowed in every state.
    let registration_caps = [
        "register",
        "tool",
        "slash_command",
        "shortcut",
        "flag",
        "event_hook",
        "log",
    ];
    for cap in &registration_caps {
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Quarantined, cap),
            "quarantined must allow registration cap: {cap}"
        );
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Restricted, cap),
            "restricted must allow registration cap: {cap}"
        );
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Trusted, cap),
            "trusted must allow registration cap: {cap}"
        );
    }

    // Read-only: blocked in quarantine, allowed in restricted+trusted.
    let read_caps = ["read", "list", "stat", "session_read", "ui"];
    for cap in &read_caps {
        assert!(
            !is_hostcall_allowed_for_trust(ExtensionTrustState::Quarantined, cap),
            "quarantined must block read cap: {cap}"
        );
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Restricted, cap),
            "restricted must allow read cap: {cap}"
        );
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Trusted, cap),
            "trusted must allow read cap: {cap}"
        );
    }

    // Dangerous: only trusted.
    let dangerous_caps = [
        "write",
        "exec",
        "env",
        "http",
        "session_write",
        "fs_write",
        "fs_delete",
        "fs_mkdir",
    ];
    for cap in &dangerous_caps {
        assert!(
            !is_hostcall_allowed_for_trust(ExtensionTrustState::Quarantined, cap),
            "quarantined must block dangerous cap: {cap}"
        );
        assert!(
            !is_hostcall_allowed_for_trust(ExtensionTrustState::Restricted, cap),
            "restricted must block dangerous cap: {cap}"
        );
        assert!(
            is_hostcall_allowed_for_trust(ExtensionTrustState::Trusted, cap),
            "trusted must allow dangerous cap: {cap}"
        );
    }
}
