//! Behavioral tests for compiled extension-policy snapshots.

use super::{
    ALL_CAPABILITIES, ExtensionOverride, ExtensionPolicy, PolicyDecision, PolicyProfile,
    PolicySnapshot,
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
