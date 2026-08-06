#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
//! Final full-suite CI gate wiring and release-block policy (bd-1f42.6.5).
//!
//! This is the top-level CI gate that aggregates all sub-gates into a single
//! release-blocking verdict.  Each sub-gate is a named check with:
//!
//! - An artifact path that proves the check ran
//! - A pass/fail/warn/skip status
//! - An owning bead ID for failure triage
//! - A direct link to the failing artifact
//!
//! Sub-gates:
//! 1. Non-mock unit compliance (bd-1f42.2.6)
//! 2. E2E log contract (bd-1f42.3.6)
//! 3. Extension must-pass gate (bd-1f42.4.4)
//! 4. Extension provider compatibility matrix (bd-1f42.4.6)
//! 5. Unified evidence bundle (bd-1f42.6.8)
//! 6. Cross-platform matrix (bd-1f42.6.7)
//! 7. Conformance regression gate
//! 8. Release gate (evidence completeness)
//! 9. Suite classification guard
//! 10. Requirement traceability matrix
//! 11. Canonical E2E scenario matrix
//! 12. Provider gap test matrix (bd-3uqg.11.11.5)
//! 13. Waiver lifecycle (bd-1f42.8.8.1)
//! 14. SEC-6.4 security compatibility conformance (bd-1a2cu)
//! 15. PERF-3X bead-to-artifact coverage audit (bd-3ar8v.6.11)
//! 16. Practical-finish checkpoint (bd-3ar8v.6.9)
//! 17. Extension remediation backlog (bd-3ar8v.6.8)
//! 18. Opportunity matrix (bd-3ar8v.6.1)
//! 19. Parameter sweeps (bd-3ar8v.6.2)
//! 20. Conformance+stress lineage coherence (bd-3ar8v.6.3)
//!
//! Run:
//!   cargo test --test `ci_full_suite_gate` -- --nocapture

use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

const CI_WORKFLOW_PATH: &str = ".github/workflows/ci.yml";
const RUN_ALL_SCRIPT_PATH: &str = "scripts/e2e/run_all.sh";
const QA_RUNBOOK_PATH: &str = "docs/qa-runbook.md";
const CI_OPERATOR_RUNBOOK_PATH: &str = "docs/ci-operator-runbook.md";
const PRACTICAL_FINISH_SNAPSHOT_PATH: &str =
    "tests/full_suite_gate/practical_finish_issues_snapshot.jsonl";
const PRACTICAL_FINISH_LIVE_ISSUE_SOURCE: &str = ".beads/issues.jsonl";
const PRACTICAL_FINISH_BASE_SOURCE: &str = ".beads/beads.base.jsonl";
// The live ledger is release-authoritative only when its working-tree bytes,
// index blob, and HEAD blob are identical. Lower-authority snapshots remain
// diagnostics; neither mtimes nor a partial list of familiar IDs can establish
// completeness.
const PRACTICAL_FINISH_REQUIRED_LEDGER_ANCHORS: &[&str] =
    &["bd-3ar8v", "bd-3ar8v.6", "bd-3ar8v.6.9"];

const MUST_PASS_GATE_SCHEMA: &str = "pi.ext.must_pass_gate.v1";
const MUST_PASS_EVENT_SCHEMA: &str = "pi.ext.gate_event.v1";
const MUST_PASS_INCLUSION_PATH: &str = "docs/extension-inclusion-list.json";
const MUST_PASS_MANIFEST_PATH: &str = "tests/ext_conformance/VALIDATED_MANIFEST.json";
const MUST_PASS_VERDICT_PATH: &str =
    "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json";
const MUST_PASS_EVENTS_PATH: &str = "tests/ext_conformance/reports/gate/must_pass_events.jsonl";
const MUST_PASS_ARTIFACTS_PATH: &str = "tests/ext_conformance/artifacts";
const EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1: u64 = 208;
const MUST_PASS_SOURCE_PATHS: &[&str] = &[
    ".cargo/config.toml",
    ".gitattributes",
    "CHANGELOG.md",
    "Cargo.lock",
    "Cargo.toml",
    "build.rs",
    "docs/evidence/tool-output-context-cache.jsonl",
    "docs/extension-artifact-provenance.json",
    "docs/provider-upstream-model-ids-snapshot.json",
    "docs/schema/extension_protocol.json",
    "legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts",
    "rust-toolchain.toml",
    "src",
    "tests/common",
    MUST_PASS_INCLUSION_PATH,
    MUST_PASS_MANIFEST_PATH,
    MUST_PASS_ARTIFACTS_PATH,
    "tests/ext_conformance_generated.rs",
    "tests/release_readiness.rs",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ── Waiver lifecycle infrastructure ─────────────────────────────────────

/// A parsed waiver entry from suite_classification.toml.
#[derive(Debug, Clone, serde::Serialize)]
struct Waiver {
    gate_id: String,
    owner: String,
    created: String,
    expires: String,
    bead: String,
    reason: String,
    scope: String,
    remove_when: String,
}

/// Waiver validation result.
#[derive(Debug, Clone, serde::Serialize)]
struct WaiverValidation {
    gate_id: String,
    status: String, // "active", "expired", "expiring_soon", "invalid"
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days_remaining: Option<i64>,
}

/// Waiver audit report written alongside gate verdicts.
#[derive(Debug, serde::Serialize)]
struct WaiverAuditReport {
    schema: String,
    generated_at: String,
    total_waivers: usize,
    active: usize,
    expired: usize,
    expiring_soon: usize,
    invalid: usize,
    waivers: Vec<WaiverValidation>,
    raw_waivers: Vec<Waiver>,
}

const WAIVER_REQUIRED_FIELDS: &[&str] = &[
    "owner",
    "created",
    "expires",
    "bead",
    "reason",
    "scope",
    "remove_when",
];
const WAIVER_VALID_SCOPES: &[&str] = &["full", "preflight", "both"];
const WAIVER_MAX_DURATION_DAYS: i64 = 30;
const WAIVER_EXPIRY_WARN_DAYS: i64 = 3;
const GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV: &str = "PI_GENERATE_FULL_SUITE_GATE_ARTIFACTS";

fn full_suite_gate_artifact_generation_requested() -> bool {
    let raw = std::env::var(GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV).ok();
    full_suite_gate_artifact_generation_requested_from(raw.as_deref())
}

fn full_suite_gate_artifact_generation_requested_from(raw: Option<&str>) -> bool {
    matches!(raw, Some("1"))
}

fn prepare_report_dir_for_generation(report_dir: &Path) {
    std::fs::create_dir_all(report_dir).unwrap_or_else(|err| {
        panic!(
            "failed to create full-suite report directory {}: {err}",
            report_dir.display()
        )
    });
}

/// Parse all `[waiver.*]` sections from suite_classification.toml.
fn parse_waivers(root: &Path) -> (Vec<Waiver>, Vec<WaiverValidation>) {
    let toml_path = root.join("tests/suite_classification.toml");
    let Ok(text) = std::fs::read_to_string(&toml_path) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return (Vec::new(), Vec::new());
    };

    let mut waivers = Vec::new();
    let mut validations = Vec::new();

    let Some(waiver_table) = table.get("waiver").and_then(toml::Value::as_table) else {
        return (waivers, validations);
    };

    let today = today_date_str();

    for (gate_id, entry) in waiver_table {
        let Some(entry_table) = entry.as_table() else {
            validations.push(WaiverValidation {
                gate_id: gate_id.clone(),
                status: "invalid".to_string(),
                detail: Some("Waiver entry is not a table".to_string()),
                days_remaining: None,
            });
            continue;
        };

        // Check required fields
        let mut missing = Vec::new();
        for &field in WAIVER_REQUIRED_FIELDS {
            if !entry_table.contains_key(field) {
                missing.push(field);
            }
        }
        if !missing.is_empty() {
            validations.push(WaiverValidation {
                gate_id: gate_id.clone(),
                status: "invalid".to_string(),
                detail: Some(format!("Missing required fields: {}", missing.join(", "))),
                days_remaining: None,
            });
            continue;
        }

        let get_str = |key: &str| -> String {
            entry_table
                .get(key)
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_string()
        };

        let waiver = Waiver {
            gate_id: gate_id.clone(),
            owner: get_str("owner"),
            created: get_str("created"),
            expires: get_str("expires"),
            bead: get_str("bead"),
            reason: get_str("reason"),
            scope: get_str("scope"),
            remove_when: get_str("remove_when"),
        };

        // Validate scope
        if !WAIVER_VALID_SCOPES.contains(&waiver.scope.as_str()) {
            validations.push(WaiverValidation {
                gate_id: gate_id.clone(),
                status: "invalid".to_string(),
                detail: Some(format!(
                    "Invalid scope '{}' (expected one of: {:?})",
                    waiver.scope, WAIVER_VALID_SCOPES
                )),
                days_remaining: None,
            });
            waivers.push(waiver);
            continue;
        }

        // Validate date format and expiry
        let validation = validate_waiver_dates(&waiver, &today);
        validations.push(validation);
        waivers.push(waiver);
    }

    (waivers, validations)
}

/// Returns today's date as YYYY-MM-DD string.
fn today_date_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Parse a YYYY-MM-DD date string into a chrono NaiveDate.
fn parse_date(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Validate waiver dates: expiry, duration, expiring-soon.
fn validate_waiver_dates(waiver: &Waiver, today: &str) -> WaiverValidation {
    let Some(today_date) = parse_date(today) else {
        return WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "invalid".to_string(),
            detail: Some("Cannot parse today's date".to_string()),
            days_remaining: None,
        };
    };

    let Some(created) = parse_date(&waiver.created) else {
        return WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "invalid".to_string(),
            detail: Some(format!("Invalid created date: '{}'", waiver.created)),
            days_remaining: None,
        };
    };

    let Some(expires) = parse_date(&waiver.expires) else {
        return WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "invalid".to_string(),
            detail: Some(format!("Invalid expires date: '{}'", waiver.expires)),
            days_remaining: None,
        };
    };

    // Check max duration
    let duration_days = (expires - created).num_days();
    if duration_days > WAIVER_MAX_DURATION_DAYS {
        return WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "invalid".to_string(),
            detail: Some(format!(
                "Waiver duration {duration_days} days exceeds max {WAIVER_MAX_DURATION_DAYS} days"
            )),
            days_remaining: Some((expires - today_date).num_days()),
        };
    }

    let days_remaining = (expires - today_date).num_days();

    if days_remaining < 0 {
        WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "expired".to_string(),
            detail: Some(format!(
                "Expired {} day(s) ago (owner: {}, bead: {})",
                -days_remaining, waiver.owner, waiver.bead
            )),
            days_remaining: Some(days_remaining),
        }
    } else if days_remaining <= WAIVER_EXPIRY_WARN_DAYS {
        WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "expiring_soon".to_string(),
            detail: Some(format!(
                "Expires in {days_remaining} day(s) — action required (owner: {}, bead: {})",
                waiver.owner, waiver.bead
            )),
            days_remaining: Some(days_remaining),
        }
    } else {
        WaiverValidation {
            gate_id: waiver.gate_id.clone(),
            status: "active".to_string(),
            detail: None,
            days_remaining: Some(days_remaining),
        }
    }
}

/// Build a set of waived gate_ids for a given lane scope.
fn waived_gate_ids(
    waivers: &[Waiver],
    validations: &[WaiverValidation],
    lane: &str,
) -> HashMap<String, Waiver> {
    let mut map = HashMap::new();
    for waiver in waivers {
        // Only active/expiring_soon waivers take effect (not expired/invalid)
        let is_valid = validations.iter().any(|v| {
            v.gate_id == waiver.gate_id && (v.status == "active" || v.status == "expiring_soon")
        });
        if !is_valid {
            continue;
        }
        // Check scope matches lane
        let scope_matches = match waiver.scope.as_str() {
            "both" => true,
            s => s == lane,
        };
        if scope_matches {
            map.insert(waiver.gate_id.clone(), waiver.clone());
        }
    }
    map
}

/// A sub-gate in the full suite.
#[derive(Debug, Clone, serde::Serialize)]
struct SubGate {
    id: String,
    name: String,
    bead: String,
    status: String, // "pass", "fail", "warn", "skip"
    blocking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reproduce_command: Option<String>,
}

/// Full-suite gate verdict.
#[derive(Debug, serde::Serialize)]
struct FullSuiteVerdict {
    schema: String,
    generated_at: String,
    verdict: String, // "pass", "fail", "warn"
    policy: String,
    gates: Vec<SubGate>,
    summary: GateSummary,
}

/// Gate summary statistics.
#[derive(Debug, serde::Serialize)]
struct GateSummary {
    total_gates: usize,
    passed: usize,
    failed: usize,
    warned: usize,
    skipped: usize,
    blocking_pass: usize,
    blocking_total: usize,
    all_blocking_pass: bool,
}

/// One row in the PERF-3X bead coverage contract.
#[derive(Debug, Clone)]
struct Perf3xBeadCoverageRow {
    bead: String,
    unit_evidence: Vec<String>,
    e2e_evidence: Vec<String>,
    log_evidence: Vec<String>,
}

#[derive(Debug)]
struct Perf3xBeadCoverageEvaluation {
    status: String,
    detail: Option<String>,
    rows: Vec<Perf3xBeadCoverageRow>,
    missing_evidence: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct Perf3xBeadCoverageAuditRow {
    bead: String,
    unit_evidence: Vec<String>,
    e2e_evidence: Vec<String>,
    log_evidence: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct Perf3xBeadCoverageAuditReport {
    schema: String,
    generated_at: String,
    contract_schema: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    row_count: usize,
    missing_evidence_count: usize,
    missing_evidence: Vec<String>,
    rows: Vec<Perf3xBeadCoverageAuditRow>,
}

/// Critical implementation beads that must appear in the PERF-3X coverage matrix.
const PERF3X_CRITICAL_BEADS: &[&str] = &[
    "bd-3ar8v.2.8",
    "bd-3ar8v.3.8",
    "bd-3ar8v.4.7",
    "bd-3ar8v.4.8",
    "bd-3ar8v.4.9",
    "bd-3ar8v.4.10",
    "bd-3ar8v.6.11",
];
const PERF3X_COVERAGE_AUDIT_ARTIFACT_REL: &str =
    "tests/full_suite_gate/perf3x_bead_coverage_audit.json";
#[allow(dead_code)]
const PRACTICAL_FINISH_CHECKPOINT_ARTIFACT_REL: &str =
    "tests/full_suite_gate/practical_finish_checkpoint.json";
const EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL: &str =
    "tests/full_suite_gate/extension_remediation_backlog.json";
const EXTENSION_REMEDIATION_BACKLOG_SCHEMA: &str = "pi.qa.extension_remediation_backlog.v1";
const OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/opportunity_matrix.json";
const OPPORTUNITY_MATRIX_SCHEMA: &str = "pi.perf.opportunity_matrix.v1";
const PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/parameter_sweeps.json";
const PARAMETER_SWEEPS_SCHEMA: &str = "pi.perf.parameter_sweeps.v1";
const STRESS_TRIAGE_ARTIFACT_REL: &str = "tests/perf/reports/stress_triage.json";
const CONFORMANCE_SUMMARY_ARTIFACT_REL: &str =
    "tests/ext_conformance/reports/conformance_summary.json";

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize)]
struct PracticalFinishOpenIssue {
    id: String,
    title: String,
    status: String,
    issue_type: String,
    labels: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize)]
struct PracticalFinishCheckpointReport {
    schema: String,
    generated_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    technical_completion_reached: bool,
    residual_open_scope: String,
    open_perf3x_count: usize,
    technical_open_count: usize,
    docs_or_report_open_count: usize,
    technical_open_issues: Vec<PracticalFinishOpenIssue>,
    docs_or_report_open_issues: Vec<PracticalFinishOpenIssue>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
struct PracticalFinishIssueRecord {
    id: String,
    status: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    labels: Vec<String>,
}

#[allow(dead_code)]
fn normalize_issue_labels(labels: &[String]) -> Vec<String> {
    let mut normalized = labels
        .iter()
        .map(|label| label.trim().to_ascii_lowercase())
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

#[allow(dead_code)]
fn is_nonterminal_issue_status(status: &str) -> Result<bool, String> {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "closed" | "tombstone" => Ok(false),
        "" => Err("missing Beads issue status".to_string()),
        // Beads has exactly two terminal states. Treat every other state as
        // unresolved so new/custom workflow states cannot create a release
        // false-green merely because this gate predates them.
        _ => Ok(true),
    }
}

#[allow(dead_code)]
fn is_post_perf3x_phase7_issue_id(id: &str) -> bool {
    id == "bd-3ar8v.7" || id.starts_with("bd-3ar8v.7.")
}

#[allow(dead_code)]
fn is_practical_finish_in_scope_issue_id(id: &str) -> bool {
    // Exclude the top-level PERF-3X epic (rollup parent spanning Phase 6+)
    // and all Post-PERF-3X Phase 7 nodes from practical-finish scope.
    id.starts_with("bd-3ar8v") && id != "bd-3ar8v" && !is_post_perf3x_phase7_issue_id(id)
}

#[allow(dead_code)]
fn filter_practical_finish_leaf_issues(
    open_issues: &[PracticalFinishOpenIssue],
) -> Vec<PracticalFinishOpenIssue> {
    open_issues
        .iter()
        .filter(|issue| {
            let prefix = format!("{}.", issue.id);
            let issue_is_docs_only = is_docs_only_issue(issue);
            !open_issues.iter().any(|other| {
                other.id != issue.id
                    && other.id.starts_with(&prefix)
                    && is_docs_only_issue(other) == issue_is_docs_only
            })
        })
        .cloned()
        .collect()
}

#[allow(dead_code)]
fn is_docs_only_issue(issue: &PracticalFinishOpenIssue) -> bool {
    // Generic labels such as `report`, `runbook`, or `docs-last` describe an
    // output or phase, not the work's semantics. Only the explicit Beads docs
    // issue type is strong enough to exempt an open item from the technical
    // practical-finish blocker set.
    issue.issue_type.eq_ignore_ascii_case("docs")
}

#[allow(dead_code)]
fn load_open_perf3x_issues(root: &Path) -> Result<Vec<PracticalFinishOpenIssue>, String> {
    let (source, _path, contents) = read_practical_finish_issue_source(root)?;

    let mut in_scope_issues = Vec::new();
    let mut seen_issue_ids = HashSet::new();
    for (line_idx, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let record: PracticalFinishIssueRecord = serde_json::from_str(line)
            .map_err(|err| format!("invalid JSONL row {} in {source}: {err}", line_idx + 1))?;
        let id = record.id.trim();
        if id.is_empty() {
            return Err(format!(
                "invalid JSONL row {} in {source}: missing id",
                line_idx + 1
            ));
        }
        if !seen_issue_ids.insert(id.to_string()) {
            return Err(format!(
                "invalid JSONL row {} in {source}: duplicate issue id {id}",
                line_idx + 1
            ));
        }
        if !is_practical_finish_in_scope_issue_id(id) {
            continue;
        }
        let status = record.status.trim().to_ascii_lowercase();
        let is_nonterminal = is_nonterminal_issue_status(&status)
            .map_err(|err| format!("invalid JSONL row {} in {source}: {err}", line_idx + 1))?;
        let issue_type = if record.issue_type.trim().is_empty() {
            "unknown".to_string()
        } else {
            record.issue_type.trim().to_ascii_lowercase()
        };
        let title = if record.title.trim().is_empty() {
            "(untitled)".to_string()
        } else {
            record.title.trim().to_string()
        };
        let labels = normalize_issue_labels(&record.labels);
        if is_nonterminal {
            in_scope_issues.push(PracticalFinishOpenIssue {
                id: id.to_string(),
                title,
                status,
                issue_type,
                labels,
            });
        }
    }

    let missing_anchors = PRACTICAL_FINISH_REQUIRED_LEDGER_ANCHORS
        .iter()
        .filter(|id| !seen_issue_ids.contains(**id))
        .copied()
        .collect::<Vec<_>>();
    if !missing_anchors.is_empty() {
        return Err(format!(
            "practical-finish source {source} is missing required canonical ledger anchor(s): {}; source may be empty, truncated, or non-authoritative",
            missing_anchors.join(", ")
        ));
    }

    let mut open = in_scope_issues;
    open.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(filter_practical_finish_leaf_issues(&open))
}

#[allow(dead_code)]
fn git_blob_bytes(root: &Path, object: &str, context: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", object])
        .output()
        .map_err(|err| format!("failed to execute git show for {context}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git show failed for {context}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedRegularFile {
    git_commit: String,
    blob_oid: String,
    contents: Vec<u8>,
}

#[allow(clippy::too_many_lines)]
fn capture_committed_regular_file(
    root: &Path,
    relative: &str,
    description: &str,
) -> Result<CommittedRegularFile, String> {
    let git_commit = current_git_commit(root)?;
    let tree_output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-z", "--full-tree", &git_commit, "--", relative])
        .output()
        .map_err(|err| format!("failed to inspect {description} at HEAD: {err}"))?;
    if !tree_output.status.success() {
        return Err(format!(
            "git ls-tree failed while inspecting {description}: {}",
            String::from_utf8_lossy(&tree_output.stderr).trim()
        ));
    }
    let tree_records = String::from_utf8(tree_output.stdout)
        .map_err(|err| format!("git ls-tree returned non-UTF-8 {description} data: {err}"))?
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tree_records.len() != 1 {
        return Err(format!(
            "{description} must be exactly one tracked regular blob at HEAD: {relative}"
        ));
    }
    let (tree_metadata, tree_path) = tree_records[0]
        .split_once('\t')
        .ok_or_else(|| format!("malformed HEAD record for {description}"))?;
    let mut tree_fields = tree_metadata.split_ascii_whitespace();
    let head_mode = tree_fields.next().unwrap_or("");
    let head_type = tree_fields.next().unwrap_or("");
    let head_oid = tree_fields.next().unwrap_or("");
    if tree_fields.next().is_some()
        || tree_path != relative
        || !matches!(head_mode, "100644" | "100755")
        || head_type != "blob"
        || !matches!(head_oid.len(), 40 | 64)
        || !head_oid.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "{description} is not a canonical regular blob at HEAD: {relative}"
        ));
    }

    let index_output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--stage", "-z", "--", relative])
        .output()
        .map_err(|err| format!("failed to inspect {description} index entry: {err}"))?;
    if !index_output.status.success() {
        return Err(format!(
            "git ls-files --stage failed while inspecting {description}: {}",
            String::from_utf8_lossy(&index_output.stderr).trim()
        ));
    }
    let index_records = String::from_utf8(index_output.stdout)
        .map_err(|err| format!("git ls-files returned non-UTF-8 {description} data: {err}"))?
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if index_records.len() != 1 {
        return Err(format!(
            "{description} must have exactly one stage-0 index entry: {relative}"
        ));
    }
    let (index_metadata, index_path) = index_records[0]
        .split_once('\t')
        .ok_or_else(|| format!("malformed index record for {description}"))?;
    let mut index_fields = index_metadata.split_ascii_whitespace();
    let index_mode = index_fields.next().unwrap_or("");
    let index_oid = index_fields.next().unwrap_or("");
    let index_stage = index_fields.next().unwrap_or("");
    if index_fields.next().is_some()
        || index_path != relative
        || index_stage != "0"
        || !matches!(index_mode, "100644" | "100755")
        || index_mode != head_mode
        || index_oid != head_oid
    {
        return Err(format!(
            "{description} differs between the Git index and HEAD; the index entry does not exactly match its regular HEAD blob: {relative}"
        ));
    }

    let flag_output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-v", "-z", "--", relative])
        .output()
        .map_err(|err| format!("failed to inspect {description} index flags: {err}"))?;
    if !flag_output.status.success() {
        return Err(format!(
            "git ls-files -v failed while inspecting {description}: {}",
            String::from_utf8_lossy(&flag_output.stderr).trim()
        ));
    }
    let canonical_flag_record = format!("H {relative}\0");
    if flag_output.stdout != canonical_flag_record.as_bytes() {
        return Err(format!(
            "{description} uses non-canonical index flags (including assume-unchanged or skip-worktree): {relative}"
        ));
    }

    for (label, args) in [
        (
            "worktree",
            vec!["diff", "--quiet", "--no-ext-diff", "--", relative],
        ),
        (
            "index",
            vec![
                "diff",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                git_commit.as_str(),
                "--",
                relative,
            ],
        ),
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .map_err(|err| format!("failed to inspect {description} {label} drift: {err}"))?;
        match status.code() {
            Some(0) => {}
            Some(1) => {
                let relationship = if label == "worktree" {
                    "between the working tree and Git index"
                } else {
                    "between the Git index and HEAD"
                };
                return Err(format!(
                    "{description} differs {relationship}; commit the authoritative file before release evaluation"
                ));
            }
            code => {
                return Err(format!(
                    "git diff failed while inspecting {description} {label} drift (status {code:?})"
                ));
            }
        }
    }

    let full_path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&full_path)
        .map_err(|err| format!("failed to inspect {description} worktree file: {err}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{description} must be a regular worktree file: {}",
            full_path.display()
        ));
    }
    let contents = std::fs::read(&full_path)
        .map_err(|err| format!("failed to read {description} worktree bytes: {err}"))?;
    let worktree_oid = git_blob_oid(&contents, head_oid.len())?;
    if worktree_oid != head_oid {
        return Err(format!(
            "{description} raw worktree bytes differ from the HEAD blob: {relative}"
        ));
    }
    if current_git_commit(root)? != git_commit {
        return Err(format!(
            "HEAD changed while binding {description} to the release commit"
        ));
    }

    Ok(CommittedRegularFile {
        git_commit,
        blob_oid: head_oid.to_string(),
        contents,
    })
}

#[allow(dead_code)]
fn validate_live_ledger_git_binding(
    root: &Path,
    relative: &str,
    worktree_bytes: &[u8],
) -> Result<(), String> {
    let before = capture_committed_regular_file(root, relative, "live Beads ledger")?;
    if worktree_bytes != before.contents {
        return Err(format!(
            "live Beads ledger {relative} differs between the working tree and Git index; flush and commit the authoritative ledger before release evaluation"
        ));
    }
    let after = capture_committed_regular_file(root, relative, "live Beads ledger")?;
    if before != after {
        return Err(format!(
            "live Beads ledger {relative} changed while validating its Git binding"
        ));
    }
    Ok(())
}

#[allow(dead_code)]
fn read_practical_finish_issue_source(
    root: &Path,
) -> Result<(&'static str, PathBuf, String), String> {
    let path = root.join(PRACTICAL_FINISH_LIVE_ISSUE_SOURCE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let diagnostics = [PRACTICAL_FINISH_BASE_SOURCE, PRACTICAL_FINISH_SNAPSHOT_PATH]
                .into_iter()
                .filter(|relative| root.join(relative).is_file())
                .collect::<Vec<_>>();
            let suffix = if diagnostics.is_empty() {
                String::new()
            } else {
                format!(
                    "; lower-authority diagnostic source(s) present but non-certifying: {}",
                    diagnostics.join(", ")
                )
            };
            return Err(format!(
                "release-authoritative live Beads ledger is missing at {}{suffix}",
                path.display()
            ));
        }
        Err(err) => {
            return Err(format!(
                "failed to read release-authoritative live Beads ledger at {}: {err}",
                path.display()
            ));
        }
    };
    validate_live_ledger_git_binding(root, PRACTICAL_FINISH_LIVE_ISSUE_SOURCE, &bytes)?;
    let contents = String::from_utf8(bytes).map_err(|err| {
        format!(
            "release-authoritative live Beads ledger at {} is not UTF-8: {err}",
            path.display()
        )
    })?;
    Ok((PRACTICAL_FINISH_LIVE_ISSUE_SOURCE, path, contents))
}

#[allow(dead_code)]
fn split_practical_finish_issue_buckets(
    open_issues: &[PracticalFinishOpenIssue],
) -> (Vec<PracticalFinishOpenIssue>, Vec<PracticalFinishOpenIssue>) {
    let mut docs_or_report = Vec::new();
    let mut technical = Vec::new();
    for issue in open_issues {
        if is_docs_only_issue(issue) {
            docs_or_report.push(issue.clone());
        } else {
            technical.push(issue.clone());
        }
    }
    (docs_or_report, technical)
}

#[allow(dead_code)]
fn build_practical_finish_checkpoint_report(root: &Path) -> PracticalFinishCheckpointReport {
    use chrono::{SecondsFormat, Utc};

    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let open_perf3x = match load_open_perf3x_issues(root) {
        Ok(issues) => issues,
        Err(err) => {
            return PracticalFinishCheckpointReport {
                schema: "pi.perf3x.practical_finish_checkpoint.v1".to_string(),
                generated_at,
                status: "fail".to_string(),
                detail: Some(format!(
                    "Fail-closed practical-finish source read error: {err}"
                )),
                technical_completion_reached: false,
                residual_open_scope: "technical_remaining".to_string(),
                open_perf3x_count: 0,
                technical_open_count: 0,
                docs_or_report_open_count: 0,
                technical_open_issues: Vec::new(),
                docs_or_report_open_issues: Vec::new(),
            };
        }
    };

    let (docs_or_report_open, technical_open) = split_practical_finish_issue_buckets(&open_perf3x);
    let technical_completion_reached = technical_open.is_empty();
    let residual_open_scope = if technical_completion_reached {
        if docs_or_report_open.is_empty() {
            "none".to_string()
        } else {
            "docs_or_report_only".to_string()
        }
    } else {
        "technical_remaining".to_string()
    };
    let status = if technical_completion_reached {
        "pass".to_string()
    } else {
        "fail".to_string()
    };
    let detail = if technical_completion_reached {
        if docs_or_report_open.is_empty() {
            Some("Practical-finish checkpoint reached: no open PERF-3X issues remain.".to_string())
        } else {
            Some(format!(
                "Practical-finish checkpoint reached: technical PERF-3X scope complete; {} docs/report issue(s) remain.",
                docs_or_report_open.len()
            ))
        }
    } else {
        let preview = technical_open
            .iter()
            .take(5)
            .map(|issue| issue.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if technical_open.len() > 5 {
            ", ..."
        } else {
            ""
        };
        Some(format!(
            "Practical-finish checkpoint blocked: {} technical PERF-3X issue(s) still open ({preview}{suffix})",
            technical_open.len()
        ))
    };

    PracticalFinishCheckpointReport {
        schema: "pi.perf3x.practical_finish_checkpoint.v1".to_string(),
        generated_at,
        status,
        detail,
        technical_completion_reached,
        residual_open_scope,
        open_perf3x_count: open_perf3x.len(),
        technical_open_count: technical_open.len(),
        docs_or_report_open_count: docs_or_report_open.len(),
        technical_open_issues: technical_open,
        docs_or_report_open_issues: docs_or_report_open,
    }
}

#[allow(dead_code)]
fn evaluate_practical_finish_checkpoint(root: &Path) -> (String, Option<String>) {
    let report = build_practical_finish_checkpoint_report(root);
    (report.status, report.detail)
}

/// Machine-readable coverage contract consumed by the Phase-5 gate.
fn perf3x_bead_coverage_contract() -> Value {
    serde_json::json!({
        "schema": "pi.perf3x.bead_coverage.v1",
        "coverage_rows": [
            {
                "bead": "bd-3ar8v.2.8",
                "unit_evidence": ["tests/bench_schema.rs"],
                "e2e_evidence": ["tests/e2e_results"],
                "log_evidence": ["tests/full_suite_gate/full_suite_events.jsonl"]
            },
            {
                "bead": "bd-3ar8v.3.8",
                "unit_evidence": ["tests/compaction.rs"],
                "e2e_evidence": ["tests/e2e_session_persistence.rs"],
                "log_evidence": ["tests/full_suite_gate/certification_events.jsonl"]
            },
            {
                "bead": "bd-3ar8v.4.7",
                "unit_evidence": ["tests/ext_conformance.rs"],
                "e2e_evidence": ["tests/e2e_extension_registration.rs"],
                "log_evidence": ["tests/ext_conformance/reports/conformance_summary.json"]
            },
            {
                "bead": "bd-3ar8v.4.8",
                "unit_evidence": ["tests/ext_proptest.rs"],
                "e2e_evidence": ["tests/e2e_extension_registration.rs"],
                "log_evidence": ["tests/full_suite_gate/full_suite_events.jsonl"]
            },
            {
                "bead": "bd-3ar8v.4.9",
                "unit_evidence": ["tests/ext_bench_harness.rs"],
                "e2e_evidence": ["tests/e2e_extension_registration.rs"],
                "log_evidence": ["tests/full_suite_gate/certification_events.jsonl"]
            },
            {
                "bead": "bd-3ar8v.4.10",
                "unit_evidence": ["tests/phase3_security_invariants.rs"],
                "e2e_evidence": ["tests/e2e_security_scenario_sec66.rs"],
                "log_evidence": ["tests/full_suite_gate/certification_events.jsonl"]
            },
            {
                "bead": "bd-3ar8v.6.11",
                "unit_evidence": ["tests/ci_full_suite_gate.rs"],
                "e2e_evidence": ["tests/full_suite_gate/certification_verdict.json"],
                "log_evidence": ["tests/full_suite_gate/certification_events.jsonl"]
            }
        ]
    })
}

fn parse_required_evidence_paths(
    row: &Value,
    row_idx: usize,
    field_name: &str,
) -> Result<Vec<String>, String> {
    let values = row
        .get(field_name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("coverage_rows[{row_idx}] missing '{field_name}' array"))?;
    if values.is_empty() {
        return Err(format!(
            "coverage_rows[{row_idx}] field '{field_name}' must be non-empty"
        ));
    }

    let mut seen_paths = HashSet::new();
    let mut previous_path: Option<String> = None;
    let mut paths = Vec::with_capacity(values.len());
    for (path_idx, value) in values.iter().enumerate() {
        let raw_path = value.as_str().ok_or_else(|| {
            format!("coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must be a string")
        })?;
        let raw_path = raw_path.trim();
        if raw_path.is_empty() {
            return Err(format!(
                "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must not be empty"
            ));
        }
        let normalized_path =
            normalize_repo_relative_evidence_path(raw_path, row_idx, field_name, path_idx)?;
        if let Some(previous) = previous_path.as_deref()
            && normalized_path.as_str() < previous
        {
            return Err(format!(
                "coverage_rows[{row_idx}] field '{field_name}' must be sorted by normalized path order: '{normalized_path}' appears after '{previous}'"
            ));
        }
        previous_path = Some(normalized_path.clone());
        if !seen_paths.insert(normalized_path.clone()) {
            return Err(format!(
                "coverage_rows[{row_idx}] field '{field_name}' contains duplicate path: {normalized_path}"
            ));
        }
        paths.push(normalized_path);
    }

    Ok(paths)
}

fn normalize_repo_relative_evidence_path(
    raw_path: &str,
    row_idx: usize,
    field_name: &str,
    path_idx: usize,
) -> Result<String, String> {
    let normalized_separators = raw_path.replace('\\', "/");
    if normalized_separators.starts_with("//") {
        return Err(format!(
            "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must not use UNC paths: {raw_path}"
        ));
    }
    if is_windows_absolute_path(&normalized_separators) {
        return Err(format!(
            "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must not use windows-absolute paths: {raw_path}"
        ));
    }

    let candidate = Path::new(&normalized_separators);
    if candidate.is_absolute() {
        return Err(format!(
            "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must be repo-relative, got absolute path: {raw_path}"
        ));
    }

    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => parts.push(segment.to_string_lossy().into_owned()),
            Component::ParentDir => {
                return Err(format!(
                    "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must not contain parent traversal ('..'): {raw_path}"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must be repo-relative: {raw_path}"
                ));
            }
        }
    }

    if parts.is_empty() {
        return Err(format!(
            "coverage_rows[{row_idx}] field '{field_name}[{path_idx}]' must contain a path segment: {raw_path}"
        ));
    }

    Ok(parts.join("/"))
}

const fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_canonical_perf3x_bead_id(bead: &str) -> bool {
    let Some(suffix) = bead.strip_prefix("bd-3ar8v.") else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    suffix
        .split('.')
        .all(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
}

fn canonical_perf3x_bead_segments(bead: &str) -> Option<Vec<u64>> {
    let suffix = bead.strip_prefix("bd-3ar8v.")?;
    suffix
        .split('.')
        .map(|segment| segment.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
}

fn canonicalize_perf3x_bead_id(bead: &str) -> Option<String> {
    let segments = canonical_perf3x_bead_segments(bead)?;
    let suffix = segments
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".");
    Some(format!("bd-3ar8v.{suffix}"))
}

fn validate_cross_category_evidence_uniqueness(
    row_idx: usize,
    unit_evidence: &[String],
    e2e_evidence: &[String],
    log_evidence: &[String],
) -> Result<(), String> {
    let mut seen_paths = HashMap::new();
    for (field_name, paths) in [
        ("unit_evidence", unit_evidence),
        ("e2e_evidence", e2e_evidence),
        ("log_evidence", log_evidence),
    ] {
        for path in paths {
            if let Some(previous_field) = seen_paths.insert(path.clone(), field_name) {
                return Err(format!(
                    "coverage_rows[{row_idx}] reuses evidence path across categories: {path} appears in '{previous_field}' and '{field_name}'"
                ));
            }
        }
    }
    Ok(())
}

/// Parse and validate the bead coverage contract.
fn validate_perf3x_bead_coverage_contract(
    contract: &Value,
) -> Result<Vec<Perf3xBeadCoverageRow>, String> {
    let schema = contract
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| "coverage contract missing schema".to_string())?;
    if schema != "pi.perf3x.bead_coverage.v1" {
        return Err(format!("unexpected coverage contract schema: {schema}"));
    }

    let rows = contract
        .get("coverage_rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "coverage contract missing coverage_rows array".to_string())?;
    if rows.is_empty() {
        return Err("coverage_rows must not be empty".to_string());
    }

    let mut seen_beads = HashSet::new();
    let mut seen_canonical_beads = HashMap::new();
    let mut previous_bead_segments: Option<Vec<u64>> = None;
    let mut previous_bead_id: Option<String> = None;
    let mut parsed = Vec::with_capacity(rows.len());
    for (row_idx, row) in rows.iter().enumerate() {
        let bead = row
            .get("bead")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("coverage_rows[{row_idx}] missing 'bead' string"))?
            .trim()
            .to_string();
        if !is_canonical_perf3x_bead_id(&bead) {
            return Err(format!(
                "coverage_rows[{row_idx}] has invalid PERF-3X bead id: {bead}"
            ));
        }
        let canonical_bead = canonicalize_perf3x_bead_id(&bead).ok_or_else(|| {
            format!("coverage_rows[{row_idx}] has unparsable canonical PERF-3X bead id: {bead}")
        })?;
        if let Some(existing_bead) =
            seen_canonical_beads.insert(canonical_bead.clone(), bead.clone())
        {
            return Err(format!(
                "coverage_rows[{row_idx}] bead '{bead}' is numerically equivalent to existing bead '{existing_bead}' (canonical id: {canonical_bead})"
            ));
        }
        if bead != canonical_bead {
            return Err(format!(
                "coverage_rows[{row_idx}] bead '{bead}' must be canonical (expected '{canonical_bead}')"
            ));
        }
        let bead_segments = canonical_perf3x_bead_segments(&bead).ok_or_else(|| {
            format!("coverage_rows[{row_idx}] has unparsable PERF-3X bead id segments: {bead}")
        })?;
        if let Some(previous_segments) = previous_bead_segments.as_ref()
            && bead_segments < *previous_segments
        {
            let previous_bead = previous_bead_id.as_deref().unwrap_or("<unknown>");
            return Err(format!(
                "coverage_rows must be sorted by canonical bead id order: row {row_idx} bead '{bead}' appears after '{previous_bead}'"
            ));
        }
        previous_bead_segments = Some(bead_segments);
        previous_bead_id = Some(bead.clone());
        if !seen_beads.insert(bead.clone()) {
            return Err(format!("duplicate bead in coverage_rows: {bead}"));
        }

        let unit_evidence = parse_required_evidence_paths(row, row_idx, "unit_evidence")?;
        let e2e_evidence = parse_required_evidence_paths(row, row_idx, "e2e_evidence")?;
        let log_evidence = parse_required_evidence_paths(row, row_idx, "log_evidence")?;
        validate_cross_category_evidence_uniqueness(
            row_idx,
            &unit_evidence,
            &e2e_evidence,
            &log_evidence,
        )?;

        parsed.push(Perf3xBeadCoverageRow {
            bead,
            unit_evidence,
            e2e_evidence,
            log_evidence,
        });
    }

    let mut missing_critical = Vec::new();
    for bead_id in PERF3X_CRITICAL_BEADS {
        if !seen_beads.contains(*bead_id) {
            missing_critical.push(*bead_id);
        }
    }
    if !missing_critical.is_empty() {
        return Err(format!(
            "coverage_rows missing critical PERF-3X bead(s): {}",
            missing_critical.join(", ")
        ));
    }

    Ok(parsed)
}

fn validate_repo_evidence_path(root: &Path, relative: &str) -> Result<(), String> {
    let mut cursor = root.to_path_buf();
    let mut final_metadata = None;
    for component in Path::new(relative).components() {
        let Component::Normal(segment) = component else {
            return Err("path is not canonical repository-relative evidence".to_string());
        };
        cursor.push(segment);
        let metadata = std::fs::symlink_metadata(&cursor)
            .map_err(|err| format!("path is missing or unreadable: {err}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "path traverses symbolic link at {}",
                cursor.display()
            ));
        }
        final_metadata = Some(metadata);
    }

    let metadata = final_metadata.ok_or_else(|| "path has no components".to_string())?;
    if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
        return Err("path is not a regular file or directory".to_string());
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|err| format!("repository root is unreadable: {err}"))?;
    let canonical_path = std::fs::canonicalize(&cursor)
        .map_err(|err| format!("path cannot be canonicalized: {err}"))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err("path resolves outside the repository root".to_string());
    }
    Ok(())
}

/// Evaluate contract coverage against files present in repository artifacts.
fn evaluate_perf3x_bead_coverage_internal(
    root: &Path,
    contract: &Value,
) -> Perf3xBeadCoverageEvaluation {
    let rows = match validate_perf3x_bead_coverage_contract(contract) {
        Ok(rows) => rows,
        Err(err) => {
            return Perf3xBeadCoverageEvaluation {
                status: "fail".to_string(),
                detail: Some(format!("Invalid PERF-3X coverage contract: {err}")),
                rows: Vec::new(),
                missing_evidence: Vec::new(),
            };
        }
    };

    let mut missing = Vec::new();
    for row in &rows {
        for (class_name, evidence_paths) in [
            ("unit", &row.unit_evidence),
            ("e2e", &row.e2e_evidence),
            ("log", &row.log_evidence),
        ] {
            for path in evidence_paths {
                if let Err(detail) = validate_repo_evidence_path(root, path) {
                    missing.push(format!("{}:{}:{} ({detail})", row.bead, class_name, path));
                }
            }
        }
    }
    missing.sort();

    if missing.is_empty() {
        return Perf3xBeadCoverageEvaluation {
            status: "pass".to_string(),
            detail: Some(format!(
                "Validated {} PERF-3X bead coverage row(s) with complete unit/e2e/log evidence paths",
                rows.len()
            )),
            rows,
            missing_evidence: missing,
        };
    }

    let preview = missing
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if missing.len() > 3 { " ..." } else { "" };
    Perf3xBeadCoverageEvaluation {
        status: "fail".to_string(),
        detail: Some(format!(
            "Coverage contract parsed ({} rows) but {} evidence path(s) are missing (fail-closed): {}{}",
            rows.len(),
            missing.len(),
            preview,
            suffix
        )),
        rows,
        missing_evidence: missing,
    }
}

fn evaluate_perf3x_bead_coverage(root: &Path, contract: &Value) -> (String, Option<String>) {
    let evaluation = evaluate_perf3x_bead_coverage_internal(root, contract);
    (evaluation.status, evaluation.detail)
}

fn build_perf3x_bead_coverage_audit_report(
    root: &Path,
    contract: &Value,
) -> Perf3xBeadCoverageAuditReport {
    use chrono::{SecondsFormat, Utc};

    let evaluation = evaluate_perf3x_bead_coverage_internal(root, contract);
    let contract_schema = contract
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let rows = evaluation
        .rows
        .into_iter()
        .map(|row| Perf3xBeadCoverageAuditRow {
            bead: row.bead,
            unit_evidence: row.unit_evidence,
            e2e_evidence: row.e2e_evidence,
            log_evidence: row.log_evidence,
        })
        .collect::<Vec<_>>();

    Perf3xBeadCoverageAuditReport {
        schema: "pi.perf3x.bead_coverage.audit.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        contract_schema,
        status: evaluation.status,
        detail: evaluation.detail,
        row_count: rows.len(),
        missing_evidence_count: evaluation.missing_evidence.len(),
        missing_evidence: evaluation.missing_evidence,
        rows,
    }
}

/// Check a JSON artifact for a specific status field.
fn check_artifact_status(
    root: &Path,
    artifact_rel: &str,
    status_path: &[&str],
    pass_values: &[&str],
) -> (String, Option<String>) {
    let full = root.join(artifact_rel);
    let Some(val) = load_json(&full) else {
        return (
            "skip".to_string(),
            Some(format!("Artifact not found: {artifact_rel}")),
        );
    };

    let mut current = &val;
    for key in status_path {
        match current.get(*key) {
            Some(v) => current = v,
            None => {
                return (
                    "warn".to_string(),
                    Some(format!(
                        "Missing field '{}' in {}",
                        status_path.join("."),
                        artifact_rel
                    )),
                );
            }
        }
    }

    let status_str = current.as_str().unwrap_or("");
    if pass_values.contains(&status_str) {
        ("pass".to_string(), None)
    } else {
        (
            "fail".to_string(),
            Some(format!(
                "{} = '{status_str}' (expected one of: {:?})",
                status_path.join("."),
                pass_values
            )),
        )
    }
}

fn current_git_commit(root: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|err| format!("failed to execute git rev-parse HEAD: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let commit = String::from_utf8(output.stdout)
        .map_err(|err| format!("git rev-parse HEAD returned non-UTF-8 output: {err}"))?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "git rev-parse HEAD returned invalid commit: {commit}"
        ));
    }
    Ok(commit.to_string())
}

fn ensure_must_pass_worktree_matches_commit(
    root: &Path,
    commit: &str,
    source_paths: &[&str],
) -> Result<(), String> {
    let observed_head = current_git_commit(root)?;
    if observed_head != commit {
        return Err(format!(
            "must-pass source HEAD changed while capturing provenance: expected {commit}, found {observed_head}"
        ));
    }

    let mut untracked_command = std::process::Command::new("git");
    untracked_command
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--others", "-z", "--"])
        .args(source_paths);
    let untracked_output = untracked_command
        .output()
        .map_err(|err| format!("failed to list untracked must-pass source inputs: {err}"))?;
    if !untracked_output.status.success() {
        return Err(format!(
            "git ls-files failed while checking untracked must-pass inputs: {}",
            String::from_utf8_lossy(&untracked_output.stderr).trim()
        ));
    }
    let untracked = String::from_utf8(untracked_output.stdout)
        .map_err(|err| format!("git ls-files returned non-UTF-8 untracked paths: {err}"))?;
    let untracked = untracked
        .split('\0')
        .filter(|path| !path.is_empty())
        .take(5)
        .collect::<Vec<_>>();
    if !untracked.is_empty() {
        return Err(format!(
            "must-pass source inputs contain untracked files: {}",
            untracked.join(", ")
        ));
    }

    let flag_output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-v", "-z", "--"])
        .args(source_paths)
        .output()
        .map_err(|err| format!("failed to inspect must-pass index flags: {err}"))?;
    if !flag_output.status.success() {
        return Err(format!(
            "git ls-files failed while checking must-pass index flags: {}",
            String::from_utf8_lossy(&flag_output.stderr).trim()
        ));
    }
    let flagged = String::from_utf8(flag_output.stdout)
        .map_err(|err| format!("git ls-files returned non-UTF-8 flag records: {err}"))?
        .split('\0')
        .filter(|record| !record.is_empty())
        .filter(|record| !record.starts_with("H "))
        .take(5)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !flagged.is_empty() {
        return Err(format!(
            "must-pass source inputs use non-canonical index flags (including assume-unchanged or skip-worktree): {}",
            flagged.join(", ")
        ));
    }

    for (label, diff_args) in [
        ("worktree", vec!["diff", "--quiet", "--no-ext-diff", "--"]),
        (
            "index",
            vec!["diff", "--cached", "--quiet", "--no-ext-diff", commit, "--"],
        ),
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(diff_args)
            .args(source_paths)
            .status()
            .map_err(|err| format!("failed to inspect must-pass source dirt: {err}"))?;
        match status.code() {
            Some(0) => {}
            Some(1) => {
                return Err(format!(
                    "must-pass source inputs differ in the {label}; commit them before release evaluation"
                ));
            }
            code => {
                return Err(format!(
                    "git diff failed while inspecting must-pass source inputs (status {code:?})"
                ));
            }
        }
    }
    Ok(())
}

fn must_pass_tree_records(
    root: &Path,
    commit: &str,
    source_paths: &[&str],
) -> Result<Vec<(String, String, String)>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "-z", "--full-tree", commit, "--"])
        .args(source_paths)
        .output()
        .map_err(|err| format!("failed to list tracked must-pass source inputs: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree failed for must-pass source inputs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let records = String::from_utf8(output.stdout)
        .map_err(|err| format!("git ls-tree returned non-UTF-8 records: {err}"))?;
    let records = records
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (metadata, path) = record
                .split_once('\t')
                .ok_or_else(|| format!("malformed git ls-tree record: {record}"))?;
            let mut fields = metadata.split_ascii_whitespace();
            let mode = fields
                .next()
                .ok_or_else(|| format!("missing mode in git ls-tree record: {record}"))?;
            let object_type = fields
                .next()
                .ok_or_else(|| format!("missing type in git ls-tree record: {record}"))?;
            let blob = fields
                .next()
                .ok_or_else(|| format!("missing blob in git ls-tree record: {record}"))?;
            if fields.next().is_some()
                || object_type != "blob"
                || !matches!(mode, "100644" | "100755")
                || !matches!(blob.len(), 40 | 64)
                || !blob.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(format!("invalid git ls-tree record: {record}"));
            }
            Ok((path.to_string(), mode.to_string(), blob.to_string()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut parsed = records;
    parsed.sort_by(|left, right| left.0.cmp(&right.0));
    if parsed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("git ls-tree returned duplicate must-pass source paths".to_string());
    }
    for required in source_paths {
        let prefix = format!("{required}/");
        if !parsed
            .iter()
            .any(|(path, _, _)| *path == *required || path.starts_with(&prefix))
        {
            return Err(format!(
                "required must-pass source input is not tracked: {required}"
            ));
        }
    }
    Ok(parsed)
}

fn must_pass_source_tree_sha256(records: &[(String, String, String)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"pi.ext.must_pass_source_tree.v2\0");
    for (path, mode, blob) in records {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(mode.as_bytes());
        hasher.update([0]);
        hasher.update(blob.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn git_blob_oid(contents: &[u8], oid_hex_len: usize) -> Result<String, String> {
    let header = format!("blob {}\0", contents.len());
    match oid_hex_len {
        40 => {
            let mut hasher = Sha1::new();
            hasher.update(header.as_bytes());
            hasher.update(contents);
            Ok(format!("{:x}", hasher.finalize()))
        }
        64 => {
            let mut hasher = Sha256::new();
            hasher.update(header.as_bytes());
            hasher.update(contents);
            Ok(format!("{:x}", hasher.finalize()))
        }
        length => Err(format!("unsupported Git object ID length: {length}")),
    }
}

fn ensure_worktree_bytes_match_tree(
    root: &Path,
    records: &[(String, String, String)],
) -> Result<(), String> {
    for (path, _, expected_blob) in records {
        let contents = std::fs::read(root.join(path)).map_err(|err| {
            format!("failed to read must-pass worktree input {path} for byte comparison: {err}")
        })?;
        let actual_blob = git_blob_oid(&contents, expected_blob.len())?;
        if actual_blob != *expected_blob {
            return Err(format!(
                "must-pass worktree bytes differ from canonical commit for {path}"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedMustPassEvidence {
    // The artifact commit is HEAD; the embedded git_commit remains the tested
    // source commit and may be an earlier evidence-only ancestor.
    git_commit: String,
    verdict_contents: Vec<u8>,
    events_contents: Vec<u8>,
}

fn capture_committed_must_pass_evidence(root: &Path) -> Result<CommittedMustPassEvidence, String> {
    let verdict_before =
        capture_committed_regular_file(root, MUST_PASS_VERDICT_PATH, "must-pass verdict evidence")?;
    let events =
        capture_committed_regular_file(root, MUST_PASS_EVENTS_PATH, "must-pass event evidence")?;
    let verdict_after =
        capture_committed_regular_file(root, MUST_PASS_VERDICT_PATH, "must-pass verdict evidence")?;
    let events_after =
        capture_committed_regular_file(root, MUST_PASS_EVENTS_PATH, "must-pass event evidence")?;
    if verdict_before != verdict_after
        || events != events_after
        || verdict_before.git_commit != events.git_commit
    {
        return Err(
            "committed must-pass evidence changed while binding both artifacts to HEAD".to_string(),
        );
    }

    Ok(CommittedMustPassEvidence {
        git_commit: verdict_before.git_commit,
        verdict_contents: verdict_before.contents,
        events_contents: events.contents,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct MustPassSourceSnapshot {
    git_commit: String,
    source_tree_sha256: String,
    inclusion_sha256: String,
    manifest_sha256: String,
    inclusion_contents: Vec<u8>,
    manifest_contents: Vec<u8>,
    tracked_paths: BTreeSet<String>,
}

fn capture_must_pass_source_snapshot(root: &Path) -> Result<MustPassSourceSnapshot, String> {
    let git_commit = current_git_commit(root)?;
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    let records = must_pass_tree_records(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let inclusion_contents = git_blob_bytes(
        root,
        &format!("{git_commit}:{MUST_PASS_INCLUSION_PATH}"),
        "canonical must-pass inclusion-list blob",
    )?;
    let manifest_contents = git_blob_bytes(
        root,
        &format!("{git_commit}:{MUST_PASS_MANIFEST_PATH}"),
        "canonical must-pass manifest blob",
    )?;
    let observed_head = current_git_commit(root)?;
    if observed_head != git_commit {
        return Err(format!(
            "must-pass source HEAD changed during snapshot capture: {git_commit} -> {observed_head}"
        ));
    }
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let tracked_paths = records.iter().map(|record| record.0.clone()).collect();
    Ok(MustPassSourceSnapshot {
        git_commit,
        source_tree_sha256: must_pass_source_tree_sha256(&records),
        inclusion_sha256: format!("{:x}", Sha256::digest(&inclusion_contents)),
        manifest_sha256: format!("{:x}", Sha256::digest(&manifest_contents)),
        inclusion_contents,
        manifest_contents,
        tracked_paths,
    })
}

fn canonical_must_pass_entries(
    inclusion_contents: &[u8],
    manifest_contents: &[u8],
    tracked_paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, u64>, String> {
    let inclusion: Value = serde_json::from_slice(inclusion_contents)
        .map_err(|err| format!("invalid {MUST_PASS_INCLUSION_PATH}: {err}"))?;
    if inclusion.get("schema").and_then(Value::as_str) != Some("pi.ext.inclusion_list.v1") {
        return Err(format!(
            "unexpected schema in {MUST_PASS_INCLUSION_PATH}: {}",
            inclusion
                .get("schema")
                .and_then(Value::as_str)
                .unwrap_or("missing")
        ));
    }

    let mut ids = BTreeSet::new();
    let mut observed_section_counts = BTreeMap::new();
    for section in ["tier1", "tier1_review"] {
        let rows = inclusion
            .get(section)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{MUST_PASS_INCLUSION_PATH} missing {section} array"))?;
        observed_section_counts.insert(section, rows.len());
        for (index, row) in rows.iter().enumerate() {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.is_empty() && id.trim() == *id && !id.chars().any(char::is_control)
                })
                .ok_or_else(|| {
                    format!("{MUST_PASS_INCLUSION_PATH} {section}[{index}] has a malformed id")
                })?;
            if !ids.insert(id.to_string()) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} contains duplicate must-pass id {id}"
                ));
            }
        }
    }

    let summary_count = |field: &str| {
        inclusion
            .pointer(&format!("/summary/{field}"))
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{MUST_PASS_INCLUSION_PATH} missing unsigned summary.{field}"))
    };
    let tier1_count = summary_count("tier1_count")?;
    let review_count = summary_count("tier1_review_count")?;
    let total_count = summary_count("total_must_pass")?;
    let observed_tier1 = u64::try_from(observed_section_counts["tier1"]).unwrap_or(u64::MAX);
    let observed_review =
        u64::try_from(observed_section_counts["tier1_review"]).unwrap_or(u64::MAX);
    let observed_total = u64::try_from(ids.len()).unwrap_or(u64::MAX);
    if tier1_count != observed_tier1
        || review_count != observed_review
        || total_count != observed_total
        || observed_total != EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1
    {
        return Err(format!(
            "{MUST_PASS_INCLUSION_PATH} summary mismatch or unexpected versioned must-pass denominator: summary={tier1_count}+{review_count}={total_count}, observed={observed_tier1}+{observed_review}={observed_total}, expected={EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1}"
        ));
    }

    let manifest: Value = serde_json::from_slice(manifest_contents)
        .map_err(|err| format!("invalid {MUST_PASS_MANIFEST_PATH}: {err}"))?;
    if manifest.get("schema").and_then(Value::as_str) != Some("pi.ext.validated-manifest.v1") {
        return Err(format!(
            "unexpected schema in {MUST_PASS_MANIFEST_PATH}: {}",
            manifest
                .get("schema")
                .and_then(Value::as_str)
                .unwrap_or("missing")
        ));
    }
    let extensions = manifest
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{MUST_PASS_MANIFEST_PATH} missing extensions array"))?;
    let mut manifest_tiers = BTreeMap::new();
    let mut manifest_artifact_ids = BTreeMap::new();
    for (index, extension) in extensions.iter().enumerate() {
        let id = extension
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.trim() == *id && !id.chars().any(char::is_control))
            .ok_or_else(|| {
                format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] has a malformed id")
            })?;
        let tier = extension
            .get("conformance_tier")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                format!(
                    "{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing unsigned conformance_tier"
                )
            })?;
        if !(1..=5).contains(&tier) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} extensions[{index}] has invalid conformance_tier {tier}; expected 1..=5"
            ));
        }
        let relative = extension
            .get("entry_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing artifact entry_path")
            })?;
        let relative_path = Path::new(relative);
        if relative.is_empty()
            || relative.trim() != relative
            || relative.chars().any(char::is_control)
            || relative.contains('\\')
            || is_windows_absolute_path(relative)
            || relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!(
                "manifest entry {id} has a non-canonical artifact entry_path: {relative:?}"
            ));
        }
        let artifact_path = format!("{MUST_PASS_ARTIFACTS_PATH}/{relative}");
        if !tracked_paths.contains(&artifact_path) {
            return Err(format!(
                "manifest entry {id} points to an artifact input not tracked by the canonical commit: {artifact_path}"
            ));
        }
        if let Some(first_id) = manifest_artifact_ids.insert(artifact_path.clone(), id.to_string())
        {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} reuses artifact entry_path {relative} for extension ids {first_id} and {id}"
            ));
        }
        if manifest_tiers.insert(id.to_string(), tier).is_some() {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} contains duplicate extension id {id}"
            ));
        }
    }

    ids.into_iter()
        .map(|id| {
            let tier = manifest_tiers.get(&id).copied().ok_or_else(|| {
                format!(
                    "canonical must-pass id {id} from {MUST_PASS_INCLUSION_PATH} is absent from {MUST_PASS_MANIFEST_PATH}"
                )
            })?;
            Ok((id, tier))
        })
        .collect()
}

fn required_must_pass_string<'a>(verdict: &'a Value, field: &str) -> Result<&'a str, String> {
    verdict
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("must-pass verdict missing non-empty {field}"))
}

fn required_must_pass_count(verdict: &Value, field: &str) -> Result<u64, String> {
    verdict
        .pointer(&format!("/observed/{field}"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("must-pass verdict missing unsigned observed.{field}"))
}

fn validate_must_pass_verdict_contract(
    verdict: &Value,
    expected_count: u64,
    expected_source_tree_sha256: &str,
    expected_inclusion_sha256: &str,
    expected_manifest_sha256: &str,
) -> Result<(), String> {
    let schema = verdict
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    if schema != MUST_PASS_GATE_SCHEMA {
        return Err(format!(
            "must-pass verdict schema mismatch: expected {MUST_PASS_GATE_SCHEMA}, found {schema}"
        ));
    }
    if verdict.get("status").and_then(Value::as_str) != Some("pass") {
        return Err("must-pass verdict status must be pass".to_string());
    }

    let git_commit = required_must_pass_string(verdict, "git_commit")?;
    if !matches!(git_commit.len(), 40 | 64)
        || !git_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(
            "must-pass verdict git_commit must be a full 40- or 64-character hexadecimal commit ID"
                .to_string(),
        );
    }
    for (field, expected) in [
        ("source_tree_sha256", expected_source_tree_sha256),
        ("inclusion_sha256", expected_inclusion_sha256),
        ("manifest_sha256", expected_manifest_sha256),
    ] {
        let actual = required_must_pass_string(verdict, field)?;
        if actual.len() != 64 || !actual.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "must-pass verdict {field} must be a 64-character hexadecimal SHA-256"
            ));
        }
        if actual != expected {
            return Err(format!(
                "must-pass verdict {field} does not match current authoritative release inputs"
            ));
        }
    }

    let total = required_must_pass_count(verdict, "must_pass_total")?;
    let tested = required_must_pass_count(verdict, "must_pass_tested")?;
    let passed = required_must_pass_count(verdict, "must_pass_passed")?;
    let failed = required_must_pass_count(verdict, "must_pass_failed")?;
    let skipped = required_must_pass_count(verdict, "must_pass_skipped")?;
    if passed.checked_add(failed) != Some(tested) || tested.checked_add(skipped) != Some(total) {
        return Err(format!(
            "must-pass verdict counts are incoherent: total={total}, tested={tested}, passed={passed}, failed={failed}, skipped={skipped}"
        ));
    }
    if total != expected_count
        || tested != expected_count
        || passed != expected_count
        || failed != 0
        || skipped != 0
    {
        return Err(format!(
            "must-pass verdict must prove the exact canonical set: expected={expected_count}, total={total}, tested={tested}, passed={passed}, failed={failed}, skipped={skipped}"
        ));
    }
    let pass_rate = verdict
        .pointer("/observed/must_pass_pass_rate_pct")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            "must-pass verdict missing numeric observed.must_pass_pass_rate_pct".to_string()
        })?;
    if pass_rate.to_bits() != 100.0_f64.to_bits() {
        return Err(format!(
            "must-pass verdict pass rate must be 100.0, found {pass_rate}"
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedGateEvents {
    must_pass: BTreeMap<String, u64>,
    stretch: BTreeSet<String>,
}

fn observed_must_pass_events_from_contents(
    contents: &str,
    verdict: &Value,
) -> Result<ObservedGateEvents, String> {
    let run_id = required_must_pass_string(verdict, "run_id")?;
    let correlation_id = required_must_pass_string(verdict, "correlation_id")?;
    let git_commit = required_must_pass_string(verdict, "git_commit")?;
    let source_tree_sha256 = required_must_pass_string(verdict, "source_tree_sha256")?;
    let inclusion_sha256 = required_must_pass_string(verdict, "inclusion_sha256")?;
    let manifest_sha256 = required_must_pass_string(verdict, "manifest_sha256")?;
    let mut must_pass = BTreeMap::new();
    let mut stretch = BTreeSet::new();
    let mut event_keys = BTreeSet::new();

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|err| {
            format!(
                "invalid JSONL row {} in {MUST_PASS_EVENTS_PATH}: {err}",
                line_index + 1
            )
        })?;
        if event.get("schema").and_then(Value::as_str) != Some(MUST_PASS_EVENT_SCHEMA) {
            return Err(format!(
                "invalid event schema at {MUST_PASS_EVENTS_PATH}:{}",
                line_index + 1
            ));
        }
        let set = event.get("set").and_then(Value::as_str).ok_or_else(|| {
            format!(
                "event at {MUST_PASS_EVENTS_PATH}:{} is missing a string set",
                line_index + 1
            )
        })?;
        if !matches!(set, "must_pass" | "stretch") {
            return Err(format!(
                "event at {MUST_PASS_EVENTS_PATH}:{} has unexpected set {set}",
                line_index + 1
            ));
        }
        for (field, expected) in [
            ("run_id", run_id),
            ("correlation_id", correlation_id),
            ("git_commit", git_commit),
            ("source_tree_sha256", source_tree_sha256),
            ("inclusion_sha256", inclusion_sha256),
            ("manifest_sha256", manifest_sha256),
        ] {
            if event.get(field).and_then(Value::as_str) != Some(expected) {
                return Err(format!(
                    "must-pass event {field} mismatch at {MUST_PASS_EVENTS_PATH}:{}",
                    line_index + 1
                ));
            }
        }
        let id = event
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.trim() == *id && !id.chars().any(char::is_control))
            .ok_or_else(|| {
                format!(
                    "must-pass event at {MUST_PASS_EVENTS_PATH}:{} has a malformed id",
                    line_index + 1
                )
            })?;
        let tier = event.get("tier").and_then(Value::as_u64).ok_or_else(|| {
            format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} is missing an unsigned tier",
                line_index + 1
            )
        })?;
        if !(1..=5).contains(&tier) {
            return Err(format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} has invalid tier {tier}",
                line_index + 1
            ));
        }
        let status = event.get("status").and_then(Value::as_str).ok_or_else(|| {
            format!(
                "event at {MUST_PASS_EVENTS_PATH}:{} is missing a string status",
                line_index + 1
            )
        })?;
        if !matches!(status, "pass" | "fail" | "skip") {
            return Err(format!(
                "event at {MUST_PASS_EVENTS_PATH}:{} has unexpected status {status}",
                line_index + 1
            ));
        }
        if !event_keys.insert((set.to_string(), id.to_string())) {
            let set_label = if set == "must_pass" {
                "must-pass"
            } else {
                "stretch"
            };
            return Err(format!(
                "duplicate {set_label} event id {id} in {MUST_PASS_EVENTS_PATH}"
            ));
        }
        if set == "must_pass" {
            if status != "pass" {
                return Err(format!(
                    "non-pass must-pass event at {MUST_PASS_EVENTS_PATH}:{}",
                    line_index + 1
                ));
            }
            must_pass.insert(id.to_string(), tier);
        } else {
            stretch.insert(id.to_string());
        }
    }
    if must_pass.is_empty() {
        return Err(format!(
            "{MUST_PASS_EVENTS_PATH} contains no must-pass events"
        ));
    }
    Ok(ObservedGateEvents { must_pass, stretch })
}

fn observed_must_pass_events(
    root: &Path,
    verdict: &Value,
) -> Result<BTreeMap<String, u64>, String> {
    let contents = std::fs::read_to_string(root.join(MUST_PASS_EVENTS_PATH))
        .map_err(|err| format!("failed to read {MUST_PASS_EVENTS_PATH}: {err}"))?;
    observed_must_pass_events_from_contents(&contents, verdict).map(|events| events.must_pass)
}

fn validate_event_set_identities(
    events: &ObservedGateEvents,
    expected: &BTreeMap<String, u64>,
) -> Result<(), String> {
    if let Some(mislabeled_id) = events.stretch.iter().find(|id| expected.contains_key(*id)) {
        return Err(format!(
            "canonical must-pass id {mislabeled_id} is incorrectly labeled stretch in {MUST_PASS_EVENTS_PATH}"
        ));
    }
    Ok(())
}

fn evidence_followup_path_allowed(path: &str) -> bool {
    [
        "tests/ext_conformance/reports/",
        "tests/perf/reports/",
        "tests/cross_platform_reports/",
        "tests/franken_node_compat/reports/",
        "tests/evidence_bundle/",
        "tests/certification/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

fn validate_must_pass_git_commit(
    root: &Path,
    source_commit: &str,
    current_commit: &str,
) -> Result<(), String> {
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("must-pass evidence git_commit is not a full commit ID".to_string());
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ])
        .output()
        .map_err(|err| format!("failed to resolve must-pass evidence git_commit: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "must-pass evidence git_commit does not resolve to a commit: {source_commit}"
        ));
    }
    let resolved = String::from_utf8(output.stdout)
        .map_err(|err| format!("git rev-parse returned non-UTF-8 output: {err}"))?;
    if !resolved.trim().eq_ignore_ascii_case(source_commit) {
        return Err(format!(
            "must-pass evidence git_commit did not resolve exactly: expected {source_commit}, found {}",
            resolved.trim()
        ));
    }
    let ancestor_status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", source_commit, current_commit])
        .status()
        .map_err(|err| format!("failed to inspect must-pass evidence ancestry: {err}"))?;
    match ancestor_status.code() {
        Some(0) => {}
        Some(1) => {
            return Err(format!(
                "must-pass evidence git_commit {source_commit} is not an ancestor of current release commit {current_commit}"
            ));
        }
        code => {
            return Err(format!(
                "git merge-base failed while inspecting evidence ancestry (status {code:?})"
            ));
        }
    }
    if source_commit.eq_ignore_ascii_case(current_commit) {
        return Ok(());
    }

    let history = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            "--format=",
            "--name-only",
            "-z",
            "--no-renames",
            &format!("{source_commit}..{current_commit}"),
            "--",
        ])
        .output()
        .map_err(|err| format!("failed to inspect evidence follow-up history: {err}"))?;
    if !history.status.success() {
        return Err(format!(
            "git log failed while inspecting evidence follow-up history: {}",
            String::from_utf8_lossy(&history.stderr).trim()
        ));
    }
    let paths = String::from_utf8(history.stdout)
        .map_err(|err| format!("git log returned non-UTF-8 paths: {err}"))?;
    let disallowed = paths
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter(|path| !evidence_followup_path_allowed(path))
        .take(5)
        .collect::<Vec<_>>();
    if !disallowed.is_empty() {
        return Err(format!(
            "must-pass evidence git_commit is followed by commits touching non-evidence paths: {}",
            disallowed.join(", ")
        ));
    }
    Ok(())
}

fn validate_authoritative_must_pass_artifacts(root: &Path, verdict: &Value) -> Result<(), String> {
    let evidence_before = capture_committed_must_pass_evidence(root)?;
    let committed_verdict: Value = serde_json::from_slice(&evidence_before.verdict_contents)
        .map_err(|err| format!("failed to parse committed {MUST_PASS_VERDICT_PATH}: {err}"))?;
    if &committed_verdict != verdict {
        return Err(format!(
            "validated {MUST_PASS_VERDICT_PATH} does not match the commit-bound artifact bytes"
        ));
    }
    let events_contents = std::str::from_utf8(&evidence_before.events_contents)
        .map_err(|err| format!("committed {MUST_PASS_EVENTS_PATH} is not UTF-8: {err}"))?;
    let snapshot = capture_must_pass_source_snapshot(root)?;
    let expected = canonical_must_pass_entries(
        &snapshot.inclusion_contents,
        &snapshot.manifest_contents,
        &snapshot.tracked_paths,
    )?;
    let expected_count = u64::try_from(expected.len()).unwrap_or(u64::MAX);
    validate_must_pass_verdict_contract(
        verdict,
        expected_count,
        &snapshot.source_tree_sha256,
        &snapshot.inclusion_sha256,
        &snapshot.manifest_sha256,
    )?;
    validate_must_pass_git_commit(
        root,
        required_must_pass_string(verdict, "git_commit")?,
        &snapshot.git_commit,
    )?;
    let observed_events = observed_must_pass_events_from_contents(events_contents, verdict)?;
    validate_event_set_identities(&observed_events, &expected)?;
    let observed = observed_events.must_pass;
    if observed != expected {
        let expected_ids = expected.keys().cloned().collect::<BTreeSet<_>>();
        let observed_ids = observed.keys().cloned().collect::<BTreeSet<_>>();
        let missing = expected_ids
            .difference(&observed_ids)
            .take(5)
            .cloned()
            .collect::<Vec<_>>();
        let unexpected = observed_ids
            .difference(&expected_ids)
            .take(5)
            .cloned()
            .collect::<Vec<_>>();
        let tier_mismatches = expected
            .iter()
            .filter_map(|(id, expected_tier)| {
                observed
                    .get(id)
                    .filter(|observed_tier| *observed_tier != expected_tier)
                    .map(|observed_tier| format!("{id}:{observed_tier}!={expected_tier}"))
            })
            .take(5)
            .collect::<Vec<_>>();
        return Err(format!(
            "must-pass events do not match the exact canonical inclusion-list set: expected={}, observed={}, missing=[{}], unexpected=[{}], tier_mismatches=[{}]",
            expected.len(),
            observed.len(),
            missing.join(", "),
            unexpected.join(", "),
            tier_mismatches.join(", ")
        ));
    }
    let snapshot_after = capture_must_pass_source_snapshot(root)?;
    if snapshot != snapshot_after {
        return Err(
            "must-pass canonical source inputs changed during aggregate release-gate validation"
                .to_string(),
        );
    }
    let evidence_after = capture_committed_must_pass_evidence(root)?;
    if evidence_before != evidence_after {
        return Err(
            "committed must-pass evidence changed during aggregate release-gate validation"
                .to_string(),
        );
    }
    Ok(())
}

const MUST_PASS_LINEAGE_MAX_AGE_DAYS: i64 = 7;
const MUST_PASS_LINEAGE_MAX_FUTURE_SKEW_MINUTES: i64 = 5;

fn validate_must_pass_lineage_metadata(
    verdict: &Value,
    artifact_rel: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let run_id = verdict
        .get("run_id")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if run_id.is_empty() {
        return Err(format!(
            "{artifact_rel} missing non-empty lineage field 'run_id'"
        ));
    }

    let correlation_id = verdict
        .get("correlation_id")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if correlation_id.is_empty() {
        return Err(format!(
            "{artifact_rel} missing non-empty lineage field 'correlation_id'"
        ));
    }
    if !correlation_id.contains(run_id) {
        return Err(format!(
            "{artifact_rel} correlation_id '{correlation_id}' must include run_id '{run_id}'"
        ));
    }

    let generated_at = verdict
        .get("generated_at")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if generated_at.is_empty() {
        return Err(format!(
            "{artifact_rel} missing non-empty freshness field 'generated_at'"
        ));
    }
    let parsed_generated_at = chrono::DateTime::parse_from_rfc3339(generated_at)
        .map_err(|err| format!("{artifact_rel} has invalid generated_at '{generated_at}': {err}"))?
        .with_timezone(&chrono::Utc);

    let oldest_allowed = now - chrono::Duration::days(MUST_PASS_LINEAGE_MAX_AGE_DAYS);
    let newest_allowed = now + chrono::Duration::minutes(MUST_PASS_LINEAGE_MAX_FUTURE_SKEW_MINUTES);
    if parsed_generated_at < oldest_allowed {
        return Err(format!(
            "{artifact_rel} generated_at '{generated_at}' is stale (older than {MUST_PASS_LINEAGE_MAX_AGE_DAYS} days)"
        ));
    }
    if parsed_generated_at > newest_allowed {
        return Err(format!(
            "{artifact_rel} generated_at '{generated_at}' is too far in the future"
        ));
    }

    Ok(())
}

fn check_must_pass_gate_artifact(root: &Path, artifact_rel: &str) -> (String, Option<String>) {
    let (status, detail) = check_artifact_status(root, artifact_rel, &["status"], &["pass"]);
    if status != "pass" {
        return (status, detail);
    }

    let full = root.join(artifact_rel);
    let Some(verdict) = load_json(&full) else {
        return (
            "skip".to_string(),
            Some(format!("Artifact not found: {artifact_rel}")),
        );
    };

    if let Err(detail) =
        validate_must_pass_lineage_metadata(&verdict, artifact_rel, chrono::Utc::now())
    {
        return ("fail".to_string(), Some(detail));
    }
    if let Err(detail) = validate_authoritative_must_pass_artifacts(root, &verdict) {
        return ("fail".to_string(), Some(detail));
    }

    ("pass".to_string(), None)
}

fn check_extension_remediation_backlog_artifact(
    root: &Path,
    artifact_rel: &str,
) -> (String, Option<String>) {
    let full = root.join(artifact_rel);
    let Some(backlog) = load_json(&full) else {
        return (
            "skip".to_string(),
            Some(format!("Artifact not found: {artifact_rel}")),
        );
    };

    let schema = backlog
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if schema != EXTENSION_REMEDIATION_BACKLOG_SCHEMA {
        return (
            "fail".to_string(),
            Some(format!(
                "schema='{schema}' (expected '{EXTENSION_REMEDIATION_BACKLOG_SCHEMA}')"
            )),
        );
    }

    let Some(entries) = backlog.get("entries").and_then(Value::as_array) else {
        return (
            "fail".to_string(),
            Some("missing entries array".to_string()),
        );
    };
    let entry_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);

    let Some(total_non_pass) = backlog
        .pointer("/summary/total_non_pass_extensions")
        .and_then(Value::as_u64)
    else {
        return (
            "fail".to_string(),
            Some("missing summary.total_non_pass_extensions".to_string()),
        );
    };
    let Some(actionable) = backlog
        .pointer("/summary/actionable")
        .and_then(Value::as_u64)
    else {
        return (
            "fail".to_string(),
            Some("missing summary.actionable".to_string()),
        );
    };
    let Some(non_actionable) = backlog
        .pointer("/summary/non_actionable")
        .and_then(Value::as_u64)
    else {
        return (
            "fail".to_string(),
            Some("missing summary.non_actionable".to_string()),
        );
    };

    if total_non_pass != entry_count {
        return (
            "fail".to_string(),
            Some(format!(
                "summary.total_non_pass_extensions={total_non_pass} does not match entries.len={entry_count}"
            )),
        );
    }

    let Some(summary_breakdown) = actionable.checked_add(non_actionable) else {
        return (
            "fail".to_string(),
            Some("summary.actionable + summary.non_actionable overflowed".to_string()),
        );
    };
    if summary_breakdown != total_non_pass {
        return (
            "fail".to_string(),
            Some(format!(
                "summary.actionable({actionable}) + summary.non_actionable({non_actionable}) != summary.total_non_pass_extensions({total_non_pass})"
            )),
        );
    }

    ("pass".to_string(), None)
}

fn find_latest_opportunity_matrix(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    for relative in [
        "tests/perf/reports/opportunity_matrix.json",
        "tests/perf/runs/results/opportunity_matrix.json",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }

    let e2e_results_dir = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_results_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("results/opportunity_matrix.json");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn validate_opportunity_matrix_readiness(
    readiness: &serde_json::Map<String, Value>,
    artifact: &str,
) -> Result<(), String> {
    let status = readiness
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(status, "ready" | "blocked") {
        return Err(format!(
            "opportunity_matrix.readiness.status must be ready|blocked in {artifact}, got {status}"
        ));
    }

    let decision = readiness
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(decision, "RANKED" | "NO_DECISION") {
        return Err(format!(
            "opportunity_matrix.readiness.decision must be RANKED|NO_DECISION in {artifact}, got {decision}"
        ));
    }

    let Some(ready_for_phase5) = readiness.get("ready_for_phase5").and_then(Value::as_bool) else {
        return Err(format!(
            "opportunity_matrix.readiness.ready_for_phase5 must be boolean in {artifact}"
        ));
    };
    let Some(blocking_reasons) = readiness.get("blocking_reasons").and_then(Value::as_array) else {
        return Err(format!(
            "opportunity_matrix.readiness.blocking_reasons must be an array in {artifact}"
        ));
    };

    match status {
        "ready" => {
            if !ready_for_phase5 {
                return Err(format!(
                    "opportunity_matrix.readiness.ready_for_phase5 must be true when status=ready in {artifact}"
                ));
            }
            if decision != "RANKED" {
                return Err(format!(
                    "opportunity_matrix.readiness.decision must be RANKED when status=ready in {artifact}"
                ));
            }
            if !blocking_reasons.is_empty() {
                return Err(format!(
                    "opportunity_matrix.readiness.blocking_reasons must be empty when status=ready in {artifact}"
                ));
            }
        }
        "blocked" => {
            if ready_for_phase5 {
                return Err(format!(
                    "opportunity_matrix.readiness.ready_for_phase5 must be false when status=blocked in {artifact}"
                ));
            }
            if decision != "NO_DECISION" {
                return Err(format!(
                    "opportunity_matrix.readiness.decision must be NO_DECISION when status=blocked in {artifact}"
                ));
            }
            if blocking_reasons.is_empty() {
                return Err(format!(
                    "opportunity_matrix.readiness.blocking_reasons must be non-empty when status=blocked in {artifact}"
                ));
            }
        }
        _ => {}
    }

    Ok(())
}

fn validate_opportunity_matrix_ranked_rows(
    ranked: &[Value],
    readiness_status: &str,
    artifact: &str,
) -> Result<(), String> {
    if readiness_status == "ready" && ranked.is_empty() {
        return Err(format!(
            "opportunity_matrix.ranked_opportunities must be non-empty when readiness.status=ready in {artifact}"
        ));
    }
    if readiness_status == "blocked" && !ranked.is_empty() {
        return Err(format!(
            "opportunity_matrix.ranked_opportunities must be empty when readiness.status=blocked in {artifact}"
        ));
    }

    for (index, row) in ranked.iter().enumerate() {
        let Some(row_obj) = row.as_object() else {
            return Err(format!(
                "opportunity_matrix.ranked_opportunities[{index}] must be an object in {artifact}"
            ));
        };
        let expected_rank = u64::try_from(index + 1).unwrap_or(u64::MAX);
        let rank = row_obj.get("rank").and_then(Value::as_u64).ok_or_else(|| {
            format!(
                "opportunity_matrix.ranked_opportunities[{index}].rank must be a positive integer in {artifact}"
            )
        })?;
        if rank != expected_rank {
            return Err(format!(
                "opportunity_matrix.ranked_opportunities[{index}].rank expected {expected_rank}, got {rank} in {artifact}"
            ));
        }

        let stage = row_obj
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if stage.is_empty() {
            return Err(format!(
                "opportunity_matrix.ranked_opportunities[{index}].stage must be non-empty in {artifact}"
            ));
        }

        let Some(priority_score) = row_obj.get("priority_score").and_then(Value::as_f64) else {
            return Err(format!(
                "opportunity_matrix.ranked_opportunities[{index}].priority_score must be numeric in {artifact}"
            ));
        };
        if !priority_score.is_finite() || priority_score <= 0.0 {
            return Err(format!(
                "opportunity_matrix.ranked_opportunities[{index}].priority_score must be > 0 in {artifact}"
            ));
        }
    }

    Ok(())
}

fn check_opportunity_matrix_artifact(root: &Path) -> (String, Option<String>) {
    let Some(path) = find_latest_opportunity_matrix(root) else {
        return (
            "skip".to_string(),
            Some(
                "opportunity_matrix artifact not found (expected tests/perf/reports, tests/perf/runs/results, or tests/e2e_results/*/results)".to_string(),
            ),
        );
    };
    let artifact = path.strip_prefix(root).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    );
    let Some(matrix) = load_json(&path) else {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix artifact is not valid JSON: {artifact}"
            )),
        );
    };

    let schema = matrix
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if schema != OPPORTUNITY_MATRIX_SCHEMA {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix schema mismatch in {artifact}: expected {OPPORTUNITY_MATRIX_SCHEMA}, got {schema}"
            )),
        );
    }

    let Some(source_identity) = matrix.get("source_identity").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.source_identity must be an object in {artifact}"
            )),
        );
    };
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source_artifact != "phase1_matrix_validation" {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.source_identity.source_artifact mismatch in {artifact}: {source_artifact}"
            )),
        );
    }
    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source_artifact_path.trim().is_empty()
        || !source_artifact_path.contains("phase1_matrix_validation.json")
    {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.source_identity.source_artifact_path must reference phase1_matrix_validation.json in {artifact}"
            )),
        );
    }

    let weighted_schema = source_identity
        .get("weighted_bottleneck_schema")
        .and_then(Value::as_str)
        .unwrap_or("");
    if weighted_schema != "pi.perf.phase1_weighted_bottleneck_attribution.v1" {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.source_identity.weighted_bottleneck_schema mismatch in {artifact}: {weighted_schema}"
            )),
        );
    }
    let weighted_status = source_identity
        .get("weighted_bottleneck_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(weighted_status, "computed" | "missing") {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.source_identity.weighted_bottleneck_status must be computed|missing in {artifact}, got {weighted_status}"
            )),
        );
    }

    let Some(readiness) = matrix.get("readiness").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.readiness must be an object in {artifact}"
            )),
        );
    };
    if let Err(detail) = validate_opportunity_matrix_readiness(readiness, &artifact) {
        return ("fail".to_string(), Some(detail));
    }
    let readiness_status = readiness
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let Some(ranked) = matrix.get("ranked_opportunities").and_then(Value::as_array) else {
        return (
            "fail".to_string(),
            Some(format!(
                "opportunity_matrix.ranked_opportunities must be an array in {artifact}"
            )),
        );
    };
    if let Err(detail) =
        validate_opportunity_matrix_ranked_rows(ranked, readiness_status, &artifact)
    {
        return ("fail".to_string(), Some(detail));
    }

    ("pass".to_string(), None)
}

fn find_latest_parameter_sweeps(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    for relative in [
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/runs/results/parameter_sweeps.json",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }

    let e2e_results_dir = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_results_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("results/parameter_sweeps.json");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn validate_parameter_sweeps_readiness(
    readiness: &serde_json::Map<String, Value>,
    artifact: &str,
) -> Result<(), String> {
    let status = readiness
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(status, "ready" | "blocked") {
        return Err(format!(
            "parameter_sweeps.readiness.status must be ready|blocked in {artifact}, got {status}"
        ));
    }

    let Some(ready_for_phase5) = readiness.get("ready_for_phase5").and_then(Value::as_bool) else {
        return Err(format!(
            "parameter_sweeps.readiness.ready_for_phase5 must be boolean in {artifact}"
        ));
    };
    let Some(blocking_reasons) = readiness.get("blocking_reasons").and_then(Value::as_array) else {
        return Err(format!(
            "parameter_sweeps.readiness.blocking_reasons must be an array in {artifact}"
        ));
    };

    match status {
        "ready" => {
            if !ready_for_phase5 {
                return Err(format!(
                    "parameter_sweeps.readiness.ready_for_phase5 must be true when status=ready in {artifact}"
                ));
            }
            if !blocking_reasons.is_empty() {
                return Err(format!(
                    "parameter_sweeps.readiness.blocking_reasons must be empty when status=ready in {artifact}"
                ));
            }
        }
        "blocked" => {
            if ready_for_phase5 {
                return Err(format!(
                    "parameter_sweeps.readiness.ready_for_phase5 must be false when status=blocked in {artifact}"
                ));
            }
            if blocking_reasons.is_empty() {
                return Err(format!(
                    "parameter_sweeps.readiness.blocking_reasons must be non-empty when status=blocked in {artifact}"
                ));
            }
        }
        _ => {}
    }

    Ok(())
}

fn validate_parameter_sweeps_selected_defaults(
    selected_defaults: &serde_json::Map<String, Value>,
    artifact: &str,
) -> Result<(), String> {
    for key in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        let Some(value) = selected_defaults.get(key).and_then(Value::as_u64) else {
            return Err(format!(
                "parameter_sweeps.selected_defaults.{key} must be a positive integer in {artifact}"
            ));
        };
        if value == 0 {
            return Err(format!(
                "parameter_sweeps.selected_defaults.{key} must be > 0 in {artifact}"
            ));
        }
    }
    Ok(())
}

fn validate_parameter_sweeps_dimensions(
    sweep_plan: &serde_json::Map<String, Value>,
    artifact: &str,
) -> Result<(), String> {
    let Some(dimensions) = sweep_plan.get("dimensions").and_then(Value::as_array) else {
        return Err(format!(
            "parameter_sweeps.sweep_plan.dimensions must be an array in {artifact}"
        ));
    };

    let mut seen = HashSet::new();
    for (index, dimension) in dimensions.iter().enumerate() {
        let Some(dimension_obj) = dimension.as_object() else {
            return Err(format!(
                "parameter_sweeps.sweep_plan.dimensions[{index}] must be an object in {artifact}"
            ));
        };
        let name = dimension_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return Err(format!(
                "parameter_sweeps.sweep_plan.dimensions[{index}].name must be non-empty in {artifact}"
            ));
        }

        let Some(candidate_values) = dimension_obj
            .get("candidate_values")
            .and_then(Value::as_array)
        else {
            return Err(format!(
                "parameter_sweeps.sweep_plan.dimensions[{index}].candidate_values must be an array in {artifact}"
            ));
        };
        if candidate_values.is_empty() {
            return Err(format!(
                "parameter_sweeps.sweep_plan.dimensions[{index}].candidate_values must be non-empty in {artifact}"
            ));
        }

        seen.insert(name.to_string());
    }

    for required in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        if !seen.contains(required) {
            return Err(format!(
                "parameter_sweeps.sweep_plan.dimensions missing required knob {required} in {artifact}"
            ));
        }
    }

    Ok(())
}

fn check_parameter_sweeps_artifact(root: &Path) -> (String, Option<String>) {
    let Some(path) = find_latest_parameter_sweeps(root) else {
        return (
            "skip".to_string(),
            Some(
                "parameter_sweeps artifact not found (expected tests/perf/reports, tests/perf/runs/results, or tests/e2e_results/*/results)".to_string(),
            ),
        );
    };
    let artifact = path.strip_prefix(root).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    );
    let Some(sweeps) = load_json(&path) else {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps artifact is not valid JSON: {artifact}"
            )),
        );
    };

    let schema = sweeps
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if schema != PARAMETER_SWEEPS_SCHEMA {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps schema mismatch in {artifact}: expected {PARAMETER_SWEEPS_SCHEMA}, got {schema}"
            )),
        );
    }

    let Some(source_identity) = sweeps.get("source_identity").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.source_identity must be an object in {artifact}"
            )),
        );
    };
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source_artifact != "phase1_matrix_validation" {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.source_identity.source_artifact mismatch in {artifact}: {source_artifact}"
            )),
        );
    }
    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source_artifact_path.trim().is_empty()
        || !source_artifact_path.contains("phase1_matrix_validation.json")
    {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.source_identity.source_artifact_path must reference phase1_matrix_validation.json in {artifact}"
            )),
        );
    }

    let Some(readiness) = sweeps.get("readiness").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.readiness must be an object in {artifact}"
            )),
        );
    };
    if let Err(detail) = validate_parameter_sweeps_readiness(readiness, &artifact) {
        return ("fail".to_string(), Some(detail));
    }

    let Some(selected_defaults) = sweeps.get("selected_defaults").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.selected_defaults must be an object in {artifact}"
            )),
        );
    };
    if let Err(detail) = validate_parameter_sweeps_selected_defaults(selected_defaults, &artifact) {
        return ("fail".to_string(), Some(detail));
    }

    let Some(sweep_plan) = sweeps.get("sweep_plan").and_then(Value::as_object) else {
        return (
            "fail".to_string(),
            Some(format!(
                "parameter_sweeps.sweep_plan must be an object in {artifact}"
            )),
        );
    };
    if let Err(detail) = validate_parameter_sweeps_dimensions(sweep_plan, &artifact) {
        return ("fail".to_string(), Some(detail));
    }

    ("pass".to_string(), None)
}

/// Gate 20: Cross-artifact lineage coherence for conformance+stress certification (bd-3ar8v.6.3).
///
/// Validates that conformance_summary.json and stress_triage.json both carry
/// non-empty `run_id` and `correlation_id` fields, ensuring Phase-5 lineage
/// traceability across the full conformance+stress certification pipeline.
fn check_conformance_stress_lineage_coherence(root: &Path) -> (String, Option<String>) {
    let mut issues: Vec<String> = Vec::new();

    // Check conformance_summary.json lineage
    let conformance_path = root.join(CONFORMANCE_SUMMARY_ARTIFACT_REL);
    match load_json(&conformance_path) {
        Some(conformance) => {
            let run_id = conformance
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let correlation_id = conformance
                .get("correlation_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if run_id.trim().is_empty() {
                issues.push("conformance_summary.json missing non-empty run_id".to_string());
            }
            if correlation_id.trim().is_empty() {
                issues
                    .push("conformance_summary.json missing non-empty correlation_id".to_string());
            }
        }
        None => {
            issues.push(format!(
                "conformance_summary.json not found or invalid at {CONFORMANCE_SUMMARY_ARTIFACT_REL}"
            ));
        }
    }

    // Check stress_triage.json lineage
    let stress_path = root.join(STRESS_TRIAGE_ARTIFACT_REL);
    match load_json(&stress_path) {
        Some(stress) => {
            let run_id = stress.get("run_id").and_then(Value::as_str).unwrap_or("");
            let correlation_id = stress
                .get("correlation_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if run_id.trim().is_empty() {
                issues.push("stress_triage.json missing non-empty run_id".to_string());
            }
            if correlation_id.trim().is_empty() {
                issues.push("stress_triage.json missing non-empty correlation_id".to_string());
            }
            let pass = stress.get("pass").and_then(Value::as_bool).unwrap_or(false);
            if !pass {
                issues.push("stress_triage.json verdict is not pass".to_string());
            }
        }
        None => {
            issues.push(format!(
                "stress_triage.json not found or invalid at {STRESS_TRIAGE_ARTIFACT_REL}"
            ));
        }
    }

    if issues.is_empty() {
        (
            "pass".to_string(),
            Some("Conformance+stress lineage coherence validated: both artifacts carry run_id and correlation_id".to_string()),
        )
    } else {
        (
            "fail".to_string(),
            Some(format!(
                "Conformance+stress lineage coherence failed: {}",
                issues.join("; ")
            )),
        )
    }
}

/// Check that a file exists and is non-empty.
fn check_artifact_present(root: &Path, artifact_rel: &str) -> (String, Option<String>) {
    let full = root.join(artifact_rel);
    if full.is_file() {
        let size = std::fs::metadata(&full).map_or(0, |m| m.len());
        if size > 0 {
            ("pass".to_string(), None)
        } else {
            (
                "warn".to_string(),
                Some(format!("Artifact empty: {artifact_rel}")),
            )
        }
    } else if full.is_dir() {
        ("pass".to_string(), None)
    } else {
        (
            "skip".to_string(),
            Some(format!("Artifact not found: {artifact_rel}")),
        )
    }
}

fn write_non_empty_artifact(
    path: &Path,
    artifact_rel: &str,
    contents: &str,
) -> Result<u64, String> {
    if contents.trim().is_empty() {
        return Err(format!(
            "generated empty artifact payload for {artifact_rel}"
        ));
    }

    std::fs::write(path, contents).map_err(|err| {
        format!(
            "failed to write {artifact_rel} at {}: {err}",
            path.display()
        )
    })?;

    let bytes = std::fs::metadata(path)
        .map_err(|err| format!("failed to stat {artifact_rel} at {}: {err}", path.display()))?
        .len();
    if bytes == 0 {
        return Err(format!(
            "zero-byte artifact emitted for {artifact_rel} at {}",
            path.display()
        ));
    }

    Ok(bytes)
}

fn assert_non_empty_text_artifact(path: &Path, artifact_rel: &str) -> Result<u64, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {artifact_rel} at {}: {err}", path.display()))?;
    if contents.trim().is_empty() {
        return Err(format!(
            "empty artifact body detected for {artifact_rel} at {}",
            path.display()
        ));
    }
    Ok(contents.len() as u64)
}

/// Convert blocking `skip` states into fail-closed failures.
fn fail_close_blocking_skips(gates: &mut [SubGate]) {
    for gate in gates {
        if gate.blocking && gate.status == "skip" {
            gate.status = "fail".to_string();
            let prior_detail = gate
                .detail
                .take()
                .unwrap_or_else(|| "Blocking gate reported skip".to_string());
            gate.detail = Some(format!(
                "{prior_detail}; fail-closed policy: blocking gates cannot remain in skip state"
            ));
        }
    }
}

/// Collect all sub-gate results.
#[allow(clippy::too_many_lines)]
fn collect_gates(root: &Path) -> Vec<SubGate> {
    let mut gates = Vec::new();

    // Gate 1: Non-mock unit compliance.
    let (status, detail) = check_artifact_present(root, "docs/non-mock-rubric.json");
    gates.push(SubGate {
        id: "non_mock_unit".to_string(),
        name: "Non-mock unit compliance".to_string(),
        bead: "bd-1f42.2.6".to_string(),
        status,
        blocking: true,
        artifact_path: Some("docs/non-mock-rubric.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test non_mock_compliance_gate -- --nocapture".to_string(),
        ),
    });

    // Gate 2: E2E log contract.
    let (status, detail) = check_artifact_present(root, "tests/e2e_results");
    gates.push(SubGate {
        id: "e2e_log_contract".to_string(),
        name: "E2E log contract and transcripts".to_string(),
        bead: "bd-1f42.3.6".to_string(),
        status,
        blocking: false,
        artifact_path: Some("tests/e2e_results".to_string()),
        detail,
        reproduce_command: None,
    });

    // Gate 3: Extension must-pass gate.
    let (status, detail) = check_must_pass_gate_artifact(root, MUST_PASS_VERDICT_PATH);
    gates.push(SubGate {
        id: "ext_must_pass".to_string(),
        name: "Extension must-pass gate".to_string(),
        bead: "bd-1f42.4.4".to_string(),
        status,
        blocking: true,
        artifact_path: Some(MUST_PASS_VERDICT_PATH.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture --exact".to_string(),
        ),
    });

    // Gate 4: Extension provider compatibility.
    let (status, detail) = check_artifact_present(
        root,
        "tests/ext_conformance/reports/provider_compat/provider_compat_report.json",
    );
    gates.push(SubGate {
        id: "ext_provider_compat".to_string(),
        name: "Extension provider compatibility matrix".to_string(),
        bead: "bd-1f42.4.6".to_string(),
        status,
        blocking: false,
        artifact_path: Some(
            "tests/ext_conformance/reports/provider_compat/provider_compat_report.json".to_string(),
        ),
        detail,
        reproduce_command: Some(
            "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_provider_compat_matrix --nocapture --exact".to_string(),
        ),
    });

    // Gate 5: Unified evidence bundle.
    let (status, detail) = check_artifact_status(
        root,
        "tests/evidence_bundle/index.json",
        &["summary", "verdict"],
        &["complete", "partial"],
    );
    gates.push(SubGate {
        id: "evidence_bundle".to_string(),
        name: "Unified evidence bundle".to_string(),
        bead: "bd-1f42.6.8".to_string(),
        status,
        blocking: false,
        artifact_path: Some("tests/evidence_bundle/index.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture --exact"
                .to_string(),
        ),
    });

    // Gate 6: Cross-platform matrix.
    let platform = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    };
    let xplat_path = format!("tests/cross_platform_reports/{platform}/platform_report.json");
    let (status, detail) =
        check_artifact_status(root, &xplat_path, &["summary", "all_required_pass"], &[]);
    // Special handling: check bool field, not string.
    let (status, detail) = if status == "fail" {
        // The field is a boolean, not a string — check it directly.
        let full = root.join(&xplat_path);
        load_json(&full).map_or((status, detail), |val| {
            let ok = val
                .pointer("/summary/all_required_pass")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if ok {
                ("pass".to_string(), None)
            } else {
                (
                    "fail".to_string(),
                    Some("Not all required platform checks passed".to_string()),
                )
            }
        })
    } else {
        (status, detail)
    };
    gates.push(SubGate {
        id: "cross_platform".to_string(),
        name: "Cross-platform matrix validation".to_string(),
        bead: "bd-1f42.6.7".to_string(),
        status,
        blocking: cfg!(target_os = "linux"),
        artifact_path: Some(xplat_path),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture --exact".to_string(),
        ),
    });

    // Gate 7: Conformance regression gate.
    let (status, detail) = check_artifact_status(
        root,
        "tests/ext_conformance/reports/regression_verdict.json",
        &["status"],
        &["pass", "warn"],
    );
    gates.push(SubGate {
        id: "conformance_regression".to_string(),
        name: "Conformance regression gate".to_string(),
        bead: "bd-1f42.4".to_string(),
        status,
        blocking: true,
        artifact_path: Some("tests/ext_conformance/reports/regression_verdict.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test conformance_regression_gate -- --nocapture".to_string(),
        ),
    });

    // Gate 8: Conformance summary (pass rate).
    let (status, detail) = {
        let artifact = "tests/ext_conformance/reports/conformance_summary.json";
        let full = root.join(artifact);
        load_json(&full).map_or_else(
            || {
                (
                    "skip".to_string(),
                    Some(format!("Artifact not found: {artifact}")),
                )
            },
            |val| {
                let rate = val
                    .get("pass_rate_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if rate >= 80.0 {
                    ("pass".to_string(), None)
                } else {
                    (
                        "fail".to_string(),
                        Some(format!("Pass rate {rate:.1}% < 80% threshold")),
                    )
                }
            },
        )
    };
    gates.push(SubGate {
        id: "conformance_pass_rate".to_string(),
        name: "Conformance pass rate >= 80%".to_string(),
        bead: "bd-1f42.4".to_string(),
        status,
        blocking: true,
        artifact_path: Some("tests/ext_conformance/reports/conformance_summary.json".to_string()),
        detail,
        reproduce_command: Some("cargo test --test conformance_report -- --nocapture".to_string()),
    });

    // Gate 9: Suite classification guard (all tests classified).
    let (status, detail) = check_artifact_present(root, "tests/suite_classification.toml");
    gates.push(SubGate {
        id: "suite_classification".to_string(),
        name: "Suite classification guard".to_string(),
        bead: "bd-1f42.6.1".to_string(),
        status,
        blocking: true,
        artifact_path: Some("tests/suite_classification.toml".to_string()),
        detail,
        reproduce_command: None,
    });

    // Gate 10: Traceability matrix.
    let (status, detail) = check_artifact_present(root, "docs/traceability_matrix.json");
    gates.push(SubGate {
        id: "traceability_matrix".to_string(),
        name: "Requirement traceability matrix".to_string(),
        bead: "bd-1f42.6.4".to_string(),
        status,
        blocking: false,
        artifact_path: Some("docs/traceability_matrix.json".to_string()),
        detail,
        reproduce_command: None,
    });

    // Gate 11: Canonical E2E scenario matrix.
    let (status, detail) = check_artifact_present(root, "docs/e2e_scenario_matrix.json");
    gates.push(SubGate {
        id: "e2e_scenario_matrix".to_string(),
        name: "Canonical E2E scenario matrix".to_string(),
        bead: "bd-1f42.8.5.1".to_string(),
        status,
        blocking: false,
        artifact_path: Some("docs/e2e_scenario_matrix.json".to_string()),
        detail,
        reproduce_command: Some("python3 scripts/check_traceability_matrix.py".to_string()),
    });

    // Gate 12: Provider gap test matrix (bd-3uqg.11.11.5).
    // Validates that the provider test matrix artifact exists and all focus
    // providers are listed.
    let (status, detail) = {
        let artifact = "docs/provider-gaps-test-matrix.json";
        let full = root.join(artifact);
        load_json(&full).map_or_else(
            || {
                (
                    "skip".to_string(),
                    Some(format!("Artifact not found: {artifact}")),
                )
            },
            |val| {
                let focus = val
                    .get("focus_provider_ids")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let providers = val
                    .get("providers")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if focus >= 5 && providers >= 5 {
                    ("pass".to_string(), None)
                } else {
                    (
                        "fail".to_string(),
                        Some(format!(
                            "Expected >= 5 focus providers; found {focus} focus, {providers} detailed"
                        )),
                    )
                }
            },
        )
    };
    gates.push(SubGate {
        id: "provider_gap_matrix".to_string(),
        name: "Provider gap test matrix coverage".to_string(),
        bead: "bd-3uqg.11.11.5".to_string(),
        status,
        blocking: false,
        artifact_path: Some("docs/provider-gaps-test-matrix.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test provider_native_contract --test e2e_provider_scenarios -- --nocapture"
                .to_string(),
        ),
    });

    // Gate 14: SEC-6.4 Security compatibility conformance (bd-1a2cu).
    // Validates that benign extensions remain compatible under hardened security
    // policies. Reads the conformance verdict artifact generated by the
    // sec_compatibility_conformance test suite.
    let (status, detail) = {
        let artifact = "tests/full_suite_gate/sec_conformance_verdict.json";
        let full = root.join(artifact);
        load_json(&full).map_or_else(
            || {
                (
                    "skip".to_string(),
                    Some(format!("Artifact not found: {artifact}")),
                )
            },
            |val| {
                let verdict = val
                    .get("verdict")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let pass_rate = val
                    .get("pass_rate_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let threshold = val
                    .get("threshold_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(95.0);
                if verdict == "pass" {
                    ("pass".to_string(), None)
                } else {
                    (
                        "fail".to_string(),
                        Some(format!(
                            "SEC conformance verdict={verdict}, pass_rate={pass_rate:.1}% (threshold={threshold:.0}%)"
                        )),
                    )
                }
            },
        )
    };
    gates.push(SubGate {
        id: "sec_conformance".to_string(),
        name: "SEC-6.4 security compatibility conformance".to_string(),
        bead: "bd-1a2cu".to_string(),
        status,
        blocking: true,
        artifact_path: Some("tests/full_suite_gate/sec_conformance_verdict.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test sec_compatibility_conformance -- --nocapture".to_string(),
        ),
    });

    // Gate 15: PERF-3X bead-to-evidence coverage audit (bd-3ar8v.6.11).
    let (status, detail) = evaluate_perf3x_bead_coverage(root, &perf3x_bead_coverage_contract());
    gates.push(SubGate {
        id: "perf3x_bead_coverage".to_string(),
        name: "PERF-3X bead-to-artifact coverage audit".to_string(),
        bead: "bd-3ar8v.6.11".to_string(),
        status,
        blocking: true,
        artifact_path: Some(PERF3X_COVERAGE_AUDIT_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- perf3x_bead_coverage_contract_is_well_formed --nocapture --exact".to_string(),
        ),
    });

    // Gate 16: Practical-finish checkpoint (bd-3ar8v.6.9).
    // Validates that open PERF-3X residual work is restricted to docs/report
    // wrap-up only; any remaining technical issues fail closed.
    let (status, detail) = evaluate_practical_finish_checkpoint(root);
    gates.push(SubGate {
        id: "practical_finish_checkpoint".to_string(),
        name: "Practical-finish checkpoint (docs-only residual filter)".to_string(),
        bead: "bd-3ar8v.6.9".to_string(),
        status,
        blocking: true,
        artifact_path: Some(PRACTICAL_FINISH_CHECKPOINT_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact".to_string(),
        ),
    });

    // Gate 17: Extension remediation backlog artifact integrity (bd-3ar8v.6.8).
    let (status, detail) = check_extension_remediation_backlog_artifact(
        root,
        EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL,
    );
    gates.push(SubGate {
        id: "extension_remediation_backlog".to_string(),
        name: "Extension remediation backlog artifact integrity".to_string(),
        bead: "bd-3ar8v.6.8".to_string(),
        status,
        blocking: true,
        artifact_path: Some(EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact"
                .to_string(),
        ),
    });

    // Gate 18: Opportunity matrix artifact integrity (bd-3ar8v.6.1).
    let (status, detail) = check_opportunity_matrix_artifact(root);
    gates.push(SubGate {
        id: "opportunity_matrix_integrity".to_string(),
        name: "Opportunity matrix artifact integrity".to_string(),
        bead: "bd-3ar8v.6.1".to_string(),
        status,
        blocking: true,
        artifact_path: Some(OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test release_evidence_gate -- phase1_weighted_attribution_contract_links_phase5_consumers --nocapture --exact".to_string(),
        ),
    });

    // Gate 19: Parameter sweeps artifact integrity (bd-3ar8v.6.2).
    let (status, detail) = check_parameter_sweeps_artifact(root);
    gates.push(SubGate {
        id: "parameter_sweeps_integrity".to_string(),
        name: "Parameter sweeps artifact integrity".to_string(),
        bead: "bd-3ar8v.6.2".to_string(),
        status,
        blocking: true,
        artifact_path: Some(PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact".to_string(),
        ),
    });

    // Gate 20: Conformance+stress lineage coherence (bd-3ar8v.6.3).
    // Validates that both conformance_summary.json and stress_triage.json carry
    // non-empty run_id and correlation_id for Phase-5 traceability.
    let (status, detail) = check_conformance_stress_lineage_coherence(root);
    gates.push(SubGate {
        id: "conformance_stress_lineage".to_string(),
        name: "Conformance+stress lineage coherence".to_string(),
        bead: "bd-3ar8v.6.3".to_string(),
        status,
        blocking: true,
        artifact_path: Some(CONFORMANCE_SUMMARY_ARTIFACT_REL.to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- conformance_stress_lineage_passes_with_valid_artifacts --nocapture --exact".to_string(),
        ),
    });

    // Gate 13: Waiver lifecycle (bd-1f42.8.8.1).
    // Validates that all waivers in suite_classification.toml are well-formed,
    // not expired, and have required metadata.
    let (waivers, validations) = parse_waivers(root);
    let expired_count = validations.iter().filter(|v| v.status == "expired").count();
    let invalid_count = validations.iter().filter(|v| v.status == "invalid").count();
    let expiring_count = validations
        .iter()
        .filter(|v| v.status == "expiring_soon")
        .count();
    let (status, detail) = if waivers.is_empty() {
        ("pass".to_string(), None)
    } else if expired_count > 0 || invalid_count > 0 {
        (
            "fail".to_string(),
            Some(format!(
                "{} expired, {} invalid out of {} waivers",
                expired_count,
                invalid_count,
                waivers.len()
            )),
        )
    } else if expiring_count > 0 {
        (
            "warn".to_string(),
            Some(format!(
                "{expiring_count} waiver(s) expiring within {WAIVER_EXPIRY_WARN_DAYS} days"
            )),
        )
    } else {
        ("pass".to_string(), None)
    };
    gates.push(SubGate {
        id: "waiver_lifecycle".to_string(),
        name: "Waiver lifecycle compliance".to_string(),
        bead: "bd-1f42.8.8.1".to_string(),
        status,
        blocking: true,
        artifact_path: Some("tests/full_suite_gate/waiver_audit.json".to_string()),
        detail,
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact"
                .to_string(),
        ),
    });

    fail_close_blocking_skips(&mut gates);

    gates
}

/// Full-suite CI gate test.
///
/// Run with:
/// `cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn full_suite_gate() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let root = repo_root();
    let report_dir = root.join("tests").join("full_suite_gate");
    let generate = full_suite_gate_artifact_generation_requested();
    if generate {
        prepare_report_dir_for_generation(&report_dir);
    }

    eprintln!("\n=== Full-Suite CI Gate (bd-1f42.6.5) ===\n");

    let gates = collect_gates(&root);

    for gate in &gates {
        let icon = match gate.status.as_str() {
            "pass" => "PASS",
            "fail" => "FAIL",
            "warn" => "WARN",
            _ => "SKIP",
        };
        let blocking = if gate.blocking { "(blocking)" } else { "" };
        eprintln!(
            "  [{icon}] {:<50} {:<12} {}",
            gate.name, blocking, gate.bead
        );
        if let Some(ref detail) = gate.detail {
            eprintln!("         {detail}");
        }
    }
    eprintln!();

    // ── Compute summary ──
    let passed = gates.iter().filter(|g| g.status == "pass").count();
    let failed = gates.iter().filter(|g| g.status == "fail").count();
    let warned = gates.iter().filter(|g| g.status == "warn").count();
    let skipped = gates.iter().filter(|g| g.status == "skip").count();

    let blocking_gates: Vec<&SubGate> = gates.iter().filter(|g| g.blocking).collect();
    let blocking_pass = blocking_gates.iter().filter(|g| g.status == "pass").count();
    let blocking_total = blocking_gates.len();
    let all_blocking_pass = blocking_pass == blocking_total;

    let verdict = if all_blocking_pass && failed == 0 {
        "pass"
    } else if all_blocking_pass {
        "warn"
    } else {
        "fail"
    };

    let policy = "Full-suite gate: all blocking gates must pass for release. \
                  Non-blocking gates produce warnings but do not block.";

    let report = FullSuiteVerdict {
        schema: "pi.ci.full_suite_gate.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        verdict: verdict.to_string(),
        policy: policy.to_string(),
        gates: gates.clone(),
        summary: GateSummary {
            total_gates: gates.len(),
            passed,
            failed,
            warned,
            skipped,
            blocking_pass,
            blocking_total,
            all_blocking_pass,
        },
    };

    // ── Write JSON verdict ──
    let verdict_path = report_dir.join("full_suite_verdict.json");
    let verdict_payload =
        serde_json::to_string_pretty(&report).expect("full-suite verdict must serialize as JSON");
    assert!(
        !verdict_payload.trim().is_empty(),
        "full-suite verdict payload must be non-empty"
    );

    // ── Write JSONL events ──
    let events_path = report_dir.join("full_suite_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for gate in &gates {
        let line = serde_json::json!({
            "schema": "pi.ci.full_suite_gate_event.v1",
            "gate_id": gate.id,
            "gate_name": gate.name,
            "bead": gate.bead,
            "status": gate.status,
            "blocking": gate.blocking,
            "artifact_path": gate.artifact_path,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(
            serde_json::to_string(&line).expect("full-suite gate event must serialize as JSON"),
        );
    }
    let events_payload = lines.join("\n") + "\n";
    assert!(
        !events_payload.trim().is_empty(),
        "full-suite event payload must be non-empty"
    );

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# Full-Suite CI Gate Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = writeln!(md, "> Verdict: **{}**\n", verdict.to_uppercase());

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total gates | {} |", gates.len());
    let _ = writeln!(md, "| Passed | {passed} |");
    let _ = writeln!(md, "| Failed | {failed} |");
    let _ = writeln!(md, "| Warned | {warned} |");
    let _ = writeln!(md, "| Skipped | {skipped} |");
    let _ = writeln!(md, "| Blocking pass | {blocking_pass}/{blocking_total} |");
    md.push('\n');

    md.push_str("## Gate Results\n\n");
    md.push_str(
        "| Gate | Bead | Blocking | Status | Artifact |\n|------|------|----------|--------|----------|\n",
    );
    for gate in &gates {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | `{}` |",
            gate.name,
            gate.bead,
            if gate.blocking { "YES" } else { "no" },
            gate.status.to_uppercase(),
            gate.artifact_path.as_deref().unwrap_or("-"),
        );
    }
    md.push('\n');

    let failures: Vec<&SubGate> = gates
        .iter()
        .filter(|g| g.status == "fail" || g.status == "warn")
        .collect();
    if !failures.is_empty() {
        md.push_str("## Issues Requiring Attention\n\n");
        for g in &failures {
            let blocking_tag = if g.blocking { " **(BLOCKING)**" } else { "" };
            let _ = writeln!(
                md,
                "### {} — {}{}\n",
                g.name,
                g.status.to_uppercase(),
                blocking_tag
            );
            let _ = writeln!(md, "- **Bead:** {}", g.bead);
            if let Some(ref detail) = g.detail {
                let _ = writeln!(md, "- **Detail:** {detail}");
            }
            if let Some(ref path) = g.artifact_path {
                let _ = writeln!(md, "- **Artifact:** `{path}`");
            }
            if let Some(ref cmd) = g.reproduce_command {
                let _ = writeln!(md, "- **Reproduce:**\n  ```bash\n  {cmd}\n  ```");
            }
            md.push('\n');
        }
    }

    let md_path = report_dir.join("full_suite_report.md");
    assert!(
        !md.trim().is_empty(),
        "full-suite markdown report must be non-empty"
    );

    if generate {
        write_non_empty_artifact(
            &verdict_path,
            "tests/full_suite_gate/full_suite_verdict.json",
            &verdict_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed full-suite verdict emission: {detail}"));
        write_non_empty_artifact(
            &events_path,
            "tests/full_suite_gate/full_suite_events.jsonl",
            &events_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed full-suite events emission: {detail}"));
        write_non_empty_artifact(&md_path, "tests/full_suite_gate/full_suite_report.md", &md)
            .unwrap_or_else(|detail| panic!("fail-closed full-suite report emission: {detail}"));
    }

    // ── Print summary ──
    eprintln!(
        "=== Full-Suite Gate Verdict: {} ===",
        verdict.to_uppercase()
    );
    eprintln!("  Gates:    {passed}/{} passed", gates.len());
    eprintln!("  Blocking: {blocking_pass}/{blocking_total}");
    if failed > 0 {
        eprintln!("  Failed:   {failed}");
    }
    if warned > 0 {
        eprintln!("  Warned:   {warned}");
    }
    eprintln!();
    if generate {
        eprintln!("  Reports:");
        eprintln!("    JSON:  {}", verdict_path.display());
        eprintln!("    JSONL: {}", events_path.display());
        eprintln!("    MD:    {}", md_path.display());
    } else {
        eprintln!(
            "  Reports validated in memory; set {GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV}=1 to write tracked artifacts"
        );
    }
    eprintln!();

    // ── Block release if blocking gates fail ──
    // NOTE: This is informational at build time; the actual hard gate
    // is enforced by the individual test sub-gates. This test aggregates
    // and reports but does not block (to allow partial evidence collection).
}

/// Validate full-suite gate report schema.
#[test]
fn full_suite_gate_report_schema() {
    let report_path = repo_root()
        .join("tests")
        .join("full_suite_gate")
        .join("full_suite_verdict.json");

    let Some(val) = load_json(&report_path) else {
        eprintln!("  SKIP: Report not found. Run full_suite_gate first.");
        return;
    };

    assert_eq!(
        val.get("schema").and_then(Value::as_str),
        Some("pi.ci.full_suite_gate.v1"),
        "Must have correct schema"
    );
    assert!(val.get("verdict").is_some(), "Must have verdict");
    assert!(
        val.get("gates").and_then(Value::as_array).is_some(),
        "Must have gates array"
    );
    assert!(val.get("summary").is_some(), "Must have summary");

    // Each gate must have required fields.
    if let Some(gates) = val.get("gates").and_then(Value::as_array) {
        for gate in gates {
            assert!(gate.get("id").is_some(), "Gate missing id");
            assert!(gate.get("bead").is_some(), "Gate missing bead");
            assert!(gate.get("status").is_some(), "Gate missing status");
            assert!(gate.get("blocking").is_some(), "Gate missing blocking");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane 1: Preflight fast-fail (bd-1f42.8.8.1)
// ═══════════════════════════════════════════════════════════════════════════

/// Preflight lane verdict artifact.
#[derive(Debug, serde::Serialize)]
struct PreflightVerdict {
    schema: String,
    lane: String,
    generated_at: String,
    verdict: String,
    policy: String,
    gates_evaluated: usize,
    first_failure: Option<SubGate>,
    blocking_gates: Vec<SubGate>,
    waivers_applied: Vec<String>,
    summary: PreflightSummary,
}

#[derive(Debug, serde::Serialize)]
struct PreflightSummary {
    blocking_pass: usize,
    blocking_fail: usize,
    blocking_skip: usize,
    blocking_waived: usize,
    blocking_total: usize,
    fail_fast_triggered: bool,
}

/// Lane 1: Preflight fast-fail gate.
///
/// Evaluates ONLY blocking gates. Stops at the first failure unless waived.
/// Produces a deterministic verdict artifact for early regression detection.
///
/// Run with:
/// `cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn preflight_fast_fail() {
    use chrono::{SecondsFormat, Utc};

    let root = repo_root();
    let report_dir = root.join("tests").join("full_suite_gate");
    let generate = full_suite_gate_artifact_generation_requested();
    if generate {
        prepare_report_dir_for_generation(&report_dir);
    }

    eprintln!("\n=== Preflight Fast-Fail Lane (bd-1f42.8.8.1) ===\n");

    let all_gates = collect_gates(&root);
    let (waivers, validations) = parse_waivers(&root);
    let waived = waived_gate_ids(&waivers, &validations, "preflight");

    let blocking_gates: Vec<SubGate> = all_gates.into_iter().filter(|g| g.blocking).collect();

    let mut evaluated = Vec::new();
    let mut first_failure: Option<SubGate> = None;
    let mut waivers_applied = Vec::new();
    let mut pass_count = 0_usize;
    let mut fail_count = 0_usize;
    let mut skip_count = 0_usize;
    let mut waived_count = 0_usize;

    for gate in &blocking_gates {
        if gate.status == "pass" {
            pass_count += 1;
            evaluated.push(gate.clone());
        } else if gate.status == "skip" {
            skip_count += 1;
            evaluated.push(gate.clone());
        } else if waived.contains_key(&gate.id) {
            waived_count += 1;
            waivers_applied.push(gate.id.clone());
            let mut waived_gate = gate.clone();
            waived_gate.detail = Some(format!(
                "WAIVED: {} (bead: {})",
                waived.get(&gate.id).map_or("", |w| &w.reason),
                waived.get(&gate.id).map_or("", |w| &w.bead),
            ));
            evaluated.push(waived_gate);
        } else {
            // Failure — record and stop
            fail_count += 1;
            evaluated.push(gate.clone());
            if first_failure.is_none() {
                first_failure = Some(gate.clone());
            }
            // Fast-fail: stop evaluating after first failure
            break;
        }
    }

    let fail_fast_triggered = first_failure.is_some();
    let verdict = if fail_count == 0 { "pass" } else { "fail" };

    for gate in &evaluated {
        let icon = match gate.status.as_str() {
            "pass" => "PASS",
            "fail" => "FAIL",
            _ => "SKIP",
        };
        let waived_tag = if waivers_applied.contains(&gate.id) {
            " [WAIVED]"
        } else {
            ""
        };
        eprintln!("  [{icon}] {}{waived_tag}", gate.name);
        if let Some(ref detail) = gate.detail {
            eprintln!("         {detail}");
        }
    }
    if fail_fast_triggered {
        eprintln!(
            "\n  FAST-FAIL: Stopped after first blocking failure ({})",
            first_failure.as_ref().map_or("-", |g| &g.name)
        );
    }
    eprintln!();

    let report = PreflightVerdict {
        schema: "pi.ci.preflight_lane.v1".to_string(),
        lane: "preflight".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        verdict: verdict.to_string(),
        policy: "Preflight lane: evaluate blocking gates only, fail-fast on first failure. \
                 Waived gates are skipped with audit trail."
            .to_string(),
        gates_evaluated: evaluated.len(),
        first_failure,
        blocking_gates: evaluated,
        waivers_applied,
        summary: PreflightSummary {
            blocking_pass: pass_count,
            blocking_fail: fail_count,
            blocking_skip: skip_count,
            blocking_waived: waived_count,
            blocking_total: blocking_gates.len(),
            fail_fast_triggered,
        },
    };

    let verdict_path = report_dir.join("preflight_verdict.json");
    let verdict_payload =
        serde_json::to_string_pretty(&report).expect("preflight verdict must serialize as JSON");
    assert!(
        !verdict_payload.trim().is_empty(),
        "preflight verdict payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &verdict_path,
            "tests/full_suite_gate/preflight_verdict.json",
            &verdict_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed preflight verdict emission: {detail}"));
    }

    eprintln!("=== Preflight Verdict: {} ===", verdict.to_uppercase());
    eprintln!(
        "  Blocking: {pass_count} pass, {fail_count} fail, {skip_count} skip, {waived_count} waived / {} total",
        blocking_gates.len()
    );
    if generate {
        eprintln!("  Report: {}", verdict_path.display());
    } else {
        eprintln!(
            "  Report validated in memory; set {GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV}=1 to write it"
        );
    }
    eprintln!();
}

// ═══════════════════════════════════════════════════════════════════════════
// Lane 2: Full certification (bd-1f42.8.8.1)
// ═══════════════════════════════════════════════════════════════════════════

/// Full certification verdict — includes all gates plus waiver audit.
#[derive(Debug, serde::Serialize)]
struct CertificationVerdict {
    schema: String,
    lane: String,
    generated_at: String,
    verdict: String,
    policy: String,
    gates: Vec<SubGate>,
    waiver_audit: WaiverAuditReport,
    waivers_applied: Vec<String>,
    summary: CertificationSummary,
    promotion_rules: PromotionRules,
    rerun_guidance: RerunGuidance,
}

#[derive(Debug, serde::Serialize)]
struct CertificationSummary {
    total_gates: usize,
    passed: usize,
    failed: usize,
    warned: usize,
    skipped: usize,
    waived: usize,
    blocking_pass: usize,
    blocking_total: usize,
    all_blocking_pass: bool,
}

#[derive(Debug, serde::Serialize)]
struct PromotionRules {
    can_promote: bool,
    blocker_gates: Vec<String>,
    waiver_gates: Vec<String>,
    conditions: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct RerunGuidance {
    preflight_command: String,
    full_command: String,
    single_gate_template: String,
}

/// Lane 2: Full certification gate.
///
/// Evaluates ALL gates (blocking + non-blocking), generates waiver audit,
/// and produces comprehensive verdict with promotion rules and rerun guidance.
///
/// Run with:
/// `cargo test --test ci_full_suite_gate -- full_certification --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn full_certification() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let root = repo_root();
    let report_dir = root.join("tests").join("full_suite_gate");
    let generate = full_suite_gate_artifact_generation_requested();
    if generate {
        prepare_report_dir_for_generation(&report_dir);
    }
    let perf3x_coverage_audit_report =
        build_perf3x_bead_coverage_audit_report(&root, &perf3x_bead_coverage_contract());
    let perf3x_coverage_audit_path = report_dir.join("perf3x_bead_coverage_audit.json");
    let perf3x_payload = serde_json::to_string_pretty(&perf3x_coverage_audit_report)
        .expect("PERF-3X coverage audit must serialize as JSON");
    assert!(
        !perf3x_payload.trim().is_empty(),
        "PERF-3X coverage audit payload must be non-empty"
    );
    let practical_finish_checkpoint_report = build_practical_finish_checkpoint_report(&root);
    let practical_finish_checkpoint_path = report_dir.join("practical_finish_checkpoint.json");
    let practical_finish_payload =
        serde_json::to_string_pretty(&practical_finish_checkpoint_report)
            .expect("practical-finish checkpoint must serialize as JSON");
    assert!(
        !practical_finish_payload.trim().is_empty(),
        "practical-finish checkpoint payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &perf3x_coverage_audit_path,
            PERF3X_COVERAGE_AUDIT_ARTIFACT_REL,
            &perf3x_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed perf3x coverage audit emission: {detail}"));
        write_non_empty_artifact(
            &practical_finish_checkpoint_path,
            PRACTICAL_FINISH_CHECKPOINT_ARTIFACT_REL,
            &practical_finish_payload,
        )
        .unwrap_or_else(|detail| {
            panic!("fail-closed practical-finish checkpoint emission: {detail}")
        });
    }

    eprintln!("\n=== Full Certification Lane (bd-1f42.8.8.1) ===\n");

    let gates = collect_gates(&root);
    let (waivers, validations) = parse_waivers(&root);
    let waived = waived_gate_ids(&waivers, &validations, "full");

    // Build waiver audit
    let active = validations.iter().filter(|v| v.status == "active").count();
    let expired = validations.iter().filter(|v| v.status == "expired").count();
    let expiring_soon = validations
        .iter()
        .filter(|v| v.status == "expiring_soon")
        .count();
    let invalid = validations.iter().filter(|v| v.status == "invalid").count();

    let waiver_audit = WaiverAuditReport {
        schema: "pi.ci.waiver_audit.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        total_waivers: waivers.len(),
        active,
        expired,
        expiring_soon,
        invalid,
        waivers: validations.clone(),
        raw_waivers: waivers.clone(),
    };

    // Write standalone waiver audit
    let waiver_path = report_dir.join("waiver_audit.json");
    let waiver_payload =
        serde_json::to_string_pretty(&waiver_audit).expect("waiver audit must serialize as JSON");
    assert!(
        !waiver_payload.trim().is_empty(),
        "waiver audit payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &waiver_path,
            "tests/full_suite_gate/waiver_audit.json",
            &waiver_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed waiver audit emission: {detail}"));
    }

    // Evaluate gates with waiver application
    let mut waivers_applied = Vec::new();
    let mut effective_gates = Vec::new();
    for gate in &gates {
        if (gate.status == "fail" || gate.status == "warn") && waived.contains_key(&gate.id) {
            waivers_applied.push(gate.id.clone());
            let mut waived_gate = gate.clone();
            waived_gate.detail = Some(format!(
                "WAIVED (original: {}): {} (bead: {})",
                gate.status,
                waived.get(&gate.id).map_or("", |w| &w.reason),
                waived.get(&gate.id).map_or("", |w| &w.bead),
            ));
            effective_gates.push(waived_gate);
        } else {
            effective_gates.push(gate.clone());
        }
    }

    // Print gate results
    for gate in &effective_gates {
        let icon = match gate.status.as_str() {
            "pass" => "PASS",
            "fail" => "FAIL",
            "warn" => "WARN",
            _ => "SKIP",
        };
        let blocking = if gate.blocking { "(blocking)" } else { "" };
        let waived_tag = if waivers_applied.contains(&gate.id) {
            " [WAIVED]"
        } else {
            ""
        };
        eprintln!("  [{icon}] {:<50} {:<12}{waived_tag}", gate.name, blocking);
        if let Some(ref detail) = gate.detail {
            eprintln!("         {detail}");
        }
    }
    eprintln!();

    // Compute summary
    let passed = gates.iter().filter(|g| g.status == "pass").count();
    let failed = gates
        .iter()
        .filter(|g| g.status == "fail" && !waivers_applied.contains(&g.id))
        .count();
    let warned = gates
        .iter()
        .filter(|g| g.status == "warn" && !waivers_applied.contains(&g.id))
        .count();
    let skipped = gates.iter().filter(|g| g.status == "skip").count();
    let waived_count = waivers_applied.len();

    let blocking_gates: Vec<&SubGate> = gates.iter().filter(|g| g.blocking).collect();
    let blocking_pass = blocking_gates
        .iter()
        .filter(|g| g.status == "pass" || waivers_applied.contains(&g.id))
        .count();
    let blocking_total = blocking_gates.len();
    let all_blocking_pass = blocking_pass == blocking_total;

    let blocker_ids: Vec<String> = blocking_gates
        .iter()
        .filter(|g| g.status != "pass" && g.status != "skip" && !waivers_applied.contains(&g.id))
        .map(|g| g.id.clone())
        .collect();

    let verdict = if all_blocking_pass && failed == 0 {
        "pass"
    } else if all_blocking_pass {
        "warn"
    } else {
        "fail"
    };

    let mut conditions = Vec::new();
    if all_blocking_pass {
        conditions.push("All blocking gates pass (including waivers)".to_string());
    } else {
        conditions.push(format!(
            "Blocking gates still failing: {}",
            blocker_ids.join(", ")
        ));
    }
    if !waivers_applied.is_empty() {
        conditions.push(format!(
            "Waivers active for: {} (review before release)",
            waivers_applied.join(", ")
        ));
    }
    if expired > 0 {
        conditions.push(format!(
            "{expired} expired waiver(s) must be renewed or fixed"
        ));
    }

    let report = CertificationVerdict {
        schema: "pi.ci.certification_lane.v1".to_string(),
        lane: "full".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        verdict: verdict.to_string(),
        policy: "Full certification: all blocking gates must pass for release. \
                 Waived gates are tracked but do not block. Expired waivers fail the \
                 waiver_lifecycle gate."
            .to_string(),
        gates: effective_gates.clone(),
        waiver_audit,
        waivers_applied: waivers_applied.clone(),
        summary: CertificationSummary {
            total_gates: gates.len(),
            passed,
            failed,
            warned,
            skipped,
            waived: waived_count,
            blocking_pass,
            blocking_total,
            all_blocking_pass,
        },
        promotion_rules: PromotionRules {
            can_promote: all_blocking_pass && expired == 0 && invalid == 0,
            blocker_gates: blocker_ids,
            waiver_gates: waivers_applied.clone(),
            conditions,
        },
        rerun_guidance: RerunGuidance {
            preflight_command:
                "cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact"
                    .to_string(),
            full_command:
                "cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact"
                    .to_string(),
            single_gate_template: "See reproduce_command field on each gate".to_string(),
        },
    };

    // Write certification verdict
    let cert_path = report_dir.join("certification_verdict.json");
    let cert_payload = serde_json::to_string_pretty(&report)
        .expect("certification verdict must serialize as JSON");
    assert!(
        !cert_payload.trim().is_empty(),
        "certification verdict payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &cert_path,
            "tests/full_suite_gate/certification_verdict.json",
            &cert_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed certification verdict emission: {detail}"));
    }

    // Write JSONL events for certification
    let cert_events_path = report_dir.join("certification_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for gate in &effective_gates {
        let line = serde_json::json!({
            "schema": "pi.ci.certification_event.v1",
            "lane": "full",
            "gate_id": gate.id,
            "gate_name": gate.name,
            "bead": gate.bead,
            "status": gate.status,
            "blocking": gate.blocking,
            "waived": waivers_applied.contains(&gate.id),
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(
            serde_json::to_string(&line).unwrap_or_else(|err| {
                panic!("fail-closed certification events serialization: {err}")
            }),
        );
    }
    let cert_events_payload = lines.join("\n") + "\n";
    assert!(
        !cert_events_payload.trim().is_empty(),
        "certification events payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &cert_events_path,
            "tests/full_suite_gate/certification_events.jsonl",
            &cert_events_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed certification events emission: {detail}"));
    }

    // Write certification markdown report
    let mut md = String::new();
    md.push_str("# Full Certification Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = writeln!(md, "> Lane: **full**");
    let _ = writeln!(md, "> Verdict: **{}**\n", verdict.to_uppercase());

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total gates | {} |", gates.len());
    let _ = writeln!(md, "| Passed | {passed} |");
    let _ = writeln!(md, "| Failed | {failed} |");
    let _ = writeln!(md, "| Warned | {warned} |");
    let _ = writeln!(md, "| Skipped | {skipped} |");
    let _ = writeln!(md, "| Waived | {waived_count} |");
    let _ = writeln!(md, "| Blocking | {blocking_pass}/{blocking_total} |");
    let _ = writeln!(
        md,
        "| Can promote | {} |",
        if all_blocking_pass && expired == 0 && invalid == 0 {
            "YES"
        } else {
            "NO"
        }
    );
    md.push('\n');

    if !waivers.is_empty() {
        md.push_str("## Active Waivers\n\n");
        md.push_str("| Gate | Owner | Bead | Expires | Status | Removal Condition |\n");
        md.push_str("|------|-------|------|---------|--------|-------------------|\n");
        for (w, v) in waivers.iter().zip(validations.iter()) {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {} |",
                w.gate_id, w.owner, w.bead, w.expires, v.status, w.remove_when
            );
        }
        md.push('\n');
    }

    md.push_str("## Gate Results\n\n");
    md.push_str(
        "| Gate | Bead | Blocking | Status | Waived | Artifact |\n\
         |------|------|----------|--------|--------|----------|\n",
    );
    for gate in &effective_gates {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | `{}` |",
            gate.name,
            gate.bead,
            if gate.blocking { "YES" } else { "no" },
            gate.status.to_uppercase(),
            if waivers_applied.contains(&gate.id) {
                "YES"
            } else {
                "-"
            },
            gate.artifact_path.as_deref().unwrap_or("-"),
        );
    }
    md.push('\n');

    md.push_str("## Rerun Commands\n\n");
    md.push_str("| Lane | Command |\n|------|--------|\n");
    let _ = writeln!(
        md,
        "| Preflight | `cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact` |"
    );
    let _ = writeln!(
        md,
        "| Full | `cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact` |"
    );
    md.push('\n');

    let cert_md_path = report_dir.join("certification_report.md");
    assert!(
        !md.trim().is_empty(),
        "certification markdown report must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &cert_md_path,
            "tests/full_suite_gate/certification_report.md",
            &md,
        )
        .unwrap_or_else(|detail| panic!("fail-closed certification report emission: {detail}"));
        assert_non_empty_text_artifact(
            &cert_md_path,
            "tests/full_suite_gate/certification_report.md",
        )
        .unwrap_or_else(|detail| panic!("fail-closed certification report verification: {detail}"));
    }

    // Print summary
    eprintln!("=== Certification Verdict: {} ===", verdict.to_uppercase());
    eprintln!("  Gates:    {passed}/{} passed", gates.len());
    eprintln!("  Blocking: {blocking_pass}/{blocking_total}");
    eprintln!("  Waived:   {waived_count}");
    if failed > 0 {
        eprintln!("  Failed:   {failed}");
    }
    if expired > 0 {
        eprintln!("  Expired waivers: {expired}");
    }
    eprintln!();
    if generate {
        eprintln!("  Reports:");
        eprintln!("    Cert:    {}", cert_path.display());
        eprintln!("    Waiver:  {}", waiver_path.display());
        eprintln!("    Events:  {}", cert_events_path.display());
        eprintln!("    Coverage: {}", perf3x_coverage_audit_path.display());
        eprintln!(
            "    Practical Finish: {}",
            practical_finish_checkpoint_path.display()
        );
        eprintln!("    MD:      {}", cert_md_path.display());
    } else {
        eprintln!(
            "  Reports validated in memory; set {GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV}=1 to write tracked artifacts"
        );
    }
    eprintln!();
}

// ═══════════════════════════════════════════════════════════════════════════
// Waiver lifecycle audit (bd-1f42.8.8.1)
// ═══════════════════════════════════════════════════════════════════════════

/// Standalone waiver lifecycle audit.
///
/// Validates all waivers, produces audit report, and fails on expired/invalid waivers.
///
/// Run with:
/// `cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture`
#[test]
fn waiver_lifecycle_audit() {
    use chrono::{SecondsFormat, Utc};

    let root = repo_root();
    let report_dir = root.join("tests").join("full_suite_gate");
    let generate = full_suite_gate_artifact_generation_requested();
    if generate {
        prepare_report_dir_for_generation(&report_dir);
    }

    eprintln!("\n=== Waiver Lifecycle Audit (bd-1f42.8.8.1) ===\n");

    let (waivers, validations) = parse_waivers(&root);

    let active = validations.iter().filter(|v| v.status == "active").count();
    let expired = validations.iter().filter(|v| v.status == "expired").count();
    let expiring_soon = validations
        .iter()
        .filter(|v| v.status == "expiring_soon")
        .count();
    let invalid = validations.iter().filter(|v| v.status == "invalid").count();

    for (w, v) in waivers.iter().zip(validations.iter()) {
        let icon = match v.status.as_str() {
            "active" => "OK",
            "expiring_soon" => "WARN",
            "expired" => "EXPIRED",
            _ => "INVALID",
        };
        eprintln!(
            "  [{icon}] {} — owner: {}, bead: {}, expires: {}",
            w.gate_id, w.owner, w.bead, w.expires
        );
        if let Some(ref detail) = v.detail {
            eprintln!("       {detail}");
        }
    }

    if waivers.is_empty() {
        eprintln!("  No waivers defined. Gate passes.");
    }
    eprintln!();

    let report = WaiverAuditReport {
        schema: "pi.ci.waiver_audit.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        total_waivers: waivers.len(),
        active,
        expired,
        expiring_soon,
        invalid,
        waivers: validations,
        raw_waivers: waivers,
    };

    let waiver_path = report_dir.join("waiver_audit.json");
    let waiver_payload = serde_json::to_string_pretty(&report)
        .expect("waiver lifecycle audit must serialize as JSON");
    assert!(
        !waiver_payload.trim().is_empty(),
        "waiver lifecycle audit payload must be non-empty"
    );
    if generate {
        write_non_empty_artifact(
            &waiver_path,
            "tests/full_suite_gate/waiver_audit.json",
            &waiver_payload,
        )
        .unwrap_or_else(|detail| panic!("fail-closed waiver audit emission: {detail}"));
    }

    if generate {
        eprintln!("  Report: {}", waiver_path.display());
    } else {
        eprintln!(
            "  Report validated in memory; set {GENERATE_FULL_SUITE_GATE_ARTIFACTS_ENV}=1 to write it"
        );
    }
    eprintln!(
        "  Summary: {} total, {} active, {} expiring_soon, {} expired, {} invalid",
        report.total_waivers, active, expiring_soon, expired, invalid
    );
    eprintln!();

    // Fail on expired or invalid waivers
    assert_eq!(
        expired, 0,
        "Expired waivers must be renewed or the underlying issue fixed"
    );
    assert_eq!(
        invalid, 0,
        "Invalid waivers must have all required fields and valid dates"
    );
}

#[test]
fn full_suite_gate_artifact_generation_requires_exact_one() {
    assert!(full_suite_gate_artifact_generation_requested_from(Some(
        "1"
    )));
    for raw in [None, Some(""), Some("0"), Some("true"), Some(" 1 ")] {
        assert!(
            !full_suite_gate_artifact_generation_requested_from(raw),
            "unexpected generation request for {raw:?}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Waiver schema validation tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn waiver_date_validation_active() {
    let waiver = Waiver {
        gate_id: "test_gate".to_string(),
        owner: "TestOwner".to_string(),
        created: "2026-02-01".to_string(),
        expires: "2026-02-28".to_string(),
        bead: "bd-test".to_string(),
        reason: "test".to_string(),
        scope: "both".to_string(),
        remove_when: "never".to_string(),
    };
    let v = validate_waiver_dates(&waiver, "2026-02-13");
    assert!(
        v.status == "active" || v.status == "expiring_soon",
        "Should be active or expiring_soon, got: {}",
        v.status
    );
    assert!(
        v.days_remaining.unwrap_or(-1) >= 0,
        "Should have positive days remaining"
    );
}

#[test]
fn waiver_date_validation_expired() {
    let waiver = Waiver {
        gate_id: "test_gate".to_string(),
        owner: "TestOwner".to_string(),
        created: "2026-01-01".to_string(),
        expires: "2026-01-15".to_string(),
        bead: "bd-test".to_string(),
        reason: "test".to_string(),
        scope: "both".to_string(),
        remove_when: "never".to_string(),
    };
    let v = validate_waiver_dates(&waiver, "2026-02-13");
    assert_eq!(v.status, "expired", "Should be expired");
    assert!(
        v.days_remaining.unwrap_or(0) < 0,
        "Should have negative days remaining"
    );
}

#[test]
fn waiver_date_validation_too_long_duration() {
    let waiver = Waiver {
        gate_id: "test_gate".to_string(),
        owner: "TestOwner".to_string(),
        created: "2026-02-01".to_string(),
        expires: "2026-04-01".to_string(), // 59 days
        bead: "bd-test".to_string(),
        reason: "test".to_string(),
        scope: "both".to_string(),
        remove_when: "never".to_string(),
    };
    let v = validate_waiver_dates(&waiver, "2026-02-13");
    assert_eq!(
        v.status, "invalid",
        "Should be invalid due to excessive duration"
    );
}

#[test]
fn waiver_date_validation_expiring_soon() {
    let waiver = Waiver {
        gate_id: "test_gate".to_string(),
        owner: "TestOwner".to_string(),
        created: "2026-02-10".to_string(),
        expires: "2026-02-15".to_string(),
        bead: "bd-test".to_string(),
        reason: "test".to_string(),
        scope: "both".to_string(),
        remove_when: "never".to_string(),
    };
    let v = validate_waiver_dates(&waiver, "2026-02-13");
    assert_eq!(
        v.status, "expiring_soon",
        "Should be expiring_soon (2 days left)"
    );
    assert_eq!(v.days_remaining, Some(2));
}

#[test]
fn waiver_scope_filtering() {
    let waivers = vec![
        Waiver {
            gate_id: "gate_a".to_string(),
            owner: "A".to_string(),
            created: "2026-02-01".to_string(),
            expires: "2026-02-28".to_string(),
            bead: "bd-a".to_string(),
            reason: "test".to_string(),
            scope: "preflight".to_string(),
            remove_when: "never".to_string(),
        },
        Waiver {
            gate_id: "gate_b".to_string(),
            owner: "B".to_string(),
            created: "2026-02-01".to_string(),
            expires: "2026-02-28".to_string(),
            bead: "bd-b".to_string(),
            reason: "test".to_string(),
            scope: "full".to_string(),
            remove_when: "never".to_string(),
        },
        Waiver {
            gate_id: "gate_c".to_string(),
            owner: "C".to_string(),
            created: "2026-02-01".to_string(),
            expires: "2026-02-28".to_string(),
            bead: "bd-c".to_string(),
            reason: "test".to_string(),
            scope: "both".to_string(),
            remove_when: "never".to_string(),
        },
    ];
    let validations = vec![
        WaiverValidation {
            gate_id: "gate_a".to_string(),
            status: "active".to_string(),
            detail: None,
            days_remaining: Some(15),
        },
        WaiverValidation {
            gate_id: "gate_b".to_string(),
            status: "active".to_string(),
            detail: None,
            days_remaining: Some(15),
        },
        WaiverValidation {
            gate_id: "gate_c".to_string(),
            status: "active".to_string(),
            detail: None,
            days_remaining: Some(15),
        },
    ];

    let preflight = waived_gate_ids(&waivers, &validations, "preflight");
    assert!(
        preflight.contains_key("gate_a"),
        "gate_a scoped to preflight"
    );
    assert!(
        !preflight.contains_key("gate_b"),
        "gate_b scoped to full only"
    );
    assert!(preflight.contains_key("gate_c"), "gate_c scoped to both");

    let full = waived_gate_ids(&waivers, &validations, "full");
    assert!(!full.contains_key("gate_a"), "gate_a not in full scope");
    assert!(full.contains_key("gate_b"), "gate_b scoped to full");
    assert!(full.contains_key("gate_c"), "gate_c scoped to both");
}

#[test]
fn waiver_expired_not_applied() {
    let waivers = vec![Waiver {
        gate_id: "gate_x".to_string(),
        owner: "X".to_string(),
        created: "2026-01-01".to_string(),
        expires: "2026-01-15".to_string(),
        bead: "bd-x".to_string(),
        reason: "expired test".to_string(),
        scope: "both".to_string(),
        remove_when: "never".to_string(),
    }];
    let validations = vec![WaiverValidation {
        gate_id: "gate_x".to_string(),
        status: "expired".to_string(),
        detail: Some("Expired".to_string()),
        days_remaining: Some(-29),
    }];

    let result = waived_gate_ids(&waivers, &validations, "both");
    assert!(result.is_empty(), "Expired waivers should not be applied");
}

#[test]
fn parse_waivers_empty_is_ok() {
    // When no waiver sections exist, should return empty with no errors
    let (waivers, validations) = parse_waivers(&repo_root());
    // Currently no waivers defined, so both should be empty
    eprintln!(
        "  Parsed {} waivers, {} validations",
        waivers.len(),
        validations.len()
    );
    // This test just verifies parsing doesn't panic
}

#[test]
fn perf3x_bead_coverage_contract_is_well_formed() {
    let contract = perf3x_bead_coverage_contract();
    let rows = validate_perf3x_bead_coverage_contract(&contract).expect("contract should be valid");
    assert!(
        rows.len() >= 6,
        "expected at least 6 PERF-3X rows, got {}",
        rows.len()
    );
    let bead_ids: HashSet<&str> = rows.iter().map(|row| row.bead.as_str()).collect();
    for bead_id in PERF3X_CRITICAL_BEADS {
        assert!(
            bead_ids.contains(*bead_id),
            "contract should include critical PERF-3X bead: {bead_id}"
        );
    }
    for row in rows {
        assert!(
            !row.unit_evidence.is_empty(),
            "unit evidence must be present"
        );
        assert!(!row.e2e_evidence.is_empty(), "e2e evidence must be present");
        assert!(!row.log_evidence.is_empty(), "log evidence must be present");
    }
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_missing_e2e_array() {
    let mut contract = perf3x_bead_coverage_contract();
    let first = contract
        .get_mut("coverage_rows")
        .and_then(Value::as_array_mut)
        .and_then(|rows| rows.first_mut())
        .and_then(Value::as_object_mut)
        .expect("first coverage row should exist");
    first.remove("e2e_evidence");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("missing e2e_evidence must fail closed");
    assert!(
        err.contains("e2e_evidence"),
        "error should mention missing e2e_evidence, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_empty_log_array() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["log_evidence"] = serde_json::json!([]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("empty log_evidence must fail closed");
    assert!(
        err.contains("log_evidence"),
        "error should mention log_evidence, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_missing_critical_bead() {
    let mut contract = perf3x_bead_coverage_contract();
    let rows = contract
        .get_mut("coverage_rows")
        .and_then(Value::as_array_mut)
        .expect("coverage_rows must exist");
    rows.retain(|row| row.get("bead").and_then(Value::as_str) != Some("bd-3ar8v.3.8"));

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("missing critical PERF-3X bead must fail closed");
    assert!(
        err.contains("missing critical PERF-3X bead"),
        "error should mention missing critical bead set, got: {err}"
    );
    assert!(
        err.contains("bd-3ar8v.3.8"),
        "error should identify missing bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_misordered_rows() {
    let mut contract = perf3x_bead_coverage_contract();
    let rows = contract
        .get_mut("coverage_rows")
        .and_then(Value::as_array_mut)
        .expect("coverage_rows must exist");
    rows.swap(0, 1);

    let err =
        validate_perf3x_bead_coverage_contract(&contract).expect_err("misordered rows must fail");
    assert!(
        err.contains("must be sorted by canonical bead id order"),
        "error should mention ordering requirement, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_unsorted_unit_evidence_paths() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] =
        serde_json::json!(["tests/z_unit.rs", "tests/a_unit.rs"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("unsorted unit evidence paths must fail closed");
    assert!(
        err.contains("unit_evidence"),
        "error should mention affected evidence field, got: {err}"
    );
    assert!(
        err.contains("must be sorted by normalized path order"),
        "error should mention deterministic normalized ordering requirement, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_unsorted_e2e_evidence_paths() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["e2e_evidence"] =
        serde_json::json!(["tests/z_e2e.json", "tests/a_e2e.json"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("unsorted e2e evidence paths must fail closed");
    assert!(
        err.contains("e2e_evidence"),
        "error should mention affected evidence field, got: {err}"
    );
    assert!(
        err.contains("must be sorted by normalized path order"),
        "error should mention deterministic normalized ordering requirement, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_unsorted_log_evidence_paths() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["log_evidence"] = serde_json::json!([
        "tests/full_suite_gate/z_events.jsonl",
        "tests/full_suite_gate/a_events.jsonl"
    ]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("unsorted log evidence paths must fail closed");
    assert!(
        err.contains("log_evidence"),
        "error should mention affected evidence field, got: {err}"
    );
    assert!(
        err.contains("must be sorted by normalized path order"),
        "error should mention deterministic normalized ordering requirement, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_non_numeric_bead_segment() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["bead"] = serde_json::json!("bd-3ar8v.4.alpha");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("non-numeric bead segment must fail closed");
    assert!(
        err.contains("invalid PERF-3X bead id"),
        "error should mention invalid bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_empty_bead_segment() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["bead"] = serde_json::json!("bd-3ar8v.4..10");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("empty bead segment must fail closed");
    assert!(
        err.contains("invalid PERF-3X bead id"),
        "error should mention invalid bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_missing_bead_suffix() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["bead"] = serde_json::json!("bd-3ar8v");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("missing bead suffix must fail closed");
    assert!(
        err.contains("invalid PERF-3X bead id"),
        "error should mention invalid bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_zero_padded_bead_segment() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["bead"] = serde_json::json!("bd-3ar8v.02.8");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("zero-padded bead segment must fail closed");
    assert!(
        err.contains("must be canonical"),
        "error should mention canonical bead id requirement, got: {err}"
    );
    assert!(
        err.contains("bd-3ar8v.2.8"),
        "error should include canonical equivalent bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_numeric_equivalent_bead_ids() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][1]["bead"] = serde_json::json!("bd-3ar8v.02.8");

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("numerically equivalent bead IDs must fail closed");
    assert!(
        err.contains("numerically equivalent"),
        "error should mention numeric-equivalence rejection, got: {err}"
    );
    assert!(
        err.contains("bd-3ar8v.2.8"),
        "error should mention canonical bead id, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_duplicate_unit_evidence_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] =
        serde_json::json!(["tests/bench_schema.rs", "tests/bench_schema.rs"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("duplicate unit evidence path must fail closed");
    assert!(
        err.contains("duplicate path"),
        "error should mention duplicate path, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_cross_category_duplicate_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] = serde_json::json!(["tests/bench_schema.rs"]);
    contract["coverage_rows"][0]["e2e_evidence"] = serde_json::json!(["tests/bench_schema.rs"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("cross-category duplicate path must fail closed");
    assert!(
        err.contains("reuses evidence path across categories"),
        "error should mention cross-category reuse, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_cross_category_duplicate_after_normalization() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["e2e_evidence"] = serde_json::json!(["./tests/e2e_results"]);
    contract["coverage_rows"][0]["log_evidence"] = serde_json::json!(["tests/e2e_results"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("normalized cross-category duplicate path must fail closed");
    assert!(
        err.contains("reuses evidence path across categories"),
        "error should mention cross-category reuse, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_parent_traversal_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["log_evidence"] = serde_json::json!([
        "tests/full_suite_gate/certification_events.jsonl",
        "../escape.json"
    ]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("parent traversal path must fail closed");
    assert!(
        err.contains("parent traversal"),
        "error should mention parent traversal, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_absolute_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["e2e_evidence"] =
        serde_json::json!(["tests/e2e_results", "/tmp/outside_repo.json"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("absolute path must fail closed");
    assert!(
        err.contains("absolute path"),
        "error should mention absolute path, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_normalized_duplicate_dot_slash_variant() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] =
        serde_json::json!(["./tests/bench_schema.rs", "tests/bench_schema.rs"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("normalized duplicate evidence path must fail closed");
    assert!(
        err.contains("duplicate path"),
        "error should mention duplicate path, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_normalized_duplicate_backslash_variant() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] =
        serde_json::json!(["tests\\bench_schema.rs", "tests/bench_schema.rs"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("normalized duplicate backslash variant must fail closed");
    assert!(
        err.contains("duplicate path"),
        "error should mention duplicate path, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_normalizes_relative_paths_before_storage() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["unit_evidence"] = serde_json::json!(["./tests//bench_schema.rs"]);

    let parsed = validate_perf3x_bead_coverage_contract(&contract)
        .expect("normalized relative path should pass");
    assert_eq!(
        parsed[0].unit_evidence[0], "tests/bench_schema.rs",
        "path should be normalized before storage"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_windows_absolute_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["e2e_evidence"] =
        serde_json::json!(["tests/e2e_results", "C:\\temp\\outside.json"]);

    let err = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("windows absolute path must fail closed");
    assert!(
        err.contains("windows-absolute"),
        "error should mention windows-absolute path, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_windows_drive_relative_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["e2e_evidence"] =
        serde_json::json!(["tests/e2e_results", "C:temp/outside.json"]);

    let error = validate_perf3x_bead_coverage_contract(&contract)
        .expect_err("windows drive-relative path must fail closed");
    assert!(
        error.contains("windows-absolute"),
        "error should mention the rejected Windows drive prefix: {error}"
    );
}

#[test]
fn perf3x_bead_coverage_contract_fails_closed_on_unc_path() {
    let mut contract = perf3x_bead_coverage_contract();
    contract["coverage_rows"][0]["log_evidence"] = serde_json::json!([
        "tests/full_suite_gate/certification_events.jsonl",
        "\\\\server\\share\\outside.json"
    ]);

    let err =
        validate_perf3x_bead_coverage_contract(&contract).expect_err("UNC path must fail closed");
    assert!(
        err.contains("UNC paths"),
        "error should mention UNC path rejection, got: {err}"
    );
}

#[test]
fn perf3x_bead_coverage_evaluator_fails_closed_when_evidence_paths_are_missing() {
    let mut temp = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos();
    temp.push(format!("pi_agent_rust_per3x_coverage_warn_{nonce}"));
    std::fs::create_dir_all(&temp).expect("create temp root");

    let (status, detail) = evaluate_perf3x_bead_coverage(&temp, &perf3x_bead_coverage_contract());
    assert_eq!(status, "fail", "missing paths should fail closed");
    let detail = detail.unwrap_or_default();
    assert!(
        detail.contains("missing"),
        "failure detail should mention missing paths: {detail}"
    );
    assert!(
        detail.contains("fail-closed"),
        "failure detail should indicate fail-closed policy: {detail}"
    );

    let _ = std::fs::remove_dir_all(&temp);
}

#[cfg(unix)]
#[test]
fn perf3x_bead_coverage_evaluator_rejects_symlink_evidence() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("create coverage root");
    let outside = tempfile::tempdir().expect("create outside coverage root");
    std::fs::create_dir_all(root.path().join("tests")).expect("create coverage tests directory");
    let outside_file = outside.path().join("bench_schema.rs");
    std::fs::write(&outside_file, "external evidence\n").expect("write outside evidence");
    symlink(&outside_file, root.path().join("tests/bench_schema.rs"))
        .expect("create evidence symlink fixture");

    let evaluation =
        evaluate_perf3x_bead_coverage_internal(root.path(), &perf3x_bead_coverage_contract());
    assert_eq!(evaluation.status, "fail");
    let symlink_failure = evaluation
        .missing_evidence
        .iter()
        .find(|entry| entry.contains("tests/bench_schema.rs"))
        .expect("symlink evidence must be included in fail-closed diagnostics");
    assert!(
        symlink_failure.contains("symbolic link"),
        "{symlink_failure}"
    );
}

#[test]
fn perf3x_bead_coverage_sub_gate_is_blocking() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "perf3x_bead_coverage")
        .expect("perf3x_bead_coverage gate should exist");
    assert!(
        gate.blocking,
        "PERF-3X bead coverage gate must be release-blocking"
    );
}

#[test]
fn perf3x_bead_coverage_evaluator_fails_on_malformed_contract() {
    let malformed = serde_json::json!({
        "schema": "pi.perf3x.bead_coverage.v1",
        "coverage_rows": [
            {
                "bead": "bd-3ar8v.2.8",
                "unit_evidence": [],
                "e2e_evidence": ["tests/e2e_results"],
                "log_evidence": ["tests/full_suite_gate/full_suite_events.jsonl"]
            }
        ]
    });

    let (status, detail) = evaluate_perf3x_bead_coverage(&repo_root(), &malformed);
    assert_eq!(status, "fail");
    let detail = detail.unwrap_or_default();
    assert!(
        detail.contains("Invalid PERF-3X coverage contract"),
        "expected malformed-contract failure, got: {detail}"
    );
}

#[test]
fn perf3x_bead_coverage_sub_gate_points_to_dedicated_audit_artifact() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "perf3x_bead_coverage")
        .expect("perf3x_bead_coverage gate should exist");
    assert_eq!(
        gate.artifact_path.as_deref(),
        Some(PERF3X_COVERAGE_AUDIT_ARTIFACT_REL),
        "coverage gate should point at dedicated audit artifact path"
    );
}

#[test]
fn perf3x_bead_coverage_audit_report_includes_contract_schema_and_rows() {
    let report =
        build_perf3x_bead_coverage_audit_report(&repo_root(), &perf3x_bead_coverage_contract());
    assert_eq!(report.schema, "pi.perf3x.bead_coverage.audit.v1");
    assert_eq!(report.contract_schema, "pi.perf3x.bead_coverage.v1");
    assert_eq!(
        report.row_count,
        PERF3X_CRITICAL_BEADS.len(),
        "audit report row count should track current critical PERF-3X contract entries"
    );
    assert_eq!(
        report.rows.len(),
        report.row_count,
        "row_count should match serialized rows"
    );
}

#[test]
fn perf3x_bead_coverage_audit_report_tracks_missing_evidence_fail_closed() {
    let mut temp = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos();
    temp.push(format!(
        "pi_agent_rust_perf3x_coverage_audit_missing_{nonce}"
    ));
    std::fs::create_dir_all(&temp).expect("create temp root");

    let report = build_perf3x_bead_coverage_audit_report(&temp, &perf3x_bead_coverage_contract());
    assert_eq!(
        report.status, "fail",
        "missing evidence paths must fail the audit report"
    );
    assert!(
        report.missing_evidence_count > 0,
        "audit should surface missing evidence entries"
    );
    assert_eq!(
        report.missing_evidence.len(),
        report.missing_evidence_count,
        "missing evidence count should match missing evidence payload length"
    );
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("fail-closed"),
        "detail should mention fail-closed policy: {detail}"
    );

    let _ = std::fs::remove_dir_all(&temp);
}

fn write_practical_finish_issue_fixture(root: &Path, entries: &[Value]) {
    write_complete_practical_finish_issue_fixture_to(
        root,
        PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        entries,
    );
    commit_practical_finish_issue_fixture(root, PRACTICAL_FINISH_LIVE_ISSUE_SOURCE);
}

fn write_complete_practical_finish_issue_fixture_to(
    root: &Path,
    relative: &str,
    entries: &[Value],
) {
    let mut complete_entries = PRACTICAL_FINISH_REQUIRED_LEDGER_ANCHORS
        .iter()
        .filter(|anchor| {
            !entries
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(**anchor))
        })
        .map(|anchor| {
            serde_json::json!({
                "id": anchor,
                "status": "closed",
                "issue_type": "epic",
                "title": "Canonical PERF-3X ledger anchor",
                "labels": ["perf-3x"]
            })
        })
        .collect::<Vec<_>>();
    complete_entries.extend(entries.iter().cloned());
    write_practical_finish_issue_fixture_to(root, relative, &complete_entries);
}

fn write_practical_finish_issue_fixture_to(root: &Path, relative: &str, entries: &[Value]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create practical-finish source directory");
    }
    let mut lines = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).expect("serialize fixture entry"))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    std::fs::write(path, lines.join("\n") + "\n").expect("write issues fixture");
}

fn run_practical_finish_fixture_git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to execute fixture git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "fixture git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_practical_finish_issue_fixture(root: &Path, relative: &str) {
    run_practical_finish_fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
    run_practical_finish_fixture_git(root, &["add", "--", relative]);
    run_practical_finish_fixture_git(
        root,
        &[
            "-c",
            "user.name=Pi Gate Fixture",
            "-c",
            "user.email=pi-gate-fixture@example.invalid",
            "commit",
            "--quiet",
            "--message",
            "Add authoritative Beads fixture",
        ],
    );
}

#[test]
fn practical_finish_classifier_requires_explicit_docs_issue_type() {
    let docs_issue = PracticalFinishOpenIssue {
        id: "bd-3ar8v.6.5".to_string(),
        title: "Final report artifact".to_string(),
        status: "open".to_string(),
        issue_type: "docs".to_string(),
        labels: vec!["report".to_string()],
    };
    assert!(
        is_docs_only_issue(&docs_issue),
        "the explicit docs issue type should classify as docs-only"
    );

    let mislabeled_technical_issue = PracticalFinishOpenIssue {
        id: "bd-3ar8v.6.2".to_string(),
        title: "Harden permanent gates and write their runbook".to_string(),
        status: "open".to_string(),
        issue_type: "task".to_string(),
        labels: vec![
            "docs-last".to_string(),
            "report".to_string(),
            "runbook".to_string(),
        ],
    };
    assert!(
        !is_docs_only_issue(&mislabeled_technical_issue),
        "generic output labels must not exempt a technical task"
    );
}

#[test]
fn practical_finish_report_passes_with_docs_only_residual_open_issues() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[serde_json::json!({
            "id": "bd-3ar8v.6.5",
            "status": "open",
            "issue_type": "docs",
            "title": "Final report and go/no-go summary",
            "labels": ["report"]
        })],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "pass",
        "docs/report-only residual should satisfy practical-finish gate"
    );
    assert!(
        report.technical_completion_reached,
        "docs/report-only residual should mark technical completion reached"
    );
    assert_eq!(report.residual_open_scope, "docs_or_report_only");
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 1);
    assert!(
        report
            .detail
            .unwrap_or_default()
            .contains("docs/report issue(s) remain"),
        "pass detail should explain docs/report-only residual state"
    );
}

#[test]
fn practical_finish_report_treats_report_labeled_task_as_technical() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[serde_json::json!({
            "id": "bd-3ar8v.6.4",
            "status": "open",
            "issue_type": "task",
            "title": "Harden permanent performance gates and their triage runbook",
            "labels": ["docs-last", "report", "runbook"]
        })],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "generic documentation-output labels must not exempt a technical task"
    );
    assert!(!report.technical_completion_reached);
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.open_perf3x_count, 1);
    assert_eq!(report.technical_open_count, 1);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.6.4"),
        "technical blocker ID should remain visible: {detail}"
    );
}

#[test]
fn practical_finish_report_fails_when_technical_open_issues_remain() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v.6.2",
                "status": "in_progress",
                "issue_type": "task",
                "title": "Parameter sweep execution",
                "labels": ["perf-3x", "tuning"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6.5",
                "status": "open",
                "issue_type": "docs",
                "title": "Final report and go/no-go summary",
                "labels": ["report"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "remaining technical work should block practical-finish checkpoint"
    );
    assert!(
        !report.technical_completion_reached,
        "technical residuals must keep technical completion false"
    );
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.technical_open_count, 1);
    assert_eq!(report.docs_or_report_open_count, 1);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.6.2"),
        "failure detail should identify blocking technical issue IDs: {detail}"
    );
}

#[test]
fn practical_finish_report_blocks_every_nonterminal_technical_status() {
    for (index, status) in [
        "open",
        "in_progress",
        "blocked",
        "deferred",
        "draft",
        "pinned",
        "custom_waiting",
    ]
    .into_iter()
    .enumerate()
    {
        let temp = tempfile::tempdir().expect("create tempdir");
        let issue_id = format!("bd-3ar8v.6.{}", index + 20);
        write_practical_finish_issue_fixture(
            temp.path(),
            &[serde_json::json!({
                "id": issue_id,
                "status": status,
                "issue_type": "task",
                "title": "Unresolved technical work",
                "labels": ["perf-3x"]
            })],
        );

        let report = build_practical_finish_checkpoint_report(temp.path());
        assert_eq!(
            report.status, "fail",
            "nonterminal status {status} must block practical finish"
        );
        assert!(!report.technical_completion_reached);
        assert_eq!(report.open_perf3x_count, 1, "status={status}");
        assert_eq!(report.technical_open_count, 1, "status={status}");
        assert_eq!(report.docs_or_report_open_count, 0, "status={status}");
        let detail = report.detail.unwrap_or_default();
        assert!(
            detail.contains(&issue_id),
            "blocker for status {status} should remain visible: {detail}"
        );
    }
}

#[test]
fn practical_finish_report_rejects_duplicate_issue_ids() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v.6.21",
                "status": "closed",
                "issue_type": "task",
                "title": "Historical copy",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6.21",
                "status": "open",
                "issue_type": "task",
                "title": "Conflicting live copy",
                "labels": ["perf-3x"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "fail");
    assert!(!report.technical_completion_reached);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("duplicate issue id bd-3ar8v.6.21"),
        "duplicate rows must fail instead of using last-row-wins semantics: {detail}"
    );
}

#[test]
fn practical_finish_docs_descendant_cannot_hide_technical_parent() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v.6.22",
                "status": "open",
                "issue_type": "task",
                "title": "Technical parent still incomplete",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6.22.1",
                "status": "open",
                "issue_type": "docs",
                "title": "Document the incomplete parent",
                "labels": ["report"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "fail");
    assert!(!report.technical_completion_reached);
    assert_eq!(report.open_perf3x_count, 2);
    assert_eq!(report.technical_open_count, 1);
    assert_eq!(report.docs_or_report_open_count, 1);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.6.22"),
        "a docs-only child must not collapse its technical parent: {detail}"
    );
}

#[test]
fn practical_finish_report_rejects_base_when_live_issues_missing() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_complete_practical_finish_issue_fixture_to(
        temp.path(),
        ".beads/beads.base.jsonl",
        &[serde_json::json!({
            "id": "bd-3ar8v.6.5",
            "status": "open",
            "issue_type": "docs",
            "title": "Final report and go/no-go summary",
            "labels": ["report"]
        })],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "a base snapshot must not replace the commit-bound live ledger"
    );
    assert!(!report.technical_completion_reached);
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.open_perf3x_count, 0);
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("live Beads ledger is missing")
            && detail.contains(PRACTICAL_FINISH_BASE_SOURCE)
            && detail.contains("non-certifying"),
        "failure should identify the diagnostic-only base source: {detail}"
    );
}

#[test]
fn practical_finish_report_fails_closed_on_unproven_snapshot_only() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture_to(
        temp.path(),
        PRACTICAL_FINISH_SNAPSHOT_PATH,
        &[serde_json::json!({
            "id": "bd-3ar8v.6.5",
            "status": "open",
            "issue_type": "task",
            "title": "Final report and go/no-go summary",
            "labels": ["report"]
        })],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "an unproven snapshot must not certify completion when authoritative Beads sources are unavailable"
    );
    assert!(
        !report.technical_completion_reached,
        "an unproven snapshot must keep technical completion false"
    );
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.open_perf3x_count, 0);
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("live Beads ledger is missing")
            && detail.contains(PRACTICAL_FINISH_SNAPSHOT_PATH)
            && detail.contains("non-certifying"),
        "failure should explain why snapshot-only evidence is rejected: {detail}"
    );
}

#[test]
fn practical_finish_report_rejects_uncommitted_truncation_that_preserves_anchors() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[serde_json::json!({
            "id": "bd-3ar8v.6.2",
            "status": "open",
            "issue_type": "task",
            "title": "Authoritative technical blocker",
            "labels": ["perf-3x"]
        })],
    );

    write_complete_practical_finish_issue_fixture_to(
        temp.path(),
        PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        &[],
    );
    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "fail");
    assert!(!report.technical_completion_reached);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("differs between the working tree and Git index"),
        "a truncated ledger preserving all weak anchors must fail provenance: {detail}"
    );
}

#[test]
fn practical_finish_report_rejects_staged_truncation_that_preserves_anchors() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[serde_json::json!({
            "id": "bd-3ar8v.6.2",
            "status": "open",
            "issue_type": "task",
            "title": "Authoritative technical blocker",
            "labels": ["perf-3x"]
        })],
    );

    write_complete_practical_finish_issue_fixture_to(
        temp.path(),
        PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        &[],
    );
    run_practical_finish_fixture_git(
        temp.path(),
        &["add", "--", PRACTICAL_FINISH_LIVE_ISSUE_SOURCE],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "fail");
    assert!(!report.technical_completion_reached);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("differs between the Git index and HEAD"),
        "a staged truncated ledger must not certify until it is committed: {detail}"
    );
}

#[test]
fn practical_finish_report_rejects_newer_incomplete_snapshot_over_live_blocker() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[serde_json::json!({
            "id": "bd-3ar8v.6.2",
            "status": "open",
            "issue_type": "task",
            "title": "Stale technical blocker from remote cache",
            "labels": ["perf-3x", "tuning"]
        })],
    );

    write_practical_finish_issue_fixture_to(
        temp.path(),
        PRACTICAL_FINISH_SNAPSHOT_PATH,
        &[serde_json::json!({
            "id": "bd-3ar8v.6.5",
            "status": "open",
            "issue_type": "task",
            "title": "Final report and go/no-go summary",
            "labels": ["report"]
        })],
    );

    let live_path = temp.path().join(".beads/issues.jsonl");
    let snapshot_path = temp.path().join(PRACTICAL_FINISH_SNAPSHOT_PATH);
    filetime::set_file_mtime(
        &live_path,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .expect("set older live-ledger mtime");
    filetime::set_file_mtime(
        &snapshot_path,
        filetime::FileTime::from_unix_time(1_700_000_100, 0),
    )
    .expect("set newer snapshot mtime");
    let live_mtime = std::fs::metadata(&live_path)
        .and_then(|metadata| metadata.modified())
        .expect("read live-ledger mtime");
    let snapshot_mtime = std::fs::metadata(&snapshot_path)
        .and_then(|metadata| metadata.modified())
        .expect("read snapshot mtime");
    assert!(
        snapshot_mtime > live_mtime,
        "regression fixture must plant a snapshot newer than the live ledger"
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "a newer incomplete snapshot must not conceal a live technical blocker"
    );
    assert!(
        !report.technical_completion_reached,
        "the authoritative live technical blocker must keep completion false"
    );
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.open_perf3x_count, 1);
    assert_eq!(report.technical_open_count, 1);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.6.2"),
        "failure detail should retain the authoritative blocker ID: {detail}"
    );
}

#[test]
fn practical_finish_report_passes_with_no_open_perf3x_issues() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(temp.path(), &[]);

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "pass");
    assert!(
        report.technical_completion_reached,
        "no open perf-3x issues should mark technical completion reached"
    );
    assert_eq!(report.residual_open_scope, "none");
    assert_eq!(report.open_perf3x_count, 0);
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 0);
}

#[test]
fn practical_finish_report_fails_closed_on_empty_live_ledger() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture_to(temp.path(), PRACTICAL_FINISH_LIVE_ISSUE_SOURCE, &[]);
    commit_practical_finish_issue_fixture(temp.path(), PRACTICAL_FINISH_LIVE_ISSUE_SOURCE);

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "an empty live-ledger file must not certify that all technical work is complete"
    );
    assert!(!report.technical_completion_reached);
    assert_eq!(report.residual_open_scope, "technical_remaining");
    assert_eq!(report.open_perf3x_count, 0);
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("missing required canonical ledger anchor(s)")
            && detail.contains("bd-3ar8v"),
        "failure should identify the missing semantic completeness anchors: {detail}"
    );
}

#[test]
fn practical_finish_report_ignores_rollup_parent_nodes() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v",
                "status": "open",
                "issue_type": "epic",
                "title": "PERF-3X epic rollup",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6",
                "status": "open",
                "issue_type": "task",
                "title": "Phase 5 rollup",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6.2",
                "status": "in_progress",
                "issue_type": "task",
                "title": "Parameter sweep execution",
                "labels": ["perf-3x", "tuning"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(report.status, "fail");
    assert_eq!(
        report.open_perf3x_count, 1,
        "rollup nodes should not count as leaf practical-finish blockers"
    );
    assert_eq!(report.technical_open_count, 1);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.6.2"),
        "leaf blocking ID should be surfaced in failure detail: {detail}"
    );
    assert!(
        !detail.contains("bd-3ar8v,"),
        "rollup parent IDs should be excluded from failure detail: {detail}"
    );
}

#[test]
fn practical_finish_scope_only_excludes_phase7_branch_tokens() {
    assert!(
        !is_practical_finish_in_scope_issue_id("bd-3ar8v"),
        "top-level PERF-3X epic must stay out of practical-finish scope (spans Phase 6+)"
    );
    assert!(
        !is_practical_finish_in_scope_issue_id("bd-3ar8v.7"),
        "phase-7 root must stay out of practical-finish blocker scope"
    );
    assert!(
        !is_practical_finish_in_scope_issue_id("bd-3ar8v.7.2"),
        "phase-7 descendants must stay out of practical-finish blocker scope"
    );
    assert!(
        is_practical_finish_in_scope_issue_id("bd-3ar8v.70"),
        "non-phase7 ids sharing numeric prefix must remain in scope"
    );
    assert!(
        is_practical_finish_in_scope_issue_id("bd-3ar8v.70.4"),
        "descendants of non-phase7 numeric prefixes must remain in scope"
    );
}

#[test]
fn practical_finish_report_excludes_post_perf3x_phase7_nodes() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v.7.2",
                "status": "open",
                "issue_type": "task",
                "title": "Post-PERF-3X follow-on runtime hardening",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.6.5",
                "status": "open",
                "issue_type": "docs",
                "title": "Final report and go/no-go summary",
                "labels": ["report"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "pass",
        "post-PERF-3X phase-7 nodes must not block practical-finish checkpoint"
    );
    assert!(
        report.technical_completion_reached,
        "phase-7-only technical nodes should not count toward Phase-5 practical-finish blockers"
    );
    assert_eq!(report.open_perf3x_count, 1);
    assert_eq!(report.technical_open_count, 0);
    assert_eq!(report.docs_or_report_open_count, 1);
}

#[test]
fn practical_finish_report_keeps_non_phase7_numeric_prefix_ids_in_scope() {
    let temp = tempfile::tempdir().expect("create tempdir");
    write_practical_finish_issue_fixture(
        temp.path(),
        &[
            serde_json::json!({
                "id": "bd-3ar8v.7.2",
                "status": "open",
                "issue_type": "task",
                "title": "Post-PERF-3X follow-on runtime hardening",
                "labels": ["perf-3x"]
            }),
            serde_json::json!({
                "id": "bd-3ar8v.70",
                "status": "open",
                "issue_type": "task",
                "title": "In-scope technical blocker with similar numeric prefix",
                "labels": ["perf-3x"]
            }),
        ],
    );

    let report = build_practical_finish_checkpoint_report(temp.path());
    assert_eq!(
        report.status, "fail",
        "non-phase7 numeric-prefix ids must remain practical-finish blockers"
    );
    assert_eq!(report.open_perf3x_count, 1);
    assert_eq!(report.technical_open_count, 1);
    assert_eq!(report.docs_or_report_open_count, 0);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("bd-3ar8v.70"),
        "in-scope blocker id must appear in failure detail: {detail}"
    );
    assert!(
        !detail.contains("bd-3ar8v.7.2"),
        "phase-7 descendant should remain excluded from blocker detail: {detail}"
    );
}

#[test]
fn practical_finish_sub_gate_is_blocking_and_points_to_dedicated_artifact() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "practical_finish_checkpoint")
        .expect("practical_finish_checkpoint gate should exist");
    assert!(
        gate.blocking,
        "practical-finish gate must be release-blocking"
    );
    assert_eq!(
        gate.artifact_path.as_deref(),
        Some(PRACTICAL_FINISH_CHECKPOINT_ARTIFACT_REL),
        "practical-finish gate should point at dedicated checkpoint artifact"
    );
}

#[test]
fn extension_remediation_backlog_sub_gate_is_blocking_and_points_to_dedicated_artifact() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "extension_remediation_backlog")
        .expect("extension_remediation_backlog gate should exist");
    assert!(
        gate.blocking,
        "extension remediation backlog gate must be release-blocking"
    );
    assert_eq!(
        gate.artifact_path.as_deref(),
        Some(EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL),
        "extension remediation backlog gate should point at dedicated artifact"
    );
}

#[test]
fn extension_remediation_backlog_gate_fails_closed_on_summary_shape_mismatch() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("full_suite_gate");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("extension_remediation_backlog.json");
    let payload = serde_json::json!({
        "schema": "pi.qa.extension_remediation_backlog.v1",
        "summary": {
            "total_non_pass_extensions": 2,
            "actionable": 1,
            "non_actionable": 0
        },
        "entries": [
            { "extension_id": "npm/example-a" }
        ]
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_extension_remediation_backlog_artifact(
        temp.path(),
        EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL,
    );
    assert_eq!(status, "fail");
    assert!(
        detail
            .unwrap_or_default()
            .contains("summary.total_non_pass_extensions"),
        "shape mismatch should be reported explicitly"
    );
}

#[test]
fn extension_remediation_backlog_gate_passes_on_consistent_summary_shape() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("full_suite_gate");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("extension_remediation_backlog.json");
    let payload = serde_json::json!({
        "schema": "pi.qa.extension_remediation_backlog.v1",
        "summary": {
            "total_non_pass_extensions": 2,
            "actionable": 1,
            "non_actionable": 1
        },
        "entries": [
            { "extension_id": "npm/example-a" },
            { "extension_id": "npm/example-b" }
        ]
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_extension_remediation_backlog_artifact(
        temp.path(),
        EXTENSION_REMEDIATION_BACKLOG_ARTIFACT_REL,
    );
    assert_eq!(status, "pass");
    assert!(
        detail.is_none(),
        "valid shape should not produce gate detail"
    );
}

#[test]
fn opportunity_matrix_sub_gate_is_blocking_and_points_to_primary_artifact() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "opportunity_matrix_integrity")
        .expect("opportunity_matrix_integrity gate should exist");
    assert!(
        gate.blocking,
        "opportunity matrix gate must be release-blocking"
    );
    assert_eq!(
        gate.artifact_path.as_deref(),
        Some(OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL),
        "opportunity matrix gate should point at primary artifact path"
    );
}

#[test]
fn opportunity_matrix_gate_fails_closed_on_readiness_decision_incoherence() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("opportunity_matrix.json");
    let payload = serde_json::json!({
        "schema": "pi.perf.opportunity_matrix.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json",
            "weighted_bottleneck_schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
            "weighted_bottleneck_status": "computed"
        },
        "readiness": {
            "status": "ready",
            "decision": "NO_DECISION",
            "ready_for_phase5": true,
            "blocking_reasons": []
        },
        "ranked_opportunities": [
            {
                "rank": 1,
                "stage": "open_ms",
                "priority_score": 1.2
            }
        ]
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_opportunity_matrix_artifact(temp.path());
    assert_eq!(status, "fail");
    assert!(
        detail.unwrap_or_default().contains("readiness.decision"),
        "readiness decision mismatch should be reported explicitly"
    );
}

#[test]
fn opportunity_matrix_gate_passes_on_consistent_contract_shape() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("opportunity_matrix.json");
    let payload = serde_json::json!({
        "schema": "pi.perf.opportunity_matrix.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json",
            "weighted_bottleneck_schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
            "weighted_bottleneck_status": "missing"
        },
        "readiness": {
            "status": "blocked",
            "decision": "NO_DECISION",
            "ready_for_phase5": false,
            "blocking_reasons": ["phase1_matrix_not_ready_for_phase5"]
        },
        "ranked_opportunities": []
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_opportunity_matrix_artifact(temp.path());
    assert_eq!(status, "pass");
    assert!(
        detail.is_none(),
        "valid contract should not produce gate detail"
    );
}

#[test]
fn parameter_sweeps_sub_gate_is_blocking_and_points_to_primary_artifact() {
    let gates = collect_gates(&repo_root());
    let gate = gates
        .iter()
        .find(|gate| gate.id == "parameter_sweeps_integrity")
        .expect("parameter_sweeps_integrity gate should exist");
    assert!(
        gate.blocking,
        "parameter sweeps gate must be release-blocking"
    );
    assert_eq!(
        gate.artifact_path.as_deref(),
        Some(PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL),
        "parameter sweeps gate should point at primary artifact path"
    );
}

#[test]
fn parameter_sweeps_gate_fails_closed_on_readiness_incoherence() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("parameter_sweeps.json");
    let payload = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json"
        },
        "readiness": {
            "status": "ready",
            "ready_for_phase5": false,
            "blocking_reasons": []
        },
        "selected_defaults": {
            "flush_cadence_ms": 500,
            "queue_max_items": 3072,
            "compaction_quota_mb": 96
        },
        "sweep_plan": {
            "dimensions": [
                { "name": "flush_cadence_ms", "candidate_values": [250, 500] },
                { "name": "queue_max_items", "candidate_values": [1024, 2048] },
                { "name": "compaction_quota_mb", "candidate_values": [64, 96] }
            ]
        }
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_parameter_sweeps_artifact(temp.path());
    assert_eq!(status, "fail");
    assert!(
        detail.unwrap_or_default().contains("readiness"),
        "readiness mismatch should be reported explicitly"
    );
}

#[test]
fn parameter_sweeps_gate_passes_on_consistent_contract_shape() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let report_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&report_dir).expect("create report directory");
    let artifact_path = report_dir.join("parameter_sweeps.json");
    let payload = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json"
        },
        "readiness": {
            "status": "blocked",
            "ready_for_phase5": false,
            "blocking_reasons": ["phase1_matrix_not_ready_for_phase5"]
        },
        "selected_defaults": {
            "flush_cadence_ms": 500,
            "queue_max_items": 3072,
            "compaction_quota_mb": 96
        },
        "sweep_plan": {
            "dimensions": [
                { "name": "flush_cadence_ms", "candidate_values": [250, 500] },
                { "name": "queue_max_items", "candidate_values": [1024, 2048] },
                { "name": "compaction_quota_mb", "candidate_values": [64, 96] }
            ]
        }
    });
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
    )
    .expect("write artifact");

    let (status, detail) = check_parameter_sweeps_artifact(temp.path());
    assert_eq!(status, "pass");
    assert!(
        detail.is_none(),
        "valid contract should not produce gate detail"
    );
}

#[test]
fn conformance_stress_lineage_passes_with_valid_artifacts() {
    let temp = tempfile::tempdir().expect("create tempdir");

    let conformance_dir = temp
        .path()
        .join("tests")
        .join("ext_conformance")
        .join("reports");
    std::fs::create_dir_all(&conformance_dir).expect("create conformance reports directory");
    let conformance_path = conformance_dir.join("conformance_summary.json");
    std::fs::write(
        &conformance_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "run_id": "local-20260217T000000000Z",
            "correlation_id": "conformance-summary-local-20260217T000000000Z",
            "pass_rate_pct": 100.0,
            "counts": { "total": 223, "pass": 60, "fail": 0, "na": 163, "tested": 60 }
        }))
        .expect("serialize"),
    )
    .expect("write conformance artifact");

    let stress_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&stress_dir).expect("create perf reports directory");
    let stress_path = stress_dir.join("stress_triage.json");
    std::fs::write(
        &stress_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.stress_triage.v1",
            "run_id": "local-20260217T000000000Z",
            "correlation_id": "stress-triage-local-20260217T000000000Z",
            "pass": true,
            "generated_at": "2026-02-17T00:00:00Z",
            "results": { "extensions_loaded": 15 }
        }))
        .expect("serialize"),
    )
    .expect("write stress artifact");

    let (status, _detail) = check_conformance_stress_lineage_coherence(temp.path());
    assert_eq!(status, "pass", "valid lineage should pass");
}

#[test]
fn conformance_stress_lineage_fails_when_run_id_missing() {
    let temp = tempfile::tempdir().expect("create tempdir");

    let conformance_dir = temp
        .path()
        .join("tests")
        .join("ext_conformance")
        .join("reports");
    std::fs::create_dir_all(&conformance_dir).expect("create conformance reports directory");
    std::fs::write(
        conformance_dir.join("conformance_summary.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "correlation_id": "test-corr",
            "pass_rate_pct": 100.0,
            "counts": { "total": 60, "pass": 60 }
        }))
        .expect("serialize"),
    )
    .expect("write");

    let stress_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&stress_dir).expect("create perf reports directory");
    std::fs::write(
        stress_dir.join("stress_triage.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.stress_triage.v1",
            "run_id": "local-test",
            "correlation_id": "stress-test",
            "pass": true
        }))
        .expect("serialize"),
    )
    .expect("write");

    let (status, detail) = check_conformance_stress_lineage_coherence(temp.path());
    assert_eq!(status, "fail", "missing run_id should fail");
    assert!(
        detail
            .as_deref()
            .unwrap_or("")
            .contains("conformance_summary.json missing non-empty run_id"),
        "detail should mention missing run_id: {detail:?}"
    );
}

#[test]
fn conformance_stress_lineage_fails_when_stress_verdict_not_pass() {
    let temp = tempfile::tempdir().expect("create tempdir");

    let conformance_dir = temp
        .path()
        .join("tests")
        .join("ext_conformance")
        .join("reports");
    std::fs::create_dir_all(&conformance_dir).expect("create conformance reports directory");
    std::fs::write(
        conformance_dir.join("conformance_summary.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "run_id": "local-test",
            "correlation_id": "test-corr",
            "pass_rate_pct": 100.0,
            "counts": { "total": 60, "pass": 60 }
        }))
        .expect("serialize"),
    )
    .expect("write");

    let stress_dir = temp.path().join("tests").join("perf").join("reports");
    std::fs::create_dir_all(&stress_dir).expect("create perf reports directory");
    std::fs::write(
        stress_dir.join("stress_triage.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.stress_triage.v1",
            "run_id": "local-test",
            "correlation_id": "stress-test",
            "pass": false
        }))
        .expect("serialize"),
    )
    .expect("write");

    let (status, detail) = check_conformance_stress_lineage_coherence(temp.path());
    assert_eq!(status, "fail", "stress verdict not pass should fail");
    assert!(
        detail
            .as_deref()
            .unwrap_or("")
            .contains("stress_triage.json verdict is not pass"),
        "detail should mention stress verdict: {detail:?}"
    );
}

#[test]
fn fail_close_blocking_skips_only_converts_blocking_skip_statuses() {
    let mut gates = vec![
        SubGate {
            id: "blocking_skip".to_string(),
            name: "Blocking skip".to_string(),
            bead: "bd-test.1".to_string(),
            status: "skip".to_string(),
            blocking: true,
            artifact_path: Some("missing.json".to_string()),
            detail: Some("Artifact not found: missing.json".to_string()),
            reproduce_command: None,
        },
        SubGate {
            id: "non_blocking_skip".to_string(),
            name: "Non-blocking skip".to_string(),
            bead: "bd-test.2".to_string(),
            status: "skip".to_string(),
            blocking: false,
            artifact_path: Some("optional.json".to_string()),
            detail: Some("Artifact not found: optional.json".to_string()),
            reproduce_command: None,
        },
        SubGate {
            id: "blocking_pass".to_string(),
            name: "Blocking pass".to_string(),
            bead: "bd-test.3".to_string(),
            status: "pass".to_string(),
            blocking: true,
            artifact_path: Some("present.json".to_string()),
            detail: None,
            reproduce_command: None,
        },
    ];

    fail_close_blocking_skips(&mut gates);

    assert_eq!(gates[0].status, "fail");
    assert!(
        gates[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("fail-closed policy")
    );
    assert_eq!(gates[1].status, "skip");
    assert_eq!(gates[2].status, "pass");
}

#[test]
fn write_non_empty_artifact_rejects_empty_payload() {
    let path = repo_root()
        .join("tests")
        .join("full_suite_gate")
        .join("certification_report.md");
    let err = write_non_empty_artifact(
        &path,
        "tests/full_suite_gate/certification_report.md",
        " \n\t ",
    )
    .expect_err("empty payload must fail closed");
    assert!(
        err.contains("empty artifact payload"),
        "expected empty-payload detail, got: {err}"
    );
}

#[test]
fn write_non_empty_artifact_fails_when_write_path_is_not_a_file() {
    let path = repo_root().join("tests").join("full_suite_gate");
    let err = write_non_empty_artifact(
        &path,
        "tests/full_suite_gate/certification_report.md",
        "# non-empty markdown",
    )
    .expect_err("directory write path must fail closed");
    assert!(
        err.contains("failed to write"),
        "expected write failure detail, got: {err}"
    );
}

#[test]
fn assert_non_empty_text_artifact_rejects_whitespace_only_file() {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos();
    path.push(format!(
        "pi_agent_rust_certification_report_empty_{nonce}.md"
    ));

    std::fs::write(&path, " \n\t").expect("write whitespace artifact");
    let err =
        assert_non_empty_text_artifact(&path, "tests/full_suite_gate/certification_report.md")
            .expect_err("whitespace-only report must fail closed");
    assert!(
        err.contains("empty artifact body"),
        "expected empty-body detail, got: {err}"
    );

    let _ = std::fs::remove_file(&path);
}

fn fixed_utc(ts: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .unwrap_or_else(|err| panic!("invalid fixed RFC3339 timestamp {ts}: {err}"))
        .with_timezone(&chrono::Utc)
}

fn must_pass_lineage_fixture(generated_at: &str) -> Value {
    serde_json::json!({
        "status": "pass",
        "run_id": "run-123",
        "correlation_id": "must-pass-gate-run-123",
        "generated_at": generated_at
    })
}

const TEST_MUST_PASS_SHA256_A: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TEST_MUST_PASS_SHA256_B: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn must_pass_contract_fixture(count: u64) -> Value {
    serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "status": "pass",
        "run_id": "run-123",
        "correlation_id": "must-pass-gate-run-123",
        "generated_at": "2026-08-04T12:00:00Z",
        "git_commit": "0123456789abcdef0123456789abcdef01234567",
        "source_tree_sha256": TEST_MUST_PASS_SHA256_A,
        "inclusion_sha256": TEST_MUST_PASS_SHA256_A,
        "manifest_sha256": TEST_MUST_PASS_SHA256_A,
        "observed": {
            "must_pass_total": count,
            "must_pass_tested": count,
            "must_pass_passed": count,
            "must_pass_failed": 0,
            "must_pass_skipped": 0,
            "must_pass_pass_rate_pct": 100.0
        }
    })
}

fn canonical_must_pass_fixture(count: usize) -> (Vec<u8>, Vec<u8>, BTreeSet<String>) {
    let ids = (0..count)
        .map(|index| format!("extension-{index:03}"))
        .collect::<Vec<_>>();
    let inclusion = serde_json::json!({
        "schema": "pi.ext.inclusion_list.v1",
        "tier1": ids
            .iter()
            .map(|id| serde_json::json!({"id": id}))
            .collect::<Vec<_>>(),
        "tier1_review": [],
        "summary": {
            "tier1_count": count,
            "tier1_review_count": 0,
            "total_must_pass": count
        }
    });
    let manifest = serde_json::json!({
        "schema": "pi.ext.validated-manifest.v1",
        "extensions": ids
            .iter()
            .map(|id| serde_json::json!({
                "id": id,
                "conformance_tier": 1,
                "entry_path": format!("{id}/index.ts")
            }))
            .collect::<Vec<_>>()
    });
    let tracked_paths = ids
        .iter()
        .map(|id| format!("{MUST_PASS_ARTIFACTS_PATH}/{id}/index.ts"))
        .collect();
    (
        serde_json::to_vec(&inclusion).expect("serialize inclusion fixture"),
        serde_json::to_vec(&manifest).expect("serialize manifest fixture"),
        tracked_paths,
    )
}

#[test]
fn canonical_must_pass_contract_requires_exact_versioned_denominator() {
    let (inclusion, manifest, tracked_paths) = canonical_must_pass_fixture(208);
    let entries = canonical_must_pass_entries(&inclusion, &manifest, &tracked_paths)
        .expect("the exact v1 denominator should pass");
    assert_eq!(entries.len(), 208);

    let (inclusion, manifest, tracked_paths) = canonical_must_pass_fixture(207);
    let err = canonical_must_pass_entries(&inclusion, &manifest, &tracked_paths)
        .expect_err("a smaller internally coherent set must not redefine the v1 denominator");
    assert!(
        err.contains("unexpected versioned must-pass denominator") && err.contains("expected=208"),
        "{err}"
    );
}

#[test]
fn canonical_must_pass_contract_binds_manifest_entry_paths_to_tracked_inputs() {
    let (inclusion, manifest, mut tracked_paths) = canonical_must_pass_fixture(208);
    assert!(
        tracked_paths.remove("tests/ext_conformance/artifacts/extension-007/index.ts"),
        "fixture must remove one canonical artifact path"
    );
    let err = canonical_must_pass_entries(&inclusion, &manifest, &tracked_paths)
        .expect_err("an untracked manifest artifact input must fail closed");
    assert!(
        err.contains("extension-007") && err.contains("not tracked"),
        "{err}"
    );
}

#[test]
fn canonical_must_pass_contract_rejects_duplicate_artifact_identities() {
    let (inclusion, manifest, tracked_paths) = canonical_must_pass_fixture(208);
    let mut manifest: Value =
        serde_json::from_slice(&manifest).expect("parse canonical manifest fixture");
    let first_entry_path = manifest["extensions"][0]["entry_path"].clone();
    manifest["extensions"][1]["entry_path"] = first_entry_path;
    let manifest = serde_json::to_vec(&manifest).expect("serialize duplicate-artifact fixture");

    let error = canonical_must_pass_entries(&inclusion, &manifest, &tracked_paths)
        .expect_err("two extension IDs must not share one artifact identity");
    assert!(error.contains("reuses artifact entry_path"), "{error}");
}

#[test]
fn canonical_must_pass_contract_rejects_unsafe_artifact_paths_even_when_tracked() {
    for unsafe_path in [
        "C:/outside.ts",
        "C:outside.ts",
        "../outside.ts",
        "directory/\ncontrol.ts",
    ] {
        let (inclusion, manifest, mut tracked_paths) = canonical_must_pass_fixture(208);
        let mut manifest: Value =
            serde_json::from_slice(&manifest).expect("parse canonical manifest fixture");
        manifest["extensions"][0]["entry_path"] = Value::String(unsafe_path.to_string());
        tracked_paths.insert(format!("{MUST_PASS_ARTIFACTS_PATH}/{unsafe_path}"));
        let manifest =
            serde_json::to_vec(&manifest).expect("serialize unsafe-artifact-path fixture");

        let error = canonical_must_pass_entries(&inclusion, &manifest, &tracked_paths)
            .expect_err("unsafe manifest artifact path must fail before tracked-path lookup");
        assert!(
            error.contains("non-canonical artifact entry_path"),
            "{unsafe_path}: {error}"
        );
    }
}

fn committed_evidence_binding_fixture(
    attributes: Option<&str>,
    track_events: bool,
) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("create committed-evidence fixture");
    let root = temp.path();
    run_practical_finish_fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
    let verdict_path = root.join(MUST_PASS_VERDICT_PATH);
    let events_path = root.join(MUST_PASS_EVENTS_PATH);
    std::fs::create_dir_all(verdict_path.parent().expect("verdict fixture parent"))
        .expect("create committed-evidence fixture directory");
    std::fs::write(&verdict_path, "{}\n").expect("write verdict fixture");
    std::fs::write(&events_path, "{}\n").expect("write events fixture");
    if let Some(contents) = attributes {
        std::fs::write(root.join(".gitattributes"), contents)
            .expect("write evidence attributes fixture");
        run_practical_finish_fixture_git(root, &["add", ".gitattributes"]);
    }
    run_practical_finish_fixture_git(root, &["add", MUST_PASS_VERDICT_PATH]);
    if track_events {
        run_practical_finish_fixture_git(root, &["add", MUST_PASS_EVENTS_PATH]);
    }
    run_practical_finish_fixture_git(
        root,
        &[
            "-c",
            "user.name=Pi Evidence Fixture",
            "-c",
            "user.email=pi-evidence@example.invalid",
            "commit",
            "--quiet",
            "--message",
            "fixture",
        ],
    );
    temp
}

#[test]
fn committed_must_pass_evidence_rejects_untracked_staged_and_worktree_inputs() {
    let untracked = committed_evidence_binding_fixture(None, false);
    let error = capture_committed_must_pass_evidence(untracked.path())
        .expect_err("an untracked event log must fail closed");
    assert!(error.contains("tracked regular blob at HEAD"), "{error}");

    let staged = committed_evidence_binding_fixture(None, true);
    capture_committed_must_pass_evidence(staged.path())
        .expect("clean committed evidence must be accepted");
    std::fs::write(
        staged.path().join(MUST_PASS_VERDICT_PATH),
        "{\"staged\":true}\n",
    )
    .expect("write staged verdict drift");
    run_practical_finish_fixture_git(staged.path(), &["add", MUST_PASS_VERDICT_PATH]);
    let error = capture_committed_must_pass_evidence(staged.path())
        .expect_err("staged evidence drift must fail closed");
    assert!(error.contains("between the Git index and HEAD"), "{error}");

    let worktree = committed_evidence_binding_fixture(None, true);
    std::fs::write(
        worktree.path().join(MUST_PASS_EVENTS_PATH),
        "{\"worktree\":true}\n",
    )
    .expect("write worktree event drift");
    let error = capture_committed_must_pass_evidence(worktree.path())
        .expect_err("unstaged evidence drift must fail closed");
    assert!(
        error.contains("between the working tree and Git index"),
        "{error}"
    );
}

#[test]
fn committed_must_pass_evidence_rejects_flags_and_filter_hidden_bytes() {
    for (flag, path) in [
        ("--assume-unchanged", MUST_PASS_VERDICT_PATH),
        ("--skip-worktree", MUST_PASS_EVENTS_PATH),
    ] {
        let flagged = committed_evidence_binding_fixture(None, true);
        run_practical_finish_fixture_git(flagged.path(), &["update-index", flag, path]);
        let error = capture_committed_must_pass_evidence(flagged.path())
            .expect_err("non-canonical evidence index flags must fail closed");
        assert!(
            error.contains("non-canonical index flags"),
            "{flag}: {error}"
        );
    }

    let filtered =
        committed_evidence_binding_fixture(Some("*.json text eol=lf\n*.jsonl text eol=lf\n"), true);
    std::fs::write(filtered.path().join(MUST_PASS_EVENTS_PATH), "{}\r\n")
        .expect("write filter-hidden event drift");
    let diff_status = std::process::Command::new("git")
        .arg("-C")
        .arg(filtered.path())
        .args(["diff", "--quiet", "--", MUST_PASS_EVENTS_PATH])
        .status()
        .expect("check filter-hidden evidence fixture");
    assert!(
        diff_status.success(),
        "fixture must demonstrate evidence drift hidden by Git clean filtering"
    );
    let error = capture_committed_must_pass_evidence(filtered.path())
        .expect_err("raw evidence byte drift must fail closed");
    assert!(error.contains("raw worktree bytes differ"), "{error}");
}

#[test]
fn live_ledger_binding_rejects_noncanonical_flags_and_head_mode() {
    let flagged = tempfile::tempdir().expect("create flagged ledger fixture");
    write_practical_finish_issue_fixture(flagged.path(), &[]);
    run_practical_finish_fixture_git(
        flagged.path(),
        &[
            "update-index",
            "--skip-worktree",
            PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        ],
    );
    let bytes = std::fs::read(flagged.path().join(PRACTICAL_FINISH_LIVE_ISSUE_SOURCE))
        .expect("read flagged ledger fixture");
    let error = validate_live_ledger_git_binding(
        flagged.path(),
        PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        &bytes,
    )
    .expect_err("skip-worktree ledger must fail closed");
    assert!(error.contains("non-canonical index flags"), "{error}");

    let nonregular = tempfile::tempdir().expect("create nonregular ledger fixture");
    write_practical_finish_issue_fixture(nonregular.path(), &[]);
    let oid_output = std::process::Command::new("git")
        .arg("-C")
        .arg(nonregular.path())
        .args(["hash-object", PRACTICAL_FINISH_LIVE_ISSUE_SOURCE])
        .output()
        .expect("hash nonregular ledger fixture");
    assert!(oid_output.status.success(), "git hash-object must succeed");
    let oid = String::from_utf8(oid_output.stdout)
        .expect("ledger fixture object ID is UTF-8")
        .trim()
        .to_string();
    run_practical_finish_fixture_git(
        nonregular.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{oid},{PRACTICAL_FINISH_LIVE_ISSUE_SOURCE}"),
        ],
    );
    run_practical_finish_fixture_git(
        nonregular.path(),
        &[
            "-c",
            "user.name=Pi Gate Fixture",
            "-c",
            "user.email=pi-gate-fixture@example.invalid",
            "commit",
            "--quiet",
            "--message",
            "Make ledger nonregular",
        ],
    );
    let bytes = std::fs::read(nonregular.path().join(PRACTICAL_FINISH_LIVE_ISSUE_SOURCE))
        .expect("read nonregular ledger fixture");
    let error = validate_live_ledger_git_binding(
        nonregular.path(),
        PRACTICAL_FINISH_LIVE_ISSUE_SOURCE,
        &bytes,
    )
    .expect_err("nonregular HEAD ledger mode must fail closed");
    assert!(
        error.contains("not a canonical regular blob at HEAD"),
        "{error}"
    );
}

#[test]
fn must_pass_source_snapshot_rejects_filter_normalized_worktree_bytes() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let root = temp.path();
    run_practical_finish_fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join(".gitattributes"), "tracked.txt text eol=lf\n")
        .expect("write attributes fixture");
    std::fs::write(root.join("tracked.txt"), "tracked\n").expect("write source fixture");
    run_practical_finish_fixture_git(root, &["add", ".gitattributes", "tracked.txt"]);
    run_practical_finish_fixture_git(
        root,
        &[
            "-c",
            "user.name=Pi Gate Fixture",
            "-c",
            "user.email=pi-gate-fixture@example.invalid",
            "commit",
            "--quiet",
            "--message",
            "Add must-pass source fixture",
        ],
    );

    let commit = current_git_commit(root).expect("resolve source fixture commit");
    let records =
        must_pass_tree_records(root, &commit, &["tracked.txt"]).expect("read source fixture tree");
    ensure_worktree_bytes_match_tree(root, &records).expect("initial bytes must match tree");

    std::fs::write(root.join("tracked.txt"), "tracked\r\n")
        .expect("write filter-normalized source fixture");
    let diff_status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--quiet", "--", "tracked.txt"])
        .status()
        .expect("run git diff for filter-normalized fixture");
    assert!(
        diff_status.success(),
        "fixture must demonstrate a byte change hidden by Git clean filtering"
    );
    let err = ensure_worktree_bytes_match_tree(root, &records)
        .expect_err("filter-normalized byte drift must fail closed");
    assert!(err.contains("worktree bytes differ"), "{err}");
}

#[test]
fn must_pass_source_commit_rejects_reverted_non_evidence_followup() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let root = temp.path();
    let commit = |message: &str| {
        run_practical_finish_fixture_git(
            root,
            &[
                "-c",
                "user.name=Pi Gate Fixture",
                "-c",
                "user.email=pi-gate-fixture@example.invalid",
                "commit",
                "--quiet",
                "--message",
                message,
            ],
        );
        current_git_commit(root).expect("resolve fixture commit")
    };

    run_practical_finish_fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join("source.txt"), "tested source\n").expect("write source fixture");
    run_practical_finish_fixture_git(root, &["add", "source.txt"]);
    let source_commit = commit("Add tested source");

    std::fs::create_dir_all(root.join("tests/evidence_bundle"))
        .expect("create evidence fixture directory");
    std::fs::write(root.join("tests/evidence_bundle/index.json"), "{}\n")
        .expect("write evidence fixture");
    run_practical_finish_fixture_git(root, &["add", "tests/evidence_bundle/index.json"]);
    let evidence_commit = commit("Add evidence only");
    validate_must_pass_git_commit(root, &source_commit, &evidence_commit)
        .expect("evidence-only descendant must be accepted");

    std::fs::write(root.join("source.txt"), "temporary change\n")
        .expect("change non-evidence source");
    run_practical_finish_fixture_git(root, &["add", "source.txt"]);
    commit("Temporarily change source");
    std::fs::write(root.join("source.txt"), "tested source\n")
        .expect("restore non-evidence source");
    run_practical_finish_fixture_git(root, &["add", "source.txt"]);
    let reverted_commit = commit("Restore source bytes");

    let err = validate_must_pass_git_commit(root, &source_commit, &reverted_commit)
        .expect_err("a reverted non-evidence follow-up must still fail closed");
    assert!(err.contains("non-evidence paths"), "{err}");
}

#[test]
fn must_pass_verdict_contract_accepts_exact_current_set() {
    let verdict = must_pass_contract_fixture(208);
    let result = validate_must_pass_verdict_contract(
        &verdict,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    );
    assert!(
        result.is_ok(),
        "exact authoritative contract should pass: {result:?}"
    );
}

#[test]
fn must_pass_verdict_contract_rejects_status_only_lineage_artifact() {
    let verdict = must_pass_lineage_fixture("2026-08-04T12:00:00Z");
    let err = validate_must_pass_verdict_contract(
        &verdict,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    )
    .expect_err("status plus fresh lineage must not substitute for certification evidence");
    assert!(
        err.contains("schema mismatch"),
        "the strict contract should reject the incomplete artifact immediately: {err}"
    );
}

#[test]
fn must_pass_verdict_contract_rejects_abbreviated_commit_ids() {
    let mut verdict = must_pass_contract_fixture(208);
    verdict["git_commit"] = serde_json::json!("0123456");
    let err = validate_must_pass_verdict_contract(
        &verdict,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    )
    .expect_err("abbreviated commit IDs are not immutable provenance");
    assert!(err.contains("full 40- or 64-character"), "{err}");
}

#[test]
fn must_pass_verdict_contract_rejects_smaller_historical_pass_set() {
    let verdict = must_pass_contract_fixture(1);
    let err = validate_must_pass_verdict_contract(
        &verdict,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    )
    .expect_err("a historical 1/1 pass must not certify the current 208-entry set");
    assert!(
        err.contains("exact canonical set")
            && err.contains("expected=208")
            && err.contains("total=1"),
        "count mismatch should identify the historical subset: {err}"
    );
}

#[test]
fn must_pass_verdict_contract_rejects_stale_or_missing_provenance() {
    for field in ["source_tree_sha256", "inclusion_sha256", "manifest_sha256"] {
        let mut stale = must_pass_contract_fixture(208);
        stale[field] = serde_json::json!(TEST_MUST_PASS_SHA256_B);
        let err = validate_must_pass_verdict_contract(
            &stale,
            208,
            TEST_MUST_PASS_SHA256_A,
            TEST_MUST_PASS_SHA256_A,
            TEST_MUST_PASS_SHA256_A,
        )
        .expect_err("stale provenance must fail closed");
        assert!(
            err.contains(field) && err.contains("current authoritative release inputs"),
            "stale {field} should be diagnosed precisely: {err}"
        );

        let mut missing = must_pass_contract_fixture(208);
        missing
            .as_object_mut()
            .expect("fixture is an object")
            .remove(field);
        let err = validate_must_pass_verdict_contract(
            &missing,
            208,
            TEST_MUST_PASS_SHA256_A,
            TEST_MUST_PASS_SHA256_A,
            TEST_MUST_PASS_SHA256_A,
        )
        .expect_err("missing provenance must fail closed");
        assert!(
            err.contains(field) && err.contains("missing non-empty"),
            "missing {field} should be diagnosed precisely: {err}"
        );
    }
}

#[test]
fn must_pass_verdict_contract_rejects_incoherent_or_nonpassing_counts() {
    let mut incoherent = must_pass_contract_fixture(208);
    incoherent["observed"]["must_pass_tested"] = serde_json::json!(207);
    let err = validate_must_pass_verdict_contract(
        &incoherent,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    )
    .expect_err("incoherent counts must fail closed");
    assert!(err.contains("counts are incoherent"), "{err}");

    let mut skipped = must_pass_contract_fixture(208);
    skipped["observed"]["must_pass_tested"] = serde_json::json!(207);
    skipped["observed"]["must_pass_passed"] = serde_json::json!(207);
    skipped["observed"]["must_pass_skipped"] = serde_json::json!(1);
    let err = validate_must_pass_verdict_contract(
        &skipped,
        208,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
        TEST_MUST_PASS_SHA256_A,
    )
    .expect_err("skipped canonical entries must fail closed");
    assert!(
        err.contains("exact canonical set") && err.contains("skipped=1"),
        "{err}"
    );
}

fn write_must_pass_event_fixture(root: &Path, events: &[Value]) {
    let path = root.join(MUST_PASS_EVENTS_PATH);
    std::fs::create_dir_all(path.parent().expect("events fixture parent"))
        .expect("create events fixture directory");
    let body = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize event fixture"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, body + "\n").expect("write events fixture");
}

fn must_pass_event_fixture(id: &str) -> Value {
    serde_json::json!({
        "schema": MUST_PASS_EVENT_SCHEMA,
        "set": "must_pass",
        "status": "pass",
        "id": id,
        "tier": 1,
        "run_id": "run-123",
        "correlation_id": "must-pass-gate-run-123",
        "git_commit": "0123456789abcdef0123456789abcdef01234567",
        "source_tree_sha256": TEST_MUST_PASS_SHA256_A,
        "inclusion_sha256": TEST_MUST_PASS_SHA256_A,
        "manifest_sha256": TEST_MUST_PASS_SHA256_A
    })
}

#[test]
fn must_pass_event_contract_rejects_duplicate_ids() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let event = must_pass_event_fixture("duplicate-extension");
    write_must_pass_event_fixture(temp.path(), &[event.clone(), event]);

    let verdict = must_pass_contract_fixture(2);
    let err = observed_must_pass_events(temp.path(), &verdict)
        .expect_err("duplicate must-pass events must fail closed");
    assert!(
        err.contains("duplicate must-pass event id duplicate-extension"),
        "{err}"
    );
}

#[test]
fn must_pass_event_contract_rejects_unbound_source_provenance() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let mut event = must_pass_event_fixture("extension-a");
    event["source_tree_sha256"] = serde_json::json!(TEST_MUST_PASS_SHA256_B);
    write_must_pass_event_fixture(temp.path(), &[event]);

    let verdict = must_pass_contract_fixture(1);
    let err = observed_must_pass_events(temp.path(), &verdict)
        .expect_err("events from another source tree must fail closed");
    assert!(err.contains("source_tree_sha256 mismatch"), "{err}");
}

#[test]
fn must_pass_event_contract_validates_stretch_rows_instead_of_skipping_them() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let must_pass = must_pass_event_fixture("extension-a");
    let mut invalid_stretch = must_pass_event_fixture("extension-stretch");
    invalid_stretch["set"] = Value::String("stretch".to_string());
    invalid_stretch["schema"] = Value::String("pi.ext.gate_event.v0".to_string());
    write_must_pass_event_fixture(temp.path(), &[must_pass.clone(), invalid_stretch]);

    let verdict = must_pass_contract_fixture(1);
    let error = observed_must_pass_events(temp.path(), &verdict)
        .expect_err("a malformed stretch row must not be silently ignored");
    assert!(error.contains("invalid event schema"), "{error}");

    let mut stretch = must_pass_event_fixture("extension-stretch");
    stretch["set"] = Value::String("stretch".to_string());
    stretch["status"] = Value::String("fail".to_string());
    write_must_pass_event_fixture(temp.path(), &[must_pass, stretch.clone(), stretch]);
    let error = observed_must_pass_events(temp.path(), &verdict)
        .expect_err("duplicate stretch identities must fail closed");
    assert!(
        error.contains("duplicate stretch event id extension-stretch"),
        "{error}"
    );
}

#[test]
fn must_pass_event_contract_rejects_canonical_ids_mislabeled_as_stretch() {
    let verdict = must_pass_contract_fixture(1);
    let mut stretch = must_pass_event_fixture("extension-a");
    stretch["set"] = Value::String("stretch".to_string());
    let contents = [
        serde_json::to_string(&must_pass_event_fixture("extension-a"))
            .expect("serialize must-pass event"),
        serde_json::to_string(&stretch).expect("serialize stretch event"),
    ]
    .join("\n");
    let events = observed_must_pass_events_from_contents(&contents, &verdict)
        .expect("both rows are individually well-formed");
    let expected = BTreeMap::from([("extension-a".to_string(), 1)]);

    let error = validate_event_set_identities(&events, &expected)
        .expect_err("a canonical must-pass ID must not also be labeled stretch");
    assert!(error.contains("incorrectly labeled stretch"), "{error}");
}

#[test]
fn must_pass_lineage_validator_accepts_fresh_linked_metadata() {
    let now = fixed_utc("2026-02-17T00:00:00Z");
    let verdict = must_pass_lineage_fixture("2026-02-16T23:59:00Z");
    let result = validate_must_pass_lineage_metadata(&verdict, "must_pass_gate_verdict.json", now);
    assert!(
        result.is_ok(),
        "fresh linked metadata should pass: {result:?}"
    );
}

#[test]
fn must_pass_lineage_validator_fails_when_run_id_missing() {
    let now = fixed_utc("2026-02-17T00:00:00Z");
    let mut verdict = must_pass_lineage_fixture("2026-02-16T23:59:00Z");
    verdict["run_id"] = serde_json::json!(" ");
    let err = validate_must_pass_lineage_metadata(&verdict, "must_pass_gate_verdict.json", now)
        .expect_err("missing run_id must fail closed");
    assert!(
        err.contains("run_id"),
        "expected run_id failure detail: {err}"
    );
}

#[test]
fn must_pass_lineage_validator_fails_when_correlation_does_not_include_run_id() {
    let now = fixed_utc("2026-02-17T00:00:00Z");
    let mut verdict = must_pass_lineage_fixture("2026-02-16T23:59:00Z");
    verdict["correlation_id"] = serde_json::json!("corr-xyz");
    let err = validate_must_pass_lineage_metadata(&verdict, "must_pass_gate_verdict.json", now)
        .expect_err("correlation/run mismatch must fail closed");
    assert!(
        err.contains("must include run_id"),
        "expected correlation/run_id linkage detail: {err}"
    );
}

#[test]
fn must_pass_lineage_validator_fails_when_generated_at_stale() {
    let now = fixed_utc("2026-02-17T00:00:00Z");
    let verdict = must_pass_lineage_fixture("2026-02-09T23:59:59Z");
    let err = validate_must_pass_lineage_metadata(&verdict, "must_pass_gate_verdict.json", now)
        .expect_err("stale generated_at must fail closed");
    assert!(
        err.contains("stale"),
        "expected stale freshness detail in error: {err}"
    );
}

#[test]
fn ci_workflow_publishes_scenario_cell_gate_artifacts() {
    let workflow = std::fs::read_to_string(CI_WORKFLOW_PATH)
        .unwrap_or_else(|err| panic!("failed to read {CI_WORKFLOW_PATH}: {err}"));

    for token in [
        "PERF_SCENARIO_CELL_STATUS_JSON",
        "PERF_SCENARIO_CELL_STATUS_MD",
        "Publish scenario-cell gate status [linux]",
        "Upload scenario-cell gate artifacts [linux]",
        "scenario-cell-gate-${{ github.run_id }}-${{ github.run_attempt }}",
        "## Scenario Cell Gate Status",
        "Generate perf claim-integrity evidence bundle [linux]",
        "./scripts/perf/orchestrate.sh",
        "--profile ci",
        "PERF_BASELINE_CONFIDENCE_JSON",
        "PERF_EXTENSION_STRATIFICATION_JSON",
        "CLAIM_INTEGRITY_REQUIRED=1",
    ] {
        assert!(
            workflow.contains(token),
            "workflow must include scenario-cell gate token: {token}"
        );
    }
}

#[test]
fn run_all_wires_scenario_cell_status_artifacts_into_evidence_contract() {
    let script = std::fs::read_to_string(RUN_ALL_SCRIPT_PATH)
        .unwrap_or_else(|err| panic!("failed to read {RUN_ALL_SCRIPT_PATH}: {err}"));

    for token in [
        "CLAIM_INTEGRITY_REQUIRED",
        "PERF_BASELINE_CONFIDENCE_JSON",
        "PERF_EXTENSION_STRATIFICATION_JSON",
        "claim_integrity_scenario_cell_status.json",
        "claim_integrity_scenario_cell_status.md",
        "pi.claim_integrity.scenario_cell_status.v1",
        "\"claim_integrity_scenario_cells\"",
        "PERF_PHASE1_MATRIX_VALIDATION_JSON",
        "phase1_matrix_validation.json",
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
        "claim_integrity.phase1_matrix_cells_primary_e2e_metrics_present",
        "claim_integrity.phase1_matrix_regression_guards_object",
        "claim_integrity.phase1_matrix_regression_guards_required_fields",
        "claim_integrity.phase1_matrix_regression_guard_reasons_format",
        "claim_integrity.phase1_matrix_regression_guard_reasons_known",
        "claim_integrity.phase1_matrix_regression_guard_status_valid",
        "claim_integrity.phase1_matrix_regression_guard_reason_alignment",
        "claim_integrity.phase1_matrix_regression_guard_reason_set_exact",
        "claim_integrity.phase1_matrix_consumption_contract_object",
        "claim_integrity.phase1_matrix_downstream_beads_include_phase5",
        "claim_integrity.phase1_matrix_downstream_consumers_contract",
        "_regression_unverified",
        "failure_or_gap_reasons",
        "expected_regression_guard_reason_set",
        "claim_id=\"phase1_matrix_validation.matrix_cells.status\"",
        "metric_scope=\"matrix_cell_primary_e2e\"",
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
        "cherry_pick_guard.global_claim_valid must be true",
        "required_layers = [",
        "\"full_e2e_long_session\",",
        "extension_stratification.global_claim_valid",
        "claim_integrity.realistic_session_shape_coverage",
        "claim_integrity.microbench_only_claim",
        "claim_integrity.global_claim_missing_partition_coverage",
        "claim_integrity.unresolved_conflicting_claims",
        "claim_integrity.evidence_adjudication_matrix_schema",
        "claim_integrity_evidence_adjudication_matrix.json",
        "claim_integrity_evidence_adjudication_matrix.md",
        "pi.claim_integrity.evidence_adjudication_matrix.v1",
        "\"claim_integrity_adjudication_matrix\"",
        "\"source_record_stream\"",
        "\"source_workload_path\"",
        "missing_matrix_source_record",
        "missing_stage_metrics:",
        "\"source\"",
        "\"source_path\"",
        "phase1_matrix_validation",
        "benchmark_partitions",
        "realistic_long_session",
        "realistic_session_shape",
        "missing required realistic_session_shape coverage",
    ] {
        assert!(
            script.contains(token),
            "run_all.sh must include scenario-cell status token: {token}"
        );
    }
}

#[test]
fn qa_runbook_contains_perf3x_regression_triage_playbooks() {
    let runbook = std::fs::read_to_string(QA_RUNBOOK_PATH)
        .unwrap_or_else(|err| panic!("failed to read {QA_RUNBOOK_PATH}: {err}"));

    for token in [
        "## PERF-3X Regression Triage (bd-3ar8v.6.4)",
        "fail-closed artifact checks",
        "tests/full_suite_gate/perf3x_bead_coverage_audit.json",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "tests/perf/reports/budget_summary.json",
        "tests/perf/reports/perf_comparison.json",
        "tests/perf/reports/stress_triage.json",
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/reports/budget_events.jsonl",
        "tests/perf/reports/perf_comparison_events.jsonl",
        "tests/perf/reports/stress_events.jsonl",
        "tests/perf/reports/parameter_sweeps_events.jsonl",
    ] {
        assert!(
            runbook.contains(token),
            "qa runbook must include PERF-3X triage token: {token}"
        );
    }
}

#[test]
fn qa_runbook_contains_perf3x_final_go_no_go_workflow() {
    let runbook = std::fs::read_to_string(QA_RUNBOOK_PATH)
        .unwrap_or_else(|err| panic!("failed to read {QA_RUNBOOK_PATH}: {err}"));

    for token in [
        "### Final >=3x Go/No-Go Decision Workflow (bd-3ar8v.6.5)",
        "`NO-GO` (fail-closed)",
        "tests/perf/reports/opportunity_matrix.json",
        "tests/perf/reports/parameter_sweeps.json",
        "perf3x_bead_coverage = pass",
        "practical_finish_checkpoint = pass",
        "extension_remediation_backlog = pass",
        "opportunity_matrix_integrity = pass",
        "parameter_sweeps_integrity = pass",
    ] {
        assert!(
            runbook.contains(token),
            "qa runbook must include final go/no-go token: {token}"
        );
    }
}

#[test]
fn ci_operator_runbook_contains_perf3x_gate_incident_addendum() {
    let runbook = std::fs::read_to_string(CI_OPERATOR_RUNBOOK_PATH)
        .unwrap_or_else(|err| panic!("failed to read {CI_OPERATOR_RUNBOOK_PATH}: {err}"));

    for token in [
        "### PERF-3X Gate Incident Addendum (bd-3ar8v.6.4)",
        "Treat missing/stale PERF-3X artifacts as blocking failures",
        "tests/full_suite_gate/perf3x_bead_coverage_audit.json",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/reports/budget_events.jsonl",
        "tests/perf/reports/perf_comparison_events.jsonl",
        "tests/perf/reports/stress_events.jsonl",
        "tests/perf/reports/parameter_sweeps_events.jsonl",
        "docs/qa-runbook.md",
        "PERF-3X Regression Triage (bd-3ar8v.6.4)",
    ] {
        assert!(
            runbook.contains(token),
            "ci-operator runbook must include PERF-3X addendum token: {token}"
        );
    }
}
