//! Integration tests for SEC-5.1: Security control center and real-time
//! runtime alerts with reason codes.
//!
//! Verifies the `SecurityAlert` types, alert stream recording, artifact
//! export, and category/severity classification.

use pi::extensions::{
    ExtensionManager, RuntimeRiskStateLabelValue, SECURITY_ALERT_SCHEMA_VERSION, SecurityAlert,
    SecurityAlertAction, SecurityAlertArtifact, SecurityAlertCategory, SecurityAlertCategoryCounts,
    SecurityAlertSeverity, SecurityAlertSeverityCounts,
};

// ==========================================================================
// Alert category classification
// ==========================================================================

#[test]
fn alert_categories_are_distinct() {
    let cats = [
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertCategory::ExecMediation,
        SecurityAlertCategory::SecretBroker,
        SecurityAlertCategory::QuotaBreach,
        SecurityAlertCategory::Quarantine,
        SecurityAlertCategory::ProfileTransition,
    ];
    // All categories are distinct.
    for (i, a) in cats.iter().enumerate() {
        for (j, b) in cats.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "Categories at {i} and {j} should differ");
            }
        }
    }
}

#[test]
fn alert_category_serde_roundtrip() {
    for cat in [
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertCategory::ExecMediation,
        SecurityAlertCategory::SecretBroker,
        SecurityAlertCategory::QuotaBreach,
        SecurityAlertCategory::Quarantine,
        SecurityAlertCategory::ProfileTransition,
    ] {
        let json = serde_json::to_string(&cat).expect("serialize");
        let restored: SecurityAlertCategory = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cat, restored);
    }
}

// ==========================================================================
// Alert severity ordering
// ==========================================================================

#[test]
fn alert_severity_ordering() {
    assert!(SecurityAlertSeverity::Info < SecurityAlertSeverity::Warning);
    assert!(SecurityAlertSeverity::Warning < SecurityAlertSeverity::Error);
    assert!(SecurityAlertSeverity::Error < SecurityAlertSeverity::Critical);
}

#[test]
fn alert_severity_serde_roundtrip() {
    for sev in [
        SecurityAlertSeverity::Info,
        SecurityAlertSeverity::Warning,
        SecurityAlertSeverity::Error,
        SecurityAlertSeverity::Critical,
    ] {
        let json = serde_json::to_string(&sev).expect("serialize");
        let restored: SecurityAlertSeverity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sev, restored);
    }
}

// ==========================================================================
// Alert action types
// ==========================================================================

#[test]
fn alert_action_serde_roundtrip() {
    for action in [
        SecurityAlertAction::Allow,
        SecurityAlertAction::Harden,
        SecurityAlertAction::Prompt,
        SecurityAlertAction::Deny,
        SecurityAlertAction::Terminate,
        SecurityAlertAction::Redact,
    ] {
        let json = serde_json::to_string(&action).expect("serialize");
        let restored: SecurityAlertAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(action, restored);
    }
}

// ==========================================================================
// SecurityAlert struct: who/what/why/action fields
// ==========================================================================

fn sample_alert(
    category: SecurityAlertCategory,
    severity: SecurityAlertSeverity,
    action: SecurityAlertAction,
) -> SecurityAlert {
    SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms: 1_700_000_000_000,
        sequence_id: 0,
        extension_id: "test-ext".to_string(),
        category,
        severity,
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        reason_codes: vec!["recursive_delete".to_string()],
        summary: "Exec denied: recursive delete detected".to_string(),
        policy_source: "exec_mediation".to_string(),
        action,
        remediation: "Review the command.".to_string(),
        risk_score: 0.85,
        risk_state: Some(RuntimeRiskStateLabelValue::Suspicious),
        context_hash: "abc123".to_string(),
    }
}

#[test]
fn alert_has_who_field() {
    let alert = sample_alert(
        SecurityAlertCategory::ExecMediation,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    assert_eq!(alert.extension_id, "test-ext");
}

#[test]
fn alert_has_what_fields() {
    let alert = sample_alert(
        SecurityAlertCategory::ExecMediation,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    assert_eq!(alert.category, SecurityAlertCategory::ExecMediation);
    assert_eq!(alert.capability, "exec");
    assert_eq!(alert.method, "spawn");
}

#[test]
fn alert_has_why_fields() {
    let alert = sample_alert(
        SecurityAlertCategory::ExecMediation,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    assert_ne!(alert.reason_codes, [] as [std::string::String; 0]);
    assert_ne!(alert.summary, "");
    assert_eq!(alert.policy_source, "exec_mediation");
}

#[test]
fn alert_has_action_field() {
    let alert = sample_alert(
        SecurityAlertCategory::ExecMediation,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    assert_eq!(alert.action, SecurityAlertAction::Deny);
}

#[test]
fn alert_serde_roundtrip() {
    let alert = sample_alert(
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertSeverity::Critical,
        SecurityAlertAction::Terminate,
    );
    let json = serde_json::to_string_pretty(&alert).expect("serialize");
    let restored: SecurityAlert = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.category, alert.category);
    assert_eq!(restored.severity, alert.severity);
    assert_eq!(restored.action, alert.action);
    assert_eq!(restored.extension_id, alert.extension_id);
    assert_eq!(restored.reason_codes, alert.reason_codes);
    assert_eq!(restored.summary, alert.summary);
    assert!((restored.risk_score - alert.risk_score).abs() < f64::EPSILON);
    assert_eq!(restored.risk_state, alert.risk_state);
}

#[test]
fn alert_json_has_stable_field_names() {
    let alert = sample_alert(
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    let json = serde_json::to_string(&alert).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

    // Verify stable field names for downstream integrations.
    assert!(parsed["schema"].is_string());
    assert!(parsed["ts_ms"].is_number());
    assert!(parsed["sequence_id"].is_number());
    assert!(parsed["extension_id"].is_string());
    assert!(parsed["category"].is_string());
    assert!(parsed["severity"].is_string());
    assert!(parsed["capability"].is_string());
    assert!(parsed["method"].is_string());
    assert!(parsed["reason_codes"].is_array());
    assert!(parsed["summary"].is_string());
    assert!(parsed["policy_source"].is_string());
    assert!(parsed["action"].is_string());
    assert!(parsed["remediation"].is_string());
    assert!(parsed["risk_score"].is_number());
    assert!(parsed["context_hash"].is_string());
}

#[test]
fn alert_category_serializes_to_snake_case() {
    let json = serde_json::to_string(&SecurityAlertCategory::PolicyDenial).unwrap();
    assert_eq!(json, "\"policy_denial\"");

    let json = serde_json::to_string(&SecurityAlertCategory::AnomalyDenial).unwrap();
    assert_eq!(json, "\"anomaly_denial\"");

    let json = serde_json::to_string(&SecurityAlertCategory::ExecMediation).unwrap();
    assert_eq!(json, "\"exec_mediation\"");

    let json = serde_json::to_string(&SecurityAlertCategory::QuotaBreach).unwrap();
    assert_eq!(json, "\"quota_breach\"");

    let json = serde_json::to_string(&SecurityAlertCategory::Quarantine).unwrap();
    assert_eq!(json, "\"quarantine\"");

    let json = serde_json::to_string(&SecurityAlertCategory::ProfileTransition).unwrap();
    assert_eq!(json, "\"profile_transition\"");
}

// ==========================================================================
// Policy denial vs anomaly denial distinction
// ==========================================================================

#[test]
fn policy_denial_distinguishable_from_anomaly_denial() {
    let policy_alert = sample_alert(
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );
    let anomaly_alert = sample_alert(
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    );

    assert_ne!(policy_alert.category, anomaly_alert.category);

    // Both serialize to different category values.
    let p_json = serde_json::to_string(&policy_alert).unwrap();
    let a_json = serde_json::to_string(&anomaly_alert).unwrap();
    assert!(p_json.contains("\"policy_denial\""));
    assert!(a_json.contains("\"anomaly_denial\""));
}

// ==========================================================================
// Category counts
// ==========================================================================

#[test]
fn category_counts_default_is_zero() {
    let counts = SecurityAlertCategoryCounts::default();
    assert_eq!(counts.policy_denial, 0);
    assert_eq!(counts.anomaly_denial, 0);
    assert_eq!(counts.exec_mediation, 0);
    assert_eq!(counts.quota_breach, 0);
    assert_eq!(counts.quarantine, 0);
    assert_eq!(counts.secret_broker, 0);
    assert_eq!(counts.profile_transition, 0);
}

#[test]
fn category_counts_fields_settable() {
    let counts = SecurityAlertCategoryCounts {
        policy_denial: 2,
        anomaly_denial: 1,
        exec_mediation: 1,
        quota_breach: 1,
        quarantine: 1,
        secret_broker: 1,
        profile_transition: 1,
    };
    assert_eq!(counts.policy_denial, 2);
    assert_eq!(counts.anomaly_denial, 1);
    assert_eq!(counts.exec_mediation, 1);
}

#[test]
fn severity_counts_default_is_zero() {
    let counts = SecurityAlertSeverityCounts::default();
    assert_eq!(counts.info, 0);
    assert_eq!(counts.warning, 0);
    assert_eq!(counts.error, 0);
    assert_eq!(counts.critical, 0);
}

#[test]
fn severity_counts_fields_settable() {
    let counts = SecurityAlertSeverityCounts {
        info: 1,
        warning: 2,
        error: 1,
        critical: 1,
    };
    assert_eq!(counts.warning, 2);
    assert_eq!(counts.critical, 1);
}

// ==========================================================================
// SecurityAlertArtifact
// ==========================================================================

#[test]
fn empty_alert_artifact_has_schema() {
    let artifact = SecurityAlertArtifact {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        generated_at_ms: 0,
        alert_count: 0,
        category_counts: SecurityAlertCategoryCounts::default(),
        severity_counts: SecurityAlertSeverityCounts::default(),
        alerts: Vec::new(),
    };
    assert_eq!(artifact.schema, SECURITY_ALERT_SCHEMA_VERSION);
    assert_eq!(artifact.alert_count, 0);
}

#[test]
fn alert_artifact_serde_roundtrip() {
    let alerts = vec![
        sample_alert(
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
            SecurityAlertAction::Deny,
        ),
        sample_alert(
            SecurityAlertCategory::Quarantine,
            SecurityAlertSeverity::Critical,
            SecurityAlertAction::Terminate,
        ),
    ];

    let artifact = SecurityAlertArtifact {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1_700_000_000_000,
        alert_count: alerts.len(),
        category_counts: SecurityAlertCategoryCounts {
            policy_denial: 1,
            quarantine: 1,
            ..Default::default()
        },
        severity_counts: SecurityAlertSeverityCounts {
            error: 1,
            critical: 1,
            ..Default::default()
        },
        alerts,
    };

    let json = serde_json::to_string_pretty(&artifact).expect("serialize");
    let restored: SecurityAlertArtifact = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.schema, artifact.schema);
    assert_eq!(restored.alert_count, 2);
    assert_eq!(restored.category_counts.policy_denial, 1);
    assert_eq!(restored.category_counts.quarantine, 1);
    assert_eq!(restored.severity_counts.error, 1);
    assert_eq!(restored.severity_counts.critical, 1);
    assert_eq!(restored.alerts.len(), 2);
}

// ==========================================================================
// ExtensionManager alert stream
// ==========================================================================

#[test]
fn manager_records_and_exports_alerts() {
    let mgr = ExtensionManager::new();

    assert_eq!(mgr.security_alert_count(), 0);

    mgr.record_security_alert(sample_alert(
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    ));
    mgr.record_security_alert(sample_alert(
        SecurityAlertCategory::AnomalyDenial,
        SecurityAlertSeverity::Warning,
        SecurityAlertAction::Harden,
    ));

    assert_eq!(mgr.security_alert_count(), 2);

    let artifact = mgr.security_alert_artifact();
    assert_eq!(artifact.schema, SECURITY_ALERT_SCHEMA_VERSION);
    assert_eq!(artifact.alert_count, 2);
    assert_eq!(artifact.category_counts.policy_denial, 1);
    assert_eq!(artifact.category_counts.anomaly_denial, 1);
    assert_eq!(artifact.severity_counts.error, 1);
    assert_eq!(artifact.severity_counts.warning, 1);
}

#[test]
fn manager_assigns_monotonic_sequence_ids() {
    let mgr = ExtensionManager::new();

    for _ in 0..5 {
        mgr.record_security_alert(sample_alert(
            SecurityAlertCategory::PolicyDenial,
            SecurityAlertSeverity::Error,
            SecurityAlertAction::Deny,
        ));
    }

    let artifact = mgr.security_alert_artifact();
    let ids: Vec<u64> = artifact.alerts.iter().map(|a| a.sequence_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn empty_manager_exports_empty_artifact() {
    let mgr = ExtensionManager::new();
    let artifact = mgr.security_alert_artifact();
    assert_eq!(artifact.alert_count, 0);
    assert_eq!(artifact.alerts, [] as [pi::extensions::SecurityAlert; 0]);
    assert_eq!(artifact.category_counts.policy_denial, 0);
    assert_eq!(artifact.severity_counts.info, 0);
}

// ==========================================================================
// Schema version constant
// ==========================================================================

#[test]
fn schema_version_is_stable() {
    assert_eq!(SECURITY_ALERT_SCHEMA_VERSION, "pi.ext.security_alert.v1");
}

// ==========================================================================
// Alert risk state context
// ==========================================================================

#[test]
fn alert_with_risk_state() {
    let alert = SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms: 0,
        sequence_id: 1,
        extension_id: "ext".to_string(),
        category: SecurityAlertCategory::AnomalyDenial,
        severity: SecurityAlertSeverity::Error,
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        reason_codes: vec!["burst".to_string()],
        summary: "Burst detected".to_string(),
        policy_source: "risk_scorer".to_string(),
        action: SecurityAlertAction::Deny,
        remediation: String::new(),
        risk_score: 0.95,
        risk_state: Some(RuntimeRiskStateLabelValue::Unsafe),
        context_hash: String::new(),
    };
    assert_eq!(alert.risk_state, Some(RuntimeRiskStateLabelValue::Unsafe));
    assert!(alert.risk_score > 0.9);
}

#[test]
fn alert_without_risk_state() {
    let alert = SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms: 0,
        sequence_id: 1,
        extension_id: "ext".to_string(),
        category: SecurityAlertCategory::PolicyDenial,
        severity: SecurityAlertSeverity::Error,
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        reason_codes: vec!["deny_caps".to_string()],
        summary: "Policy denial".to_string(),
        policy_source: "deny_caps".to_string(),
        action: SecurityAlertAction::Deny,
        remediation: String::new(),
        risk_score: 0.0,
        risk_state: None,
        context_hash: String::new(),
    };
    assert!(alert.risk_state.is_none());
    assert!(alert.risk_score.abs() < f64::EPSILON);
}

// ==========================================================================
// Edge cases
// ==========================================================================

#[test]
fn alert_with_empty_reason_codes() {
    let alert = SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms: 0,
        sequence_id: 0,
        extension_id: String::new(),
        category: SecurityAlertCategory::SecretBroker,
        severity: SecurityAlertSeverity::Info,
        capability: "env".to_string(),
        method: "get".to_string(),
        reason_codes: Vec::new(),
        summary: "Secret redacted".to_string(),
        policy_source: "secret_broker".to_string(),
        action: SecurityAlertAction::Redact,
        remediation: String::new(),
        risk_score: 0.0,
        risk_state: None,
        context_hash: String::new(),
    };
    let json = serde_json::to_string(&alert).expect("serialize");
    let restored: SecurityAlert = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.reason_codes, [] as [std::string::String; 0]);
    assert_eq!(restored.action, SecurityAlertAction::Redact);
}

#[test]
fn alert_with_multiple_reason_codes() {
    let alert = SecurityAlert {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        ts_ms: 0,
        sequence_id: 0,
        extension_id: "ext".to_string(),
        category: SecurityAlertCategory::AnomalyDenial,
        severity: SecurityAlertSeverity::Critical,
        capability: "exec".to_string(),
        method: "spawn".to_string(),
        reason_codes: vec![
            "burst_1s".to_string(),
            "high_error_rate".to_string(),
            "drift_detected".to_string(),
        ],
        summary: "Multiple anomalies".to_string(),
        policy_source: "risk_scorer".to_string(),
        action: SecurityAlertAction::Terminate,
        remediation: "Remove extension".to_string(),
        risk_score: 0.99,
        risk_state: Some(RuntimeRiskStateLabelValue::Unsafe),
        context_hash: "deadbeef".to_string(),
    };
    assert_eq!(alert.reason_codes.len(), 3);
    assert!(alert.reason_codes.contains(&"burst_1s".to_string()));
    assert!(alert.reason_codes.contains(&"drift_detected".to_string()));
}

#[test]
fn category_counts_serde_roundtrip() {
    let counts = SecurityAlertCategoryCounts {
        policy_denial: 1,
        quarantine: 1,
        ..Default::default()
    };

    let json = serde_json::to_string(&counts).expect("serialize");
    let restored: SecurityAlertCategoryCounts = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.policy_denial, 1);
    assert_eq!(restored.quarantine, 1);
    assert_eq!(restored.anomaly_denial, 0);
}

#[test]
fn severity_counts_serde_roundtrip() {
    let counts = SecurityAlertSeverityCounts {
        critical: 2,
        ..Default::default()
    };

    let json = serde_json::to_string(&counts).expect("serialize");
    let restored: SecurityAlertSeverityCounts = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.critical, 2);
    assert_eq!(restored.info, 0);
}

// ==========================================================================
// Acceptance criterion: alert stream stability
// ==========================================================================

#[test]
fn alert_artifact_json_stability() {
    // Verify that the artifact JSON format is stable across serialization.
    let mgr = ExtensionManager::new();
    mgr.record_security_alert(sample_alert(
        SecurityAlertCategory::PolicyDenial,
        SecurityAlertSeverity::Error,
        SecurityAlertAction::Deny,
    ));

    let artifact1 = mgr.security_alert_artifact();
    let json1 = serde_json::to_string_pretty(&artifact1).expect("serialize");
    let artifact2: SecurityAlertArtifact = serde_json::from_str(&json1).expect("deserialize");
    let json2 = serde_json::to_string_pretty(&artifact2).expect("re-serialize");

    // The two JSON strings should match (excluding generated_at_ms which may differ).
    let v1: serde_json::Value = serde_json::from_str(&json1).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
    // Compare everything except generated_at_ms.
    assert_eq!(v1["schema"], v2["schema"]);
    assert_eq!(v1["alert_count"], v2["alert_count"]);
    assert_eq!(v1["alerts"], v2["alerts"]);
    assert_eq!(v1["category_counts"], v2["category_counts"]);
    assert_eq!(v1["severity_counts"], v2["severity_counts"]);
}
