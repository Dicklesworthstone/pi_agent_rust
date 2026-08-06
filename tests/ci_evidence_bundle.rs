//! Unified CI Evidence Bundle — collects all test artifacts into a single
//! structured bundle per CI run (bd-1f42.6.8).
//!
//! Produces:
//! - `tests/evidence_bundle/index.json` — machine-readable index with pointers
//!   to every section.
//! - `tests/evidence_bundle/bundle_report.md` — human-readable summary with
//!   pass/fail verdict for every section.
//! - `tests/evidence_bundle/events.jsonl` — JSONL event log of all collected
//!   artifacts.
//!
//! The bundle unifies:
//! 1. Extension conformance reports (summaries, baselines, gate verdicts)
//! 2. Extension diagnostics (dossiers, health delta, provider compat)
//! 3. E2E test results and transcripts
//! 4. Unit coverage summaries
//! 5. Quarantine audit trails
//! 6. Release gate verdicts
//! 7. Performance budgets
//! 8. Traceability matrices
//!
//! Run:
//!   cargo test --test `ci_evidence_bundle` -- --nocapture
//!
//! Regenerate the tracked bundle explicitly with:
//!   `PI_GENERATE_EVIDENCE_BUNDLE=1 cargo test --test ci_evidence_bundle build_evidence_bundle -- --exact --nocapture`

use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const GENERATE_EVIDENCE_BUNDLE_ENV: &str = "PI_GENERATE_EVIDENCE_BUNDLE";

fn evidence_bundle_generation_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn evidence_bundle_generation_requested() -> bool {
    evidence_bundle_generation_enabled(std::env::var(GENERATE_EVIDENCE_BUNDLE_ENV).ok().as_deref())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// A section in the evidence bundle.
#[derive(Debug, Clone, serde::Serialize)]
struct BundleSection {
    id: String,
    label: String,
    category: String,
    status: String, // "present", "missing", "invalid"
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<String>,
    file_count: usize,
    total_bytes: u64,
}

/// The full evidence bundle index.
#[derive(Debug, serde::Serialize)]
struct EvidenceBundle {
    schema: String,
    generated_at: String,
    git_ref: String,
    ci_run_id: String,
    sections: Vec<BundleSection>,
    summary: BundleSummary,
}

/// Summary statistics for the bundle.
#[derive(Debug, serde::Serialize)]
struct BundleSummary {
    total_sections: usize,
    present_sections: usize,
    missing_sections: usize,
    invalid_sections: usize,
    total_artifacts: usize,
    total_bytes: u64,
    verdict: String, // "complete", "partial", "insufficient"
}

/// Artifact source descriptor.
struct ArtifactSource {
    id: &'static str,
    label: &'static str,
    category: &'static str,
    path: &'static str,
    /// Expected schema identifier (if JSON with `schema` field).
    expected_schema: Option<&'static str>,
    /// If true, this is a directory and we count all files inside.
    is_directory: bool,
    /// If true, missing this artifact downgrades verdict.
    required: bool,
}

const PERF3X_LINEAGE_CONTRACT_SCHEMA: &str = "pi.perf3x.lineage_contract.v1";
const PERF3X_LINEAGE_CONTRACT_ARTIFACTS: &str = "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json | \
tests/ext_conformance/reports/conformance_summary.json | \
tests/perf/reports/stress_triage.json";
const PERF3X_LINEAGE_MAX_ARTIFACT_SPAN_DAYS: i64 = 14;
const PARAMETER_SWEEPS_MISSING_DIAGNOSTIC: &str = "parameter_sweeps artifact not found (expected tests/perf/reports, tests/perf/runs/results, or tests/e2e_results/*/results)";
const MUST_PASS_GATE_SCHEMA: &str = "pi.ext.must_pass_gate.v1";
const MUST_PASS_EVENT_SCHEMA: &str = "pi.ext.gate_event.v1";
const MUST_PASS_INCLUSION_PATH: &str = "docs/extension-inclusion-list.json";
const MUST_PASS_MANIFEST_PATH: &str = "tests/ext_conformance/VALIDATED_MANIFEST.json";
const MUST_PASS_VERDICT_PATH: &str =
    "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json";
const MUST_PASS_EVENTS_PATH: &str = "tests/ext_conformance/reports/gate/must_pass_events.jsonl";
const MUST_PASS_EVIDENCE_PATHS: &[&str] = &[MUST_PASS_VERDICT_PATH, MUST_PASS_EVENTS_PATH];
const MUST_PASS_ARTIFACTS_PATH: &str = "tests/ext_conformance/artifacts";
const EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1: usize = 208;
const MIN_CANONICAL_MUST_PASS_EXTENSIONS: usize = 200;

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

const ARTIFACT_SOURCES: &[ArtifactSource] = &[
    // ── Extension conformance ──
    ArtifactSource {
        id: "conformance_summary",
        label: "Extension conformance summary",
        category: "conformance",
        path: "tests/ext_conformance/reports/conformance_summary.json",
        expected_schema: Some("pi.ext.conformance_summary"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "conformance_baseline",
        label: "Conformance baseline",
        category: "conformance",
        path: "tests/ext_conformance/reports/conformance_baseline.json",
        expected_schema: Some("pi.ext.conformance_baseline"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "conformance_events",
        label: "Conformance event log",
        category: "conformance",
        path: "tests/ext_conformance/reports/conformance_events.jsonl",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "conformance_report_md",
        label: "Conformance report (Markdown)",
        category: "conformance",
        path: "tests/ext_conformance/reports/CONFORMANCE_REPORT.md",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "regression_verdict",
        label: "Regression gate verdict",
        category: "conformance",
        path: "tests/ext_conformance/reports/regression_verdict.json",
        expected_schema: Some("pi.conformance.regression_gate"),
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "conformance_trend",
        label: "Conformance trend data",
        category: "conformance",
        path: "tests/ext_conformance/reports/conformance_trend.jsonl",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    // ── Extension diagnostics ──
    ArtifactSource {
        id: "must_pass_gate",
        label: "Must-pass gate verdict",
        category: "diagnostics",
        path: MUST_PASS_VERDICT_PATH,
        expected_schema: Some("pi.ext.must_pass_gate"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "must_pass_gate_events",
        label: "Must-pass gate event log",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/gate/must_pass_events.jsonl",
        expected_schema: None,
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "failure_dossiers",
        label: "Per-extension failure dossiers",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/dossiers",
        expected_schema: None,
        is_directory: true,
        required: false,
    },
    ArtifactSource {
        id: "health_delta",
        label: "Health & regression delta report",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/health_delta",
        expected_schema: None,
        is_directory: true,
        required: true,
    },
    ArtifactSource {
        id: "provider_compat",
        label: "Provider compatibility matrix",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/provider_compat",
        expected_schema: None,
        is_directory: true,
        required: false,
    },
    ArtifactSource {
        id: "sharded_reports",
        label: "Sharded extension matrix reports",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/sharded",
        expected_schema: None,
        is_directory: true,
        required: false,
    },
    ArtifactSource {
        id: "journey_report",
        label: "Extension journey report",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/journeys/journey_report.json",
        expected_schema: Some("pi.ext.journey_report"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "auto_repair_summary",
        label: "Auto-repair summary",
        category: "diagnostics",
        path: "tests/ext_conformance/reports/auto_repair_summary.json",
        expected_schema: Some("pi.ext.auto_repair_summary"),
        is_directory: false,
        required: false,
    },
    // ── E2E results ──
    ArtifactSource {
        id: "e2e_results",
        label: "E2E test results",
        category: "e2e",
        path: "tests/e2e_results",
        expected_schema: None,
        is_directory: true,
        required: false,
    },
    // ── Quarantine ──
    ArtifactSource {
        id: "quarantine_report",
        label: "Quarantine report",
        category: "quarantine",
        path: "tests/quarantine_report.json",
        expected_schema: Some("pi.test.quarantine_report"),
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "quarantine_audit",
        label: "Quarantine audit trail",
        category: "quarantine",
        path: "tests/quarantine_audit.jsonl",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    // ── Performance ──
    ArtifactSource {
        id: "perf_budget_summary",
        label: "Performance budget summary",
        category: "performance",
        path: "tests/perf/reports/budget_summary.json",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "perf_comparison",
        label: "PERF-3X comparison report",
        category: "performance",
        path: "tests/perf/reports/perf_comparison.json",
        expected_schema: Some("pi.ext.perf_comparison"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "parameter_sweeps",
        label: "PERF-3X parameter sweeps report",
        category: "performance",
        path: "tests/perf/reports/parameter_sweeps.json",
        expected_schema: Some("pi.perf.parameter_sweeps"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "stress_triage",
        label: "PERF-3X stress triage report",
        category: "performance",
        path: "tests/perf/reports/stress_triage.json",
        expected_schema: Some("pi.ext.stress_triage"),
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "load_time_benchmark",
        label: "Extension load-time benchmark",
        category: "performance",
        path: "tests/ext_conformance/reports/load_time_benchmark.json",
        expected_schema: None,
        is_directory: false,
        required: false,
    },
    // ── Security & provenance ──
    ArtifactSource {
        id: "risk_review",
        label: "Security and licensing risk review",
        category: "security",
        path: "tests/ext_conformance/artifacts/RISK_REVIEW.json",
        expected_schema: None,
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "provenance_verification",
        label: "Extension provenance verification",
        category: "security",
        path: "tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json",
        expected_schema: None,
        is_directory: false,
        required: true,
    },
    // ── Traceability ──
    ArtifactSource {
        id: "traceability_matrix",
        label: "Requirement-to-test traceability matrix",
        category: "traceability",
        path: "docs/traceability_matrix.json",
        expected_schema: None,
        is_directory: false,
        required: true,
    },
    ArtifactSource {
        id: "high_value_suite_artifact_inventory",
        label: "High-value suite artifact inventory",
        category: "traceability",
        path: "docs/evidence/high-value-suite-artifact-inventory.json",
        expected_schema: Some("pi.traceability.high_value_suite_artifact_inventory.v1"),
        is_directory: false,
        required: true,
    },
    // ── Inventory ──
    ArtifactSource {
        id: "extension_inventory",
        label: "Extension inventory",
        category: "inventory",
        path: "tests/ext_conformance/reports/inventory.json",
        expected_schema: Some("pi.ext.inventory"),
        is_directory: false,
        required: false,
    },
    ArtifactSource {
        id: "inclusion_manifest",
        label: "Extension inclusion manifest",
        category: "inventory",
        path: "tests/ext_conformance/reports/inclusion_manifest",
        expected_schema: None,
        is_directory: true,
        required: false,
    },
];

/// Count files and total bytes in a directory recursively.
fn dir_stats(path: &Path) -> (usize, u64) {
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = entry.file_type();
            if ft.as_ref().is_ok_and(std::fs::FileType::is_dir) {
                let (c, b) = dir_stats(&entry.path());
                count += c;
                bytes += b;
            } else if ft.as_ref().is_ok_and(std::fs::FileType::is_file) {
                count += 1;
                bytes += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    (count, bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MustPassSourceBindings {
    git_commit: String,
    source_tree_sha256: String,
    inclusion_sha256: String,
    manifest_sha256: String,
    inclusion_contents: Vec<u8>,
    manifest_contents: Vec<u8>,
    tracked_paths: BTreeSet<String>,
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionList {
    schema: String,
    summary: AuthoritativeInclusionSummary,
    tier1: Vec<AuthoritativeInclusionEntry>,
    tier1_review: Vec<AuthoritativeInclusionEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionSummary {
    tier1_count: usize,
    tier1_review_count: usize,
    total_must_pass: usize,
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionEntry {
    id: String,
}

fn is_canonical_extension_id(id: &str) -> bool {
    !id.is_empty() && id.trim() == id && !id.chars().any(char::is_control)
}

const fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn authoritative_must_pass_ids(
    inclusion_contents: &[u8],
    expected_must_pass: usize,
) -> Result<BTreeSet<String>, String> {
    let inclusion: AuthoritativeInclusionList = serde_json::from_slice(inclusion_contents)
        .map_err(|err| format!("invalid {MUST_PASS_INCLUSION_PATH}: {err}"))?;
    if inclusion.schema != "pi.ext.inclusion_list.v1" {
        return Err(format!(
            "unexpected schema in {MUST_PASS_INCLUSION_PATH}: {}",
            inclusion.schema
        ));
    }

    let observed_tier1 = inclusion.tier1.len();
    let observed_review = inclusion.tier1_review.len();
    let observed_total = observed_tier1
        .checked_add(observed_review)
        .ok_or_else(|| format!("must-pass count overflow in {MUST_PASS_INCLUSION_PATH}"))?;
    if inclusion.summary.tier1_count != observed_tier1
        || inclusion.summary.tier1_review_count != observed_review
        || inclusion.summary.total_must_pass != observed_total
        || observed_total < MIN_CANONICAL_MUST_PASS_EXTENSIONS
        || observed_total != expected_must_pass
    {
        return Err(format!(
            "{MUST_PASS_INCLUSION_PATH} summary mismatch or unexpected versioned must-pass denominator: summary={}+{}={}, observed={observed_tier1}+{observed_review}={observed_total}, minimum={MIN_CANONICAL_MUST_PASS_EXTENSIONS}, expected={expected_must_pass}",
            inclusion.summary.tier1_count,
            inclusion.summary.tier1_review_count,
            inclusion.summary.total_must_pass,
        ));
    }

    let mut inclusion_ids = BTreeSet::new();
    for (section, entries) in [
        ("tier1", inclusion.tier1),
        ("tier1_review", inclusion.tier1_review),
    ] {
        for (index, entry) in entries.into_iter().enumerate() {
            if !is_canonical_extension_id(&entry.id) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} {section}[{index}] has a malformed id"
                ));
            }
            if !inclusion_ids.insert(entry.id.clone()) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} contains duplicate must-pass id {}",
                    entry.id
                ));
            }
        }
    }
    Ok(inclusion_ids)
}

fn validated_manifest_tiers(
    manifest_contents: &[u8],
    tracked_paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, u64>, String> {
    let manifest: Value = serde_json::from_slice(manifest_contents)
        .map_err(|err| format!("invalid {MUST_PASS_MANIFEST_PATH}: {err}"))?;
    if manifest.get("schema").and_then(Value::as_str) != Some("pi.ext.validated-manifest.v1") {
        return Err(format!(
            "unexpected schema in {MUST_PASS_MANIFEST_PATH}: {}",
            manifest
                .get("schema")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
        ));
    }
    let extensions = manifest
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{MUST_PASS_MANIFEST_PATH} missing extensions array"))?;

    let mut manifest_tiers = BTreeMap::new();
    let mut manifest_artifact_ids = BTreeMap::new();
    for (index, extension) in extensions.iter().enumerate() {
        let id = extension.get("id").and_then(Value::as_str).ok_or_else(|| {
            format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing string id")
        })?;
        if !is_canonical_extension_id(id) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} extensions[{index}] has a malformed id"
            ));
        }
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
        let entry_path = extension
            .get("entry_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing string entry_path")
            })?;
        let relative_path = Path::new(entry_path);
        if !is_canonical_extension_id(entry_path)
            || entry_path.contains('\\')
            || has_windows_drive_prefix(entry_path)
            || relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} extensions[{index}] has malformed entry_path"
            ));
        }
        let artifact_path = format!("{MUST_PASS_ARTIFACTS_PATH}/{entry_path}");
        if !tracked_paths.contains(&artifact_path) {
            return Err(format!(
                "manifest entry {id} points to artifact input not tracked by the canonical commit: {artifact_path}"
            ));
        }
        if let Some(first_id) = manifest_artifact_ids.insert(artifact_path.clone(), id.to_string())
        {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} reuses artifact entry_path {entry_path} for extension ids {first_id} and {id}"
            ));
        }
        if manifest_tiers.insert(id.to_string(), tier).is_some() {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} contains duplicate extension id {id}"
            ));
        }
    }
    Ok(manifest_tiers)
}

fn canonical_must_pass_entries(
    inclusion_contents: &[u8],
    manifest_contents: &[u8],
    tracked_paths: &BTreeSet<String>,
    expected_must_pass: usize,
) -> Result<BTreeMap<String, u64>, String> {
    let inclusion_ids = authoritative_must_pass_ids(inclusion_contents, expected_must_pass)?;
    let manifest_tiers = validated_manifest_tiers(manifest_contents, tracked_paths)?;
    let mut canonical_entries = BTreeMap::new();
    for id in inclusion_ids {
        let tier = manifest_tiers.get(&id).copied().ok_or_else(|| {
            format!(
                "canonical must-pass id {id} from {MUST_PASS_INCLUSION_PATH} is absent from {MUST_PASS_MANIFEST_PATH}"
            )
        })?;
        canonical_entries.insert(id, tier);
    }
    Ok(canonical_entries)
}

fn git_commit_file_contents(root: &Path, commit: &str, relative: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{commit}:{relative}")])
        .output()
        .map_err(|err| format!("failed to read canonical Git blob for {relative}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git show failed for canonical {relative} blob: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
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

fn validate_evidence_source_commit(
    root: &Path,
    source_commit: &str,
    current_commit: &str,
) -> Result<(), String> {
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("must-pass evidence git_commit is not a full commit ID".to_string());
    }
    let resolved = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ])
        .output()
        .map_err(|err| format!("failed to resolve must-pass evidence git_commit: {err}"))?;
    if !resolved.status.success() {
        return Err(format!(
            "must-pass evidence git_commit does not resolve to a commit: {source_commit}"
        ));
    }
    let resolved_commit = String::from_utf8(resolved.stdout)
        .map_err(|err| format!("git rev-parse returned non-UTF-8 output: {err}"))?;
    if !resolved_commit.trim().eq_ignore_ascii_case(source_commit) {
        return Err(format!(
            "must-pass evidence git_commit did not resolve exactly: expected {source_commit}, found {}",
            resolved_commit.trim()
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

    let commit_range = format!("{source_commit}..{current_commit}");
    let history = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            "--format=",
            "--name-only",
            "-z",
            "--no-renames",
            &commit_range,
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
            "must-pass evidence git_commit is followed by non-evidence changes: {}",
            disallowed.join(", ")
        ));
    }
    Ok(())
}

fn ensure_must_pass_paths_are_clean(
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
                    "must-pass source inputs differ in the {label}; commit them before collecting release evidence"
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

fn tracked_must_pass_source_records(
    root: &Path,
    commit: &str,
    source_paths: &[&str],
) -> Result<Vec<(String, String, String)>, String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "-z", "--full-tree", commit, "--"])
        .args(source_paths);
    let output = command
        .output()
        .map_err(|err| format!("failed to list committed must-pass source inputs: {err}"))?;
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
            .any(|(path, _, _)| path == required || path.starts_with(&prefix))
        {
            return Err(format!(
                "required must-pass source input is not tracked: {required}"
            ));
        }
    }
    Ok(parsed)
}

fn source_tree_sha256(records: &[(String, String, String)]) -> String {
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
    let git_commit = current_git_commit(root)?;
    ensure_must_pass_paths_are_clean(root, &git_commit, MUST_PASS_EVIDENCE_PATHS)?;
    let records = tracked_must_pass_source_records(root, &git_commit, MUST_PASS_EVIDENCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;

    let mut contents = BTreeMap::new();
    for (path, _, expected_blob) in &records {
        let full_path = root.join(path);
        let metadata = std::fs::symlink_metadata(&full_path).map_err(|err| {
            format!("failed to inspect committed must-pass evidence file {path}: {err}")
        })?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "committed must-pass evidence path {path} is not a regular worktree file"
            ));
        }
        let bytes = std::fs::read(&full_path)
            .map_err(|err| format!("failed to read committed must-pass evidence {path}: {err}"))?;
        if git_blob_oid(&bytes, expected_blob.len())? != *expected_blob {
            return Err(format!(
                "must-pass evidence worktree bytes differ from the HEAD blob for {path}"
            ));
        }
        contents.insert(path.clone(), bytes);
    }

    if current_git_commit(root)? != git_commit {
        return Err("must-pass evidence HEAD changed during snapshot capture".to_string());
    }
    ensure_must_pass_paths_are_clean(root, &git_commit, MUST_PASS_EVIDENCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;

    let verdict_contents = contents.remove(MUST_PASS_VERDICT_PATH).ok_or_else(|| {
        format!("required must-pass evidence is not tracked at HEAD: {MUST_PASS_VERDICT_PATH}")
    })?;
    let events_contents = contents.remove(MUST_PASS_EVENTS_PATH).ok_or_else(|| {
        format!("required must-pass evidence is not tracked at HEAD: {MUST_PASS_EVENTS_PATH}")
    })?;
    if !contents.is_empty() {
        return Err("unexpected paths in committed must-pass evidence snapshot".to_string());
    }

    Ok(CommittedMustPassEvidence {
        git_commit,
        verdict_contents,
        events_contents,
    })
}

fn current_must_pass_source_bindings(root: &Path) -> Result<MustPassSourceBindings, String> {
    let git_commit = current_git_commit(root)?;
    ensure_must_pass_paths_are_clean(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    let records = tracked_must_pass_source_records(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let inclusion_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_INCLUSION_PATH)?;
    let manifest_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_MANIFEST_PATH)?;
    if current_git_commit(root)? != git_commit {
        return Err("must-pass source HEAD changed while reading canonical blobs".to_string());
    }
    ensure_must_pass_paths_are_clean(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    Ok(MustPassSourceBindings {
        git_commit,
        source_tree_sha256: source_tree_sha256(&records),
        inclusion_sha256: format!("{:x}", Sha256::digest(&inclusion_contents)),
        manifest_sha256: format!("{:x}", Sha256::digest(&manifest_contents)),
        inclusion_contents,
        manifest_contents,
        tracked_paths: records.into_iter().map(|record| record.0).collect(),
    })
}

fn format_id_preview(ids: &BTreeSet<String>) -> String {
    let mut preview = ids.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if ids.len() > 5 {
        preview.push_str(", ...");
    }
    preview
}

struct ParsedGateEvent<'a> {
    set: &'a str,
    id: &'a str,
    tier: u64,
    status: &'a str,
}

fn parse_gate_event(event: &Value, line_number: usize) -> Result<ParsedGateEvent<'_>, String> {
    if event.get("schema").and_then(Value::as_str) != Some(MUST_PASS_EVENT_SCHEMA) {
        return Err(format!(
            "invalid event schema at {MUST_PASS_EVENTS_PATH}:{line_number}"
        ));
    }
    let required_string = |field: &str| {
        event.get(field).and_then(Value::as_str).ok_or_else(|| {
            format!("event at {MUST_PASS_EVENTS_PATH}:{line_number} is missing string {field}")
        })
    };
    let set = required_string("set")?;
    if !matches!(set, "must_pass" | "stretch") {
        return Err(format!(
            "event at {MUST_PASS_EVENTS_PATH}:{line_number} has unexpected set {set}"
        ));
    }
    let id = required_string("id")?;
    if !is_canonical_extension_id(id) {
        return Err(format!(
            "event at {MUST_PASS_EVENTS_PATH}:{line_number} has malformed id"
        ));
    }
    let tier = event.get("tier").and_then(Value::as_u64).ok_or_else(|| {
        format!("event at {MUST_PASS_EVENTS_PATH}:{line_number} is missing unsigned tier")
    })?;
    if !(1..=5).contains(&tier) {
        return Err(format!(
            "event at {MUST_PASS_EVENTS_PATH}:{line_number} has invalid tier {tier}"
        ));
    }
    let status = required_string("status")?;
    if !matches!(status, "pass" | "fail" | "skip") {
        return Err(format!(
            "event at {MUST_PASS_EVENTS_PATH}:{line_number} has unexpected status {status}"
        ));
    }
    Ok(ParsedGateEvent {
        set,
        id,
        tier,
        status,
    })
}

fn validate_gate_event_lineage(
    event: &Value,
    line_number: usize,
    bindings: &MustPassSourceBindings,
    evidence_commit: &str,
    run_id: &str,
    correlation_id: &str,
) -> Result<(), String> {
    for (field, expected_value) in [
        ("run_id", run_id),
        ("correlation_id", correlation_id),
        ("git_commit", evidence_commit),
        ("source_tree_sha256", bindings.source_tree_sha256.as_str()),
        ("inclusion_sha256", bindings.inclusion_sha256.as_str()),
        ("manifest_sha256", bindings.manifest_sha256.as_str()),
    ] {
        if event.get(field).and_then(Value::as_str) != Some(expected_value) {
            return Err(format!(
                "event {field} mismatch at {MUST_PASS_EVENTS_PATH}:{line_number}"
            ));
        }
    }
    Ok(())
}

fn validate_must_pass_events(
    contents: &str,
    expected: &BTreeMap<String, u64>,
    bindings: &MustPassSourceBindings,
    evidence_commit: &str,
    run_id: &str,
    correlation_id: &str,
) -> Result<(), String> {
    let mut observed = BTreeMap::new();
    let mut event_keys = BTreeSet::new();
    let mut row_count = 0_usize;

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        row_count += 1;
        let event: Value = serde_json::from_str(line).map_err(|err| {
            format!(
                "invalid JSONL row {} in {MUST_PASS_EVENTS_PATH}: {err}",
                line_index + 1
            )
        })?;
        let line_number = line_index + 1;
        let parsed = parse_gate_event(&event, line_number)?;
        validate_gate_event_lineage(
            &event,
            line_number,
            bindings,
            evidence_commit,
            run_id,
            correlation_id,
        )?;
        if !event_keys.insert((parsed.set.to_string(), parsed.id.to_string())) {
            return Err(format!(
                "duplicate {} event id {} in {MUST_PASS_EVENTS_PATH}",
                parsed.set, parsed.id
            ));
        }
        if parsed.set == "must_pass" {
            let expected_tier = expected.get(parsed.id).ok_or_else(|| {
                format!(
                    "unexpected must-pass event id {} in {MUST_PASS_EVENTS_PATH}",
                    parsed.id
                )
            })?;
            if parsed.status != "pass" {
                return Err(format!(
                    "non-pass must-pass event {} has status {} in {MUST_PASS_EVENTS_PATH}",
                    parsed.id, parsed.status
                ));
            }
            if parsed.tier != *expected_tier {
                return Err(format!(
                    "must-pass event tier mismatch for {}: observed {}, expected {expected_tier}",
                    parsed.id, parsed.tier
                ));
            }
            observed.insert(parsed.id.to_string(), parsed.tier);
        } else if expected.contains_key(parsed.id) {
            return Err(format!(
                "canonical must-pass id {} is incorrectly labeled stretch in {MUST_PASS_EVENTS_PATH}",
                parsed.id
            ));
        }
    }

    if row_count == 0 {
        return Err(format!("required {MUST_PASS_EVENTS_PATH} is empty"));
    }
    let expected_ids = expected.keys().cloned().collect::<BTreeSet<_>>();
    let observed_ids = observed.keys().cloned().collect::<BTreeSet<_>>();
    let missing = expected_ids
        .difference(&observed_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    let unexpected = observed_ids
        .difference(&expected_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    if observed.len() != expected.len() || !missing.is_empty() || !unexpected.is_empty() {
        return Err(format!(
            "must-pass events do not exactly match the canonical inclusion-list set: observed={}, expected={}, missing=[{}], unexpected=[{}]",
            observed.len(),
            expected.len(),
            format_id_preview(&missing),
            format_id_preview(&unexpected),
        ));
    }
    Ok(())
}

struct ValidatedMustPassCounts {
    total: u64,
    tested: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    pass_rate_pct: f64,
}

fn validate_must_pass_counts(
    observed: &serde_json::Map<String, Value>,
    canonical_total: u64,
) -> Result<ValidatedMustPassCounts, String> {
    let count = |field: &str| {
        observed
            .get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("must_pass_gate: missing observed.{field}"))
    };
    let counts = ValidatedMustPassCounts {
        total: count("must_pass_total")?,
        tested: count("must_pass_tested")?,
        passed: count("must_pass_passed")?,
        failed: count("must_pass_failed")?,
        skipped: count("must_pass_skipped")?,
        pass_rate_pct: observed
            .get("must_pass_pass_rate_pct")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                "must_pass_gate: missing observed.must_pass_pass_rate_pct".to_string()
            })?,
    };
    if counts.total != canonical_total {
        return Err(format!(
            "must_pass_gate: reported denominator {} does not equal authoritative inclusion-list total {canonical_total}",
            counts.total
        ));
    }
    if counts.passed.checked_add(counts.failed) != Some(counts.tested) {
        return Err(format!(
            "must_pass_gate: passed ({}) + failed ({}) must equal tested ({})",
            counts.passed, counts.failed, counts.tested
        ));
    }
    if counts.tested.checked_add(counts.skipped) != Some(counts.total) {
        return Err(format!(
            "must_pass_gate: tested ({}) + skipped ({}) must equal total ({})",
            counts.tested, counts.skipped, counts.total
        ));
    }
    if counts.passed != counts.total
        || counts.failed != 0
        || counts.skipped != 0
        || counts.pass_rate_pct.to_bits() != 100.0_f64.to_bits()
    {
        return Err(format!(
            "must_pass_gate: pass status requires 100% pass with zero failures and skips (total={}, passed={}, failed={}, skipped={}, rate={})",
            counts.total, counts.passed, counts.failed, counts.skipped, counts.pass_rate_pct
        ));
    }
    Ok(counts)
}

fn validate_must_pass_gate_payload_against_bindings_with_events(
    val: &Value,
    bindings: &MustPassSourceBindings,
    events_contents: &str,
    expected_evidence_commit: &str,
    expected_must_pass: usize,
) -> Result<Value, String> {
    if val.get("schema").and_then(Value::as_str) != Some(MUST_PASS_GATE_SCHEMA) {
        return Err(format!(
            "must_pass_gate: schema must be exactly {MUST_PASS_GATE_SCHEMA}"
        ));
    }
    let generated_at = val
        .get("generated_at")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "must_pass_gate: missing/empty generated_at".to_string())?;
    let run_id = val
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "must_pass_gate: missing/empty run_id".to_string())?;
    let correlation_id = val
        .get("correlation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "must_pass_gate: missing/empty correlation_id".to_string())?;
    if val.get("status").and_then(Value::as_str) != Some("pass") {
        return Err("must_pass_gate: release evidence must have pass status".to_string());
    }

    let evidence_commit = val.get("git_commit").and_then(Value::as_str).unwrap_or("");
    if evidence_commit != expected_evidence_commit {
        return Err(
            "must_pass_gate: git_commit does not match the accepted evidence source commit"
                .to_string(),
        );
    }

    for (field, expected_value) in [
        ("source_tree_sha256", bindings.source_tree_sha256.as_str()),
        ("inclusion_sha256", bindings.inclusion_sha256.as_str()),
        ("manifest_sha256", bindings.manifest_sha256.as_str()),
    ] {
        let observed = val.get(field).and_then(Value::as_str).unwrap_or("");
        if observed != expected_value {
            return Err(format!(
                "must_pass_gate: {field} does not match the current canonical Git input"
            ));
        }
    }

    let canonical_entries = canonical_must_pass_entries(
        &bindings.inclusion_contents,
        &bindings.manifest_contents,
        &bindings.tracked_paths,
        expected_must_pass,
    )?;
    let canonical_total = u64::try_from(canonical_entries.len())
        .map_err(|_| "must_pass_gate: canonical denominator does not fit u64".to_string())?;
    let observed = val
        .get("observed")
        .and_then(Value::as_object)
        .ok_or_else(|| "must_pass_gate: missing observed object".to_string())?;
    let counts = validate_must_pass_counts(observed, canonical_total)?;

    validate_must_pass_events(
        events_contents,
        &canonical_entries,
        bindings,
        evidence_commit,
        run_id,
        correlation_id,
    )?;

    Ok(serde_json::json!({
        "status": "pass",
        "must_pass_total": counts.total,
        "must_pass_tested": counts.tested,
        "must_pass_passed": counts.passed,
        "must_pass_failed": counts.failed,
        "must_pass_skipped": counts.skipped,
        "must_pass_pass_rate_pct": counts.pass_rate_pct,
        "run_id": run_id,
        "correlation_id": correlation_id,
        "git_commit": evidence_commit,
        "source_tree_sha256": bindings.source_tree_sha256,
        "inclusion_sha256": bindings.inclusion_sha256,
        "manifest_sha256": bindings.manifest_sha256,
        "generated_at": generated_at,
    }))
}

fn validate_must_pass_gate_payload_against_bindings(
    root: &Path,
    val: &Value,
    bindings: &MustPassSourceBindings,
    expected_evidence_commit: &str,
    expected_must_pass: usize,
) -> Result<Value, String> {
    let events_contents = std::fs::read_to_string(root.join(MUST_PASS_EVENTS_PATH))
        .map_err(|err| format!("failed to read required {MUST_PASS_EVENTS_PATH}: {err}"))?;
    validate_must_pass_gate_payload_against_bindings_with_events(
        val,
        bindings,
        &events_contents,
        expected_evidence_commit,
        expected_must_pass,
    )
}

fn validate_must_pass_gate_payload(root: &Path, val: &Value) -> Result<Value, String> {
    let evidence_before = capture_committed_must_pass_evidence(root)?;
    let committed_verdict: Value = serde_json::from_slice(&evidence_before.verdict_contents)
        .map_err(|err| format!("failed to parse committed {MUST_PASS_VERDICT_PATH}: {err}"))?;
    if &committed_verdict != val {
        return Err(format!(
            "validated {MUST_PASS_VERDICT_PATH} does not match the commit-bound artifact bytes"
        ));
    }
    let events_contents = std::str::from_utf8(&evidence_before.events_contents)
        .map_err(|err| format!("committed {MUST_PASS_EVENTS_PATH} is not UTF-8: {err}"))?;
    let before = current_must_pass_source_bindings(root)?;
    let evidence_commit = val.get("git_commit").and_then(Value::as_str).unwrap_or("");
    validate_evidence_source_commit(root, evidence_commit, &before.git_commit)?;
    let summary = validate_must_pass_gate_payload_against_bindings_with_events(
        val,
        &before,
        events_contents,
        evidence_commit,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )?;
    let after = current_must_pass_source_bindings(root)?;
    if before != after {
        return Err(
            "must-pass canonical Git inputs changed while collecting the evidence bundle"
                .to_string(),
        );
    }
    let evidence_after = capture_committed_must_pass_evidence(root)?;
    if evidence_before != evidence_after {
        return Err(
            "committed must-pass evidence changed while collecting the evidence bundle".to_string(),
        );
    }
    Ok(summary)
}

fn validate_perf_comparison_payload(val: &Value) -> Result<Value, String> {
    let generated_at = val
        .get("generated_at")
        .and_then(Value::as_str)
        .ok_or_else(|| "perf_comparison: missing generated_at".to_string())?;
    if generated_at.trim().is_empty() {
        return Err("perf_comparison: generated_at is empty".to_string());
    }

    let summary = val
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| "perf_comparison: missing summary object".to_string())?;
    let overall_verdict = summary
        .get("overall_verdict")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if overall_verdict.is_empty() {
        return Err("perf_comparison: summary.overall_verdict is missing/empty".to_string());
    }

    let faster_count = summary
        .get("faster_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "perf_comparison: missing summary.faster_count".to_string())?;
    let slower_count = summary
        .get("slower_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "perf_comparison: missing summary.slower_count".to_string())?;
    let comparable_count = summary
        .get("comparable_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "perf_comparison: missing summary.comparable_count".to_string())?;

    Ok(serde_json::json!({
        "generated_at": generated_at,
        "overall_verdict": overall_verdict,
        "faster_count": faster_count,
        "slower_count": slower_count,
        "comparable_count": comparable_count,
    }))
}

fn validate_parameter_sweeps_payload(val: &Value) -> Result<Value, String> {
    let generated_at = val
        .get("generated_at")
        .and_then(Value::as_str)
        .ok_or_else(|| "parameter_sweeps: missing generated_at".to_string())?;
    if generated_at.trim().is_empty() {
        return Err("parameter_sweeps: generated_at is empty".to_string());
    }

    let readiness = val
        .get("readiness")
        .and_then(Value::as_object)
        .ok_or_else(|| "parameter_sweeps: missing readiness object".to_string())?;
    let readiness_status = readiness
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "parameter_sweeps: missing readiness.status".to_string())?;
    if !matches!(readiness_status, "ready" | "blocked") {
        return Err(format!(
            "parameter_sweeps: readiness.status must be ready|blocked, got '{readiness_status}'"
        ));
    }

    let ready_for_phase5 = readiness
        .get("ready_for_phase5")
        .and_then(Value::as_bool)
        .ok_or_else(|| "parameter_sweeps: missing readiness.ready_for_phase5 bool".to_string())?;
    let blocking_reasons = readiness
        .get("blocking_reasons")
        .and_then(Value::as_array)
        .ok_or_else(|| "parameter_sweeps: missing readiness.blocking_reasons array".to_string())?;

    let source_identity = val
        .get("source_identity")
        .and_then(Value::as_object)
        .ok_or_else(|| "parameter_sweeps: missing source_identity object".to_string())?;
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if source_artifact.is_empty() {
        return Err(
            "parameter_sweeps: source_identity.source_artifact is missing/empty".to_string(),
        );
    }

    Ok(serde_json::json!({
        "generated_at": generated_at,
        "readiness_status": readiness_status,
        "ready_for_phase5": ready_for_phase5,
        "blocking_reasons_count": blocking_reasons.len(),
        "source_artifact": source_artifact,
    }))
}

fn missing_section(source: &ArtifactSource, diagnostics: &str) -> BundleSection {
    BundleSection {
        id: source.id.to_string(),
        label: source.label.to_string(),
        category: source.category.to_string(),
        status: "missing".to_string(),
        artifact_path: Some(source.path.to_string()),
        schema: None,
        summary: None,
        diagnostics: Some(diagnostics.to_string()),
        file_count: 0,
        total_bytes: 0,
    }
}

fn invalid_section(source: &ArtifactSource, diagnostics: &str) -> BundleSection {
    BundleSection {
        id: source.id.to_string(),
        label: source.label.to_string(),
        category: source.category.to_string(),
        status: "invalid".to_string(),
        artifact_path: Some(source.path.to_string()),
        schema: None,
        summary: None,
        diagnostics: Some(diagnostics.to_string()),
        file_count: 0,
        total_bytes: 0,
    }
}

fn ensure_repo_local_artifact_path(root: &Path, full_path: &Path) -> Result<(), String> {
    let relative = full_path.strip_prefix(root).map_err(|_| {
        format!(
            "artifact path {} is outside repository root {}",
            full_path.display(),
            root.display()
        )
    })?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err(format!(
                "artifact path {} is not a canonical repository-relative path",
                relative.display()
            ));
        };
        cursor.push(segment);
        let metadata = std::fs::symlink_metadata(&cursor).map_err(|err| {
            format!(
                "failed to inspect artifact path component {}: {err}",
                cursor.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "artifact path traverses symbolic link: {}",
                cursor.display()
            ));
        }
    }
    Ok(())
}

fn collect_directory_section(
    root: &Path,
    full_path: &Path,
    source: &ArtifactSource,
) -> BundleSection {
    let Ok(metadata) = std::fs::symlink_metadata(full_path) else {
        return missing_section(source, "Directory not found");
    };
    if !metadata.file_type().is_dir() {
        return if metadata.file_type().is_symlink() {
            invalid_section(
                source,
                "Directory artifact path must not be a symbolic link",
            )
        } else {
            missing_section(source, "Directory not found")
        };
    }
    if let Err(err) = ensure_repo_local_artifact_path(root, full_path) {
        return invalid_section(source, &err);
    }

    let (file_count, total_bytes) = dir_stats(full_path);
    BundleSection {
        id: source.id.to_string(),
        label: source.label.to_string(),
        category: source.category.to_string(),
        status: if file_count > 0 {
            "present".to_string()
        } else {
            "missing".to_string()
        },
        artifact_path: Some(source.path.to_string()),
        schema: None,
        summary: Some(serde_json::json!({
            "file_count": file_count,
            "total_bytes": total_bytes,
        })),
        diagnostics: None,
        file_count,
        total_bytes,
    }
}

#[derive(Debug, Default)]
struct JsonFileAnalysis {
    status: String,
    schema: Option<String>,
    summary: Option<Value>,
    diagnostics: Option<String>,
}

fn artifact_uses_json_schema(source: &ArtifactSource) -> bool {
    Path::new(source.path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
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

fn analyze_json_file(root: &Path, full_path: &Path, source: &ArtifactSource) -> JsonFileAnalysis {
    let Some(val) = load_json(full_path) else {
        return JsonFileAnalysis {
            status: "invalid".to_string(),
            diagnostics: Some("Failed to parse JSON".to_string()),
            ..JsonFileAnalysis::default()
        };
    };

    let mut analysis = JsonFileAnalysis {
        status: "present".to_string(),
        schema: val.get("schema").and_then(Value::as_str).map(String::from),
        ..JsonFileAnalysis::default()
    };

    if let Some(expected) = source.expected_schema {
        if let Some(actual) = analysis.schema.as_deref() {
            if !actual.starts_with(expected) {
                analysis.status = "invalid".to_string();
                analysis.diagnostics = Some(format!(
                    "Schema mismatch: expected prefix '{expected}', found '{actual}'"
                ));
            }
        } else {
            analysis.status = "invalid".to_string();
            analysis.diagnostics = Some(format!(
                "Missing schema field (expected prefix '{expected}')"
            ));
        }
    }

    if source.id == "must_pass_gate" {
        match validate_must_pass_gate_payload(root, &val) {
            Ok(payload) => {
                analysis.summary = Some(payload);
            }
            Err(err) => {
                analysis.status = "invalid".to_string();
                analysis.diagnostics = Some(err);
            }
        }
    } else if source.id == "perf_comparison" {
        match validate_perf_comparison_payload(&val) {
            Ok(payload) => {
                analysis.summary = Some(payload);
            }
            Err(err) => {
                analysis.status = "invalid".to_string();
                analysis.diagnostics = Some(err);
            }
        }
    } else if source.id == "parameter_sweeps" {
        match validate_parameter_sweeps_payload(&val) {
            Ok(payload) => {
                analysis.summary = Some(payload);
            }
            Err(err) => {
                analysis.status = "invalid".to_string();
                analysis.diagnostics = Some(err);
            }
        }
    } else {
        analysis.summary = extract_summary(&val, source.id);
    }

    analysis
}

fn collect_file_section(root: &Path, full_path: &Path, source: &ArtifactSource) -> BundleSection {
    let Ok(metadata) = std::fs::symlink_metadata(full_path) else {
        return missing_section(source, "File not found");
    };
    if !metadata.file_type().is_file() {
        return if metadata.file_type().is_symlink() {
            invalid_section(source, "File artifact path must not be a symbolic link")
        } else {
            missing_section(source, "File not found")
        };
    }
    if let Err(err) = ensure_repo_local_artifact_path(root, full_path) {
        return invalid_section(source, &err);
    }

    let file_size = metadata.len();
    let (status, schema, summary, diagnostics) = if artifact_uses_json_schema(source) {
        let analysis = analyze_json_file(root, full_path, source);
        (
            analysis.status,
            analysis.schema,
            analysis.summary,
            analysis.diagnostics,
        )
    } else {
        ("present".to_string(), None, None, None)
    };

    BundleSection {
        id: source.id.to_string(),
        label: source.label.to_string(),
        category: source.category.to_string(),
        status,
        artifact_path: Some(source.path.to_string()),
        schema,
        summary,
        diagnostics,
        file_count: 1,
        total_bytes: file_size,
    }
}

fn collect_parameter_sweeps_section(root: &Path, source: &ArtifactSource) -> BundleSection {
    let Some(full_path) = find_latest_parameter_sweeps(root) else {
        return missing_section(source, PARAMETER_SWEEPS_MISSING_DIAGNOSTIC);
    };

    let artifact_path = full_path.strip_prefix(root).map_or_else(
        |_| full_path.display().to_string(),
        |relative| relative.display().to_string(),
    );
    if let Err(err) = ensure_repo_local_artifact_path(root, &full_path) {
        let mut section = invalid_section(source, &err);
        section.artifact_path = Some(artifact_path);
        return section;
    }
    let file_size = std::fs::symlink_metadata(&full_path).map_or(0, |m| m.len());
    let analysis = analyze_json_file(root, &full_path, source);

    BundleSection {
        id: source.id.to_string(),
        label: source.label.to_string(),
        category: source.category.to_string(),
        status: analysis.status,
        artifact_path: Some(artifact_path),
        schema: analysis.schema,
        summary: analysis.summary,
        diagnostics: analysis.diagnostics,
        file_count: 1,
        total_bytes: file_size,
    }
}

/// Collect a section from an artifact source.
fn collect_section(root: &Path, source: &ArtifactSource) -> BundleSection {
    if source.id == "parameter_sweeps" {
        return collect_parameter_sweeps_section(root, source);
    }

    let full_path = root.join(source.path);

    if source.is_directory {
        collect_directory_section(root, &full_path, source)
    } else {
        collect_file_section(root, &full_path, source)
    }
}

/// Extract a lightweight summary from a JSON artifact for the bundle index.
fn extract_summary(val: &Value, section_id: &str) -> Option<Value> {
    match section_id {
        "conformance_summary" => {
            let counts = val.get("counts")?;
            Some(serde_json::json!({
                "total": counts.get("total"),
                "pass": counts.get("pass"),
                "fail": counts.get("fail"),
                "pass_rate_pct": val.get("pass_rate_pct"),
                "generated_at": val.get("generated_at"),
            }))
        }
        "conformance_baseline" => {
            let ec = val.get("extension_conformance")?;
            Some(serde_json::json!({
                "tested": ec.get("tested"),
                "passed": ec.get("passed"),
                "failed": ec.get("failed"),
                "pass_rate_pct": ec.get("pass_rate_pct"),
                "generated_at": val.get("generated_at"),
            }))
        }
        "regression_verdict" => Some(serde_json::json!({
            "status": val.get("status"),
            "effective_pass_rate_pct": val.get("effective_pass_rate_pct"),
        })),
        "quarantine_report" => Some(serde_json::json!({
            "active_count": val.get("active_count"),
            "expired_count": val.get("expired_count"),
        })),
        "perf_comparison" => {
            let summary = val.get("summary")?;
            Some(serde_json::json!({
                "generated_at": val.get("generated_at"),
                "overall_verdict": summary.get("overall_verdict"),
                "faster_count": summary.get("faster_count"),
                "slower_count": summary.get("slower_count"),
                "comparable_count": summary.get("comparable_count"),
            }))
        }
        "parameter_sweeps" => {
            let readiness = val.get("readiness")?;
            Some(serde_json::json!({
                "generated_at": val.get("generated_at"),
                "readiness_status": readiness.get("status"),
                "ready_for_phase5": readiness.get("ready_for_phase5"),
                "blocking_reasons_count": readiness.get("blocking_reasons").and_then(Value::as_array).map(Vec::len),
            }))
        }
        "stress_triage" => Some(serde_json::json!({
            "pass": val.get("pass"),
            "generated_at": val.get("generated_at"),
        })),
        "extension_inventory" => Some(serde_json::json!({
            "total_extensions": val.get("total_extensions"),
        })),
        _ => None,
    }
}

fn summary_string_field(
    sections: &[BundleSection],
    section_id: &str,
    field: &str,
) -> Result<String, String> {
    let section = sections
        .iter()
        .find(|section| section.id == section_id)
        .ok_or_else(|| format!("missing required section '{section_id}'"))?;
    if section.status != "present" {
        return Err(format!(
            "section '{section_id}' must be present, found status '{}'",
            section.status
        ));
    }
    let summary = section
        .summary
        .as_ref()
        .ok_or_else(|| format!("section '{section_id}' missing summary payload"))?;
    let value = summary
        .get(field)
        .and_then(Value::as_str)
        .map_or("", str::trim);
    if value.is_empty() {
        return Err(format!(
            "section '{section_id}' missing non-empty summary field '{field}'"
        ));
    }
    Ok(value.to_string())
}

fn summary_generated_at(
    sections: &[BundleSection],
    section_id: &str,
) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let generated_at = summary_string_field(sections, section_id, "generated_at")?;
    chrono::DateTime::parse_from_rfc3339(&generated_at)
        .map(|ts| ts.with_timezone(&chrono::Utc))
        .map_err(|err| {
            format!("section '{section_id}' has invalid generated_at '{generated_at}': {err}")
        })
}

fn validate_perf3x_lineage_contract(sections: &[BundleSection]) -> Result<Value, String> {
    let run_id = summary_string_field(sections, "must_pass_gate", "run_id")?;
    let correlation_id = summary_string_field(sections, "must_pass_gate", "correlation_id")?;
    if !correlation_id.contains(&run_id) {
        return Err(format!(
            "must_pass_gate correlation_id '{correlation_id}' must include run_id '{run_id}'"
        ));
    }

    let must_pass_generated_at = summary_generated_at(sections, "must_pass_gate")?;
    let conformance_generated_at = summary_generated_at(sections, "conformance_summary")?;
    let stress_generated_at = summary_generated_at(sections, "stress_triage")?;

    let oldest = [
        must_pass_generated_at,
        conformance_generated_at,
        stress_generated_at,
    ]
    .iter()
    .min()
    .copied()
    .expect("lineage timestamp set is non-empty");
    let newest = [
        must_pass_generated_at,
        conformance_generated_at,
        stress_generated_at,
    ]
    .iter()
    .max()
    .copied()
    .expect("lineage timestamp set is non-empty");

    let span = newest.signed_duration_since(oldest);
    if span > chrono::Duration::days(PERF3X_LINEAGE_MAX_ARTIFACT_SPAN_DAYS) {
        return Err(format!(
            "PERF-3X lineage span exceeds {PERF3X_LINEAGE_MAX_ARTIFACT_SPAN_DAYS} days \
             for run_id '{run_id}' (oldest={oldest}, newest={newest})"
        ));
    }

    Ok(serde_json::json!({
        "run_id": run_id,
        "correlation_id": correlation_id,
        "must_pass_generated_at": must_pass_generated_at.to_rfc3339(),
        "conformance_generated_at": conformance_generated_at.to_rfc3339(),
        "stress_generated_at": stress_generated_at.to_rfc3339(),
        "artifact_span_minutes": span.num_minutes(),
        "max_allowed_span_days": PERF3X_LINEAGE_MAX_ARTIFACT_SPAN_DAYS,
    }))
}

fn build_perf3x_lineage_section(sections: &[BundleSection]) -> BundleSection {
    match validate_perf3x_lineage_contract(sections) {
        Ok(summary) => BundleSection {
            id: "perf3x_lineage_contract".to_string(),
            label: "PERF-3X lineage coherence contract".to_string(),
            category: "performance".to_string(),
            status: "present".to_string(),
            artifact_path: Some(PERF3X_LINEAGE_CONTRACT_ARTIFACTS.to_string()),
            schema: Some(PERF3X_LINEAGE_CONTRACT_SCHEMA.to_string()),
            summary: Some(summary),
            diagnostics: None,
            file_count: 0,
            total_bytes: 0,
        },
        Err(err) => BundleSection {
            id: "perf3x_lineage_contract".to_string(),
            label: "PERF-3X lineage coherence contract".to_string(),
            category: "performance".to_string(),
            status: "invalid".to_string(),
            artifact_path: Some(PERF3X_LINEAGE_CONTRACT_ARTIFACTS.to_string()),
            schema: Some(PERF3X_LINEAGE_CONTRACT_SCHEMA.to_string()),
            summary: None,
            diagnostics: Some(err),
            file_count: 0,
            total_bytes: 0,
        },
    }
}

/// Build the unified evidence bundle.
///
/// Run with:
/// `cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture`
#[test]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn build_evidence_bundle() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let root = repo_root();
    let bundle_dir = root.join("tests").join("evidence_bundle");

    let git_ref = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());

    let ci_run_id = std::env::var("GITHUB_RUN_ID")
        .or_else(|_| std::env::var("CI_RUN_ID"))
        .unwrap_or_else(|_| format!("local-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));

    eprintln!("\n=== Unified CI Evidence Bundle (bd-1f42.6.8) ===");
    eprintln!("  Git ref:    {git_ref}");
    eprintln!("  CI run:     {ci_run_id}");
    eprintln!("  Bundle dir: {}", bundle_dir.display());
    eprintln!();

    // ── Collect all sections ──
    let mut sections: Vec<BundleSection> = Vec::new();

    for source in ARTIFACT_SOURCES {
        eprint!("  [{:.<40}] ", source.label);
        let section = collect_section(&root, source);
        match section.status.as_str() {
            "present" => eprintln!(
                "PRESENT  ({} file(s), {} bytes)",
                section.file_count, section.total_bytes
            ),
            "invalid" => eprintln!("INVALID  {}", section.diagnostics.as_deref().unwrap_or("")),
            _ => eprintln!("MISSING"),
        }
        sections.push(section);
    }

    let perf3x_lineage_section = build_perf3x_lineage_section(&sections);
    eprint!("  [{:.<40}] ", perf3x_lineage_section.label);
    match perf3x_lineage_section.status.as_str() {
        "present" => eprintln!("PRESENT"),
        "invalid" => eprintln!(
            "INVALID  {}",
            perf3x_lineage_section.diagnostics.as_deref().unwrap_or("")
        ),
        status => eprintln!("{status}"),
    }
    let lineage_failed = perf3x_lineage_section.status == "invalid";
    sections.push(perf3x_lineage_section);

    // ── Compute summary ──
    let present = sections.iter().filter(|s| s.status == "present").count();
    let missing = sections.iter().filter(|s| s.status == "missing").count();
    let invalid = sections.iter().filter(|s| s.status == "invalid").count();
    let total_artifacts: usize = sections.iter().map(|s| s.file_count).sum();
    let total_bytes: u64 = sections.iter().map(|s| s.total_bytes).sum();

    let required_present = ARTIFACT_SOURCES
        .iter()
        .zip(sections.iter())
        .filter(|(src, sec)| src.required && sec.status == "present")
        .count();
    let required_total = ARTIFACT_SOURCES.iter().filter(|s| s.required).count();

    let verdict = if lineage_failed {
        "insufficient"
    } else if required_present == required_total && invalid == 0 {
        "complete"
    } else if required_present > 0 {
        "partial"
    } else {
        "insufficient"
    };

    let bundle = EvidenceBundle {
        schema: "pi.ci.evidence_bundle.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        git_ref: git_ref.clone(),
        ci_run_id: ci_run_id.clone(),
        sections: sections.clone(),
        summary: BundleSummary {
            total_sections: sections.len(),
            present_sections: present,
            missing_sections: missing,
            invalid_sections: invalid,
            total_artifacts,
            total_bytes,
            verdict: verdict.to_string(),
        },
    };

    // Render every output during ordinary tests so serialization and report
    // construction remain covered. Writing the tracked bundle is an explicit
    // maintainer operation because its timestamp, CI run ID, and collected
    // evidence inventory are inherently run-specific.
    let index_path = bundle_dir.join("index.json");
    let index_json =
        serde_json::to_string_pretty(&bundle).expect("serialize unified evidence bundle");

    // ── Write events.jsonl ──
    let events_path = bundle_dir.join("events.jsonl");
    let mut event_lines: Vec<String> = Vec::new();
    for section in &sections {
        let line = serde_json::json!({
            "schema": "pi.ci.evidence_bundle_event.v1",
            "section_id": section.id,
            "category": section.category,
            "status": section.status,
            "file_count": section.file_count,
            "total_bytes": section.total_bytes,
            "artifact_path": section.artifact_path,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines.push(serde_json::to_string(&line).expect("serialize evidence bundle event"));
    }
    let events_jsonl = event_lines.join("\n") + "\n";

    // ── Write bundle_report.md ──
    let mut md = String::new();
    md.push_str("# Unified CI Evidence Bundle\n\n");
    writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    )
    .expect("render evidence bundle timestamp");
    writeln!(md, "> Git ref: {git_ref}").expect("render evidence bundle Git ref");
    writeln!(md, "> CI run: {ci_run_id}").expect("render evidence bundle CI run ID");
    writeln!(md, "> Verdict: **{}**\n", verdict.to_uppercase())
        .expect("render evidence bundle verdict");

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    writeln!(md, "| Total sections | {} |", sections.len())
        .expect("render evidence bundle total sections");
    writeln!(md, "| Present | {present} |").expect("render evidence bundle present count");
    writeln!(md, "| Missing | {missing} |").expect("render evidence bundle missing count");
    writeln!(md, "| Invalid | {invalid} |").expect("render evidence bundle invalid count");
    writeln!(md, "| Total artifacts | {total_artifacts} |")
        .expect("render evidence bundle artifact count");
    writeln!(md, "| Total size | {:.1} KB |", total_bytes as f64 / 1024.0)
        .expect("render evidence bundle total size");
    writeln!(
        md,
        "| Required present | {required_present}/{required_total} |"
    )
    .expect("render evidence bundle required count");
    md.push('\n');

    // Group by category.
    let categories: Vec<&str> = {
        let mut cats = Vec::new();
        for section in &sections {
            let category = section.category.as_str();
            if !cats.contains(&category) {
                cats.push(category);
            }
        }
        cats
    };

    for cat in &categories {
        let cat_sections: Vec<&BundleSection> =
            sections.iter().filter(|s| s.category == *cat).collect();

        writeln!(md, "## {} ({})\n", capitalize(cat), cat_sections.len())
            .expect("render evidence bundle category heading");
        md.push_str(
            "| Section | Status | Files | Size | Path |\n|---------|--------|-------|------|------|\n",
        );
        for s in &cat_sections {
            let status_icon = match s.status.as_str() {
                "present" => "PASS",
                "invalid" => "WARN",
                _ => "MISS",
            };
            writeln!(
                md,
                "| {} | {} | {} | {} B | `{}` |",
                s.label,
                status_icon,
                s.file_count,
                s.total_bytes,
                s.artifact_path.as_deref().unwrap_or("-"),
            )
            .expect("render evidence bundle section row");
        }
        md.push('\n');
    }

    // Failures section for quick navigation.
    let failures: Vec<&BundleSection> = sections
        .iter()
        .filter(|s| s.status == "missing" || s.status == "invalid")
        .collect();
    if !failures.is_empty() {
        md.push_str("## Missing / Invalid Sections\n\n");
        for s in &failures {
            let required_marker = if ARTIFACT_SOURCES
                .iter()
                .any(|src| src.id == s.id && src.required)
            {
                " **(REQUIRED)**"
            } else {
                ""
            };
            writeln!(
                md,
                "- **{}** ({}): {}{}\n  Path: `{}`",
                s.label,
                s.status,
                s.diagnostics.as_deref().unwrap_or(""),
                required_marker,
                s.artifact_path.as_deref().unwrap_or("-"),
            )
            .expect("render evidence bundle failure row");
        }
        md.push('\n');
    }

    let md_path = bundle_dir.join("bundle_report.md");
    let generate = evidence_bundle_generation_requested();
    if generate {
        std::fs::create_dir_all(&bundle_dir).expect("create evidence bundle directory");
        std::fs::write(&index_path, index_json).expect("write evidence bundle index");
        std::fs::write(&events_path, events_jsonl).expect("write evidence bundle events");
        std::fs::write(&md_path, &md).expect("write evidence bundle Markdown report");
    } else {
        eprintln!(
            "  Bundle not written; set {GENERATE_EVIDENCE_BUNDLE_ENV}=1 to regenerate the tracked artifacts"
        );
    }

    // ── Print summary ──
    eprintln!("\n=== Evidence Bundle Summary ===");
    eprintln!("  Verdict:    {}", verdict.to_uppercase());
    eprintln!("  Sections:   {present}/{} present", sections.len());
    eprintln!("  Missing:    {missing}");
    eprintln!("  Invalid:    {invalid}");
    eprintln!("  Artifacts:  {total_artifacts} files");
    eprintln!("  Size:       {:.1} KB", total_bytes as f64 / 1024.0);
    eprintln!("  Required:   {required_present}/{required_total}");
    eprintln!();
    eprintln!(
        "  Reports ({}):",
        if generate { "generated" } else { "not written" }
    );
    eprintln!("    Index: {}", index_path.display());
    eprintln!("    JSONL: {}", events_path.display());
    eprintln!("    MD:    {}", md_path.display());
    eprintln!();
}

#[test]
fn evidence_bundle_generation_requires_exact_one() {
    assert!(!evidence_bundle_generation_enabled(None));
    assert!(!evidence_bundle_generation_enabled(Some("")));
    assert!(!evidence_bundle_generation_enabled(Some("0")));
    assert!(!evidence_bundle_generation_enabled(Some("true")));
    assert!(!evidence_bundle_generation_enabled(Some(" 1")));
    assert!(!evidence_bundle_generation_enabled(Some("1 ")));
    assert!(evidence_bundle_generation_enabled(Some("1")));
}

/// Verify the evidence bundle index has the correct structure.
#[test]
fn evidence_bundle_index_schema() {
    let bundle_path = repo_root()
        .join("tests")
        .join("evidence_bundle")
        .join("index.json");

    // Bundle may not exist yet on first run; skip gracefully.
    let Some(val) = load_json(&bundle_path) else {
        eprintln!(
            "  SKIP: Bundle index not found at {}. Run build_evidence_bundle first.",
            bundle_path.display()
        );
        return;
    };

    // Validate schema.
    assert_eq!(
        val.get("schema").and_then(Value::as_str),
        Some("pi.ci.evidence_bundle.v1"),
        "Bundle index must have schema pi.ci.evidence_bundle.v1"
    );

    // Must have sections array.
    let sections = val
        .get("sections")
        .and_then(Value::as_array)
        .expect("Bundle must have sections array");
    assert!(
        !sections.is_empty(),
        "Bundle must have at least one section"
    );

    // Each section must have required fields.
    for section in sections {
        assert!(
            section.get("id").and_then(Value::as_str).is_some(),
            "Section missing id"
        );
        assert!(
            section.get("status").and_then(Value::as_str).is_some(),
            "Section missing status"
        );
        assert!(
            section.get("category").and_then(Value::as_str).is_some(),
            "Section missing category"
        );
    }

    // Must have summary.
    let summary = val.get("summary").expect("Bundle must have summary");
    assert!(
        summary.get("verdict").and_then(Value::as_str).is_some(),
        "Summary must have verdict"
    );
    assert!(
        summary.get("total_sections").is_some(),
        "Summary must have total_sections"
    );
}

/// Verify that every failing section in the bundle points to a precise path.
#[test]
fn evidence_bundle_failures_have_paths() {
    let bundle_path = repo_root()
        .join("tests")
        .join("evidence_bundle")
        .join("index.json");

    let Some(val) = load_json(&bundle_path) else {
        eprintln!("  SKIP: Bundle not found. Run build_evidence_bundle first.");
        return;
    };

    let sections = val
        .get("sections")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .clone();

    for section in &sections {
        let status = section.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "missing" || status == "invalid" {
            let has_path = section
                .get("artifact_path")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty());
            assert!(
                has_path,
                "Failing section {:?} must have artifact_path",
                section.get("id")
            );
        }
    }
}

#[test]
fn must_pass_gate_source_is_required_json_verdict_file() {
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "must_pass_gate")
        .expect("must_pass_gate source must exist");
    assert!(
        !source.is_directory,
        "must_pass_gate must target a JSON verdict artifact, not a directory"
    );
    assert!(
        source.path.ends_with("must_pass_gate_verdict.json"),
        "must_pass_gate path must target must_pass_gate_verdict.json"
    );
    assert!(
        source.required,
        "must_pass_gate should be required for complete evidence bundles"
    );
}

#[test]
fn must_pass_gate_events_source_is_required_jsonl_file() {
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "must_pass_gate_events")
        .expect("must_pass_gate_events source must exist");
    assert!(!source.is_directory);
    assert_eq!(source.path, MUST_PASS_EVENTS_PATH);
    assert!(
        source.required,
        "exact per-extension event coverage is required for a complete evidence bundle"
    );
}

#[test]
fn canonical_must_pass_contract_currently_maps_exactly_208_entries() {
    let root = repo_root();
    let inclusion =
        std::fs::read(root.join(MUST_PASS_INCLUSION_PATH)).expect("read canonical inclusion list");
    let manifest =
        std::fs::read(root.join(MUST_PASS_MANIFEST_PATH)).expect("read canonical manifest");
    let manifest_value: Value =
        serde_json::from_slice(&manifest).expect("parse canonical validated manifest");
    let entry_paths: Vec<String> = manifest_value["extensions"]
        .as_array()
        .expect("canonical manifest extensions array")
        .iter()
        .map(|extension| {
            extension["entry_path"]
                .as_str()
                .expect("canonical manifest entry_path")
                .to_string()
        })
        .collect();
    let tracked_paths = entry_paths
        .iter()
        .map(|entry_path| format!("{MUST_PASS_ARTIFACTS_PATH}/{entry_path}"))
        .collect();
    let entries = canonical_must_pass_entries(
        &inclusion,
        &manifest,
        &tracked_paths,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )
    .expect("map every authoritative inclusion-list ID to one manifest tier");
    assert_eq!(
        entries.len(),
        208,
        "unexpected authoritative must-pass denominator"
    );
    for entry_path in entry_paths {
        assert!(
            root.join(MUST_PASS_ARTIFACTS_PATH)
                .join(&entry_path)
                .is_file(),
            "canonical artifact is missing: {entry_path}"
        );
    }
}

#[test]
fn perf_comparison_source_is_required_json_artifact() {
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "perf_comparison")
        .expect("perf_comparison source must exist");
    assert!(
        !source.is_directory,
        "perf_comparison source must target a JSON artifact"
    );
    assert!(
        source.path.ends_with("perf_comparison.json"),
        "perf_comparison source must point to perf_comparison.json"
    );
    assert!(
        source.required,
        "perf_comparison source should be required for PERF-3X evidence completeness"
    );
}

#[test]
fn parameter_sweeps_source_is_required_json_artifact() {
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "parameter_sweeps")
        .expect("parameter_sweeps source must exist");
    assert!(
        !source.is_directory,
        "parameter_sweeps source must target a JSON artifact"
    );
    assert!(
        source.path.ends_with("parameter_sweeps.json"),
        "parameter_sweeps source must point to parameter_sweeps.json"
    );
    assert!(
        source.required,
        "parameter_sweeps source should be required for PERF-3X evidence completeness"
    );
}

#[test]
fn full_cert_diagnostics_are_required_for_complete_verdict() {
    let health_delta = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "health_delta")
        .expect("health_delta source must exist");
    assert!(
        health_delta.required,
        "health_delta should be required for complete evidence bundles"
    );

    let journey_report = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "journey_report")
        .expect("journey_report source must exist");
    assert!(
        journey_report.required,
        "journey_report should be required for complete evidence bundles"
    );
}

#[derive(Clone)]
struct MustPassValidationFixture {
    root: PathBuf,
    payload: Value,
    bindings: MustPassSourceBindings,
    events: Vec<Value>,
}

fn run_evidence_binding_fixture_git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run Git for committed-evidence fixture");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn committed_evidence_binding_fixture(
    label: &str,
    attributes: Option<&str>,
    track_events: bool,
) -> PathBuf {
    let root = unique_temp_root(label);
    std::fs::create_dir_all(&root).expect("create committed-evidence fixture root");
    run_evidence_binding_fixture_git(&root, &["init", "-q"]);
    let verdict_path = root.join(MUST_PASS_VERDICT_PATH);
    let events_path = root.join(MUST_PASS_EVENTS_PATH);
    std::fs::create_dir_all(verdict_path.parent().expect("verdict fixture parent"))
        .expect("create committed-evidence fixture directory");
    std::fs::write(&verdict_path, "{}\n").expect("write verdict fixture");
    std::fs::write(&events_path, "{}\n").expect("write events fixture");
    if let Some(contents) = attributes {
        std::fs::write(root.join(".gitattributes"), contents)
            .expect("write evidence attributes fixture");
        run_evidence_binding_fixture_git(&root, &["add", ".gitattributes"]);
    }
    run_evidence_binding_fixture_git(&root, &["add", MUST_PASS_VERDICT_PATH]);
    if track_events {
        run_evidence_binding_fixture_git(&root, &["add", MUST_PASS_EVENTS_PATH]);
    }
    run_evidence_binding_fixture_git(
        &root,
        &[
            "-c",
            "user.name=Pi Evidence Fixture",
            "-c",
            "user.email=pi-evidence@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );
    root
}

#[test]
fn committed_must_pass_evidence_rejects_untracked_staged_and_worktree_inputs() {
    let untracked = committed_evidence_binding_fixture("untracked-evidence", None, false);
    let error = capture_committed_must_pass_evidence(&untracked)
        .expect_err("an untracked event log must fail closed");
    assert!(
        error.contains("untracked files") || error.contains("not tracked"),
        "{error}"
    );

    let staged = committed_evidence_binding_fixture("staged-evidence", None, true);
    capture_committed_must_pass_evidence(&staged)
        .expect("clean committed evidence must be accepted");
    std::fs::write(staged.join(MUST_PASS_VERDICT_PATH), "{\"staged\":true}\n")
        .expect("write staged verdict drift");
    run_evidence_binding_fixture_git(&staged, &["add", MUST_PASS_VERDICT_PATH]);
    let error = capture_committed_must_pass_evidence(&staged)
        .expect_err("staged evidence drift must fail closed");
    assert!(error.contains("differ in the index"), "{error}");

    let worktree = committed_evidence_binding_fixture("worktree-evidence", None, true);
    std::fs::write(
        worktree.join(MUST_PASS_EVENTS_PATH),
        "{\"worktree\":true}\n",
    )
    .expect("write worktree event drift");
    let error = capture_committed_must_pass_evidence(&worktree)
        .expect_err("unstaged evidence drift must fail closed");
    assert!(error.contains("differ in the worktree"), "{error}");
}

#[test]
fn committed_must_pass_evidence_rejects_flags_and_filter_hidden_bytes() {
    for (label, flag, path) in [
        (
            "assume-unchanged-evidence",
            "--assume-unchanged",
            MUST_PASS_VERDICT_PATH,
        ),
        (
            "skip-worktree-evidence",
            "--skip-worktree",
            MUST_PASS_EVENTS_PATH,
        ),
    ] {
        let flagged = committed_evidence_binding_fixture(label, None, true);
        run_evidence_binding_fixture_git(&flagged, &["update-index", flag, path]);
        let error = capture_committed_must_pass_evidence(&flagged)
            .expect_err("non-canonical evidence index flags must fail closed");
        assert!(error.contains("index flags"), "{flag}: {error}");
    }

    let filtered = committed_evidence_binding_fixture(
        "filtered-evidence",
        Some("*.json text eol=lf\n*.jsonl text eol=lf\n"),
        true,
    );
    std::fs::write(filtered.join(MUST_PASS_EVENTS_PATH), "{}\r\n")
        .expect("write filter-hidden event drift");
    let diff_status = std::process::Command::new("git")
        .arg("-C")
        .arg(&filtered)
        .args(["diff", "--quiet", "--", MUST_PASS_EVENTS_PATH])
        .status()
        .expect("check filter-hidden evidence fixture");
    assert!(
        diff_status.success(),
        "fixture must demonstrate evidence drift hidden by Git clean filtering"
    );
    let error = capture_committed_must_pass_evidence(&filtered)
        .expect_err("raw evidence byte drift must fail closed");
    assert!(error.contains("worktree bytes differ"), "{error}");
}

#[test]
fn evidence_source_commit_rejects_reverted_non_evidence_followup() {
    let temp = tempfile::tempdir().expect("create evidence-history fixture");
    let root = temp.path();
    run_evidence_binding_fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
    let commit = |message: &str| {
        run_evidence_binding_fixture_git(
            root,
            &[
                "-c",
                "user.name=Pi Evidence Fixture",
                "-c",
                "user.email=pi-evidence@example.invalid",
                "commit",
                "--quiet",
                "--message",
                message,
            ],
        );
        current_git_commit(root).expect("resolve evidence-history fixture commit")
    };

    std::fs::write(root.join("source.txt"), "tested source\n")
        .expect("write evidence-history source fixture");
    run_evidence_binding_fixture_git(root, &["add", "source.txt"]);
    let source_commit = commit("Add tested source");

    std::fs::create_dir_all(root.join("tests/evidence_bundle"))
        .expect("create evidence-only fixture directory");
    std::fs::write(root.join("tests/evidence_bundle/index.json"), "{}\n")
        .expect("write evidence-only fixture");
    run_evidence_binding_fixture_git(root, &["add", "tests/evidence_bundle/index.json"]);
    let evidence_commit = commit("Add evidence only");
    validate_evidence_source_commit(root, &source_commit, &evidence_commit)
        .expect("evidence-only descendant must be accepted");

    std::fs::write(root.join("source.txt"), "temporary change\n")
        .expect("change non-evidence source fixture");
    run_evidence_binding_fixture_git(root, &["add", "source.txt"]);
    commit("Temporarily change source");
    std::fs::write(root.join("source.txt"), "tested source\n")
        .expect("restore non-evidence source fixture");
    run_evidence_binding_fixture_git(root, &["add", "source.txt"]);
    let reverted_commit = commit("Restore source bytes");

    let error = validate_evidence_source_commit(root, &source_commit, &reverted_commit)
        .expect_err("reverted non-evidence history must still invalidate stale evidence");
    assert!(error.contains("non-evidence changes"), "{error}");
}

fn write_must_pass_fixture_events(root: &Path, events: &[Value]) {
    let path = root.join(MUST_PASS_EVENTS_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create must-pass event fixture directory");
    }
    let mut contents = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize must-pass event fixture"))
        .collect::<Vec<_>>()
        .join("\n");
    contents.push('\n');
    std::fs::write(path, contents).expect("write must-pass event fixture");
}

fn must_pass_validation_fixture() -> MustPassValidationFixture {
    const CURRENT_MUST_PASS_TOTAL: usize = 208;
    const CURRENT_TIER1_TOTAL: usize = 184;

    let root = unique_temp_root("must-pass-validation");
    let entries = (0..CURRENT_MUST_PASS_TOTAL)
        .map(|index| {
            (
                format!("extension-{index:03}"),
                u64::try_from(index % 5 + 1).expect("fixture tier fits u64"),
            )
        })
        .collect::<Vec<_>>();
    let inclusion_entry = |(id, _): &(String, u64)| serde_json::json!({"id": id});
    let inclusion = serde_json::json!({
        "schema": "pi.ext.inclusion_list.v1",
        "summary": {
            "tier1_count": CURRENT_TIER1_TOTAL,
            "tier1_review_count": CURRENT_MUST_PASS_TOTAL - CURRENT_TIER1_TOTAL,
            "total_must_pass": CURRENT_MUST_PASS_TOTAL
        },
        "tier1": entries[..CURRENT_TIER1_TOTAL]
            .iter()
            .map(inclusion_entry)
            .collect::<Vec<_>>(),
        "tier1_review": entries[CURRENT_TIER1_TOTAL..]
            .iter()
            .map(inclusion_entry)
            .collect::<Vec<_>>()
    });
    let manifest = serde_json::json!({
        "schema": "pi.ext.validated-manifest.v1",
        "extensions": entries
            .iter()
            .map(|(id, tier)| serde_json::json!({
                "id": id,
                "entry_path": format!("{id}.js"),
                "conformance_tier": tier
            }))
            .collect::<Vec<_>>()
    });
    let inclusion_contents =
        serde_json::to_vec(&inclusion).expect("serialize inclusion-list fixture");
    let manifest_contents = serde_json::to_vec(&manifest).expect("serialize manifest fixture");
    let bindings = MustPassSourceBindings {
        git_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        source_tree_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .to_string(),
        inclusion_sha256: format!("{:x}", Sha256::digest(&inclusion_contents)),
        manifest_sha256: format!("{:x}", Sha256::digest(&manifest_contents)),
        inclusion_contents,
        manifest_contents,
        tracked_paths: entries
            .iter()
            .map(|(id, _)| format!("{MUST_PASS_ARTIFACTS_PATH}/{id}.js"))
            .collect(),
    };
    let payload = serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "generated_at": "2026-08-04T12:00:00.000Z",
        "run_id": "ci-123",
        "correlation_id": "must-pass-gate-ci-123",
        "git_commit": bindings.git_commit,
        "source_tree_sha256": bindings.source_tree_sha256,
        "inclusion_sha256": bindings.inclusion_sha256,
        "manifest_sha256": bindings.manifest_sha256,
        "status": "pass",
        "observed": {
            "must_pass_total": CURRENT_MUST_PASS_TOTAL,
            "must_pass_tested": CURRENT_MUST_PASS_TOTAL,
            "must_pass_passed": CURRENT_MUST_PASS_TOTAL,
            "must_pass_failed": 0,
            "must_pass_skipped": 0,
            "must_pass_pass_rate_pct": 100.0
        }
    });
    let events = entries
        .iter()
        .map(|(id, tier)| {
            serde_json::json!({
                "schema": MUST_PASS_EVENT_SCHEMA,
                "set": "must_pass",
                "run_id": "ci-123",
                "correlation_id": "must-pass-gate-ci-123",
                "git_commit": bindings.git_commit,
                "source_tree_sha256": bindings.source_tree_sha256,
                "inclusion_sha256": bindings.inclusion_sha256,
                "manifest_sha256": bindings.manifest_sha256,
                "id": id,
                "tier": tier,
                "status": "pass"
            })
        })
        .collect::<Vec<_>>();
    write_must_pass_fixture_events(&root, &events);

    MustPassValidationFixture {
        root,
        payload,
        bindings,
        events,
    }
}

fn validate_must_pass_fixture(fixture: &MustPassValidationFixture) -> Result<Value, String> {
    let events_contents = std::fs::read_to_string(fixture.root.join(MUST_PASS_EVENTS_PATH))
        .expect("read must-pass event fixture");
    validate_must_pass_gate_payload_against_bindings_with_events(
        &fixture.payload,
        &fixture.bindings,
        &events_contents,
        &fixture.bindings.git_commit,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )
}

fn update_fixture_inclusion_binding(fixture: &mut MustPassValidationFixture, value: &Value) {
    fixture.bindings.inclusion_contents =
        serde_json::to_vec(value).expect("serialize mutated inclusion fixture");
    fixture.bindings.inclusion_sha256 =
        format!("{:x}", Sha256::digest(&fixture.bindings.inclusion_contents));
    fixture.payload["inclusion_sha256"] = Value::String(fixture.bindings.inclusion_sha256.clone());
    for event in &mut fixture.events {
        event["inclusion_sha256"] = Value::String(fixture.bindings.inclusion_sha256.clone());
    }
    write_must_pass_fixture_events(&fixture.root, &fixture.events);
}

fn update_fixture_manifest_binding(fixture: &mut MustPassValidationFixture, value: &Value) {
    fixture.bindings.manifest_contents =
        serde_json::to_vec(value).expect("serialize mutated manifest fixture");
    fixture.bindings.manifest_sha256 =
        format!("{:x}", Sha256::digest(&fixture.bindings.manifest_contents));
    fixture.payload["manifest_sha256"] = Value::String(fixture.bindings.manifest_sha256.clone());
    for event in &mut fixture.events {
        event["manifest_sha256"] = Value::String(fixture.bindings.manifest_sha256.clone());
    }
    write_must_pass_fixture_events(&fixture.root, &fixture.events);
}

#[test]
fn validate_must_pass_gate_payload_accepts_exact_authoritative_evidence() {
    let fixture = must_pass_validation_fixture();
    let summary = validate_must_pass_fixture(&fixture)
        .expect("exact authoritative must-pass evidence should validate");
    assert_eq!(summary["status"], "pass");
    assert_eq!(summary["must_pass_total"], 208);
    assert_eq!(summary["must_pass_passed"], 208);
    assert_eq!(summary["git_commit"], fixture.bindings.git_commit);
}

#[test]
fn validate_must_pass_gate_payload_rejects_non_exact_schema() {
    let mut fixture = must_pass_validation_fixture();
    fixture.payload["schema"] = Value::String("pi.ext.must_pass_gate.v1.extra".to_string());
    let error = validate_must_pass_fixture(&fixture)
        .expect_err("a prefix match must not satisfy the exact verdict schema");
    assert!(error.contains("schema must be exactly"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_stale_commit_and_hashes() {
    for field in [
        "git_commit",
        "source_tree_sha256",
        "inclusion_sha256",
        "manifest_sha256",
    ] {
        let mut fixture = must_pass_validation_fixture();
        fixture.payload[field] = Value::String(if field == "git_commit" {
            "ffffffffffffffffffffffffffffffffffffffff".to_string()
        } else {
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string()
        });
        let error = validate_must_pass_fixture(&fixture)
            .expect_err("stale source identity must fail closed");
        assert!(error.contains(field), "{field}: {error}");
    }
}

#[test]
fn validate_must_pass_gate_payload_rejects_truncated_denominator() {
    let mut fixture = must_pass_validation_fixture();
    fixture.payload["observed"]["must_pass_total"] = serde_json::json!(207);
    fixture.payload["observed"]["must_pass_tested"] = serde_json::json!(207);
    fixture.payload["observed"]["must_pass_passed"] = serde_json::json!(207);
    let error = validate_must_pass_fixture(&fixture)
        .expect_err("a coherent 207/207 report must not replace the authoritative 208 set");
    assert!(error.contains("denominator 207"), "{error}");
    assert!(error.contains("208"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_incoherent_or_nonpassing_counts() {
    let mut incoherent = must_pass_validation_fixture();
    incoherent.payload["observed"]["must_pass_failed"] = serde_json::json!(1);
    let error = validate_must_pass_fixture(&incoherent)
        .expect_err("incoherent must-pass counts must fail closed");
    assert!(error.contains("must equal tested"), "{error}");

    let mut nonpassing = must_pass_validation_fixture();
    nonpassing.payload["observed"]["must_pass_passed"] = serde_json::json!(207);
    nonpassing.payload["observed"]["must_pass_failed"] = serde_json::json!(1);
    nonpassing.payload["observed"]["must_pass_pass_rate_pct"] = serde_json::json!(99.5);
    let error = validate_must_pass_fixture(&nonpassing)
        .expect_err("a reported failure must fail closed even with pass status");
    assert!(error.contains("100% pass"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_missing_or_duplicate_events() {
    let mut missing = must_pass_validation_fixture();
    missing.events.pop();
    write_must_pass_fixture_events(&missing.root, &missing.events);
    let error = validate_must_pass_fixture(&missing)
        .expect_err("missing authoritative event must fail closed");
    assert!(error.contains("do not exactly match"), "{error}");
    assert!(error.contains("missing=["), "{error}");

    let mut duplicate = must_pass_validation_fixture();
    duplicate.events.push(duplicate.events[0].clone());
    write_must_pass_fixture_events(&duplicate.root, &duplicate.events);
    let error = validate_must_pass_fixture(&duplicate)
        .expect_err("duplicate authoritative event must fail closed");
    assert!(error.contains("duplicate must_pass event id"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_wrong_tier_and_nonpass_events() {
    let mut wrong_tier = must_pass_validation_fixture();
    let expected_tier = wrong_tier.events[0]["tier"].as_u64().expect("fixture tier");
    wrong_tier.events[0]["tier"] = serde_json::json!(expected_tier % 5 + 1);
    write_must_pass_fixture_events(&wrong_tier.root, &wrong_tier.events);
    let error =
        validate_must_pass_fixture(&wrong_tier).expect_err("wrong manifest tier must fail closed");
    assert!(error.contains("tier mismatch"), "{error}");

    let mut nonpass = must_pass_validation_fixture();
    nonpass.events[0]["status"] = Value::String("skip".to_string());
    write_must_pass_fixture_events(&nonpass.root, &nonpass.events);
    let error = validate_must_pass_fixture(&nonpass)
        .expect_err("non-pass authoritative event must fail closed");
    assert!(error.contains("non-pass must-pass event"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_event_lineage_and_unexpected_ids() {
    let mut stale = must_pass_validation_fixture();
    stale.events[0]["source_tree_sha256"] = Value::String(
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string(),
    );
    write_must_pass_fixture_events(&stale.root, &stale.events);
    let error = validate_must_pass_fixture(&stale)
        .expect_err("event provenance must match the verdict and current source");
    assert!(error.contains("source_tree_sha256 mismatch"), "{error}");

    let mut unexpected = must_pass_validation_fixture();
    unexpected.events[0]["id"] = Value::String("not-authoritative".to_string());
    write_must_pass_fixture_events(&unexpected.root, &unexpected.events);
    let error = validate_must_pass_fixture(&unexpected)
        .expect_err("unexpected must-pass event ID must fail closed");
    assert!(error.contains("unexpected must-pass event id"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_missing_event_log() {
    let fixture = must_pass_validation_fixture();
    let missing_root = unique_temp_root("must-pass-events-missing");
    let error = validate_must_pass_gate_payload_against_bindings(
        &missing_root,
        &fixture.payload,
        &fixture.bindings,
        &fixture.bindings.git_commit,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )
    .expect_err("must-pass verdict without the required event log must fail closed");
    assert!(error.contains("failed to read required"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_malformed_inclusion_list() {
    let mut duplicate = must_pass_validation_fixture();
    let mut inclusion: Value = serde_json::from_slice(&duplicate.bindings.inclusion_contents)
        .expect("parse inclusion fixture");
    inclusion["tier1"][1]["id"] = inclusion["tier1"][0]["id"].clone();
    update_fixture_inclusion_binding(&mut duplicate, &inclusion);
    let error = validate_must_pass_fixture(&duplicate)
        .expect_err("duplicate authoritative inclusion ID must fail closed");
    assert!(error.contains("duplicate must-pass id"), "{error}");

    let mut mismatched_summary = must_pass_validation_fixture();
    let mut inclusion: Value =
        serde_json::from_slice(&mismatched_summary.bindings.inclusion_contents)
            .expect("parse inclusion fixture");
    inclusion["summary"]["total_must_pass"] = serde_json::json!(207);
    update_fixture_inclusion_binding(&mut mismatched_summary, &inclusion);
    let error = validate_must_pass_fixture(&mismatched_summary)
        .expect_err("inclusion-list summary drift must fail closed");
    assert!(error.contains("summary mismatch"), "{error}");
}

#[test]
fn validate_must_pass_gate_payload_rejects_malformed_manifest() {
    let mut invalid_tier = must_pass_validation_fixture();
    let mut manifest: Value = serde_json::from_slice(&invalid_tier.bindings.manifest_contents)
        .expect("parse manifest fixture");
    manifest["extensions"][0]["conformance_tier"] = serde_json::json!(0);
    update_fixture_manifest_binding(&mut invalid_tier, &manifest);
    let error = validate_must_pass_fixture(&invalid_tier)
        .expect_err("invalid manifest tier must fail closed");
    assert!(error.contains("invalid conformance_tier"), "{error}");

    let mut duplicate = must_pass_validation_fixture();
    let mut manifest: Value = serde_json::from_slice(&duplicate.bindings.manifest_contents)
        .expect("parse manifest fixture");
    manifest["extensions"][1]["id"] = manifest["extensions"][0]["id"].clone();
    update_fixture_manifest_binding(&mut duplicate, &manifest);
    let error =
        validate_must_pass_fixture(&duplicate).expect_err("duplicate manifest ID must fail closed");
    assert!(error.contains("duplicate extension id"), "{error}");

    let mut duplicate_artifact = must_pass_validation_fixture();
    let mut manifest: Value =
        serde_json::from_slice(&duplicate_artifact.bindings.manifest_contents)
            .expect("parse manifest fixture");
    let first_entry_path = manifest["extensions"][0]["entry_path"].clone();
    manifest["extensions"][1]["entry_path"] = first_entry_path;
    update_fixture_manifest_binding(&mut duplicate_artifact, &manifest);
    let error = validate_must_pass_fixture(&duplicate_artifact)
        .expect_err("two extension IDs must not share one artifact identity");
    assert!(error.contains("reuses artifact entry_path"), "{error}");

    for unsafe_path in ["C:/outside.ts", "C:outside.ts", "../outside.ts"] {
        let mut unsafe_artifact = must_pass_validation_fixture();
        let mut manifest: Value =
            serde_json::from_slice(&unsafe_artifact.bindings.manifest_contents)
                .expect("parse manifest fixture");
        manifest["extensions"][0]["entry_path"] = Value::String(unsafe_path.to_string());
        unsafe_artifact
            .bindings
            .tracked_paths
            .insert(format!("{MUST_PASS_ARTIFACTS_PATH}/{unsafe_path}"));
        update_fixture_manifest_binding(&mut unsafe_artifact, &manifest);
        let error = validate_must_pass_fixture(&unsafe_artifact)
            .expect_err("unsafe manifest artifact paths must fail before tracked-path lookup");
        assert!(
            error.contains("malformed entry_path"),
            "{unsafe_path}: {error}"
        );
    }

    let mut untracked_artifact = must_pass_validation_fixture();
    let mut manifest: Value =
        serde_json::from_slice(&untracked_artifact.bindings.manifest_contents)
            .expect("parse manifest fixture");
    manifest["extensions"][0]["entry_path"] = Value::String("missing.js".to_string());
    update_fixture_manifest_binding(&mut untracked_artifact, &manifest);
    let error = validate_must_pass_fixture(&untracked_artifact)
        .expect_err("manifest artifact absent from canonical commit must fail closed");
    assert!(
        error.contains("not tracked by the canonical commit"),
        "{error}"
    );
}

#[test]
fn validate_perf_comparison_payload_accepts_current_shape() {
    let payload = serde_json::json!({
        "schema": "pi.ext.perf_comparison.v1",
        "generated_at": "2026-02-17T03:00:00.000Z",
        "summary": {
            "overall_verdict": "faster",
            "faster_count": 7,
            "slower_count": 1,
            "comparable_count": 2
        }
    });

    let summary = validate_perf_comparison_payload(&payload)
        .expect("current perf_comparison payload shape should validate");
    assert_eq!(summary["overall_verdict"], "faster");
    assert_eq!(summary["faster_count"], 7);
    assert_eq!(summary["slower_count"], 1);
    assert_eq!(summary["comparable_count"], 2);
}

#[test]
fn validate_perf_comparison_payload_rejects_missing_overall_verdict() {
    let payload = serde_json::json!({
        "schema": "pi.ext.perf_comparison.v1",
        "generated_at": "2026-02-17T03:00:00.000Z",
        "summary": {
            "faster_count": 7,
            "slower_count": 1,
            "comparable_count": 2
        }
    });

    let err = validate_perf_comparison_payload(&payload)
        .expect_err("perf_comparison without overall_verdict should fail closed");
    assert!(
        err.contains("overall_verdict"),
        "expected overall_verdict validation error, got: {err}"
    );
}

#[test]
fn validate_parameter_sweeps_payload_accepts_current_shape() {
    let payload = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "generated_at": "2026-02-17T03:00:00.000Z",
        "readiness": {
            "status": "ready",
            "ready_for_phase5": true,
            "blocking_reasons": []
        },
        "source_identity": {
            "source_artifact": "tests/perf/runs/results/phase1_matrix_validation.json"
        }
    });

    let summary = validate_parameter_sweeps_payload(&payload)
        .expect("current parameter_sweeps payload shape should validate");
    assert_eq!(summary["readiness_status"], "ready");
    assert_eq!(summary["ready_for_phase5"], true);
    assert_eq!(summary["blocking_reasons_count"], 0);
}

#[test]
fn validate_parameter_sweeps_payload_rejects_unknown_readiness_status() {
    let payload = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "generated_at": "2026-02-17T03:00:00.000Z",
        "readiness": {
            "status": "unknown",
            "ready_for_phase5": false,
            "blocking_reasons": ["lineage_missing"]
        },
        "source_identity": {
            "source_artifact": "tests/perf/runs/results/phase1_matrix_validation.json"
        }
    });

    let err = validate_parameter_sweeps_payload(&payload)
        .expect_err("parameter_sweeps with non-contract readiness status must fail closed");
    assert!(
        err.contains("ready|blocked"),
        "expected readiness status validation error, got: {err}"
    );
}

fn lineage_fixture_section(id: &str, summary: Value) -> BundleSection {
    BundleSection {
        id: id.to_string(),
        label: id.to_string(),
        category: "performance".to_string(),
        status: "present".to_string(),
        artifact_path: Some(format!("{id}.json")),
        schema: None,
        summary: Some(summary),
        diagnostics: None,
        file_count: 1,
        total_bytes: 1,
    }
}

fn unique_temp_root(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "ci-evidence-bundle-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn write_fixture_json(path: &Path, payload: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(payload).expect("fixture JSON should serialize");
    std::fs::write(path, text).expect("fixture JSON should write");
}

#[test]
fn stress_triage_source_is_required_json_artifact() {
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "stress_triage")
        .expect("stress_triage source must exist");
    assert!(
        !source.is_directory,
        "stress_triage source must target a JSON artifact"
    );
    assert!(
        source.path.ends_with("stress_triage.json"),
        "stress_triage source must point to stress_triage.json"
    );
    assert!(
        source.required,
        "stress_triage source should be required for PERF-3X lineage contract"
    );
}

#[test]
fn validate_perf3x_lineage_contract_accepts_coherent_generated_at_fields() {
    let sections = vec![
        lineage_fixture_section(
            "must_pass_gate",
            serde_json::json!({
                "run_id": "local-20260217T030608928Z",
                "correlation_id": "corr-local-20260217T030608928Z",
                "generated_at": "2026-02-17T03:06:08.928Z"
            }),
        ),
        lineage_fixture_section(
            "conformance_summary",
            serde_json::json!({
                "generated_at": "2026-02-16T20:45:35Z"
            }),
        ),
        lineage_fixture_section(
            "stress_triage",
            serde_json::json!({
                "generated_at": "2026-02-06T01:29:10Z"
            }),
        ),
    ];

    let summary = validate_perf3x_lineage_contract(&sections)
        .expect("coherent lineage metadata should pass contract validation");
    assert_eq!(summary["run_id"], "local-20260217T030608928Z");
}

#[test]
fn validate_perf3x_lineage_contract_rejects_excessive_artifact_span() {
    let sections = vec![
        lineage_fixture_section(
            "must_pass_gate",
            serde_json::json!({
                "run_id": "run-123",
                "correlation_id": "corr-run-123",
                "generated_at": "2026-02-17T03:06:08.928Z"
            }),
        ),
        lineage_fixture_section(
            "conformance_summary",
            serde_json::json!({
                "generated_at": "2026-02-16T20:45:35Z"
            }),
        ),
        lineage_fixture_section(
            "stress_triage",
            serde_json::json!({
                "generated_at": "2026-01-01T00:00:00Z"
            }),
        ),
    ];

    let err = validate_perf3x_lineage_contract(&sections)
        .expect_err("lineage span beyond threshold must fail closed");
    assert!(
        err.contains("span exceeds"),
        "expected span-threshold failure detail, got: {err}"
    );
}

#[test]
fn validate_perf3x_lineage_contract_rejects_missing_generated_at() {
    let sections = vec![
        lineage_fixture_section(
            "must_pass_gate",
            serde_json::json!({
                "run_id": "run-123",
                "correlation_id": "corr-run-123",
                "generated_at": "2026-02-17T03:06:08.928Z"
            }),
        ),
        lineage_fixture_section("conformance_summary", serde_json::json!({})),
        lineage_fixture_section(
            "stress_triage",
            serde_json::json!({
                "generated_at": "2026-02-06T01:29:10Z"
            }),
        ),
    ];

    let err = validate_perf3x_lineage_contract(&sections)
        .expect_err("missing generated_at metadata must fail closed");
    assert!(
        err.contains("generated_at"),
        "expected generated_at validation detail, got: {err}"
    );
}

#[test]
fn collect_section_reports_missing_file_path_diagnostics() {
    let root = unique_temp_root("missing-file");
    let _ = std::fs::create_dir_all(&root);
    let source = ArtifactSource {
        id: "missing_file",
        label: "Missing file",
        category: "unit",
        path: "does/not/exist.json",
        expected_schema: Some("pi.test"),
        is_directory: false,
        required: false,
    };

    let section = collect_section(&root, &source);
    assert_eq!(section.status, "missing");
    assert_eq!(section.file_count, 0);
    assert_eq!(section.total_bytes, 0);
    assert_eq!(
        section.artifact_path.as_deref(),
        Some("does/not/exist.json")
    );
    assert_eq!(section.diagnostics.as_deref(), Some("File not found"));
}

#[test]
fn collect_section_reports_missing_directory_path_diagnostics() {
    let root = unique_temp_root("missing-directory");
    let _ = std::fs::create_dir_all(&root);
    let source = ArtifactSource {
        id: "missing_dir",
        label: "Missing dir",
        category: "unit",
        path: "does/not/exist",
        expected_schema: None,
        is_directory: true,
        required: false,
    };

    let section = collect_section(&root, &source);
    assert_eq!(section.status, "missing");
    assert_eq!(section.file_count, 0);
    assert_eq!(section.total_bytes, 0);
    assert_eq!(section.artifact_path.as_deref(), Some("does/not/exist"));
    assert_eq!(section.diagnostics.as_deref(), Some("Directory not found"));
}

#[cfg(unix)]
#[test]
fn collect_section_rejects_artifact_paths_that_escape_through_symlinks() {
    use std::os::unix::fs::symlink;

    let root = unique_temp_root("symlink-root");
    let outside = unique_temp_root("symlink-outside");
    std::fs::create_dir_all(&root).expect("create artifact fixture root");
    std::fs::create_dir_all(&outside).expect("create outside artifact fixture root");
    write_fixture_json(
        &outside.join("artifact.json"),
        &serde_json::json!({"schema": "pi.test.v1"}),
    );
    symlink(&outside, root.join("linked")).expect("create repository artifact symlink fixture");
    let source = ArtifactSource {
        id: "symlink_escape",
        label: "Symlink escape",
        category: "unit",
        path: "linked/artifact.json",
        expected_schema: Some("pi.test"),
        is_directory: false,
        required: true,
    };

    let section = collect_section(&root, &source);
    assert_eq!(section.status, "invalid");
    assert!(
        section
            .diagnostics
            .as_deref()
            .is_some_and(|detail| detail.contains("symbolic link")),
        "symlink escape must have an explicit diagnostic: {:?}",
        section.diagnostics
    );
}

#[test]
fn collect_section_parameter_sweeps_reports_custom_missing_diagnostic() {
    let root = unique_temp_root("parameter-sweeps-missing");
    let _ = std::fs::create_dir_all(&root);
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "parameter_sweeps")
        .expect("parameter_sweeps source must exist");

    let section = collect_section(&root, source);
    assert_eq!(section.status, "missing");
    assert_eq!(section.artifact_path.as_deref(), Some(source.path));
    assert_eq!(
        section.diagnostics.as_deref(),
        Some(PARAMETER_SWEEPS_MISSING_DIAGNOSTIC)
    );
}

#[test]
fn collect_section_parameter_sweeps_uses_discovered_artifact_path() {
    let root = unique_temp_root("parameter-sweeps-discovery");
    let _ = std::fs::create_dir_all(&root);
    let source = ARTIFACT_SOURCES
        .iter()
        .find(|source| source.id == "parameter_sweeps")
        .expect("parameter_sweeps source must exist");
    let discovered_path = root.join("tests/e2e_results/run-42/results/parameter_sweeps.json");
    write_fixture_json(
        &discovered_path,
        &serde_json::json!({
            "schema": "pi.perf.parameter_sweeps.v1",
            "generated_at": "2026-02-17T04:00:00.000Z",
            "readiness": {
                "status": "blocked",
                "ready_for_phase5": false,
                "blocking_reasons": ["need_additional_runs"]
            },
            "source_identity": {
                "source_artifact": "tests/perf/runs/results/phase1_matrix_validation.json"
            }
        }),
    );

    let section = collect_section(&root, source);
    assert_eq!(section.status, "present");
    assert_eq!(
        section.artifact_path.as_deref(),
        Some("tests/e2e_results/run-42/results/parameter_sweeps.json")
    );
    assert_eq!(section.file_count, 1);
    assert!(
        section.total_bytes > 0,
        "parameter_sweeps section should include file size for discovered artifact"
    );
    let summary = section
        .summary
        .as_ref()
        .expect("parameter_sweeps section should include summary payload");
    assert_eq!(
        summary.get("readiness_status").and_then(Value::as_str),
        Some("blocked")
    );
    assert_eq!(
        summary.get("ready_for_phase5").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        summary
            .get("blocking_reasons_count")
            .and_then(Value::as_u64),
        Some(1)
    );
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or_else(String::new, |f| f.to_uppercase().to_string() + c.as_str())
}
