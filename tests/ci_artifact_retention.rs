#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_precision_loss)]

//! CI Artifact Retention + Log Triage Workflow (bd-3uqg.8.11).
//!
//! Validates:
//! 1. CI workflow captures provider test artifacts with explicit retention policy.
//! 2. E2E runner generates structured failure diagnostics and triage output.
//! 3. Failure digests include replay commands and root cause classification.
//! 4. Artifact retention policy is documented and enforced.
//!
//! Run:
//! ```bash
//! cargo test --test ci_artifact_retention -- --nocapture
//! ```
//!
//! Regenerate the tracked summary explicitly with:
//! ```bash
//! PI_GENERATE_ARTIFACT_RETENTION_REPORT=1 \
//!   cargo test --test ci_artifact_retention artifact_retention_summary_report -- --exact --nocapture
//! ```

use serde_json::Value;
use std::path::{Path, PathBuf};

const GENERATE_ARTIFACT_RETENTION_REPORT_ENV: &str = "PI_GENERATE_ARTIFACT_RETENTION_REPORT";

fn artifact_retention_report_generation_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn artifact_retention_report_generation_requested() -> bool {
    artifact_retention_report_generation_enabled(
        std::env::var(GENERATE_ARTIFACT_RETENTION_REPORT_ENV)
            .ok()
            .as_deref(),
    )
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn load_json(path: &Path) -> Option<Value> {
    let text = load_text(path)?;
    serde_json::from_str(&text).ok()
}

// ═══════════════════════════════════════════════════════════════════════
// CI workflow artifact retention validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn ci_workflow_has_explicit_retention_policy() {
    let root = repo_root();
    let ci_path = root.join(".github/workflows/ci.yml");
    let content = load_text(&ci_path).expect("CI workflow must exist");

    // Verify conformance reports have retention-days
    assert!(
        content.contains("name: conformance-reports"),
        "CI must upload conformance-reports artifact"
    );

    // Verify provider test artifacts are captured
    assert!(
        content.contains("name: provider-test-artifacts"),
        "CI must upload provider-test-artifacts"
    );

    // Verify shard artifacts have retention-days
    assert!(
        content.contains("retention-days: 30"),
        "CI must specify retention-days for artifacts"
    );

    // Verify full_suite_gate artifacts are captured
    assert!(
        content.contains("full_suite_gate") || content.contains("full-suite-gate"),
        "CI must upload full-suite-gate artifacts"
    );

    // Verify e2e_results are captured
    assert!(
        content.contains("e2e_results"),
        "CI must upload e2e_results artifacts"
    );

    eprintln!("[OK] CI workflow has explicit retention policy");
}

#[test]
fn ci_workflow_captures_provider_failure_artifacts() {
    let root = repo_root();
    let ci_path = root.join(".github/workflows/ci.yml");
    let content = load_text(&ci_path).expect("CI workflow must exist");

    // Provider test failure artifacts
    let required_patterns = [
        "tests/e2e_results/**/*.json",
        "tests/e2e_results/**/*.jsonl",
        "tests/e2e_results/**/*.log",
    ];

    for pattern in &required_patterns {
        assert!(
            content.contains(pattern),
            "CI must capture {pattern} for provider failure triage"
        );
    }

    eprintln!("[OK] CI workflow captures provider failure artifacts");
}

// ═══════════════════════════════════════════════════════════════════════
// Failure digest schema validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn failure_digest_schema_has_required_fields() {
    // Validate that the failure_digest generation produces the expected schema
    // by checking the run_all.sh script contains the required fields
    let root = repo_root();
    let script_path = root.join("scripts/e2e/run_all.sh");
    let content = load_text(&script_path).expect("run_all.sh must exist");

    let required_digest_fields = [
        "schema",
        "suite",
        "root_cause_class",
        "impacted_scenario_ids",
        "first_failing_assertion",
        "remediation_pointer",
    ];

    for field in &required_digest_fields {
        assert!(
            content.contains(field),
            "run_all.sh must generate failure_digest with field: {field}"
        );
    }

    // Verify remediation pointer includes replay commands
    let replay_fields = [
        "replay_command",
        "suite_replay_command",
        "targeted_test_replay_command",
    ];

    for field in &replay_fields {
        assert!(
            content.contains(field),
            "failure_digest remediation_pointer must include: {field}"
        );
    }

    eprintln!("[OK] Failure digest schema has required fields");
}

#[test]
fn failure_digest_includes_root_cause_classification() {
    let root = repo_root();
    let script_path = root.join("scripts/e2e/run_all.sh");
    let content = load_text(&script_path).expect("run_all.sh must exist");

    // Root cause classification taxonomy
    let root_causes = [
        "timeout",
        "assertion_failure",
        "permission_denied",
        "network_io",
        "missing_file",
        "panic",
    ];

    for cause in &root_causes {
        assert!(
            content.contains(cause),
            "run_all.sh must classify root cause: {cause}"
        );
    }

    eprintln!("[OK] Failure digest includes root cause classification");
}

// ═══════════════════════════════════════════════════════════════════════
// Replay bundle validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn replay_bundle_artifact_exists_and_valid() {
    let root = repo_root();
    let bundle_path = root.join("tests/full_suite_gate/replay_bundle.json");

    // replay_bundle may not exist if the full runner hasn't been executed,
    // but if it does, validate its schema
    if let Some(bundle) = load_json(&bundle_path) {
        assert!(
            bundle.get("schema").is_some(),
            "replay_bundle must have schema field"
        );
        assert!(
            bundle.get("environment").is_some(),
            "replay_bundle must have environment context"
        );
        assert!(
            bundle.get("one_command_replay").is_some(),
            "replay_bundle must have one_command_replay"
        );
        eprintln!("[OK] replay_bundle.json exists and has valid schema");
    } else {
        eprintln!("[SKIP] replay_bundle.json not present (generated during full E2E run)");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Evidence contract validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn evidence_contract_schema_enforced_in_runner() {
    let root = repo_root();
    let script_path = root.join("scripts/e2e/run_all.sh");
    let content = load_text(&script_path).expect("run_all.sh must exist");

    // Evidence contract must be generated
    assert!(
        content.contains("evidence_contract"),
        "run_all.sh must generate evidence_contract"
    );

    // Must include environment manifest
    assert!(
        content.contains("environment.json"),
        "run_all.sh must generate environment.json"
    );

    // Must include artifact-index
    assert!(
        content.contains("artifact-index") || content.contains("artifact_index"),
        "run_all.sh must generate artifact-index"
    );

    // Must include failure diagnostics
    assert!(
        content.contains("failure_diagnostics") || content.contains("failure_digest"),
        "run_all.sh must generate failure diagnostics"
    );

    eprintln!("[OK] Evidence contract schema enforced in runner");
}

#[test]
fn conformance_summary_lineage_contract_enforced_in_runner() {
    let root = repo_root();
    let script_path = root.join("scripts/e2e/run_all.sh");
    let content = load_text(&script_path).expect("run_all.sh must exist");

    let required_tokens = [
        "conformance.summary_run_id_nonempty",
        "conformance.summary_correlation_id_nonempty",
        "conformance.summary_correlation_id_matches_summary",
        "Emit conformance_summary.run_id from the latest canonical consolidated",
    ];

    for token in &required_tokens {
        assert!(
            content.contains(token),
            "run_all.sh must enforce conformance lineage token: {token}"
        );
    }

    eprintln!("[OK] Conformance summary lineage contract enforced in runner");
}

// ═══════════════════════════════════════════════════════════════════════
// Triage workflow documentation validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn triage_workflow_documented_in_runbooks() {
    let root = repo_root();

    // QA runbook must document replay workflow
    let qa_runbook = root.join("docs/qa-runbook.md");
    let qa_content = load_text(&qa_runbook).expect("qa-runbook.md must exist");

    assert!(
        qa_content.contains("Replay Workflow"),
        "QA runbook must document replay workflow"
    );
    assert!(
        qa_content.contains("replay_bundle"),
        "QA runbook must reference replay_bundle artifact"
    );
    assert!(
        qa_content.contains("failure_digest"),
        "QA runbook must reference failure_digest"
    );

    // CI operator runbook must have triage guidance
    let ci_runbook = root.join("docs/ci-operator-runbook.md");
    let ci_content = load_text(&ci_runbook).expect("ci-operator-runbook.md must exist");

    assert!(
        ci_content.contains("Failure Signature"),
        "CI runbook must document failure signatures"
    );
    assert!(
        ci_content.contains("Evidence Artifact"),
        "CI runbook must document evidence artifacts"
    );

    eprintln!("[OK] Triage workflow documented in runbooks");
}

#[test]
fn triage_workflow_covers_provider_failure_patterns() {
    let root = repo_root();

    // CI operator runbook should cover provider-specific failure patterns
    let ci_runbook = root.join("docs/ci-operator-runbook.md");
    let ci_content = load_text(&ci_runbook).expect("ci-operator-runbook.md must exist");

    let provider_patterns = ["provider_streaming", "VCR", "cassette"];

    for pattern in &provider_patterns {
        assert!(
            ci_content.contains(pattern),
            "CI runbook must cover provider failure pattern: {pattern}"
        );
    }

    // Auth troubleshooting doc should exist
    let auth_doc = root.join("docs/provider-auth-troubleshooting.md");
    assert!(
        auth_doc.exists(),
        "provider-auth-troubleshooting.md must exist for triage workflow"
    );

    eprintln!("[OK] Triage workflow covers provider failure patterns");
}

// ═══════════════════════════════════════════════════════════════════════
// Artifact retention policy document
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn artifact_retention_policy_consistent() {
    let root = repo_root();
    let ci_path = root.join(".github/workflows/ci.yml");
    let content = load_text(&ci_path).expect("CI workflow must exist");

    // Count retention-days occurrences — should be consistent
    let retention_count = content.matches("retention-days: 30").count();
    assert!(
        retention_count >= 2,
        "CI must have at least 2 explicit retention-days: 30 (shards + conformance), got {retention_count}"
    );

    // Verify if-no-files-found is set for all upload steps
    let upload_count = content.matches("actions/upload-artifact").count();
    let if_no_files_count = content.matches("if-no-files-found:").count();
    // Most uploads should have if-no-files-found
    assert!(
        if_no_files_count >= upload_count.saturating_sub(2),
        "Most artifact uploads should specify if-no-files-found policy"
    );

    eprintln!(
        "[OK] Artifact retention policy consistent ({retention_count} explicit retention-days)"
    );
}

#[test]
fn provider_test_infrastructure_produces_structured_output() {
    let root = repo_root();

    // Verify provider E2E test files exist
    let provider_test_files = [
        "tests/e2e_provider_scenarios.rs",
        "tests/e2e_provider_failure_injection.rs",
        "tests/e2e_cross_provider_parity.rs",
    ];

    for file in &provider_test_files {
        let path = root.join(file);
        assert!(path.exists(), "Provider test file must exist: {file}");
        let content = load_text(&path).unwrap_or_default();
        // Each test should use TestHarness for structured output
        assert!(
            content.contains("TestHarness"),
            "{file}: must use TestHarness for structured artifact output"
        );
    }

    // Verify failure injection tests produce JSONL artifacts
    let injection = root.join("tests/e2e_provider_failure_injection.rs");
    let content = load_text(&injection).expect("failure injection tests must exist");
    assert!(
        content.contains("write_results") || content.contains(".jsonl"),
        "Failure injection tests must produce JSONL artifact output"
    );
    assert!(
        content.contains("InjectionResult"),
        "Failure injection tests must use structured InjectionResult"
    );

    eprintln!("[OK] Provider test infrastructure produces structured output");
}

#[test]
fn artifact_retention_report_generation_requires_exact_one() {
    assert!(!artifact_retention_report_generation_enabled(None));
    assert!(!artifact_retention_report_generation_enabled(Some("")));
    assert!(!artifact_retention_report_generation_enabled(Some("0")));
    assert!(!artifact_retention_report_generation_enabled(Some("true")));
    assert!(!artifact_retention_report_generation_enabled(Some(" 1")));
    assert!(!artifact_retention_report_generation_enabled(Some("1 ")));
    assert!(artifact_retention_report_generation_enabled(Some("1")));
}

// ═══════════════════════════════════════════════════════════════════════
// Summary: Comprehensive artifact retention report
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn artifact_retention_summary_report() {
    let root = repo_root();
    let report_dir = root.join("tests/full_suite_gate");

    let ci_path = root.join(".github/workflows/ci.yml");
    let ci_content = load_text(&ci_path).unwrap_or_default();

    let upload_count = ci_content.matches("actions/upload-artifact").count();
    let retention_30d = ci_content.matches("retention-days: 30").count();
    let total_paths = ci_content.matches("pi_agent_rust/tests/").count();

    let report = serde_json::json!({
        "schema": "pi.ci.artifact_retention_report.v1",
        "bead": "bd-3uqg.8.11",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "ci_workflow": {
            "artifact_upload_steps": upload_count,
            "explicit_retention_30d": retention_30d,
            "test_path_references": total_paths,
        },
        "artifact_categories": [
            {
                "name": "conformance-reports",
                "retention_days": 30,
                "includes": ["ext_conformance/**", "e2e_results/**", "quarantine_*"],
            },
            {
                "name": "provider-test-artifacts",
                "retention_days": 30,
                "includes": ["full_suite_gate/**", "cross_platform_reports/**"],
            },
            {
                "name": "ci-shard-*",
                "retention_days": 30,
                "includes": ["e2e_results/ci-shards/<shard>/"],
            },
            {
                "name": "coverage",
                "retention_days": 90,
                "includes": ["llvm-cov-summary.txt", "lcov.info", "llvm-cov/html"],
            },
        ],
        "triage_artifacts": {
            "failure_digest": "tests/e2e_results/<ts>/<suite>/failure_digest.json",
            "failure_timeline": "tests/e2e_results/<ts>/<suite>/failure_timeline.jsonl",
            "replay_bundle": "tests/full_suite_gate/replay_bundle.json",
            "evidence_contract": "tests/e2e_results/<ts>/evidence_contract.json",
        },
        "documentation": {
            "qa_runbook": "docs/qa-runbook.md",
            "ci_operator_runbook": "docs/ci-operator-runbook.md",
            "auth_troubleshooting": "docs/provider-auth-troubleshooting.md",
            "testing_policy": "docs/testing-policy.md",
        },
    });

    let report_path = report_dir.join("artifact_retention_report.json");
    let report_json =
        serde_json::to_string_pretty(&report).expect("serialize artifact retention report");
    let generate = artifact_retention_report_generation_requested();
    if generate {
        std::fs::create_dir_all(&report_dir).expect("create artifact retention report directory");
        std::fs::write(&report_path, report_json).expect("write artifact retention report");
    } else {
        eprintln!(
            "  Report not written; set {GENERATE_ARTIFACT_RETENTION_REPORT_ENV}=1 to regenerate the tracked artifact"
        );
    }

    eprintln!("\n=== Artifact Retention Report ===");
    eprintln!("  Upload steps: {upload_count}");
    eprintln!("  Explicit 30d retention: {retention_30d}");
    eprintln!("  Test path references: {total_paths}");
    eprintln!(
        "  Report: {} ({})",
        report_path.display(),
        if generate { "generated" } else { "not written" }
    );
    eprintln!();

    assert!(
        upload_count >= 3,
        "CI must have at least 3 artifact upload steps, got {upload_count}"
    );
}
