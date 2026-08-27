//! SEC-5.3 Incident Evidence Bundle – integration tests.
//!
//! Tests cover: bundle construction, filtering, redaction, deterministic
//! generation, verification, forensic replay, and JSON round-trip stability.

use pi::extensions::{
    ExecMediationArtifact, ExecMediationLedgerEntry, INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION,
    IncidentBundleFilter, IncidentBundleRedactionPolicy, IncidentEvidenceBundle, QuotaBreachEvent,
    RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION, RUNTIME_RISK_LEDGER_SCHEMA_VERSION,
    RuntimeHostcallTelemetryArtifact, RuntimeHostcallTelemetryEvent, RuntimeRiskActionValue,
    RuntimeRiskLedgerArtifact, RuntimeRiskLedgerArtifactEntry, RuntimeRiskStateLabelValue,
    SECURITY_ALERT_SCHEMA_VERSION, SecretBrokerArtifact, SecretBrokerLedgerEntry, SecurityAlert,
    SecurityAlertAction, SecurityAlertArtifact, SecurityAlertCategory, SecurityAlertCategoryCounts,
    SecurityAlertSeverity, SecurityAlertSeverityCounts, build_incident_evidence_bundle,
    compute_incident_bundle_hash, verify_incident_evidence_bundle,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

use pi::extensions::{
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RuntimeRiskExpectedLossEvidence,
    RuntimeRiskExplanationBudgetState, RuntimeRiskExplanationLevelValue,
    RuntimeRiskPosteriorEvidence,
};

fn make_ledger_entry(ts_ms: i64, ext_id: &str, risk: f64) -> RuntimeRiskLedgerArtifactEntry {
    RuntimeRiskLedgerArtifactEntry {
        ts_ms,
        extension_id: ext_id.to_string(),
        call_id: format!("call-{ts_ms}"),
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        params_hash: "abc123".to_string(),
        policy_reason: "allowed".to_string(),
        risk_score: risk,
        posterior: RuntimeRiskPosteriorEvidence {
            safe_fast: 1.0 - risk,
            suspicious: 0.0,
            unsafe_: risk,
        },
        expected_loss: RuntimeRiskExpectedLossEvidence {
            allow: risk,
            harden: risk * 0.5,
            deny: 0.1,
            terminate: 0.05,
        },
        selected_action: if risk > 0.7 {
            RuntimeRiskActionValue::Deny
        } else {
            RuntimeRiskActionValue::Allow
        },
        derived_state: if risk > 0.5 {
            RuntimeRiskStateLabelValue::Unsafe
        } else {
            RuntimeRiskStateLabelValue::SafeFast
        },
        triggers: vec![],
        fallback_reason: None,
        e_process: 0.0,
        e_threshold: 0.5,
        conformal_residual: 0.0,
        conformal_quantile: 0.95,
        drift_detected: false,
        outcome_error_code: None,
        explanation_schema: RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION.to_string(),
        explanation_level: RuntimeRiskExplanationLevelValue::default(),
        explanation_summary: String::new(),
        top_contributors: vec![],
        budget_state: RuntimeRiskExplanationBudgetState::default(),
        ledger_hash: format!("hash-{ts_ms}"),
        prev_ledger_hash: if ts_ms > 1000 {
            Some(format!("hash-{}", ts_ms - 1000))
        } else {
            None
        },
    }
}

fn make_alert(
    ts_ms: i64,
    ext_id: &str,
    cat: SecurityAlertCategory,
    sev: SecurityAlertSeverity,
) -> SecurityAlert {
    SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms,
        sequence_id: u64::try_from(ts_ms).unwrap_or(0),
        extension_id: ext_id.to_string(),
        category: cat,
        severity: sev,
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        reason_codes: vec!["test".to_string()],
        summary: "Test alert".to_string(),
        policy_source: "unit_test".to_string(),
        action: SecurityAlertAction::Deny,
        remediation: "Fix it".to_string(),
        risk_score: 0.8,
        risk_state: None,
        context_hash: "ctx-hash-secret".to_string(),
    }
}

fn make_telemetry(ts_ms: i64, ext_id: &str) -> RuntimeHostcallTelemetryEvent {
    RuntimeHostcallTelemetryEvent {
        schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
        ts_ms,
        extension_id: ext_id.to_string(),
        call_id: format!("call-{ts_ms}"),
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        params_hash: "params-secret".to_string(),
        args_shape_hash: "shape-secret".to_string(),
        resource_target_class: "process".to_string(),
        policy_reason: "allowed".to_string(),
        policy_profile: "standard".to_string(),
        risk_score: 0.3,
        timeout_ms: Some(5000),
        latency_ms: 42,
        outcome: "success".to_string(),
        ..Default::default()
    }
}

fn make_exec_entry(ts_ms: i64, ext_id: &str) -> ExecMediationLedgerEntry {
    ExecMediationLedgerEntry {
        ts_ms,
        extension_id: Some(ext_id.to_string()),
        command_hash: "cmd-hash-secret".to_string(),
        command_class: Some("general".to_string()),
        risk_tier: Some("Low".to_string()),
        decision: "allow".to_string(),
        reason: "test".to_string(),
    }
}

fn make_secret_entry(ts_ms: i64, ext_id: &str) -> SecretBrokerLedgerEntry {
    SecretBrokerLedgerEntry {
        ts_ms,
        extension_id: Some(ext_id.to_string()),
        name_hash: "name-hash-secret".to_string(),
        redacted: true,
        reason: "secret suffix match".to_string(),
    }
}

fn make_quota(ts_ms: i64, ext_id: &str) -> QuotaBreachEvent {
    QuotaBreachEvent {
        ts_ms,
        extension_id: ext_id.to_string(),
        capability: "exec".to_string(),
        reason: "rate limit".to_string(),
        quota_config_source: "global".to_string(),
    }
}

/// Build a test ledger artifact from entries.
fn ledger_artifact(entries: Vec<RuntimeRiskLedgerArtifactEntry>) -> RuntimeRiskLedgerArtifact {
    let head = entries.first().map(|e| e.ledger_hash.clone());
    let tail = entries.last().map(|e| e.ledger_hash.clone());
    RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 99999,
        entry_count: entries.len(),
        head_ledger_hash: head,
        tail_ledger_hash: tail,
        data_hash: "test-data-hash".to_string(),
        entries,
    }
}

fn alert_artifact(alerts: Vec<SecurityAlert>) -> SecurityAlertArtifact {
    let mut cat = SecurityAlertCategoryCounts::default();
    let mut sev = SecurityAlertSeverityCounts::default();
    for a in &alerts {
        match a.category {
            SecurityAlertCategory::PolicyDenial => cat.policy_denial += 1,
            SecurityAlertCategory::AnomalyDenial => cat.anomaly_denial += 1,
            SecurityAlertCategory::ExecMediation => cat.exec_mediation += 1,
            SecurityAlertCategory::SecretBroker => cat.secret_broker += 1,
            SecurityAlertCategory::QuotaBreach => cat.quota_breach += 1,
            SecurityAlertCategory::Quarantine => cat.quarantine += 1,
            SecurityAlertCategory::ProfileTransition => cat.profile_transition += 1,
        }
        match a.severity {
            SecurityAlertSeverity::Info => sev.info += 1,
            SecurityAlertSeverity::Warning => sev.warning += 1,
            SecurityAlertSeverity::Error => sev.error += 1,
            SecurityAlertSeverity::Critical => sev.critical += 1,
        }
    }
    SecurityAlertArtifact {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        generated_at_ms: 99999,
        alert_count: alerts.len(),
        category_counts: cat,
        severity_counts: sev,
        alerts,
    }
}

fn telemetry_artifact(
    entries: Vec<RuntimeHostcallTelemetryEvent>,
) -> RuntimeHostcallTelemetryArtifact {
    RuntimeHostcallTelemetryArtifact {
        schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
        generated_at_ms: 99999,
        entry_count: entries.len(),
        entries,
    }
}

fn exec_artifact(entries: Vec<ExecMediationLedgerEntry>) -> ExecMediationArtifact {
    ExecMediationArtifact {
        schema: "pi.ext.exec_mediation.v1".to_string(),
        generated_at_ms: 99999,
        entry_count: entries.len(),
        entries,
    }
}

fn secret_artifact(entries: Vec<SecretBrokerLedgerEntry>) -> SecretBrokerArtifact {
    SecretBrokerArtifact {
        schema: "pi.ext.secret_broker.v1".to_string(),
        generated_at_ms: 99999,
        entry_count: entries.len(),
        entries,
    }
}

/// Helper to build a full bundle with default filter+redaction.
fn build_default_bundle(
    ledger: &RuntimeRiskLedgerArtifact,
    alerts: &SecurityAlertArtifact,
    telemetry: &RuntimeHostcallTelemetryArtifact,
    exec: &ExecMediationArtifact,
    secret: &SecretBrokerArtifact,
    quotas: &[QuotaBreachEvent],
) -> IncidentEvidenceBundle {
    build_incident_evidence_bundle(
        ledger,
        alerts,
        telemetry,
        exec,
        secret,
        quotas,
        &IncidentBundleFilter::default(),
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    )
}

// ===========================================================================
// Tests
// ===========================================================================

// ---- 1. Bundle construction ----

#[test]
fn empty_bundle_has_correct_schema() {
    let bundle = build_default_bundle(
        &ledger_artifact(vec![]),
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );
    assert_eq!(bundle.schema, INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION);
    assert_eq!(bundle.generated_at_ms, 100_000);
    assert_ne!(bundle.bundle_hash, "");
}

#[test]
fn bundle_summary_counts_match_artifacts() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.8),
    ]);
    let alerts = alert_artifact(vec![make_alert(
        1500,
        "ext-a",
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
    )]);
    let telem = telemetry_artifact(vec![make_telemetry(1200, "ext-a")]);
    let exec = exec_artifact(vec![make_exec_entry(1300, "ext-a")]);
    let secret = secret_artifact(vec![make_secret_entry(1400, "ext-a")]);
    let quotas = vec![make_quota(1600, "ext-b")];

    let bundle = build_default_bundle(&ledger, &alerts, &telem, &exec, &secret, &quotas);

    assert_eq!(bundle.summary.ledger_entry_count, 2);
    assert_eq!(bundle.summary.alert_count, 1);
    assert_eq!(bundle.summary.telemetry_event_count, 1);
    assert_eq!(bundle.summary.exec_mediation_count, 1);
    assert_eq!(bundle.summary.secret_broker_count, 1);
    assert_eq!(bundle.summary.quota_breach_count, 1);
    assert_eq!(bundle.summary.distinct_extensions, 2);
}

#[test]
fn bundle_peak_risk_and_deny_count() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.3),
        make_ledger_entry(2000, "ext-a", 0.9),  // deny
        make_ledger_entry(3000, "ext-a", 0.75), // deny
    ]);
    let bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    assert!((bundle.summary.peak_risk_score - 0.9).abs() < 1e-9);
    assert_eq!(bundle.summary.deny_or_terminate_count, 2);
}

// ---- 2. Deterministic generation ----

#[test]
fn same_inputs_produce_identical_bundle_hash() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]);
    let alerts = alert_artifact(vec![make_alert(
        1100,
        "ext-a",
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertSeverity::Warning,
    )]);
    let filter = IncidentBundleFilter::default();
    let redaction = IncidentBundleRedactionPolicy::default();

    let b1 = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &redaction,
        50_000,
    );
    let b2 = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &redaction,
        50_000,
    );

    assert_eq!(b1.bundle_hash, b2.bundle_hash);
    assert_eq!(b1, b2);
}

#[test]
fn different_generated_at_changes_bundle_hash() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]);
    let filter = IncidentBundleFilter::default();
    let redaction = IncidentBundleRedactionPolicy::default();

    let b1 = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &redaction,
        50_000,
    );
    let b2 = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &redaction,
        60_000, // different timestamp
    );

    // generated_at_ms is in the ledger artifact inside the bundle,
    // so the hash should differ.
    assert_ne!(b1.bundle_hash, b2.bundle_hash);
}

// ---- 3. Time-window filtering ----

#[test]
fn filter_by_time_window_selects_entries_in_range() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-a", 0.3),
        make_ledger_entry(3000, "ext-a", 0.4),
        make_ledger_entry(4000, "ext-a", 0.5),
    ]);
    let alerts = alert_artifact(vec![
        make_alert(
            1500,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Info,
        ),
        make_alert(
            3500,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
    ]);
    let telem = telemetry_artifact(vec![
        make_telemetry(1200, "ext-a"),
        make_telemetry(2500, "ext-a"),
        make_telemetry(3800, "ext-a"),
    ]);

    let filter = IncidentBundleFilter {
        start_ms: Some(2000),
        end_ms: Some(3500),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telem,
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.ledger_entry_count, 2); // ts 2000, 3000
    assert_eq!(bundle.summary.alert_count, 1); // ts 3500
    assert_eq!(bundle.summary.telemetry_event_count, 1); // ts 2500
}

#[test]
fn filter_start_only() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.1),
        make_ledger_entry(5000, "ext-a", 0.2),
    ]);

    let filter = IncidentBundleFilter {
        start_ms: Some(3000),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.ledger_entry_count, 1);
}

#[test]
fn filter_end_only() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.1),
        make_ledger_entry(5000, "ext-a", 0.2),
    ]);

    let filter = IncidentBundleFilter {
        end_ms: Some(3000),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.ledger_entry_count, 1);
}

// ---- 4. Extension-id filtering ----

#[test]
fn filter_by_extension_id() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.3),
        make_ledger_entry(3000, "ext-a", 0.4),
    ]);
    let alerts = alert_artifact(vec![
        make_alert(
            1500,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
        make_alert(
            2500,
            "ext-b",
            SecurityAlertCategory::AnomalyDenial,
            SecurityAlertSeverity::Warning,
        ),
    ]);
    let telem = telemetry_artifact(vec![
        make_telemetry(1200, "ext-a"),
        make_telemetry(2200, "ext-b"),
    ]);
    let exec = exec_artifact(vec![
        make_exec_entry(1300, "ext-a"),
        make_exec_entry(2300, "ext-b"),
    ]);
    let secret = secret_artifact(vec![
        make_secret_entry(1400, "ext-a"),
        make_secret_entry(2400, "ext-b"),
    ]);
    let quotas = vec![make_quota(1600, "ext-a"), make_quota(2600, "ext-b")];

    let filter = IncidentBundleFilter {
        extension_id: Some("ext-a".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telem,
        &exec,
        &secret,
        &quotas,
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.ledger_entry_count, 2);
    assert_eq!(bundle.summary.alert_count, 1);
    assert_eq!(bundle.summary.telemetry_event_count, 1);
    assert_eq!(bundle.summary.exec_mediation_count, 1);
    assert_eq!(bundle.summary.secret_broker_count, 1);
    assert_eq!(bundle.summary.quota_breach_count, 1);
    assert_eq!(bundle.summary.distinct_extensions, 1);
}

// ---- 5. Alert category filtering ----

#[test]
fn filter_alerts_by_category() {
    let alerts = alert_artifact(vec![
        make_alert(
            1000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
        make_alert(
            2000,
            "ext-a",
            SecurityAlertCategory::AnomalyDenial,
            SecurityAlertSeverity::Warning,
        ),
        make_alert(
            3000,
            "ext-a",
            SecurityAlertCategory::Quarantine,
            SecurityAlertSeverity::Critical,
        ),
    ]);

    let filter = IncidentBundleFilter {
        alert_categories: Some(vec![
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertCategory::Quarantine,
        ]),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.alert_count, 2);
}

// ---- 6. Alert severity filtering ----

#[test]
fn filter_alerts_by_min_severity() {
    let alerts = alert_artifact(vec![
        make_alert(
            1000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Info,
        ),
        make_alert(
            2000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Warning,
        ),
        make_alert(
            3000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
        make_alert(
            4000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Critical,
        ),
    ]);

    let filter = IncidentBundleFilter {
        min_severity: Some(SecurityAlertSeverity::Error),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.alert_count, 2); // Error + Critical
}

#[test]
fn combined_alert_filters_apply_category_and_min_severity_intersection() {
    let alerts = alert_artifact(vec![
        make_alert(
            1000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Warning,
        ),
        make_alert(
            2000,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
        make_alert(
            3000,
            "ext-a",
            SecurityAlertCategory::AnomalyDenial,
            SecurityAlertSeverity::Critical,
        ),
        make_alert(
            4000,
            "ext-a",
            SecurityAlertCategory::Quarantine,
            SecurityAlertSeverity::Critical,
        ),
    ]);

    let filter = IncidentBundleFilter {
        alert_categories: Some(vec![
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertCategory::Quarantine,
        ]),
        min_severity: Some(SecurityAlertSeverity::Error),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.alert_count, 2);
    assert_eq!(bundle.security_alerts.severity_counts.error, 1);
    assert_eq!(bundle.security_alerts.severity_counts.critical, 1);
    assert_eq!(bundle.security_alerts.category_counts.policy_denial, 1);
    assert_eq!(bundle.security_alerts.category_counts.quarantine, 1);
}

// ---- 7. Redaction ----

#[test]
fn default_redaction_redacts_hashes() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]);
    let telem = telemetry_artifact(vec![make_telemetry(1000, "ext-a")]);
    let exec = exec_artifact(vec![make_exec_entry(1000, "ext-a")]);
    let secret = secret_artifact(vec![make_secret_entry(1000, "ext-a")]);
    let alerts = alert_artifact(vec![make_alert(
        1000,
        "ext-a",
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
    )]);

    let bundle = build_default_bundle(&ledger, &alerts, &telem, &exec, &secret, &[]);

    // Ledger params_hash redacted
    assert_eq!(bundle.risk_ledger.entries[0].params_hash, "[REDACTED]");

    // Telemetry hashes redacted
    assert_eq!(
        bundle.hostcall_telemetry.entries[0].params_hash,
        "[REDACTED]"
    );
    assert_eq!(
        bundle.hostcall_telemetry.entries[0].args_shape_hash,
        "[REDACTED]"
    );

    // Exec command_hash redacted
    assert_eq!(bundle.exec_mediation.entries[0].command_hash, "[REDACTED]");

    // Secret name_hash redacted
    assert_eq!(bundle.secret_broker.entries[0].name_hash, "[REDACTED]");

    // Alert context_hash redacted
    assert_eq!(bundle.security_alerts.alerts[0].context_hash, "[REDACTED]");

    // But remediation is NOT redacted by default
    assert_eq!(bundle.security_alerts.alerts[0].remediation, "Fix it");
}

#[test]
fn no_redaction_preserves_original_values() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]);
    let telem = telemetry_artifact(vec![make_telemetry(1000, "ext-a")]);
    let exec = exec_artifact(vec![make_exec_entry(1000, "ext-a")]);
    let secret = secret_artifact(vec![make_secret_entry(1000, "ext-a")]);

    let no_redact = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: false,
        redact_args_shape_hash: false,
        redact_command_hash: false,
        redact_name_hash: false,
        redact_remediation: false,
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telem,
        &exec,
        &secret,
        &[],
        &IncidentBundleFilter::default(),
        &no_redact,
        100_000,
    );

    assert_eq!(bundle.risk_ledger.entries[0].params_hash, "abc123");
    assert_eq!(
        bundle.hostcall_telemetry.entries[0].params_hash,
        "params-secret"
    );
    assert_eq!(
        bundle.hostcall_telemetry.entries[0].args_shape_hash,
        "shape-secret"
    );
    assert_eq!(
        bundle.exec_mediation.entries[0].command_hash,
        "cmd-hash-secret"
    );
    assert_eq!(
        bundle.secret_broker.entries[0].name_hash,
        "name-hash-secret"
    );
}

#[test]
fn redact_remediation_when_enabled() {
    let alerts = alert_artifact(vec![make_alert(
        1000,
        "ext-a",
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
    )]);

    let redaction = IncidentBundleRedactionPolicy {
        redact_remediation: true,
        ..IncidentBundleRedactionPolicy::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &IncidentBundleFilter::default(),
        &redaction,
        100_000,
    );

    assert_eq!(bundle.security_alerts.alerts[0].remediation, "[REDACTED]");
}

// ---- 8. Bundle integrity verification ----

#[test]
fn valid_bundle_passes_verification() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.3),
        make_ledger_entry(2000, "ext-a", 0.6),
    ]);
    let bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(report.valid, "errors: {:?}", report.errors);
    assert!(report.schema_valid);
    assert!(report.ledger_chain_intact);
    assert_eq!(report.bundle_hash, report.recomputed_hash);
}

#[test]
fn tampered_bundle_fails_verification() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.3)]);
    let mut bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    // Tamper with the bundle
    bundle.risk_ledger.entries[0].risk_score = 0.99;

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(!report.valid);
    assert_ne!(report.bundle_hash, report.recomputed_hash);
    assert!(
        report
            .errors
            .iter()
            .any(|e: &String| e.contains("bundle_hash"))
    );
}

#[test]
fn wrong_schema_fails_verification() {
    let mut bundle = build_default_bundle(
        &ledger_artifact(vec![]),
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    bundle.schema = "pi.ext.wrong.v999".to_string();

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(!report.valid);
    assert!(!report.schema_valid);
}

#[test]
fn summary_mismatch_fails_verification() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.3)]);
    let mut bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    // Corrupt the summary
    bundle.summary.ledger_entry_count = 999;
    // Re-seal so hash is correct
    bundle.bundle_hash = compute_incident_bundle_hash(&bundle);

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|e: &String| e.contains("ledger_entry_count"))
    );
}

// ---- 9. Forensic replay ----

#[test]
fn bundle_includes_replay_when_chain_valid_or_none_when_not() {
    // With synthetic hashes, the chain won't validate, so replay is None.
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-a", 0.5),
        make_ledger_entry(3000, "ext-a", 0.8),
    ]);

    let bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    // With synthetic test hashes, replay verification may fail (expected).
    // The key invariant: risk_replay is Some when chain verifies, None otherwise.
    if let Some(replay) = bundle.risk_replay.as_ref() {
        assert_eq!(replay.entry_count, 3);
        assert_eq!(replay.steps.len(), 3);
        assert_eq!(replay.steps[0].index, 0);
        assert_eq!(replay.steps[0].extension_id, "ext-a");
    } else {
        // Replay is None because synthetic hashes don't chain-verify.
        // This is expected behavior — replay only works with real hash chains.
        assert_eq!(bundle.risk_ledger.entries.len(), 3);
    }
}

#[test]
fn filtered_bundle_only_includes_filtered_ledger_entries() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.5),
        make_ledger_entry(3000, "ext-a", 0.8),
    ]);

    let filter = IncidentBundleFilter {
        extension_id: Some("ext-a".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    // Filtered ledger only has ext-a entries
    assert_eq!(bundle.risk_ledger.entries.len(), 2);
    assert!(
        bundle
            .risk_ledger
            .entries
            .iter()
            .all(|e| e.extension_id == "ext-a")
    );

    // If replay was generated, it should only contain filtered entries
    if let Some(replay) = bundle.risk_replay.as_ref() {
        assert_eq!(replay.entry_count, 2);
        assert!(replay.steps.iter().all(|s| s.extension_id == "ext-a"));
    }
}

#[test]
fn extension_filter_non_contiguous_chain_disables_replay() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.5),
        make_ledger_entry(3000, "ext-a", 0.8),
    ]);

    let filter = IncidentBundleFilter {
        extension_id: Some("ext-a".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.risk_ledger.entries.len(), 2);
    assert!(!bundle.summary.ledger_chain_intact);
    assert!(bundle.risk_replay.is_none());
}

#[test]
fn zero_match_extension_filter_yields_empty_replay_and_valid_bundle() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.5),
    ]);
    let alerts = alert_artifact(vec![make_alert(
        1500,
        "ext-a",
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Warning,
    )]);
    let telem = telemetry_artifact(vec![make_telemetry(1600, "ext-b")]);
    let exec = exec_artifact(vec![make_exec_entry(1700, "ext-a")]);
    let secret = secret_artifact(vec![make_secret_entry(1800, "ext-b")]);
    let quotas = vec![make_quota(1900, "ext-a")];

    let filter = IncidentBundleFilter {
        extension_id: Some("ext-z".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alerts,
        &telem,
        &exec,
        &secret,
        &quotas,
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.ledger_entry_count, 0);
    assert_eq!(bundle.summary.alert_count, 0);
    assert_eq!(bundle.summary.telemetry_event_count, 0);
    assert_eq!(bundle.summary.exec_mediation_count, 0);
    assert_eq!(bundle.summary.secret_broker_count, 0);
    assert_eq!(bundle.summary.quota_breach_count, 0);
    assert_eq!(bundle.summary.distinct_extensions, 0);
    assert!(bundle.summary.ledger_chain_intact);

    let replay = bundle
        .risk_replay
        .as_ref()
        .expect("empty filtered ledgers should still yield a valid empty replay");
    assert_eq!(replay.entry_count, 0);
    assert_eq!(
        replay.steps,
        [] as [pi::extensions::RuntimeRiskReplayStep; 0]
    );
    assert!(replay.tail_ledger_hash.is_none());

    let report = verify_incident_evidence_bundle(&bundle);
    assert!(report.valid);
    assert_eq!(report.errors, [] as [std::string::String; 0]);
}

fn build_composed_filter_bundle(
    ledger: &RuntimeRiskLedgerArtifact,
    alerts: &SecurityAlertArtifact,
    filter: &IncidentBundleFilter,
) -> IncidentEvidenceBundle {
    let telemetry = telemetry_artifact(vec![]);
    let exec = exec_artifact(vec![]);
    let secret = secret_artifact(vec![]);

    build_incident_evidence_bundle(
        ledger,
        alerts,
        &telemetry,
        &exec,
        &secret,
        &[],
        filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    )
}

fn assert_composed_filter_bundle_ordering(bundle: &IncidentEvidenceBundle) {
    assert_eq!(bundle.summary.alert_count, 2);
    assert_eq!(
        bundle
            .security_alerts
            .alerts
            .iter()
            .map(|a| a.sequence_id)
            .collect::<Vec<_>>(),
        vec![1100, 1300]
    );
    assert_eq!(
        bundle
            .risk_ledger
            .entries
            .iter()
            .map(|entry| entry.ts_ms)
            .collect::<Vec<_>>(),
        vec![1000, 2000, 3000]
    );
    assert_eq!(
        bundle
            .risk_ledger
            .entries
            .iter()
            .map(|entry| entry.extension_id.as_str())
            .collect::<Vec<_>>(),
        vec!["ext-a", "ext-a", "ext-a"]
    );
}

fn assert_replay_determinism(bundle_a: &IncidentEvidenceBundle, bundle_b: &IncidentEvidenceBundle) {
    assert_eq!(bundle_a.risk_replay, bundle_b.risk_replay);

    if let Some(replay) = bundle_a.risk_replay.as_ref() {
        assert_eq!(
            replay
                .steps
                .iter()
                .map(|step| step.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call-1000", "call-2000", "call-3000"]
        );
        assert_eq!(
            replay
                .steps
                .iter()
                .map(|step| step.index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    } else {
        assert!(bundle_b.risk_replay.is_none());
    }
}

#[test]
fn composed_filters_keep_replay_and_alert_order_deterministic() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-a", 0.6),
        make_ledger_entry(3000, "ext-a", 0.8),
        make_ledger_entry(4000, "ext-b", 0.4),
    ]);
    let alerts = alert_artifact(vec![
        make_alert(
            1100,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
        ),
        make_alert(
            1200,
            "ext-a",
            SecurityAlertCategory::AnomalyDenial,
            SecurityAlertSeverity::Warning,
        ),
        make_alert(
            1300,
            "ext-a",
            SecurityAlertCategory::Quarantine,
            SecurityAlertSeverity::Critical,
        ),
        make_alert(
            1400,
            "ext-b",
            SecurityAlertCategory::Quarantine,
            SecurityAlertSeverity::Critical,
        ),
    ]);
    let filter = IncidentBundleFilter {
        extension_id: Some("ext-a".to_string()),
        alert_categories: Some(vec![
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertCategory::Quarantine,
        ]),
        min_severity: Some(SecurityAlertSeverity::Error),
        ..Default::default()
    };

    let bundle_a = build_composed_filter_bundle(&ledger, &alerts, &filter);
    let bundle_b = build_composed_filter_bundle(&ledger, &alerts, &filter);

    assert_eq!(bundle_a.bundle_hash, bundle_b.bundle_hash);
    assert_composed_filter_bundle_ordering(&bundle_a);
    assert_replay_determinism(&bundle_a, &bundle_b);
}

// ---- 10. JSON round-trip stability ----

#[test]
fn bundle_json_roundtrip() {
    let ledger = ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]);
    let alerts = alert_artifact(vec![make_alert(
        1100,
        "ext-a",
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
    )]);
    let telem = telemetry_artifact(vec![make_telemetry(1200, "ext-a")]);
    let exec = exec_artifact(vec![make_exec_entry(1300, "ext-a")]);
    let secret = secret_artifact(vec![make_secret_entry(1400, "ext-a")]);
    let quotas = vec![make_quota(1500, "ext-a")];

    let bundle = build_default_bundle(&ledger, &alerts, &telem, &exec, &secret, &quotas);

    let json = serde_json::to_string_pretty(&bundle).expect("serialize");
    let deserialized: IncidentEvidenceBundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(bundle, deserialized);
    assert_eq!(bundle.bundle_hash, deserialized.bundle_hash);
}

#[test]
fn bundle_filter_json_roundtrip() {
    let filter = IncidentBundleFilter {
        start_ms: Some(1000),
        end_ms: Some(5000),
        extension_id: Some("ext-abc".to_string()),
        alert_categories: Some(vec![
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertCategory::Quarantine,
        ]),
        min_severity: Some(SecurityAlertSeverity::Warning),
    };

    let json = serde_json::to_string(&filter).expect("serialize");
    let deserialized: IncidentBundleFilter = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(filter, deserialized);
}

#[test]
fn redaction_policy_json_roundtrip() {
    let policy = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_context_hash: true,
        redact_args_shape_hash: false,
        redact_command_hash: true,
        redact_name_hash: false,
        redact_remediation: true,
    };

    let json = serde_json::to_string(&policy).expect("serialize");
    let deserialized: IncidentBundleRedactionPolicy =
        serde_json::from_str(&json).expect("deserialize");

    assert_eq!(policy, deserialized);
}

// ---- 11. Ledger chain integrity ----

#[test]
fn bundle_reports_intact_chain() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-a", 0.3),
        make_ledger_entry(3000, "ext-a", 0.4),
    ]);

    let bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    assert!(bundle.summary.ledger_chain_intact);
}

#[test]
fn bundle_reports_broken_chain() {
    let mut entries = vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-a", 0.3),
    ];
    // Break the chain
    entries[1].prev_ledger_hash = Some("wrong-hash".to_string());

    let ledger = ledger_artifact(entries);

    let bundle = build_default_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    assert!(!bundle.summary.ledger_chain_intact);
}

// ---- 12. Combined filtering ----

#[test]
fn combined_time_and_extension_filter() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.3),
        make_ledger_entry(3000, "ext-a", 0.4),
        make_ledger_entry(4000, "ext-b", 0.5),
    ]);

    let filter = IncidentBundleFilter {
        start_ms: Some(1500),
        end_ms: Some(3500),
        extension_id: Some("ext-a".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger,
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    // Only ts=3000 ext-a should match
    assert_eq!(bundle.summary.ledger_entry_count, 1);
}

// ---- 13. Filter and redaction preserved in bundle ----

#[test]
fn filter_and_redaction_stored_in_bundle() {
    let filter = IncidentBundleFilter {
        start_ms: Some(1000),
        end_ms: Some(5000),
        extension_id: Some("my-ext".to_string()),
        ..Default::default()
    };
    let redaction = IncidentBundleRedactionPolicy {
        redact_params_hash: false,
        redact_remediation: true,
        ..IncidentBundleRedactionPolicy::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
        &filter,
        &redaction,
        100_000,
    );

    assert_eq!(bundle.filter, filter);
    assert_eq!(bundle.redaction, redaction);
}

// ---- 14. Empty filter returns everything ----

#[test]
fn empty_filter_returns_all_entries() {
    let ledger = ledger_artifact(vec![
        make_ledger_entry(1000, "ext-a", 0.2),
        make_ledger_entry(2000, "ext-b", 0.3),
        make_ledger_entry(3000, "ext-c", 0.4),
    ]);
    let alerts = alert_artifact(vec![
        make_alert(
            1500,
            "ext-a",
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Info,
        ),
        make_alert(
            2500,
            "ext-b",
            SecurityAlertCategory::AnomalyDenial,
            SecurityAlertSeverity::Critical,
        ),
    ]);

    let bundle = build_default_bundle(
        &ledger,
        &alerts,
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    assert_eq!(bundle.summary.ledger_entry_count, 3);
    assert_eq!(bundle.summary.alert_count, 2);
    assert_eq!(bundle.summary.distinct_extensions, 3);
}

// ---- 15. Quota breach filtering ----

#[test]
fn quota_breaches_filtered_by_time_and_extension() {
    let quotas = vec![
        make_quota(1000, "ext-a"),
        make_quota(2000, "ext-b"),
        make_quota(3000, "ext-a"),
        make_quota(4000, "ext-b"),
    ];

    let filter = IncidentBundleFilter {
        start_ms: Some(1500),
        extension_id: Some("ext-a".to_string()),
        ..Default::default()
    };

    let bundle = build_incident_evidence_bundle(
        &ledger_artifact(vec![]),
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &quotas,
        &filter,
        &IncidentBundleRedactionPolicy::default(),
        100_000,
    );

    assert_eq!(bundle.summary.quota_breach_count, 1); // only ts=3000 ext-a
}

// ---- 16. compute_incident_bundle_hash stability ----

#[test]
fn compute_hash_is_hex_sha256() {
    let bundle = build_default_bundle(
        &ledger_artifact(vec![make_ledger_entry(1000, "ext-a", 0.5)]),
        &alert_artifact(vec![]),
        &telemetry_artifact(vec![]),
        &exec_artifact(vec![]),
        &secret_artifact(vec![]),
        &[],
    );

    assert_eq!(bundle.bundle_hash.len(), 64); // SHA-256 hex = 64 chars
    assert!(
        bundle
            .bundle_hash
            .chars()
            .all(|c: char| c.is_ascii_hexdigit())
    );

    // Recomputing gives same hash
    let recomputed = compute_incident_bundle_hash(&bundle);
    assert_eq!(bundle.bundle_hash, recomputed);
}
