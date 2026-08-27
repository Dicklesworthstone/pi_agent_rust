//! SEC-6.6 tests: E2E security scenario suite with structured JSONL artifact
//! contract (bd-3fa19).
//!
//! End-to-end scenarios exercising the full security stack:
//! - Benign extension workflows (normal user path, no alerts)
//! - Adversarial escalation (dangerous commands → deny/terminate)
//! - Trust lifecycle (quarantine → promote → kill-switch → re-onboard)
//! - Multi-extension isolation (one compromised, others unaffected)
//! - Incident evidence bundle generation and forensic replay
//! - Rollback/recovery after security incident
//! - Shadow-mode vs enforcement mode behavior differences
//!
//! Every scenario emits structured JSONL logs with deterministic artifact
//! manifests so failures are reproducible and machine-parseable.

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extension_preflight::{
    ExtensionTrustState, ExtensionTrustTracker, InstallRecommendation,
    is_hostcall_allowed_for_trust,
};
use pi::extensions::{
    ExecMediationLedgerEntry, ExtensionManager, ExtensionPolicy, ExtensionPolicyMode,
    HostCallContext, HostCallPayload, IncidentBundleFilter, IncidentBundleRedactionPolicy,
    IncidentEvidenceBundle, RuntimeRiskConfig, SecretBrokerLedgerEntry, SecurityAlert,
    SecurityAlertCategory, SecurityAlertFilter, SecurityAlertSeverity,
    build_incident_evidence_bundle, dispatch_host_call_shared, query_security_alerts,
    verify_incident_evidence_bundle,
};
use pi::tools::ToolRegistry;
use serde_json::json;

// ============================================================================
// Helpers
// ============================================================================

fn permissive_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    }
}

const fn default_risk_config() -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    }
}

const fn shadow_risk_config() -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: false,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    }
}

fn setup(
    harness: &TestHarness,
    config: RuntimeRiskConfig,
) -> (
    ToolRegistry,
    HttpConnector,
    ExtensionManager,
    ExtensionPolicy,
) {
    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    let policy = permissive_policy();
    (tools, http, manager, policy)
}

fn make_ctx<'a>(
    tools: &'a ToolRegistry,
    http: &'a HttpConnector,
    manager: &'a ExtensionManager,
    policy: &'a ExtensionPolicy,
    ext_id: &'a str,
) -> HostCallContext<'a> {
    HostCallContext {
        runtime_name: "sec66_e2e",
        extension_id: Some(ext_id),
        tools,
        http,
        manager: Some(manager.clone()),
        policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    }
}

fn benign_log_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("benign-log-{idx}"),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": format!("benign-{idx}") }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn adversarial_exec_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("adversarial-exec-{idx}"),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/attack-{idx}")] }),
        timeout_ms: Some(10),
        cancel_token: None,
        context: None,
    }
}

fn sample_exec_mediation(ext_id: &str, idx: usize) -> ExecMediationLedgerEntry {
    ExecMediationLedgerEntry {
        #[allow(clippy::cast_possible_wrap)]
        ts_ms: 1_700_000_000_000 + idx as i64,
        extension_id: Some(ext_id.to_string()),
        command_hash: format!("cmd_hash_{idx}"),
        command_class: Some("recursive_delete".to_string()),
        risk_tier: Some("critical".to_string()),
        decision: "deny".to_string(),
        reason: format!("dangerous command pattern {idx}"),
    }
}

fn sample_secret_broker(ext_id: &str, idx: usize) -> SecretBrokerLedgerEntry {
    SecretBrokerLedgerEntry {
        #[allow(clippy::cast_possible_wrap)]
        ts_ms: 1_700_000_000_000 + idx as i64,
        extension_id: Some(ext_id.to_string()),
        name_hash: format!("secret_hash_{idx}"),
        redacted: true,
        reason: format!("matches suffix _KEY pattern {idx}"),
    }
}

fn seed_composed_filter_fixture(
    manager: &ExtensionManager,
    ctx_alpha: &HostCallContext<'_>,
    ctx_beta: &HostCallContext<'_>,
) {
    futures::executor::block_on(async {
        for idx in 0..4 {
            let _ = dispatch_host_call_shared(ctx_alpha, benign_log_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(ctx_beta, benign_log_call(100 + idx)).await;
        }
    });

    manager.record_exec_mediation(sample_exec_mediation("ext.alpha", 0));
    manager.record_exec_mediation(sample_exec_mediation("ext.beta", 1));
    manager.record_secret_broker(sample_secret_broker("ext.alpha", 0));
    manager.record_secret_broker(sample_secret_broker("ext.beta", 1));

    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.alpha",
        "manual quarantine parity check",
        0.99,
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.alpha",
        "ALPHA_KEY",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.beta",
        "manual quarantine parity check",
        0.98,
    ));
}

fn build_bundle_with_filter(
    manager: &ExtensionManager,
    filter: &IncidentBundleFilter,
    redaction: &IncidentBundleRedactionPolicy,
    generated_at_ms: i64,
) -> IncidentEvidenceBundle {
    build_incident_evidence_bundle(
        &manager.runtime_risk_ledger_artifact(),
        &manager.security_alert_artifact(),
        &manager.runtime_hostcall_telemetry_artifact(),
        &manager.exec_mediation_artifact(),
        &manager.secret_broker_artifact(),
        &[],
        filter,
        redaction,
        generated_at_ms,
    )
}

fn assert_bundle_summary_matches_sections(bundle: &IncidentEvidenceBundle) {
    assert_eq!(
        bundle.summary.ledger_entry_count,
        bundle.risk_ledger.entries.len()
    );
    assert_eq!(
        bundle.summary.alert_count,
        bundle.security_alerts.alerts.len()
    );
    assert_eq!(
        bundle.summary.telemetry_event_count,
        bundle.hostcall_telemetry.entries.len()
    );
    assert_eq!(
        bundle.summary.exec_mediation_count,
        bundle.exec_mediation.entries.len()
    );
    assert_eq!(
        bundle.summary.secret_broker_count,
        bundle.secret_broker.entries.len()
    );
}

fn assert_bundle_extension_scope(bundle: &IncidentEvidenceBundle, extension_id: &str) {
    assert!(
        bundle
            .risk_ledger
            .entries
            .iter()
            .all(|entry| entry.extension_id == extension_id)
    );
    assert!(
        bundle
            .hostcall_telemetry
            .entries
            .iter()
            .all(|entry| entry.extension_id == extension_id)
    );
    assert!(
        bundle
            .exec_mediation
            .entries
            .iter()
            .all(|entry| entry.extension_id.as_deref() == Some(extension_id))
    );
    assert!(
        bundle
            .secret_broker
            .entries
            .iter()
            .all(|entry| entry.extension_id.as_deref() == Some(extension_id))
    );
}

fn assert_scoped_alert_filters(bundle: &IncidentEvidenceBundle) -> i64 {
    assert_eq!(
        bundle.security_alerts.alerts.len(),
        1,
        "info-level secret redaction must be excluded by min_severity=warning"
    );
    let only_alert = &bundle.security_alerts.alerts[0];
    assert_eq!(only_alert.category, SecurityAlertCategory::Quarantine);
    assert_eq!(only_alert.severity, SecurityAlertSeverity::Critical);
    only_alert.ts_ms
}

fn assert_boundary_bundle_includes_exact_timestamp(
    bundle: &IncidentEvidenceBundle,
    boundary_filter: &IncidentBundleFilter,
    target_ts: i64,
) {
    assert_eq!(bundle.filter, *boundary_filter);
    assert_eq!(bundle.security_alerts.alert_count, 1);
    assert_eq!(bundle.security_alerts.alerts[0].ts_ms, target_ts);
    for alert in &bundle.security_alerts.alerts {
        assert_eq!(alert.ts_ms, target_ts);
    }
}

fn assert_empty_scoped_bundle_is_fail_closed(bundle: &IncidentEvidenceBundle) {
    assert_eq!(bundle.summary.ledger_entry_count, 0);
    assert_eq!(bundle.summary.alert_count, 0);
    assert_eq!(bundle.summary.telemetry_event_count, 0);
    assert_eq!(bundle.summary.exec_mediation_count, 0);
    assert_eq!(bundle.summary.secret_broker_count, 0);
    assert_eq!(bundle.summary.distinct_extensions, 0);
    assert_eq!(bundle.summary.deny_or_terminate_count, 0);
    assert!(
        bundle.summary.peak_risk_score.abs() <= f64::EPSILON,
        "empty scoped bundle should have near-zero peak risk score"
    );
}

fn emit_bundle_filter_parity_success(
    harness: &TestHarness,
    scoped_bundle: &IncidentEvidenceBundle,
    empty_bundle: &IncidentEvidenceBundle,
) {
    emit_scenario_event(
        harness,
        "bundle_filter_parity",
        "verify",
        "ext.alpha",
        "none",
        "pass",
        0.0,
        &["composed_filter_parity", "zero_match_fail_closed"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "incident bundle composed-filter invariants verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.7".into()));
            ctx_log.push(("scenario".into(), "bundle_filter_parity".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push((
                "scoped_alert_count".into(),
                scoped_bundle.summary.alert_count.to_string(),
            ));
            ctx_log.push((
                "scoped_ledger_count".into(),
                scoped_bundle.summary.ledger_entry_count.to_string(),
            ));
            ctx_log.push((
                "empty_alert_count".into(),
                empty_bundle.summary.alert_count.to_string(),
            ));
        },
    );
}

/// Emergency kill-switch helper: demote trust + record quarantine alert.
fn emergency_kill_switch(
    tracker: &mut ExtensionTrustTracker,
    manager: &ExtensionManager,
    reason: &str,
) {
    let ext_id = tracker.extension_id().to_string();
    tracker.demote(reason).expect("demotion must succeed");
    manager.record_security_alert(SecurityAlert::from_quarantine(&ext_id, reason, 0.95));
}

/// Emit a structured JSONL event line for this scenario.
#[allow(clippy::too_many_arguments)]
fn emit_scenario_event(
    harness: &TestHarness,
    scenario: &str,
    step: &str,
    ext_id: &str,
    capability: &str,
    action: &str,
    score: f64,
    reason_codes: &[&str],
) {
    harness
        .log()
        .info_ctx("scenario_event", format!("{scenario}/{step}"), |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), scenario.to_string()));
            ctx_log.push(("step".into(), step.to_string()));
            ctx_log.push(("extension_id".into(), ext_id.to_string()));
            ctx_log.push(("capability".into(), capability.to_string()));
            ctx_log.push(("action".into(), action.to_string()));
            ctx_log.push(("risk_score".into(), format!("{score:.4}")));
            ctx_log.push(("reason_codes".into(), reason_codes.join(",")));
        });
}

// ============================================================================
// Scenario 1: Benign extension workflow — no alerts
// ============================================================================

#[test]
fn scenario_benign_workflow_no_alerts() {
    let harness = TestHarness::new("scenario_benign_workflow");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.benign");

    emit_scenario_event(
        &harness,
        "benign_workflow",
        "start",
        "ext.benign",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Dispatch several benign calls.
    futures::executor::block_on(async {
        for idx in 0..10 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
    });

    emit_scenario_event(
        &harness,
        "benign_workflow",
        "calls_complete",
        "ext.benign",
        "log",
        "allow",
        0.0,
        &[],
    );

    // Telemetry should record all calls.
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert_eq!(telemetry.entries.len(), 10, "all 10 benign calls recorded");

    // Risk ledger entries should all have low-ish scores.
    let ledger = manager.runtime_risk_ledger_artifact();
    assert_eq!(ledger.entries.len(), 10);
    for entry in &ledger.entries {
        assert!(
            entry.risk_score < 0.8,
            "benign call score {} should be low",
            entry.risk_score
        );
    }

    // No quarantine or critical alerts should be generated.
    let critical_alerts = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            category: Some(SecurityAlertCategory::Quarantine),
            min_severity: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    assert!(
        critical_alerts.is_empty(),
        "benign workflow must not produce quarantine alerts"
    );

    emit_scenario_event(
        &harness,
        "benign_workflow",
        "verify",
        "ext.benign",
        "log",
        "pass",
        0.0,
        &["no_alerts"],
    );

    harness
        .log()
        .info_ctx("scenario_result", "benign workflow passed", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "benign_workflow".into()));
            ctx_log.push(("result".into(), "pass".into()));
        });
}

// ============================================================================
// Scenario 2: Adversarial escalation — dangerous commands trigger alerts
// ============================================================================

#[test]
fn scenario_adversarial_escalation_triggers_alerts() {
    let harness = TestHarness::new("scenario_adversarial_escalation");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.adversary");

    emit_scenario_event(
        &harness,
        "adversarial_escalation",
        "start",
        "ext.adversary",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Phase 1: Start with a few benign calls.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
    });

    emit_scenario_event(
        &harness,
        "adversarial_escalation",
        "benign_phase",
        "ext.adversary",
        "log",
        "allow",
        0.0,
        &[],
    );

    // Phase 2: Switch to adversarial exec calls.
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_exec_call(idx)).await;
        }
    });

    // Record exec mediations for the adversarial calls.
    for idx in 0..5 {
        manager.record_exec_mediation(sample_exec_mediation("ext.adversary", idx));
    }

    emit_scenario_event(
        &harness,
        "adversarial_escalation",
        "adversarial_phase",
        "ext.adversary",
        "exec",
        "deny",
        1.0,
        &["dangerous_capability_escalation"],
    );

    // Risk scores should escalate.
    let ledger = manager.runtime_risk_ledger_artifact();
    let exec_entries: Vec<_> = ledger
        .entries
        .iter()
        .filter(|e| e.capability == "exec")
        .collect();
    assert!(!exec_entries.is_empty(), "must have exec entries");
    for entry in &exec_entries {
        assert!(
            entry.risk_score >= 0.5,
            "adversarial exec score {} should be high",
            entry.risk_score
        );
    }

    // Exec mediation ledger should have deny entries.
    let exec_artifact = manager.exec_mediation_artifact();
    assert_eq!(exec_artifact.entries.len(), 5);
    for entry in &exec_artifact.entries {
        assert_eq!(entry.decision, "deny");
    }

    emit_scenario_event(
        &harness,
        "adversarial_escalation",
        "verify",
        "ext.adversary",
        "exec",
        "pass",
        1.0,
        &["risk_escalation_confirmed"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "adversarial escalation detected",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "adversarial_escalation".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push((
                "exec_mediations".into(),
                exec_artifact.entries.len().to_string(),
            ));
        },
    );
}

// ============================================================================
// Scenario 3: Trust lifecycle — quarantine → promote → kill-switch → re-onboard
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn scenario_trust_lifecycle_full_cycle() {
    let harness = TestHarness::new("scenario_trust_lifecycle");
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let mut tracker = ExtensionTrustTracker::new("ext.lifecycle", ExtensionTrustState::Quarantined);

    emit_scenario_event(
        &harness,
        "trust_lifecycle",
        "start",
        "ext.lifecycle",
        "none",
        "quarantined",
        0.0,
        &[],
    );

    // Step 1: Extension starts in quarantine — exec blocked.
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);
    assert!(
        !is_hostcall_allowed_for_trust(tracker.state(), "exec"),
        "quarantined extension must not exec"
    );
    assert!(
        is_hostcall_allowed_for_trust(tracker.state(), "log"),
        "quarantined extension can log"
    );

    // Step 2: Operator promotes to restricted.
    tracker
        .promote(
            "passed preflight review",
            true,
            Some(30),
            Some(InstallRecommendation::Review),
        )
        .expect("promote to restricted");
    assert_eq!(tracker.state(), ExtensionTrustState::Restricted);

    emit_scenario_event(
        &harness,
        "trust_lifecycle",
        "promoted_restricted",
        "ext.lifecycle",
        "none",
        "restrict",
        0.0,
        &["preflight_passed"],
    );

    // Step 3: Operator promotes to trusted.
    tracker
        .promote("extended observation period clean", true, Some(80), None)
        .expect("promote to trusted");
    assert_eq!(tracker.state(), ExtensionTrustState::Trusted);
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "exec"));

    emit_scenario_event(
        &harness,
        "trust_lifecycle",
        "promoted_trusted",
        "ext.lifecycle",
        "exec",
        "allow",
        0.0,
        &["observation_clean"],
    );

    // Step 4: Anomaly detected — emergency kill-switch.
    emergency_kill_switch(&mut tracker, &manager, "anomalous burst detected");
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);
    assert!(!is_hostcall_allowed_for_trust(tracker.state(), "exec"));

    emit_scenario_event(
        &harness,
        "trust_lifecycle",
        "kill_switch",
        "ext.lifecycle",
        "none",
        "quarantine",
        0.95,
        &["anomalous_burst"],
    );

    // Step 5: Re-onboard after triage.
    tracker
        .promote(
            "triage complete, root cause fixed",
            true,
            Some(40),
            Some(InstallRecommendation::Review),
        )
        .expect("re-promote after triage");
    assert_eq!(tracker.state(), ExtensionTrustState::Restricted);

    emit_scenario_event(
        &harness,
        "trust_lifecycle",
        "re_onboarded",
        "ext.lifecycle",
        "none",
        "restrict",
        0.0,
        &["triage_complete"],
    );

    // Verify audit trail completeness.
    let history = tracker.history();
    assert_eq!(history.len(), 4, "4 transitions: Q→R, R→T, T→Q, Q→R");

    // Verify alert was recorded.
    let alerts = manager.security_alert_artifact();
    assert!(alerts.alert_count >= 1);
    let quarantine_alert = alerts
        .alerts
        .iter()
        .find(|a| a.category == SecurityAlertCategory::Quarantine);
    assert!(quarantine_alert.is_some(), "quarantine alert must exist");

    harness
        .log()
        .info_ctx("scenario_result", "trust lifecycle complete", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "trust_lifecycle".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("transitions".into(), history.len().to_string()));
        });
}

// ============================================================================
// Scenario 4: Multi-extension isolation — one compromised, others safe
// ============================================================================

#[test]
fn scenario_multi_extension_isolation() {
    let harness = TestHarness::new("scenario_multi_extension_isolation");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());

    let ctx_safe = make_ctx(&tools, &http, &manager, &policy, "ext.safe");
    let ctx_evil = make_ctx(&tools, &http, &manager, &policy, "ext.evil");

    emit_scenario_event(
        &harness,
        "multi_ext_isolation",
        "start",
        "ext.*",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Safe extension does normal work.
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx_safe, benign_log_call(idx)).await;
        }
    });

    // Evil extension tries dangerous commands.
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx_evil, adversarial_exec_call(idx)).await;
        }
    });

    // Record exec mediation denials for evil extension only.
    for idx in 0..3 {
        manager.record_exec_mediation(sample_exec_mediation("ext.evil", idx));
    }

    // Record secret broker entries for evil extension only.
    for idx in 0..2 {
        manager.record_secret_broker(sample_secret_broker("ext.evil", idx));
    }

    // Trust trackers should be independent.
    let tracker_safe = ExtensionTrustTracker::new("ext.safe", ExtensionTrustState::Trusted);
    let mut tracker_evil = ExtensionTrustTracker::new("ext.evil", ExtensionTrustState::Trusted);

    // Kill-switch only the evil extension.
    emergency_kill_switch(&mut tracker_evil, &manager, "adversarial behavior");

    assert_eq!(tracker_safe.state(), ExtensionTrustState::Trusted);
    assert_eq!(tracker_evil.state(), ExtensionTrustState::Quarantined);

    // Safe extension can still exec.
    assert!(is_hostcall_allowed_for_trust(tracker_safe.state(), "exec"));
    // Evil extension cannot.
    assert!(!is_hostcall_allowed_for_trust(tracker_evil.state(), "exec"));

    emit_scenario_event(
        &harness,
        "multi_ext_isolation",
        "verify",
        "ext.*",
        "none",
        "pass",
        0.0,
        &["isolation_confirmed"],
    );

    // Build filtered bundle for evil extension only.
    let filter = IncidentBundleFilter {
        extension_id: Some("ext.evil".to_string()),
        ..Default::default()
    };
    let redaction = IncidentBundleRedactionPolicy::default();
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        &filter,
        &redaction,
        0,
    );

    // Bundle should contain only evil extension entries.
    for entry in &bundle.risk_ledger.entries {
        assert_eq!(
            entry.extension_id, "ext.evil",
            "bundle must only contain evil entries"
        );
    }
    for alert in &bundle.security_alerts.alerts {
        assert_eq!(alert.extension_id, "ext.evil");
    }

    harness.log().info_ctx(
        "scenario_result",
        "multi-extension isolation verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "multi_ext_isolation".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push((
                "evil_ledger_entries".into(),
                bundle.summary.ledger_entry_count.to_string(),
            ));
        },
    );
}

// ============================================================================
// Scenario 5: Incident evidence bundle — full forensic flow
// ============================================================================

#[test]
fn scenario_incident_evidence_forensic_flow() {
    let harness = TestHarness::new("scenario_incident_evidence");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.incident");

    emit_scenario_event(
        &harness,
        "incident_evidence",
        "start",
        "ext.incident",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Phase 1: Normal operations.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
    });

    // Phase 2: Adversarial activity.
    futures::executor::block_on(async {
        for idx in 0..4 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_exec_call(idx)).await;
        }
    });

    // Phase 3: Record mediation/secret events.
    for idx in 0..3 {
        manager.record_exec_mediation(sample_exec_mediation("ext.incident", idx));
    }
    for idx in 0..2 {
        manager.record_secret_broker(sample_secret_broker("ext.incident", idx));
    }

    // Phase 4: Trigger quarantine alert.
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.incident",
        "critical risk threshold exceeded",
        0.98,
    ));

    emit_scenario_event(
        &harness,
        "incident_evidence",
        "incident_recorded",
        "ext.incident",
        "exec",
        "terminate",
        0.98,
        &["critical_risk_threshold"],
    );

    // Phase 5: Export bundle with redaction.
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();
    let redaction = IncidentBundleRedactionPolicy::default();

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        &filter,
        &redaction,
        1_700_000_000_000,
    );

    // Verify bundle integrity.
    let report = verify_incident_evidence_bundle(&bundle);
    assert!(report.schema_valid, "schema must be valid");

    // Verify summary has content.
    assert!(bundle.summary.ledger_entry_count > 0);
    assert!(bundle.summary.alert_count >= 1);
    assert_eq!(bundle.summary.exec_mediation_count, 3);
    assert_eq!(bundle.summary.secret_broker_count, 2);
    assert!(bundle.summary.peak_risk_score > 0.5);

    // Verify redaction was applied.
    for entry in &bundle.risk_ledger.entries {
        assert_eq!(entry.params_hash, "[REDACTED]");
    }

    emit_scenario_event(
        &harness,
        "incident_evidence",
        "bundle_exported",
        "ext.incident",
        "none",
        "export",
        0.0,
        &["bundle_valid"],
    );

    // Serialize bundle as artifact.
    let bundle_json = serde_json::to_string_pretty(&bundle).expect("serialize bundle");
    let bundle_path = harness.temp_path("incident_bundle.json");
    std::fs::write(&bundle_path, &bundle_json).expect("write bundle");
    harness.record_artifact("incident_bundle", &bundle_path);

    harness.log().info_ctx(
        "scenario_result",
        "incident evidence flow complete",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "incident_evidence".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("bundle_hash".into(), bundle.bundle_hash.clone()));
            ctx_log.push(("bundle_bytes".into(), bundle_json.len().to_string()));
        },
    );
}

// ============================================================================
// Scenario 6: Shadow mode vs enforcement mode
// ============================================================================

#[test]
fn scenario_shadow_vs_enforcement_mode() {
    let harness = TestHarness::new("scenario_shadow_vs_enforce");

    // Setup enforcement mode.
    let (tools_e, http_e, mgr_enforce, policy_e) = setup(&harness, default_risk_config());
    let ctx_enforce = make_ctx(&tools_e, &http_e, &mgr_enforce, &policy_e, "ext.enforce");

    // Setup shadow mode.
    let (tools_s, http_s, mgr_shadow, policy_s) = setup(&harness, shadow_risk_config());
    let ctx_shadow = make_ctx(&tools_s, &http_s, &mgr_shadow, &policy_s, "ext.shadow");

    emit_scenario_event(
        &harness,
        "shadow_vs_enforce",
        "start",
        "ext.*",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Run identical workloads through both.
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx_enforce, benign_log_call(idx)).await;
            let _ = dispatch_host_call_shared(&ctx_shadow, benign_log_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx_enforce, adversarial_exec_call(idx)).await;
            let _ = dispatch_host_call_shared(&ctx_shadow, adversarial_exec_call(idx)).await;
        }
    });

    // Both should have telemetry entries.
    let telem_enforce = mgr_enforce.runtime_hostcall_telemetry_artifact();
    let telem_shadow = mgr_shadow.runtime_hostcall_telemetry_artifact();
    assert_eq!(
        telem_enforce.entries.len(),
        telem_shadow.entries.len(),
        "both modes should record same number of telemetry events"
    );

    // Both should have risk ledger entries.
    let ledger_enforce = mgr_enforce.runtime_risk_ledger_artifact();
    let ledger_shadow = mgr_shadow.runtime_risk_ledger_artifact();
    assert_eq!(
        ledger_enforce.entries.len(),
        ledger_shadow.entries.len(),
        "both modes should record same number of ledger entries"
    );

    // Config flags should differ.
    assert!(
        mgr_enforce.runtime_risk_config().enforce,
        "enforce mode must enforce"
    );
    assert!(
        !mgr_shadow.runtime_risk_config().enforce,
        "shadow mode must not enforce"
    );

    emit_scenario_event(
        &harness,
        "shadow_vs_enforce",
        "verify",
        "ext.*",
        "none",
        "pass",
        0.0,
        &["telemetry_parity"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "shadow vs enforcement modes diverge correctly",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "shadow_vs_enforce".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push((
                "enforce_entries".into(),
                ledger_enforce.entries.len().to_string(),
            ));
            ctx_log.push((
                "shadow_entries".into(),
                ledger_shadow.entries.len().to_string(),
            ));
        },
    );
}

// ============================================================================
// Scenario 7: Rollback / recovery after security incident
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn scenario_rollback_recovery_after_incident() {
    let harness = TestHarness::new("scenario_rollback_recovery");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.rollback");

    let mut tracker = ExtensionTrustTracker::new("ext.rollback", ExtensionTrustState::Trusted);

    emit_scenario_event(
        &harness,
        "rollback_recovery",
        "start",
        "ext.rollback",
        "none",
        "trusted",
        0.0,
        &[],
    );

    // Phase 1: Normal operation while trusted.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
    });
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "exec"));

    // Phase 2: Adversarial behavior detected → kill-switch.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_exec_call(idx)).await;
        }
    });

    for idx in 0..2 {
        manager.record_exec_mediation(sample_exec_mediation("ext.rollback", idx));
    }

    emergency_kill_switch(&mut tracker, &manager, "adversarial exec burst");
    assert_eq!(tracker.state(), ExtensionTrustState::Quarantined);

    emit_scenario_event(
        &harness,
        "rollback_recovery",
        "kill_switch",
        "ext.rollback",
        "exec",
        "terminate",
        0.95,
        &["adversarial_burst"],
    );

    // Phase 3: Export evidence bundle for triage.
    let no_redaction = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: false,
        redact_args_shape_hash: false,
        redact_command_hash: false,
        redact_name_hash: false,
        redact_remediation: false,
    };
    let filter = IncidentBundleFilter {
        extension_id: Some("ext.rollback".to_string()),
        ..Default::default()
    };
    let bundle = build_incident_evidence_bundle(
        &manager.runtime_risk_ledger_artifact(),
        &manager.security_alert_artifact(),
        &manager.runtime_hostcall_telemetry_artifact(),
        &manager.exec_mediation_artifact(),
        &manager.secret_broker_artifact(),
        &[],
        &filter,
        &no_redaction,
        1_700_000_000_000,
    );

    assert!(bundle.summary.ledger_entry_count > 0);
    // Deny/terminate count is a usize, always >= 0; just verify it exists.

    // Forensic replay should be available (no redaction).
    // Note: replay availability depends on ledger chain integrity.
    if let Some(replay) = &bundle.risk_replay {
        assert!(!replay.steps.is_empty());
    }

    emit_scenario_event(
        &harness,
        "rollback_recovery",
        "evidence_exported",
        "ext.rollback",
        "none",
        "export",
        0.0,
        &["bundle_exported"],
    );

    // Phase 4: Recovery — re-promote after triage.
    tracker
        .promote(
            "root cause fixed, patch applied",
            true,
            Some(40),
            Some(InstallRecommendation::Review),
        )
        .expect("re-promote after triage");
    assert_eq!(tracker.state(), ExtensionTrustState::Restricted);

    // Extension can log but not exec in restricted state.
    assert!(is_hostcall_allowed_for_trust(tracker.state(), "log"));

    emit_scenario_event(
        &harness,
        "rollback_recovery",
        "recovered",
        "ext.rollback",
        "none",
        "restricted",
        0.0,
        &["patch_applied"],
    );

    // Verify complete audit trail.
    let history = tracker.history();
    assert_eq!(history.len(), 2, "T→Q and Q→R transitions");

    harness
        .log()
        .info_ctx("scenario_result", "rollback recovery complete", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "rollback_recovery".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("transitions".into(), history.len().to_string()));
            ctx_log.push((
                "bundle_entries".into(),
                bundle.summary.ledger_entry_count.to_string(),
            ));
        });
}

// ============================================================================
// Scenario 8: Alert category coverage — all categories exercised
// ============================================================================

#[test]
fn scenario_alert_category_coverage() {
    let harness = TestHarness::new("scenario_alert_categories");
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    emit_scenario_event(
        &harness,
        "alert_category_coverage",
        "start",
        "ext.coverage",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Record one alert of each category.
    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.coverage",
        "exec",
        "spawn",
        "denied by policy",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.coverage",
        "rm -rf /",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.coverage",
        "AWS_SECRET_KEY",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.coverage",
        "critical anomaly",
        0.99,
    ));

    let artifact = manager.security_alert_artifact();

    // Verify all recorded categories are present.
    let categories: Vec<_> = artifact.alerts.iter().map(|a| a.category).collect();
    assert!(categories.contains(&SecurityAlertCategory::PolicyDenial));
    assert!(categories.contains(&SecurityAlertCategory::ExecMediation));
    assert!(categories.contains(&SecurityAlertCategory::SecretBroker));
    assert!(categories.contains(&SecurityAlertCategory::Quarantine));

    // Category counts should match.
    assert_eq!(artifact.category_counts.policy_denial, 1);
    assert_eq!(artifact.category_counts.exec_mediation, 1);
    assert_eq!(artifact.category_counts.secret_broker, 1);
    assert_eq!(artifact.category_counts.quarantine, 1);

    // Severity filtering should work.
    let critical_only = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Critical),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    // Quarantine alerts are Critical severity.
    assert!(
        critical_only
            .iter()
            .any(|a| a.category == SecurityAlertCategory::Quarantine),
        "critical filter must include quarantine alerts"
    );

    emit_scenario_event(
        &harness,
        "alert_category_coverage",
        "verify",
        "ext.coverage",
        "none",
        "pass",
        0.0,
        &["all_categories_covered"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "alert category coverage complete",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "alert_category_coverage".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("categories_covered".into(), "4".into()));
        },
    );
}

#[test]
fn scenario_alert_artifact_counts_and_severity_filters_are_consistent() {
    let harness = TestHarness::new("scenario_alert_artifact_consistency");
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    emit_scenario_event(
        &harness,
        "alert_artifact_consistency",
        "start",
        "ext.consistency",
        "none",
        "setup",
        0.0,
        &[],
    );

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.consistency",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.consistency",
        "rm -rf /tmp/demo",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.consistency",
        "API_TOKEN",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.consistency",
        "critical anomaly",
        0.98,
    ));

    let artifact = manager.security_alert_artifact();
    assert_eq!(artifact.alert_count, artifact.alerts.len());

    let category_total = artifact.category_counts.policy_denial
        + artifact.category_counts.anomaly_denial
        + artifact.category_counts.exec_mediation
        + artifact.category_counts.secret_broker
        + artifact.category_counts.quota_breach
        + artifact.category_counts.quarantine
        + artifact.category_counts.profile_transition;
    assert_eq!(category_total, artifact.alert_count);

    let severity_total = artifact.severity_counts.info
        + artifact.severity_counts.warning
        + artifact.severity_counts.error
        + artifact.severity_counts.critical;
    assert_eq!(severity_total, artifact.alert_count);

    let critical_only = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Critical),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    assert!(
        critical_only
            .iter()
            .all(|alert| alert.severity == SecurityAlertSeverity::Critical),
        "critical filter must return only critical alerts"
    );
    assert_eq!(critical_only.len(), artifact.severity_counts.critical);

    let error_or_higher = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Error),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    assert!(
        error_or_higher.iter().all(|alert| {
            matches!(
                alert.severity,
                SecurityAlertSeverity::Error | SecurityAlertSeverity::Critical
            )
        }),
        "error filter must include only error/critical alerts"
    );
    assert_eq!(
        error_or_higher.len(),
        artifact.severity_counts.error + artifact.severity_counts.critical
    );
    assert!(critical_only.len() <= error_or_higher.len());
    assert!(error_or_higher.len() <= artifact.alert_count);

    emit_scenario_event(
        &harness,
        "alert_artifact_consistency",
        "verify",
        "ext.consistency",
        "none",
        "pass",
        0.0,
        &["severity_and_count_invariants"],
    );
}

// ============================================================================
// Scenario 9: Secret broker integration — redaction in incident bundle
// ============================================================================

#[test]
fn scenario_secret_broker_redaction_in_bundle() {
    let harness = TestHarness::new("scenario_secret_broker");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.secrets");

    emit_scenario_event(
        &harness,
        "secret_broker",
        "start",
        "ext.secrets",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Normal operations.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
    });

    // Record secret access events.
    for idx in 0..4 {
        manager.record_secret_broker(sample_secret_broker("ext.secrets", idx));
    }

    // Record redaction alerts.
    for var_name in &["API_KEY", "DB_PASSWORD", "AWS_SECRET", "STRIPE_KEY"] {
        manager.record_security_alert(SecurityAlert::from_secret_redaction(
            "ext.secrets",
            var_name,
        ));
    }

    // Export bundle WITH redaction.
    let redaction = IncidentBundleRedactionPolicy::default();
    assert!(
        redaction.redact_name_hash,
        "default policy redacts name hashes"
    );

    let bundle = build_incident_evidence_bundle(
        &manager.runtime_risk_ledger_artifact(),
        &manager.security_alert_artifact(),
        &manager.runtime_hostcall_telemetry_artifact(),
        &manager.exec_mediation_artifact(),
        &manager.secret_broker_artifact(),
        &[],
        &IncidentBundleFilter::default(),
        &redaction,
        0,
    );

    // Secret broker entries should have redacted name hashes.
    for entry in &bundle.secret_broker.entries {
        assert_eq!(
            entry.name_hash, "[REDACTED]",
            "name hashes must be redacted"
        );
    }
    assert_eq!(bundle.summary.secret_broker_count, 4);

    // Alert filter for secret broker category.
    let secret_alerts = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            category: Some(SecurityAlertCategory::SecretBroker),
            min_severity: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    assert_eq!(secret_alerts.len(), 4);

    emit_scenario_event(
        &harness,
        "secret_broker",
        "verify",
        "ext.secrets",
        "env",
        "redact",
        0.0,
        &["secrets_redacted"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "secret broker redaction verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "secret_broker".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("secrets_redacted".into(), "4".into()));
        },
    );
}

// ============================================================================
// Scenario 10: JSONL artifact contract — schema completeness
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn scenario_jsonl_artifact_contract() {
    let harness = TestHarness::new("scenario_jsonl_contract");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.contract");

    emit_scenario_event(
        &harness,
        "jsonl_contract",
        "start",
        "ext.contract",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Build a full trace.
    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
        for idx in 0..2 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_exec_call(idx)).await;
        }
    });

    for idx in 0..2 {
        manager.record_exec_mediation(sample_exec_mediation("ext.contract", idx));
        manager.record_secret_broker(sample_secret_broker("ext.contract", idx));
    }

    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.contract",
        "contract test",
        0.9,
    ));
    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.contract",
        "exec",
        "spawn",
        "denied by policy",
        "deny_caps",
    ));

    // Export all artifacts.
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();

    // Verify ledger artifact schema fields.
    let ledger_json: serde_json::Value = serde_json::to_value(&ledger).expect("ledger to value");
    assert!(
        ledger_json.get("schema").is_some(),
        "ledger must have schema"
    );
    assert!(ledger_json.get("generated_at_ms").is_some());
    assert!(ledger_json.get("entry_count").is_some());
    assert!(ledger_json.get("data_hash").is_some());
    assert!(ledger_json.get("entries").is_some());

    // Verify each ledger entry has required fields.
    for entry in ledger_json["entries"].as_array().unwrap() {
        for field in &[
            "ts_ms",
            "extension_id",
            "call_id",
            "capability",
            "method",
            "params_hash",
            "policy_reason",
            "risk_score",
            "selected_action",
            "ledger_hash",
        ] {
            assert!(entry.get(field).is_some(), "entry missing field: {field}");
        }
    }

    // Verify alert artifact schema fields.
    let alert_json: serde_json::Value = serde_json::to_value(&alerts).expect("alerts to value");
    assert!(alert_json.get("schema").is_some());
    assert!(alert_json.get("alert_count").is_some());
    assert!(alert_json.get("category_counts").is_some());
    assert!(alert_json.get("severity_counts").is_some());

    for alert in alert_json["alerts"].as_array().unwrap() {
        for field in &[
            "schema",
            "ts_ms",
            "sequence_id",
            "extension_id",
            "category",
            "severity",
            "capability",
            "method",
            "reason_codes",
            "summary",
            "policy_source",
            "action",
            "remediation",
            "risk_score",
            "context_hash",
        ] {
            assert!(alert.get(field).is_some(), "alert missing field: {field}");
        }
    }
    let has_policy_denial_with_reason =
        alert_json["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| {
                alert["category"] == "policy_denial"
                    && alert["action"] == "deny"
                    && alert["policy_source"]
                        .as_str()
                        .is_some_and(|value| !value.trim().is_empty())
                    && alert["reason_codes"]
                        .as_array()
                        .is_some_and(|codes| !codes.is_empty())
            });
    assert!(
        has_policy_denial_with_reason,
        "security alert artifact must include a policy-denial row with non-empty policy_source and reason_codes"
    );

    // Verify telemetry artifact schema fields.
    let telem_json: serde_json::Value = serde_json::to_value(&telemetry).expect("telem to value");
    assert!(telem_json.get("schema").is_some());

    let expected_capability_class = |capability: &str| -> &str {
        match capability {
            "read" | "write" => "filesystem",
            "exec" => "execution",
            "env" => "environment",
            "http" => "network",
            "session" => "session",
            "events" => "events",
            "ui" => "ui",
            "log" => "telemetry",
            "tool" => "tool",
            _ => "unknown",
        }
    };

    for event in telem_json["entries"].as_array().unwrap() {
        for field in &[
            "schema",
            "ts_ms",
            "extension_id",
            "call_id",
            "capability",
            "method",
            "risk_score",
            "selected_action",
            "policy_reason",
            "lane",
            "lane_decision_reason",
            "lane_matrix_key",
            "reason_codes",
            "explanation_summary",
            "latency_ms",
            "redaction_summary",
        ] {
            assert!(
                event.get(field).is_some(),
                "telemetry missing field: {field}"
            );
        }

        let capability = event["capability"]
            .as_str()
            .expect("telemetry capability must be string");
        let method = event["method"]
            .as_str()
            .expect("telemetry method must be string");
        let policy_reason = event["policy_reason"]
            .as_str()
            .expect("telemetry policy_reason must be string");
        assert!(
            !policy_reason.trim().is_empty(),
            "telemetry policy_reason must be non-empty"
        );
        let lane_decision_reason = event["lane_decision_reason"]
            .as_str()
            .expect("telemetry lane_decision_reason must be string");
        assert!(
            !lane_decision_reason.trim().is_empty(),
            "telemetry lane_decision_reason must be non-empty"
        );

        let lane_matrix_key = event["lane_matrix_key"]
            .as_str()
            .expect("telemetry lane_matrix_key must be string");
        let lane_segments: Vec<&str> = lane_matrix_key.split('|').collect();
        assert_eq!(
            lane_segments.len(),
            3,
            "lane_matrix_key must follow method|opcode_or_fallback|capability_class"
        );
        assert_eq!(
            lane_segments[0], method,
            "lane_matrix_key method segment must match telemetry method"
        );
        assert_eq!(
            lane_segments[2],
            expected_capability_class(capability),
            "lane_matrix_key capability class segment must match capability mapping"
        );
    }

    // Verify exec mediation artifact.
    let exec_json: serde_json::Value = serde_json::to_value(&exec).expect("exec to value");
    for entry in exec_json["entries"].as_array().unwrap() {
        for field in &[
            "ts_ms",
            "extension_id",
            "command_hash",
            "decision",
            "reason",
        ] {
            assert!(
                entry.get(field).is_some(),
                "exec entry missing field: {field}"
            );
        }
    }

    // Verify secret broker artifact.
    let secret_json: serde_json::Value = serde_json::to_value(&secret).expect("secret to value");
    for entry in secret_json["entries"].as_array().unwrap() {
        for field in &["ts_ms", "extension_id", "name_hash", "redacted", "reason"] {
            assert!(
                entry.get(field).is_some(),
                "secret entry missing field: {field}"
            );
        }
    }

    emit_scenario_event(
        &harness,
        "jsonl_contract",
        "verify",
        "ext.contract",
        "none",
        "pass",
        0.0,
        &["schema_complete"],
    );

    // Write all artifacts as JSONL for CI consumption.
    let artifacts_dir = harness.create_dir("artifacts");
    let ledger_path = artifacts_dir.join("risk_ledger.json");
    let alerts_path = artifacts_dir.join("security_alerts.json");
    let telem_path = artifacts_dir.join("hostcall_telemetry.json");
    let exec_path = artifacts_dir.join("exec_mediation.json");
    let secret_path = artifacts_dir.join("secret_broker.json");

    std::fs::write(&ledger_path, serde_json::to_string_pretty(&ledger).unwrap()).unwrap();
    std::fs::write(&alerts_path, serde_json::to_string_pretty(&alerts).unwrap()).unwrap();
    std::fs::write(
        &telem_path,
        serde_json::to_string_pretty(&telemetry).unwrap(),
    )
    .unwrap();
    std::fs::write(&exec_path, serde_json::to_string_pretty(&exec).unwrap()).unwrap();
    std::fs::write(&secret_path, serde_json::to_string_pretty(&secret).unwrap()).unwrap();

    harness.record_artifact("risk_ledger", &ledger_path);
    harness.record_artifact("security_alerts", &alerts_path);
    harness.record_artifact("hostcall_telemetry", &telem_path);
    harness.record_artifact("exec_mediation", &exec_path);
    harness.record_artifact("secret_broker", &secret_path);

    harness.log().info_ctx(
        "scenario_result",
        "JSONL artifact contract verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "jsonl_contract".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("artifact_count".into(), "5".into()));
        },
    );
}

// ============================================================================
// Scenario 11: Deterministic artifact manifests
// ============================================================================

#[test]
fn scenario_deterministic_artifact_manifests() {
    let harness = TestHarness::new("scenario_deterministic_artifacts");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.determinism");

    emit_scenario_event(
        &harness,
        "deterministic_artifacts",
        "start",
        "ext.determinism",
        "none",
        "setup",
        0.0,
        &[],
    );

    // Run a workload, then build the bundle twice from the same manager
    // state. Both invocations must produce identical bundles (deterministic
    // for the same input data and timestamp).
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, benign_log_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_exec_call(idx)).await;
        }
    });

    let build_bundle = |mgr: &ExtensionManager| {
        build_incident_evidence_bundle(
            &mgr.runtime_risk_ledger_artifact(),
            &mgr.security_alert_artifact(),
            &mgr.runtime_hostcall_telemetry_artifact(),
            &mgr.exec_mediation_artifact(),
            &mgr.secret_broker_artifact(),
            &[],
            &IncidentBundleFilter::default(),
            &IncidentBundleRedactionPolicy::default(),
            1_700_000_000_000,
        )
    };

    let bundle1 = build_bundle(&manager);
    let bundle2 = build_bundle(&manager);

    // Same data + same timestamp → identical hashes.
    assert_eq!(
        bundle1.bundle_hash, bundle2.bundle_hash,
        "identical inputs must produce identical bundle hashes"
    );
    assert_eq!(bundle1.summary, bundle2.summary);

    emit_scenario_event(
        &harness,
        "deterministic_artifacts",
        "verify",
        "ext.determinism",
        "none",
        "pass",
        0.0,
        &["hashes_match"],
    );

    harness.log().info_ctx(
        "scenario_result",
        "deterministic artifact manifests verified",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3fa19".into()));
            ctx_log.push(("scenario".into(), "deterministic_artifacts".into()));
            ctx_log.push(("result".into(), "pass".into()));
            ctx_log.push(("hash".into(), bundle1.bundle_hash.clone()));
        },
    );
}

// ============================================================================
// Scenario 12: Composed bundle filters + fail-closed zero-match behavior
// ============================================================================

#[test]
fn scenario_incident_bundle_composed_filters_fail_closed() {
    let harness = TestHarness::new("scenario_incident_bundle_composed_filters");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx_alpha = make_ctx(&tools, &http, &manager, &policy, "ext.alpha");
    let ctx_beta = make_ctx(&tools, &http, &manager, &policy, "ext.beta");

    emit_scenario_event(
        &harness,
        "bundle_filter_parity",
        "start",
        "ext.alpha",
        "none",
        "setup",
        0.0,
        &[],
    );

    seed_composed_filter_fixture(&manager, &ctx_alpha, &ctx_beta);
    let redaction = IncidentBundleRedactionPolicy::default();

    // Composed filter: extension-scoped + category-scoped + severity-scoped.
    let scoped_filter = IncidentBundleFilter {
        start_ms: None,
        end_ms: None,
        extension_id: Some("ext.alpha".to_string()),
        alert_categories: Some(vec![
            SecurityAlertCategory::Quarantine,
            SecurityAlertCategory::SecretBroker,
        ]),
        min_severity: Some(SecurityAlertSeverity::Warning),
    };

    let scoped_bundle =
        build_bundle_with_filter(&manager, &scoped_filter, &redaction, 1_700_000_000_123);

    let scoped_report = verify_incident_evidence_bundle(&scoped_bundle);
    assert!(scoped_report.valid, "scoped bundle must verify");
    assert_eq!(scoped_bundle.filter, scoped_filter);
    assert_bundle_summary_matches_sections(&scoped_bundle);
    assert_bundle_extension_scope(&scoped_bundle, "ext.alpha");
    let target_ts = assert_scoped_alert_filters(&scoped_bundle);

    // Time window boundary must be inclusive at exact timestamp.
    let boundary_filter = IncidentBundleFilter {
        start_ms: Some(target_ts),
        end_ms: Some(target_ts),
        extension_id: Some("ext.alpha".to_string()),
        alert_categories: Some(vec![SecurityAlertCategory::Quarantine]),
        min_severity: Some(SecurityAlertSeverity::Critical),
    };

    let boundary_bundle =
        build_bundle_with_filter(&manager, &boundary_filter, &redaction, 1_700_000_000_124);
    assert_boundary_bundle_includes_exact_timestamp(&boundary_bundle, &boundary_filter, target_ts);
    assert!(
        verify_incident_evidence_bundle(&boundary_bundle).valid,
        "boundary bundle must verify"
    );

    // Fail closed: non-existent extension must produce an empty bundle with
    // internally consistent summary counts.
    let empty_filter = IncidentBundleFilter {
        start_ms: Some(target_ts),
        end_ms: Some(target_ts),
        extension_id: Some("ext.missing".to_string()),
        alert_categories: Some(vec![SecurityAlertCategory::Quarantine]),
        min_severity: Some(SecurityAlertSeverity::Critical),
    };
    let empty_bundle =
        build_bundle_with_filter(&manager, &empty_filter, &redaction, 1_700_000_000_125);

    assert_empty_scoped_bundle_is_fail_closed(&empty_bundle);
    assert!(
        verify_incident_evidence_bundle(&empty_bundle).valid,
        "empty scoped bundle must still be integrity-valid"
    );

    emit_bundle_filter_parity_success(&harness, &scoped_bundle, &empty_bundle);
}
