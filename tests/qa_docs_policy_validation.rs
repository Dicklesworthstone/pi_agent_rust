//! QA Docs: Testing policy, operator runbooks, and triage playbook validation (bd-1f42.8.9).
//!
//! Validates that documentation artifacts match enforced behavior and evidence formats:
//! 1. testing-policy.md allowlist table integrity (owner, expiry, replacement plan)
//! 2. non-mock-rubric.json alignment with enforced thresholds/gates
//! 3. qa-runbook.md and flake-triage-policy.md for replay, triage, evidence contract
//! 4. Operator troubleshooting runbook: failure signatures → replay commands + artifact paths
//! 5. Every CI gate references documented remediation steps
//! 6. Documentation examples are command-valid and artifact-path accurate
//! 7. Stale/expired exceptions flagged with follow-up actions
//!
//! Run:
//! ```bash
//! cargo test --test qa_docs_policy_validation
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::items_after_statements)]

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// ─── Constants ──────────────────────────────────────────────────────────────

const TESTING_POLICY_PATH: &str = "docs/testing-policy.md";
const PLAN_TO_COMPLETE_PORT_PATH: &str = "docs/planning/PLAN_TO_COMPLETE_PORT.md";
const PROGRAM_GOVERNANCE_PATH: &str = "docs/program-governance.md";
const NON_MOCK_RUBRIC_PATH: &str = "docs/non-mock-rubric.json";
const QA_RUNBOOK_PATH: &str = "docs/qa-runbook.md";
const CI_OPERATOR_RUNBOOK_PATH: &str = "docs/ci-operator-runbook.md";
const FLAKE_TRIAGE_PATH: &str = "docs/flake-triage-policy.md";
const SCENARIO_MATRIX_PATH: &str = "docs/e2e_scenario_matrix.json";
const PERF_SLI_MATRIX_PATH: &str = "docs/perf_sli_matrix.json";
const FRANKEN_NODE_MISSION_CONTRACT_PATH: &str = "docs/franken-node-mission-contract.json";
const DROPIN_CERTIFICATION_CONTRACT_PATH: &str =
    "docs/contracts/dropin-certification-contract.json";
const INTEGRATOR_MIGRATION_PLAYBOOK_PATH: &str = "docs/integrator-migration-playbook.md";
const MAINTENANCE_DASHBOARD_SCRIPT_PATH: &str = "scripts/generate_maintenance_dashboard.py";
const RECONCILE_BEADS_LEDGER_SCRIPT_PATH: &str = "scripts/reconcile_beads_ledger.sh";
const DROPIN_RPC_SURFACE_DIFF_PATH: &str = "docs/dropin-rpc-surface-diff.json";
const DROPIN_PARITY_GAP_LEDGER_PATH: &str = "docs/evidence/dropin-parity-gap-ledger.json";
const CI_WORKFLOW_PATH: &str = ".github/workflows/ci.yml";
const WEEKLY_CERTIFICATION_VERDICT_WORKFLOW_PATH: &str =
    ".github/workflows/weekly-certification-verdict.yml";
const SUITE_CLASSIFICATION_PATH: &str = "tests/suite_classification.toml";
const TEST_DOUBLE_INVENTORY_PATH: &str = "docs/test_double_inventory.json";
const FULL_SUITE_GATE_PATH: &str = "tests/ci_full_suite_gate.rs";
const COVERAGE_BASELINE_PATH: &str = "docs/coverage-baseline-map.json";
const RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_PATH: &str = "docs/schema/runtime_hostcall_telemetry.json";
const SWARM_OPERATIONS_RUNBOOK_PATH: &str = "docs/swarm-operations-runbook.md";

fn load_json(path: &str) -> Value {
    let content = std::fs::read_to_string(path).expect("should read JSON fixture");
    serde_json::from_str(&content).expect("should parse fixture as JSON")
}

fn load_text(path: &str) -> String {
    std::fs::read_to_string(path).expect("should read text fixture")
}

fn parse_f64_literal_after(haystack: &str, needle: &str) -> Option<f64> {
    let start = haystack.find(needle)? + needle.len();
    let literal: String = haystack[start..]
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    if literal.is_empty() {
        return None;
    }
    literal.parse::<f64>().ok()
}

fn parse_identifier_after(haystack: &str, needle: &str) -> Option<String> {
    let start = haystack.find(needle)? + needle.len();
    let token: String = haystack[start..]
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
        .collect();
    if token.is_empty() {
        return None;
    }
    token.rsplit("::").next().map(ToString::to_string)
}

fn resolve_f64_constant_in_source(source: &str, constant_name: &str) -> Option<f64> {
    source
        .lines()
        .map(str::trim)
        .find(|line| {
            line.contains(constant_name)
                && line.contains(": f64")
                && (line.starts_with("const ")
                    || line.starts_with("pub const ")
                    || line.starts_with("pub(crate) const "))
        })
        .and_then(|line| parse_f64_literal_after(line, "="))
}

fn parse_f64_or_resolved_const_after(
    haystack: &str,
    needle: &str,
    local_source: &str,
    fallback_sources: &[&str],
) -> Option<f64> {
    if let Some(value) = parse_f64_literal_after(haystack, needle) {
        return Some(value);
    }
    let constant_name = parse_identifier_after(haystack, needle)?;
    if let Some(value) = resolve_f64_constant_in_source(local_source, &constant_name) {
        return Some(value);
    }
    for path in fallback_sources {
        let source = load_text(path);
        if let Some(value) = resolve_f64_constant_in_source(&source, &constant_name) {
            return Some(value);
        }
    }
    None
}

fn binary_size_threshold_from_perf_budgets_source() -> f64 {
    let source = load_text("tests/perf_budgets.rs");
    let anchor = source
        .find("name: \"binary_size_release\"")
        .expect("perf_budgets.rs must define binary_size_release budget");
    parse_f64_or_resolved_const_after(
        &source[anchor..],
        "threshold:",
        &source,
        &["src/perf_build.rs"],
    )
    .expect("binary_size_release budget must define a numeric threshold")
}

fn binary_size_threshold_from_perf_regression_source() -> f64 {
    let source = load_text("tests/perf_regression.rs");
    let anchor = source
        .find("fn binary_size_check")
        .expect("perf_regression.rs must define binary_size_check");
    parse_f64_or_resolved_const_after(
        &source[anchor..],
        "let threshold =",
        &source,
        &["src/perf_build.rs"],
    )
    .expect("binary_size_check must define a numeric threshold")
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 1: Testing policy document structure and completeness
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn testing_policy_defines_all_three_suites() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("Suite 1: Unit"),
        "must define Suite 1 (Unit)"
    );
    assert!(
        policy.contains("Suite 2: VCR"),
        "must define Suite 2 (VCR / Fixture Replay)"
    );
    assert!(
        policy.contains("Suite 3: Live E2E"),
        "must define Suite 3 (Live E2E)"
    );
}

#[test]
fn testing_policy_has_allowlist_table_with_required_columns() {
    let policy = load_text(TESTING_POLICY_PATH);
    // The allowlist table must have these column headers
    assert!(
        policy.contains("| Identifier |"),
        "allowlist table must have Identifier column"
    );
    assert!(
        policy.contains("| Location |") || policy.contains("Location |"),
        "allowlist table must have Location column"
    );
    assert!(
        policy.contains("Suite |"),
        "allowlist table must have Suite column"
    );
    assert!(
        policy.contains("Rationale |"),
        "allowlist table must have Rationale column"
    );
}

#[test]
fn testing_policy_allowlist_entries_reference_real_files() {
    let policy = load_text(TESTING_POLICY_PATH);
    // Each allowlisted exception should reference a real file path
    let known_locations = [
        "tests/common/harness.rs",
        "tests/e2e_cli.rs",
        "src/extensions.rs",
    ];
    for loc in &known_locations {
        assert!(
            policy.contains(loc),
            "allowlist must reference real file: {loc}"
        );
    }
    // Verify those files actually exist
    for loc in &known_locations {
        assert!(
            std::path::Path::new(loc).exists(),
            "allowlisted file must exist on disk: {loc}"
        );
    }
}

#[test]
fn testing_policy_has_exception_template_with_mandatory_fields() {
    let policy = load_text(TESTING_POLICY_PATH);
    let mandatory_fields = [
        "bead_id",
        "owner",
        "expires_at",
        "replacement_plan",
        "scope",
        "verification",
    ];
    for field in &mandatory_fields {
        assert!(
            policy.contains(field),
            "exception template must include mandatory field: {field}"
        );
    }
}

#[test]
fn testing_policy_defines_ci_enforcement_guards() {
    let policy = load_text(TESTING_POLICY_PATH);
    let guards = [
        "No-mock dependency guard",
        "No-mock code guard",
        "Suite classification guard",
        "VCR leak guard",
    ];
    for guard in &guards {
        assert!(
            policy.contains(guard),
            "testing-policy must document CI guard: {guard}"
        );
    }
}

#[test]
fn testing_policy_has_migration_checklist() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("Migration Checklist"),
        "must include migration checklist for suite transitions"
    );
}

#[test]
fn testing_policy_has_flaky_test_quarantine_section() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("Flaky-Test Quarantine"),
        "must include flaky-test quarantine section"
    );
    // Must define the 6 flake categories
    let categories = [
        "FLAKE-TIMING",
        "FLAKE-ENV",
        "FLAKE-NET",
        "FLAKE-RES",
        "FLAKE-EXT",
        "FLAKE-LOGIC",
    ];
    for cat in &categories {
        assert!(
            policy.contains(cat),
            "quarantine section must define category: {cat}"
        );
    }
}

#[test]
fn testing_policy_quarantine_has_9_required_fields() {
    let policy = load_text(TESTING_POLICY_PATH);
    let fields = [
        "category",
        "owner",
        "quarantined",
        "expires",
        "bead",
        "evidence",
        "repro",
        "reason",
        "remove_when",
    ];
    for field in &fields {
        assert!(
            policy.contains(field),
            "quarantine section must require field: {field}"
        );
    }
}

#[test]
fn testing_policy_defines_gate_promotion_runbook() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("CI Gate Promotion Runbook"),
        "must document CI gate promotion runbook"
    );
    assert!(
        policy.contains("CI_GATE_PROMOTION_MODE"),
        "must document promotion mode variable"
    );
    assert!(
        policy.contains("rollback"),
        "must document rollback procedure"
    );
}

#[test]
fn testing_policy_documents_practical_finish_docs_last_contract() {
    let policy = load_text(TESTING_POLICY_PATH);
    let required_tokens = [
        "Practical-finish checkpoint policy (bd-3ar8v.6.9)",
        "docs-last contract",
        "practical_finish_checkpoint",
        "parameter_sweeps_integrity",
        "extension_remediation_backlog",
        "docs/report scope",
        "technical open PERF-3X issue is",
    ];
    for token in &required_tokens {
        assert!(
            policy.contains(token),
            "testing-policy must retain practical-finish docs-last token: {token}"
        );
    }
}

#[test]
fn testing_policy_practical_finish_policy_references_required_artifacts() {
    let policy = load_text(TESTING_POLICY_PATH);
    let required_artifact_tokens = [
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "pi.perf3x.practical_finish_checkpoint.v1",
        "tests/perf/reports/parameter_sweeps.json",
        "pi.perf.parameter_sweeps.v1",
        "tests/full_suite_gate/extension_remediation_backlog.json",
        "pi.qa.extension_remediation_backlog.v1",
        "tests/ci_full_suite_gate.rs",
        "tests/release_readiness.rs",
    ];
    for token in &required_artifact_tokens {
        assert!(
            policy.contains(token),
            "testing-policy practical-finish policy must reference token: {token}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 2: Non-mock rubric alignment with CI gates
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn rubric_module_thresholds_match_runbook_table() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let runbook = load_text(QA_RUNBOOK_PATH);

    // module_thresholds.modules is an array of objects with "name" field
    let modules = rubric["module_thresholds"]["modules"]
        .as_array()
        .expect("module_thresholds.modules must be an array");

    // Every rubric module must appear in the qa-runbook coverage table
    for module in modules {
        let name = module["name"].as_str().unwrap_or("unknown");
        assert!(
            runbook.contains(name),
            "rubric module '{name}' must be documented in qa-runbook.md coverage table"
        );
    }
}

#[test]
fn rubric_has_floor_and_target_for_each_module() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let modules = rubric["module_thresholds"]["modules"]
        .as_array()
        .expect("module_thresholds.modules must be an array");

    for module in modules {
        let name = module["name"].as_str().unwrap_or("unknown");
        assert!(
            module["line_floor_pct"].is_number(),
            "module {name} must have numeric line_floor_pct"
        );
        assert!(
            module["line_target_pct"].is_number(),
            "module {name} must have numeric line_target_pct"
        );
        assert!(
            module["function_floor_pct"].is_number(),
            "module {name} must have numeric function_floor_pct"
        );
        assert!(
            module["function_target_pct"].is_number(),
            "module {name} must have numeric function_target_pct"
        );

        // Floor must be <= target
        let line_floor = module["line_floor_pct"].as_f64().unwrap();
        let line_target = module["line_target_pct"].as_f64().unwrap();
        assert!(
            line_floor <= line_target,
            "module {name}: line_floor_pct ({line_floor}) must be <= line_target_pct ({line_target})"
        );

        let fn_floor = module["function_floor_pct"].as_f64().unwrap();
        let fn_target = module["function_target_pct"].as_f64().unwrap();
        assert!(
            fn_floor <= fn_target,
            "module {name}: function_floor_pct ({fn_floor}) must be <= function_target_pct ({fn_target})"
        );
    }
}

#[test]
fn rubric_global_thresholds_are_consistent() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let global = &rubric["module_thresholds"]["global"];
    assert!(
        global.is_object(),
        "rubric must have module_thresholds.global"
    );

    let line_floor = global["line_floor_pct"]
        .as_f64()
        .expect("global line_floor_pct");
    let line_target = global["line_target_pct"]
        .as_f64()
        .expect("global line_target_pct");
    assert!(
        line_floor <= line_target,
        "global line_floor_pct must be <= line_target_pct"
    );
}

#[test]
fn rubric_critical_modules_have_highest_thresholds() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let modules = rubric["module_thresholds"]["modules"]
        .as_array()
        .expect("module_thresholds.modules");
    let global_floor = rubric["module_thresholds"]["global"]["line_floor_pct"]
        .as_f64()
        .unwrap_or(0.0);

    // Critical modules must have thresholds >= global floor
    let critical = ["providers", "extensions", "agent_loop", "tools"];
    for crit_name in &critical {
        if let Some(module) = modules
            .iter()
            .find(|m| m["name"].as_str() == Some(crit_name))
        {
            let line_floor = module["line_floor_pct"].as_f64().unwrap_or(0.0);
            assert!(
                line_floor >= global_floor,
                "critical module {crit_name} line_floor_pct ({line_floor}) must be >= global ({global_floor})"
            );
        }
    }
}

#[test]
fn rubric_exception_mechanism_documented() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let text = serde_json::to_string(&rubric).unwrap_or_default();
    assert!(
        text.contains("exception") || text.contains("allowlist") || text.contains("waiver"),
        "rubric must document an exception/waiver mechanism"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 3: QA runbook completeness
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn qa_runbook_exists_and_has_required_sections() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let sections = [
        "Quick Start",
        "Test Suite Classification",
        "Artifact Locations",
        "Failure Triage Playbook",
        "Replay Workflow",
        "Smoke Suite",
        "CI Gate Thresholds",
        "Per-Module Coverage Thresholds",
        "Quarantine Workflow",
    ];
    for section in &sections {
        assert!(
            runbook.contains(section),
            "qa-runbook.md must contain section: {section}"
        );
    }
}

#[test]
fn qa_runbook_artifact_paths_are_accurate() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    // Verify documented artifact paths reference real patterns
    let artifact_patterns = [
        "tests/smoke_results/",
        "tests/e2e_results/",
        "tests/ext_conformance/reports/",
        "docs/coverage-baseline-map.json",
        "docs/e2e_scenario_matrix.json",
        "tests/fixtures/vcr/",
        "target/test-failures.jsonl",
    ];
    for path in &artifact_patterns {
        assert!(
            runbook.contains(path),
            "runbook must document artifact path: {path}"
        );
    }
}

#[test]
fn qa_runbook_has_failure_signature_table() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    // The failure triage section must map signatures to actions
    let signatures = [
        "assertion failed",
        "missing Start event",
        "request URL mismatch",
        "connection refused",
        "DummyProvider",
    ];
    for sig in &signatures {
        assert!(
            runbook.contains(sig),
            "triage playbook must include failure signature: {sig}"
        );
    }
}

#[test]
fn qa_runbook_has_reproduction_commands() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    // Must include actual reproduction commands
    assert!(
        runbook.contains("cargo test --test"),
        "runbook must include cargo test reproduction commands"
    );
    assert!(
        runbook.contains("VCR_MODE=playback"),
        "runbook must include VCR playback command"
    );
    assert!(
        runbook.contains("RUST_LOG=debug"),
        "runbook must include debug logging command"
    );
    assert!(
        runbook.contains("RUST_BACKTRACE=1"),
        "runbook must include backtrace command"
    );
}

#[test]
fn qa_runbook_references_replay_commands() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("--rerun-from"),
        "runbook must document --rerun-from replay"
    );
    assert!(
        runbook.contains("--diff-from"),
        "runbook must document --diff-from comparison"
    );
}

#[test]
fn qa_runbook_coverage_table_matches_rubric_modules() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let modules = rubric["module_thresholds"]["modules"]
        .as_array()
        .expect("module_thresholds.modules must be an array");

    // Every rubric module should appear in the runbook coverage table
    for module in modules {
        let name = module["name"].as_str().unwrap_or("unknown");
        assert!(
            runbook.contains(name),
            "runbook coverage table must include rubric module: {name}"
        );
    }
}

#[test]
fn qa_runbook_documents_extension_failure_dossier() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("Extension Failure Dossier") || runbook.contains("conformance_summary"),
        "runbook must document extension failure dossier interpretation"
    );
}

#[test]
fn qa_runbook_contains_perf3x_regression_triage_contract() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let required_tokens = [
        "## PERF-3X Regression Triage (bd-3ar8v.6.4)",
        "practical_finish_checkpoint.json",
        "parameter_sweeps.json",
        "Perf diagnostics (budget/comparison/stress/parameter-sweeps event streams)",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "tests/full_suite_gate/extension_remediation_backlog.json",
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/reports/parameter_sweeps_events.jsonl",
    ];
    for token in &required_tokens {
        assert!(
            runbook.contains(token),
            "qa-runbook.md must retain PERF-3X triage token: {token}"
        );
    }
}

#[test]
fn qa_runbook_contains_perf3x_signature_quick_reference() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let required_tokens = [
        "### PERF-3X signature quick-reference",
        "First remediation action",
        "`parameter_sweeps_integrity`",
        "`practical_finish_checkpoint`",
        "tests/perf/reports/parameter_sweeps_events.jsonl",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact",
        "cargo test --test ci_full_suite_gate -- practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact",
    ];
    for token in &required_tokens {
        assert!(
            runbook.contains(token),
            "qa-runbook.md must retain PERF-3X signature quick-reference token: {token}"
        );
    }
}

#[test]
fn ci_operator_runbook_contains_perf3x_incident_signature_playbooks() {
    let runbook = load_text(CI_OPERATOR_RUNBOOK_PATH);
    let required_tokens = [
        "### PERF-3X Gate Incident Addendum (bd-3ar8v.6.4)",
        "### PERF-3X signature: `parameter_sweeps_integrity` gate failure",
        "### PERF-3X signature: `practical_finish_checkpoint` readiness drift",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/reports/parameter_sweeps_events.jsonl",
        "docs/qa-runbook.md",
        "PERF-3X Regression Triage (bd-3ar8v.6.4)",
    ];
    for token in &required_tokens {
        assert!(
            runbook.contains(token),
            "ci-operator-runbook.md must retain PERF-3X signature token: {token}"
        );
    }
}

#[test]
fn qa_runbook_contains_user_facing_diagnostics_workflow_tokens() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let required_tokens = [
        "### User-facing diagnostics workflow (durability/resume/extension/build-profile)",
        "#### Durability troubleshooting",
        "#### Resume troubleshooting",
        "#### Extension troubleshooting",
        "#### Build-profile troubleshooting",
        "cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact",
        "./scripts/e2e/run_all.sh --rerun-from <scenario-id> --diff-from <baseline-dir>",
        "tests/e2e_results/<ts>/<suite>/test-log.jsonl",
        "tests/full_suite_gate/extension_remediation_backlog.json",
        "tests/perf/reports/perf_comparison.json",
        "tests/perf/reports/perf_comparison_events.jsonl",
    ];
    for token in &required_tokens {
        assert!(
            runbook.contains(token),
            "qa-runbook.md must retain user-facing diagnostics workflow token: {token}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 4: Flake triage policy completeness
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn flake_triage_policy_exists_with_required_sections() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let sections = [
        "Failure Classification",
        "Known Flake Patterns",
        "Retry Policy",
        "Quarantine Contract",
        "Flake Budget",
        "Triage Workflow",
        "Evidence Artifacts",
    ];
    for section in &sections {
        assert!(
            policy.contains(section),
            "flake-triage-policy must contain section: {section}"
        );
    }
}

#[test]
fn flake_triage_has_three_failure_buckets() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let buckets = ["Deterministic", "Transient", "Environmental"];
    for bucket in &buckets {
        assert!(
            policy.contains(bucket),
            "flake triage must define failure bucket: {bucket}"
        );
    }
}

#[test]
fn flake_triage_has_known_flake_patterns_with_regex() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let patterns = [
        "oracle_timeout",
        "resource_exhaustion",
        "fs_contention",
        "port_conflict",
        "tmpdir_race",
        "js_gc_pressure",
    ];
    for pattern in &patterns {
        assert!(
            policy.contains(pattern),
            "flake triage must list known pattern: {pattern}"
        );
    }
}

#[test]
fn flake_triage_documents_retry_limits() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    assert!(
        policy.contains("Max retries") || policy.contains("max retries"),
        "must document max retry limit"
    );
    assert!(
        policy.contains("5 seconds") || policy.contains("Retry delay"),
        "must document retry delay"
    );
}

#[test]
fn flake_triage_documents_quarantine_required_fields() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let fields = [
        "category",
        "owner",
        "quarantined",
        "expires",
        "bead",
        "evidence",
        "repro",
        "reason",
        "remove_when",
    ];
    for field in &fields {
        assert!(
            policy.contains(field),
            "flake triage quarantine contract must list required field: {field}"
        );
    }
}

#[test]
fn flake_triage_has_configuration_variables() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let vars = [
        "PI_CONFORMANCE_MAX_RETRIES",
        "PI_CONFORMANCE_RETRY_DELAY",
        "PI_CONFORMANCE_FLAKE_BUDGET",
    ];
    for var in &vars {
        assert!(
            policy.contains(var),
            "flake triage must document config variable: {var}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 5: Cross-document consistency
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn testing_policy_references_inventory_and_key_artifacts() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("test_double_inventory.json"),
        "testing-policy must reference test_double_inventory.json"
    );
    assert!(
        policy.contains("suite_classification.toml"),
        "testing-policy must reference suite_classification.toml"
    );
    assert!(
        policy.contains("e2e_scenario_matrix.json"),
        "testing-policy must reference e2e_scenario_matrix.json"
    );
}

#[test]
fn qa_runbook_references_testing_policy() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("testing-policy.md"),
        "runbook must reference testing-policy.md"
    );
}

#[test]
fn qa_runbook_references_rubric() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("non-mock-rubric.json"),
        "runbook must reference non-mock-rubric.json"
    );
}

#[test]
fn qa_runbook_references_coverage_baseline() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("coverage-baseline-map.json"),
        "runbook must reference coverage-baseline-map.json"
    );
}

#[test]
fn ci_workflow_guard_names_match_testing_policy() {
    let ci = load_text(CI_WORKFLOW_PATH);
    let policy = load_text(TESTING_POLICY_PATH);

    // Guards documented in policy should be reflected in CI
    if policy.contains("Suite classification guard") {
        assert!(
            ci.contains("suite_classification") || ci.contains("suite-classification"),
            "CI must implement suite classification guard documented in policy"
        );
    }
    if policy.contains("No-mock dependency guard") {
        assert!(
            ci.contains("mockall") || ci.contains("mockito") || ci.contains("wiremock"),
            "CI must check for mock dependencies documented in policy"
        );
    }
}

#[test]
fn scenario_matrix_rows_have_replay_commands() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let rows = matrix["rows"].as_array().expect("rows array");

    for row in rows {
        let id = row["workflow_id"].as_str().unwrap_or("unknown");
        let replay = row["replay_command"].as_str().unwrap_or("");
        assert!(
            !replay.is_empty(),
            "workflow {id} must have a non-empty replay_command"
        );
        // Replay commands should reference run_all.sh
        assert!(
            replay.contains("run_all.sh"),
            "workflow {id} replay_command must reference scripts/e2e/run_all.sh"
        );
    }
}

#[test]
fn scenario_matrix_suites_match_suite_classification() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let classification = load_text(SUITE_CLASSIFICATION_PATH);
    let rows = matrix["rows"].as_array().expect("rows array");

    for row in rows {
        let id = row["workflow_id"].as_str().unwrap_or("unknown");
        let suite_ids = row["suite_ids"].as_array();
        if let Some(suite_ids) = suite_ids {
            for suite in suite_ids {
                let name = suite.as_str().unwrap_or("");
                if !name.is_empty() {
                    assert!(
                        classification.contains(name),
                        "scenario matrix workflow {id} references suite '{name}' not in suite_classification.toml"
                    );
                }
            }
        }
    }
}

#[test]
fn scenario_matrix_rows_define_non_empty_sli_ids() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let rows = matrix["rows"].as_array().expect("rows array");

    for row in rows {
        let id = row["workflow_id"].as_str().unwrap_or("unknown");
        let sli_ids = row["sli_ids"]
            .as_array()
            .expect("workflow row must define a sli_ids array");
        assert!(
            !sli_ids.is_empty(),
            "workflow {id} must include at least one SLI"
        );
        for sli_id in sli_ids {
            let sli = sli_id.as_str().unwrap_or("");
            assert!(
                !sli.trim().is_empty(),
                "workflow {id} contains an empty sli_id entry"
            );
        }
    }
}

#[test]
fn scenario_matrix_sli_ids_exist_in_perf_sli_catalog() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let rows = matrix["rows"].as_array().expect("rows array");
    let catalog = perf["sli_catalog"]
        .as_array()
        .expect("perf_sli_matrix sli_catalog array");

    let known_ids: HashSet<String> = catalog
        .iter()
        .filter_map(|entry| entry["sli_id"].as_str().map(ToOwned::to_owned))
        .collect();
    assert!(
        !known_ids.is_empty(),
        "perf_sli_matrix sli_catalog must define at least one sli_id"
    );
    let alias_map = perf["sli_aliases"].as_object().cloned().unwrap_or_default();

    for row in rows {
        let workflow_id = row["workflow_id"].as_str().unwrap_or("unknown");
        let sli_ids = row["sli_ids"]
            .as_array()
            .expect("workflow row must define sli_ids");
        for sli_id in sli_ids {
            let sli = sli_id.as_str().unwrap_or("");
            let alias_target = alias_map.get(sli).and_then(Value::as_str);
            assert!(
                known_ids.contains(sli)
                    || alias_target.is_some_and(|target| known_ids.contains(target)),
                "workflow {workflow_id} references unknown SLI id '{sli}' (and no valid alias target)"
            );
        }
    }
}

#[test]
fn perf_sli_catalog_entries_have_thresholds_and_user_guidance() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let catalog = perf["sli_catalog"]
        .as_array()
        .expect("perf_sli_matrix sli_catalog array");

    for entry in catalog {
        let sli_id = entry["sli_id"].as_str().unwrap_or("<unknown>");
        let thresholds = &entry["thresholds"];
        assert!(
            thresholds["target"].is_number(),
            "{sli_id} must define numeric thresholds.target"
        );
        assert!(
            thresholds["warning"].is_number(),
            "{sli_id} must define numeric thresholds.warning"
        );
        assert!(
            thresholds["fail"].is_number(),
            "{sli_id} must define numeric thresholds.fail"
        );

        let interpretation = &entry["user_interpretation"];
        for key in ["target", "warning", "fail"] {
            let value = interpretation[key].as_str().unwrap_or("");
            assert!(
                !value.trim().is_empty(),
                "{sli_id} must define non-empty user_interpretation.{key}"
            );
        }
    }
}

#[test]
fn perf_sli_workflow_mapping_covers_scenario_matrix_workflows() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let rows = matrix["rows"].as_array().expect("rows array");
    let mappings = perf["workflow_sli_mapping"]
        .as_array()
        .expect("perf_sli_matrix workflow_sli_mapping array");

    let mapped_workflows: HashSet<String> = mappings
        .iter()
        .filter_map(|entry| entry["workflow_id"].as_str().map(ToOwned::to_owned))
        .collect();

    for row in rows {
        let workflow_id = row["workflow_id"].as_str().unwrap_or("unknown");
        assert!(
            mapped_workflows.contains(workflow_id),
            "perf_sli_matrix workflow_sli_mapping missing scenario workflow {workflow_id}"
        );
    }
}

#[test]
fn perf_sli_responsiveness_workflows_keep_primary_latency_sli() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let primary_sli_ids: HashSet<String> = perf["metric_hierarchy"]["primary"]
        .as_array()
        .expect("perf_sli_matrix metric_hierarchy.primary array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    assert!(
        !primary_sli_ids.is_empty(),
        "metric_hierarchy.primary must define release-deciding latency SLIs"
    );

    let workflow_mapping: HashMap<String, HashSet<String>> = perf["workflow_sli_mapping"]
        .as_array()
        .expect("perf_sli_matrix workflow_sli_mapping array")
        .iter()
        .filter_map(|entry| {
            let workflow_id = entry["workflow_id"].as_str()?;
            let sli_ids = entry["sli_ids"]
                .as_array()
                .expect("workflow_sli_mapping rows must define sli_ids")
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            Some((workflow_id.to_owned(), sli_ids))
        })
        .collect();

    let responsiveness_markers = [
        "swarm",
        "performance",
        "responsiv",
        "latency",
        "smooth",
        "fast",
    ];
    let mut missing_workflow_mappings = Vec::new();

    for row in perf["scenario_sli_matrix"]
        .as_array()
        .expect("perf_sli_matrix scenario_sli_matrix array")
    {
        let workflow_id = row["scenario_id"].as_str().unwrap_or("unknown");
        let user_outcome = row["user_outcome"].as_str().unwrap_or("");
        let is_responsiveness_workflow = responsiveness_markers.iter().any(|needle| {
            contains_ascii_case_insensitive(workflow_id, needle)
                || contains_ascii_case_insensitive(user_outcome, needle)
        });
        if !is_responsiveness_workflow {
            continue;
        }

        let scenario_sli_ids: HashSet<String> = row["sli_ids"]
            .as_array()
            .expect("scenario_sli_matrix rows must define sli_ids")
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect();
        assert!(
            !scenario_sli_ids.is_disjoint(&primary_sli_ids),
            "responsiveness workflow {workflow_id} must include at least one primary latency SLI in scenario_sli_matrix"
        );

        let Some(mapped_sli_ids) = workflow_mapping.get(workflow_id) else {
            missing_workflow_mappings.push(workflow_id.to_owned());
            continue;
        };
        assert!(
            !mapped_sli_ids.is_disjoint(&primary_sli_ids),
            "responsiveness workflow {workflow_id} must include at least one primary latency SLI in workflow_sli_mapping"
        );
    }

    assert!(
        missing_workflow_mappings.is_empty(),
        "workflow_sli_mapping missing responsiveness workflows: {}",
        missing_workflow_mappings.join(", ")
    );
}

#[test]
fn swarm_runbook_threads_agent_mail_health_into_empty_queue_convergence() {
    let runbook = load_text(SWARM_OPERATIONS_RUNBOOK_PATH);
    for required in [
        "agent-mail-health.json",
        "--agent-mail-health-json",
        "use_beads_fallback",
        "agent_mail_status=unavailable",
        "Beads claim as the soft lock",
    ] {
        assert!(
            runbook.contains(required),
            "swarm operations startup checklist must preserve Agent Mail health diagnostic `{required}`"
        );
    }
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    !needle.is_empty()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[test]
fn perf_sli_phase_validation_consumers_include_dependent_beads() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let consumers = perf["phase_validation_consumers"]
        .as_array()
        .expect("perf_sli_matrix phase_validation_consumers array");
    let consumer_ids: HashSet<String> = consumers
        .iter()
        .filter_map(|entry| entry["issue_id"].as_str().map(ToOwned::to_owned))
        .collect();

    for required in [
        "bd-3ar8v.1.5",
        "bd-3ar8v.2.11",
        "bd-3ar8v.3.11",
        "bd-3ar8v.6.7",
    ] {
        assert!(
            consumer_ids.contains(required),
            "phase_validation_consumers must include dependent bead {required}"
        );
    }
}

#[test]
fn perf_sli_epistemology_contract_is_versioned_and_reference_linked() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);

    let version = perf["contract_version"]
        .as_str()
        .expect("perf_sli_matrix contract_version must be present");
    let segments: Vec<&str> = version.split('.').collect();
    assert!(
        segments.len() == 3 && segments.iter().all(|part| part.parse::<u64>().is_ok()),
        "contract_version must be semantic version (x.y.z), got: {version}"
    );

    let refs = &perf["contract_references"];
    let mut linked_ids = HashSet::new();
    for key in [
        "ci_blocking_beads",
        "certification_beads",
        "reporting_beads",
    ] {
        let entries = refs[key]
            .as_array()
            .expect("contract reference section must be an array");
        assert!(
            !entries.is_empty(),
            "contract_references.{key} must not be empty"
        );
        for entry in entries {
            let issue_id = entry["issue_id"]
                .as_str()
                .expect("contract reference entry needs issue_id");
            linked_ids.insert(issue_id.to_string());
        }
    }

    for required in ["bd-3ar8v.1.2", "bd-3ar8v.1.12", "bd-3ar8v.6.5"] {
        assert!(
            linked_ids.contains(required),
            "perf epistemology contract must reference critical CI/cert/report bead {required}"
        );
    }
}

#[test]
fn perf_sli_workload_partition_contract_is_versioned_and_complete() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let run_all = load_text("scripts/e2e/run_all.sh");
    let partition_contract = &perf["workload_partition_contract"];

    let schema = partition_contract["schema"]
        .as_str()
        .expect("workload_partition_contract.schema must be present");
    assert!(
        schema.starts_with("pi.perf.workload_partition_contract."),
        "workload_partition_contract.schema must be versioned, got: {schema}"
    );

    let version = partition_contract["contract_version"]
        .as_str()
        .expect("workload_partition_contract.contract_version must be present");
    let segments: Vec<&str> = version.split('.').collect();
    assert!(
        segments.len() == 3 && segments.iter().all(|part| part.parse::<u64>().is_ok()),
        "workload_partition_contract.contract_version must be semantic version (x.y.z), got: {version}"
    );
    assert_eq!(
        partition_contract["bead_id"].as_str(),
        Some("bd-3ar8v.1.10"),
        "workload_partition_contract must be tied to bd-3ar8v.1.10"
    );

    let tags: HashSet<String> = perf["benchmark_partitions"]["partition_tags"]
        .as_array()
        .expect("benchmark_partitions.partition_tags must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    let expected_tags: HashSet<String> = ["matched-state", "realistic"]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(
        tags, expected_tags,
        "benchmark_partitions.partition_tags must be exactly [matched-state, realistic]"
    );

    let script_refs = partition_contract["benchmark_script_references"]
        .as_array()
        .expect("workload_partition_contract.benchmark_script_references must be an array");
    assert!(
        !script_refs.is_empty(),
        "workload_partition_contract must reference benchmark scripts"
    );
    for entry in script_refs {
        let path = entry["path"]
            .as_str()
            .expect("benchmark_script_references entries must include path");
        assert!(
            std::path::Path::new(path).exists(),
            "referenced benchmark script must exist: {path}"
        );
    }
    assert!(
        run_all.contains("docs/perf_sli_matrix.json"),
        "scripts/e2e/run_all.sh must reference docs/perf_sli_matrix.json"
    );

    let metadata_fields: HashSet<String> = partition_contract["required_result_metadata_fields"]
        .as_array()
        .expect("workload_partition_contract.required_result_metadata_fields must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for field in [
        "workflow_id",
        "workflow_class",
        "suite_ids",
        "vcr_mode",
        "scenario_owner",
    ] {
        assert!(
            metadata_fields.contains(field),
            "required_result_metadata_fields must include {field}"
        );
    }

    let workflow_ids: HashSet<String> = matrix["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .filter_map(|row| row["workflow_id"].as_str().map(ToOwned::to_owned))
        .collect();
    let scenario_partition_coverage = partition_contract["scenario_partition_coverage"]
        .as_array()
        .expect("workload_partition_contract.scenario_partition_coverage must be an array");
    let mut covered_workflows = HashSet::new();
    for row in scenario_partition_coverage {
        let workflow_id = row["workflow_id"]
            .as_str()
            .expect("scenario_partition_coverage entries must include workflow_id");
        assert!(
            workflow_ids.contains(workflow_id),
            "scenario_partition_coverage references unknown workflow_id {workflow_id}"
        );
        covered_workflows.insert(workflow_id.to_string());
        let required_partitions: HashSet<String> = row["required_partitions"]
            .as_array()
            .expect("scenario partition coverage must include required_partitions")
            .iter()
            .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
            .collect();
        assert_eq!(
            required_partitions, expected_tags,
            "workflow {workflow_id} must require both matched-state and realistic partitions"
        );
    }
    assert_eq!(
        covered_workflows, workflow_ids,
        "workload_partition_contract.scenario_partition_coverage must cover every workflow"
    );

    let release_policy = &partition_contract["release_claim_policy"];
    assert_eq!(
        release_policy["forbidden_single_partition_claim"].as_bool(),
        Some(true),
        "release_claim_policy.forbidden_single_partition_claim must be true"
    );
    let required_for_global: HashSet<String> =
        release_policy["global_conclusion_requires_partitions"]
            .as_array()
            .expect("release_claim_policy.global_conclusion_requires_partitions must be an array")
            .iter()
            .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
            .collect();
    assert_eq!(
        required_for_global, expected_tags,
        "release_claim_policy must require both partition tags for global conclusions"
    );

    let weights = &partition_contract["weighting_policy"]["decision_weights"];
    let matched_weight = weights["matched-state"]
        .as_f64()
        .expect("weighting_policy.decision_weights.matched-state must be numeric");
    let realistic_weight = weights["realistic"]
        .as_f64()
        .expect("weighting_policy.decision_weights.realistic must be numeric");
    assert!(
        (matched_weight + realistic_weight - 1.0).abs() < 1e-9,
        "weighting_policy decision weights must sum to 1.0"
    );
    assert!(
        realistic_weight > matched_weight,
        "realistic partition weight must be greater than matched-state weight"
    );
}

#[test]
fn franken_node_mission_contract_is_versioned_and_has_required_tiers() {
    let contract = load_json(FRANKEN_NODE_MISSION_CONTRACT_PATH);

    assert_eq!(
        contract["schema"].as_str(),
        Some("pi.franken_node.mission_contract.v1"),
        "franken mission contract schema must be pi.franken_node.mission_contract.v1"
    );
    assert_eq!(
        contract["bead_id"].as_str(),
        Some("bd-3ar8v.7.1"),
        "franken mission contract must be tied to bd-3ar8v.7.1"
    );

    let version = contract["contract_version"]
        .as_str()
        .expect("franken mission contract_version must be present");
    let segments: Vec<&str> = version.split('.').collect();
    assert!(
        segments.len() == 3 && segments.iter().all(|part| part.parse::<u64>().is_ok()),
        "franken mission contract_version must be semantic version (x.y.z), got: {version}"
    );

    let mission_statement = contract["mission_statement"]
        .as_str()
        .expect("franken mission contract must include mission_statement");
    assert!(
        !mission_statement.trim().is_empty(),
        "mission_statement must be non-empty"
    );

    let tiers = contract["claim_tiers"]
        .as_array()
        .expect("franken mission claim_tiers must be an array");
    let tier_ids: HashSet<String> = tiers
        .iter()
        .filter_map(|tier| tier["tier_id"].as_str().map(ToOwned::to_owned))
        .collect();

    for required in ["extension_host_dropin", "full_runtime_replacement"] {
        assert!(
            tier_ids.contains(required),
            "franken mission contract must include claim tier: {required}"
        );
    }
}

#[test]
fn franken_node_mission_contract_forbidden_claims_cover_strict_node_and_bun_language() {
    let contract = load_json(FRANKEN_NODE_MISSION_CONTRACT_PATH);
    let forbidden = contract["forbidden_claims"]
        .as_array()
        .expect("franken mission forbidden_claims must be an array");
    assert!(
        !forbidden.is_empty(),
        "franken mission forbidden_claims must not be empty"
    );

    let mut strict_claim_ids = Vec::new();
    let mut phrases = Vec::new();
    for claim in forbidden {
        let blocked_until_tier = claim["blocked_until_tier"]
            .as_str()
            .expect("forbidden_claims entries must include blocked_until_tier");
        if blocked_until_tier != "full_runtime_replacement" {
            continue;
        }
        let claim_id = claim["claim_id"]
            .as_str()
            .expect("forbidden_claims entries must include claim_id");
        strict_claim_ids.push(claim_id.to_string());
        let phrase = claim["phrase"]
            .as_str()
            .expect("forbidden_claims entries must include phrase")
            .to_ascii_lowercase();
        phrases.push(phrase);
    }

    assert!(
        !strict_claim_ids.is_empty(),
        "strict-tier forbidden claim IDs must be declared"
    );
    assert!(
        phrases.iter().any(|phrase| phrase.contains("node")),
        "strict-tier forbidden claims must include explicit Node language"
    );
    assert!(
        phrases.iter().any(|phrase| phrase.contains("bun")),
        "strict-tier forbidden claims must include explicit Bun language"
    );

    let release_policy = &contract["release_claim_policy"];
    assert_eq!(
        release_policy["default_allowed_tier"].as_str(),
        Some("extension_host_dropin"),
        "release_claim_policy.default_allowed_tier must be extension_host_dropin"
    );
    assert_eq!(
        release_policy["strict_claim_tier"].as_str(),
        Some("full_runtime_replacement"),
        "release_claim_policy.strict_claim_tier must be full_runtime_replacement"
    );
    assert_eq!(
        release_policy["strict_claim_verdict_artifact"].as_str(),
        Some("docs/evidence/dropin-certification-verdict.json"),
        "strict_claim_verdict_artifact must point at drop-in certification verdict"
    );
    assert_eq!(
        release_policy["strict_claim_requires_dropin_verdict"].as_str(),
        Some("CERTIFIED"),
        "strict claim verdict requirement must be CERTIFIED"
    );
    assert_eq!(
        release_policy["fail_closed"].as_bool(),
        Some(true),
        "release claim policy must be fail-closed"
    );
}

#[test]
fn franken_node_mission_contract_tier_mapping_declares_required_checks_and_phase6_beads() {
    let contract = load_json(FRANKEN_NODE_MISSION_CONTRACT_PATH);
    let tiers = contract["claim_tiers"]
        .as_array()
        .expect("franken mission claim_tiers must be an array");

    let extension = tiers
        .iter()
        .find(|tier| tier["tier_id"].as_str() == Some("extension_host_dropin"))
        .expect("extension_host_dropin tier must exist");
    let extension_checks: HashSet<String> = extension["required_check_ids"]
        .as_array()
        .expect("extension_host_dropin.required_check_ids must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for check in [
        "claim_integrity.realistic_session_shape_coverage",
        "claim_integrity.microbench_only_claim",
        "claim_integrity.global_claim_missing_partition_coverage",
        "claim_integrity.unresolved_conflicting_claims",
        "claim_integrity.evidence_adjudication_matrix_schema",
        "claim_integrity.franken_node_requested_claim_tier_allowed",
    ] {
        assert!(
            extension_checks.contains(check),
            "extension_host_dropin.required_check_ids must include {check}"
        );
    }

    let strict = tiers
        .iter()
        .find(|tier| tier["tier_id"].as_str() == Some("full_runtime_replacement"))
        .expect("full_runtime_replacement tier must exist");
    let strict_checks: HashSet<String> = strict["required_check_ids"]
        .as_array()
        .expect("full_runtime_replacement.required_check_ids must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for check in [
        "claim_integrity.franken_node_strict_replacement_dropin_certified",
        "claim_integrity.franken_node_phase6_runtime_beads_declared",
        "claim_integrity.franken_node_strict_tier_required_evidence",
    ] {
        assert!(
            strict_checks.contains(check),
            "full_runtime_replacement.required_check_ids must include {check}"
        );
    }

    let strict_beads: HashSet<String> = strict["required_beads"]
        .as_array()
        .expect("full_runtime_replacement.required_beads must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for bead in [
        "bd-3ar8v.7.2",
        "bd-3ar8v.7.3",
        "bd-3ar8v.7.4",
        "bd-3ar8v.7.5",
        "bd-3ar8v.7.6",
        "bd-3ar8v.7.15",
        "bd-3ar8v.7.16",
    ] {
        assert!(
            strict_beads.contains(bead),
            "full_runtime_replacement.required_beads must include {bead}"
        );
    }

    let strict_artifacts: HashSet<String> = strict["required_evidence_artifacts"]
        .as_array()
        .expect("full_runtime_replacement.required_evidence_artifacts must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for artifact in [
        "docs/evidence/dropin-certification-verdict.json",
        "docs/franken-node-kernel-extraction-boundary-manifest.json",
        "docs/franken-node-package-interop-contract.json",
        "docs/franken-node-runtime-substrate-contract.json",
        "docs/franken-node-remediation-backlog-contract.json",
        "docs/franken-node-shadow-canary-rollout-contract.json",
        "tests/full_suite_gate/franken_node_kernel_boundary_drift_report.json",
    ] {
        assert!(
            strict_artifacts.contains(artifact),
            "full_runtime_replacement.required_evidence_artifacts must include {artifact}"
        );
    }
}

#[test]
fn perf_sli_metric_hierarchy_has_three_levels_without_overlap() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let hierarchy = &perf["metric_hierarchy"];

    let primary = hierarchy["primary"]
        .as_array()
        .expect("metric_hierarchy.primary must be an array");
    let secondary = hierarchy["secondary"]
        .as_array()
        .expect("metric_hierarchy.secondary must be an array");
    let tertiary = hierarchy["tertiary"]
        .as_array()
        .expect("metric_hierarchy.tertiary must be an array");

    assert!(
        !primary.is_empty(),
        "metric_hierarchy.primary must not be empty"
    );
    assert!(
        !secondary.is_empty(),
        "metric_hierarchy.secondary must not be empty"
    );
    assert!(
        !tertiary.is_empty(),
        "metric_hierarchy.tertiary must not be empty"
    );

    let mut seen = HashSet::new();
    for (_level, items) in [
        ("primary", primary),
        ("secondary", secondary),
        ("tertiary", tertiary),
    ] {
        for item in items {
            let metric = item
                .as_str()
                .expect("metric_hierarchy items must be strings");
            assert!(
                seen.insert(metric.to_string()),
                "metric_hierarchy has duplicate metric id across levels: {metric}"
            );
        }
    }

    let rules = hierarchy["interpretation_rules"]
        .as_array()
        .expect("metric_hierarchy.interpretation_rules must be an array");
    assert!(
        !rules.is_empty(),
        "metric_hierarchy.interpretation_rules must define at least one rule"
    );
}

#[test]
fn perf_sli_catalog_entries_define_priority_and_mandatory_interpretation_notes() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let catalog = perf["sli_catalog"]
        .as_array()
        .expect("perf_sli_matrix sli_catalog array");

    for entry in catalog {
        let sli_id = entry["sli_id"].as_str().unwrap_or("<unknown>");
        let priority = entry["priority_level"].as_str().unwrap_or("");
        assert!(
            ["primary", "secondary", "tertiary"].contains(&priority),
            "{sli_id} must declare priority_level as primary/secondary/tertiary"
        );

        let notes = entry["mandatory_interpretation_notes"]
            .as_array()
            .expect("SLI catalog entry must define mandatory_interpretation_notes");
        assert!(
            !notes.is_empty(),
            "{sli_id} must include at least one mandatory_interpretation_note"
        );
        for (idx, note) in notes.iter().enumerate() {
            let text = note
                .as_str()
                .expect("SLI interpretation note must be a string");
            assert!(
                !text.trim().is_empty(),
                "{sli_id} interpretation note {idx} must be non-empty"
            );
        }
    }
}

#[test]
fn perf_sli_reporting_contract_requires_absolute_and_relative_metrics_for_all_workflows() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let perf = load_json(PERF_SLI_MATRIX_PATH);
    let reporting = &perf["reporting_contract"];

    let required_fields = reporting["required_metric_fields"]
        .as_array()
        .expect("reporting_contract.required_metric_fields must be an array");
    let required_field_set: HashSet<String> = required_fields
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();

    for field in [
        "scenario_id",
        "workload_partition",
        "scenario_metadata",
        "sli_id",
        "evidence_class",
        "confidence",
        "absolute_value",
        "rust_vs_node_ratio",
        "rust_vs_bun_ratio",
        "correlation_id",
    ] {
        assert!(
            required_field_set.contains(field),
            "reporting_contract.required_metric_fields missing {field}"
        );
    }

    let workflow_ids: HashSet<String> = matrix["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .filter_map(|row| row["workflow_id"].as_str().map(ToOwned::to_owned))
        .collect();
    let required_scenarios: HashSet<String> = reporting["required_scenarios"]
        .as_array()
        .expect("reporting_contract.required_scenarios must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();

    assert_eq!(
        required_scenarios, workflow_ids,
        "reporting_contract.required_scenarios must exactly match e2e_scenario_matrix workflows"
    );

    let required_partition_tags: HashSet<String> = reporting["required_partition_tags"]
        .as_array()
        .expect("reporting_contract.required_partition_tags must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    let expected_partition_tags: HashSet<String> = ["matched-state", "realistic"]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(
        required_partition_tags, expected_partition_tags,
        "reporting_contract.required_partition_tags must be exactly [matched-state, realistic]"
    );

    let required_metadata_fields: HashSet<String> = reporting["required_scenario_metadata_fields"]
        .as_array()
        .expect("reporting_contract.required_scenario_metadata_fields must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for field in [
        "workflow_id",
        "workflow_class",
        "suite_ids",
        "vcr_mode",
        "scenario_owner",
    ] {
        assert!(
            required_metadata_fields.contains(field),
            "reporting_contract.required_scenario_metadata_fields missing {field}"
        );
    }

    let scenario_requirements = reporting["scenario_requirements"]
        .as_array()
        .expect("reporting_contract.scenario_requirements must be an array");
    for row in scenario_requirements {
        let workflow_id = row["workflow_id"]
            .as_str()
            .expect("scenario_requirements entries need workflow_id");
        assert!(
            workflow_ids.contains(workflow_id),
            "scenario_requirements references unknown workflow_id {workflow_id}"
        );
        assert_eq!(
            row["required_absolute_metrics"].as_bool(),
            Some(true),
            "workflow {workflow_id} must require absolute metrics"
        );
        let ratios = row["required_relative_ratios"]
            .as_array()
            .expect("workflow must define required_relative_ratios");
        let ratio_set: HashSet<&str> = ratios.iter().filter_map(Value::as_str).collect();
        for required_ratio in ["rust_vs_node_ratio", "rust_vs_bun_ratio"] {
            assert!(
                ratio_set.contains(required_ratio),
                "workflow {workflow_id} must require {required_ratio}"
            );
        }
    }

    let scenario_partition_requirements = reporting["scenario_partition_requirements"]
        .as_array()
        .expect("reporting_contract.scenario_partition_requirements must be an array");
    let mut covered_workflows = HashSet::new();
    for row in scenario_partition_requirements {
        let workflow_id = row["workflow_id"]
            .as_str()
            .expect("scenario_partition_requirements entries need workflow_id");
        assert!(
            workflow_ids.contains(workflow_id),
            "scenario_partition_requirements references unknown workflow_id {workflow_id}"
        );
        covered_workflows.insert(workflow_id.to_string());
        let required_partitions: HashSet<String> = row["required_partitions"]
            .as_array()
            .expect("workflow must define required_partitions")
            .iter()
            .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
            .collect();
        assert_eq!(
            required_partitions, expected_partition_tags,
            "workflow {workflow_id} must require matched-state and realistic partitions"
        );
    }
    assert_eq!(
        covered_workflows, workflow_ids,
        "scenario_partition_requirements must cover every required workflow"
    );
}

#[test]
fn perf_sli_confidence_and_evidence_labels_are_machine_enforced() {
    let perf = load_json(PERF_SLI_MATRIX_PATH);

    let evidence_class: Vec<&str> = perf["evidence_labels"]["evidence_class"]
        .as_array()
        .expect("evidence_labels.evidence_class must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        evidence_class,
        vec!["measured", "inferred"],
        "evidence_labels.evidence_class must be [measured, inferred]"
    );

    let confidence_labels: Vec<&str> = perf["evidence_labels"]["confidence"]
        .as_array()
        .expect("evidence_labels.confidence must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        confidence_labels,
        vec!["high", "medium", "low"],
        "evidence_labels.confidence must be [high, medium, low]"
    );

    let ci = &perf["ci_enforcement"];
    let required_result_fields: HashSet<String> = ci["required_result_fields"]
        .as_array()
        .expect("ci_enforcement.required_result_fields must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for field in [
        "evidence_class",
        "confidence",
        "absolute_value",
        "rust_vs_node_ratio",
        "rust_vs_bun_ratio",
        "workload_partition",
        "scenario_metadata",
    ] {
        assert!(
            required_result_fields.contains(field),
            "ci_enforcement.required_result_fields must include {field}"
        );
    }

    let fail_closed: HashSet<String> = ci["fail_closed_conditions"]
        .as_array()
        .expect("ci_enforcement.fail_closed_conditions must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();
    for condition in [
        "missing_absolute_or_relative_values",
        "missing_workload_partition_tag",
        "missing_scenario_metadata",
        "invalid_evidence_class",
        "invalid_confidence_label",
        "responsiveness_scenario_missing_primary_latency_sli",
        "responsiveness_workflow_missing_primary_latency_sli",
        "microbench_only_claim",
        "global_claim_missing_partition_coverage",
        "unresolved_conflicting_claims",
    ] {
        assert!(
            fail_closed.contains(condition),
            "ci_enforcement.fail_closed_conditions must include {condition}"
        );
    }
}

#[test]
fn run_all_claim_integrity_gate_wires_fail_closed_conditions() {
    let run_all = load_text("scripts/e2e/run_all.sh");

    for token in [
        "CLAIM_INTEGRITY_REQUIRED",
        "FRANKEN_NODE_CLAIM_TIER",
        "PERF_BASELINE_CONFIDENCE_JSON",
        "PERF_EXTENSION_STRATIFICATION_JSON",
        "PERF_PHASE1_MATRIX_VALIDATION_JSON",
        "docs/franken-node-mission-contract.json",
        "claim_integrity.phase1_matrix_validation_path_configured",
        "claim_integrity.phase1_matrix_validation_json",
        "claim_integrity.phase1_matrix_validation_schema",
        "claim_integrity.phase1_matrix_validation_generated_at_fresh",
        "claim_integrity.phase1_matrix_correlation_matches_run",
        "claim_integrity.phase1_matrix_primary_outcomes_object",
        "claim_integrity.phase1_matrix_primary_outcomes_required_fields",
        "claim_integrity.phase1_matrix_primary_outcomes_status_valid",
        "claim_integrity.phase1_matrix_primary_outcomes_metrics_present",
        "claim_integrity.phase1_matrix_primary_outcomes_ordering_policy",
        "claim_integrity.phase1_matrix_stage_summary_object",
        "claim_integrity.phase1_matrix_required_stage_keys_exact",
        "claim_integrity.phase1_matrix_stage_summary_counts_coherent",
        "claim_integrity.phase1_matrix_missing_stage_metrics_visibility",
        "claim_integrity.phase1_matrix_regression_guards_object",
        "claim_integrity.phase1_matrix_regression_guards_required_fields",
        "claim_integrity.responsiveness_scenario_missing_primary_latency_sli",
        "claim_integrity.responsiveness_workflow_missing_primary_latency_sli",
        "scenario_to_sli_ids",
        "primary_sli_ids",
        "responsiveness_workflow_ids",
        "workflow_to_sli_ids",
        "metric_hierarchy",
        "isdisjoint(primary_sli_ids)",
        "claim_integrity.phase1_matrix_regression_guard_reasons_format",
        "claim_integrity.phase1_matrix_regression_guard_reasons_known",
        "claim_integrity.phase1_matrix_regression_guard_status_valid",
        "claim_integrity.phase1_matrix_regression_guard_reason_alignment",
        "claim_integrity.phase1_matrix_regression_guard_reason_set_exact",
        "claim_integrity.phase1_matrix_consumption_contract_object",
        "claim_integrity.phase1_matrix_downstream_beads_include_phase5",
        "claim_integrity.phase1_matrix_downstream_consumers_contract",
        "claim_integrity.phase1_matrix_weighted_bottleneck_object",
        "claim_integrity.phase1_matrix_weighted_bottleneck_schema",
        "claim_integrity.phase1_matrix_weighted_bottleneck_status_valid",
        "claim_integrity.phase1_matrix_weighted_bottleneck_outputs_array",
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_object",
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_counts_present",
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_bounds",
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_source_matches_matrix_cells",
        "claim_integrity.phase1_matrix_weighted_bottleneck_status_coherence",
        "claim_integrity.phase1_matrix_weighted_bottleneck_stage_coverage",
        "failure_or_gap_reasons",
        "expected_regression_guard_reason_set",
        "_regression_unverified",
        "claim_integrity.phase1_matrix_cells_primary_e2e_metrics_present",
        "downstream_consumers",
        "weighted_bottleneck_attribution.global_ranking",
        "weighted_bottleneck_attribution.per_scale",
        "primary_e2e_before_microbench",
        "claim_integrity.missing_or_stale_evidence",
        "claim_integrity.missing_required_result_field",
        "claim_integrity.scenario_without_sli_mapping",
        "claim_integrity.sli_without_thresholds",
        "claim_integrity.missing_absolute_or_relative_values",
        "claim_integrity.invalid_evidence_class",
        "claim_integrity.invalid_confidence_label",
        "missing_matrix_source_record",
        "missing_stage_metrics:",
        "claim_integrity.realistic_session_shape_coverage",
        "claim_integrity.microbench_only_claim",
        "cherry_pick_guard.global_claim_valid must be true",
        "required_layers = [",
        "\"full_e2e_long_session\",",
        "extension_stratification.global_claim_valid",
        "claim_integrity.global_claim_missing_partition_coverage",
        "claim_integrity.evidence_adjudication_matrix_schema",
        "claim_integrity.unresolved_conflicting_claims",
        "claim_integrity.franken_node_mission_contract_json",
        "claim_integrity.franken_node_mission_contract_schema",
        "claim_integrity.franken_node_claim_tiers_defined",
        "claim_integrity.franken_node_extension_tier_required_checks",
        "claim_integrity.franken_node_forbidden_claims_defined",
        "claim_integrity.franken_node_forbidden_claim_language_coverage",
        "claim_integrity.franken_node_extension_host_tier_evidence",
        "claim_integrity.franken_node_phase6_runtime_beads_declared",
        "claim_integrity.franken_node_strict_replacement_dropin_certified",
        "claim_integrity.franken_node_strict_tier_required_evidence",
        "claim_integrity.franken_node_kernel_boundary_manifest_json",
        "claim_integrity.franken_node_kernel_boundary_manifest_schema",
        "claim_integrity.franken_node_kernel_boundary_report_artifact_declared",
        "claim_integrity.franken_node_kernel_boundary_required_checks_declared",
        "claim_integrity.franken_node_kernel_boundary_failure_policy_hard_fail",
        "claim_integrity.franken_node_kernel_boundary_module_mappings_present",
        "claim_integrity.franken_node_kernel_boundary_drift_report_json",
        "claim_integrity.franken_node_kernel_boundary_drift_report_schema",
        "claim_integrity.franken_node_kernel_boundary_drift_report_pass",
        "claim_integrity.franken_node_requested_claim_tier_known",
        "claim_integrity.franken_node_requested_claim_tier_allowed",
        "claim_integrity.franken_node_claim_gate_status_json",
        "pi.franken_node.claim_gate_status.v1",
        "pi.franken_node.kernel_boundary_drift_report.v1",
        "\"franken_node_claim_gate_status\"",
        "\"franken_node_kernel_boundary_drift_report\"",
    ] {
        assert!(
            run_all.contains(token),
            "scripts/e2e/run_all.sh must enforce claim-integrity token: {token}"
        );
    }
}

#[test]
fn ci_workflow_runs_perf_claim_integrity_bundle_before_run_all_gate() {
    let ci = load_text(CI_WORKFLOW_PATH);

    for token in [
        "Generate perf claim-integrity evidence bundle [linux]",
        "./scripts/perf/orchestrate.sh",
        "--profile ci",
        "PERF_BASELINE_CONFIDENCE_JSON",
        "PERF_EXTENSION_STRATIFICATION_JSON",
        "CLAIM_INTEGRITY_REQUIRED=1",
    ] {
        assert!(
            ci.contains(token),
            "CI workflow must include claim-integrity gate wiring token: {token}"
        );
    }
}

#[test]
fn weekly_certification_verdict_workflow_regenerates_verdict_and_opens_pr() {
    let workflow = load_text(WEEKLY_CERTIFICATION_VERDICT_WORKFLOW_PATH);

    for token in [
        "cron: \"0 7 * * 1\"",
        "workflow_dispatch:",
        "contents: write",
        "pull-requests: write",
        "conformance_must_pass_gate",
        "lifecycle_hook_parity_matrix_writes_evidence_artifact",
        "full_certification",
        "tests/full_suite_gate/certification_verdict.json",
        "docs/evidence/dropin-certification-verdict.json",
        "overall_verdict = (",
        "if source_commit == expected_source_sha",
        "and len(hard_gate_results) == 12",
        "and [gate[\"gate_id\"] for gate in hard_gate_results] == expected_gate_ids",
        "and all(gate[\"status\"] == \"pass\" for gate in hard_gate_results)",
        "and lane_verdict == \"pass\"",
        "and not blocking_reasons",
        "else \"NOT_CERTIFIED\"",
        "peter-evans/create-pull-request@v7",
        "automation/weekly-certification-verdict",
    ] {
        assert!(
            workflow.contains(token),
            "weekly certification verdict workflow must include token: {token}"
        );
    }

    let cron_idx = workflow.find("cron: \"0 7 * * 1\"").unwrap_or(usize::MAX);
    let verdict_idx = workflow
        .find("docs/evidence/dropin-certification-verdict.json")
        .unwrap_or(usize::MAX);
    let pr_idx = workflow
        .find("peter-evans/create-pull-request@v7")
        .unwrap_or(usize::MAX);
    assert!(
        cron_idx < verdict_idx && verdict_idx < pr_idx,
        "weekly certification verdict workflow must schedule, regenerate the verdict, then open a PR"
    );
}

#[test]
fn dropin_evidence_policy_uses_current_docs_evidence_paths() {
    for path in [
        "AGENTS.md",
        DROPIN_CERTIFICATION_CONTRACT_PATH,
        INTEGRATOR_MIGRATION_PLAYBOOK_PATH,
        MAINTENANCE_DASHBOARD_SCRIPT_PATH,
        RECONCILE_BEADS_LEDGER_SCRIPT_PATH,
        DROPIN_RPC_SURFACE_DIFF_PATH,
        DROPIN_PARITY_GAP_LEDGER_PATH,
    ] {
        let content = load_text(path);
        for stale_path in [
            "docs/dropin-certification-verdict.json",
            "docs/dropin-feature-inventory-matrix.json",
            "docs/dropin-parity-gap-ledger.json",
        ] {
            assert!(
                !content.contains(stale_path),
                "{path} must not reference stale root-level drop-in evidence path {stale_path}"
            );
        }
    }

    let dashboard_script = load_text(MAINTENANCE_DASHBOARD_SCRIPT_PATH);
    for current_path in [
        "docs/evidence/dropin-certification-verdict.json",
        "docs/evidence/dropin-parity-gap-ledger.json",
    ] {
        assert!(
            dashboard_script.contains(current_path),
            "maintenance dashboard must read current evidence path {current_path}"
        );
    }

    let parity_gap_ledger = load_text(DROPIN_PARITY_GAP_LEDGER_PATH);
    assert!(
        parity_gap_ledger.contains("docs/evidence/dropin-feature-inventory-matrix.json"),
        "parity gap ledger must reference the current evidence feature inventory path"
    );
}

#[test]
fn dropin_gap_ledger_non_blocking_entries_are_reconciled() {
    let ledger = load_json(DROPIN_PARITY_GAP_LEDGER_PATH);
    let entries = ledger
        .get("entries")
        .and_then(Value::as_array)
        .expect("drop-in gap ledger must contain entries");
    let mut severity_counts = std::collections::BTreeMap::<&str, usize>::new();

    for entry in entries {
        let gap_id = entry
            .get("gap_id")
            .and_then(Value::as_str)
            .expect("ledger entries must have gap_id");
        let severity = entry.get("severity").and_then(Value::as_str).unwrap_or("");
        *severity_counts.entry(severity).or_default() += 1;
        let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
        assert!(
            !status.is_empty(),
            "{gap_id} must have an explicit status so stale medium/low gaps cannot hide behind null status"
        );

        if matches!(severity, "medium" | "low") {
            let has_active_owner = matches!(status, "open" | "in_progress")
                && entry
                    .get("owner_issue_primary")
                    .and_then(Value::as_str)
                    .is_some_and(|owner| !owner.is_empty())
                && entry
                    .get("closure_path")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty());
            let has_closed_evidence = matches!(status, "resolved" | "closed")
                && entry
                    .get("verification_evidence")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty())
                && (entry.get("resolution_date").is_some()
                    || entry.get("closure_date").is_some()
                    || entry.get("closure_bead").is_some());
            let has_waiver = status == "waived"
                && entry
                    .get("waiver_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| !reason.is_empty())
                && entry
                    .get("waiver_decision_bead")
                    .and_then(Value::as_str)
                    .is_some_and(|bead| !bead.is_empty())
                && entry
                    .get("release_claim_impact")
                    .and_then(Value::as_str)
                    .is_some_and(|impact| impact.contains("certification verdict"));

            assert!(
                has_active_owner || has_closed_evidence || has_waiver,
                "{gap_id} must have an active owner, closed evidence, or deliberate waiver"
            );
        }

        if status == "waived" {
            assert!(
                entry
                    .get("waiver_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| !reason.is_empty()),
                "{gap_id} waived entries must explain the waiver"
            );
            assert!(
                entry
                    .get("release_claim_impact")
                    .and_then(Value::as_str)
                    .is_some_and(|impact| impact.contains("certification verdict")),
                "{gap_id} waived entries must keep release claims tied to the certification verdict"
            );
        }
    }

    let resolved_gap_ids = [
        "gap-config-ui-editor-knobs",
        "gap-env-pi-ai-antigravity-version",
        "gap-provider-support-verification",
    ];
    for gap_id in resolved_gap_ids {
        let entry = entries
            .iter()
            .find(|entry| entry.get("gap_id").and_then(Value::as_str) == Some(gap_id))
            .expect("resolved gap must remain in the drop-in gap ledger");
        assert_eq!(
            entry.get("severity").and_then(Value::as_str),
            Some("resolved"),
            "{gap_id} must be reconciled out of the medium/low active gap set"
        );
        assert_eq!(
            entry.get("status").and_then(Value::as_str),
            Some("resolved"),
            "{gap_id} must carry an explicit resolved status"
        );
        assert!(
            entry
                .get("closure_bead")
                .and_then(Value::as_str)
                .is_some_and(|bead| !bead.is_empty()),
            "{gap_id} must preserve the bead that closed the stale gap"
        );
        assert!(
            entry
                .get("verification_evidence")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            "{gap_id} must keep evidence for its resolved state"
        );
    }

    let waiver = entries
        .iter()
        .find(|entry| {
            entry.get("gap_id").and_then(Value::as_str) == Some("gap-rust-superset-governance")
        })
        .expect("rust superset governance waiver must remain in the ledger");
    assert_eq!(
        waiver.get("severity").and_then(Value::as_str),
        Some("waived"),
        "rust superset governance must be a deliberate waiver, not an open low-severity gap"
    );
    assert_eq!(
        waiver.get("status").and_then(Value::as_str),
        Some("waived"),
        "rust superset governance must carry an explicit waived status"
    );
    assert!(
        waiver
            .get("release_claim_impact")
            .and_then(Value::as_str)
            .is_some_and(|impact| impact.contains("certification verdict")),
        "rust superset governance waiver must keep strict release claims tied to the certification verdict"
    );

    let rollup_counts = ledger
        .pointer("/rollup/severity_counts")
        .and_then(Value::as_object)
        .expect("gap ledger rollup must contain severity_counts");
    for (severity, actual_count) in severity_counts {
        assert_eq!(
            rollup_counts
                .get(severity)
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            actual_count as u64,
            "gap ledger rollup count for {severity} must match entries"
        );
    }
}

#[test]
fn run_all_emits_scenario_cell_status_artifacts() {
    let run_all = load_text("scripts/e2e/run_all.sh");

    for token in [
        "pi.claim_integrity.scenario_cell_status.v1",
        "claim_integrity_scenario_cell_status.json",
        "claim_integrity_scenario_cell_status.md",
        "claim_integrity.scenario_cell_status_json",
        "claim_integrity.scenario_cell_status_markdown",
        "\"realistic_session_shape_coverage\"",
        "\"source_record_stream\"",
        "\"source_workload_path\"",
        "missing_matrix_source_record",
        "\"source\"",
        "\"source_path\"",
        "\"total_cells\"",
        "\"passing_cells\"",
        "\"failing_cells\"",
        "\"overall_status\"",
    ] {
        assert!(
            run_all.contains(token),
            "scripts/e2e/run_all.sh must include scenario-cell status token: {token}"
        );
    }
}

#[test]
fn run_all_emits_evidence_adjudication_matrix_artifacts() {
    let run_all = load_text("scripts/e2e/run_all.sh");

    for token in [
        "pi.claim_integrity.evidence_adjudication_matrix.v1",
        "claim_integrity_evidence_adjudication_matrix.json",
        "claim_integrity_evidence_adjudication_matrix.md",
        "claim_integrity.evidence_adjudication_matrix_json",
        "claim_integrity.evidence_adjudication_matrix_markdown",
        "\"claim_integrity_adjudication_matrix\"",
        "\"unresolved_conflict_count\"",
        "\"canonical_sources\"",
        "\"stale_or_noncanonical_sources\"",
    ] {
        assert!(
            run_all.contains(token),
            "scripts/e2e/run_all.sh must include adjudication-matrix token: {token}"
        );
    }
}

#[test]
fn run_all_wires_reactor_comparison_evidence_tokens() {
    let run_all = load_text("scripts/e2e/run_all.sh");

    for token in [
        "ext_stress_report_path",
        "\"ext_stress_report\"",
        "\"reactor_comparison\": reactor_compare_metrics",
        "inputs.ext_stress_report_present",
        "inputs.ext_stress_comparison_present",
        "reactor_comparison.throughput_gain_pct",
        "reactor_comparison.p95_delta_us",
        "reactor_comparison.p99_delta_us",
        "reactor_comparison.contention_proxy_improved",
        "pi.ext.stress_comparison.v1",
        "Reactor comparison throughput gain (%)",
    ] {
        assert!(
            run_all.contains(token),
            "scripts/e2e/run_all.sh must include reactor-comparison token: {token}"
        );
    }
}

#[test]
fn ci_workflow_publishes_scenario_cell_gate_artifacts() {
    let ci = load_text(CI_WORKFLOW_PATH);

    for token in [
        "PERF_SCENARIO_CELL_STATUS_JSON",
        "PERF_SCENARIO_CELL_STATUS_MD",
        "Publish scenario-cell gate status [linux]",
        "Upload scenario-cell gate artifacts [linux]",
        "scenario-cell-gate-${{ github.run_id }}-${{ github.run_attempt }}",
        "## Scenario Cell Gate Status",
    ] {
        assert!(
            ci.contains(token),
            "CI workflow must include scenario-cell publish token: {token}"
        );
    }
}

#[test]
fn run_all_claim_integrity_python_block_compiles() {
    let run_all = load_text("scripts/e2e/run_all.sh");
    let anchor = run_all
        .find("PERF_PHASE1_MATRIX_VALIDATION_JSON")
        .expect("run_all.sh must include phase1 matrix env wiring");
    let marker = "python3 - <<'PY'\n";
    let py_block_start = run_all[..anchor]
        .rfind(marker)
        .expect("run_all.sh must include claim-integrity Python heredoc")
        + marker.len();
    let py_block_end_rel = run_all[py_block_start..]
        .find("\nPY")
        .expect("run_all.sh claim-integrity Python heredoc must terminate with PY");
    let py_source = &run_all[py_block_start..py_block_start + py_block_end_rel];

    let temp = tempfile::tempdir().expect("create tempdir for claim-integrity Python compile");
    let py_path = temp.path().join("run_all_claim_integrity.py");
    std::fs::write(&py_path, py_source).expect("write extracted run_all claim-integrity Python");

    let output = Command::new("python3")
        .arg("-m")
        .arg("py_compile")
        .arg(&py_path)
        .output()
        .expect("run python3 -m py_compile for run_all claim-integrity Python");
    assert!(
        output.status.success(),
        "run_all claim-integrity Python must compile\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 6: CI gate remediation guidance
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn full_suite_gate_has_remediation_for_each_gate() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    // The gate should have remediation/hint/fix guidance
    assert!(
        gate.contains("remediation") || gate.contains("hint") || gate.contains("fix"),
        "full suite gate must include remediation guidance"
    );
}

#[test]
fn full_suite_gate_contains_sorted_coverage_path_ordering_guards() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    for token in [
        "must be sorted by normalized path order",
        "perf3x_bead_coverage_contract_fails_closed_on_unsorted_unit_evidence_paths",
        "perf3x_bead_coverage_contract_fails_closed_on_unsorted_e2e_evidence_paths",
        "perf3x_bead_coverage_contract_fails_closed_on_unsorted_log_evidence_paths",
    ] {
        assert!(
            gate.contains(token),
            "full suite gate must include sorted-coverage guard token: {token}"
        );
    }
}

#[test]
fn full_suite_gate_contains_canonical_coverage_row_ordering_guards() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    for token in [
        "coverage_rows must be sorted by canonical bead id order",
        "perf3x_bead_coverage_contract_fails_closed_on_misordered_rows",
    ] {
        assert!(
            gate.contains(token),
            "full suite gate must include canonical row-order guard token: {token}"
        );
    }
}

#[test]
fn full_suite_gate_contains_normalized_duplicate_path_fail_closed_guards() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    for token in [
        "perf3x_bead_coverage_contract_fails_closed_on_normalized_duplicate_dot_slash_variant",
        "perf3x_bead_coverage_contract_fails_closed_on_normalized_duplicate_backslash_variant",
    ] {
        assert!(
            gate.contains(token),
            "full suite gate must include normalized duplicate-path guard token: {token}"
        );
    }
}

#[test]
fn full_suite_gate_contains_path_hygiene_fail_closed_guards() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    for token in [
        "perf3x_bead_coverage_contract_fails_closed_on_parent_traversal_path",
        "perf3x_bead_coverage_contract_fails_closed_on_absolute_path",
        "perf3x_bead_coverage_contract_fails_closed_on_windows_absolute_path",
        "perf3x_bead_coverage_contract_fails_closed_on_unc_path",
    ] {
        assert!(
            gate.contains(token),
            "full suite gate must include path-hygiene guard token: {token}"
        );
    }
}

#[test]
fn ci_workflow_has_failure_output_guidance() {
    let ci = load_text(CI_WORKFLOW_PATH);
    // CI should produce structured output on failure
    assert!(
        ci.contains("evidence") || ci.contains("summary") || ci.contains("report"),
        "CI workflow must reference evidence/summary artifacts for failure diagnosis"
    );
}

#[test]
fn qa_runbook_maps_ci_gate_failures_to_remediation() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    // The runbook must document how to handle CI gate failures
    assert!(
        runbook.contains("CI Gate Thresholds"),
        "runbook must document CI gate thresholds"
    );
    assert!(
        runbook.contains("CI_GATE_PROMOTION_MODE"),
        "runbook must reference promotion mode for gate remediation"
    );
}

#[test]
fn qa_runbook_documents_rollback_procedure() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    // The runbook should have rollback/emergency procedure
    assert!(
        runbook.contains("rollback") || runbook.contains("Emergency"),
        "runbook must document rollback/emergency procedure"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 7: Documentation command validity
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn testing_policy_run_commands_reference_real_tools() {
    let policy = load_text(TESTING_POLICY_PATH);
    // Commands must reference real tools
    assert!(
        policy.contains("cargo test"),
        "policy must reference cargo test"
    );
    assert!(
        policy.contains("cargo test --all-targets --lib"),
        "policy must document unit test command"
    );
}

#[test]
fn qa_runbook_smoke_commands_reference_real_script() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("./scripts/smoke.sh"),
        "runbook must reference scripts/smoke.sh"
    );
    // Verify the script exists
    assert!(
        std::path::Path::new("scripts/smoke.sh").exists(),
        "scripts/smoke.sh must exist on disk"
    );
}

#[test]
fn qa_runbook_e2e_commands_reference_real_script() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("./scripts/e2e/run_all.sh"),
        "runbook must reference scripts/e2e/run_all.sh"
    );
    assert!(
        std::path::Path::new("scripts/e2e/run_all.sh").exists(),
        "scripts/e2e/run_all.sh must exist on disk"
    );
}

#[test]
fn testing_policy_suite_classification_path_accurate() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("tests/suite_classification.toml"),
        "policy must reference suite classification TOML"
    );
    assert!(
        std::path::Path::new(SUITE_CLASSIFICATION_PATH).exists(),
        "suite_classification.toml must exist on disk"
    );
}

#[test]
fn runbook_referenced_json_artifacts_exist() {
    // Validate that key JSON artifacts referenced in the runbook actually exist
    let artifacts = [
        NON_MOCK_RUBRIC_PATH,
        SCENARIO_MATRIX_PATH,
        COVERAGE_BASELINE_PATH,
        TEST_DOUBLE_INVENTORY_PATH,
    ];
    for path in &artifacts {
        assert!(
            std::path::Path::new(path).exists(),
            "runbook-referenced artifact must exist: {path}"
        );
        // Also verify it's valid JSON
        let content = std::fs::read_to_string(path).unwrap();
        assert!(
            serde_json::from_str::<Value>(&content).is_ok(),
            "artifact must be valid JSON: {path}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 8: Allowlist integrity and staleness detection
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn testing_policy_allowlist_entries_have_cleanup_beads() {
    let policy = load_text(TESTING_POLICY_PATH);
    // Allowlisted exceptions with cleanup tracking should reference bead IDs
    // The allowlist mentions cleanup tracked by beads
    assert!(
        policy.contains("bd-m9rk") || policy.contains("bd-"),
        "allowlist entries should reference tracking beads for cleanup"
    );
}

#[test]
fn testing_policy_rejected_doubles_are_explicit() {
    let policy = load_text(TESTING_POLICY_PATH);
    let rejected = ["DummyProvider", "NullSession", "NullUiHandler"];
    for name in &rejected {
        assert!(
            policy.contains(name),
            "testing-policy must explicitly list rejected double: {name}"
        );
    }
}

#[test]
fn testing_policy_exception_process_documented() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("Process for adding new exceptions"),
        "must document the process for adding new allowlist exceptions"
    );
}

#[test]
fn ci_allowlist_aligns_with_testing_policy() {
    let ci = load_text(CI_WORKFLOW_PATH);
    let policy = load_text(TESTING_POLICY_PATH);

    // CI now reads from .no-mock-allowlist at repo root instead of an
    // inline regex. The CI workflow must reference that file, and the
    // policy must document the canonical core exception (MockHttpServer).
    let ci_uses_allowlist_file = ci.contains(".no-mock-allowlist");
    let policy_documents_allowlist =
        policy.contains("MockHttpServer") && policy.contains(".no-mock-allowlist");

    assert!(
        ci_uses_allowlist_file,
        "CI workflow must reference .no-mock-allowlist for the no-mock policy gate"
    );
    assert!(
        policy_documents_allowlist,
        "testing-policy must document the .no-mock-allowlist file alongside MockHttpServer"
    );
}

#[test]
fn test_double_inventory_entry_count_matches_policy_baseline() {
    let inventory = load_json(TEST_DOUBLE_INVENTORY_PATH);
    let policy = load_text(TESTING_POLICY_PATH);

    // The inventory should have an entry_count
    let count = inventory["summary"]["entry_count"]
        .as_u64()
        .or_else(|| inventory["entry_count"].as_u64());
    assert!(count.is_some(), "inventory must report entry_count");

    // The testing-policy should reference the baseline count
    let count_val = count.unwrap();
    let count_str = count_val.to_string();
    assert!(
        policy.contains(&count_str) || policy.contains("entry_count"),
        "testing-policy should reference the inventory baseline count ({count_val})"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 9: Schema consistency across documentation artifacts
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn non_mock_rubric_schema_is_versioned() {
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);
    let schema = rubric["schema"]
        .as_str()
        .expect("rubric must have schema field");
    assert!(
        schema.starts_with("pi.qa.non_mock_rubric"),
        "rubric schema must be pi.qa.non_mock_rubric.*, got: {schema}"
    );
}

#[test]
fn scenario_matrix_schema_is_versioned() {
    let matrix = load_json(SCENARIO_MATRIX_PATH);
    let schema = matrix["schema"]
        .as_str()
        .expect("matrix must have schema field");
    assert!(
        schema.starts_with("pi.e2e.scenario_matrix"),
        "matrix schema must be pi.e2e.scenario_matrix.*, got: {schema}"
    );
}

#[test]
fn perf_sli_matrix_schema_is_versioned() {
    let matrix = load_json(PERF_SLI_MATRIX_PATH);
    let schema = matrix["schema"]
        .as_str()
        .expect("perf_sli_matrix must have schema field");
    assert!(
        schema.starts_with("pi.perf.sli_ux_matrix"),
        "perf_sli_matrix schema must be pi.perf.sli_ux_matrix.*, got: {schema}"
    );
}

#[test]
fn test_double_inventory_schema_is_versioned() {
    let inventory = load_json(TEST_DOUBLE_INVENTORY_PATH);
    let schema = inventory["schema"]
        .as_str()
        .expect("inventory must have schema field");
    assert!(
        schema.starts_with("pi.qa.test_double_inventory"),
        "inventory schema must be pi.qa.test_double_inventory.*, got: {schema}"
    );
}

#[test]
fn runtime_hostcall_telemetry_schema_is_versioned() {
    let schema = load_json(RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_PATH);
    assert_eq!(
        schema["properties"]["schema"]["enum"][0], "pi.ext.hostcall_telemetry.v1",
        "runtime hostcall telemetry schema id must be versioned and canonical"
    );
    assert_eq!(
        schema["$defs"]["event"]["properties"]["schema"]["enum"][0], "pi.ext.hostcall_telemetry.v1",
        "runtime hostcall telemetry event schema id must match artifact schema"
    );
}

#[test]
fn runtime_hostcall_telemetry_schema_requires_lane_and_marshalling_fields() {
    let schema = load_json(RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_PATH);
    let required: HashSet<String> = schema["$defs"]["event"]["required"]
        .as_array()
        .expect("runtime hostcall telemetry event.required must be an array")
        .iter()
        .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
        .collect();

    for field in [
        "lane",
        "lane_decision_reason",
        "lane_matrix_key",
        "lane_dispatch_latency_ms",
        "lane_latency_share_bps",
        "marshalling_path",
        "marshalling_latency_us",
        "marshalling_fallback_count",
        "marshalling_superinstruction_expected_cost_delta",
        "marshalling_superinstruction_observed_cost_delta",
    ] {
        assert!(
            required.contains(field),
            "runtime hostcall telemetry schema must require field: {field}"
        );
    }
}

#[test]
fn coverage_baseline_exists_and_has_critical_paths() {
    let baseline = load_json(COVERAGE_BASELINE_PATH);
    assert!(
        baseline["critical_paths"].is_array() || baseline["summary"].is_object(),
        "coverage baseline must have critical_paths or summary"
    );
}

#[test]
fn coverage_baseline_branch_metrics_are_non_null() {
    let baseline = load_json(COVERAGE_BASELINE_PATH);

    assert!(
        baseline["summary"]["branch_pct"].as_f64().is_some(),
        "coverage baseline summary.branch_pct must be numeric (fallback values are allowed)"
    );

    let critical_paths = baseline["critical_paths"]
        .as_array()
        .expect("coverage baseline must have critical_paths");

    for cp in critical_paths {
        let area = cp["area"].as_str().unwrap_or("<unknown>");
        let coverage = &cp["coverage"];
        assert!(
            coverage["branch_pct"].as_f64().is_some(),
            "coverage baseline critical path '{area}' must have numeric coverage.branch_pct"
        );
        assert!(
            coverage["branch_count"].as_u64().is_some(),
            "coverage baseline critical path '{area}' must have numeric coverage.branch_count"
        );
        assert!(
            coverage["covered_branch_count"].as_u64().is_some(),
            "coverage baseline critical path '{area}' must have numeric coverage.covered_branch_count"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 10: Operator runbook executable examples
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn qa_runbook_has_vcr_cassette_verification_commands() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("python3 -m json.tool"),
        "runbook must include JSON validation command for cassettes"
    );
    assert!(
        runbook.contains("verify_") || runbook.contains("cassette"),
        "runbook must reference VCR cassette verification"
    );
}

#[test]
fn qa_runbook_has_compliance_check_commands() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    assert!(
        runbook.contains("COMPLIANCE_REPORT=1"),
        "runbook must document compliance report generation"
    );
    assert!(
        runbook.contains("non_mock_compliance_gate") || runbook.contains("non_mock_rubric_gate"),
        "runbook must reference compliance/rubric gate tests"
    );
}

#[test]
fn qa_runbook_smoke_targets_match_suite_classification() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let classification = load_text(SUITE_CLASSIFICATION_PATH);

    // Smoke targets mentioned in the runbook should be in the suite classification
    let smoke_targets = [
        "model_serialization",
        "config_precedence",
        "session_conformance",
        "error_types",
        "provider_streaming",
        "error_handling",
        "http_client",
    ];
    for target in &smoke_targets {
        assert!(
            runbook.contains(target),
            "runbook should list smoke target: {target}"
        );
        assert!(
            classification.contains(target),
            "smoke target '{target}' must be in suite_classification.toml"
        );
    }
}

#[test]
fn flake_triage_evidence_artifacts_documented() {
    let policy = load_text(FLAKE_TRIAGE_PATH);
    let artifacts = [
        "flake_events.jsonl",
        "conformance_summary.json",
        "retry_manifest.json",
        "quarantine_report.json",
        "quarantine_audit.jsonl",
    ];
    for artifact in &artifacts {
        assert!(
            policy.contains(artifact),
            "flake triage must document evidence artifact: {artifact}"
        );
    }
}

#[test]
fn testing_policy_and_runbook_coverage_thresholds_agree() {
    let policy = load_text(TESTING_POLICY_PATH);
    let runbook = load_text(QA_RUNBOOK_PATH);
    let rubric = load_json(NON_MOCK_RUBRIC_PATH);

    // Find the "providers" module in the modules array
    let modules = rubric["module_thresholds"]["modules"]
        .as_array()
        .expect("modules array");
    let providers = modules
        .iter()
        .find(|m| m["name"].as_str() == Some("providers"));
    if let Some(providers) = providers
        && let Some(line_floor) = providers["line_floor_pct"].as_f64()
    {
        let floor_str = format!("{line_floor:.0}%");
        // At least one of policy or runbook should mention this threshold
        assert!(
            policy.contains(&floor_str) || runbook.contains(&floor_str),
            "provider line_floor_pct ({floor_str}) must appear in testing-policy or runbook"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 11: End-to-end documentation coverage gap detection
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn every_ci_gate_has_documented_artifact_path() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    let runbook = load_text(QA_RUNBOOK_PATH);

    // Gate references to artifact paths should also appear in the runbook
    let gate_artifacts = [
        "non-mock-rubric.json",
        "suite_classification.toml",
        "e2e_scenario_matrix.json",
    ];
    for artifact in &gate_artifacts {
        assert!(
            gate.contains(artifact),
            "full suite gate must reference: {artifact}"
        );
        assert!(
            runbook.contains(artifact),
            "runbook must also reference gate artifact: {artifact}"
        );
    }
}

#[test]
fn testing_policy_smoke_section_exists() {
    let policy = load_text(TESTING_POLICY_PATH);
    assert!(
        policy.contains("Fast Local Smoke Suite") || policy.contains("smoke.sh"),
        "testing-policy must document the smoke suite"
    );
}

#[test]
fn all_doc_files_referenced_in_runbook_exist() {
    let runbook = load_text(QA_RUNBOOK_PATH);
    let doc_refs = [
        "docs/testing-policy.md",
        "docs/non-mock-rubric.json",
        "docs/coverage-baseline-map.json",
        "docs/e2e_scenario_matrix.json",
    ];
    for path in &doc_refs {
        // Strip "docs/" prefix since runbook might use relative paths
        let short = path.trim_start_matches("docs/");
        assert!(runbook.contains(short), "runbook must reference {path}");
        assert!(
            std::path::Path::new(path).exists(),
            "referenced doc must exist: {path}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section: Unified Structured Logging Contract (bd-3ar8v.1.7)
// ═══════════════════════════════════════════════════════════════════════════

const EVIDENCE_LOGGING_CONTRACT_PATH: &str = "docs/schema/test_evidence_logging_contract.json";
const EVIDENCE_LOGGING_INSTANCE_PATH: &str = "docs/schema/test_evidence_logging_instance.json";
const EVIDENCE_LOGGING_CRITICAL_PERF3X_BEADS: &[&str] = &[
    "bd-3ar8v.2.8",
    "bd-3ar8v.3.8",
    "bd-3ar8v.4.7",
    "bd-3ar8v.4.8",
    "bd-3ar8v.4.9",
    "bd-3ar8v.4.10",
    "bd-3ar8v.6.11",
];

fn parse_perf3x_critical_beads_from_full_suite_gate_source(
    gate_source: &str,
) -> Result<Vec<String>, String> {
    let anchor = "const PERF3X_CRITICAL_BEADS: &[&str] = &[";
    let start = gate_source
        .find(anchor)
        .ok_or_else(|| "ci_full_suite_gate.rs must define PERF3X_CRITICAL_BEADS".to_string())?
        + anchor.len();
    let tail = &gate_source[start..];
    let end = tail.find("];").ok_or_else(|| {
        "ci_full_suite_gate.rs PERF3X_CRITICAL_BEADS must terminate with ];".to_string()
    })?;
    let body = &tail[..end];

    let mut beads = Vec::new();
    let mut seen = HashSet::new();

    for entry in body.split(',') {
        let token = entry.trim();
        if token.is_empty() {
            continue;
        }
        let bead = token
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or_else(|| {
                format!("PERF3X_CRITICAL_BEADS entry must be a quoted string literal: {token}")
            })?;
        if !bead.starts_with("bd-3ar8v.") {
            return Err(format!(
                "PERF3X_CRITICAL_BEADS entry must be a PERF-3X bead id, got: {bead}"
            ));
        }
        if !seen.insert(bead.to_string()) {
            return Err(format!(
                "PERF3X_CRITICAL_BEADS must not contain duplicates, saw: {bead}"
            ));
        }
        beads.push(bead.to_string());
    }

    if beads.is_empty() {
        return Err("PERF3X_CRITICAL_BEADS must not be empty".to_string());
    }

    Ok(beads)
}

#[test]
fn evidence_logging_contract_schema_exists_and_is_valid_json() {
    let schema = load_json(EVIDENCE_LOGGING_CONTRACT_PATH);
    assert_eq!(
        schema["$id"], "pi.test.evidence_logging_contract.v1",
        "contract schema must have correct $id"
    );
    assert_eq!(
        schema["version"], "1.0.0",
        "contract schema must be versioned"
    );
}

#[test]
fn evidence_logging_contract_defines_all_required_sections() {
    let schema = load_json(EVIDENCE_LOGGING_CONTRACT_PATH);
    let required = schema["required"]
        .as_array()
        .expect("must have required array");
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    for section in &[
        "schema_registry",
        "suite_requirements",
        "correlation_model",
        "perf_evidence_contract",
        "bead_coverage_contract",
    ] {
        assert!(
            required_strs.contains(section),
            "contract must require section: {section}"
        );
    }
}

#[test]
fn evidence_logging_contract_bead_coverage_policy_requires_critical_perf3x_beads() {
    let schema = load_json(EVIDENCE_LOGGING_CONTRACT_PATH);
    let required_fields = schema["definitions"]["bead_coverage_contract"]["properties"]
        ["coverage_policy"]["required"]
        .as_array()
        .expect("coverage_policy.required must be an array");

    let required_names: HashSet<&str> = required_fields.iter().filter_map(Value::as_str).collect();
    assert!(
        required_names.contains("critical_perf3x_beads"),
        "coverage_policy.required must include critical_perf3x_beads"
    );

    let critical = &schema["definitions"]["bead_coverage_contract"]["properties"]["coverage_policy"]
        ["properties"]["critical_perf3x_beads"];
    assert_eq!(
        critical["type"], "array",
        "critical_perf3x_beads must be declared as an array"
    );
    assert_eq!(
        critical["uniqueItems"], true,
        "critical_perf3x_beads must enforce unique members"
    );
    assert_eq!(
        critical["items"]["pattern"], "^bd-3ar8v\\.",
        "critical_perf3x_beads item pattern must enforce PERF-3X bead ids"
    );
}

#[test]
fn evidence_logging_instance_exists_and_has_schema_registry() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let schemas = instance["schema_registry"]["schemas"]
        .as_array()
        .expect("instance must have schema_registry.schemas array");
    assert!(
        schemas.len() >= 10,
        "registry must contain at least 10 schemas, found {}",
        schemas.len()
    );
}

#[test]
fn evidence_logging_instance_schema_registry_contains_all_canonical_schemas() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let schemas = instance["schema_registry"]["schemas"]
        .as_array()
        .expect("schemas array");
    let schema_ids: HashSet<&str> = schemas
        .iter()
        .filter_map(|s| s["schema_id"].as_str())
        .collect();

    let required_schemas = [
        "pi.test.log.v2",
        "pi.test.artifact.v1",
        "pi.qa.evidence_contract.v1",
        "pi.e2e.failure_digest.v1",
        "pi.parity.test_logging_contract.v1",
        "pi.ext.rust_bench.v1",
        "pi.perf.budget.v1",
        "pi.bench.protocol.v1",
        "pi.perf.sli_ux_matrix.v1",
        "pi.test.transcript.v1",
    ];
    for schema_id in &required_schemas {
        assert!(
            schema_ids.contains(schema_id),
            "registry must contain schema: {schema_id}"
        );
    }
}

#[test]
fn evidence_logging_instance_has_schema_relationships() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let relationships = instance["schema_registry"]["schema_relationships"]
        .as_array()
        .expect("must have schema_relationships array");
    assert!(
        relationships.len() >= 5,
        "must have at least 5 schema relationships, found {}",
        relationships.len()
    );
    // Verify evidence contract references test log schema
    let has_evidence_to_log = relationships.iter().any(|r| {
        r["from_schema"] == "pi.qa.evidence_contract.v1" && r["to_schema"] == "pi.test.log.v2"
    });
    assert!(
        has_evidence_to_log,
        "must have evidence_contract -> test_log relationship"
    );
}

#[test]
fn evidence_logging_instance_suite_requirements_cover_all_suites() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let suites = instance["suite_requirements"]
        .as_object()
        .expect("must have suite_requirements object");
    for suite in &["unit", "vcr", "e2e", "perf_bench", "perf_regression"] {
        assert!(
            suites.contains_key(*suite),
            "suite_requirements must define: {suite}"
        );
        let req = &suites[*suite];
        assert!(
            req["required_schemas"].as_array().is_some(),
            "{suite} must have required_schemas"
        );
        assert!(
            req["evidence_level"].as_str().is_some(),
            "{suite} must have evidence_level"
        );
    }
}

#[test]
fn evidence_logging_instance_e2e_suite_requires_forensic_evidence() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let e2e = &instance["suite_requirements"]["e2e"];
    assert_eq!(
        e2e["evidence_level"], "forensic",
        "e2e suite must require forensic evidence level"
    );
    assert_eq!(
        e2e["correlation_required"], true,
        "e2e suite must require correlation"
    );
    assert_eq!(
        e2e["artifact_checksums_required"], true,
        "e2e suite must require artifact checksums"
    );
}

#[test]
fn evidence_logging_instance_correlation_model_has_four_levels() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let levels = instance["correlation_model"]["levels"]
        .as_array()
        .expect("must have correlation levels");
    assert_eq!(
        levels.len(),
        4,
        "correlation model must have exactly 4 levels (ci_run, suite, test, operation)"
    );
    let scopes: Vec<&str> = levels.iter().filter_map(|l| l["scope"].as_str()).collect();
    assert!(scopes.contains(&"ci_run"));
    assert!(scopes.contains(&"suite"));
    assert!(scopes.contains(&"test"));
    assert!(scopes.contains(&"operation"));
}

#[test]
fn evidence_logging_instance_join_keys_are_complete() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let keys = instance["correlation_model"]["join_keys"]
        .as_object()
        .expect("must have join_keys object");
    for key in &[
        "ci_run_to_evidence",
        "evidence_to_suite",
        "suite_to_log",
        "log_to_span",
        "bench_to_evidence",
    ] {
        assert!(keys.contains_key(*key), "join_keys must define: {key}");
    }
}

#[test]
fn evidence_logging_instance_cross_domain_paths_exist() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let paths = instance["correlation_model"]["cross_domain_paths"]
        .as_array()
        .expect("must have cross_domain_paths");
    assert!(
        paths.len() >= 3,
        "must have at least 3 cross-domain forensic paths"
    );
    let names: Vec<&str> = paths.iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(
        names.contains(&"bead_to_evidence"),
        "must define bead_to_evidence path"
    );
    assert!(
        names.contains(&"failure_to_replay"),
        "must define failure_to_replay path"
    );
    assert!(
        names.contains(&"perf_claim_to_measurements"),
        "must define perf_claim_to_measurements path"
    );
}

#[test]
fn evidence_logging_instance_perf_evidence_contract_is_complete() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let perf = &instance["perf_evidence_contract"];
    assert_eq!(
        perf["schema"], "pi.perf.evidence.v1",
        "perf evidence must use pi.perf.evidence.v1 schema"
    );
    // Must define required record types
    let record_types = perf["required_record_types"]
        .as_array()
        .expect("must have required_record_types");
    assert!(
        record_types.len() >= 4,
        "must define at least 4 record types"
    );
    // Must define env fingerprint fields
    let env_fields = perf["env_fingerprint_fields"]
        .as_array()
        .expect("must have env_fingerprint_fields");
    assert!(
        env_fields.len() >= 5,
        "must require at least 5 env fingerprint fields"
    );
    // Must define no_data policy as hard_fail
    assert_eq!(
        perf["no_data_policy"]["action"], "hard_fail",
        "NO_DATA must be treated as hard failure"
    );
}

#[test]
fn evidence_logging_instance_bead_coverage_contract_is_complete() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let coverage = &instance["bead_coverage_contract"];
    assert_eq!(
        coverage["schema"], "pi.perf.bead_coverage.v1",
        "bead coverage must use pi.perf.bead_coverage.v1 schema"
    );
    // Must define coverage policy
    let policy = &coverage["coverage_policy"];
    assert_eq!(
        policy["min_evidence_per_bead"], 1,
        "every bead must have at least 1 evidence link"
    );
    // Must define allowed evidence types
    let types = policy["allowed_evidence_types"]
        .as_array()
        .expect("must have allowed_evidence_types");
    assert!(types.len() >= 4, "must allow at least 4 evidence types");
}

fn canonical_perf3x_bead_segments(bead_id: &str) -> Option<Vec<u64>> {
    let suffix = bead_id.strip_prefix("bd-3ar8v.")?;
    if suffix.is_empty() {
        return None;
    }
    suffix
        .split('.')
        .map(|segment| {
            if segment.is_empty() {
                return None;
            }
            segment.parse::<u64>().ok()
        })
        .collect::<Option<Vec<_>>>()
}

fn validate_critical_perf3x_bead_entries(entries: &[Value]) -> Result<HashSet<String>, String> {
    let mut declared = HashSet::new();
    let mut previous_segments: Option<Vec<u64>> = None;
    let mut previous_bead_id: Option<String> = None;

    for (index, entry) in entries.iter().enumerate() {
        let bead_id = entry
            .as_str()
            .ok_or_else(|| format!("critical_perf3x_beads[{index}] must be a string"))?
            .trim();
        if !bead_id.starts_with("bd-3ar8v.") {
            return Err(format!(
                "critical_perf3x_beads[{index}] must be a PERF-3X bead id, got: {bead_id}"
            ));
        }

        let bead_segments = canonical_perf3x_bead_segments(bead_id).ok_or_else(|| {
            format!(
                "critical_perf3x_beads[{index}] must be canonical numeric PERF-3X id: {bead_id}"
            )
        })?;
        if let Some(previous) = previous_segments.as_ref()
            && bead_segments < *previous
        {
            let previous_bead = previous_bead_id.as_deref().unwrap_or("<unknown>");
            return Err(format!(
                "critical_perf3x_beads must be sorted by canonical bead id order: index {index} '{bead_id}' appears after '{previous_bead}'"
            ));
        }
        previous_segments = Some(bead_segments);
        previous_bead_id = Some(bead_id.to_string());

        if !declared.insert(bead_id.to_string()) {
            return Err(format!(
                "critical_perf3x_beads must not contain duplicates, saw: {bead_id}"
            ));
        }
    }

    Ok(declared)
}

#[test]
fn evidence_logging_instance_bead_coverage_policy_declares_critical_perf3x_beads() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let critical = instance["bead_coverage_contract"]["coverage_policy"]["critical_perf3x_beads"]
        .as_array()
        .expect("must have critical_perf3x_beads array");

    let declared = validate_critical_perf3x_bead_entries(critical)
        .expect("critical_perf3x_beads should be valid and deterministically ordered");

    for required in EVIDENCE_LOGGING_CRITICAL_PERF3X_BEADS {
        assert!(
            declared.contains(*required),
            "critical_perf3x_beads must include required bead: {required}"
        );
    }
}

#[test]
fn evidence_logging_instance_critical_perf3x_beads_fail_closed_when_unsorted() {
    let unsorted = vec![
        Value::String("bd-3ar8v.4.10".to_string()),
        Value::String("bd-3ar8v.2.8".to_string()),
    ];

    let err = validate_critical_perf3x_bead_entries(&unsorted)
        .expect_err("unsorted critical_perf3x_beads must fail closed");
    assert!(
        err.contains("sorted by canonical bead id order"),
        "unexpected error for unsorted critical bead list: {err}"
    );
}

#[test]
fn evidence_logging_critical_perf3x_beads_match_full_suite_gate_contract() {
    let gate = load_text(FULL_SUITE_GATE_PATH);
    let gate_beads = parse_perf3x_critical_beads_from_full_suite_gate_source(&gate)
        .expect("PERF3X_CRITICAL_BEADS in ci_full_suite_gate.rs should be valid");
    let expected_beads: Vec<String> = EVIDENCE_LOGGING_CRITICAL_PERF3X_BEADS
        .iter()
        .map(|bead| (*bead).to_string())
        .collect();

    assert_eq!(
        gate_beads, expected_beads,
        "critical PERF-3X bead set/order drift between ci_full_suite_gate.rs and evidence logging contract"
    );
}

#[test]
fn evidence_logging_instance_schema_source_files_exist() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let schemas = instance["schema_registry"]["schemas"]
        .as_array()
        .expect("schemas array");
    for schema in schemas {
        let schema_id = schema["schema_id"].as_str().unwrap_or("unknown");
        let sources = schema["source_files"]
            .as_array()
            .expect("schema must have source_files");
        for src in sources {
            let path = src.as_str().unwrap();
            assert!(
                std::path::Path::new(path).exists(),
                "source file for {schema_id} must exist: {path}"
            );
        }
    }
}

#[test]
fn evidence_logging_contract_perf_statistical_fields_include_required_percentiles() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let percentiles =
        instance["perf_evidence_contract"]["statistical_fields"]["required_percentiles"]
            .as_array()
            .expect("must have required_percentiles");
    let p_strs: Vec<&str> = percentiles.iter().filter_map(|v| v.as_str()).collect();
    for p in &["p50", "p95", "p99"] {
        assert!(p_strs.contains(p), "required_percentiles must include {p}");
    }
    let aggregates =
        instance["perf_evidence_contract"]["statistical_fields"]["required_aggregates"]
            .as_array()
            .expect("must have required_aggregates");
    let a_strs: Vec<&str> = aggregates.iter().filter_map(|v| v.as_str()).collect();
    for a in &["min", "max", "mean", "count"] {
        assert!(a_strs.contains(a), "required_aggregates must include {a}");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section: Reproducible Benchmark/Test Orchestration (bd-3ar8v.1.8)
// ═══════════════════════════════════════════════════════════════════════════

const ORCHESTRATE_SCRIPT_PATH: &str = "scripts/perf/orchestrate.sh";
const BUNDLE_SCRIPT_PATH: &str = "scripts/perf/bundle.sh";
const BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH: &str = "scripts/bench_extension_workloads.sh";

fn write_stub_command(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("failed to write stub command");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .expect("failed to stat stub command")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("failed to set executable permissions");
    }
}

fn pgo_events_from_dir(out_dir: &Path) -> Vec<Value> {
    let events_path = out_dir.join("pgo_pipeline_events.jsonl");
    let content = std::fs::read_to_string(&events_path).expect("failed to read PGO events");

    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("invalid PGO event JSON line"))
        .collect()
}

#[allow(clippy::literal_string_with_formatting_args)] // bash ${VAR} syntax, not Rust fmt
fn run_bench_extension_workloads_with_stubs(
    pgo_mode: &str,
    allow_fallback: bool,
    profile_data: Option<&[u8]>,
    llvm_profdata_show_ok: bool,
) -> (Output, tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("create temp test directory");
    let fake_bin = temp_dir.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake-bin directory");

    write_stub_command(
        &fake_bin.join("cargo"),
        r#"#!/usr/bin/env bash
set -euo pipefail
target_dir="${CARGO_TARGET_DIR:-target}"
profile="debug"
args=("$@")
saw_pijs_example=0
for ((idx = 0; idx < ${#args[@]}; idx++)); do
  if [[ "${args[$idx]}" == "--bin" ]]; then
    echo "pijs_workload must be built as a Cargo example, not --bin" >&2
    exit 44
  fi
  if [[ "${args[$idx]}" == "--example" ]]; then
    next=$((idx + 1))
    if [[ $next -lt ${#args[@]} && "${args[$next]}" == "pijs_workload" ]]; then
      saw_pijs_example=1
    fi
  fi
  if [[ "${args[$idx]}" == "--profile" ]]; then
    next=$((idx + 1))
    if [[ $next -lt ${#args[@]} ]]; then
      profile="${args[$next]}"
    fi
  fi
done
if [[ "$saw_pijs_example" != "1" ]]; then
  echo "missing --example pijs_workload" >&2
  exit 45
fi
bin="$target_dir/$profile/examples/pijs_workload"
mkdir -p "$(dirname "$bin")"
cat > "$bin" <<'EOF'
#!/usr/bin/env bash
echo '{"schema":"pi.perf.workload.stub.v1"}'
EOF
chmod +x "$bin"
"#,
    );

    write_stub_command(
        &fake_bin.join("hyperfine"),
        r#"#!/usr/bin/env bash
set -euo pipefail
export_json=""
while [[ $# -gt 0 ]]; do
  if [[ "$1" == "--export-json" ]]; then
    export_json="$2"
    shift 2
    continue
  fi
  shift
done
if [[ -z "$export_json" ]]; then
  echo "missing --export-json" >&2
  exit 1
fi
mkdir -p "$(dirname "$export_json")"
cat > "$export_json" <<'EOF'
{"results":[{"mean":1.0}]}
EOF
"#,
    );

    write_stub_command(
        &fake_bin.join("llvm-profdata"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "show" ]]; then
  if [[ "${LLVM_PROFDATA_SHOW_OK:-1}" == "1" ]]; then
    exit 0
  fi
  exit 1
fi
if [[ "${1:-}" == "merge" ]]; then
  out=""
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == "-o" ]]; then
      out="${2:-}"
      shift 2
      continue
    fi
    shift
  done
  if [[ -n "$out" ]]; then
    echo "merged-profile" > "$out"
  fi
  exit 0
fi
exit 0
"#,
    );

    let mut path_entries = vec![fake_bin];
    if let Some(existing_path) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&existing_path));
    }
    let joined_path = std::env::join_paths(path_entries).expect("join PATH");

    let target_dir = temp_dir.path().join("target");
    let out_dir = temp_dir.path().join("out");
    let profile_data_path = temp_dir.path().join("profiles/pijs_workload.profdata");
    if let Some(bytes) = profile_data {
        if let Some(parent) = profile_data_path.parent() {
            std::fs::create_dir_all(parent).expect("create profile data parent directory");
        }
        std::fs::write(&profile_data_path, bytes).expect("write profile data fixture");
    }

    let output = Command::new("bash")
        .arg(BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PATH", joined_path)
        .env("BENCH_CARGO_RUNNER", "local")
        .env("BENCH_CARGO_PROFILE", "perf")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("OUT_DIR", &out_dir)
        .env("BENCH_ALLOCATORS_CSV", "system")
        .env("BENCH_PGO_MODE", pgo_mode)
        .env(
            "BENCH_PGO_ALLOW_FALLBACK",
            if allow_fallback { "1" } else { "0" },
        )
        .env("BENCH_PGO_PROFILE_DATA", &profile_data_path)
        .env(
            "LLVM_PROFDATA_SHOW_OK",
            if llvm_profdata_show_ok { "1" } else { "0" },
        )
        .env("ITERATIONS", "1")
        .env("TOOL_CALLS_CSV", "1")
        .env("HYPERFINE_WARMUP", "0")
        .env("HYPERFINE_RUNS", "1")
        .env("BENCH_PGO_TRAIN_ITERATIONS", "1")
        .env("BENCH_PGO_TRAIN_TOOL_CALLS", "1")
        .output()
        .expect("run bench_extension_workloads.sh with stubs");

    (output, temp_dir, out_dir)
}

#[test]
fn orchestrate_script_exists_and_is_executable() {
    let path = std::path::Path::new(ORCHESTRATE_SCRIPT_PATH);
    assert!(
        path.exists(),
        "orchestrate.sh must exist at {ORCHESTRATE_SCRIPT_PATH}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(path).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "orchestrate.sh must be executable"
        );
    }
}

#[test]
fn bundle_script_exists_and_is_executable() {
    let path = std::path::Path::new(BUNDLE_SCRIPT_PATH);
    assert!(
        path.exists(),
        "bundle.sh must exist at {BUNDLE_SCRIPT_PATH}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(path).unwrap().permissions();
        assert!(perms.mode() & 0o111 != 0, "bundle.sh must be executable");
    }
}

#[test]
fn orchestrate_script_defines_all_required_suites() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    let required_suites = [
        "bench_schema",
        "bench_scenario",
        "perf_bench_harness",
        "perf_budgets",
        "perf_regression",
        "perf_comparison",
    ];

    for suite in &required_suites {
        assert!(
            content.contains(suite),
            "orchestrate.sh must reference suite: {suite}"
        );
    }
}

#[test]
fn orchestrate_script_defines_required_profiles() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    for profile in &["full", "quick", "ci"] {
        assert!(
            content.contains(profile),
            "orchestrate.sh must support profile: {profile}"
        );
    }
}

#[test]
fn orchestrate_script_generates_manifest_with_required_schema() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("pi.perf.run_manifest.v1"),
        "orchestrate.sh must emit manifest with schema pi.perf.run_manifest.v1"
    );

    let required_manifest_fields = [
        "correlation_id",
        "git_commit",
        "run_summary",
        "suite_results",
        "contract_refs",
    ];

    for field in &required_manifest_fields {
        assert!(
            content.contains(field),
            "manifest must include field: {field}"
        );
    }
}

#[test]
fn orchestrate_script_generates_checksums() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("sha256sum") || content.contains("sha256"),
        "orchestrate.sh must generate SHA-256 checksums"
    );

    assert!(
        content.contains("checksums.sha256"),
        "orchestrate.sh must write checksums.sha256 file"
    );
}

#[test]
fn orchestrate_script_generates_env_fingerprint() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("env_fingerprint"),
        "orchestrate.sh must generate environment fingerprint"
    );

    for field in &["cpu_model", "cpu_cores", "mem_total_mb", "build_profile"] {
        assert!(
            content.contains(field),
            "env fingerprint must include field: {field}"
        );
    }
}

#[test]
fn orchestrate_script_generates_baseline_variance_confidence_artifact() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("baseline_variance_confidence.json"),
        "orchestrate.sh must emit baseline_variance_confidence.json"
    );
    assert!(
        content.contains("pi.perf.baseline_variance_confidence.v1"),
        "orchestrate.sh must emit pi.perf.baseline_variance_confidence.v1 schema"
    );

    let required_fields = [
        "scenario_id",
        "sli_id",
        "confidence",
        "ci95_lower_ms",
        "ci95_upper_ms",
        "run_id_lineage",
        "environment_fingerprint_hash",
    ];

    for field in &required_fields {
        assert!(
            content.contains(field),
            "baseline variance/confidence artifact must include field: {field}"
        );
    }
}

#[test]
fn orchestrate_script_generates_pgo_pipeline_summary_artifact() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("pgo_pipeline_summary.json"),
        "orchestrate.sh must emit pgo_pipeline_summary.json"
    );
    assert!(
        content.contains("pi.perf.pgo_pipeline_summary.v1"),
        "orchestrate.sh must emit pi.perf.pgo_pipeline_summary.v1 schema"
    );

    for field in &[
        "pgo_mode_requested",
        "pgo_mode_effective",
        "profile_data_state",
        "fallback",
        "comparison_artifacts",
    ] {
        assert!(
            content.contains(field),
            "pgo pipeline summary must include field: {field}"
        );
    }
}

#[test]
fn orchestrate_script_references_contract_schemas() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    let contract_schemas = [
        "pi.test.evidence_logging_contract.v1",
        "pi.qa.evidence_contract.v1",
        "pi.bench.protocol.v1",
        "pi.perf.sli_ux_matrix.v1",
    ];

    for schema in &contract_schemas {
        assert!(
            content.contains(schema),
            "orchestrate.sh must reference contract schema: {schema}"
        );
    }
}

#[test]
fn orchestrate_script_supports_correlation_id() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("CI_CORRELATION_ID"),
        "orchestrate.sh must accept CI_CORRELATION_ID env var"
    );

    assert!(
        content.contains("correlation_id") || content.contains("CORRELATION_ID"),
        "orchestrate.sh must propagate correlation ID to suites"
    );
}

#[test]
fn bundle_script_supports_required_operations() {
    let content = load_text(BUNDLE_SCRIPT_PATH);

    let required_ops = ["--verify", "--extract", "--list", "--inventory", "--latest"];

    for op in &required_ops {
        assert!(
            content.contains(op),
            "bundle.sh must support operation: {op}"
        );
    }
}

#[test]
fn bundle_script_generates_metadata_sidecar() {
    let content = load_text(BUNDLE_SCRIPT_PATH);

    assert!(
        content.contains("pi.perf.bundle_meta.v1"),
        "bundle.sh must emit metadata with schema pi.perf.bundle_meta.v1"
    );

    assert!(
        content.contains("bundle_sha256"),
        "bundle metadata must include bundle_sha256 checksum"
    );
}

#[test]
fn bundle_script_generates_inventory() {
    let content = load_text(BUNDLE_SCRIPT_PATH);

    assert!(
        content.contains("pi.perf.bundle_inventory.v1"),
        "bundle.sh must emit inventory with schema pi.perf.bundle_inventory.v1"
    );

    assert!(
        content.contains("inventory.json"),
        "bundle.sh must write inventory.json"
    );
}

#[test]
fn bundle_script_verifies_checksums_before_bundling() {
    let content = load_text(BUNDLE_SCRIPT_PATH);

    assert!(
        content.contains("checksums.sha256"),
        "bundle.sh must verify checksums.sha256 during bundling"
    );

    assert!(
        content.contains("sha256sum"),
        "bundle.sh must use sha256sum for integrity verification"
    );
}

#[test]
fn bench_extension_workloads_script_exists_and_is_executable() {
    let path = std::path::Path::new(BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH);
    assert!(
        path.exists(),
        "bench_extension_workloads.sh must exist at {BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(path).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "bench_extension_workloads.sh must be executable"
        );
    }
}

#[test]
fn bench_extension_workloads_script_supports_pgo_modes_and_fallback() {
    let content = load_text(BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH);

    for token in &[
        "BENCH_PGO_MODE",
        "off|train|use|compare",
        "BENCH_PGO_PROFILE_DATA",
        "BENCH_PGO_ALLOW_FALLBACK",
        "missing_profile_data",
        "corrupt_profile_data",
    ] {
        assert!(
            content.contains(token),
            "bench_extension_workloads.sh must include token: {token}"
        );
    }
}

#[test]
fn bench_extension_workloads_script_emits_pgo_comparison_and_event_schemas() {
    let content = load_text(BENCH_EXTENSION_WORKLOADS_SCRIPT_PATH);

    assert!(
        content.contains("pi.perf.pgo_pipeline_event.v1"),
        "bench_extension_workloads.sh must emit pi.perf.pgo_pipeline_event.v1 records"
    );
    assert!(
        content.contains("pi.perf.pgo_comparison.v1"),
        "bench_extension_workloads.sh must emit pi.perf.pgo_comparison.v1 comparison artifacts"
    );
    assert!(
        content.contains("pgo_delta_"),
        "bench_extension_workloads.sh must generate pgo_delta_*.json comparison files"
    );
}

#[test]
fn bench_extension_workloads_use_mode_missing_profile_falls_back_and_emits_event() {
    let (output, _temp_dir, out_dir) =
        run_bench_extension_workloads_with_stubs("use", true, None, true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "script should succeed with fallback enabled; stderr: {stderr}"
    );

    let events = pgo_events_from_dir(&out_dir);
    let build_event = events
        .iter()
        .find(|event| event["phase"].as_str() == Some("build"))
        .expect("build event must be emitted");

    assert_eq!(
        build_event["pgo_mode_requested"].as_str(),
        Some("use"),
        "build event should preserve requested mode"
    );
    assert_eq!(
        build_event["profile_data_state"].as_str(),
        Some("missing"),
        "missing profile must be recorded"
    );
    assert_eq!(
        build_event["pgo_mode_effective"].as_str(),
        Some("baseline_fallback"),
        "missing profile with fallback enabled must use baseline fallback mode"
    );
    assert_eq!(
        build_event["fallback_reason"].as_str(),
        Some("missing_profile_data"),
        "fallback reason should be explicit for missing profile data"
    );
}

#[test]
fn bench_extension_workloads_use_mode_corrupt_profile_falls_back_and_marks_corrupt() {
    let (output, _temp_dir, out_dir) =
        run_bench_extension_workloads_with_stubs("use", true, Some(b"corrupt-data"), false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "script should succeed with fallback enabled; stderr: {stderr}"
    );

    let events = pgo_events_from_dir(&out_dir);
    let build_event = events
        .iter()
        .find(|event| event["phase"].as_str() == Some("build"))
        .expect("build event must be emitted");

    assert_eq!(
        build_event["profile_data_state"].as_str(),
        Some("corrupt"),
        "corrupt profile data must be recorded"
    );
    assert_eq!(
        build_event["pgo_mode_effective"].as_str(),
        Some("baseline_fallback"),
        "corrupt profile with fallback enabled must use baseline fallback mode"
    );
    assert_eq!(
        build_event["fallback_reason"].as_str(),
        Some("corrupt_profile_data"),
        "fallback reason should identify corrupt profile data"
    );
}

#[test]
fn bench_extension_workloads_use_mode_missing_profile_fails_closed_without_fallback() {
    let (output, _temp_dir, _out_dir) =
        run_bench_extension_workloads_with_stubs("use", false, None, true);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "script must fail when profile data is missing and fallback is disabled"
    );
    assert!(
        stderr.contains("fallback disabled"),
        "stderr should explain fail-closed behavior; stderr: {stderr}"
    );
}

#[test]
fn bench_extension_workloads_build_event_captures_profile_and_build_log_for_reproducibility() {
    let (output, _temp_dir, out_dir) =
        run_bench_extension_workloads_with_stubs("use", true, None, true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "script should succeed with fallback enabled; stderr: {stderr}"
    );

    let events = pgo_events_from_dir(&out_dir);
    let build_event = events
        .iter()
        .find(|event| event["phase"].as_str() == Some("build"))
        .expect("build event must be emitted");

    assert_eq!(
        build_event["build_profile"].as_str(),
        Some("perf"),
        "build event must record active cargo profile"
    );

    let build_log_path = build_event["build_log"]
        .as_str()
        .expect("build event must include build_log path");
    assert!(
        !build_log_path.trim().is_empty(),
        "build_log path must be non-empty"
    );
    assert!(
        Path::new(build_log_path).exists(),
        "build_log path from event must exist: {build_log_path}"
    );

    let profile_data_path = build_event["profile_data_path"]
        .as_str()
        .expect("build event must include profile_data_path");
    assert!(
        profile_data_path.ends_with("pijs_workload.profdata"),
        "profile_data_path should preserve canonical profdata path naming"
    );
}

#[test]
fn bench_extension_workloads_compare_mode_emits_reproducible_artifact_lineage() {
    let (output, _temp_dir, out_dir) =
        run_bench_extension_workloads_with_stubs("compare", true, None, true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "compare mode should succeed with fallback enabled; stderr: {stderr}"
    );

    let events = pgo_events_from_dir(&out_dir);
    let comparison_event = events
        .iter()
        .find(|event| event["phase"].as_str() == Some("comparison"))
        .expect("compare mode must emit a comparison event");

    let comparison_json_path = comparison_event["comparison_json"]
        .as_str()
        .expect("comparison event must include comparison_json path");
    let comparison_json = Path::new(comparison_json_path);
    assert!(
        comparison_json.exists(),
        "comparison artifact path from event must exist: {comparison_json_path}"
    );
    assert!(
        comparison_json.starts_with(&out_dir),
        "comparison artifact should be rooted under OUT_DIR for reproducibility"
    );

    let comparison_payload: Value = serde_json::from_str(
        &std::fs::read_to_string(comparison_json).expect("read comparison artifact payload"),
    )
    .expect("parse comparison artifact payload");

    assert_eq!(
        comparison_payload["schema"].as_str(),
        Some("pi.perf.pgo_comparison.v1"),
        "comparison artifact must use expected schema"
    );
    assert_eq!(
        comparison_payload["build_profile"].as_str(),
        Some("perf"),
        "comparison artifact must preserve build profile used for benchmark"
    );

    for key in &["baseline_hyperfine_json", "pgo_hyperfine_json"] {
        let path_value = comparison_payload[*key]
            .as_str()
            .expect("comparison artifact must include artifact path");
        assert!(
            Path::new(path_value).exists(),
            "{key} artifact path must exist: {path_value}"
        );
    }
}

#[test]
fn orchestrate_script_supports_validate_only_mode() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("--validate-only"),
        "orchestrate.sh must support --validate-only mode for existing bundles"
    );
}

#[test]
fn orchestrate_script_has_deterministic_parallelism_default() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);

    assert!(
        content.contains("PARALLELISM=\"${PERF_PARALLELISM:-1}\"")
            || content.contains("PARALLELISM=${PERF_PARALLELISM:-1}"),
        "orchestrate.sh must default parallelism to 1 for determinism"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Section: Baseline Variance and Confidence Bands (bd-3ar8v.1.5)
// ═══════════════════════════════════════════════════════════════════════════

const CAPTURE_BASELINE_SCRIPT_PATH: &str = "scripts/perf/capture_baseline.sh";

#[test]
fn capture_baseline_script_exists() {
    let path = std::path::Path::new(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        path.exists(),
        "capture_baseline.sh must exist at {CAPTURE_BASELINE_SCRIPT_PATH}"
    );
}

#[test]
fn capture_baseline_script_emits_variance_schema() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        content.contains("pi.perf.baseline_variance.v1"),
        "capture_baseline.sh must emit schema pi.perf.baseline_variance.v1"
    );
}

#[test]
fn capture_baseline_script_computes_confidence_intervals() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        content.contains("confidence_interval_95"),
        "must compute 95% confidence intervals"
    );
    assert!(
        content.contains("confidence_interval_99"),
        "must compute 99% confidence intervals"
    );
}

#[test]
fn capture_baseline_script_computes_coefficient_of_variation() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        content.contains("coefficient_of_variation"),
        "must compute coefficient of variation"
    );
}

#[test]
fn capture_baseline_script_classifies_variance() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        content.contains("var_class"),
        "must classify variance levels"
    );
}

#[test]
fn capture_baseline_script_supports_validation_mode() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    assert!(
        content.contains("--validate"),
        "must support --validate mode for existing baselines"
    );
}

#[test]
fn capture_baseline_script_supports_cross_environment_diagnosis_mode() {
    let content = load_text(CAPTURE_BASELINE_SCRIPT_PATH);
    for token in [
        "--diagnose-env",
        "--diagnose-output",
        "--variance-alert-pct",
        "pi.perf.cross_env_variance_diagnosis.v1",
        "pi.perf.cross_env_variance_diagnostic.v1",
    ] {
        assert!(
            content.contains(token),
            "capture_baseline.sh must support cross-env diagnosis token: {token}"
        );
    }
}

#[test]
fn baseline_variance_schema_in_evidence_instance() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let schemas = instance["schema_registry"]["schemas"]
        .as_array()
        .expect("must have schemas");
    let found = schemas
        .iter()
        .any(|s| s["schema_id"].as_str() == Some("pi.perf.baseline_variance.v1"));
    assert!(
        found,
        "pi.perf.baseline_variance.v1 must be in schema registry"
    );
}

#[test]
fn run_manifest_schema_in_evidence_instance() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let schemas = instance["schema_registry"]["schemas"]
        .as_array()
        .expect("must have schemas");
    let found = schemas
        .iter()
        .any(|s| s["schema_id"].as_str() == Some("pi.perf.run_manifest.v1"));
    assert!(found, "pi.perf.run_manifest.v1 must be in schema registry");
}

#[test]
fn baseline_variance_has_schema_relationships() {
    let instance = load_json(EVIDENCE_LOGGING_INSTANCE_PATH);
    let relationships = instance["schema_registry"]["schema_relationships"]
        .as_array()
        .expect("must have relationships");
    let has_baseline_rel = relationships
        .iter()
        .any(|r| r["from_schema"].as_str() == Some("pi.perf.baseline_variance.v1"));
    assert!(
        has_baseline_rel,
        "baseline_variance schema must have relationship entries"
    );
}

#[test]
fn orchestrate_script_includes_baseline_variance_suite() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);
    assert!(
        content.contains("perf_baseline_variance"),
        "orchestrate.sh must include perf_baseline_variance in suite registry"
    );
}

#[test]
fn orchestrate_script_supports_cross_environment_diagnosis_flow() {
    let content = load_text(ORCHESTRATE_SCRIPT_PATH);
    for token in [
        "PERF_CROSS_ENV_BASELINES",
        "PERF_CROSS_ENV_VARIANCE_ALERT_PCT",
        "PERF_CROSS_ENV_ENFORCE",
        "cross_env_variance_diagnosis",
        "--diagnose-env",
        "pi.perf.cross_env_variance_diagnosis.v1",
    ] {
        assert!(
            content.contains(token),
            "orchestrate.sh must include cross-env diagnosis token: {token}"
        );
    }
}

#[test]
fn capture_baseline_cross_env_diagnosis_emits_structured_report_and_log() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let baseline_ci = temp.path().join("baseline_ci.json");
    let baseline_canary = temp.path().join("baseline_canary.json");
    let diagnosis_out = temp.path().join("cross_env_diagnosis.json");

    let baseline_ci_payload = serde_json::json!({
        "schema": "pi.perf.baseline_variance.v1",
        "version": "1.0.0",
        "git_commit": "aaaaaaaa",
        "measurement_rounds": 5,
        "warmup_rounds": 1,
        "metrics": [
            {
                "metric_name": "latency_ms",
                "mean": 100.0,
                "coefficient_of_variation": 0.02,
                "variance_class": "low"
            },
            {
                "metric_name": "throughput_ops",
                "mean": 1000.0,
                "coefficient_of_variation": 0.03,
                "variance_class": "low"
            }
        ]
    });
    let baseline_canary_payload = serde_json::json!({
        "schema": "pi.perf.baseline_variance.v1",
        "version": "1.0.0",
        "git_commit": "bbbbbbbb",
        "measurement_rounds": 5,
        "warmup_rounds": 1,
        "metrics": [
            {
                "metric_name": "latency_ms",
                "mean": 140.0,
                "coefficient_of_variation": 0.03,
                "variance_class": "low"
            },
            {
                "metric_name": "throughput_ops",
                "mean": 1010.0,
                "coefficient_of_variation": 0.03,
                "variance_class": "low"
            }
        ]
    });

    std::fs::write(
        &baseline_ci,
        serde_json::to_string_pretty(&baseline_ci_payload).expect("serialize ci baseline"),
    )
    .expect("write ci baseline fixture");
    std::fs::write(
        &baseline_canary,
        serde_json::to_string_pretty(&baseline_canary_payload).expect("serialize canary baseline"),
    )
    .expect("write canary baseline fixture");

    let output = Command::new("bash")
        .arg(CAPTURE_BASELINE_SCRIPT_PATH)
        .arg("--diagnose-env")
        .arg(format!("ci={}", baseline_ci.display()))
        .arg("--diagnose-env")
        .arg(format!("canary={}", baseline_canary.display()))
        .arg("--diagnose-output")
        .arg(&diagnosis_out)
        .arg("--variance-alert-pct")
        .arg("10.0")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run capture_baseline cross-env diagnosis");

    assert!(
        output.status.success(),
        "capture_baseline diagnosis failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        diagnosis_out.exists(),
        "cross-env diagnosis report must be created: {}",
        diagnosis_out.display()
    );

    let report_text =
        std::fs::read_to_string(&diagnosis_out).expect("failed to read cross-env diagnosis report");
    let report: Value = serde_json::from_str(&report_text).expect("parse diagnosis report JSON");

    assert_eq!(
        report["schema"].as_str(),
        Some("pi.perf.cross_env_variance_diagnosis.v1"),
        "diagnosis report must use cross-env diagnosis schema"
    );
    assert_eq!(
        report["summary"]["environment_count"].as_u64(),
        Some(2),
        "report summary must include environment_count=2"
    );
    assert_eq!(
        report["summary"]["metric_count"].as_u64(),
        Some(2),
        "report summary must include both common metrics"
    );
    assert_eq!(
        report["summary"]["alert_count"].as_u64(),
        Some(1),
        "report must trigger one alert for latency spread above threshold"
    );

    let diagnostics_log_path = report["diagnostics_log"]["jsonl_path"]
        .as_str()
        .expect("diagnostics_log.jsonl_path must be present");
    let diagnostics_log = Path::new(diagnostics_log_path);
    assert!(
        diagnostics_log.exists(),
        "diagnostics jsonl must be written: {}",
        diagnostics_log.display()
    );

    let diagnostics_lines =
        std::fs::read_to_string(diagnostics_log).expect("read diagnostics JSONL log");
    let parsed_lines: Vec<Value> = diagnostics_lines
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse diagnostics JSONL line"))
        .collect();
    assert_eq!(
        parsed_lines.len(),
        2,
        "diagnostics log must emit one line per common metric"
    );

    for entry in parsed_lines {
        assert_eq!(
            entry["schema"].as_str(),
            Some("pi.perf.cross_env_variance_diagnostic.v1"),
            "diagnostics log entries must use cross-env diagnostic schema"
        );
        assert!(
            entry["metric_name"].as_str().is_some(),
            "diagnostics entries must include metric_name"
        );
    }
}

#[test]
fn binary_size_budget_threshold_is_consistent_between_perf_sources() {
    let perf_budget_threshold = binary_size_threshold_from_perf_budgets_source();
    let perf_regression_threshold = binary_size_threshold_from_perf_regression_source();
    assert!(
        (perf_budget_threshold - perf_regression_threshold).abs() < f64::EPSILON,
        "binary-size threshold drift: perf_budgets={perf_budget_threshold}, perf_regression={perf_regression_threshold}"
    );
}

#[test]
fn binary_size_budget_event_threshold_matches_perf_budget_source() {
    let expected_threshold = binary_size_threshold_from_perf_budgets_source();
    let events = load_text("tests/perf/reports/budget_events.jsonl");
    let binary_event = events
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("budget event line must be JSON"))
        .find(|event| event["budget_name"].as_str() == Some("binary_size_release"))
        .expect("budget_events.jsonl must include binary_size_release record");
    let actual_threshold = binary_event["threshold"]
        .as_f64()
        .expect("binary_size_release event must include numeric threshold");
    assert!(
        (actual_threshold - expected_threshold).abs() < f64::EPSILON,
        "binary-size event threshold drift: event={actual_threshold}, source={expected_threshold}"
    );
}

#[test]
fn governance_docs_reject_deprecated_bv_robot_flags() {
    let monitored_docs = [PLAN_TO_COMPLETE_PORT_PATH, PROGRAM_GOVERNANCE_PATH];
    for path in monitored_docs {
        let doc = load_text(path);
        assert!(
            !doc.contains("--robot-triage"),
            "{path} must not reference deprecated bv flag --robot-triage"
        );
        assert!(
            !doc.contains("--robot-next"),
            "{path} must not reference deprecated bv flag --robot-next"
        );
    }

    let plan_doc = load_text(PLAN_TO_COMPLETE_PORT_PATH);
    assert!(
        plan_doc.contains("bv --robot-plan"),
        "{PLAN_TO_COMPLETE_PORT_PATH} must reference bv --robot-plan"
    );
    assert!(
        plan_doc.contains("bv --robot-priority"),
        "{PLAN_TO_COMPLETE_PORT_PATH} must reference bv --robot-priority"
    );

    let governance_doc = load_text(PROGRAM_GOVERNANCE_PATH);
    for needle in [
        "bv --robot-plan",
        "bv --robot-priority",
        "br ready --json",
        "br show <id>",
    ] {
        assert!(
            governance_doc.contains(needle),
            "{PROGRAM_GOVERNANCE_PATH} must include '{needle}' for tombstone-safe triage"
        );
    }
}
