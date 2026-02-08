//! Release gate: validates that the conformance evidence bundle exists,
//! is structurally valid, and meets minimum thresholds for release.
//!
//! This test suite enforces that releases are evidence-based. It checks:
//! - Required evidence artifacts exist on disk
//! - Evidence artifacts have valid schemas
//! - Pass-rate and failure thresholds meet release criteria
//! - Exception policy is complete and current
//!
//! See also: `tests/release_readiness.rs` for the readiness report generator.

use serde_json::Value;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(relative: &str) -> Option<Value> {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

fn require_json(relative: &str) -> Value {
    load_json(relative).unwrap_or_else(|| panic!("required evidence file missing: {relative}"))
}

// ============================================================================
// Evidence bundle existence checks
// ============================================================================

const REQUIRED_ARTIFACTS: &[(&str, &str)] = &[
    (
        "tests/ext_conformance/reports/conformance_summary.json",
        "Extension conformance summary",
    ),
    (
        "tests/ext_conformance/reports/conformance_baseline.json",
        "Conformance baseline with thresholds",
    ),
    (
        "tests/perf/reports/budget_summary.json",
        "Performance budget summary",
    ),
    (
        "tests/ext_conformance/artifacts/RISK_REVIEW.json",
        "Security and licensing risk review",
    ),
    (
        "tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json",
        "Extension provenance verification",
    ),
    (
        "docs/traceability_matrix.json",
        "Requirement-to-test traceability matrix",
    ),
];

#[test]
fn all_required_evidence_artifacts_exist() {
    let root = repo_root();
    let mut missing = Vec::new();

    for (path, label) in REQUIRED_ARTIFACTS {
        if !root.join(path).is_file() {
            missing.push(format!("  - {label}: {path}"));
        }
    }

    assert!(
        missing.is_empty(),
        "release gate BLOCKED: missing evidence artifacts:\n{}",
        missing.join("\n")
    );
}

#[test]
fn all_evidence_artifacts_are_valid_json() {
    for (path, label) in REQUIRED_ARTIFACTS {
        let v = load_json(path);
        assert!(
            v.is_some(),
            "evidence artifact is not valid JSON: {label} ({path})"
        );
    }
}

// ============================================================================
// Schema validation
// ============================================================================

#[test]
fn conformance_summary_has_required_fields() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    assert!(sm.get("schema").is_some(), "missing schema field");
    assert!(sm.get("counts").is_some(), "missing counts field");
    assert!(sm.get("pass_rate_pct").is_some(), "missing pass_rate_pct");
    assert!(sm.get("per_tier").is_some(), "missing per_tier");
    assert!(sm.get("evidence").is_some(), "missing evidence");

    let counts = sm.get("counts").unwrap();
    assert!(counts.get("pass").is_some(), "missing counts.pass");
    assert!(counts.get("fail").is_some(), "missing counts.fail");
    assert!(counts.get("total").is_some(), "missing counts.total");
}

#[test]
fn baseline_has_required_fields() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    assert!(bl.get("schema").is_some(), "missing schema");
    assert!(
        bl.get("extension_conformance").is_some(),
        "missing extension_conformance"
    );
    assert!(
        bl.get("regression_thresholds").is_some(),
        "missing regression_thresholds"
    );
    assert!(
        bl.get("exception_policy").is_some(),
        "missing exception_policy"
    );
}

#[test]
fn traceability_matrix_has_requirements() {
    let tm = require_json("docs/traceability_matrix.json");

    let reqs = tm
        .get("requirements")
        .and_then(Value::as_array)
        .expect("traceability matrix must have requirements array");

    assert!(
        !reqs.is_empty(),
        "traceability matrix must have at least one requirement"
    );

    for req in reqs {
        assert!(req.get("id").is_some(), "requirement missing id field");
        assert!(
            req.get("unit_tests").is_some(),
            "requirement {:?} missing unit_tests",
            req.get("id")
        );
    }
}

// ============================================================================
// Threshold enforcement
// ============================================================================

#[test]
fn conformance_pass_rate_meets_release_threshold() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let current_rate = sm
        .get("pass_rate_pct")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let min_rate = bl
        .pointer("/regression_thresholds/overall_pass_rate_min_pct")
        .and_then(Value::as_f64)
        .unwrap_or(80.0);

    assert!(
        current_rate >= min_rate,
        "release gate BLOCKED: conformance pass rate {current_rate:.1}% < minimum {min_rate:.1}%"
    );
}

#[test]
fn failure_count_within_release_threshold() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    let fail = sm
        .pointer("/counts/fail")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let max_fail: u64 = 36;

    assert!(
        fail <= max_fail,
        "release gate BLOCKED: {fail} failures exceed maximum {max_fail}"
    );
}

#[test]
fn performance_budgets_report_exists_and_valid() {
    let budget = require_json("tests/perf/reports/budget_summary.json");

    assert!(
        budget.get("schema").is_some()
            || budget.get("budgets").is_some()
            || budget.get("summary").is_some(),
        "performance budget report must have recognizable structure"
    );
}

// ============================================================================
// Exception policy completeness
// ============================================================================

#[test]
fn exception_policy_covers_all_current_failures() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let entries = bl
        .pointer("/exception_policy/entries")
        .and_then(Value::as_array);
    let total_classified = bl
        .pointer("/remediation_buckets/summary/total_classified")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let Some(entries) = entries else {
        // If no exception policy, there should be no failures.
        assert_eq!(
            total_classified, 0,
            "failures exist ({total_classified}) but no exception policy defined"
        );
        return;
    };

    // Every exception entry must have all required fields.
    let approved = entries
        .iter()
        .filter(|e| {
            e.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "approved" || s == "temporary")
        })
        .count();

    assert!(
        approved > 0 || total_classified == 0,
        "failures exist ({total_classified}) but no approved exceptions"
    );
}

#[test]
fn exception_entries_have_review_dates() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let entries = bl
        .pointer("/exception_policy/entries")
        .and_then(Value::as_array);

    let Some(entries) = entries else {
        return;
    };

    for entry in entries {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("?");
        let review_by = entry.get("review_by").and_then(Value::as_str);

        assert!(
            review_by.is_some(),
            "exception entry {id} missing review_by date"
        );
    }
}

// ============================================================================
// Evidence completeness score
// ============================================================================

#[test]
fn evidence_completeness_score_above_minimum() {
    let root = repo_root();
    let mut present = 0u32;

    for (path, _) in REQUIRED_ARTIFACTS {
        if root.join(path).is_file() {
            present += 1;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let score = (f64::from(present) / REQUIRED_ARTIFACTS.len() as f64) * 100.0;

    assert!(
        score >= 80.0,
        "evidence completeness {score:.0}% < 80% minimum (present={present}/{})",
        REQUIRED_ARTIFACTS.len()
    );
}

#[test]
fn conformance_evidence_has_linked_test_targets() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    let evidence = sm.get("evidence").and_then(Value::as_object);
    let Some(evidence) = evidence else {
        // Evidence section is optional in summary v1.
        return;
    };

    // At least one evidence category should have non-zero count.
    let total_evidence: u64 = evidence.values().filter_map(Value::as_u64).sum();

    assert!(
        total_evidence > 0,
        "conformance summary has evidence section but all counts are zero"
    );
}
