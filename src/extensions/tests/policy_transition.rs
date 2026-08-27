//! Policy explanation and profile transition tests.

use super::*;

// ====================================================================
// SEC-4.4: Policy explanation and profile transition tests
// ====================================================================

#[test]
fn explain_effective_policy_safe_profile() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);

    assert_eq!(explanation.mode, ExtensionPolicyMode::Strict);
    assert!(explanation.exec_mediation_enabled);
    assert!(explanation.secret_broker_enabled);
    // Dangerous caps should be denied in safe profile.
    assert!(
        explanation.dangerous_denied.contains(&"exec".to_string()),
        "exec should be denied in safe profile"
    );
    assert!(
        explanation.dangerous_denied.contains(&"env".to_string()),
        "env should be denied in safe profile"
    );
    assert!(
        explanation.dangerous_allowed.is_empty(),
        "No dangerous caps should be allowed in safe profile"
    );
    assert!(explanation.extension_id.is_none());
}

#[test]
fn explain_effective_policy_permissive_profile() {
    let policy = PolicyProfile::Permissive.to_policy();
    let explanation = policy.explain_effective_policy(None);

    assert_eq!(explanation.mode, ExtensionPolicyMode::Permissive);
    // Permissive allows everything including dangerous caps.
    assert!(
        explanation.dangerous_allowed.contains(&"exec".to_string()),
        "exec should be allowed in permissive profile"
    );
    assert!(
        explanation.dangerous_allowed.contains(&"env".to_string()),
        "env should be allowed in permissive profile"
    );
    assert!(explanation.dangerous_denied.is_empty());
}

#[test]
fn explain_effective_policy_standard_profile() {
    let policy = PolicyProfile::Standard.to_policy();
    let explanation = policy.explain_effective_policy(None);

    assert_eq!(explanation.mode, ExtensionPolicyMode::Prompt);
    // Standard denies exec/env via deny_caps.
    assert!(explanation.dangerous_denied.contains(&"exec".to_string()));
    assert!(explanation.dangerous_denied.contains(&"env".to_string()));
}

#[test]
fn explain_effective_policy_with_extension_override() {
    let mut policy = PolicyProfile::Safe.to_policy();
    policy.per_extension.insert(
        "my-ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            deny: Vec::new(),
            mode: None,
            quota: None,
        },
    );
    // Without extension context: exec is denied.
    let explanation = policy.explain_effective_policy(None);
    assert!(explanation.dangerous_denied.contains(&"exec".to_string()));

    // With extension context: exec should still be denied because
    // deny_caps (layer 2) takes precedence over per-extension allow
    // (layer 3).
    let explanation = policy.explain_effective_policy(Some("my-ext"));
    assert!(explanation.dangerous_denied.contains(&"exec".to_string()));
    assert_eq!(explanation.extension_id.as_deref(), Some("my-ext"));
}

#[test]
fn explain_effective_policy_all_capabilities_present() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);
    // Every known capability must have a decision.
    assert_eq!(
        explanation.capability_decisions.len(),
        ALL_CAPABILITIES.len()
    );
    for cap in ALL_CAPABILITIES {
        assert!(
            explanation
                .capability_decisions
                .iter()
                .any(|c| c.capability == cap.as_str()),
            "Missing capability: {}",
            cap.as_str()
        );
    }
}

#[test]
fn explain_effective_policy_serializes_to_json() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);
    let json = serde_json::to_string(&explanation).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(parsed["mode"], "strict");
    assert!(parsed["exec_mediation_enabled"].as_bool().unwrap());
    assert!(parsed["dangerous_denied"].is_array());
}

// --- Profile transition checks ---

#[test]
fn downgrade_permissive_to_safe_is_valid() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        check.is_valid_downgrade,
        "Permissive → Safe should be a valid downgrade"
    );
    assert_eq!(check.exec_before, PolicyDecision::Allow);
    assert_eq!(check.exec_after, PolicyDecision::Deny);
    assert_eq!(check.env_before, PolicyDecision::Allow);
    assert_eq!(check.env_after, PolicyDecision::Deny);
}

#[test]
fn downgrade_permissive_to_standard_is_valid() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Standard.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        check.is_valid_downgrade,
        "Permissive → Standard should be a valid downgrade"
    );
}

#[test]
fn downgrade_standard_to_safe_is_valid() {
    let from = PolicyProfile::Standard.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        check.is_valid_downgrade,
        "Standard → Safe should be a valid downgrade"
    );
}

#[test]
fn upgrade_safe_to_permissive_is_not_downgrade() {
    let from = PolicyProfile::Safe.to_policy();
    let to = PolicyProfile::Permissive.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        !check.is_valid_downgrade,
        "Safe → Permissive should NOT be a valid downgrade"
    );
}

#[test]
fn upgrade_safe_to_standard_is_not_downgrade() {
    let from = PolicyProfile::Safe.to_policy();
    let to = PolicyProfile::Standard.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        !check.is_valid_downgrade,
        "Safe → Standard should NOT be a valid downgrade"
    );
}

#[test]
fn identity_transition_is_valid_downgrade() {
    // Same profile → same policy: weakly valid (nothing loosened).
    let from = PolicyProfile::Safe.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(
        check.is_valid_downgrade,
        "Same profile → same profile is a (trivial) valid downgrade"
    );
}

#[test]
fn downgrade_is_immediate_no_residual_dangerous_caps() {
    // Simulate: start with permissive, "downgrade" to safe, verify
    // that exec and env are immediately denied.
    let permissive = PolicyProfile::Permissive.to_policy();
    assert_eq!(permissive.evaluate("exec").decision, PolicyDecision::Allow);
    assert_eq!(permissive.evaluate("env").decision, PolicyDecision::Allow);

    let safe = PolicyProfile::Safe.to_policy();
    assert_eq!(safe.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(safe.evaluate("env").decision, PolicyDecision::Deny);

    // Transition check confirms it.
    let check = ExtensionPolicy::is_valid_downgrade(&permissive, &safe);
    assert!(check.is_valid_downgrade);
    assert_eq!(check.exec_after, PolicyDecision::Deny);
    assert_eq!(check.env_after, PolicyDecision::Deny);
}

// --- Dangerous opt-in cannot be implicit ---

#[test]
fn dangerous_caps_not_enabled_by_default() {
    let policy = ExtensionPolicy::default();
    assert_eq!(policy.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(policy.evaluate("env").decision, PolicyDecision::Deny);
}

#[test]
fn dangerous_caps_not_enabled_by_safe_profile() {
    let policy = PolicyProfile::Safe.to_policy();
    assert_eq!(policy.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(policy.evaluate("env").decision, PolicyDecision::Deny);
}

#[test]
fn dangerous_caps_not_enabled_by_standard_profile() {
    let policy = PolicyProfile::Standard.to_policy();
    assert_eq!(policy.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(policy.evaluate("env").decision, PolicyDecision::Deny);
}

#[test]
fn dangerous_caps_only_enabled_by_permissive_profile() {
    let safe = PolicyProfile::Safe.to_policy();
    let standard = PolicyProfile::Standard.to_policy();
    let permissive = PolicyProfile::Permissive.to_policy();

    // Only permissive allows dangerous caps.
    assert_eq!(safe.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(standard.evaluate("exec").decision, PolicyDecision::Deny);
    assert_eq!(permissive.evaluate("exec").decision, PolicyDecision::Allow);
}

#[test]
fn dangerous_opt_in_audit_entry_serializes() {
    let entry = DangerousOptInAuditEntry {
        source: "config".to_string(),
        profile: "safe".to_string(),
        capabilities_unblocked: vec!["exec".to_string(), "env".to_string()],
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(parsed["source"], "config");
    assert_eq!(parsed["profile"], "safe");
    assert_eq!(
        parsed["capabilities_unblocked"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn profile_transition_check_serializes() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    let json = serde_json::to_string(&check).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert!(parsed["is_valid_downgrade"].as_bool().unwrap());
}

#[test]
fn decision_strictness_ordering() {
    // Allow < Prompt < Deny
    assert!(
        decision_strictness(PolicyDecision::Allow) < decision_strictness(PolicyDecision::Prompt)
    );
    assert!(
        decision_strictness(PolicyDecision::Prompt) < decision_strictness(PolicyDecision::Deny)
    );
}

#[test]
fn mode_strictness_ordering() {
    // Permissive < Prompt < Strict
    assert!(
        mode_strictness(ExtensionPolicyMode::Permissive)
            < mode_strictness(ExtensionPolicyMode::Prompt)
    );
    assert!(
        mode_strictness(ExtensionPolicyMode::Prompt) < mode_strictness(ExtensionPolicyMode::Strict)
    );
}

#[test]
fn explain_policy_dangerous_flag_consistency() {
    // Verify that dangerous_allowed + dangerous_denied covers exactly
    // the dangerous capabilities.
    for profile in [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let policy = profile.to_policy();
        let explanation = policy.explain_effective_policy(None);
        let mut all_dangerous: Vec<String> = explanation
            .dangerous_allowed
            .iter()
            .chain(explanation.dangerous_denied.iter())
            .cloned()
            .collect();
        all_dangerous.sort();
        let mut expected: Vec<String> = Capability::dangerous_list()
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        expected.sort();
        assert_eq!(
            all_dangerous, expected,
            "Profile {profile:?} must cover all dangerous caps"
        );
    }
}
