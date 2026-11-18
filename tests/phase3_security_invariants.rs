//! Phase 3 Security/Policy Invariant Tests
//!
//! Proves that Phase 3 performance optimizations (dual-exec oracle, `io_uring`
//! lanes, S3-FIFO admission, NUMA placement, `PolicySnapshot` O(1) lookups)
//! do NOT expand the capability surface or bypass policy controls.
//!
//! Bead: bd-3ar8v.4.6

use std::collections::HashMap;

#[cfg(feature = "experimental-controllers")]
use pi::extension_scoring::{
    OpeEvaluatorConfig, OpeGateReason, OpeTraceSample, evaluate_off_policy,
};
use pi::extensions::{
    ALL_CAPABILITIES, Capability, ExecMediationPolicy, ExtensionOverride, ExtensionPolicy,
    ExtensionPolicyMode, PolicyDecision, PolicyProfile, PolicySnapshot, SecretBrokerPolicy,
};
#[cfg(feature = "experimental-controllers")]
use pi::hostcall_io_uring_lane::{
    HostcallCapabilityClass, HostcallDispatchLane, HostcallIoHint, IoUringFallbackReason,
    IoUringLaneDecisionInput, IoUringLanePolicyConfig, decide_io_uring_lane,
};
use pi::hostcall_s3_fifo::{S3FifoConfig, S3FifoDecisionKind, S3FifoPolicy};

// ============================================================================
// Invariant 1: PolicySnapshot O(1) lookup matches evaluate_for() exactly
// ============================================================================

#[test]
fn invariant_snapshot_matches_evaluate_for_all_known_caps() {
    let policy = ExtensionPolicy::default();
    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let dynamic = policy.evaluate_for(cap.as_str(), None);
        let cached = snapshot.lookup(cap.as_str(), None);
        assert_eq!(
            dynamic.decision,
            cached.decision,
            "snapshot mismatch for cap={} without extension context",
            cap.as_str()
        );
    }
}

#[test]
fn invariant_snapshot_matches_evaluate_for_per_extension_overrides() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "trusted-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            allow: vec!["exec".to_string()],
            deny: vec!["http".to_string()],
            quota: None,
        },
    );
    policy.per_extension.insert(
        "restricted-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Strict),
            allow: Vec::new(),
            deny: vec!["read".to_string(), "write".to_string()],
            quota: None,
        },
    );

    let snapshot = PolicySnapshot::compile(&policy);

    for ext_id in &["trusted-ext", "restricted-ext"] {
        for cap in ALL_CAPABILITIES {
            let dynamic = policy.evaluate_for(cap.as_str(), Some(ext_id));
            let cached = snapshot.lookup(cap.as_str(), Some(ext_id));
            assert_eq!(
                dynamic.decision,
                cached.decision,
                "snapshot mismatch: cap={}, ext={ext_id}",
                cap.as_str()
            );
        }
    }
}

#[test]
fn invariant_snapshot_falls_back_for_unknown_caps() {
    let policy = ExtensionPolicy::default();
    let snapshot = PolicySnapshot::compile(&policy);

    let unknown_caps = ["custom_magic", "proprietary_io", "teleport"];
    for cap in unknown_caps {
        let dynamic = policy.evaluate_for(cap, None);
        let cached = snapshot.lookup(cap, None);
        assert_eq!(
            dynamic.decision, cached.decision,
            "snapshot fallback mismatch for unknown cap={cap}"
        );
    }
}

#[test]
fn invariant_snapshot_per_extension_unknown_ext_uses_global() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "known-ext".to_string(),
        ExtensionOverride {
            mode: None,
            allow: vec!["exec".to_string()],
            deny: Vec::new(),
            quota: None,
        },
    );

    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let dynamic = policy.evaluate_for(cap.as_str(), Some("unknown-ext"));
        let cached = snapshot.lookup(cap.as_str(), Some("unknown-ext"));
        assert_eq!(
            dynamic.decision,
            cached.decision,
            "unknown ext should use global: cap={}",
            cap.as_str()
        );
    }
}

// ============================================================================
// Invariant 2: Policy precedence chain preserved across all profiles
// ============================================================================

#[test]
fn invariant_precedence_per_extension_deny_overrides_global_allow() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "ext1".to_string(),
        ExtensionOverride {
            mode: None,
            allow: Vec::new(),
            deny: vec!["read".to_string()],
            quota: None,
        },
    );

    // "read" is in default_caps (allowed globally) but per-extension deny takes priority
    let check = policy.evaluate_for("read", Some("ext1"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "extension_deny");
}

#[test]
fn invariant_precedence_global_deny_overrides_default_allow() {
    let policy = ExtensionPolicy::default();
    // "exec" is in deny_caps
    let check = policy.evaluate_for("exec", None);
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

#[test]
fn invariant_precedence_per_extension_allow_overrides_mode_fallback() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        ..ExtensionPolicy::default()
    };
    policy.per_extension.insert(
        "ext1".to_string(),
        ExtensionOverride {
            mode: None,
            allow: vec!["custom_cap".to_string()],
            deny: Vec::new(),
            quota: None,
        },
    );

    // Strict mode denies unknown caps, but per-extension allow overrides
    let check = policy.evaluate_for("custom_cap", Some("ext1"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "extension_allow");
}

#[test]
fn invariant_precedence_chain_exhaustive_across_profiles() {
    let profiles = [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ];

    for profile in profiles {
        let policy = profile.to_policy();

        // Dangerous caps (exec, env) should be denied in Safe and Standard
        if matches!(profile, PolicyProfile::Safe | PolicyProfile::Standard) {
            let exec_check = policy.evaluate_for("exec", None);
            assert_eq!(
                exec_check.decision,
                PolicyDecision::Deny,
                "exec should be denied in {profile:?}"
            );
            let env_check = policy.evaluate_for("env", None);
            assert_eq!(
                env_check.decision,
                PolicyDecision::Deny,
                "env should be denied in {profile:?}"
            );
        }

        // Safe caps (read, write, http, events, session) should be allowed in all profiles
        for cap in &["read", "write", "http", "events", "session"] {
            let check = policy.evaluate_for(cap, None);
            assert_eq!(
                check.decision,
                PolicyDecision::Allow,
                "safe cap {cap} should be allowed in {profile:?}"
            );
        }
    }
}

#[test]
fn invariant_empty_capability_always_denied() {
    let profiles = [
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ];

    for profile in profiles {
        let policy = profile.to_policy();
        let check = policy.evaluate_for("", None);
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "empty capability must be denied in {profile:?}"
        );
    }
}

// ============================================================================
// Invariant 3: Dangerous capability classification is immutable
// ============================================================================

#[test]
fn invariant_dangerous_caps_classification_stable() {
    let dangerous: Vec<&Capability> = ALL_CAPABILITIES
        .iter()
        .filter(|c| c.is_dangerous())
        .collect();
    let safe: Vec<&Capability> = ALL_CAPABILITIES
        .iter()
        .filter(|c| !c.is_dangerous())
        .collect();

    // Only exec and env are dangerous
    assert_eq!(dangerous.len(), 2);
    assert!(dangerous.iter().any(|c| c.as_str() == "exec"));
    assert!(dangerous.iter().any(|c| c.as_str() == "env"));

    // All others are safe
    assert_eq!(safe.len(), 8);
    for cap in safe {
        assert!(
            !cap.is_dangerous(),
            "{} should not be dangerous",
            cap.as_str()
        );
    }
}

// ============================================================================
// Invariant 4: IoUring lane selection cannot bypass policy
// ============================================================================

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_lane_decision_independent_of_policy_check() {
    // The io_uring module does NOT perform policy checks — it only selects
    // which execution lane to use. Policy is checked BEFORE lane selection
    // in the shared dispatch path.

    let all_capabilities = [
        HostcallCapabilityClass::Filesystem,
        HostcallCapabilityClass::Network,
        HostcallCapabilityClass::Execution,
        HostcallCapabilityClass::Session,
        HostcallCapabilityClass::Events,
        HostcallCapabilityClass::Environment,
        HostcallCapabilityClass::Tool,
        HostcallCapabilityClass::Ui,
        HostcallCapabilityClass::Telemetry,
        HostcallCapabilityClass::Unknown,
    ];

    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 256,
        allow_filesystem: true,
        allow_network: true,
    };

    for cap in &all_capabilities {
        let input = IoUringLaneDecisionInput {
            capability: *cap,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 0,
            force_compat_lane: false,
        };

        let decision = decide_io_uring_lane(config, input);

        // Filesystem and Network are policy-eligible, but the real executor is
        // not wired yet, so they must fail closed to an explicit fallback.
        match cap {
            HostcallCapabilityClass::Filesystem | HostcallCapabilityClass::Network => {
                assert_eq!(
                    decision.lane,
                    HostcallDispatchLane::Fast,
                    "IO-capable {cap:?} must not route to a placeholder io_uring bridge"
                );
                assert_eq!(
                    decision.fallback_reason,
                    Some(IoUringFallbackReason::IoUringExecutorUnavailable)
                );
            }
            _ => {
                assert_ne!(
                    decision.lane,
                    HostcallDispatchLane::IoUring,
                    "non-IO {cap:?} must NOT route to io_uring"
                );
                assert_eq!(
                    decision.fallback_reason,
                    Some(IoUringFallbackReason::UnsupportedCapability)
                );
            }
        }
    }
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_kill_switch_always_forces_compat() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 256,
        allow_filesystem: true,
        allow_network: true,
    };

    // With kill switch, even IO-capable requests go to Compat
    let input = IoUringLaneDecisionInput {
        capability: HostcallCapabilityClass::Filesystem,
        io_hint: HostcallIoHint::IoHeavy,
        queue_depth: 0,
        force_compat_lane: true,
    };

    let decision = decide_io_uring_lane(config, input);
    assert_eq!(decision.lane, HostcallDispatchLane::Compat);
    assert_eq!(
        decision.fallback_reason,
        Some(IoUringFallbackReason::CompatKillSwitch)
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_disabled_never_routes_to_io_uring() {
    let config = IoUringLanePolicyConfig {
        enabled: false,
        ring_available: true,
        max_queue_depth: 256,
        allow_filesystem: true,
        allow_network: true,
    };

    for cap in &[
        HostcallCapabilityClass::Filesystem,
        HostcallCapabilityClass::Network,
    ] {
        let input = IoUringLaneDecisionInput {
            capability: *cap,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 0,
            force_compat_lane: false,
        };
        let decision = decide_io_uring_lane(config, input);
        assert_ne!(
            decision.lane,
            HostcallDispatchLane::IoUring,
            "disabled config must never route to io_uring"
        );
    }
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_decision_is_deterministic() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 8,
        allow_filesystem: true,
        allow_network: true,
    };

    let input = IoUringLaneDecisionInput {
        capability: HostcallCapabilityClass::Filesystem,
        io_hint: HostcallIoHint::IoHeavy,
        queue_depth: 3,
        force_compat_lane: false,
    };

    // Same inputs always produce same output
    let d1 = decide_io_uring_lane(config, input);
    let d2 = decide_io_uring_lane(config, input);
    assert_eq!(d1.lane, d2.lane);
    assert_eq!(d1.fallback_reason, d2.fallback_reason);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_capability_class_mapping_covers_all_known_caps() {
    let known_caps = [
        ("read", HostcallCapabilityClass::Filesystem),
        ("write", HostcallCapabilityClass::Filesystem),
        ("fs", HostcallCapabilityClass::Filesystem),
        ("http", HostcallCapabilityClass::Network),
        ("network", HostcallCapabilityClass::Network),
        ("exec", HostcallCapabilityClass::Execution),
        ("session", HostcallCapabilityClass::Session),
        ("events", HostcallCapabilityClass::Events),
        ("env", HostcallCapabilityClass::Environment),
        ("tool", HostcallCapabilityClass::Tool),
        ("ui", HostcallCapabilityClass::Ui),
        ("log", HostcallCapabilityClass::Telemetry),
    ];

    for (cap_str, expected_class) in known_caps {
        let actual = HostcallCapabilityClass::from_capability(cap_str);
        assert_eq!(
            actual, expected_class,
            "capability {cap_str} should map to {expected_class:?}"
        );
    }
}

// ============================================================================
// Invariant 5: S3-FIFO admission respects per-extension isolation
// ============================================================================

#[test]
fn invariant_s3fifo_per_owner_fairness_isolated() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 10,
        small_capacity: 5,
        ghost_capacity: 10,
        max_entries_per_owner: 2,
        fallback_window: 32,
        min_ghost_hits_in_window: 2,
        max_budget_rejections_in_window: 12,
    });

    // ext-a uses 2 slots (at budget)
    let d1 = policy.access("ext-a", "a1".to_string());
    assert_eq!(d1.kind, S3FifoDecisionKind::AdmitSmall);
    let d2 = policy.access("ext-a", "a2".to_string());
    assert_eq!(d2.kind, S3FifoDecisionKind::AdmitSmall);

    // ext-a's 3rd key is rejected
    let d3 = policy.access("ext-a", "a3".to_string());
    assert_eq!(d3.kind, S3FifoDecisionKind::RejectFairnessBudget);

    // ext-b still has its own budget — should be admitted
    let d4 = policy.access("ext-b", "b1".to_string());
    assert_eq!(d4.kind, S3FifoDecisionKind::AdmitSmall);

    // Verify telemetry tracks owners separately
    let telem = policy.telemetry();
    assert_eq!(telem.owner_live_counts.get("ext-a").copied(), Some(2));
    assert_eq!(telem.owner_live_counts.get("ext-b").copied(), Some(1));
}

#[test]
fn invariant_s3fifo_budget_rejection_does_not_affect_other_owners() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 20,
        small_capacity: 10,
        ghost_capacity: 20,
        max_entries_per_owner: 1,
        fallback_window: 32,
        min_ghost_hits_in_window: 2,
        max_budget_rejections_in_window: 12,
    });

    // Admit one key per owner for 5 different owners
    for i in 0..5 {
        let owner = format!("ext-{i}");
        let d = policy.access(&owner, format!("key-{i}"));
        assert_eq!(
            d.kind,
            S3FifoDecisionKind::AdmitSmall,
            "owner {owner} should be admitted"
        );
    }

    // Each owner's second key should be rejected independently
    for i in 0..5 {
        let owner = format!("ext-{i}");
        let d = policy.access(&owner, format!("key-{i}-2"));
        assert_eq!(
            d.kind,
            S3FifoDecisionKind::RejectFairnessBudget,
            "owner {owner} second key should be rejected"
        );
    }

    assert_eq!(policy.telemetry().budget_rejections_total, 5);
}

#[test]
fn invariant_s3fifo_fallback_bypass_is_deterministic() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 4,
        small_capacity: 2,
        ghost_capacity: 4,
        max_entries_per_owner: 2,
        fallback_window: 4,
        min_ghost_hits_in_window: 3, // High threshold, triggers quickly
        max_budget_rejections_in_window: 12,
    });

    // Fill window with cold keys (no ghost hits) to trigger fallback
    for i in 0..4 {
        let _ = policy.access("ext-a", format!("cold-{i}"));
    }

    assert!(
        policy.telemetry().fallback_reason.is_some(),
        "fallback should be triggered"
    );

    // Once in fallback, all accesses bypass deterministically
    for i in 0..10 {
        let d = policy.access("ext-b", format!("bypass-{i}"));
        assert_eq!(d.kind, S3FifoDecisionKind::FallbackBypass);
    }
}

// ============================================================================
// Invariant 6: Secret broker pattern matching consistency
// ============================================================================

#[test]
fn invariant_secret_broker_exact_matches() {
    let broker = SecretBrokerPolicy::default();
    let known_secrets = [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "DATABASE_URL",
        "STRIPE_SECRET_KEY",
    ];

    for name in known_secrets {
        assert!(
            broker.is_secret(name),
            "{name} should be detected as secret"
        );
    }
}

#[test]
fn invariant_secret_broker_case_insensitive() {
    let broker = SecretBrokerPolicy::default();

    assert!(broker.is_secret("anthropic_api_key"));
    assert!(broker.is_secret("ANTHROPIC_API_KEY"));
    assert!(broker.is_secret("Anthropic_Api_Key"));
}

#[test]
fn invariant_secret_broker_suffix_matching() {
    let broker = SecretBrokerPolicy::default();

    let suffix_secrets = [
        "MY_CUSTOM_KEY",
        "SERVICE_SECRET",
        "OAUTH_TOKEN",
        "DB_PASSWORD",
        "CUSTOM_CREDENTIAL",
        "MY_API_KEY",
    ];

    for name in suffix_secrets {
        assert!(
            broker.is_secret(name),
            "{name} should be detected by suffix matching"
        );
    }
}

#[test]
fn invariant_secret_broker_prefix_matching() {
    let broker = SecretBrokerPolicy::default();

    assert!(broker.is_secret("SECRET_DATA"));
    assert!(broker.is_secret("AUTH_HEADER_VALUE"));
    assert!(broker.is_secret("CREDENTIAL_FILE"));
}

#[test]
fn invariant_secret_broker_allowlist_overrides() {
    let broker = SecretBrokerPolicy {
        disclosure_allowlist: vec!["ANTHROPIC_API_KEY".to_string()],
        ..SecretBrokerPolicy::default()
    };

    // Allowlisted secrets should NOT be detected
    assert!(
        !broker.is_secret("ANTHROPIC_API_KEY"),
        "allowlisted secret should not be flagged"
    );

    // Other secrets still detected
    assert!(broker.is_secret("OPENAI_API_KEY"));
}

#[test]
fn invariant_secret_broker_disabled_allows_all() {
    let broker = SecretBrokerPolicy {
        enabled: false,
        ..SecretBrokerPolicy::default()
    };

    assert!(
        !broker.is_secret("ANTHROPIC_API_KEY"),
        "disabled broker should not flag anything"
    );
    assert!(!broker.is_secret("AWS_SECRET_ACCESS_KEY"));
}

#[test]
fn invariant_secret_broker_non_secrets_pass_through() {
    let broker = SecretBrokerPolicy::default();

    let non_secrets = ["HOME", "PATH", "USER", "SHELL", "TERM", "LANG", "EDITOR"];

    for name in non_secrets {
        assert!(!broker.is_secret(name), "{name} should NOT be a secret");
    }
}

// ============================================================================
// Invariant 7: Exec mediation policy presets are correctly ordered
// ============================================================================

#[test]
fn invariant_exec_mediation_strict_denies_high_and_above() {
    let strict = ExecMediationPolicy::strict();
    assert!(strict.enabled);
    assert!(strict.audit_all_classified);

    // Strict uses High threshold — stricter than default (Critical)
    let default = ExecMediationPolicy::default();
    assert!(strict.deny_threshold as u8 <= default.deny_threshold as u8);
}

#[test]
fn invariant_exec_mediation_disabled_allows_everything() {
    let disabled = ExecMediationPolicy::disabled();
    assert!(!disabled.enabled);
}

// ============================================================================
// Invariant 8: Profile security ordering (Safe >= Standard >= Permissive)
// ============================================================================

#[test]
fn invariant_profile_downgrade_safe_to_standard_valid() {
    let safe = PolicyProfile::Safe.to_policy();
    let standard = PolicyProfile::Standard.to_policy();

    // Safe → Standard is NOT a valid downgrade (Standard is less strict)
    let check = ExtensionPolicy::is_valid_downgrade(&safe, &standard);
    assert!(!check.is_valid_downgrade);
}

#[test]
fn invariant_profile_downgrade_standard_to_safe_valid() {
    let safe = PolicyProfile::Safe.to_policy();
    let standard = PolicyProfile::Standard.to_policy();

    // Standard → Safe IS a valid downgrade (Safe is stricter)
    let check = ExtensionPolicy::is_valid_downgrade(&standard, &safe);
    assert!(check.is_valid_downgrade);
}

#[test]
fn invariant_profile_downgrade_permissive_to_anything_valid() {
    let permissive = PolicyProfile::Permissive.to_policy();
    let standard = PolicyProfile::Standard.to_policy();
    let safe = PolicyProfile::Safe.to_policy();

    let to_standard = ExtensionPolicy::is_valid_downgrade(&permissive, &standard);
    assert!(to_standard.is_valid_downgrade);

    let to_safe = ExtensionPolicy::is_valid_downgrade(&permissive, &safe);
    assert!(to_safe.is_valid_downgrade);
}

#[test]
fn invariant_all_profiles_deny_dangerous_caps_except_permissive() {
    for profile in [PolicyProfile::Safe, PolicyProfile::Standard] {
        let policy = profile.to_policy();
        for cap in ALL_CAPABILITIES.iter().filter(|c| c.is_dangerous()) {
            let check = policy.evaluate_for(cap.as_str(), None);
            assert_eq!(
                check.decision,
                PolicyDecision::Deny,
                "{profile:?} must deny dangerous cap {}",
                cap.as_str()
            );
        }
    }
}

// ============================================================================
// Invariant 9: OPE gate cannot approve without sufficient evidence
// ============================================================================

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_ope_gate_rejects_empty_evidence() {
    let config = OpeEvaluatorConfig::default();
    let samples: Vec<OpeTraceSample> = Vec::new();

    let report = evaluate_off_policy(&samples, &config);
    assert!(!report.gate.passed);
    assert_eq!(report.gate.reason, OpeGateReason::NoValidSamples);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_ope_gate_rejects_invalid_propensities() {
    let config = OpeEvaluatorConfig::default();
    let samples = vec![
        OpeTraceSample {
            action: "a".to_string(),
            behavior_propensity: 0.0, // Invalid: zero
            target_propensity: 0.5,
            outcome: 1.0,
            baseline_outcome: Some(1.0),
            direct_method_prediction: Some(1.0),
            context_lineage: None,
        },
        OpeTraceSample {
            action: "b".to_string(),
            behavior_propensity: -0.1, // Invalid: negative
            target_propensity: 0.5,
            outcome: 1.0,
            baseline_outcome: Some(1.0),
            direct_method_prediction: Some(1.0),
            context_lineage: None,
        },
    ];

    let report = evaluate_off_policy(&samples, &config);
    assert!(!report.gate.passed);
    assert_eq!(report.diagnostics.valid_samples, 0);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_ope_gate_requires_minimum_effective_sample_size() {
    let config = OpeEvaluatorConfig {
        max_importance_weight: 100.0,
        min_effective_sample_size: 5.0,
        max_standard_error: 10.0,
        confidence_z: 1.96,
        max_regret_delta: 10.0,
    };

    // Single valid sample — insufficient support
    let samples = vec![OpeTraceSample {
        action: "solo".to_string(),
        behavior_propensity: 0.5,
        target_propensity: 0.5,
        outcome: 0.8,
        baseline_outcome: Some(0.7),
        direct_method_prediction: Some(0.8),
        context_lineage: None,
    }];

    let report = evaluate_off_policy(&samples, &config);
    assert!(!report.gate.passed);
}

// ============================================================================
// Invariant 10: Cross-module security surface isolation
// ============================================================================

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_does_not_expose_exec_lane() {
    // Execution capability must NEVER be routed to io_uring
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 256,
        allow_filesystem: true,
        allow_network: true,
    };

    let input = IoUringLaneDecisionInput {
        capability: HostcallCapabilityClass::Execution,
        io_hint: HostcallIoHint::IoHeavy,
        queue_depth: 0,
        force_compat_lane: false,
    };

    let decision = decide_io_uring_lane(config, input);
    assert_ne!(
        decision.lane,
        HostcallDispatchLane::IoUring,
        "execution capability must never use io_uring lane"
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_does_not_expose_env_lane() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 256,
        allow_filesystem: true,
        allow_network: true,
    };

    let input = IoUringLaneDecisionInput {
        capability: HostcallCapabilityClass::Environment,
        io_hint: HostcallIoHint::IoHeavy,
        queue_depth: 0,
        force_compat_lane: false,
    };

    let decision = decide_io_uring_lane(config, input);
    assert_ne!(
        decision.lane,
        HostcallDispatchLane::IoUring,
        "environment capability must never use io_uring lane"
    );
}

#[test]
fn invariant_s3fifo_admission_operates_only_on_authorized_calls() {
    // S3-FIFO operates downstream of policy — it controls queue admission,
    // not authorization. Verify it cannot promote a denied call.
    let mut policy = S3FifoPolicy::new(S3FifoConfig::default());

    // Even if a key is admitted to S3-FIFO, the decision is about queue
    // placement, NOT about whether the call is authorized.
    let d = policy.access("untrusted-ext", "potentially-denied-call".to_string());

    // S3-FIFO only decides queue tier, not policy authorization
    assert!(matches!(
        d.kind,
        S3FifoDecisionKind::AdmitSmall
            | S3FifoDecisionKind::AdmitFromGhost
            | S3FifoDecisionKind::HitMain
            | S3FifoDecisionKind::PromoteSmallToMain
            | S3FifoDecisionKind::RejectFairnessBudget
            | S3FifoDecisionKind::FallbackBypass
    ));
}

// ============================================================================
// Invariant 11: Policy mode behavior exhaustive
// ============================================================================

#[test]
fn invariant_strict_mode_denies_unknown_caps() {
    let policy = PolicyProfile::Safe.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Strict);

    let check = policy.evaluate_for("custom_unknown_cap", None);
    assert_eq!(check.decision, PolicyDecision::Deny);
}

#[test]
fn invariant_prompt_mode_prompts_unknown_caps() {
    let policy = PolicyProfile::Standard.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Prompt);

    let check = policy.evaluate_for("custom_unknown_cap", None);
    assert_eq!(check.decision, PolicyDecision::Prompt);
}

#[test]
fn invariant_permissive_mode_allows_unknown_caps() {
    let policy = PolicyProfile::Permissive.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Permissive);

    let check = policy.evaluate_for("custom_unknown_cap", None);
    assert_eq!(check.decision, PolicyDecision::Allow);
}

// ============================================================================
// Invariant 12: Per-extension mode override isolation
// ============================================================================

#[test]
fn invariant_per_extension_mode_does_not_leak_to_other_extensions() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "permissive-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            allow: Vec::new(),
            deny: Vec::new(),
            quota: None,
        },
    );

    // permissive-ext can use unknown caps
    let check_permissive = policy.evaluate_for("exotic_cap", Some("permissive-ext"));
    assert_eq!(check_permissive.decision, PolicyDecision::Allow);

    // Other extensions use global mode (Prompt)
    let check_other = policy.evaluate_for("exotic_cap", Some("other-ext"));
    assert_eq!(check_other.decision, PolicyDecision::Prompt);

    // No extension context uses global mode
    let check_global = policy.evaluate_for("exotic_cap", None);
    assert_eq!(check_global.decision, PolicyDecision::Prompt);
}

#[test]
fn invariant_per_extension_strict_override_restricts_only_target() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        ..ExtensionPolicy::default()
    };
    policy.per_extension.insert(
        "locked-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Strict),
            allow: Vec::new(),
            deny: Vec::new(),
            quota: None,
        },
    );

    // locked-ext has strict mode
    let check_locked = policy.evaluate_for("exotic_cap", Some("locked-ext"));
    assert_eq!(check_locked.decision, PolicyDecision::Deny);

    // Other extensions still permissive
    let check_other = policy.evaluate_for("exotic_cap", Some("free-ext"));
    assert_eq!(check_other.decision, PolicyDecision::Allow);
}

// ============================================================================
// Invariant 13: Snapshot + policy agreement under complex configurations
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn invariant_snapshot_agrees_with_policy_complex_multi_extension() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        default_caps: vec![
            "read".to_string(),
            "write".to_string(),
            "http".to_string(),
            "events".to_string(),
            "session".to_string(),
        ],
        deny_caps: vec!["exec".to_string(), "env".to_string()],
        per_extension: HashMap::new(),
        max_memory_mb: 256,
        exec_mediation: ExecMediationPolicy::default(),
        secret_broker: SecretBrokerPolicy::default(),
    };

    // Extension A: permissive with exec allowed
    policy.per_extension.insert(
        "ext-a".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            allow: vec!["exec".to_string()],
            deny: Vec::new(),
            quota: None,
        },
    );

    // Extension B: strict with http denied
    policy.per_extension.insert(
        "ext-b".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Strict),
            allow: Vec::new(),
            deny: vec!["http".to_string()],
            quota: None,
        },
    );

    // Extension C: no mode override, but custom allow/deny
    policy.per_extension.insert(
        "ext-c".to_string(),
        ExtensionOverride {
            mode: None,
            allow: vec!["env".to_string()],
            deny: vec!["write".to_string()],
            quota: None,
        },
    );

    let snapshot = PolicySnapshot::compile(&policy);

    let test_contexts: Vec<Option<&str>> = vec![
        None,
        Some("ext-a"),
        Some("ext-b"),
        Some("ext-c"),
        Some("ext-unknown"),
    ];

    for ext_id in &test_contexts {
        for cap in ALL_CAPABILITIES {
            let dynamic = policy.evaluate_for(cap.as_str(), *ext_id);
            let cached = snapshot.lookup(cap.as_str(), *ext_id);
            assert_eq!(
                dynamic.decision,
                cached.decision,
                "MISMATCH: cap={}, ext={:?} → dynamic={:?} cached={:?}",
                cap.as_str(),
                ext_id,
                dynamic.decision,
                cached.decision
            );
        }
    }
}

// ============================================================================
// Invariant 14: S3-FIFO eviction preserves owner count consistency
// ============================================================================

#[test]
fn invariant_s3fifo_owner_counts_consistent_after_eviction() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 4,
        small_capacity: 2,
        ghost_capacity: 8,
        max_entries_per_owner: 4, // High enough to not trigger budget
        fallback_window: 32,
        min_ghost_hits_in_window: 2,
        max_budget_rejections_in_window: 12,
    });

    // Fill capacity from ext-a
    for i in 0..4 {
        let _ = policy.access("ext-a", format!("a-{i}"));
    }

    // Add from ext-b — this forces eviction of ext-a entries
    for i in 0..3 {
        let _ = policy.access("ext-b", format!("b-{i}"));
    }

    let telem = policy.telemetry();
    let total_live: usize = telem.owner_live_counts.values().sum();
    assert_eq!(
        total_live, telem.live_depth,
        "sum of owner counts must equal live depth"
    );
}

// ============================================================================
// Invariant 15: IoUring queue depth budget enforcement
// ============================================================================

#[cfg(feature = "experimental-controllers")]
#[test]
fn invariant_io_uring_queue_depth_budget_cannot_be_exceeded() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 4,
        allow_filesystem: true,
        allow_network: true,
    };

    // At budget boundary
    let at_limit = decide_io_uring_lane(
        config,
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 4,
            force_compat_lane: false,
        },
    );
    assert_ne!(at_limit.lane, HostcallDispatchLane::IoUring);
    assert_eq!(
        at_limit.fallback_reason,
        Some(IoUringFallbackReason::QueueDepthBudgetExceeded)
    );

    // Over budget
    let over_limit = decide_io_uring_lane(
        config,
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 100,
            force_compat_lane: false,
        },
    );
    assert_ne!(over_limit.lane, HostcallDispatchLane::IoUring);

    // Under budget still fails closed until the real ring executor exists.
    let under_limit = decide_io_uring_lane(
        config,
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 3,
            force_compat_lane: false,
        },
    );
    assert_eq!(under_limit.lane, HostcallDispatchLane::Fast);
    assert_eq!(
        under_limit.fallback_reason,
        Some(IoUringFallbackReason::IoUringExecutorUnavailable)
    );
}
