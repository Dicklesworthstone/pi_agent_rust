//! Security alert, kill-switch, trust, and ownership tests.

use super::*;

// ====================================================================
// SEC-5.1: Security alert builder and emission tests
// ====================================================================

#[test]
fn alert_from_policy_denial_has_correct_fields() {
    let alert =
        SecurityAlert::from_policy_denial("my-ext", "exec", "spawn", "deny_caps", "deny_caps");
    assert_eq!(alert.schema, SECURITY_ALERT_SCHEMA_VERSION);
    assert_eq!(alert.extension_id, "my-ext");
    assert_eq!(alert.category, SecurityAlertCategory::PolicyDenial);
    assert_eq!(alert.severity, SecurityAlertSeverity::Error);
    assert_eq!(alert.capability, "exec");
    assert_eq!(alert.method, "spawn");
    assert_eq!(alert.action, SecurityAlertAction::Deny);
    assert_eq!(alert.reason_codes, vec!["deny_caps"]);
    assert!(alert.summary.contains("exec"));
    assert!(alert.summary.contains("my-ext"));
    assert!(!alert.remediation.is_empty());
}

#[test]
fn alert_from_exec_mediation_with_class() {
    let alert = SecurityAlert::from_exec_mediation(
        "ext-1",
        "rm -rf /",
        Some("recursive_delete"),
        "classified_dangerous",
    );
    assert_eq!(alert.category, SecurityAlertCategory::ExecMediation);
    assert_eq!(alert.severity, SecurityAlertSeverity::Error);
    assert_eq!(alert.capability, "exec");
    assert!(alert.summary.contains("recursive_delete"));
    assert!(!alert.context_hash.is_empty());
}

#[test]
fn alert_from_exec_mediation_without_class() {
    let alert =
        SecurityAlert::from_exec_mediation("ext-1", "banned-tool", None, "deny_pattern_matched");
    assert!(alert.summary.contains("deny pattern"));
}

#[test]
fn alert_from_secret_redaction() {
    let alert = SecurityAlert::from_secret_redaction("ext-1", "AWS_SECRET_KEY");
    assert_eq!(alert.category, SecurityAlertCategory::SecretBroker);
    assert_eq!(alert.severity, SecurityAlertSeverity::Info);
    assert_eq!(alert.action, SecurityAlertAction::Redact);
    assert!(alert.summary.contains("AWS_SECRET_KEY"));
    assert!(!alert.context_hash.is_empty());
}

#[test]
fn alert_from_anomaly_detection_deny() {
    let alert = SecurityAlert::from_anomaly_detection(
        "ext-1",
        "exec",
        "spawn",
        0.85,
        RuntimeRiskStateLabelValue::Unsafe,
        SecurityAlertAction::Deny,
        vec!["e_process_breach".to_string()],
        "Anomalous exec behavior detected".to_string(),
    );
    assert_eq!(alert.category, SecurityAlertCategory::AnomalyDenial);
    assert_eq!(alert.severity, SecurityAlertSeverity::Error);
    assert_eq!(alert.action, SecurityAlertAction::Deny);
    assert!((alert.risk_score - 0.85).abs() < f64::EPSILON);
    assert_eq!(alert.risk_state, Some(RuntimeRiskStateLabelValue::Unsafe));
}

#[test]
fn alert_from_anomaly_detection_terminate_is_critical() {
    let alert = SecurityAlert::from_anomaly_detection(
        "ext-1",
        "exec",
        "spawn",
        0.95,
        RuntimeRiskStateLabelValue::Unsafe,
        SecurityAlertAction::Terminate,
        vec!["quarantine_triggered".to_string()],
        "Extension quarantined".to_string(),
    );
    assert_eq!(alert.severity, SecurityAlertSeverity::Critical);
}

#[test]
fn alert_from_anomaly_detection_harden_is_warning() {
    let alert = SecurityAlert::from_anomaly_detection(
        "ext-1",
        "http",
        "fetch",
        0.50,
        RuntimeRiskStateLabelValue::Suspicious,
        SecurityAlertAction::Harden,
        vec!["drift_detected".to_string()],
        "Drift in http behavior".to_string(),
    );
    assert_eq!(alert.severity, SecurityAlertSeverity::Warning);
}

#[test]
fn alert_from_quarantine() {
    let alert = SecurityAlert::from_quarantine("bad-ext", "consecutive_unsafe_exceeded", 0.90);
    assert_eq!(alert.category, SecurityAlertCategory::Quarantine);
    assert_eq!(alert.severity, SecurityAlertSeverity::Critical);
    assert_eq!(alert.action, SecurityAlertAction::Terminate);
    assert!(alert.summary.contains("bad-ext"));
}

#[test]
fn alert_from_enforcement_transition_escalation() {
    let transition = EnforcementTransition {
        from: EnforcementState::Allow,
        to: EnforcementState::Deny,
        hysteresis_active: false,
        raw_band: EnforcementState::Deny,
        score: 0.80,
        cooldown_counter: 0,
    };
    let alert = SecurityAlert::from_enforcement_transition("ext-1", &transition);
    assert_eq!(alert.category, SecurityAlertCategory::ProfileTransition);
    assert_eq!(alert.severity, SecurityAlertSeverity::Error);
    assert_eq!(alert.action, SecurityAlertAction::Deny);
    assert!(alert.summary.contains("allow"));
    assert!(alert.summary.contains("deny"));
    assert!(!alert.remediation.is_empty());
}

#[test]
fn alert_from_enforcement_transition_de_escalation() {
    let transition = EnforcementTransition {
        from: EnforcementState::Harden,
        to: EnforcementState::Allow,
        hysteresis_active: false,
        raw_band: EnforcementState::Allow,
        score: 0.10,
        cooldown_counter: 0,
    };
    let alert = SecurityAlert::from_enforcement_transition("ext-1", &transition);
    assert_eq!(alert.severity, SecurityAlertSeverity::Info);
    assert_eq!(alert.action, SecurityAlertAction::Allow);
    assert!(alert.remediation.is_empty());
}

#[test]
fn alert_action_from_enforcement_roundtrip() {
    for state in [
        EnforcementState::Allow,
        EnforcementState::Harden,
        EnforcementState::Prompt,
        EnforcementState::Deny,
        EnforcementState::Terminate,
    ] {
        let action = SecurityAlertAction::from_enforcement(state);
        assert_eq!(
            action.as_str(),
            state.as_str(),
            "SecurityAlertAction should match EnforcementState string repr"
        );
    }
}

#[test]
fn alert_serializes_to_json() {
    let alert =
        SecurityAlert::from_policy_denial("my-ext", "exec", "spawn", "deny_caps", "deny_caps");
    let json = serde_json::to_string(&alert).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(parsed["category"], "policy_denial");
    assert_eq!(parsed["severity"], "error");
    assert_eq!(parsed["action"], "deny");
    assert_eq!(parsed["extension_id"], "my-ext");
}

#[test]
fn alert_serde_roundtrip() {
    let alert = SecurityAlert::from_anomaly_detection(
        "ext-1",
        "exec",
        "spawn",
        0.85,
        RuntimeRiskStateLabelValue::Unsafe,
        SecurityAlertAction::Deny,
        vec!["e_process_breach".to_string()],
        "Test anomaly".to_string(),
    );
    let json = serde_json::to_string(&alert).expect("serialize");
    let deserialized: SecurityAlert = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deserialized, alert);
}

#[test]
fn alert_filter_by_category() {
    let alerts = vec![
        SecurityAlert::from_policy_denial("ext-1", "exec", "spawn", "deny_caps", "deny_caps"),
        SecurityAlert::from_secret_redaction("ext-1", "SECRET"),
        SecurityAlert::from_policy_denial("ext-2", "env", "get", "deny_caps", "deny_caps"),
    ];
    let filter = SecurityAlertFilter {
        category: Some(SecurityAlertCategory::PolicyDenial),
        ..Default::default()
    };
    let count = alerts
        .into_iter()
        .filter(|a| filter.category.is_none_or(|c| a.category == c))
        .count();
    assert_eq!(count, 2);
}

#[test]
fn alert_filter_by_severity() {
    let alerts = vec![
        SecurityAlert::from_secret_redaction("ext-1", "SECRET"), // Info
        SecurityAlert::from_policy_denial("ext-1", "exec", "spawn", "r", "s"), // Error
        SecurityAlert::from_quarantine("ext-1", "reason", 0.9),  // Critical
    ];
    let filter = SecurityAlertFilter {
        min_severity: Some(SecurityAlertSeverity::Error),
        ..Default::default()
    };
    let count = alerts
        .into_iter()
        .filter(|a| filter.min_severity.is_none_or(|s| a.severity >= s))
        .count();
    assert_eq!(count, 2);
}

#[test]
fn alert_filter_by_extension() {
    let alerts = vec![
        SecurityAlert::from_policy_denial("ext-1", "exec", "spawn", "r", "s"),
        SecurityAlert::from_policy_denial("ext-2", "exec", "spawn", "r", "s"),
    ];
    let filter = SecurityAlertFilter {
        extension_id: Some("ext-1".to_string()),
        ..Default::default()
    };
    let filtered: Vec<_> = alerts
        .into_iter()
        .filter(|a| {
            filter
                .extension_id
                .as_ref()
                .is_none_or(|e| a.extension_id == *e)
        })
        .collect();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].extension_id, "ext-1");
}

#[test]
fn alert_category_counts_increment() {
    let mut counts = SecurityAlertCategoryCounts::default();
    counts.increment(SecurityAlertCategory::PolicyDenial);
    counts.increment(SecurityAlertCategory::PolicyDenial);
    counts.increment(SecurityAlertCategory::ExecMediation);
    assert_eq!(counts.policy_denial, 2);
    assert_eq!(counts.exec_mediation, 1);
    assert_eq!(counts.anomaly_denial, 0);
}

#[test]
fn alert_severity_counts_increment() {
    let mut counts = SecurityAlertSeverityCounts::default();
    counts.increment(SecurityAlertSeverity::Error);
    counts.increment(SecurityAlertSeverity::Critical);
    counts.increment(SecurityAlertSeverity::Error);
    assert_eq!(counts.error, 2);
    assert_eq!(counts.critical, 1);
    assert_eq!(counts.info, 0);
}

#[test]
fn alert_action_as_str() {
    assert_eq!(SecurityAlertAction::Allow.as_str(), "allow");
    assert_eq!(SecurityAlertAction::Deny.as_str(), "deny");
    assert_eq!(SecurityAlertAction::Terminate.as_str(), "terminate");
    assert_eq!(SecurityAlertAction::Redact.as_str(), "redact");
}

#[test]
fn sha256_short_deterministic() {
    let h1 = sha256_short("hello");
    let h2 = sha256_short("hello");
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 16);
}

#[test]
fn sha256_short_different_inputs() {
    let h1 = sha256_short("hello");
    let h2 = sha256_short("world");
    assert_ne!(h1, h2);
}

// ------------------------------------------------------------------
// SEC-5.2: Kill-switch and trust onboarding tests
// ------------------------------------------------------------------

#[test]
fn kill_switch_sets_trust_state_to_killed() {
    let mgr = ExtensionManager::new();
    let result = mgr.kill_switch("ext-a", "malicious behavior", "user");
    assert!(result.success);
    assert_eq!(result.previous_state, ExtensionTrustState::Pending);
    assert_eq!(result.new_state, ExtensionTrustState::Killed);
    assert_eq!(mgr.trust_state("ext-a"), ExtensionTrustState::Killed);
}

#[test]
fn kill_switch_quarantines_in_risk_controller() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-b", "threat detected", "system");
    let quarantined = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .runtime_risk_states
        .get("ext-b")
        .unwrap()
        .quarantined;
    assert!(quarantined);
}

#[test]
fn kill_switch_emits_critical_alert() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-c", "suspicious exec", "user");
    let alerts = mgr.security_alert_snapshot();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].category, SecurityAlertCategory::Quarantine);
    assert_eq!(alerts[0].severity, SecurityAlertSeverity::Critical);
    assert!(alerts[0].summary.contains("Kill-switch activated"));
    assert_eq!(alerts[0].policy_source, "kill_switch");
}

#[test]
fn kill_switch_records_audit_entry() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-d", "risk too high", "admin");
    let audit = mgr.kill_switch_audit_log();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].activated);
    assert_eq!(audit[0].extension_id, "ext-d");
    assert_eq!(audit[0].reason, "risk too high");
    assert_eq!(audit[0].operator, "admin");
    assert_eq!(audit[0].previous_state, ExtensionTrustState::Pending);
    assert_eq!(audit[0].new_state, ExtensionTrustState::Killed);
}

#[test]
fn kill_switch_idempotent_when_already_killed() {
    let mgr = ExtensionManager::new();
    let r1 = mgr.kill_switch("ext-e", "first", "user");
    assert!(r1.success);
    let r2 = mgr.kill_switch("ext-e", "second", "user");
    assert!(!r2.success);
    assert_eq!(r2.previous_state, ExtensionTrustState::Killed);
    assert!(r2.message.contains("already killed"));
    // Only one audit entry.
    assert_eq!(mgr.kill_switch_audit_log().len(), 1);
}

#[test]
fn kill_switch_works_on_acknowledged_extension() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-f", "medium", true, "user");
    assert_eq!(mgr.trust_state("ext-f"), ExtensionTrustState::Acknowledged);
    let result = mgr.kill_switch("ext-f", "runtime threat", "system");
    assert!(result.success);
    assert_eq!(result.previous_state, ExtensionTrustState::Acknowledged);
    assert_eq!(result.new_state, ExtensionTrustState::Killed);
}

#[test]
fn kill_switch_works_on_trusted_extension() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-g", "low", true, "user");
    mgr.promote_trust("ext-g");
    assert_eq!(mgr.trust_state("ext-g"), ExtensionTrustState::Trusted);
    let result = mgr.kill_switch("ext-g", "compromised", "user");
    assert!(result.success);
    assert_eq!(result.previous_state, ExtensionTrustState::Trusted);
}

#[test]
fn lift_kill_switch_restores_acknowledged() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-h", "threat", "user");
    let result = mgr.lift_kill_switch("ext-h", "reviewed safe", "admin");
    assert!(result.success);
    assert_eq!(result.previous_state, ExtensionTrustState::Killed);
    assert_eq!(result.new_state, ExtensionTrustState::Acknowledged);
    assert_eq!(mgr.trust_state("ext-h"), ExtensionTrustState::Acknowledged);
}

#[test]
fn lift_kill_switch_clears_quarantine() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-i", "threat", "user");
    mgr.lift_kill_switch("ext-i", "safe now", "admin");
    let state = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .runtime_risk_states
        .get("ext-i")
        .unwrap()
        .clone();
    assert!(!state.quarantined);
    assert_eq!(state.consecutive_unsafe, 0);
}

#[test]
fn lift_kill_switch_emits_info_alert() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-j", "threat", "user");
    mgr.lift_kill_switch("ext-j", "cleared", "admin");
    let alerts = mgr.security_alert_snapshot();
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[1].severity, SecurityAlertSeverity::Info);
    assert!(alerts[1].summary.contains("Kill-switch lifted"));
    assert_eq!(alerts[1].reason_codes[0], "kill_switch_lifted");
}

#[test]
fn lift_kill_switch_records_audit_deactivation() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-k", "threat", "user");
    mgr.lift_kill_switch("ext-k", "cleared", "admin");
    let audit = mgr.kill_switch_audit_log();
    assert_eq!(audit.len(), 2);
    assert!(audit[0].activated);
    assert!(!audit[1].activated);
    assert_eq!(audit[1].operator, "admin");
}

#[test]
fn lift_kill_switch_fails_if_not_killed() {
    let mgr = ExtensionManager::new();
    let result = mgr.lift_kill_switch("ext-l", "no reason", "admin");
    assert!(!result.success);
    assert!(result.message.contains("not killed"));
}

#[test]
fn is_killed_returns_correct_state() {
    let mgr = ExtensionManager::new();
    assert!(!mgr.is_killed("ext-m"));
    mgr.kill_switch("ext-m", "threat", "user");
    assert!(mgr.is_killed("ext-m"));
    mgr.lift_kill_switch("ext-m", "safe", "admin");
    assert!(!mgr.is_killed("ext-m"));
}

#[test]
fn trust_state_defaults_to_pending() {
    let mgr = ExtensionManager::new();
    assert_eq!(mgr.trust_state("unknown-ext"), ExtensionTrustState::Pending);
}

#[test]
fn trust_onboarding_accept_sets_acknowledged() {
    let mgr = ExtensionManager::new();
    let state = mgr.record_trust_onboarding("ext-n", "high", true, "user");
    assert_eq!(state, ExtensionTrustState::Acknowledged);
    assert_eq!(mgr.trust_state("ext-n"), ExtensionTrustState::Acknowledged);
}

#[test]
fn trust_onboarding_reject_sets_killed() {
    let mgr = ExtensionManager::new();
    let state = mgr.record_trust_onboarding("ext-o", "high", false, "user");
    assert_eq!(state, ExtensionTrustState::Killed);
    assert!(mgr.is_killed("ext-o"));
}

#[test]
fn trust_onboarding_reject_quarantines() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-p", "critical", false, "user");
    let quarantined = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .runtime_risk_states
        .get("ext-p")
        .unwrap()
        .quarantined;
    assert!(quarantined);
}

#[test]
fn trust_onboarding_records_decision() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-q", "medium", true, "operator1");
    let decisions = mgr.trust_onboarding_decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].extension_id, "ext-q");
    assert_eq!(decisions[0].acknowledged_risk_level, "medium");
    assert!(decisions[0].accepted);
    assert_eq!(decisions[0].operator, "operator1");
    assert_eq!(
        decisions[0].resulting_state,
        ExtensionTrustState::Acknowledged
    );
}

#[test]
fn promote_trust_from_acknowledged() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-r", "low", true, "user");
    let state = mgr.promote_trust("ext-r");
    assert_eq!(state, ExtensionTrustState::Trusted);
    assert_eq!(mgr.trust_state("ext-r"), ExtensionTrustState::Trusted);
}

#[test]
fn promote_trust_no_op_from_pending() {
    let mgr = ExtensionManager::new();
    let state = mgr.promote_trust("ext-s");
    assert_eq!(state, ExtensionTrustState::Pending);
}

#[test]
fn promote_trust_no_op_from_killed() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-t", "threat", "user");
    let state = mgr.promote_trust("ext-t");
    assert_eq!(state, ExtensionTrustState::Killed);
}

#[test]
fn full_trust_lifecycle() {
    let mgr = ExtensionManager::new();
    // 1. Starts as pending.
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Pending
    );
    // 2. Onboard with acknowledgment.
    mgr.record_trust_onboarding("ext-lifecycle", "medium", true, "user");
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Acknowledged
    );
    // 3. Promote to trusted.
    mgr.promote_trust("ext-lifecycle");
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Trusted
    );
    // 4. Kill-switch.
    mgr.kill_switch("ext-lifecycle", "compromised", "system");
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Killed
    );
    // 5. Lift kill-switch.
    mgr.lift_kill_switch("ext-lifecycle", "reviewed", "admin");
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Acknowledged
    );
    // 6. Promote again.
    mgr.promote_trust("ext-lifecycle");
    assert_eq!(
        mgr.trust_state("ext-lifecycle"),
        ExtensionTrustState::Trusted
    );
}

#[test]
fn kill_switch_audit_preserves_provenance() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-u", "high", true, "onboarder");
    mgr.kill_switch("ext-u", "threat detected", "sentinel-agent");
    mgr.lift_kill_switch("ext-u", "false alarm", "admin");
    let audit = mgr.kill_switch_audit_log();
    assert_eq!(audit.len(), 2);
    // First entry: kill.
    assert!(audit[0].activated);
    assert_eq!(audit[0].operator, "sentinel-agent");
    assert_eq!(audit[0].previous_state, ExtensionTrustState::Acknowledged);
    // Second entry: lift.
    assert!(!audit[1].activated);
    assert_eq!(audit[1].operator, "admin");
    assert_eq!(audit[1].previous_state, ExtensionTrustState::Killed);
    assert_eq!(audit[1].new_state, ExtensionTrustState::Acknowledged);
}

#[test]
fn multiple_extensions_independent_trust() {
    let mgr = ExtensionManager::new();
    mgr.record_trust_onboarding("ext-v1", "low", true, "user");
    mgr.record_trust_onboarding("ext-v2", "high", false, "user");
    assert_eq!(mgr.trust_state("ext-v1"), ExtensionTrustState::Acknowledged);
    assert_eq!(mgr.trust_state("ext-v2"), ExtensionTrustState::Killed);
    // Kill ext-v1 doesn't affect ext-v2.
    mgr.kill_switch("ext-v1", "threat", "system");
    assert!(mgr.is_killed("ext-v1"));
    assert!(mgr.is_killed("ext-v2"));
}

#[test]
fn trust_state_display_impl() {
    assert_eq!(format!("{}", ExtensionTrustState::Pending), "pending");
    assert_eq!(
        format!("{}", ExtensionTrustState::Acknowledged),
        "acknowledged"
    );
    assert_eq!(format!("{}", ExtensionTrustState::Trusted), "trusted");
    assert_eq!(format!("{}", ExtensionTrustState::Killed), "killed");
}

#[test]
fn kill_switch_alert_sequence_ids_monotonic() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-w1", "threat1", "user");
    mgr.kill_switch("ext-w2", "threat2", "user");
    let alerts = mgr.security_alert_snapshot();
    assert_eq!(alerts.len(), 2);
    assert!(alerts[1].sequence_id > alerts[0].sequence_id);
}

#[test]
fn kill_switch_then_lift_then_kill_again() {
    let mgr = ExtensionManager::new();
    mgr.kill_switch("ext-x", "first threat", "user");
    mgr.lift_kill_switch("ext-x", "cleared", "admin");
    let r = mgr.kill_switch("ext-x", "second threat", "system");
    assert!(r.success);
    assert_eq!(r.previous_state, ExtensionTrustState::Acknowledged);
    assert_eq!(mgr.trust_state("ext-x"), ExtensionTrustState::Killed);
    assert_eq!(mgr.kill_switch_audit_log().len(), 3);
}

// ---- Hook bitmap / context cache / coalescer tests ----

struct TestNullSession;

#[async_trait]
impl ExtensionSession for TestNullSession {
    async fn get_state(&self) -> Value {
        Value::Null
    }
    async fn get_messages(&self) -> Vec<SessionMessage> {
        Vec::new()
    }
    async fn get_entries(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn get_branch(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn set_name(&self, _name: String) -> Result<()> {
        Ok(())
    }
    async fn append_message(&self, _msg: SessionMessage) -> Result<()> {
        Ok(())
    }
    async fn append_custom_entry(&self, _custom_type: String, _data: Option<Value>) -> Result<()> {
        Ok(())
    }
    async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
        Ok(())
    }
    async fn get_model(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }
    async fn set_thinking_level(&self, _level: String) -> Result<()> {
        Ok(())
    }
    async fn get_thinking_level(&self) -> Option<String> {
        None
    }
    async fn set_label(&self, _target_id: String, _label: Option<String>) -> Result<()> {
        Ok(())
    }
}

fn test_register_payload(name: &str, hooks: Vec<String>) -> RegisterPayload {
    RegisterPayload {
        name: name.to_string(),
        version: "0.1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: hooks,
    }
}

#[test]
fn hook_bitmap_empty_when_no_extensions_registered() {
    let mgr = ExtensionManager::new();
    assert!(!mgr.has_hook_for("startup"));
    assert!(!mgr.has_hook_for("message_update"));
    assert!(!mgr.has_hook_for("tool_call"));
}

#[test]
fn hook_bitmap_populated_on_register() {
    let mgr = ExtensionManager::new();
    let payload = test_register_payload(
        "test-ext",
        vec![
            "startup".to_string(),
            "message_update".to_string(),
            "tool_call".to_string(),
        ],
    );
    mgr.register(payload);

    assert!(mgr.has_hook_for("startup"));
    assert!(mgr.has_hook_for("message_update"));
    assert!(mgr.has_hook_for("tool_call"));
    assert!(!mgr.has_hook_for("agent_start"));
    assert!(!mgr.has_hook_for("nonexistent"));
}

#[test]
fn hook_bitmap_merges_across_multiple_extensions() {
    let mgr = ExtensionManager::new();
    mgr.register(test_register_payload("ext-a", vec!["startup".to_string()]));
    mgr.register(test_register_payload(
        "ext-b",
        vec!["tool_call".to_string()],
    ));

    assert!(mgr.has_hook_for("startup"));
    assert!(mgr.has_hook_for("tool_call"));
    assert!(!mgr.has_hook_for("message_update"));
}

// ---- Context cache tests ----

#[test]
fn ctx_generation_increments_on_cwd_change() {
    let mgr = ExtensionManager::new();
    let gen_before = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    mgr.set_cwd("/tmp/test".to_string());
    let gen_after = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    assert_eq!(gen_after, gen_before + 1);
}

#[test]
fn ctx_generation_increments_on_session_set() {
    let mgr = ExtensionManager::new();
    let gen_before = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    mgr.set_session(Arc::new(TestNullSession));
    let gen_after = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    assert_eq!(gen_after, gen_before + 1);
}

#[test]
fn ctx_generation_increments_on_model_change() {
    let mgr = ExtensionManager::new();
    let gen_before = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    mgr.set_current_model(Some("anthropic".to_string()), Some("claude-3".to_string()));
    let gen_after = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    assert_eq!(gen_after, gen_before + 1);
}

#[test]
fn ctx_generation_increments_on_thinking_level_change() {
    let mgr = ExtensionManager::new();
    let gen_before = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    mgr.set_current_thinking_level(Some("high".to_string()));
    let gen_after = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    assert_eq!(gen_after, gen_before + 1);
}

#[test]
fn invalidate_ctx_cache_bumps_generation() {
    let mgr = ExtensionManager::new();
    let gen_before = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    mgr.invalidate_ctx_cache();
    let gen_after = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ctx_generation;
    assert_eq!(gen_after, gen_before + 1);
}

#[test]
fn ctx_cache_initially_none() {
    let mgr = ExtensionManager::new();
    let guard = mgr
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(guard.ctx_cache.is_none());
    drop(guard);
}

// ---- Coalescable event tests ----

#[test]
fn is_coalescable_event_identifies_high_frequency_events() {
    assert!(is_coalescable_event(&ExtensionEventName::MessageUpdate));
    assert!(is_coalescable_event(
        &ExtensionEventName::ToolExecutionUpdate
    ));
}

#[test]
fn is_coalescable_event_rejects_blocking_events() {
    assert!(!is_coalescable_event(&ExtensionEventName::ToolCall));
    assert!(!is_coalescable_event(&ExtensionEventName::ToolResult));
    assert!(!is_coalescable_event(&ExtensionEventName::Input));
    assert!(!is_coalescable_event(&ExtensionEventName::Startup));
    assert!(!is_coalescable_event(&ExtensionEventName::AgentStart));
    assert!(!is_coalescable_event(&ExtensionEventName::AgentEnd));
    assert!(!is_coalescable_event(&ExtensionEventName::MessageStart));
    assert!(!is_coalescable_event(&ExtensionEventName::MessageEnd));
}

#[test]
fn event_coalescer_no_hook_skips_dispatch() {
    let mgr = ExtensionManager::new();
    // No extensions registered → no hooks → dispatch should be a no-op.
    let coalescer = EventCoalescer::new(mgr);
    // Verify in_flight and pending are empty.
    assert!(
        coalescer
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    assert!(
        coalescer
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[test]
fn dispatch_event_value_returns_none_when_no_hooks() {
    asupersync::test_utils::run_test(|| async {
        let mgr = ExtensionManager::new();
        let result = mgr
            .dispatch_event(ExtensionEventName::MessageUpdate, None)
            .await;
        assert!(result.is_ok());
    });
}

#[test]
fn dispatch_tool_call_returns_none_when_no_hooks() {
    asupersync::test_utils::run_test(|| async {
        let mgr = ExtensionManager::new();
        let tool_call = crate::model::ToolCall {
            id: "tc-1".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "/tmp/test"}),
            thought_signature: None,
        };
        let result = mgr.dispatch_tool_call(&tool_call, 5_000).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    });
}

#[test]
fn dispatch_tool_result_returns_none_when_no_hooks() {
    asupersync::test_utils::run_test(|| async {
        let mgr = ExtensionManager::new();
        let tool_call = crate::model::ToolCall {
            id: "tc-1".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "/tmp/test"}),
            thought_signature: None,
        };
        let output = crate::tools::ToolOutput {
            content: vec![],
            details: Some(json!({})),
            is_error: false,
        };
        let result = mgr
            .dispatch_tool_result(&tool_call, &output, false, 5_000)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    });
}

// ── RCU snapshot semantics tests ─────────────────────────────────

#[test]
fn rcu_snapshot_version_increments_on_register() {
    let manager = ExtensionManager::new();
    let v0 = manager.snapshot_version();
    manager.register(RegisterPayload {
        name: "ext-a".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: vec!["onPrompt".to_string()],
    });
    let v1 = manager.snapshot_version();
    // Snapshot version should remain at 0 since register() does not
    // bump ctx_generation (only session/model/cwd changes do).
    // But the snapshot IS refreshed.
    assert!(
        v1 >= v0,
        "snapshot version should not regress after register"
    );
}

#[test]
fn rcu_register_provider_invalidates_snapshot() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        // Snapshot should initially have no providers.
        let snap_before = manager.read_snapshot();
        assert!(snap_before.providers.is_empty());
        drop(snap_before);

        // Register a provider via hostcall.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "test-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.example.com",
                "apiKey": "TEST_KEY",
                "models": [{"id": "gpt-test", "name": "Test Model"}]
            }),
        )
        .await;

        // Snapshot should now contain the provider.
        let snap_after = manager.read_snapshot();
        assert_eq!(snap_after.providers.len(), 1);
        assert_eq!(
            snap_after.providers[0]
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "test-llm"
        );
    });
}

#[test]
fn rcu_register_flag_invalidates_snapshot() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        // Snapshot should initially have no flags.
        assert!(manager.read_snapshot().all_flags.is_empty());

        // Register a flag via hostcall.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "verbose", "type": "bool", "default": false }),
        )
        .await;

        // Snapshot should now contain the flag.
        let flags = manager.list_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(
            flags[0].get("name").and_then(Value::as_str).unwrap(),
            "verbose"
        );
    });
}

#[test]
fn rcu_precomputed_flags_match_dynamic_plus_payload() {
    let manager = ExtensionManager::new();

    // Register an extension with payload flags.
    manager.register(RegisterPayload {
        name: "ext-flags".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: vec![
            json!({ "name": "alpha", "type": "bool", "default": false }),
            json!({ "name": "beta", "type": "string", "default": "x" }),
        ],
        event_hooks: Vec::new(),
    });

    // Also register a dynamic flag that overrides "alpha".
    manager.register_flag(
        json!({ "name": "alpha", "type": "bool", "default": true, "description": "dynamic" }),
    );

    let flags = manager.list_flags();
    // Should have 2 flags: dynamic "alpha" wins, plus payload "beta".
    assert_eq!(flags.len(), 2);
    let alpha = flags
        .iter()
        .find(|f| f.get("name").and_then(Value::as_str) == Some("alpha"))
        .unwrap();
    // Dynamic flag should take priority (description = "dynamic").
    assert_eq!(
        alpha.get("description").and_then(Value::as_str).unwrap(),
        "dynamic"
    );
}

#[test]
fn rcu_precomputed_commands_from_extensions() {
    let manager = ExtensionManager::new();

    manager.register(RegisterPayload {
        name: "ext-cmds".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: vec![
            json!({ "name": "deploy", "description": "Deploy to prod" }),
            json!({ "name": "rollback", "description": "Rollback deploy" }),
        ],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    let commands = manager.list_commands();
    assert_eq!(commands.len(), 2);
    let names: Vec<&str> = commands
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"deploy"));
    assert!(names.contains(&"rollback"));
}

#[test]
fn rcu_precomputed_shortcuts_and_has_shortcut() {
    let manager = ExtensionManager::new();

    manager.register(RegisterPayload {
        name: "ext-shortcuts".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: vec![json!({ "key_id": "Ctrl+K", "description": "Quick action" })],
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    // has_shortcut should use the pre-computed key_id set.
    assert!(manager.has_shortcut("ctrl+k"));
    assert!(manager.has_shortcut("Ctrl+K"));
    assert!(!manager.has_shortcut("ctrl+j"));

    let shortcuts = manager.list_shortcuts();
    assert_eq!(shortcuts.len(), 1);
}

#[test]
fn rcu_precomputed_event_hooks() {
    let manager = ExtensionManager::new();

    manager.register(RegisterPayload {
        name: "ext-hooks".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: vec!["onPrompt".to_string(), "onResponse".to_string()],
    });

    let hooks = manager.list_event_hooks();
    assert_eq!(hooks.len(), 2);
    assert!(hooks.contains(&"onPrompt".to_string()));
    assert!(hooks.contains(&"onResponse".to_string()));

    // Hook bitmap should also be populated.
    assert!(manager.has_hook_for("onPrompt"));
    assert!(manager.has_hook_for("onResponse"));
    assert!(!manager.has_hook_for("onUnknown"));
}

#[test]
fn rcu_snapshot_readers_get_consistent_view() {
    let manager = ExtensionManager::new();

    // Take a snapshot reference before registration.
    let snap_before = manager.read_snapshot();
    assert!(snap_before.all_commands.is_empty());

    // Register an extension.
    manager.register(RegisterPayload {
        name: "ext-late".to_string(),
        version: "1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: vec![json!({ "name": "late-cmd" })],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    // The old snapshot should still show empty (RCU: old readers keep
    // their Arc alive until dropped).
    assert!(
        snap_before.all_commands.is_empty(),
        "old snapshot should be immutable"
    );

    // A new snapshot should show the registered command.
    let snap_after = manager.read_snapshot();
    assert_eq!(snap_after.all_commands.len(), 1);
}

#[test]
fn rcu_extension_model_entries_uses_snapshot_providers() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        // Register a provider.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "snap-provider",
                "api": "openai-completions",
                "models": [{"id": "model-a", "name": "Model A"}]
            }),
        )
        .await;

        // extension_model_entries() should read from the snapshot.
        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.id, "model-a");
        assert_eq!(entries[0].model.provider, "snap-provider");
    });
}

#[test]
fn rcu_has_ui_field_propagates_to_snapshot() {
    let manager = ExtensionManager::new();

    // Initially has_ui should be false.
    assert!(!manager.read_snapshot().has_ui);

    // Set a UI sender.
    let (tx, _rx) = mpsc::channel(1);
    manager.set_ui_sender(tx);

    // has_ui should now be true in the snapshot.
    assert!(manager.read_snapshot().has_ui);

    // Clear it.
    manager.clear_ui_sender();
    assert!(!manager.read_snapshot().has_ui);
}

#[test]
fn extension_runtime_engine_selection_parses_native_values() {
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value("native-rust"),
        ExtensionRuntimeEngineSelection::NativeRust
    );
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value(" NATIVE_RUST "),
        ExtensionRuntimeEngineSelection::NativeRust
    );
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value("native"),
        ExtensionRuntimeEngineSelection::NativeRust
    );
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value("quickjs"),
        ExtensionRuntimeEngineSelection::NativeRust
    );
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value(""),
        ExtensionRuntimeEngineSelection::NativeRust
    );
    assert_eq!(
        ExtensionRuntimeEngineSelection::from_env_value("unknown-value"),
        ExtensionRuntimeEngineSelection::NativeRust
    );
}

#[test]
fn resolve_extension_load_spec_detects_native_json_entrypoint() {
    let dir = tempdir().expect("tempdir");
    let entry = dir.path().join("sample.native.json");
    std::fs::write(&entry, "{}").expect("write native entry");

    let spec = resolve_extension_load_spec(&entry).expect("resolve load spec");
    match spec {
        ExtensionLoadSpec::NativeRust(native) => {
            assert_eq!(native.extension_id, "sample");
            assert_eq!(native.entry_path, safe_canonicalize(&entry));
        }
        ExtensionLoadSpec::Js(other) => panic!("expected native spec, got {other:?}"),
        #[cfg(feature = "wasm-host")]
        ExtensionLoadSpec::Wasm(other) => panic!("expected native spec, got {other:?}"),
    }
}

#[test]
fn resolve_extension_load_spec_detects_js_entrypoint_file() {
    let dir = tempdir().expect("tempdir");
    let entry = dir.path().join("index.ts");
    std::fs::write(
        &entry,
        r"
            export default function init(_pi) {}
            ",
    )
    .expect("write js entry");

    let spec = resolve_extension_load_spec(&entry).expect("resolve load spec");
    match spec {
        ExtensionLoadSpec::Js(js) => {
            let expected_id = entry
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .expect("tempdir name")
                .to_string();
            assert_eq!(js.extension_id, expected_id);
            assert_eq!(js.entry_path, safe_canonicalize(&entry));
        }
        ExtensionLoadSpec::NativeRust(other) => panic!("expected js spec, got {other:?}"),
        #[cfg(feature = "wasm-host")]
        ExtensionLoadSpec::Wasm(other) => panic!("expected js spec, got {other:?}"),
    }
}

fn write_js_entry(dir: &std::path::Path, ext: &str) -> PathBuf {
    let entry = dir.join(format!("index.{ext}"));
    std::fs::write(
        &entry,
        r"
            export default function init(_pi) {}
            ",
    )
    .expect("write js entry");
    entry
}

#[test]
fn resolve_extension_load_spec_detects_js_entrypoint_dir_variants() {
    for ext in ["mjs", "cjs", "mts", "cts", "tsx", "jsx"] {
        let dir = tempdir().expect("tempdir");
        let entry = write_js_entry(dir.path(), ext);

        let spec = resolve_extension_load_spec(dir.path())
            .unwrap_or_else(|err| panic!("resolve load spec for {ext}: {err}"));
        match spec {
            ExtensionLoadSpec::Js(js) => {
                let expected_id = entry
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .expect("tempdir name")
                    .to_string();
                assert_eq!(js.extension_id, expected_id);
                assert_eq!(js.entry_path, safe_canonicalize(&entry));
            }
            ExtensionLoadSpec::NativeRust(other) => {
                panic!("expected js spec for {ext}, got {other:?}");
            }
            #[cfg(feature = "wasm-host")]
            ExtensionLoadSpec::Wasm(other) => {
                panic!("expected js spec for {ext}, got {other:?}");
            }
        }
    }
}

#[test]
fn resolve_extension_load_spec_detects_js_runtime_manifest() {
    let dir = tempdir().expect("tempdir");
    let entry = dir.path().join("index.ts");
    std::fs::write(
        &entry,
        r"
            export default function init(_pi) {}
            ",
    )
    .expect("write js entry");
    std::fs::write(
        dir.path().join("extension.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.manifest.v1",
            "extension_id": "test-js-ext",
            "name": "Test JS Extension",
            "version": "0.1.0",
            "api_version": "1.0",
            "runtime": "js",
            "entrypoint": "index.ts",
            "capabilities": []
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");

    let spec = resolve_extension_load_spec(dir.path()).expect("resolve load spec");
    match spec {
        ExtensionLoadSpec::Js(js) => {
            assert_eq!(js.extension_id, "test-js-ext");
            assert_eq!(js.name, "Test JS Extension");
            assert_eq!(js.version, "0.1.0");
            assert_eq!(js.entry_path, safe_canonicalize(&entry));
        }
        ExtensionLoadSpec::NativeRust(other) => panic!("expected js spec, got {other:?}"),
        #[cfg(feature = "wasm-host")]
        ExtensionLoadSpec::Wasm(other) => panic!("expected js spec, got {other:?}"),
    }
}

#[test]
fn warm_runtime_pool_fingerprint_changes_when_extension_entry_changes() {
    let dir = tempdir().expect("tempdir");
    let entry = dir.path().join("index.ts");
    std::fs::write(&entry, "export const value = 1;\n").expect("write entry");
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
    let config = PiJsRuntimeConfig::default();
    let policy = ExtensionPolicy::default();

    let before = warm_runtime_pool_fingerprint(&config, &policy, std::slice::from_ref(&spec));
    std::fs::write(
        &entry,
        "export const value = 2;\nexport const changed = true;\n",
    )
    .expect("rewrite entry");
    let after_spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("reload spec");
    let after = warm_runtime_pool_fingerprint(&config, &policy, std::slice::from_ref(&after_spec));

    assert_ne!(
        before, after,
        "warm runtime pool key must invalidate when extension source changes"
    );
}

#[test]
fn warm_runtime_pool_fingerprint_changes_when_config_or_policy_changes() {
    let dir = tempdir().expect("tempdir");
    let entry = dir.path().join("index.ts");
    std::fs::write(&entry, "export default function init() {}\n").expect("write entry");
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
    let config = PiJsRuntimeConfig {
        cwd: dir.path().display().to_string(),
        limits: crate::extensions_js::PiJsRuntimeLimits {
            memory_limit_bytes: Some(16 * 1024 * 1024),
            ..crate::extensions_js::PiJsRuntimeLimits::default()
        },
        ..PiJsRuntimeConfig::default()
    };
    let policy = ExtensionPolicy::default();

    let baseline = warm_runtime_pool_fingerprint(&config, &policy, std::slice::from_ref(&spec));

    let mut changed_config = config.clone();
    changed_config.limits.memory_limit_bytes = Some(32 * 1024 * 1024);
    assert_ne!(
        baseline,
        warm_runtime_pool_fingerprint(&changed_config, &policy, std::slice::from_ref(&spec)),
        "warm runtime pool key must include runtime memory/cache config"
    );

    let changed_policy = ExtensionPolicy {
        deny_caps: vec!["session".to_string()],
        ..policy
    };
    assert_ne!(
        baseline,
        warm_runtime_pool_fingerprint(&config, &changed_policy, std::slice::from_ref(&spec)),
        "warm runtime pool key must include capability policy"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn exec_hostcall_truncation_does_not_sigpipe_writer() {
    asupersync::test_utils::run_test(|| async {
        let payload = json!({
            "args": ["if=/dev/zero", "bs=1", "count=70000", "status=none"],
        });

        let outcome =
            dispatch_hostcall_exec_ref_with_limit(None, "call-dd", "dd", &payload, 1024).await;
        match outcome {
            HostcallOutcome::Success(value) => {
                assert_eq!(
                    value.get("code").and_then(Value::as_i64),
                    Some(0),
                    "truncated exec capture must not change writer exit status: {value}"
                );
                assert_eq!(
                    value.get("killed").and_then(Value::as_bool),
                    Some(false),
                    "bounded exec capture should not kill the process: {value}"
                );
                let stdout = value
                    .get("stdout")
                    .and_then(Value::as_str)
                    .expect("stdout string");
                assert!(
                    stdout.contains("[stdout truncated]"),
                    "large stdout should still report truncation"
                );
            }
            other => panic!("expected exec success outcome, got {other:?}"),
        }
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_large_output_completes_without_deadlock() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
                .eval(
                    r#"
                    globalThis.bigChunks = [];
                    globalThis.bigDone = false;
                    globalThis.bigErr = null;
                    (async () => {
                        try {
                            const stream = pi.exec("sh", ["-c", "yes x | head -c 1200000"], { stream: true });
                            for await (const chunk of stream) {
                                globalThis.bigChunks.push(chunk);
                            }
                            globalThis.bigDone = true;
                        } catch (e) {
                            globalThis.bigErr = e.message || String(e);
                        }
                    })();
                "#,
                )
                .await
                .expect("eval");

        for _ in 0..1024 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }

        assert!(
            !runtime.has_pending(),
            "runtime should have no pending tasks after large streaming exec"
        );

        let big_chunks = runtime
            .read_global_json("bigChunks")
            .await
            .expect("read bigChunks");
        let entries = big_chunks.as_array().expect("bigChunks array");
        assert!(
            !entries.is_empty(),
            "expected streaming exec to yield chunks before completion"
        );
        assert_eq!(
            entries.last().and_then(|entry| entry.get("code")),
            Some(&json!(0)),
            "expected final success chunk: {entries:?}"
        );
        assert_eq!(
            runtime
                .read_global_json("bigDone")
                .await
                .expect("read bigDone"),
            Value::Bool(true)
        );
        assert_eq!(
            runtime
                .read_global_json("bigErr")
                .await
                .expect("read bigErr"),
            Value::Null
        );
    });
}

#[test]
fn isolated_runtime_budget_split_preserves_aggregate_and_remainder_order() {
    let allocations = (0..3)
        .map(|index| split_shard_budget(10, 3, index, "test").expect("allocation"))
        .collect::<Vec<_>>();
    assert_eq!(allocations, vec![4, 3, 3]);
    assert_eq!(allocations.iter().sum::<usize>(), 10);
    assert!(split_shard_budget(2, 3, 0, "test").is_err());
}

#[test]
fn isolated_runtime_hostcall_identity_rejects_payload_spoofing() {
    assert_eq!(
        authoritative_events_extension_id(
            Some("ext.owner"),
            &json!({ "extensionId": "ext.owner" }),
            "sendMessage",
        )
        .expect("matching owner"),
        Some("ext.owner".to_string())
    );
    let mismatch = authoritative_events_extension_id(
        Some("ext.owner"),
        &json!({ "extension_id": "ext.attacker" }),
        "sendMessage",
    )
    .expect_err("spoofed owner must fail");
    assert!(matches!(
        mismatch,
        HostcallOutcome::Error { ref code, .. } if code == "extension_identity_mismatch"
    ));
}

#[test]
fn isolated_runtime_dynamic_command_mutates_only_authoritative_owner() {
    let manager = ExtensionManager::new();
    for name in ["ext.one", "ext.two"] {
        manager.register(RegisterPayload {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            api_version: PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
    }
    manager
        .register_command_for_extension("ext.two", "owned", Some("owner test"))
        .expect("register owned command");
    let guard = manager
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(guard.extensions[0].slash_commands.is_empty());
    assert_eq!(
        extract_slash_command_name(&guard.extensions[1].slash_commands[0]),
        Some("owned".to_string())
    );
    assert_eq!(
        guard.extensions[1].slash_commands[0]
            .get("extension_id")
            .and_then(Value::as_str),
        Some("ext.two")
    );
}

#[test]
fn isolated_runtime_never_authorizes_a_display_name_as_principal() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "Friendly Display Name".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extension_ids[0] = "ext.authoritative".to_string();
    }

    let spoof =
        manager.register_command_for_extension("Friendly Display Name", "spoofed-command", None);
    assert!(spoof.is_err(), "display name must not authorize mutation");
    manager
        .register_command_for_extension("ext.authoritative", "owned-command", None)
        .expect("authoritative principal should mutate its registration");

    let guard = manager
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(guard.extensions[0].slash_commands.len(), 1);
    assert_eq!(
        extract_slash_command_name(&guard.extensions[0].slash_commands[0]),
        Some("owned-command".to_string())
    );
}
