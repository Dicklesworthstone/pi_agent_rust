//! Integration tests for SEC-4.4: Policy profile hardening and explicit
//! dangerous-capability opt-in semantics.
//!
//! Exercises the public API surface for profile semantics, dangerous
//! capability opt-in auditing, effective policy explanation, and profile
//! transition validation.

use pi::extensions::{
    DangerousOptInAuditEntry, ExtensionOverride, ExtensionPolicy, ExtensionPolicyMode,
    PolicyDecision, PolicyExplanation, PolicyProfile, ProfileTransitionCheck,
};

// ==========================================================================
// Profile semantics matrix
// ==========================================================================

#[test]
fn safe_profile_denies_dangerous_by_default() {
    let policy = PolicyProfile::Safe.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Strict);

    let exec_check = policy.evaluate("exec");
    assert_eq!(exec_check.decision, PolicyDecision::Deny);
    assert_eq!(exec_check.reason, "deny_caps");

    let env_check = policy.evaluate("env");
    assert_eq!(env_check.decision, PolicyDecision::Deny);
    assert_eq!(env_check.reason, "deny_caps");
}

#[test]
fn safe_profile_allows_non_dangerous_capabilities() {
    let policy = PolicyProfile::Safe.to_policy();
    for cap in ["read", "write", "http", "events", "session"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in safe profile"
        );
    }
}

#[test]
fn standard_profile_denies_dangerous_via_deny_caps() {
    let policy = PolicyProfile::Standard.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Prompt);

    // Even in prompt mode, deny_caps takes precedence
    let exec_check = policy.evaluate("exec");
    assert_eq!(exec_check.decision, PolicyDecision::Deny);
    assert_eq!(exec_check.reason, "deny_caps");
}

#[test]
fn permissive_profile_allows_everything() {
    let policy = PolicyProfile::Permissive.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Permissive);
    assert_eq!(policy.deny_caps, [] as [std::string::String; 0]);

    let exec_check = policy.evaluate("exec");
    assert_eq!(exec_check.decision, PolicyDecision::Allow);

    let env_check = policy.evaluate("env");
    assert_eq!(env_check.decision, PolicyDecision::Allow);
}

#[test]
fn profile_serde_roundtrip() {
    for profile in [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let json = serde_json::to_string(&profile).expect("serialize");
        let restored: PolicyProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, restored);
    }
}

#[test]
fn from_profile_equivalent_to_to_policy() {
    for profile in [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let via_to = profile.to_policy();
        let via_from = ExtensionPolicy::from_profile(profile);
        assert_eq!(via_to.mode, via_from.mode);
        assert_eq!(via_to.deny_caps, via_from.deny_caps);
        assert_eq!(via_to.default_caps, via_from.default_caps);
    }
}

// ==========================================================================
// Dangerous capabilities cannot be enabled implicitly
// ==========================================================================

#[test]
fn deny_caps_cannot_be_overridden_by_per_extension_allow() {
    let mut policy = PolicyProfile::Safe.to_policy();
    policy.per_extension.insert(
        "sneaky-ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string(), "env".to_string()],
            deny: Vec::new(),
            mode: None,
            quota: None,
        },
    );

    // Even with per-extension allow, global deny_caps takes precedence (layer 2 > layer 3)
    let exec_check = policy.evaluate_for("exec", Some("sneaky-ext"));
    assert_eq!(
        exec_check.decision,
        PolicyDecision::Deny,
        "deny_caps must override per-extension allow"
    );
    assert_eq!(exec_check.reason, "deny_caps");

    let env_check = policy.evaluate_for("env", Some("sneaky-ext"));
    assert_eq!(env_check.decision, PolicyDecision::Deny);
}

#[test]
fn allow_dangerous_removes_from_deny_caps() {
    let mut policy = PolicyProfile::Safe.to_policy();
    // Simulate allow_dangerous by removing exec/env from deny_caps
    policy.deny_caps.retain(|c| c != "exec" && c != "env");

    let exec_check = policy.evaluate("exec");
    // In strict mode without deny, unknown caps fall to mode fallback = Deny
    // But if exec is not in default_caps either, it goes to layer 5 (strict → deny)
    // So we need to add exec to default_caps for allow_dangerous to work
    assert_eq!(exec_check.decision, PolicyDecision::Deny); // Still denied by strict mode fallback
}

#[test]
fn allow_dangerous_with_standard_mode_enables_prompt() {
    let mut policy = PolicyProfile::Standard.to_policy();
    // Remove dangerous caps from deny list
    policy.deny_caps.retain(|c| c != "exec" && c != "env");

    // Now exec falls through to mode fallback (Prompt)
    let exec_check = policy.evaluate("exec");
    assert_eq!(
        exec_check.decision,
        PolicyDecision::Prompt,
        "With allow_dangerous in standard mode, dangerous caps should prompt"
    );
}

#[test]
fn dangerous_capabilities_identified_correctly() {
    use pi::extensions::Capability;

    assert!(Capability::Exec.is_dangerous());
    assert!(Capability::Env.is_dangerous());
    assert!(!Capability::Read.is_dangerous());
    assert!(!Capability::Write.is_dangerous());
    assert!(!Capability::Http.is_dangerous());
    assert!(!Capability::Session.is_dangerous());

    let dangerous = Capability::dangerous_list();
    assert_eq!(dangerous.len(), 2);
    assert!(dangerous.contains(&Capability::Exec));
    assert!(dangerous.contains(&Capability::Env));
}

// ==========================================================================
// Effective policy explanation
// ==========================================================================

#[test]
fn explain_safe_profile_shows_dangerous_denied() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);

    assert_eq!(explanation.mode, ExtensionPolicyMode::Strict);
    assert!(explanation.exec_mediation_enabled);
    assert!(explanation.secret_broker_enabled);
    assert!(explanation.dangerous_denied.contains(&"exec".to_string()));
    assert!(explanation.dangerous_denied.contains(&"env".to_string()));
    assert_eq!(
        explanation.dangerous_allowed,
        [] as [std::string::String; 0]
    );
    assert!(explanation.extension_id.is_none());
}

#[test]
fn explain_permissive_profile_shows_dangerous_allowed() {
    let policy = PolicyProfile::Permissive.to_policy();
    let explanation = policy.explain_effective_policy(None);

    assert_eq!(explanation.mode, ExtensionPolicyMode::Permissive);
    assert!(explanation.dangerous_allowed.contains(&"exec".to_string()));
    assert!(explanation.dangerous_allowed.contains(&"env".to_string()));
    assert_eq!(explanation.dangerous_denied, [] as [std::string::String; 0]);
}

#[test]
fn explain_includes_all_capabilities() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);

    // Should have decisions for all known capabilities
    let caps: Vec<_> = explanation
        .capability_decisions
        .iter()
        .map(|c| c.capability.as_str())
        .collect();
    for expected in ["read", "write", "http", "exec", "env", "events", "session"] {
        assert!(
            caps.contains(&expected),
            "Missing capability in explanation: {expected}"
        );
    }
}

#[test]
fn explain_with_extension_override() {
    let mut policy = PolicyProfile::Safe.to_policy();
    policy.per_extension.insert(
        "test-ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            deny: Vec::new(),
            mode: None,
            quota: None,
        },
    );

    // Without extension context
    let global = policy.explain_effective_policy(None);
    assert!(global.dangerous_denied.contains(&"exec".to_string()));
    assert!(global.extension_id.is_none());

    // With extension context — exec is STILL denied because deny_caps (layer 2)
    // beats per-extension allow (layer 3)
    let ext = policy.explain_effective_policy(Some("test-ext"));
    assert!(ext.dangerous_denied.contains(&"exec".to_string()));
    assert_eq!(ext.extension_id.as_deref(), Some("test-ext"));
}

#[test]
fn explain_serializes_to_json() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);

    let json = serde_json::to_string_pretty(&explanation).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(parsed["mode"], "strict");
    assert!(parsed["exec_mediation_enabled"].as_bool().unwrap());
    assert!(parsed["secret_broker_enabled"].as_bool().unwrap());
    assert!(parsed["dangerous_denied"].is_array());
    assert!(parsed["capability_decisions"].is_array());
}

#[test]
fn explain_roundtrip() {
    let policy = PolicyProfile::Standard.to_policy();
    let explanation = policy.explain_effective_policy(None);

    let json = serde_json::to_string(&explanation).expect("serialize");
    let restored: PolicyExplanation = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.mode, explanation.mode);
    assert_eq!(restored.dangerous_denied, explanation.dangerous_denied);
    assert_eq!(restored.dangerous_allowed, explanation.dangerous_allowed);
}

// ==========================================================================
// Profile transition validation (downgrade checks)
// ==========================================================================

#[test]
fn downgrade_permissive_to_safe_is_valid() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);

    assert!(check.is_valid_downgrade);
    assert_eq!(check.exec_before, PolicyDecision::Allow);
    assert_eq!(check.exec_after, PolicyDecision::Deny);
    assert_eq!(check.env_before, PolicyDecision::Allow);
    assert_eq!(check.env_after, PolicyDecision::Deny);
    assert_eq!(check.mode_before, ExtensionPolicyMode::Permissive);
    assert_eq!(check.mode_after, ExtensionPolicyMode::Strict);
}

#[test]
fn downgrade_permissive_to_standard_is_valid() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Standard.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(check.is_valid_downgrade);
}

#[test]
fn downgrade_standard_to_safe_is_valid() {
    let from = PolicyProfile::Standard.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(check.is_valid_downgrade);
}

#[test]
fn upgrade_safe_to_permissive_is_not_valid_downgrade() {
    let from = PolicyProfile::Safe.to_policy();
    let to = PolicyProfile::Permissive.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(!check.is_valid_downgrade);
}

#[test]
fn upgrade_safe_to_standard_is_not_valid_downgrade() {
    let from = PolicyProfile::Safe.to_policy();
    let to = PolicyProfile::Standard.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);
    assert!(!check.is_valid_downgrade);
}

#[test]
fn identity_transition_is_valid() {
    for profile in [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let policy = profile.to_policy();
        let check = ExtensionPolicy::is_valid_downgrade(&policy, &policy);
        assert!(
            check.is_valid_downgrade,
            "Identity transition should be valid for {profile:?}"
        );
    }
}

#[test]
fn transition_check_serializes() {
    let from = PolicyProfile::Permissive.to_policy();
    let to = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&from, &to);

    let json = serde_json::to_string_pretty(&check).expect("serialize");
    let restored: ProfileTransitionCheck = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.is_valid_downgrade, check.is_valid_downgrade);
    assert_eq!(restored.exec_before, check.exec_before);
    assert_eq!(restored.exec_after, check.exec_after);
}

// ==========================================================================
// Dangerous opt-in audit trail
// ==========================================================================

#[test]
fn dangerous_opt_in_audit_entry_serializes() {
    let entry = DangerousOptInAuditEntry {
        source: "config".to_string(),
        profile: "safe".to_string(),
        capabilities_unblocked: vec!["exec".to_string(), "env".to_string()],
    };
    let json = serde_json::to_string_pretty(&entry).expect("serialize");
    let restored: DangerousOptInAuditEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.source, "config");
    assert_eq!(restored.profile, "safe");
    assert_eq!(restored.capabilities_unblocked.len(), 2);
}

#[test]
fn dangerous_opt_in_audit_entry_fields() {
    let entry = DangerousOptInAuditEntry {
        source: "env".to_string(),
        profile: "standard".to_string(),
        capabilities_unblocked: vec!["exec".to_string()],
    };
    assert_eq!(entry.source, "env");
    assert_eq!(entry.profile, "standard");
    assert!(entry.capabilities_unblocked.contains(&"exec".to_string()));
}

// ==========================================================================
// SEC-4.3 integration with profiles
// ==========================================================================

#[test]
fn safe_profile_uses_strict_exec_mediation() {
    let policy = PolicyProfile::Safe.to_policy();
    assert!(policy.exec_mediation.enabled);
    assert_eq!(
        policy.exec_mediation.deny_threshold,
        pi::extensions::ExecRiskTier::High
    );
    assert!(policy.exec_mediation.audit_all_classified);
}

#[test]
fn permissive_profile_uses_permissive_exec_mediation() {
    let policy = PolicyProfile::Permissive.to_policy();
    assert!(policy.exec_mediation.enabled);
    assert_eq!(
        policy.exec_mediation.deny_threshold,
        pi::extensions::ExecRiskTier::Critical
    );
    assert!(!policy.exec_mediation.audit_all_classified);
}

#[test]
fn all_profiles_enable_secret_broker() {
    for profile in [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let policy = profile.to_policy();
        assert!(
            policy.secret_broker.enabled,
            "Secret broker should be enabled for {profile:?}"
        );
    }
}

// ==========================================================================
// Per-extension override semantics
// ==========================================================================

#[test]
fn per_extension_deny_takes_highest_precedence() {
    let mut policy = PolicyProfile::Permissive.to_policy();
    policy.per_extension.insert(
        "ext-a".to_string(),
        ExtensionOverride {
            deny: vec!["http".to_string()],
            allow: Vec::new(),
            mode: None,
            quota: None,
        },
    );

    // Global: permissive allows http
    let global = policy.evaluate("http");
    assert_eq!(global.decision, PolicyDecision::Allow);

    // Extension-specific: deny overrides global
    let ext = policy.evaluate_for("http", Some("ext-a"));
    assert_eq!(ext.decision, PolicyDecision::Deny);
    assert_eq!(ext.reason, "extension_deny");
}

#[test]
fn per_extension_allow_grants_non_denied_caps() {
    let mut policy = PolicyProfile::Safe.to_policy();
    // In strict mode, "tool" is not in default_caps → denied by mode fallback
    // Per-extension allow should grant it
    policy.per_extension.insert(
        "ext-b".to_string(),
        ExtensionOverride {
            allow: vec!["tool".to_string()],
            deny: Vec::new(),
            mode: None,
            quota: None,
        },
    );

    let global = policy.evaluate("tool");
    assert_eq!(global.decision, PolicyDecision::Deny);

    let ext = policy.evaluate_for("tool", Some("ext-b"));
    assert_eq!(ext.decision, PolicyDecision::Allow);
    assert_eq!(ext.reason, "extension_allow");
}

#[test]
fn unknown_extension_falls_through_to_global() {
    let policy = PolicyProfile::Safe.to_policy();
    let check = policy.evaluate_for("read", Some("nonexistent-ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "default_caps");
}

#[test]
fn multiple_extensions_independent() {
    let mut policy = PolicyProfile::Safe.to_policy();
    policy.per_extension.insert(
        "ext-x".to_string(),
        ExtensionOverride {
            deny: vec!["http".to_string()],
            ..Default::default()
        },
    );
    policy.per_extension.insert(
        "ext-y".to_string(),
        ExtensionOverride {
            allow: vec!["tool".to_string()],
            ..Default::default()
        },
    );

    // ext-x: http denied
    assert_eq!(
        policy.evaluate_for("http", Some("ext-x")).decision,
        PolicyDecision::Deny
    );
    // ext-y: http allowed (from default_caps)
    assert_eq!(
        policy.evaluate_for("http", Some("ext-y")).decision,
        PolicyDecision::Allow
    );
    // ext-x: tool denied (strict mode, not in default_caps or extension allow)
    assert_eq!(
        policy.evaluate_for("tool", Some("ext-x")).decision,
        PolicyDecision::Deny
    );
    // ext-y: tool allowed (from extension allow)
    assert_eq!(
        policy.evaluate_for("tool", Some("ext-y")).decision,
        PolicyDecision::Allow
    );
}

// ==========================================================================
// Edge cases
// ==========================================================================

#[test]
fn evaluate_empty_capability_denied() {
    let policy = PolicyProfile::Safe.to_policy();
    let check = policy.evaluate("");
    assert_eq!(check.decision, PolicyDecision::Deny);
}

#[test]
fn policy_default_is_standard() {
    let default_policy = ExtensionPolicy::default();
    let standard = PolicyProfile::Standard.to_policy();
    assert_eq!(default_policy.mode, standard.mode);
    assert_eq!(default_policy.deny_caps, standard.deny_caps);
    assert_eq!(default_policy.default_caps, standard.default_caps);
}

#[test]
fn extension_policy_full_serde_roundtrip() {
    let mut policy = PolicyProfile::Safe.to_policy();
    policy.per_extension.insert(
        "test".to_string(),
        ExtensionOverride {
            allow: vec!["tool".to_string()],
            deny: vec!["ui".to_string()],
            mode: Some(ExtensionPolicyMode::Prompt),
            quota: None,
        },
    );

    let json = serde_json::to_string_pretty(&policy).expect("serialize");
    let restored: ExtensionPolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.mode, policy.mode);
    assert_eq!(restored.deny_caps, policy.deny_caps);
    assert!(restored.per_extension.contains_key("test"));
    let ovr = &restored.per_extension["test"];
    assert_eq!(ovr.allow, vec!["tool".to_string()]);
    assert_eq!(ovr.deny, vec!["ui".to_string()]);
    assert_eq!(ovr.mode, Some(ExtensionPolicyMode::Prompt));
}

#[test]
fn capability_explanation_has_is_dangerous_flag() {
    let policy = PolicyProfile::Safe.to_policy();
    let explanation = policy.explain_effective_policy(None);

    let exec_exp = explanation
        .capability_decisions
        .iter()
        .find(|c| c.capability == "exec")
        .expect("exec should be present");
    assert!(exec_exp.is_dangerous);

    let read_exp = explanation
        .capability_decisions
        .iter()
        .find(|c| c.capability == "read")
        .expect("read should be present");
    assert!(!read_exp.is_dangerous);
}
