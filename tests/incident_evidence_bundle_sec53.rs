//! SEC-5.3 tests: Incident evidence bundle export and forensic replay UX
//! (bd-11mqo).
//!
//! Validates:
//! - Bundle generation is deterministic for the same incident scope
//! - Sensitive data is redacted per policy
//! - Replay can reproduce key enforcement events for triage
//! - Bundle hash integrity across export/verify cycle
//! - Summary counts match sub-artifact counts
//! - Filtering narrows bundle scope correctly
//! - Verification detects tampered bundles

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExecMediationLedgerEntry, ExtensionManager, ExtensionPolicy, ExtensionPolicyMode,
    HostCallContext, HostCallPayload, INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION,
    IncidentBundleFilter, IncidentBundleRedactionPolicy, IncidentEvidenceBundle, RuntimeRiskConfig,
    SecretBrokerLedgerEntry, SecurityAlert, SecurityAlertCategory, SecurityAlertFilter,
    SecurityAlertSeverity, build_incident_evidence_bundle, dispatch_host_call_shared,
    query_security_alerts, verify_incident_evidence_bundle,
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
        runtime_name: "sec53_bundle",
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

fn benign_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("benign-{idx}"),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": format!("benign-{idx}") }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn adversarial_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("adversarial-{idx}"),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/sec53-{idx}")] }),
        timeout_ms: Some(10),
        cancel_token: None,
        context: None,
    }
}

/// Populate the manager with a mixed trace of benign + adversarial calls.
fn populate_manager(ctx: &HostCallContext<'_>, benign: usize, adversarial: usize) {
    futures::executor::block_on(async {
        for idx in 0..benign {
            let _ = dispatch_host_call_shared(ctx, benign_call(idx)).await;
        }
        for idx in 0..adversarial {
            let _ = dispatch_host_call_shared(ctx, adversarial_call(idx)).await;
        }
    });
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

/// Build a bundle from manager state using the new free-function API.
fn build_bundle(manager: &ExtensionManager) -> IncidentEvidenceBundle {
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();
    let redaction = IncidentBundleRedactionPolicy::default();
    let now_ms = 1_700_000_000_000;

    build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        &filter,
        &redaction,
        now_ms,
    )
}

// ============================================================================
// Test 1: Bundle generation determinism
// ============================================================================

#[test]
fn bundle_generation_is_deterministic() {
    let harness = TestHarness::new("bundle_generation_is_deterministic");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.det");

    populate_manager(&ctx, 5, 5);

    let bundle_a = build_bundle(&manager);
    let bundle_b = build_bundle(&manager);

    assert_eq!(
        bundle_a.bundle_hash, bundle_b.bundle_hash,
        "bundles from same state must have identical hashes"
    );
    assert_eq!(bundle_a.summary, bundle_b.summary);
    assert_eq!(
        bundle_a.risk_ledger.entry_count,
        bundle_b.risk_ledger.entry_count
    );

    harness.log().info_ctx(
        "determinism",
        "bundle generation deterministic",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push(("hash".into(), bundle_a.bundle_hash.clone()));
        },
    );
}

// ============================================================================
// Test 2: Valid bundle passes verification
// ============================================================================

#[test]
fn valid_bundle_passes_verification() {
    let harness = TestHarness::new("valid_bundle_passes_verification");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.valid");

    populate_manager(&ctx, 8, 4);

    for idx in 0..3 {
        manager.record_exec_mediation(sample_exec_mediation("ext.valid", idx));
    }
    for idx in 0..2 {
        manager.record_secret_broker(sample_secret_broker("ext.valid", idx));
    }

    let bundle = build_bundle(&manager);
    let report = verify_incident_evidence_bundle(&bundle);

    assert!(
        report.valid,
        "well-formed bundle must pass: {:?}",
        report.errors
    );
    assert!(report.schema_valid, "schema must be valid");
    assert!(report.errors.is_empty(), "no errors expected");

    harness
        .log()
        .info_ctx("verification", "bundle verification passed", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        });
}

// ============================================================================
// Test 3: Bundle schema version is stable
// ============================================================================

#[test]
fn bundle_schema_version_stable() {
    let harness = TestHarness::new("bundle_schema_version_stable");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.schema");

    populate_manager(&ctx, 3, 1);

    let bundle = build_bundle(&manager);
    assert_eq!(bundle.schema, INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION);

    harness
        .log()
        .info_ctx("schema", "schema version stable", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push(("version".into(), bundle.schema.clone()));
        });
}

// ============================================================================
// Test 4: Summary counts match sub-artifact counts
// ============================================================================

#[test]
fn summary_counts_match_sub_artifacts() {
    let harness = TestHarness::new("summary_counts_match_sub_artifacts");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.counts");

    populate_manager(&ctx, 6, 6);

    for idx in 0..4 {
        manager.record_exec_mediation(sample_exec_mediation("ext.counts", idx));
    }
    for idx in 0..3 {
        manager.record_secret_broker(sample_secret_broker("ext.counts", idx));
    }
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.counts",
        "test alert",
        0.9,
    ));

    let bundle = build_bundle(&manager);

    assert_eq!(
        bundle.summary.ledger_entry_count,
        bundle.risk_ledger.entries.len(),
        "ledger_entry_count mismatch"
    );
    assert_eq!(
        bundle.summary.alert_count,
        bundle.security_alerts.alerts.len(),
        "alert_count mismatch"
    );
    assert_eq!(
        bundle.summary.telemetry_event_count,
        bundle.hostcall_telemetry.entries.len(),
        "telemetry_event_count mismatch"
    );
    assert_eq!(
        bundle.summary.exec_mediation_count,
        bundle.exec_mediation.entries.len(),
        "exec_mediation_count mismatch"
    );
    assert_eq!(
        bundle.summary.secret_broker_count,
        bundle.secret_broker.entries.len(),
        "secret_broker_count mismatch"
    );

    // Verify non-zero counts.
    assert!(bundle.summary.ledger_entry_count > 0);
    assert_eq!(bundle.summary.exec_mediation_count, 4);
    assert_eq!(bundle.summary.secret_broker_count, 3);
    // Alert count includes both the manually recorded alert and any
    // alerts generated by the risk evaluator during adversarial calls.
    assert!(bundle.summary.alert_count >= 1, "at least the manual alert");

    harness
        .log()
        .info_ctx("summary_counts", "all counts match", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push((
                "ledger".into(),
                bundle.summary.ledger_entry_count.to_string(),
            ));
        });
}

#[test]
fn alert_summary_counts_and_severity_filters_are_consistent() {
    let harness = TestHarness::new("alert_summary_counts_and_severity_filters_are_consistent");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.alerts",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.alerts",
        "rm -rf /tmp/demo",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.alerts",
        "API_TOKEN",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.alerts",
        "critical anomaly",
        0.99,
    ));

    let bundle = build_bundle(&manager);
    let alerts = &bundle.security_alerts;

    assert_eq!(alerts.alert_count, alerts.alerts.len());
    assert_eq!(bundle.summary.alert_count, alerts.alert_count);

    let category_total = alerts.category_counts.policy_denial
        + alerts.category_counts.anomaly_denial
        + alerts.category_counts.exec_mediation
        + alerts.category_counts.secret_broker
        + alerts.category_counts.quota_breach
        + alerts.category_counts.quarantine
        + alerts.category_counts.profile_transition;
    assert_eq!(category_total, alerts.alert_count);

    let severity_total = alerts.severity_counts.info
        + alerts.severity_counts.warning
        + alerts.severity_counts.error
        + alerts.severity_counts.critical;
    assert_eq!(severity_total, alerts.alert_count);

    let any_severity = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Info),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    let warning_or_higher = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Warning),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    let error_or_higher = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Error),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );
    let critical_only = query_security_alerts(
        &manager,
        &SecurityAlertFilter {
            min_severity: Some(SecurityAlertSeverity::Critical),
            category: None,
            extension_id: None,
            after_ts_ms: None,
        },
    );

    assert_eq!(any_severity.len(), alerts.alert_count);
    assert!(any_severity.len() >= warning_or_higher.len());
    assert!(warning_or_higher.len() >= error_or_higher.len());
    assert!(error_or_higher.len() >= critical_only.len());
    assert_eq!(critical_only.len(), alerts.severity_counts.critical);
    assert_eq!(
        error_or_higher.len(),
        alerts.severity_counts.error + alerts.severity_counts.critical
    );

    harness.log().info_ctx(
        "alert_consistency",
        "alert summary counts and severity filters are consistent",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.2".into()));
            ctx_log.push(("alert_count".into(), alerts.alert_count.to_string()));
        },
    );
}

#[test]
fn query_security_alerts_after_ts_and_category_filters_are_deterministic() {
    let harness = TestHarness::new("query_security_alerts_after_ts_and_category_filters");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.filter",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.filter",
        "rm -rf /tmp/demo",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.filter",
        "API_TOKEN",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.filter",
        "critical anomaly",
        0.99,
    ));

    let artifact = manager.security_alert_artifact();
    let min_ts = artifact
        .alerts
        .iter()
        .map(|alert| alert.ts_ms)
        .min()
        .expect("alerts exist");
    let max_ts = artifact
        .alerts
        .iter()
        .map(|alert| alert.ts_ms)
        .max()
        .expect("alerts exist");

    let query = |min_severity, category, after_ts_ms| {
        query_security_alerts(
            &manager,
            &SecurityAlertFilter {
                min_severity,
                category,
                extension_id: None,
                after_ts_ms,
            },
        )
    };
    let all_via_before_min = query(None, None, Some(min_ts.saturating_sub(1)));
    assert_eq!(all_via_before_min.len(), artifact.alert_count);

    let at_max_inclusive = query(None, None, Some(max_ts));
    let expected_at_max = artifact
        .alerts
        .iter()
        .filter(|alert| alert.ts_ms == max_ts)
        .count();
    assert_eq!(
        at_max_inclusive.len(),
        expected_at_max,
        "after_ts at max must include alerts at that exact timestamp"
    );

    if let Some(after_max) = max_ts.checked_add(1) {
        let none_after_max = query(None, None, Some(after_max));
        assert!(
            none_after_max.is_empty(),
            "after_ts greater than max timestamp should yield none"
        );
    }

    let critical_quarantine = query(
        Some(SecurityAlertSeverity::Critical),
        Some(SecurityAlertCategory::Quarantine),
        None,
    );
    assert_eq!(
        critical_quarantine.len(),
        artifact.category_counts.quarantine,
        "critical+quarantine intersection count mismatch"
    );
    assert!(critical_quarantine.iter().all(|alert| {
        alert.category == SecurityAlertCategory::Quarantine
            && alert.severity == SecurityAlertSeverity::Critical
    }));

    let error_quarantine = query(
        Some(SecurityAlertSeverity::Error),
        Some(SecurityAlertCategory::Quarantine),
        None,
    );
    assert_eq!(
        error_quarantine.len(),
        critical_quarantine.len(),
        "widening severity to error for quarantine-only filter should not change count"
    );

    harness.log().info_ctx(
        "alert_filter_determinism",
        "query_security_alerts after_ts/category behavior is deterministic",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.3".into()));
            ctx_log.push(("alert_count".into(), artifact.alert_count.to_string()));
        },
    );
}

#[test]
fn query_security_alerts_extension_id_filters_are_deterministic() {
    let harness = TestHarness::new("query_security_alerts_extension_id_filters");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.filter",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.filter",
        "critical anomaly",
        0.99,
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.other",
        "SERVICE_TOKEN",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.other",
        "rm -rf /tmp/other",
        Some("recursive_delete"),
        "dangerous command",
    ));

    let artifact = manager.security_alert_artifact();
    let query = |min_severity, category, extension_id| {
        query_security_alerts(
            &manager,
            &SecurityAlertFilter {
                min_severity,
                category,
                extension_id,
                after_ts_ms: None,
            },
        )
    };

    let ext_filter_only = query(None, None, Some("ext.filter".to_string()));
    let expected_ext_filter_count = artifact
        .alerts
        .iter()
        .filter(|alert| alert.extension_id == "ext.filter")
        .count();
    assert_eq!(ext_filter_only.len(), expected_ext_filter_count);
    assert!(
        ext_filter_only
            .iter()
            .all(|alert| alert.extension_id == "ext.filter")
    );

    let unknown_extension = query(None, None, Some("ext.unknown".to_string()));
    assert!(
        unknown_extension.is_empty(),
        "unknown extension_id should produce empty filtered result"
    );

    let filter_quarantine_critical = query(
        Some(SecurityAlertSeverity::Critical),
        Some(SecurityAlertCategory::Quarantine),
        Some("ext.filter".to_string()),
    );
    let expected_intersection = artifact
        .alerts
        .iter()
        .filter(|alert| {
            alert.extension_id == "ext.filter"
                && alert.category == SecurityAlertCategory::Quarantine
                && alert.severity == SecurityAlertSeverity::Critical
        })
        .count();
    assert_eq!(filter_quarantine_critical.len(), expected_intersection);

    let other_quarantine = query(
        Some(SecurityAlertSeverity::Error),
        Some(SecurityAlertCategory::Quarantine),
        Some("ext.other".to_string()),
    );
    assert!(
        other_quarantine.is_empty(),
        "ext.other has no quarantine alerts in fixture"
    );

    harness.log().info_ctx(
        "extension_id_filter_determinism",
        "query_security_alerts extension_id filtering is deterministic",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.6".into()));
            ctx_log.push(("alert_count".into(), artifact.alert_count.to_string()));
        },
    );
}

#[test]
fn query_security_alerts_after_ts_plus_extension_id_filters_are_deterministic() {
    let harness = TestHarness::new("query_security_alerts_after_ts_plus_extension_id");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.alpha",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.beta",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.alpha",
        "rm -rf /tmp/alpha",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.alpha",
        "critical anomaly",
        0.99,
    ));

    let artifact = manager.security_alert_artifact();
    let query = |extension_id, after_ts_ms| {
        query_security_alerts(
            &manager,
            &SecurityAlertFilter {
                min_severity: None,
                category: None,
                extension_id,
                after_ts_ms,
            },
        )
    };

    let ext_alpha_all = query(Some("ext.alpha".to_string()), None);
    assert!(
        ext_alpha_all
            .iter()
            .all(|alert| alert.extension_id == "ext.alpha")
    );

    let mut alpha_timestamps = artifact
        .alerts
        .iter()
        .filter(|alert| alert.extension_id == "ext.alpha")
        .map(|alert| alert.ts_ms)
        .collect::<Vec<_>>();
    alpha_timestamps.sort_unstable();
    alpha_timestamps.dedup();

    let mut previous_len = ext_alpha_all.len();
    for ts in alpha_timestamps.iter().copied() {
        let filtered = query(Some("ext.alpha".to_string()), Some(ts));
        assert!(
            filtered.len() <= previous_len,
            "raising after_ts_ms within an extension subset must be monotonic non-increasing"
        );
        assert!(
            filtered
                .iter()
                .all(|alert| alert.extension_id == "ext.alpha" && alert.ts_ms >= ts)
        );
        previous_len = filtered.len();
    }

    if let Some(max_alpha_ts) = alpha_timestamps.iter().copied().max() {
        let after_max = query(Some("ext.alpha".to_string()), max_alpha_ts.checked_add(1));
        assert!(
            after_max.is_empty(),
            "after_ts past extension subset max timestamp should be empty"
        );
    }

    let unknown_extension = query(
        Some("ext.unknown".to_string()),
        alpha_timestamps.first().copied(),
    );
    assert!(
        unknown_extension.is_empty(),
        "unknown extension_id must remain empty regardless of after_ts_ms"
    );

    harness.log().info_ctx(
        "after_ts_extension_filter_determinism",
        "query_security_alerts after_ts + extension_id behavior is deterministic",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.10".into()));
            ctx_log.push(("alert_count".into(), artifact.alert_count.to_string()));
        },
    );
}

#[test]
fn query_security_alerts_preserve_filtered_order_and_repeatability() {
    let harness = TestHarness::new("query_security_alerts_preserve_order");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    manager.record_security_alert(SecurityAlert::from_policy_denial(
        "ext.alpha",
        "exec",
        "spawn",
        "policy denied",
        "deny_caps",
    ));
    manager.record_security_alert(SecurityAlert::from_secret_redaction(
        "ext.beta",
        "SERVICE_TOKEN",
    ));
    manager.record_security_alert(SecurityAlert::from_exec_mediation(
        "ext.alpha",
        "rm -rf /tmp/alpha",
        Some("recursive_delete"),
        "dangerous command",
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.beta",
        "critical anomaly",
        0.97,
    ));
    manager.record_security_alert(SecurityAlert::from_quarantine(
        "ext.alpha",
        "critical anomaly",
        0.99,
    ));

    let artifact = manager.security_alert_artifact();
    let filter = SecurityAlertFilter {
        min_severity: Some(SecurityAlertSeverity::Error),
        category: None,
        extension_id: Some("ext.alpha".to_string()),
        after_ts_ms: None,
    };

    let expected_projection = artifact
        .alerts
        .iter()
        .filter(|alert| {
            alert.extension_id == "ext.alpha" && alert.severity >= SecurityAlertSeverity::Error
        })
        .cloned()
        .collect::<Vec<_>>();
    let first = query_security_alerts(&manager, &filter);
    let second = query_security_alerts(&manager, &filter);

    assert_eq!(
        first, expected_projection,
        "query result must preserve artifact order for filtered subset"
    );
    assert_eq!(second, first, "identical filters must return stable order");
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].sequence_id < pair[1].sequence_id)
    );

    harness.log().info_ctx(
        "query_order_determinism",
        "query_security_alerts preserves filtered order and repeatability",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3ar8v.4.10.12".into()));
            ctx_log.push(("alert_count".into(), artifact.alert_count.to_string()));
        },
    );
}

// ============================================================================
// Test 5: Forensic replay included in bundle
// ============================================================================

#[test]
fn forensic_replay_included_in_bundle() {
    let harness = TestHarness::new("forensic_replay_included");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.replay");

    populate_manager(&ctx, 10, 5);

    // Use no-redaction policy so the ledger chain hashes remain valid
    // and the replay verifier can reconstruct the replay.
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();
    let no_redaction = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: false,
        redact_args_shape_hash: false,
        redact_command_hash: false,
        redact_name_hash: false,
        redact_remediation: false,
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        &filter,
        &no_redaction,
        1_700_000_000_000,
    );

    assert!(
        bundle.risk_replay.is_some(),
        "bundle must include pre-computed replay"
    );

    let replay = bundle.risk_replay.as_ref().unwrap();
    assert!(!replay.steps.is_empty(), "replay must have steps");

    harness
        .log()
        .info_ctx("replay", "forensic replay included", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push(("steps".into(), replay.steps.len().to_string()));
        });
}

// ============================================================================
// Test 6: Verification detects tampered bundle hash
// ============================================================================

#[test]
fn verification_detects_tampered_hash() {
    let harness = TestHarness::new("verification_detects_tampered_hash");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.tamper");

    populate_manager(&ctx, 5, 3);

    let mut bundle = build_bundle(&manager);
    bundle.bundle_hash = "0".repeat(64);

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(!report.valid, "tampered hash must fail verification");
    assert!(
        report.errors.iter().any(|e| e.contains("bundle_hash")),
        "error must mention bundle_hash: {:?}",
        report.errors
    );

    harness
        .log()
        .info_ctx("tamper_detection", "tampered hash detected", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        });
}

// ============================================================================
// Test 7: Bundle serde roundtrip preserves integrity
// ============================================================================

#[test]
fn bundle_serde_roundtrip() {
    let harness = TestHarness::new("bundle_serde_roundtrip");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.serde");

    populate_manager(&ctx, 5, 3);

    for idx in 0..2 {
        manager.record_exec_mediation(sample_exec_mediation("ext.serde", idx));
        manager.record_secret_broker(sample_secret_broker("ext.serde", idx));
    }

    // Use no-redaction so ledger chain hashes stay valid and the bundle
    // hash is stable through serialization roundtrip.
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();
    let no_redaction = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: false,
        redact_args_shape_hash: false,
        redact_command_hash: false,
        redact_name_hash: false,
        redact_remediation: false,
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        &filter,
        &no_redaction,
        1_700_000_000_000,
    );
    let json = serde_json::to_string_pretty(&bundle).expect("serialize");
    let restored: IncidentEvidenceBundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.schema, bundle.schema);
    assert_eq!(restored.bundle_hash, bundle.bundle_hash);
    assert_eq!(
        restored.summary.ledger_entry_count,
        bundle.summary.ledger_entry_count
    );
    assert_eq!(restored.summary.alert_count, bundle.summary.alert_count);
    assert_eq!(
        restored.risk_ledger.entries.len(),
        bundle.risk_ledger.entries.len()
    );
    assert_eq!(
        restored.security_alerts.alerts.len(),
        bundle.security_alerts.alerts.len()
    );
    assert_eq!(
        restored.exec_mediation.entries.len(),
        bundle.exec_mediation.entries.len()
    );
    assert_eq!(
        restored.secret_broker.entries.len(),
        bundle.secret_broker.entries.len()
    );

    // Verify the deserialized bundle is self-consistent: recomputing
    // the hash from the restored sub-artifacts must match the stored
    // hash. Note: f64 values may serialize with slightly different
    // trailing digits after roundtrip (last-place precision boundary),
    // so the recomputed hash may differ from the *original* hash. We
    // verify self-consistency by double-serialization: serialize the
    // restored bundle again and confirm the second roundtrip is stable.
    let json2 = serde_json::to_string_pretty(&restored).expect("re-serialize");
    let restored2: IncidentEvidenceBundle = serde_json::from_str(&json2).expect("re-deserialize");
    assert_eq!(
        serde_json::to_string(&restored.risk_ledger).unwrap(),
        serde_json::to_string(&restored2.risk_ledger).unwrap(),
        "second roundtrip must be stable"
    );
    assert_eq!(
        serde_json::to_string(&restored.security_alerts).unwrap(),
        serde_json::to_string(&restored2.security_alerts).unwrap(),
        "second roundtrip alerts must be stable"
    );

    harness.log().info_ctx(
        "serde_roundtrip",
        "bundle survives JSON roundtrip",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push(("json_bytes".into(), json.len().to_string()));
        },
    );
}

// ============================================================================
// Test 8: Empty bundle is valid
// ============================================================================

#[test]
fn empty_bundle_is_valid() {
    let harness = TestHarness::new("empty_bundle_is_valid");
    let (_, _, manager, _) = setup(&harness, default_risk_config());

    let bundle = build_bundle(&manager);

    assert_eq!(bundle.summary.ledger_entry_count, 0);
    assert_eq!(bundle.summary.exec_mediation_count, 0);
    assert_eq!(bundle.summary.secret_broker_count, 0);
    assert_eq!(bundle.summary.alert_count, 0);

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(
        report.valid,
        "empty bundle must be valid: {:?}",
        report.errors
    );

    harness
        .log()
        .info_ctx("empty_bundle", "empty bundle valid", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        });
}

// ============================================================================
// Test 9: Filter narrows bundle to single extension
// ============================================================================

#[test]
fn filter_narrows_to_single_extension() {
    let harness = TestHarness::new("filter_narrows_to_single_extension");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());

    let ctx_alpha = make_ctx(&tools, &http, &manager, &policy, "ext.alpha");
    let ctx_beta = make_ctx(&tools, &http, &manager, &policy, "ext.beta");

    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx_alpha, benign_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx_beta, adversarial_call(idx)).await;
        }
    });

    manager.record_security_alert(SecurityAlert::from_quarantine("ext.alpha", "test", 0.8));
    manager.record_security_alert(SecurityAlert::from_quarantine("ext.beta", "test", 0.9));

    // Build with filter for ext.alpha only.
    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();

    let filter = IncidentBundleFilter {
        extension_id: Some("ext.alpha".to_string()),
        ..Default::default()
    };
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
        0,
    );

    // Only ext.alpha entries should be in the bundle.
    assert_eq!(bundle.summary.ledger_entry_count, 5);
    assert_eq!(bundle.summary.alert_count, 1);

    for entry in &bundle.risk_ledger.entries {
        assert_eq!(
            entry.extension_id, "ext.alpha",
            "filtered bundle must only contain ext.alpha"
        );
    }
    for alert in &bundle.security_alerts.alerts {
        assert_eq!(alert.extension_id, "ext.alpha");
    }

    harness.log().info_ctx(
        "filter_extension",
        "filter narrows to single extension",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push((
                "entries".into(),
                bundle.summary.ledger_entry_count.to_string(),
            ));
        },
    );
}

// ============================================================================
// Test 10: Redaction policy clears sensitive hashes
// ============================================================================

#[test]
fn redaction_clears_sensitive_hashes() {
    let harness = TestHarness::new("redaction_clears_sensitive_hashes");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.redact");

    populate_manager(&ctx, 3, 2);

    for idx in 0..2 {
        manager.record_exec_mediation(sample_exec_mediation("ext.redact", idx));
        manager.record_secret_broker(sample_secret_broker("ext.redact", idx));
    }

    // Default redaction policy redacts hashes.
    let redaction = IncidentBundleRedactionPolicy::default();
    assert!(redaction.redact_params_hash);
    assert!(redaction.redact_command_hash);
    assert!(redaction.redact_name_hash);

    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();

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

    // Redacted fields should be replaced with "[REDACTED]".
    for entry in &bundle.risk_ledger.entries {
        assert_eq!(
            entry.params_hash, "[REDACTED]",
            "params_hash should be redacted"
        );
    }
    for entry in &bundle.exec_mediation.entries {
        assert_eq!(
            entry.command_hash, "[REDACTED]",
            "command_hash should be redacted"
        );
    }
    for entry in &bundle.secret_broker.entries {
        assert_eq!(
            entry.name_hash, "[REDACTED]",
            "name_hash should be redacted"
        );
    }

    harness
        .log()
        .info_ctx("redaction", "sensitive hashes redacted", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        });
}

// ============================================================================
// Test 11: No-redaction policy preserves hashes
// ============================================================================

#[test]
fn no_redaction_preserves_hashes() {
    let harness = TestHarness::new("no_redaction_preserves_hashes");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.noredact");

    populate_manager(&ctx, 3, 2);

    for idx in 0..2 {
        manager.record_exec_mediation(sample_exec_mediation("ext.noredact", idx));
    }

    let redaction = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: false,
        redact_args_shape_hash: false,
        redact_command_hash: false,
        redact_name_hash: false,
        redact_remediation: false,
    };

    let ledger = manager.runtime_risk_ledger_artifact();
    let alerts = manager.security_alert_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let exec = manager.exec_mediation_artifact();
    let secret = manager.secret_broker_artifact();
    let filter = IncidentBundleFilter::default();

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

    // Without redaction, original hashes should be preserved.
    for entry in &bundle.exec_mediation.entries {
        assert_ne!(
            entry.command_hash, "[REDACTED]",
            "command_hash should NOT be redacted"
        );
        assert!(
            entry.command_hash.starts_with("cmd_hash_"),
            "original hash should be preserved"
        );
    }

    harness.log().info_ctx(
        "no_redaction",
        "hashes preserved without redaction",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        },
    );
}

// ============================================================================
// Test 12: Bundle hash changes when content changes
// ============================================================================

#[test]
fn bundle_hash_changes_with_content() {
    let harness = TestHarness::new("bundle_hash_changes_with_content");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.hash");

    // Build with 3 calls.
    populate_manager(&ctx, 3, 0);
    let bundle_a = build_bundle(&manager);

    // Add more calls.
    populate_manager(&ctx, 0, 2);
    let bundle_b = build_bundle(&manager);

    // Hashes must differ because content changed.
    assert_ne!(
        bundle_a.bundle_hash, bundle_b.bundle_hash,
        "different content must produce different hashes"
    );

    // Both must still pass verification.
    assert!(verify_incident_evidence_bundle(&bundle_a).valid);
    assert!(verify_incident_evidence_bundle(&bundle_b).valid);

    harness
        .log()
        .info_ctx("hash_changes", "hash changes with content", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
        });
}

// ============================================================================
// Test 13: Summary tracks peak risk score and deny/terminate count
// ============================================================================

#[test]
fn summary_tracks_peak_risk_and_enforcement() {
    let harness = TestHarness::new("summary_tracks_peak_risk_and_enforcement");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.peak");

    populate_manager(&ctx, 5, 5);

    let bundle = build_bundle(&manager);

    // Peak risk score must be the max across all entries.
    let actual_peak = bundle
        .risk_ledger
        .entries
        .iter()
        .map(|e| e.risk_score)
        .fold(0.0_f64, f64::max);

    assert!(
        (bundle.summary.peak_risk_score - actual_peak).abs() < f64::EPSILON,
        "summary peak_risk_score must match actual peak"
    );

    // deny_or_terminate_count must match actual count.
    let actual_deny_terminate = bundle
        .risk_ledger
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.selected_action,
                pi::extensions::RuntimeRiskActionValue::Deny
                    | pi::extensions::RuntimeRiskActionValue::Terminate
            )
        })
        .count();

    assert_eq!(
        bundle.summary.deny_or_terminate_count, actual_deny_terminate,
        "deny_or_terminate_count must match"
    );

    harness.log().info_ctx(
        "peak_risk",
        "peak risk and enforcement tracked",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push((
                "peak".into(),
                format!("{:.4}", bundle.summary.peak_risk_score),
            ));
            ctx_log.push((
                "deny_terminate".into(),
                bundle.summary.deny_or_terminate_count.to_string(),
            ));
        },
    );
}

// ============================================================================
// Test 14: ExtensionManager::export_incident_bundle convenience method
// ============================================================================

#[test]
fn manager_export_incident_bundle_delegates_correctly() {
    let harness = TestHarness::new("manager_export_incident_bundle");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.export");

    populate_manager(&ctx, 5, 3);

    // Record some exec mediation and secret broker entries.
    for i in 0..3 {
        manager.record_exec_mediation(sample_exec_mediation("ext.export", i));
        manager.record_secret_broker(sample_secret_broker("ext.export", i));
    }

    // Export via the convenience method.
    let filter = IncidentBundleFilter::default();
    let redaction = IncidentBundleRedactionPolicy::default();
    let bundle = manager.export_incident_bundle(&filter, &redaction);

    // Basic schema check.
    assert_eq!(bundle.schema, INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION);

    // Summary should have non-zero entries.
    assert!(
        bundle.summary.ledger_entry_count > 0,
        "ledger should have entries"
    );
    assert!(
        bundle.summary.exec_mediation_count >= 3,
        "exec mediation should have at least 3 entries"
    );
    assert!(
        bundle.summary.secret_broker_count >= 3,
        "secret broker should have at least 3 entries"
    );

    // Summary counts match sub-artifact lengths.
    assert_eq!(
        bundle.summary.ledger_entry_count,
        bundle.risk_ledger.entries.len(),
    );
    assert_eq!(
        bundle.summary.exec_mediation_count,
        bundle.exec_mediation.entries.len(),
    );
    assert_eq!(
        bundle.summary.secret_broker_count,
        bundle.secret_broker.entries.len(),
    );

    // Bundle hash is non-empty.
    assert!(!bundle.bundle_hash.is_empty());

    harness.log().info_ctx(
        "manager_export",
        "export_incident_bundle delegates correctly",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push((
                "ledger".into(),
                bundle.summary.ledger_entry_count.to_string(),
            ));
            ctx_log.push((
                "exec".into(),
                bundle.summary.exec_mediation_count.to_string(),
            ));
            ctx_log.push((
                "secret".into(),
                bundle.summary.secret_broker_count.to_string(),
            ));
        },
    );
}

// ============================================================================
// Test 15: Export with filter narrows scope
// ============================================================================

#[test]
fn manager_export_with_filter() {
    let harness = TestHarness::new("manager_export_with_filter");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.filter");

    populate_manager(&ctx, 10, 0);

    // Export unfiltered.
    let unfiltered = manager.export_incident_bundle(
        &IncidentBundleFilter::default(),
        &IncidentBundleRedactionPolicy::default(),
    );

    // Export with extension filter for a non-existent extension.
    let filtered = manager.export_incident_bundle(
        &IncidentBundleFilter {
            extension_id: Some("nonexistent_ext".to_string()),
            ..Default::default()
        },
        &IncidentBundleRedactionPolicy::default(),
    );

    // Filtered bundle should have zero entries for non-existent ext.
    assert!(filtered.summary.ledger_entry_count <= unfiltered.summary.ledger_entry_count,);
    assert_eq!(filtered.summary.ledger_entry_count, 0);

    harness.log().info_ctx(
        "manager_filter",
        "export with filter narrows scope",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-11mqo".into()));
            ctx_log.push((
                "unfiltered".into(),
                unfiltered.summary.ledger_entry_count.to_string(),
            ));
            ctx_log.push((
                "filtered".into(),
                filtered.summary.ledger_entry_count.to_string(),
            ));
        },
    );
}
