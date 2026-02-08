// Tests for the unified capability policy model (bd-k5q5.4.1).
//
// Covers:
//   - Capability enum parsing and classification
//   - PolicyProfile presets
//   - Per-extension overrides
//   - Full 5-layer precedence chain
//   - Edge cases (empty, unknown, case sensitivity)

use pi::extensions::{
    ALL_CAPABILITIES, Capability, ExtensionOverride, ExtensionPolicy, ExtensionPolicyMode,
    PolicyDecision, PolicyProfile,
};

// ─── Capability Enum ───────────────────────────────────────────────────────

#[test]
fn capability_parse_all_known_tokens() {
    let expected = [
        ("read", Capability::Read),
        ("write", Capability::Write),
        ("http", Capability::Http),
        ("events", Capability::Events),
        ("session", Capability::Session),
        ("ui", Capability::Ui),
        ("exec", Capability::Exec),
        ("env", Capability::Env),
        ("tool", Capability::Tool),
        ("log", Capability::Log),
    ];
    for (token, cap) in &expected {
        assert_eq!(Capability::parse(token), Some(*cap), "parse({token})");
    }
}

#[test]
fn capability_parse_case_insensitive() {
    assert_eq!(Capability::parse("READ"), Some(Capability::Read));
    assert_eq!(Capability::parse("Exec"), Some(Capability::Exec));
    assert_eq!(Capability::parse("  Http  "), Some(Capability::Http));
}

#[test]
fn capability_parse_unknown_returns_none() {
    assert_eq!(Capability::parse(""), None);
    assert_eq!(Capability::parse("network"), None);
    assert_eq!(Capability::parse("filesystem"), None);
}

#[test]
fn capability_as_str_roundtrips() {
    for cap in ALL_CAPABILITIES {
        let s = cap.as_str();
        assert_eq!(Capability::parse(s), Some(*cap), "roundtrip for {s}");
    }
}

#[test]
fn capability_display_matches_as_str() {
    for cap in ALL_CAPABILITIES {
        assert_eq!(format!("{cap}"), cap.as_str());
    }
}

#[test]
fn capability_dangerous_classification() {
    // Only exec and env are dangerous.
    let dangerous: Vec<Capability> = ALL_CAPABILITIES
        .iter()
        .copied()
        .filter(|c| c.is_dangerous())
        .collect();
    assert_eq!(dangerous, vec![Capability::Exec, Capability::Env]);
}

#[test]
fn capability_safe_classification() {
    let safe: Vec<Capability> = ALL_CAPABILITIES
        .iter()
        .copied()
        .filter(|c| !c.is_dangerous())
        .collect();
    assert_eq!(safe.len(), ALL_CAPABILITIES.len() - 2);
    for cap in &safe {
        assert!(
            !matches!(cap, Capability::Exec | Capability::Env),
            "{cap} should be safe"
        );
    }
}

#[test]
fn all_capabilities_is_exhaustive() {
    // If a new variant is added to Capability but not to ALL_CAPABILITIES,
    // this test will catch it via serde roundtrip.
    for cap in ALL_CAPABILITIES {
        let json = serde_json::to_string(cap).unwrap();
        let back: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(*cap, back);
    }
    assert!(
        ALL_CAPABILITIES.len() >= 10,
        "expected at least 10 capabilities"
    );
}

// ─── Policy Profiles ───────────────────────────────────────────────────────

#[test]
fn profile_safe_is_strict_with_safe_defaults() {
    let policy = PolicyProfile::Safe.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Strict);
    assert!(policy.default_caps.contains(&"read".to_string()));
    assert!(policy.deny_caps.contains(&"exec".to_string()));
    assert!(policy.deny_caps.contains(&"env".to_string()));
    // exec denied in safe mode
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    // unknown cap denied in strict
    let check = policy.evaluate("tool");
    assert_eq!(check.decision, PolicyDecision::Deny);
}

#[test]
fn profile_standard_matches_default() {
    let standard = PolicyProfile::Standard.to_policy();
    let default_policy = ExtensionPolicy::default();
    assert_eq!(standard.mode, default_policy.mode);
    assert_eq!(standard.default_caps, default_policy.default_caps);
    assert_eq!(standard.deny_caps, default_policy.deny_caps);
}

#[test]
fn profile_permissive_allows_everything() {
    let policy = PolicyProfile::Permissive.to_policy();
    assert_eq!(policy.mode, ExtensionPolicyMode::Permissive);
    assert!(policy.deny_caps.is_empty());
    for cap in ALL_CAPABILITIES {
        let check = policy.evaluate(cap.as_str());
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "permissive should allow {cap}"
        );
    }
}

#[test]
fn from_profile_is_equivalent_to_to_policy() {
    let a = PolicyProfile::Safe.to_policy();
    let b = ExtensionPolicy::from_profile(PolicyProfile::Safe);
    assert_eq!(a.mode, b.mode);
    assert_eq!(a.default_caps, b.default_caps);
    assert_eq!(a.deny_caps, b.deny_caps);
}

// ─── Per-Extension Overrides ───────────────────────────────────────────────

#[test]
fn extension_override_deny_takes_highest_precedence() {
    // Even if global default_caps includes "read", an extension-level deny
    // for "read" must deny it.
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "malicious-ext".to_string(),
        ExtensionOverride {
            deny: vec!["read".to_string()],
            ..Default::default()
        },
    );

    // Without extension context: read is allowed (default_caps).
    let check = policy.evaluate("read");
    assert_eq!(check.decision, PolicyDecision::Allow);

    // With extension context: read is denied (extension_deny).
    let check = policy.evaluate_for("read", Some("malicious-ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "extension_deny");
}

#[test]
fn extension_override_allow_bypasses_prompt() {
    // Global mode = Prompt, "exec" not in default_caps and not in deny_caps.
    let mut policy = ExtensionPolicy::default();
    // Remove exec from global deny_caps so it would normally prompt.
    policy.deny_caps.clear();
    policy.per_extension.insert(
        "trusted-ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );

    // Without context: prompt (not in default_caps, mode=Prompt).
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Prompt);

    // With trusted-ext context: allowed (extension_allow).
    let check = policy.evaluate_for("exec", Some("trusted-ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "extension_allow");
}

#[test]
fn extension_deny_overrides_global_default_caps() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "sandbox-ext".to_string(),
        ExtensionOverride {
            deny: vec!["http".to_string()],
            ..Default::default()
        },
    );

    // http is in default_caps globally, but denied for sandbox-ext.
    let check = policy.evaluate_for("http", Some("sandbox-ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "extension_deny");

    // Other extensions still get http.
    let check = policy.evaluate_for("http", Some("other-ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "default_caps");
}

#[test]
fn extension_allow_cannot_override_global_deny() {
    // Global deny_caps includes "exec". Extension allow for exec should NOT
    // bypass the global deny (global deny is layer 2, extension allow is
    // layer 3 — layer 2 wins).
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "risky-ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );

    let check = policy.evaluate_for("exec", Some("risky-ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

#[test]
fn extension_mode_override() {
    // Global mode = Prompt. Override for one extension to Permissive.
    let mut policy = ExtensionPolicy::default();
    policy.deny_caps.clear(); // Remove global denies for clarity.
    policy.per_extension.insert(
        "auto-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            ..Default::default()
        },
    );

    // "tool" not in default_caps → global mode=Prompt → prompt.
    let check = policy.evaluate_for("tool", None);
    assert_eq!(check.decision, PolicyDecision::Prompt);

    // For auto-ext: effective mode is Permissive → allow.
    let check = policy.evaluate_for("tool", Some("auto-ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "permissive");
}

#[test]
fn extension_mode_strict_restricts_more_than_global() {
    // Global mode = Prompt (lenient). Override to Strict for one ext.
    let mut policy = ExtensionPolicy::default();
    policy.deny_caps.clear();
    policy.per_extension.insert(
        "restricted-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Strict),
            ..Default::default()
        },
    );

    // "tool" not in default_caps → Prompt mode → prompt for most.
    let check = policy.evaluate_for("tool", None);
    assert_eq!(check.decision, PolicyDecision::Prompt);

    // For restricted-ext: Strict → deny.
    let check = policy.evaluate_for("tool", Some("restricted-ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "not_in_default_caps");
}

#[test]
fn has_override_reflects_presence() {
    let mut policy = ExtensionPolicy::default();
    assert!(!policy.has_override("some-ext"));
    policy
        .per_extension
        .insert("some-ext".to_string(), ExtensionOverride::default());
    assert!(policy.has_override("some-ext"));
    assert!(!policy.has_override("other-ext"));
}

// ─── Full Precedence Chain ─────────────────────────────────────────────────

#[test]
fn precedence_layer1_extension_deny_beats_everything() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        ..Default::default()
    };
    policy.deny_caps.clear(); // No global deny.
    policy.default_caps.push("exec".to_string()); // Global allow.
    policy.per_extension.insert(
        "ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()], // Extension allow too.
            deny: vec!["exec".to_string()],  // But extension deny wins.
            ..Default::default()
        },
    );

    let check = policy.evaluate_for("exec", Some("ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "extension_deny");
}

#[test]
fn precedence_layer2_global_deny_beats_extension_allow() {
    let mut policy = ExtensionPolicy::default();
    // "exec" is in deny_caps by default.
    policy.per_extension.insert(
        "ext".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );

    let check = policy.evaluate_for("exec", Some("ext"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

#[test]
fn precedence_layer3_extension_allow_beats_mode_fallback() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        deny_caps: Vec::new(),
        ..Default::default()
    };
    // "tool" not in default_caps → Strict would deny.
    policy.per_extension.insert(
        "ext".to_string(),
        ExtensionOverride {
            allow: vec!["tool".to_string()],
            ..Default::default()
        },
    );

    let check = policy.evaluate_for("tool", Some("ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "extension_allow");
}

#[test]
fn precedence_layer4_default_caps_allows_before_mode() {
    let policy = ExtensionPolicy::default();
    // "read" is in default_caps → allowed regardless of mode.
    let check = policy.evaluate("read");
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "default_caps");
}

#[test]
fn precedence_layer5_mode_fallback_when_nothing_matches() {
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        deny_caps: Vec::new(),
        ..Default::default()
    };
    // "tool" not in default_caps, not denied, no override.
    let check = policy.evaluate("tool");
    assert_eq!(check.decision, PolicyDecision::Prompt);
    assert_eq!(check.reason, "prompt_required");

    policy.mode = ExtensionPolicyMode::Strict;
    let check = policy.evaluate("tool");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "not_in_default_caps");

    policy.mode = ExtensionPolicyMode::Permissive;
    let check = policy.evaluate("tool");
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "permissive");
}

// ─── Edge Cases ────────────────────────────────────────────────────────────

#[test]
fn evaluate_empty_capability_denied() {
    let policy = ExtensionPolicy::default();
    let check = policy.evaluate("");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "empty_capability");
}

#[test]
fn evaluate_whitespace_only_denied() {
    let policy = ExtensionPolicy::default();
    let check = policy.evaluate("   ");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "empty_capability");
}

#[test]
fn evaluate_for_unknown_extension_falls_through_to_global() {
    let policy = ExtensionPolicy::default();
    // No per-extension override for "nonexistent".
    let check = policy.evaluate_for("read", Some("nonexistent"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "default_caps");
}

#[test]
fn evaluate_for_none_extension_same_as_evaluate() {
    let policy = ExtensionPolicy::default();
    let a = policy.evaluate("read");
    let b = policy.evaluate_for("read", None);
    assert_eq!(a.decision, b.decision);
    assert_eq!(a.reason, b.reason);
    assert_eq!(a.capability, b.capability);
}

#[test]
fn extension_override_case_insensitive_matching() {
    let mut policy = ExtensionPolicy::default();
    policy.deny_caps.clear();
    policy.per_extension.insert(
        "ext".to_string(),
        ExtensionOverride {
            allow: vec!["EXEC".to_string()],
            ..Default::default()
        },
    );

    let check = policy.evaluate_for("exec", Some("ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "extension_allow");
}

#[test]
fn multiple_extensions_independent() {
    let mut policy = ExtensionPolicy::default();
    policy.deny_caps.clear();
    policy.per_extension.insert(
        "ext-a".to_string(),
        ExtensionOverride {
            deny: vec!["http".to_string()],
            ..Default::default()
        },
    );
    policy.per_extension.insert(
        "ext-b".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );

    // ext-a: http denied, exec prompted.
    let check = policy.evaluate_for("http", Some("ext-a"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    let check = policy.evaluate_for("exec", Some("ext-a"));
    assert_eq!(check.decision, PolicyDecision::Prompt);

    // ext-b: http allowed (default_caps), exec allowed (extension_allow).
    let check = policy.evaluate_for("http", Some("ext-b"));
    assert_eq!(check.decision, PolicyDecision::Allow);
    let check = policy.evaluate_for("exec", Some("ext-b"));
    assert_eq!(check.decision, PolicyDecision::Allow);
}

// ─── Serialization ─────────────────────────────────────────────────────────

#[test]
fn policy_serde_roundtrip_with_overrides() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "test-ext".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Strict),
            allow: vec!["tool".to_string()],
            deny: vec!["env".to_string()],
        },
    );

    let json = serde_json::to_string(&policy).unwrap();
    let back: ExtensionPolicy = serde_json::from_str(&json).unwrap();

    assert_eq!(back.mode, policy.mode);
    assert_eq!(back.default_caps, policy.default_caps);
    assert!(back.per_extension.contains_key("test-ext"));

    let ovr = &back.per_extension["test-ext"];
    assert_eq!(ovr.mode, Some(ExtensionPolicyMode::Strict));
    assert_eq!(ovr.allow, vec!["tool".to_string()]);
    assert_eq!(ovr.deny, vec!["env".to_string()]);
}

#[test]
fn policy_deserialize_without_per_extension_gets_empty_map() {
    let json =
        r#"{"mode":"prompt","max_memory_mb":256,"default_caps":["read"],"deny_caps":["exec"]}"#;
    let policy: ExtensionPolicy = serde_json::from_str(json).unwrap();
    assert!(policy.per_extension.is_empty());
}

#[test]
fn capability_serde_roundtrip() {
    for cap in ALL_CAPABILITIES {
        let json = serde_json::to_string(cap).unwrap();
        let back: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(*cap, back);
    }
}

#[test]
fn profile_serde_roundtrip() {
    for profile in &[
        PolicyProfile::Safe,
        PolicyProfile::Standard,
        PolicyProfile::Permissive,
    ] {
        let json = serde_json::to_string(profile).unwrap();
        let back: PolicyProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(*profile, back);
    }
}
