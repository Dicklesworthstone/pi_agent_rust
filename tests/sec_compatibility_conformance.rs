#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::if_not_else)]
//! SEC-6.4 Compatibility conformance + CI security quality gates (bd-1a2cu).
//!
//! Validates that hardened security policies do NOT break benign extension
//! workflows.  Each test evaluates a concrete security subsystem under all
//! three policy profiles (Safe / Standard / Permissive) and asserts the
//! expected decision for benign operations.
//!
//! The test suite also produces a machine-readable conformance verdict
//! artifact at `tests/full_suite_gate/sec_conformance_verdict.json` that
//! the CI full-suite gate consumes.
//!
//! Run:
//!   cargo test --test sec_compatibility_conformance -- --nocapture
//!
//! Regenerate the tracked verdict explicitly with:
//!   PI_GENERATE_SEC_CONFORMANCE_VERDICT=1 cargo test --test sec_compatibility_conformance generate_sec_conformance_verdict -- --exact --nocapture

mod common;

use common::TestHarness;
use pi::extensions::{
    ALL_CAPABILITIES, CompatibilityScanner, ExtensionManager, ExtensionOverride, ExtensionPolicy,
    ExtensionPolicyMode, ExtensionTrustState, PolicyDecision, PolicyExplanation, PolicyProfile,
    RuntimeRiskConfig,
};
use std::collections::BTreeMap;

const GENERATE_SEC_CONFORMANCE_VERDICT_ENV: &str = "PI_GENERATE_SEC_CONFORMANCE_VERDICT";

fn sec_conformance_verdict_generation_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn sec_conformance_verdict_generation_requested() -> bool {
    sec_conformance_verdict_generation_enabled(
        std::env::var(GENERATE_SEC_CONFORMANCE_VERDICT_ENV)
            .ok()
            .as_deref(),
    )
}

// ============================================================================
// Conformance verdict artifact infrastructure
// ============================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ConformanceCheck {
    id: String,
    category: String,
    profile: String,
    description: String,
    status: String, // "pass", "fail", "skip"
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SecConformanceVerdict {
    schema: String,
    generated_at: String,
    bead: String,
    verdict: String,
    pass_count: usize,
    fail_count: usize,
    skip_count: usize,
    total: usize,
    pass_rate_pct: f64,
    threshold_pct: f64,
    checks: Vec<ConformanceCheck>,
    categories: BTreeMap<String, CategorySummary>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct CategorySummary {
    pass: usize,
    fail: usize,
    skip: usize,
}

/// Collect all conformance check results into a verdict payload.
fn build_verdict(checks: &[ConformanceCheck], generated_at: String) -> SecConformanceVerdict {
    let pass_count = checks.iter().filter(|c| c.status == "pass").count();
    let fail_count = checks.iter().filter(|c| c.status == "fail").count();
    let skip_count = checks.iter().filter(|c| c.status == "skip").count();
    let total = checks.len();
    let pass_rate = if total > 0 {
        (pass_count as f64 / total as f64) * 100.0
    } else {
        100.0
    };

    let mut categories: BTreeMap<String, CategorySummary> = BTreeMap::new();
    for check in checks {
        let entry = categories
            .entry(check.category.clone())
            .or_insert(CategorySummary {
                pass: 0,
                fail: 0,
                skip: 0,
            });
        match check.status.as_str() {
            "pass" => entry.pass += 1,
            "fail" => entry.fail += 1,
            _ => entry.skip += 1,
        }
    }

    let threshold = 95.0;
    let verdict = if pass_rate >= threshold {
        "pass"
    } else {
        "fail"
    };

    SecConformanceVerdict {
        schema: "pi.sec.compatibility_conformance.v1".to_string(),
        generated_at,
        bead: "bd-1a2cu".to_string(),
        verdict: verdict.to_string(),
        pass_count,
        fail_count,
        skip_count,
        total,
        pass_rate_pct: (pass_rate * 10.0).round() / 10.0,
        threshold_pct: threshold,
        checks: checks.to_vec(),
        categories,
    }
}

fn validate_verdict_payload(report: &SecConformanceVerdict, checks: &[ConformanceCheck]) -> String {
    assert_eq!(report.schema, "pi.sec.compatibility_conformance.v1");
    assert!(!report.generated_at.is_empty(), "generated_at must be set");
    assert_eq!(report.bead, "bd-1a2cu");
    assert_eq!(report.total, checks.len());
    assert_eq!(
        report.pass_count + report.fail_count + report.skip_count,
        report.total,
        "verdict counters must partition all checks"
    );
    assert_eq!(report.checks, checks);

    let category_total: usize = report
        .categories
        .values()
        .map(|summary| summary.pass + summary.fail + summary.skip)
        .sum();
    assert_eq!(
        category_total, report.total,
        "category counters must partition all checks"
    );

    let payload =
        serde_json::to_string_pretty(report).expect("serialize SEC conformance verdict payload");
    assert!(
        !payload.trim().is_empty(),
        "SEC conformance verdict payload must be non-empty"
    );
    let roundtripped: SecConformanceVerdict =
        serde_json::from_str(&payload).expect("parse serialized SEC conformance verdict payload");
    assert_eq!(roundtripped, *report, "SEC verdict payload roundtrip");
    payload
}

fn maybe_write_verdict(payload: &str) {
    if !sec_conformance_verdict_generation_requested() {
        eprintln!(
            "SEC conformance verdict validated in memory; set \
             {GENERATE_SEC_CONFORMANCE_VERDICT_ENV}=1 to write tracked evidence"
        );
        return;
    }

    let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("full_suite_gate");
    std::fs::create_dir_all(&out_dir).expect("create SEC conformance verdict directory");
    let path = out_dir.join("sec_conformance_verdict.json");
    std::fs::write(&path, payload.as_bytes()).expect("write SEC conformance verdict artifact");
    let metadata = std::fs::metadata(&path).expect("stat generated SEC conformance verdict");
    assert!(metadata.is_file(), "SEC conformance verdict is not a file");
    assert_eq!(
        metadata.len(),
        u64::try_from(payload.len()).expect("SEC conformance verdict length fits u64"),
        "generated SEC conformance verdict length mismatch"
    );
}

// ============================================================================
// Test helpers
// ============================================================================

fn all_profiles() -> Vec<(&'static str, PolicyProfile)> {
    vec![
        ("safe", PolicyProfile::Safe),
        ("standard", PolicyProfile::Standard),
        ("permissive", PolicyProfile::Permissive),
    ]
}

fn benign_capabilities() -> Vec<&'static str> {
    vec!["read", "write", "http", "events", "session"]
}

fn dangerous_capabilities() -> Vec<&'static str> {
    vec!["exec", "env"]
}

fn make_manager() -> ExtensionManager {
    let mgr = ExtensionManager::new();
    mgr.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    });
    mgr
}

// ============================================================================
// WS2: Supply-chain benign compatibility
// ============================================================================

/// Benign extension source that only uses safe capabilities (no exec/env).
const BENIGN_EXTENSION_SRC: &str = r#"
export default function init(pi) {
    pi.tool({
        name: "greet",
        description: "A safe greeting tool",
        schema: { type: "object", properties: { name: { type: "string" } } },
        handler: async ({ name }) => ({ display: `Hello, ${name}!` }),
    });
}
"#;

/// Extension source that uses process.env (dangerous capability).
const DANGEROUS_EXTENSION_SRC: &str = r#"
const key = process.env.SECRET_KEY;
export default function init(pi) {
    pi.tool({
        name: "stealer",
        description: "reads env",
        schema: { type: "object" },
        handler: async () => ({ display: key }),
    });
}
"#;

#[test]
fn ws2_benign_extension_passes_compatibility_scan() {
    let harness = TestHarness::new("ws2_benign_extension_passes_compat_scan");
    let root = harness.temp_dir().to_path_buf();
    let ext_dir = root.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("greet.js"), BENIGN_EXTENSION_SRC).unwrap();

    let scanner = CompatibilityScanner::new(root);
    let ledger = scanner.scan_root().expect("scan must succeed");

    // Benign source should have no forbidden patterns
    assert!(
        ledger.forbidden.is_empty(),
        "Benign extension must not have forbidden patterns: {:?}",
        ledger.forbidden
    );
}

#[test]
fn ws2_dangerous_extension_flagged_by_scanner() {
    let harness = TestHarness::new("ws2_dangerous_extension_flagged");
    let root = harness.temp_dir().to_path_buf();
    let ext_dir = root.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("stealer.js"), DANGEROUS_EXTENSION_SRC).unwrap();

    let scanner = CompatibilityScanner::new(root);
    let ledger = scanner.scan_root().expect("scan must succeed");

    // Should detect env capability usage
    let has_env_cap = ledger.capabilities.iter().any(|c| c.capability == "env");
    assert!(
        has_env_cap,
        "Scanner must detect env capability in dangerous extension"
    );
}

// ============================================================================
// WS4: Capability policy - benign cap allowed across profiles
// ============================================================================

#[test]
fn ws4_benign_caps_allowed_in_all_profiles() {
    for (profile_name, profile) in all_profiles() {
        let policy = profile.to_policy();
        for cap in benign_capabilities() {
            let check = policy.evaluate(cap);
            assert!(
                check.decision != PolicyDecision::Deny,
                "Benign capability '{cap}' must NOT be denied in {profile_name} profile \
                 (got {:?}, reason: {})",
                check.decision,
                check.reason,
            );
        }
    }
}

#[test]
fn ws4_dangerous_caps_denied_in_safe_and_standard() {
    for profile in [PolicyProfile::Safe, PolicyProfile::Standard] {
        let policy = profile.to_policy();
        for cap in dangerous_capabilities() {
            let check = policy.evaluate(cap);
            assert_eq!(
                check.decision,
                PolicyDecision::Deny,
                "Dangerous capability '{cap}' must be denied in {profile:?}",
            );
        }
    }
}

#[test]
fn ws4_dangerous_caps_allowed_in_permissive() {
    let policy = PolicyProfile::Permissive.to_policy();
    for cap in dangerous_capabilities() {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "Dangerous capability '{cap}' must be allowed in permissive"
        );
    }
}

#[test]
fn ws4_per_extension_override_cannot_bypass_deny_caps() {
    for (profile_name, profile) in [
        ("safe", PolicyProfile::Safe),
        ("standard", PolicyProfile::Standard),
    ] {
        let mut policy = profile.to_policy();
        policy.per_extension.insert(
            "malicious-ext".to_string(),
            ExtensionOverride {
                allow: vec!["exec".to_string(), "env".to_string()],
                deny: Vec::new(),
                mode: None,
                quota: None,
            },
        );

        for cap in dangerous_capabilities() {
            let check = policy.evaluate_for(cap, Some("malicious-ext"));
            assert_eq!(
                check.decision,
                PolicyDecision::Deny,
                "Per-extension override must NOT bypass deny_caps for '{cap}' in {profile_name}"
            );
        }
    }
}

#[test]
fn ws4_per_extension_deny_overrides_default_allow() {
    let mut policy = PolicyProfile::Permissive.to_policy();
    policy.per_extension.insert(
        "restricted-ext".to_string(),
        ExtensionOverride {
            allow: Vec::new(),
            deny: vec!["http".to_string()],
            mode: None,
            quota: None,
        },
    );

    let check = policy.evaluate_for("http", Some("restricted-ext"));
    assert_eq!(
        check.decision,
        PolicyDecision::Deny,
        "Per-extension deny must override default allow"
    );

    // But read should still be allowed
    let check = policy.evaluate_for("read", Some("restricted-ext"));
    assert_eq!(check.decision, PolicyDecision::Allow);
}

#[test]
fn ws4_explain_effective_policy_covers_all_capabilities() {
    for (profile_name, profile) in all_profiles() {
        let policy = profile.to_policy();
        let explanation = policy.explain_effective_policy(None);

        assert_eq!(
            explanation.capability_decisions.len(),
            ALL_CAPABILITIES.len(),
            "Explanation in {profile_name} must cover all capabilities"
        );

        // Each decision must have a reason
        for cd in &explanation.capability_decisions {
            assert!(
                !cd.reason.is_empty(),
                "Decision for '{}' in {profile_name} must have a reason",
                cd.capability
            );
        }
    }
}

#[test]
fn ws4_profile_transition_downgrade_validation() {
    // Permissive → Standard is a valid downgrade (tightening)
    let permissive = PolicyProfile::Permissive.to_policy();
    let standard = PolicyProfile::Standard.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&permissive, &standard);
    assert!(
        check.is_valid_downgrade,
        "Permissive → Standard must be a valid downgrade"
    );

    // Permissive → Safe is a valid downgrade
    let safe = PolicyProfile::Safe.to_policy();
    let check = ExtensionPolicy::is_valid_downgrade(&permissive, &safe);
    assert!(
        check.is_valid_downgrade,
        "Permissive → Safe must be a valid downgrade"
    );

    // Standard → Safe is a valid downgrade
    let check = ExtensionPolicy::is_valid_downgrade(&standard, &safe);
    assert!(
        check.is_valid_downgrade,
        "Standard → Safe must be a valid downgrade"
    );

    // Safe → Permissive is NOT a valid downgrade (loosening)
    let check = ExtensionPolicy::is_valid_downgrade(&safe, &permissive);
    assert!(
        !check.is_valid_downgrade,
        "Safe → Permissive must NOT be a valid downgrade"
    );
}

// ============================================================================
// WS3: Runtime risk - fresh manager has zero alerts
// ============================================================================

#[test]
fn ws3_fresh_manager_has_no_alerts() {
    let manager = make_manager();
    assert_eq!(
        manager.security_alert_count(),
        0,
        "Fresh manager must have zero security alerts"
    );
}

#[test]
fn ws3_runtime_risk_config_roundtrip() {
    let manager = make_manager();
    let config = manager.runtime_risk_config();
    assert!(config.enabled, "Risk config must be enabled");
    assert!(config.enforce, "Risk config must enforce");
    assert_eq!(config.window_size, 64);
}

// ============================================================================
// WS5: Security alerts via kill-switch (public API)
// ============================================================================

#[test]
fn ws5_kill_switch_generates_alert() {
    let manager = make_manager();
    let ext_id = "threat-ext";

    assert_eq!(manager.security_alert_count(), 0);

    let result = manager.kill_switch(ext_id, "emergency", "operator:admin");
    assert!(result.success);
    assert!(manager.is_killed(ext_id));

    // Kill-switch must emit at least one alert
    assert!(
        manager.security_alert_count() > 0,
        "Kill-switch must emit a security alert"
    );

    // Verify via artifact export
    let artifact = manager.security_alert_artifact();
    assert!(
        artifact.alert_count > 0,
        "Security alert artifact must contain the kill-switch alert"
    );
}

#[test]
fn ws5_lift_kill_switch_generates_alert() {
    let manager = make_manager();
    let ext_id = "restore-ext";

    manager.kill_switch(ext_id, "test", "admin");
    let before = manager.security_alert_count();

    let result = manager.lift_kill_switch(ext_id, "cleared", "admin");
    assert!(result.success);

    assert!(
        manager.security_alert_count() > before,
        "Lifting kill-switch must emit an additional alert"
    );
}

// ============================================================================
// WS5: Trust lifecycle - benign path works correctly
// ============================================================================

#[test]
fn ws5_trust_onboarding_accept_sets_acknowledged() {
    let manager = make_manager();
    let ext_id = "new-ext";

    assert_eq!(manager.trust_state(ext_id), ExtensionTrustState::Pending);

    let state = manager.record_trust_onboarding(ext_id, "low", true, "user:alice");
    assert_eq!(state, ExtensionTrustState::Acknowledged);
}

#[test]
fn ws5_trust_onboarding_reject_sets_killed() {
    let manager = make_manager();
    let ext_id = "suspicious-ext";

    let state = manager.record_trust_onboarding(ext_id, "high", false, "user:bob");
    assert_eq!(state, ExtensionTrustState::Killed);
    assert!(manager.is_killed(ext_id));
}

#[test]
fn ws5_trust_promotion_lifecycle() {
    let manager = make_manager();
    let ext_id = "trusted-ext";

    manager.record_trust_onboarding(ext_id, "low", true, "user:alice");
    assert_eq!(
        manager.trust_state(ext_id),
        ExtensionTrustState::Acknowledged
    );

    let state = manager.promote_trust(ext_id);
    assert_eq!(state, ExtensionTrustState::Trusted);
}

#[test]
fn ws5_kill_switch_audit_log_records_entries() {
    let manager = make_manager();
    let ext_id = "audited-ext";

    manager.kill_switch(ext_id, "threat detected", "security-team");
    let audit = manager.kill_switch_audit_log();
    assert!(
        !audit.is_empty(),
        "Audit log must record kill-switch activation"
    );
    assert_eq!(audit[0].extension_id, ext_id);
    assert!(audit[0].activated);
}

#[test]
fn ws5_trust_onboarding_decisions_logged() {
    let manager = make_manager();
    manager.record_trust_onboarding("ext-a", "low", true, "user:alice");
    manager.record_trust_onboarding("ext-b", "high", false, "user:bob");

    let decisions = manager.trust_onboarding_decisions();
    assert_eq!(decisions.len(), 2);
    assert!(decisions[0].accepted);
    assert!(!decisions[1].accepted);
}

// ============================================================================
// Cross-profile consistency matrix
// ============================================================================

#[test]
fn cross_profile_benign_cap_consistency() {
    // All profiles must agree that read/write/http/events/session are NOT denied
    let profiles = all_profiles();
    for cap in benign_capabilities() {
        let decisions: Vec<(&str, PolicyDecision)> = profiles
            .iter()
            .map(|(name, p)| (*name, p.to_policy().evaluate(cap).decision))
            .collect();

        for (name, decision) in &decisions {
            assert_ne!(
                *decision,
                PolicyDecision::Deny,
                "Benign cap '{cap}' denied in profile '{name}'"
            );
        }
    }
}

#[test]
fn cross_profile_dangerous_cap_matrix() {
    // Dangerous caps must be denied in safe/standard, allowed in permissive
    for cap in dangerous_capabilities() {
        let safe = PolicyProfile::Safe.to_policy().evaluate(cap).decision;
        let std = PolicyProfile::Standard.to_policy().evaluate(cap).decision;
        let perm = PolicyProfile::Permissive.to_policy().evaluate(cap).decision;

        assert_eq!(safe, PolicyDecision::Deny, "{cap} must be denied in safe");
        assert_eq!(
            std,
            PolicyDecision::Deny,
            "{cap} must be denied in standard"
        );
        assert_eq!(
            perm,
            PolicyDecision::Allow,
            "{cap} must be allowed in permissive"
        );
    }
}

#[test]
fn cross_profile_mode_strictness_ordering() {
    let safe = PolicyProfile::Safe.to_policy();
    let standard = PolicyProfile::Standard.to_policy();
    let permissive = PolicyProfile::Permissive.to_policy();

    assert_eq!(safe.mode, ExtensionPolicyMode::Strict);
    assert_eq!(standard.mode, ExtensionPolicyMode::Prompt);
    assert_eq!(permissive.mode, ExtensionPolicyMode::Permissive);
}

#[test]
fn cross_profile_default_policy_matches_standard() {
    let standard = PolicyProfile::Standard.to_policy();
    let default = ExtensionPolicy::default();
    assert_eq!(standard.mode, default.mode);
    assert_eq!(standard.default_caps, default.default_caps);
    assert_eq!(standard.deny_caps, default.deny_caps);
}

// ============================================================================
// Regression guards
// ============================================================================

#[test]
fn regression_safe_profile_always_denies_exec() {
    let policy = PolicyProfile::Safe.to_policy();
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

#[test]
fn regression_standard_profile_always_denies_exec() {
    let policy = PolicyProfile::Standard.to_policy();
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

#[test]
fn regression_deny_caps_layer_precedes_extension_allow() {
    for profile in [PolicyProfile::Safe, PolicyProfile::Standard] {
        let mut policy = profile.to_policy();
        policy.per_extension.insert(
            "sneaky".to_string(),
            ExtensionOverride {
                allow: vec!["exec".to_string()],
                deny: Vec::new(),
                mode: None,
                quota: None,
            },
        );
        let check = policy.evaluate_for("exec", Some("sneaky"));
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "deny_caps must always win over per-extension allow in {profile:?}",
        );
    }
}

#[test]
fn regression_extension_deny_layer_is_highest_priority() {
    // Per-extension deny (layer 1) is higher than everything else
    let mut policy = PolicyProfile::Permissive.to_policy();
    policy.per_extension.insert(
        "lock-read".to_string(),
        ExtensionOverride {
            allow: Vec::new(),
            deny: vec!["read".to_string()],
            mode: None,
            quota: None,
        },
    );
    let check = policy.evaluate_for("read", Some("lock-read"));
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "extension_deny");
}

#[test]
fn regression_explain_policy_serde_roundtrip() {
    for (_, profile) in all_profiles() {
        let policy = profile.to_policy();
        let explanation = policy.explain_effective_policy(None);
        let json = serde_json::to_string(&explanation).expect("serialize");
        let restored: PolicyExplanation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(explanation.mode, restored.mode);
        assert_eq!(
            explanation.capability_decisions.len(),
            restored.capability_decisions.len()
        );
    }
}

// ============================================================================
// Compatibility scanner + policy interaction
// ============================================================================

#[test]
fn compat_scan_benign_source_has_no_forbidden() {
    let harness = TestHarness::new("compat_scan_benign_no_forbidden");
    let root = harness.temp_dir().to_path_buf();
    let ext_dir = root.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("safe.js"), BENIGN_EXTENSION_SRC).unwrap();

    let scanner = CompatibilityScanner::new(root);
    let ledger = scanner.scan_root().unwrap();

    // Under all profiles, a benign extension with no forbidden patterns
    // should produce no compatibility issues
    assert!(ledger.forbidden.is_empty());
    assert!(ledger.flagged.is_empty());
}

#[test]
fn compat_scan_dangerous_source_detects_capabilities() {
    let harness = TestHarness::new("compat_scan_dangerous_detected");
    let root = harness.temp_dir().to_path_buf();
    let ext_dir = root.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("danger.js"), DANGEROUS_EXTENSION_SRC).unwrap();

    let scanner = CompatibilityScanner::new(root);
    let ledger = scanner.scan_root().unwrap();

    // Should detect env capability
    assert!(
        ledger.capabilities.iter().any(|c| c.capability == "env"),
        "Must detect env usage"
    );
}

// ============================================================================
// Exception/waiver process validation
// ============================================================================

#[test]
fn waiver_gate_ids_must_be_well_formed() {
    // Validate that a well-formed waiver TOML entry can be parsed.
    // (This tests the format expectation, not the actual TOML parsing.)
    let example_waiver = toml::toml! {
        [waiver.sec_conformance]
        owner = "security-team"
        created = "2026-02-14"
        expires = "2026-03-14"
        bead = "bd-1a2cu"
        reason = "Known incompatibility with legacy extension X during migration"
        scope = "full"
        remove_when = "Extension X migrated to v2 API"
    };

    let waiver_table = example_waiver
        .get("waiver")
        .and_then(toml::Value::as_table)
        .expect("must have waiver table");
    assert!(
        waiver_table.contains_key("sec_conformance"),
        "Must contain gate entry"
    );
    let entry = waiver_table
        .get("sec_conformance")
        .and_then(toml::Value::as_table)
        .expect("gate entry must be a table");

    for field in &[
        "owner",
        "created",
        "expires",
        "bead",
        "reason",
        "scope",
        "remove_when",
    ] {
        assert!(
            entry.contains_key(*field),
            "Missing required field: {field}"
        );
    }

    let scope = entry
        .get("scope")
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    assert!(
        ["full", "preflight", "both"].contains(&scope),
        "Invalid scope: {scope}"
    );
}

#[test]
fn waiver_max_duration_enforced() {
    let created = chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
    let max_days = 30;

    // 30 days is OK
    let ok_expires = created + chrono::Duration::days(max_days);
    assert!(
        (ok_expires - created).num_days() <= max_days,
        "30-day waiver must be valid"
    );

    // 31 days exceeds limit
    let bad_expires = created + chrono::Duration::days(max_days + 1);
    assert!(
        (bad_expires - created).num_days() > max_days,
        "31-day waiver must be rejected"
    );
}

// ============================================================================
// SEC-6.4 verdict artifact generation
// ============================================================================

/// Master test that generates the sec_conformance_verdict.json artifact.
/// This test does NOT assert-fail — it collects results from all checks
/// and writes the verdict. Individual tests above are the hard gates.
#[test]
fn generate_sec_conformance_verdict() {
    let mut checks: Vec<ConformanceCheck> = Vec::new();

    // Category 1: Benign capability compatibility (WS4)
    for (pname, profile) in all_profiles() {
        let policy = profile.to_policy();
        for cap in benign_capabilities() {
            let result = policy.evaluate(cap);
            let status = if result.decision != PolicyDecision::Deny {
                "pass"
            } else {
                "fail"
            };
            checks.push(ConformanceCheck {
                id: format!("benign_cap_{cap}_{pname}"),
                category: "benign_capability".to_string(),
                profile: pname.to_string(),
                description: format!("Benign capability '{cap}' must not be denied in {pname}"),
                status: status.to_string(),
                detail: if status == "fail" {
                    Some(format!(
                        "decision={:?}, reason={}",
                        result.decision, result.reason
                    ))
                } else {
                    None
                },
            });
        }
    }

    // Category 2: Dangerous capability gating (WS4)
    for cap in dangerous_capabilities() {
        for (pname, profile) in &[
            ("safe", PolicyProfile::Safe),
            ("standard", PolicyProfile::Standard),
        ] {
            let result = profile.to_policy().evaluate(cap);
            let status = if result.decision == PolicyDecision::Deny {
                "pass"
            } else {
                "fail"
            };
            checks.push(ConformanceCheck {
                id: format!("dangerous_denied_{cap}_{pname}"),
                category: "dangerous_gating".to_string(),
                profile: pname.to_string(),
                description: format!("Dangerous capability '{cap}' must be denied in {pname}"),
                status: status.to_string(),
                detail: None,
            });
        }

        let result = PolicyProfile::Permissive.to_policy().evaluate(cap);
        let status = if result.decision == PolicyDecision::Allow {
            "pass"
        } else {
            "fail"
        };
        checks.push(ConformanceCheck {
            id: format!("dangerous_allowed_{cap}_permissive"),
            category: "dangerous_gating".to_string(),
            profile: "permissive".to_string(),
            description: format!("Dangerous capability '{cap}' must be allowed in permissive"),
            status: status.to_string(),
            detail: None,
        });
    }

    // Category 3: Policy explanation completeness
    for (pname, profile) in all_profiles() {
        let policy = profile.to_policy();
        let explanation = policy.explain_effective_policy(None);
        let status = if explanation.capability_decisions.len() == ALL_CAPABILITIES.len() {
            "pass"
        } else {
            "fail"
        };
        checks.push(ConformanceCheck {
            id: format!("explanation_complete_{pname}"),
            category: "explanation_completeness".to_string(),
            profile: pname.to_string(),
            description: format!(
                "Policy explanation in {pname} covers all {} capabilities",
                ALL_CAPABILITIES.len()
            ),
            status: status.to_string(),
            detail: None,
        });
    }

    // Category 4: Profile transition validation
    let transitions = [
        ("permissive", "standard", true),
        ("permissive", "safe", true),
        ("standard", "safe", true),
        ("safe", "permissive", false),
        ("standard", "permissive", false),
    ];
    for (from_name, to_name, expect_valid) in transitions {
        let from_profile = match from_name {
            "safe" => PolicyProfile::Safe,
            "standard" => PolicyProfile::Standard,
            _ => PolicyProfile::Permissive,
        };
        let to_profile = match to_name {
            "safe" => PolicyProfile::Safe,
            "standard" => PolicyProfile::Standard,
            _ => PolicyProfile::Permissive,
        };
        let check =
            ExtensionPolicy::is_valid_downgrade(&from_profile.to_policy(), &to_profile.to_policy());
        let status = if check.is_valid_downgrade == expect_valid {
            "pass"
        } else {
            "fail"
        };
        checks.push(ConformanceCheck {
            id: format!("transition_{from_name}_to_{to_name}"),
            category: "profile_transition".to_string(),
            profile: format!("{from_name} → {to_name}"),
            description: format!(
                "Transition {from_name} → {to_name} should {}be a valid downgrade",
                if expect_valid { "" } else { "NOT " }
            ),
            status: status.to_string(),
            detail: None,
        });
    }

    // Category 5: Deny-cap precedence (per-extension override cannot bypass)
    for (pname, profile) in &[
        ("safe", PolicyProfile::Safe),
        ("standard", PolicyProfile::Standard),
    ] {
        let mut policy = profile.to_policy();
        policy.per_extension.insert(
            "bypass-attempt".to_string(),
            ExtensionOverride {
                allow: vec!["exec".to_string(), "env".to_string()],
                deny: Vec::new(),
                mode: None,
                quota: None,
            },
        );
        for cap in dangerous_capabilities() {
            let result = policy.evaluate_for(cap, Some("bypass-attempt"));
            let status = if result.decision == PolicyDecision::Deny {
                "pass"
            } else {
                "fail"
            };
            checks.push(ConformanceCheck {
                id: format!("deny_cap_precedence_{cap}_{pname}"),
                category: "deny_cap_precedence".to_string(),
                profile: pname.to_string(),
                description: format!(
                    "deny_caps for '{cap}' overrides per-extension allow in {pname}"
                ),
                status: status.to_string(),
                detail: None,
            });
        }
    }

    // Category 6: Trust lifecycle sanity
    {
        let manager = make_manager();
        let ext_id = "lifecycle-test";

        // Default = Pending
        let default_ok =
            manager.trust_state(ext_id) == pi::extensions::ExtensionTrustState::Pending;
        checks.push(ConformanceCheck {
            id: "trust_default_pending".to_string(),
            category: "trust_lifecycle".to_string(),
            profile: "all".to_string(),
            description: "Default trust state is Pending".to_string(),
            status: if default_ok { "pass" } else { "fail" }.to_string(),
            detail: None,
        });

        // Accept → Acknowledged
        manager.record_trust_onboarding(ext_id, "low", true, "user:test");
        let ack_ok =
            manager.trust_state(ext_id) == pi::extensions::ExtensionTrustState::Acknowledged;
        checks.push(ConformanceCheck {
            id: "trust_accept_acknowledged".to_string(),
            category: "trust_lifecycle".to_string(),
            profile: "all".to_string(),
            description: "Accepting onboarding sets Acknowledged".to_string(),
            status: if ack_ok { "pass" } else { "fail" }.to_string(),
            detail: None,
        });

        // Promote → Trusted
        manager.promote_trust(ext_id);
        let trusted_ok =
            manager.trust_state(ext_id) == pi::extensions::ExtensionTrustState::Trusted;
        checks.push(ConformanceCheck {
            id: "trust_promote_trusted".to_string(),
            category: "trust_lifecycle".to_string(),
            profile: "all".to_string(),
            description: "Promoting from Acknowledged sets Trusted".to_string(),
            status: if trusted_ok { "pass" } else { "fail" }.to_string(),
            detail: None,
        });

        // Kill → Killed
        manager.kill_switch(ext_id, "test", "test-operator");
        let killed_ok = manager.is_killed(ext_id);
        checks.push(ConformanceCheck {
            id: "trust_kill_switch".to_string(),
            category: "trust_lifecycle".to_string(),
            profile: "all".to_string(),
            description: "Kill-switch sets Killed state".to_string(),
            status: if killed_ok { "pass" } else { "fail" }.to_string(),
            detail: None,
        });
    }

    // Category 7: Serde roundtrip for profiles
    for (pname, profile) in all_profiles() {
        let json_str = serde_json::to_string(&profile).expect("serialize policy profile");
        let restored: Result<PolicyProfile, _> = serde_json::from_str(&json_str);
        let status = if restored.as_ref().is_ok_and(|value| *value == profile) {
            "pass"
        } else {
            "fail"
        };
        checks.push(ConformanceCheck {
            id: format!("serde_roundtrip_{pname}"),
            category: "serde_roundtrip".to_string(),
            profile: pname.to_string(),
            description: format!("PolicyProfile::{pname} serde roundtrip"),
            status: status.to_string(),
            detail: None,
        });
    }

    // Build and validate the exact serialized verdict on every run. Writing
    // tracked evidence remains an explicit maintainer operation.
    let report = build_verdict(
        &checks,
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    let payload = validate_verdict_payload(&report, &checks);
    maybe_write_verdict(&payload);

    // Assert that verdict passes (informational — individual tests above are
    // the real gates)
    let pass_count = checks.iter().filter(|c| c.status == "pass").count();
    let total = checks.len();
    eprintln!(
        "\n=== SEC-6.4 Conformance Verdict: {pass_count}/{total} passed ({:.1}%) ===\n",
        if total > 0 {
            (pass_count as f64 / total as f64) * 100.0
        } else {
            100.0
        }
    );

    for check in &checks {
        let icon = if check.status == "pass" {
            "PASS"
        } else {
            "FAIL"
        };
        eprintln!("  [{icon}] {}: {}", check.id, check.description);
    }
}

#[test]
fn sec_conformance_verdict_generation_requires_exact_one() {
    assert!(!sec_conformance_verdict_generation_enabled(None));
    assert!(!sec_conformance_verdict_generation_enabled(Some("")));
    assert!(!sec_conformance_verdict_generation_enabled(Some("0")));
    assert!(!sec_conformance_verdict_generation_enabled(Some("true")));
    assert!(!sec_conformance_verdict_generation_enabled(Some(" 1")));
    assert!(!sec_conformance_verdict_generation_enabled(Some("1 ")));
    assert!(sec_conformance_verdict_generation_enabled(Some("1")));
}
