//! Behavioral tests for compiled extension-policy snapshots.

use super::{
    ALL_CAPABILITIES, ExtensionOverride, ExtensionPolicy, ExtensionPolicyMode, PolicyDecision,
    PolicyProfile, PolicySnapshot,
};

fn make_policy_with_per_extension() -> ExtensionPolicy {
    let mut policy = ExtensionPolicy::default();
    policy.default_caps.push("read".to_string());
    policy.default_caps.push("write".to_string());
    policy.default_caps.push("http".to_string());
    policy.deny_caps.push("exec".to_string());

    let mut ext_overrides = ExtensionOverride::default();
    ext_overrides.allow.push("exec".to_string());
    ext_overrides.deny.push("write".to_string());
    policy
        .per_extension
        .insert("ext.special".to_string(), ext_overrides);

    policy
}

#[test]
fn snapshot_matches_evaluate_for_all_known_capabilities() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let direct = policy.evaluate_for(cap.as_str(), None);
        let via_snapshot = snapshot.lookup(cap.as_str(), None);
        assert_eq!(
            direct.decision,
            via_snapshot.decision,
            "global decision mismatch for capability '{}'",
            cap.as_str()
        );
    }
}

#[test]
fn snapshot_matches_per_extension_overrides() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let direct = policy.evaluate_for(cap.as_str(), Some("ext.special"));
        let via_snapshot = snapshot.lookup(cap.as_str(), Some("ext.special"));
        assert_eq!(
            direct.decision,
            via_snapshot.decision,
            "per-extension decision mismatch for '{}' on ext.special",
            cap.as_str()
        );
    }
}

#[test]
fn snapshot_unknown_extension_falls_back_to_global() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let global = snapshot.lookup(cap.as_str(), None);
        let unknown_ext = snapshot.lookup(cap.as_str(), Some("ext.unknown"));
        assert_eq!(
            global.decision,
            unknown_ext.decision,
            "unknown extension should fall back to global for '{}'",
            cap.as_str()
        );
    }
}

#[test]
fn snapshot_unknown_capability_falls_back_to_evaluate_for() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    let direct = policy.evaluate_for("custom_cap_xyz", None);
    let via_snapshot = snapshot.lookup("custom_cap_xyz", None);
    assert_eq!(direct.decision, via_snapshot.decision);
}

#[test]
fn snapshot_permissive_mode_allows_all() {
    let policy = PolicyProfile::Permissive.to_policy();
    let snapshot = PolicySnapshot::compile(&policy);

    for cap in ALL_CAPABILITIES {
        let check = snapshot.lookup(cap.as_str(), None);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "permissive mode should allow '{}'",
            cap.as_str()
        );
    }
}

#[test]
fn snapshot_deny_overrides_default_caps() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    // "exec" is in deny_caps
    let check = snapshot.lookup("exec", None);
    assert_eq!(check.decision, PolicyDecision::Deny);
}

#[test]
fn snapshot_per_extension_deny_overrides_global_allow() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    // "write" is in default_caps (allowed globally)
    let global = snapshot.lookup("write", None);
    assert_eq!(global.decision, PolicyDecision::Allow);

    // but ext.special denies "write"
    let ext = snapshot.lookup("write", Some("ext.special"));
    assert_eq!(ext.decision, PolicyDecision::Deny);
}

#[test]
fn snapshot_global_deny_wins_over_per_extension_allow() {
    let policy = make_policy_with_per_extension();
    let snapshot = PolicySnapshot::compile(&policy);

    // "exec" is in deny_caps (denied globally)
    let global = snapshot.lookup("exec", None);
    assert_eq!(global.decision, PolicyDecision::Deny);

    // ext.special allows "exec", but global deny remains authoritative.
    let ext = snapshot.lookup("exec", Some("ext.special"));
    assert_eq!(ext.decision, PolicyDecision::Deny);
}

#[test]
fn npm_package_policy_retains_filename_denials_in_direct_and_compiled_lookups() {
    let mut policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);
    policy.per_extension.insert(
        "src".to_string(),
        ExtensionOverride {
            deny: vec!["exec".to_string(), "env".to_string()],
            ..Default::default()
        },
    );
    policy.per_extension.insert(
        "npm:@fixture/fetch:src".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string(), "env".to_string()],
            ..Default::default()
        },
    );
    let snapshot = PolicySnapshot::compile(&policy);
    for owner in ["npm:@fixture/fetch:src", "npm:@fixture/search:src"] {
        assert!(policy.has_override(owner));
        for capability in ["exec", "env"] {
            for check in [
                policy.evaluate_for(capability, Some(owner)),
                snapshot.lookup(capability, Some(owner)),
            ] {
                assert_eq!(check.decision, PolicyDecision::Deny);
                assert_eq!(check.reason, "extension_deny");
            }
        }
        assert_eq!(
            snapshot.lookup("http", Some(owner)).decision,
            PolicyDecision::Allow
        );
    }
    // A different filename does not inherit src's restrictions.
    assert!(!policy.has_override("npm:@fixture/other:entry"));
    assert_eq!(
        snapshot
            .lookup("exec", Some("npm:@fixture/other:entry"))
            .decision,
        PolicyDecision::Allow
    );
}

#[test]
fn npm_package_policy_preserves_restrictive_modes_without_borrowing_filename_grants() {
    let owner = "npm:@fixture/fetch:src";
    for (mode, expected) in [
        (ExtensionPolicyMode::Strict, PolicyDecision::Deny),
        (ExtensionPolicyMode::Prompt, PolicyDecision::Prompt),
    ] {
        let mut policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);
        policy.per_extension.insert(
            "src".to_string(),
            ExtensionOverride {
                mode: Some(mode),
                ..Default::default()
            },
        );
        policy.per_extension.insert(
            owner.to_string(),
            ExtensionOverride {
                allow: vec!["exec".to_string()],
                ..Default::default()
            },
        );
        let snapshot = PolicySnapshot::compile(&policy);
        for capability in ["exec", "unknown-capability"] {
            assert_eq!(
                policy.evaluate_for(capability, Some(owner)).decision,
                expected
            );
            assert_eq!(snapshot.lookup(capability, Some(owner)).decision, expected);
        }
    }

    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    policy.per_extension.insert(
        "src".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );
    let snapshot = PolicySnapshot::compile(&policy);
    assert_eq!(
        policy.evaluate_for("exec", Some("src")).decision,
        PolicyDecision::Allow
    );
    for capability in ["exec", "env", "unknown-capability"] {
        assert_eq!(
            policy.evaluate_for(capability, Some(owner)).decision,
            PolicyDecision::Deny
        );
        assert_eq!(
            snapshot.lookup(capability, Some(owner)).decision,
            PolicyDecision::Deny
        );
    }
    policy.per_extension.insert(
        owner.to_string(),
        ExtensionOverride {
            allow: vec!["http".to_string()],
            ..Default::default()
        },
    );
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("http", Some(owner))
            .decision,
        PolicyDecision::Allow
    );
    policy.deny_caps.push("http".to_string());
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("http", Some(owner))
            .decision,
        PolicyDecision::Deny
    );
}

#[test]
fn npm_qualification_treats_the_filename_policy_alias_as_a_literal_key() {
    let owner = "npm:outer:npm:inner:src";
    let mut policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);
    policy.per_extension.insert(
        "src".to_string(),
        ExtensionOverride {
            deny: vec!["exec".to_string()],
            ..Default::default()
        },
    );
    policy.per_extension.insert(
        "npm:inner:src".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );
    assert_eq!(
        policy.evaluate_for("exec", Some(owner)).decision,
        PolicyDecision::Allow
    );
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("exec", Some(owner))
            .decision,
        PolicyDecision::Allow,
        "a filename containing the reserved prefix must not recursively inherit src"
    );
    policy
        .per_extension
        .get_mut("npm:inner:src")
        .unwrap()
        .deny
        .push("exec".to_string());
    assert_eq!(
        policy.evaluate_for("exec", Some(owner)).decision,
        PolicyDecision::Deny
    );
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("exec", Some(owner))
            .decision,
        PolicyDecision::Deny
    );
}

#[test]
fn local_package_policy_never_borrows_filename_or_npm_grants() {
    let owner = "package:@fixture/fetch:src";
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    for alias in ["src", "npm:@fixture/fetch:src"] {
        policy.per_extension.insert(
            alias.to_string(),
            ExtensionOverride {
                mode: Some(ExtensionPolicyMode::Permissive),
                allow: vec!["exec".to_string(), "env".to_string()],
                ..Default::default()
            },
        );
    }
    assert!(policy.has_override(owner));
    let snapshot = PolicySnapshot::compile(&policy);
    for capability in ["exec", "env", "unknown-capability"] {
        assert_eq!(
            policy.evaluate_for(capability, Some(owner)).decision,
            PolicyDecision::Deny
        );
        assert_eq!(
            snapshot.lookup(capability, Some(owner)).decision,
            PolicyDecision::Deny
        );
    }

    policy.per_extension.insert(
        owner.to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("exec", Some(owner))
            .decision,
        PolicyDecision::Allow,
        "an explicit package grant remains usable"
    );
    for (mode, expected) in [
        (ExtensionPolicyMode::Strict, PolicyDecision::Deny),
        (ExtensionPolicyMode::Prompt, PolicyDecision::Prompt),
    ] {
        policy.per_extension.insert(
            "src".to_string(),
            ExtensionOverride {
                mode: Some(mode),
                ..Default::default()
            },
        );
        assert_eq!(policy.evaluate_for("exec", Some(owner)).decision, expected);
        assert_eq!(
            PolicySnapshot::compile(&policy)
                .lookup("exec", Some(owner))
                .decision,
            expected
        );
    }
    policy
        .per_extension
        .get_mut("src")
        .unwrap()
        .deny
        .push("exec".to_string());
    assert_eq!(
        policy.evaluate_for("exec", Some(owner)).decision,
        PolicyDecision::Deny
    );
    assert_eq!(
        PolicySnapshot::compile(&policy)
            .lookup("exec", Some(owner))
            .decision,
        PolicyDecision::Deny
    );
}
