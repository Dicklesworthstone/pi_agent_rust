//! Release-readiness verification report generator (bd-k5q5.7.11).
//!
//! Aggregates evidence from conformance, performance, security, and traceability
//! into a single user-focused release-readiness summary.

use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const REPORT_SCHEMA: &str = "pi.release_readiness.v1";
const CONFORMANCE_SUMMARY_SCHEMA: &str = "pi.ext.conformance_summary.v2";
const CONFORMANCE_SUMMARY_PATH: &str = "tests/ext_conformance/reports/conformance_summary.json";
const CONFORMANCE_MAX_AGE_HOURS: i64 = 168;
const MUST_PASS_GATE_SCHEMA: &str = "pi.ext.must_pass_gate.v1";
const MUST_PASS_INCLUSION_PATH: &str = "docs/extension-inclusion-list.json";
const MUST_PASS_MANIFEST_PATH: &str = "tests/ext_conformance/VALIDATED_MANIFEST.json";
const MUST_PASS_VERDICT_PATH: &str =
    "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json";
const MUST_PASS_EVENTS_PATH: &str = "tests/ext_conformance/reports/gate/must_pass_events.jsonl";
const MUST_PASS_EVIDENCE_PATHS: &[&str] = &[MUST_PASS_VERDICT_PATH, MUST_PASS_EVENTS_PATH];
const MUST_PASS_ARTIFACTS_PATH: &str = "tests/ext_conformance/artifacts";
const EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1: u64 = 208;
const NON_MOCK_RUBRIC_SCHEMA: &str = "pi.qa.non_mock_rubric.v1";
const FULL_SUITE_GATE_SCHEMA: &str = "pi.ci.full_suite_gate.v1";
const EXT_REMEDIATION_BACKLOG_SCHEMA: &str = "pi.qa.extension_remediation_backlog.v1";
const PRACTICAL_FINISH_CHECKPOINT_SCHEMA: &str = "pi.perf3x.practical_finish_checkpoint.v1";
const PARAMETER_SWEEPS_SCHEMA: &str = "pi.perf.parameter_sweeps.v1";
const PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/parameter_sweeps.json";
const OPPORTUNITY_MATRIX_SCHEMA: &str = "pi.perf.opportunity_matrix.v1";
const OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/opportunity_matrix.json";
const PERF_BUDGET_SUMMARY_SCHEMA: &str = "pi.perf.budget_summary.v2";

// ── Data models ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Signal {
    Pass,
    Warn,
    Fail,
    NoData,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS"),
            Self::Warn => f.write_str("WARN"),
            Self::Fail => f.write_str("FAIL"),
            Self::NoData => f.write_str("NO_DATA"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DimensionScore {
    name: String,
    signal: Signal,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseReadinessReport {
    schema: String,
    generated_at: String,
    overall_verdict: Signal,
    dimensions: Vec<DimensionScore>,
    known_issues: Vec<String>,
    reproduce_command: String,
}

impl ReleaseReadinessReport {
    fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Release Readiness Report\n\n");
        let _ = writeln!(out, "**Generated**: {}", self.generated_at);
        let _ = writeln!(out, "**Overall Verdict**: {}\n", self.overall_verdict);

        out.push_str("## Quality Scorecard\n\n");
        out.push_str("| Dimension | Signal | Detail |\n");
        out.push_str("|-----------|--------|--------|\n");
        for d in &self.dimensions {
            let icon = match d.signal {
                Signal::Pass => "PASS",
                Signal::Warn => "WARN",
                Signal::Fail => "FAIL",
                Signal::NoData => "N/A",
            };
            let _ = writeln!(out, "| {} | {icon} | {} |", d.name, d.detail);
        }
        out.push('\n');

        if !self.known_issues.is_empty() {
            out.push_str("## Known Issues\n\n");
            for issue in &self.known_issues {
                let _ = writeln!(out, "- {issue}");
            }
            out.push('\n');
        }

        out.push_str("## Reproduce\n\n");
        let _ = writeln!(out, "```\n{}\n```", self.reproduce_command);

        out
    }
}

// ── JSON helpers ────────────────────────────────────────────────────────────

type V = serde_json::Value;

fn get_u64(v: &V, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(V::as_u64).unwrap_or(0)
}

fn get_f64(v: &V, pointer: &str) -> f64 {
    v.pointer(pointer).and_then(V::as_f64).unwrap_or(0.0)
}

fn get_str<'a>(v: &'a V, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(V::as_str).unwrap_or("unknown")
}

fn parse_must_pass_gate_verdict(v: &V) -> (String, u64, u64) {
    let status = get_str(v, "/status").to_string();
    let total = get_u64(v, "/observed/must_pass_total");
    let passed = get_u64(v, "/observed/must_pass_passed");

    (status, passed, total)
}

fn validate_must_pass_gate_metadata(v: &V) -> Vec<String> {
    let mut errors = Vec::new();

    let schema = get_str(v, "/schema");
    if schema != MUST_PASS_GATE_SCHEMA {
        errors.push(format!(
            "schema must be {MUST_PASS_GATE_SCHEMA}, found {schema}"
        ));
    }

    for field in [
        "/generated_at",
        "/run_id",
        "/correlation_id",
        "/git_commit",
        "/source_tree_sha256",
        "/inclusion_sha256",
        "/manifest_sha256",
    ] {
        let value = get_str(v, field);
        if value.trim().is_empty() || value == "unknown" {
            errors.push(format!("missing or empty required field: {field}"));
        } else if value.trim() != value || value.chars().any(char::is_control) {
            errors.push(format!("non-canonical required field: {field}"));
        }
    }

    let generated_at = get_str(v, "/generated_at").trim();
    if !generated_at.is_empty()
        && generated_at != "unknown"
        && chrono::DateTime::parse_from_rfc3339(generated_at).is_err()
    {
        errors.push(format!(
            "invalid RFC3339 timestamp in /generated_at: {generated_at}"
        ));
    }

    let git_commit = get_str(v, "/git_commit");
    if !git_commit.trim().is_empty()
        && git_commit != "unknown"
        && (git_commit.trim() != git_commit
            || !matches!(git_commit.len(), 40 | 64)
            || !git_commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        errors.push(
            "/git_commit must be a full 40- or 64-character hexadecimal commit ID".to_string(),
        );
    }
    for field in [
        "/source_tree_sha256",
        "/inclusion_sha256",
        "/manifest_sha256",
    ] {
        let digest = get_str(v, field).trim();
        if !digest.is_empty()
            && digest != "unknown"
            && (digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            errors.push(format!(
                "{field} must be a 64-character hexadecimal SHA-256"
            ));
        }
    }

    if v.pointer("/observed").is_none() {
        errors.push("missing required object: /observed".to_string());
        return errors;
    }

    let required_count = |name: &str| {
        v.pointer(&format!("/observed/{name}"))
            .and_then(V::as_u64)
            .ok_or_else(|| format!("missing required unsigned count: /observed/{name}"))
    };
    let counts = (
        required_count("must_pass_total"),
        required_count("must_pass_tested"),
        required_count("must_pass_passed"),
        required_count("must_pass_failed"),
        required_count("must_pass_skipped"),
    );
    match counts {
        (Ok(total), Ok(tested), Ok(passed), Ok(failed), Ok(skipped)) => {
            if passed.checked_add(failed) != Some(tested) {
                errors.push(format!(
                    "must-pass tested-count mismatch: passed({passed}) + failed({failed}) != tested({tested})"
                ));
            }
            if tested.checked_add(skipped) != Some(total) {
                errors.push(format!(
                    "must-pass total-count mismatch: tested({tested}) + skipped({skipped}) != total({total})"
                ));
            }
        }
        counts => {
            errors.extend(
                [counts.0, counts.1, counts.2, counts.3, counts.4]
                    .into_iter()
                    .filter_map(Result::err),
            );
        }
    }

    errors
}

fn required_unsigned(v: &V, pointer: &str) -> Result<u64, String> {
    v.pointer(pointer)
        .and_then(V::as_u64)
        .ok_or_else(|| format!("missing required unsigned count: {pointer}"))
}

fn required_number(v: &V, pointer: &str) -> Result<f64, String> {
    v.pointer(pointer)
        .and_then(V::as_f64)
        .ok_or_else(|| format!("missing required numeric value: {pointer}"))
}

fn validate_must_pass_thresholds(v: &V, errors: &mut Vec<String>) {
    let min_pass_rate = required_number(v, "/thresholds/min_pass_rate_pct");
    let max_failures = required_unsigned(v, "/thresholds/max_failures");
    if let Ok(value) = &min_pass_rate
        && value.to_bits() != 100.0_f64.to_bits()
    {
        errors.push(format!(
            "/thresholds/min_pass_rate_pct must be 100, found {value}"
        ));
    }
    if let Ok(value) = &max_failures
        && *value != 0
    {
        errors.push(format!("/thresholds/max_failures must be 0, found {value}"));
    }
    errors.extend(
        [min_pass_rate.err(), max_failures.err()]
            .into_iter()
            .flatten(),
    );
}

fn validate_must_pass_core_counts(v: &V, errors: &mut Vec<String>) {
    let total = required_unsigned(v, "/observed/must_pass_total");
    let passed = required_unsigned(v, "/observed/must_pass_passed");
    let failed = required_unsigned(v, "/observed/must_pass_failed");
    let skipped = required_unsigned(v, "/observed/must_pass_skipped");
    let pass_rate = required_number(v, "/observed/must_pass_pass_rate_pct");
    if let Err(error) = &pass_rate {
        errors.push(error.clone());
    }
    if let (Ok(total), Ok(passed), Ok(failed), Ok(skipped), Ok(pass_rate)) =
        (total, passed, failed, skipped, pass_rate)
    {
        if pass_rate.to_bits() != 100.0_f64.to_bits() {
            errors.push(format!(
                "certified must-pass rate must be 100, found {pass_rate}"
            ));
        }
        let expected_status = if total > 0 && passed == total && failed == 0 && skipped == 0 {
            "pass"
        } else {
            "fail"
        };
        if get_str(v, "/status") != expected_status {
            errors.push(format!(
                "must-pass status mismatch: expected {expected_status}, found {}",
                get_str(v, "/status")
            ));
        }
    }
}

fn validate_stretch_counts(v: &V, errors: &mut Vec<String>) {
    let values = [
        required_unsigned(v, "/observed/stretch_total"),
        required_unsigned(v, "/observed/stretch_tested"),
        required_unsigned(v, "/observed/stretch_passed"),
        required_unsigned(v, "/observed/stretch_failed"),
        required_unsigned(v, "/observed/stretch_skipped"),
    ];
    for error in values.iter().filter_map(|value| value.as_ref().err()) {
        errors.push(error.clone());
    }
    let [Ok(total), Ok(tested), Ok(passed), Ok(failed), Ok(skipped)] = values else {
        return;
    };
    if passed.checked_add(failed) != Some(tested) {
        errors.push(format!(
            "stretch tested-count mismatch: passed({passed}) + failed({failed}) != tested({tested})"
        ));
    }
    if tested.checked_add(skipped) != Some(total) {
        errors.push(format!(
            "stretch total-count mismatch: tested({tested}) + skipped({skipped}) != total({total})"
        ));
    }
    for (field, expected) in [
        ("total", total),
        ("tested", tested),
        ("passed", passed),
        ("failed", failed),
        ("skipped", skipped),
    ] {
        let pointer = format!("/stretch_set_summary/{field}");
        match v.pointer(&pointer).and_then(V::as_u64) {
            Some(actual) if actual == expected => {}
            Some(actual) => errors.push(format!(
                "{pointer} mismatch: expected {expected}, found {actual}"
            )),
            None => errors.push(format!("missing required unsigned count: {pointer}")),
        }
    }
}

fn validate_must_pass_check_binding(v: &V, check: &V, id: &str, errors: &mut Vec<String>) {
    let matches = match id {
        "must_pass_rate" => {
            check.get("actual").and_then(V::as_f64)
                == v.pointer("/observed/must_pass_pass_rate_pct")
                    .and_then(V::as_f64)
                && check.get("threshold").and_then(V::as_f64)
                    == v.pointer("/thresholds/min_pass_rate_pct")
                        .and_then(V::as_f64)
        }
        "must_pass_failure_count" => {
            check.get("actual").and_then(V::as_u64)
                == v.pointer("/observed/must_pass_failed").and_then(V::as_u64)
                && check.get("threshold").and_then(V::as_u64)
                    == v.pointer("/thresholds/max_failures").and_then(V::as_u64)
        }
        "must_pass_complete_coverage" => {
            check.get("actual").and_then(V::as_u64)
                == v.pointer("/observed/must_pass_tested").and_then(V::as_u64)
                && check.get("threshold").and_then(V::as_u64)
                    == v.pointer("/observed/must_pass_total").and_then(V::as_u64)
        }
        _ => return,
    };
    if !matches {
        errors.push(format!(
            "{id} check does not match observed values and thresholds"
        ));
    }
}

fn validate_must_pass_checks(v: &V, errors: &mut Vec<String>) {
    let expected = BTreeSet::from([
        "must_pass_complete_coverage",
        "must_pass_failure_count",
        "must_pass_rate",
    ]);
    let Some(checks) = v.pointer("/checks").and_then(V::as_array) else {
        errors.push("missing required array: /checks".to_string());
        return;
    };
    let mut observed = BTreeSet::new();
    for (index, check) in checks.iter().enumerate() {
        let Some(id) = check.get("id").and_then(V::as_str) else {
            errors.push(format!("/checks/{index}/id must be a string"));
            continue;
        };
        if !observed.insert(id) {
            errors.push(format!("duplicate must-pass check id: {id}"));
        }
        if check.get("ok").and_then(V::as_bool) != Some(true) {
            errors.push(format!("must-pass check {id} is not true"));
        }
        validate_must_pass_check_binding(v, check, id, errors);
    }
    if observed != expected {
        errors.push(format!(
            "must-pass check set mismatch: expected {expected:?}, found {observed:?}"
        ));
    }
}

fn validate_must_pass_gate_contract(v: &V) -> Vec<String> {
    let mut errors = validate_must_pass_gate_metadata(v);
    if get_str(v, "/mode") != "strict" {
        errors.push(format!(
            "/mode must be strict, found {}",
            get_str(v, "/mode")
        ));
    }
    if !matches!(get_str(v, "/status"), "pass" | "fail") {
        errors.push(format!(
            "/status must be pass or fail, found {}",
            get_str(v, "/status")
        ));
    }
    validate_must_pass_thresholds(v, &mut errors);
    validate_must_pass_core_counts(v, &mut errors);
    validate_stretch_counts(v, &mut errors);
    validate_must_pass_checks(v, &mut errors);
    match v.pointer("/blocking_failures").and_then(V::as_array) {
        Some(failures) if get_str(v, "/status") == "pass" && !failures.is_empty() => {
            errors.push("passing must-pass verdict contains blocking failures".to_string());
        }
        Some(_) => {}
        None => errors.push("missing required array: /blocking_failures".to_string()),
    }
    errors
}

fn is_canonical_artifact_entry_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let has_windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && !value.starts_with('/')
        && !has_windows_drive_prefix
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn canonical_must_pass_entries(
    root: &Path,
    expected_must_pass: u64,
) -> Result<BTreeMap<String, u64>, String> {
    let inclusion_path = root.join(MUST_PASS_INCLUSION_PATH);
    let inclusion_contents = std::fs::read(&inclusion_path)
        .map_err(|err| format!("failed to read {MUST_PASS_INCLUSION_PATH}: {err}"))?;
    let manifest_path = root.join(MUST_PASS_MANIFEST_PATH);
    let manifest_contents = std::fs::read(&manifest_path)
        .map_err(|err| format!("failed to read {MUST_PASS_MANIFEST_PATH}: {err}"))?;
    canonical_must_pass_entries_from_contents(
        &inclusion_contents,
        &manifest_contents,
        expected_must_pass,
    )
}

fn canonical_must_pass_entries_from_contents(
    inclusion_contents: &[u8],
    manifest_contents: &[u8],
    expected_must_pass: u64,
) -> Result<BTreeMap<String, u64>, String> {
    let must_pass_ids = parse_canonical_must_pass_ids(inclusion_contents, expected_must_pass)?;
    let manifest_tiers = parse_canonical_manifest_tiers(manifest_contents)?;
    let mut entries = BTreeMap::new();
    for id in must_pass_ids {
        let tier = manifest_tiers.get(&id).copied().ok_or_else(|| {
            format!(
                "canonical must-pass id {id} from {MUST_PASS_INCLUSION_PATH} is absent from {MUST_PASS_MANIFEST_PATH}"
            )
        })?;
        entries.insert(id, tier);
    }
    Ok(entries)
}

fn parse_canonical_must_pass_ids(
    inclusion_contents: &[u8],
    expected_must_pass: u64,
) -> Result<BTreeSet<String>, String> {
    let inclusion: V = serde_json::from_slice(inclusion_contents)
        .map_err(|err| format!("failed to parse {MUST_PASS_INCLUSION_PATH}: {err}"))?;
    if get_str(&inclusion, "/schema") != "pi.ext.inclusion_list.v1" {
        return Err(format!(
            "unexpected schema in {MUST_PASS_INCLUSION_PATH}: {}",
            get_str(&inclusion, "/schema")
        ));
    }

    let mut must_pass_ids = BTreeSet::new();
    let mut section_counts = BTreeMap::new();
    for section in ["tier1", "tier1_review"] {
        let items = inclusion
            .get(section)
            .and_then(V::as_array)
            .ok_or_else(|| format!("{MUST_PASS_INCLUSION_PATH} missing {section} array"))?;
        section_counts.insert(section, items.len());
        for (index, item) in items.iter().enumerate() {
            let id = item.get("id").and_then(V::as_str).ok_or_else(|| {
                format!("{MUST_PASS_INCLUSION_PATH} {section}[{index}] missing non-empty id")
            })?;
            if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} {section}[{index}] has a malformed id"
                ));
            }
            if !must_pass_ids.insert(id.to_string()) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} contains duplicate must-pass id {id}"
                ));
            }
        }
    }
    let required_summary_count = |name: &str| {
        inclusion
            .pointer(&format!("/summary/{name}"))
            .and_then(V::as_u64)
            .ok_or_else(|| {
                format!("{MUST_PASS_INCLUSION_PATH} missing unsigned summary count: {name}")
            })
    };
    let tier1_count = required_summary_count("tier1_count")?;
    let tier1_review_count = required_summary_count("tier1_review_count")?;
    let total_must_pass = required_summary_count("total_must_pass")?;
    let observed_tier1 = u64::try_from(section_counts["tier1"]).unwrap_or(u64::MAX);
    let observed_review = u64::try_from(section_counts["tier1_review"]).unwrap_or(u64::MAX);
    let observed_total = u64::try_from(must_pass_ids.len()).unwrap_or(u64::MAX);
    if tier1_count != observed_tier1
        || tier1_review_count != observed_review
        || total_must_pass != observed_total
        || total_must_pass != expected_must_pass
    {
        return Err(format!(
            "{MUST_PASS_INCLUSION_PATH} summary mismatch or unexpected versioned must-pass denominator: summary={tier1_count}+{tier1_review_count}={total_must_pass}, observed={observed_tier1}+{observed_review}={observed_total}, expected={expected_must_pass}"
        ));
    }

    Ok(must_pass_ids)
}

fn parse_canonical_manifest_tiers(
    manifest_contents: &[u8],
) -> Result<BTreeMap<String, u64>, String> {
    let manifest: V = serde_json::from_slice(manifest_contents)
        .map_err(|err| format!("failed to parse {MUST_PASS_MANIFEST_PATH}: {err}"))?;
    if get_str(&manifest, "/schema") != "pi.ext.validated-manifest.v1" {
        return Err(format!(
            "unexpected schema in {MUST_PASS_MANIFEST_PATH}: {}",
            get_str(&manifest, "/schema")
        ));
    }
    let extensions = manifest
        .get("extensions")
        .and_then(V::as_array)
        .ok_or_else(|| format!("{MUST_PASS_MANIFEST_PATH} missing extensions array"))?;

    let mut manifest_tiers = BTreeMap::new();
    let mut manifest_entry_paths = BTreeSet::new();
    for (index, extension) in extensions.iter().enumerate() {
        let entry_path = extension
            .get("entry_path")
            .and_then(V::as_str)
            .ok_or_else(|| {
                format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing artifact entry_path")
            })?;
        if !is_canonical_artifact_entry_path(entry_path) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} extensions[{index}] has a non-canonical artifact entry_path: {entry_path:?}"
            ));
        }
        if !manifest_entry_paths.insert(entry_path) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} contains duplicate artifact entry_path {entry_path}"
            ));
        }
        let tier = extension
            .get("conformance_tier")
            .and_then(V::as_u64)
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
        let id = extension.get("id").and_then(V::as_str).ok_or_else(|| {
            format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing non-empty id")
        })?;
        if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} extensions[{index}] has a malformed id"
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

struct ExpectedMustPassEventLineage<'a> {
    run_id: &'a str,
    correlation_id: &'a str,
    git_commit: &'a str,
    source_tree_sha256: &'a str,
    inclusion_sha256: &'a str,
    manifest_sha256: &'a str,
}

fn validate_must_pass_event_metadata(
    event: &V,
    line_number: usize,
    expected: &ExpectedMustPassEventLineage<'_>,
) -> Result<(), String> {
    if get_str(event, "/schema") != "pi.ext.gate_event.v1" {
        return Err(format!(
            "invalid must-pass event schema at {MUST_PASS_EVENTS_PATH}:{line_number}"
        ));
    }
    let timestamp = event.get("ts").and_then(V::as_str).ok_or_else(|| {
        format!(
            "must-pass event at {MUST_PASS_EVENTS_PATH}:{line_number} is missing an RFC3339 timestamp"
        )
    })?;
    if timestamp.trim() != timestamp
        || timestamp.chars().any(char::is_control)
        || chrono::DateTime::parse_from_rfc3339(timestamp).is_err()
    {
        return Err(format!(
            "must-pass event at {MUST_PASS_EVENTS_PATH}:{line_number} has an invalid RFC3339 timestamp"
        ));
    }
    if event.get("duration_ms").and_then(V::as_u64).is_none() {
        return Err(format!(
            "must-pass event at {MUST_PASS_EVENTS_PATH}:{line_number} is missing an unsigned duration_ms"
        ));
    }
    if get_str(event, "/run_id") != expected.run_id
        || get_str(event, "/correlation_id") != expected.correlation_id
    {
        return Err(format!(
            "must-pass event lineage mismatch at {MUST_PASS_EVENTS_PATH}:{line_number}"
        ));
    }
    if get_str(event, "/git_commit") != expected.git_commit
        || get_str(event, "/source_tree_sha256") != expected.source_tree_sha256
        || get_str(event, "/inclusion_sha256") != expected.inclusion_sha256
        || get_str(event, "/manifest_sha256") != expected.manifest_sha256
    {
        return Err(format!(
            "must-pass event source binding mismatch at {MUST_PASS_EVENTS_PATH}:{line_number}"
        ));
    }
    if get_str(event, "/status") != "pass" {
        return Err(format!(
            "non-pass must-pass event at {MUST_PASS_EVENTS_PATH}:{line_number}"
        ));
    }
    if event.get("failure_reason") != Some(&V::Null) {
        return Err(format!(
            "passing must-pass event at {MUST_PASS_EVENTS_PATH}:{line_number} has a non-null or missing failure_reason"
        ));
    }
    Ok(())
}

fn observed_must_pass_event_ids(
    contents: &str,
    expected_run_id: &str,
    expected_correlation_id: &str,
    expected_git_commit: &str,
    expected_source_tree_sha256: &str,
    expected_inclusion_sha256: &str,
    expected_manifest_sha256: &str,
) -> Result<BTreeMap<String, u64>, String> {
    let expected = ExpectedMustPassEventLineage {
        run_id: expected_run_id,
        correlation_id: expected_correlation_id,
        git_commit: expected_git_commit,
        source_tree_sha256: expected_source_tree_sha256,
        inclusion_sha256: expected_inclusion_sha256,
        manifest_sha256: expected_manifest_sha256,
    };
    let mut entries = BTreeMap::new();
    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let event: V = serde_json::from_str(line).map_err(|err| {
            format!(
                "invalid JSONL row {} in {MUST_PASS_EVENTS_PATH}: {err}",
                line_index + 1
            )
        })?;
        if get_str(&event, "/set") != "must_pass" {
            continue;
        }
        validate_must_pass_event_metadata(&event, line_index + 1, &expected)?;
        let id = event.get("id").and_then(V::as_str).ok_or_else(|| {
            format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} is missing a non-empty id",
                line_index + 1
            )
        })?;
        if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
            return Err(format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} has a malformed id",
                line_index + 1
            ));
        }
        let tier = event.get("tier").and_then(V::as_u64).ok_or_else(|| {
            format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} is missing an unsigned tier",
                line_index + 1
            )
        })?;
        if !(1..=5).contains(&tier) {
            return Err(format!(
                "must-pass event at {MUST_PASS_EVENTS_PATH}:{} has invalid conformance tier {tier}",
                line_index + 1
            ));
        }
        if entries.insert(id.to_string(), tier).is_some() {
            return Err(format!(
                "duplicate must-pass event id {id} in {MUST_PASS_EVENTS_PATH}"
            ));
        }
    }
    if entries.is_empty() {
        return Err(format!(
            "{MUST_PASS_EVENTS_PATH} contains no must-pass events"
        ));
    }
    Ok(entries)
}

fn format_id_preview(ids: &BTreeSet<String>) -> String {
    let mut preview = ids.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if ids.len() > 5 {
        preview.push_str(", ...");
    }
    preview
}

fn validate_exact_must_pass_set(
    expected: &BTreeMap<String, u64>,
    observed: &BTreeMap<String, u64>,
    passed: u64,
    total: u64,
) -> Result<(), String> {
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
    let tier_mismatches = expected
        .iter()
        .filter_map(|(id, expected_tier)| {
            observed.get(id).and_then(|observed_tier| {
                (observed_tier != expected_tier)
                    .then(|| format!("{id}:{observed_tier}!={expected_tier}"))
            })
        })
        .collect::<Vec<_>>();
    let expected_count = u64::try_from(expected.len()).unwrap_or(u64::MAX);
    if passed == expected_count
        && total == expected_count
        && missing.is_empty()
        && unexpected.is_empty()
        && tier_mismatches.is_empty()
    {
        return Ok(());
    }

    Err(format!(
        "must-pass evidence does not match the canonical inclusion-list set: verdict={passed}/{total}, expected={expected_count}, missing=[{}], unexpected=[{}], tier_mismatches=[{}]",
        format_id_preview(&missing),
        format_id_preview(&unexpected),
        tier_mismatches.join(", ")
    ))
}

fn validate_certified_must_pass_against_source(
    root: &Path,
    verdict: &V,
    current_source_tree_sha256: &str,
    current_inclusion_sha256: &str,
    current_manifest_sha256: &str,
    expected_must_pass: u64,
) -> (Signal, String) {
    let inclusion_contents = match std::fs::read(root.join(MUST_PASS_INCLUSION_PATH)) {
        Ok(contents) => contents,
        Err(err) => {
            return (
                Signal::Fail,
                format!("failed to read {MUST_PASS_INCLUSION_PATH}: {err}"),
            );
        }
    };
    let manifest_contents = match std::fs::read(root.join(MUST_PASS_MANIFEST_PATH)) {
        Ok(contents) => contents,
        Err(err) => {
            return (
                Signal::Fail,
                format!("failed to read {MUST_PASS_MANIFEST_PATH}: {err}"),
            );
        }
    };
    let events_contents = match std::fs::read_to_string(root.join(MUST_PASS_EVENTS_PATH)) {
        Ok(contents) => contents,
        Err(err) => {
            return (
                Signal::Fail,
                format!("failed to read {MUST_PASS_EVENTS_PATH}: {err}"),
            );
        }
    };
    validate_certified_must_pass_against_contents(
        verdict,
        current_source_tree_sha256,
        current_inclusion_sha256,
        current_manifest_sha256,
        &inclusion_contents,
        &manifest_contents,
        &events_contents,
        expected_must_pass,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_certified_must_pass_against_contents(
    verdict: &V,
    current_source_tree_sha256: &str,
    current_inclusion_sha256: &str,
    current_manifest_sha256: &str,
    inclusion_contents: &[u8],
    manifest_contents: &[u8],
    events_contents: &str,
    expected_must_pass: u64,
) -> (Signal, String) {
    let metadata_errors = validate_must_pass_gate_contract(verdict);
    if !metadata_errors.is_empty() {
        return (
            Signal::Fail,
            format!(
                "Must-pass gate metadata invalid: {}",
                metadata_errors.join("; ")
            ),
        );
    }

    let (status, passed, total) = parse_must_pass_gate_verdict(verdict);
    if status != "pass" {
        return (
            Signal::Fail,
            format!("{passed}/{total} must-pass ({status})"),
        );
    }
    if get_str(verdict, "/source_tree_sha256") != current_source_tree_sha256 {
        return (
            Signal::Fail,
            "must-pass evidence source-tree digest does not match current release inputs"
                .to_string(),
        );
    }

    if get_str(verdict, "/inclusion_sha256") != current_inclusion_sha256 {
        return (
            Signal::Fail,
            "must-pass evidence inclusion-list digest does not match the current inclusion list"
                .to_string(),
        );
    }

    if get_str(verdict, "/manifest_sha256") != current_manifest_sha256 {
        return (
            Signal::Fail,
            "must-pass evidence manifest digest does not match the current manifest".to_string(),
        );
    }

    let expected = match canonical_must_pass_entries_from_contents(
        inclusion_contents,
        manifest_contents,
        expected_must_pass,
    ) {
        Ok(entries) => entries,
        Err(err) => return (Signal::Fail, err),
    };
    let observed = match observed_must_pass_event_ids(
        events_contents,
        get_str(verdict, "/run_id"),
        get_str(verdict, "/correlation_id"),
        get_str(verdict, "/git_commit"),
        current_source_tree_sha256,
        current_inclusion_sha256,
        current_manifest_sha256,
    ) {
        Ok(entries) => entries,
        Err(err) => return (Signal::Fail, err),
    };

    if let Err(err) = validate_exact_must_pass_set(&expected, &observed, passed, total) {
        return (Signal::Fail, err);
    }

    let events_sha256 = format!("{:x}", Sha256::digest(events_contents.as_bytes()));
    (
        Signal::Pass,
        format!(
            "{passed}/{total} must-pass: PASS (exact canonical inclusion-list set; git_commit={}; source_tree_sha256={current_source_tree_sha256}; inclusion_sha256={current_inclusion_sha256}; manifest_sha256={current_manifest_sha256}; events_sha256={events_sha256})",
            get_str(verdict, "/git_commit"),
        ),
    )
}

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
    if !matches!(commit.len(), 40 | 64)
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        || commit.bytes().any(|byte| byte.is_ascii_uppercase())
    {
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
                    "must-pass source inputs differ in the {label}; commit them before generating release evidence"
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
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "-z", "--full-tree", commit, "--"])
        .args(source_paths);
    let output = command
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

fn canonical_git_tree_sha256(root: &Path, commit: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "-z", "--full-tree", commit])
        .output()
        .map_err(|err| format!("failed to enumerate canonical Git tree for {commit}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree failed for canonical source tree {commit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(format!("{:x}", Sha256::digest(&output.stdout)))
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

fn ensure_regular_path_without_symlink_components(
    root: &Path,
    relative: &str,
) -> Result<(), String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "release evidence path is not canonical: {relative}"
        ));
    }
    let mut current = root.to_path_buf();
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current).map_err(|err| {
            format!(
                "failed to inspect release evidence path component {}: {err}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "release evidence path traverses a symlink: {}",
                current.display()
            ));
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.file_type().is_file())
            || (!is_last && !metadata.file_type().is_dir())
        {
            return Err(format!(
                "release evidence path component has the wrong type: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn ensure_worktree_bytes_match_tree(
    root: &Path,
    records: &[(String, String, String)],
) -> Result<(), String> {
    for (path, _, expected_blob) in records {
        ensure_regular_path_without_symlink_components(root, path)?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedArtifact {
    head_commit: String,
    contents: Vec<u8>,
}

fn capture_committed_artifact(root: &Path, relative: &str) -> Result<CommittedArtifact, String> {
    ensure_regular_path_without_symlink_components(root, relative)?;
    let (head_commit, records) = capture_must_pass_source_snapshot_for_paths(root, &[relative])?;
    if records.len() != 1 || records[0].0 != relative {
        return Err(format!(
            "release evidence must be one exact tracked file at HEAD: {relative}"
        ));
    }
    let contents = git_commit_file_contents(root, &head_commit, relative)?;
    if current_git_commit(root)? != head_commit {
        return Err(format!(
            "release evidence HEAD changed while reading committed artifact: {relative}"
        ));
    }
    ensure_must_pass_worktree_matches_commit(root, &head_commit, &[relative])?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    Ok(CommittedArtifact {
        head_commit,
        contents,
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

type GitTreeRecord = (String, String, String);

fn capture_must_pass_source_snapshot_for_paths(
    root: &Path,
    source_paths: &[&str],
) -> Result<(String, Vec<GitTreeRecord>), String> {
    let git_commit = current_git_commit(root)?;
    ensure_must_pass_worktree_matches_commit(root, &git_commit, source_paths)?;
    let records = must_pass_tree_records(root, &git_commit, source_paths)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let observed_head = current_git_commit(root)?;
    if observed_head != git_commit {
        return Err(format!(
            "must-pass source HEAD changed during snapshot capture: {git_commit} -> {observed_head}"
        ));
    }
    ensure_must_pass_worktree_matches_commit(root, &git_commit, source_paths)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    Ok((git_commit, records))
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
    let (git_commit, records) =
        capture_must_pass_source_snapshot_for_paths(root, MUST_PASS_EVIDENCE_PATHS)?;
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
        let actual_blob = git_blob_oid(&bytes, expected_blob.len())?;
        if actual_blob != *expected_blob {
            return Err(format!(
                "must-pass evidence worktree bytes differ from the HEAD blob for {path}"
            ));
        }
        contents.insert(path.clone(), bytes);
    }

    if current_git_commit(root)? != git_commit {
        return Err("must-pass evidence HEAD changed during snapshot capture".to_string());
    }
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_EVIDENCE_PATHS)?;
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

fn must_pass_source_tree_sha256_for_paths(
    root: &Path,
    source_paths: &[&str],
) -> Result<String, String> {
    let (_, records) = capture_must_pass_source_snapshot_for_paths(root, source_paths)?;
    Ok(source_tree_sha256(&records))
}

fn capture_must_pass_source_snapshot(root: &Path) -> Result<MustPassSourceSnapshot, String> {
    let (git_commit, records) =
        capture_must_pass_source_snapshot_for_paths(root, MUST_PASS_SOURCE_PATHS)?;
    let inclusion_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_INCLUSION_PATH)?;
    let manifest_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_MANIFEST_PATH)?;
    if current_git_commit(root)? != git_commit {
        return Err("must-pass source HEAD changed while reading canonical blobs".to_string());
    }
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let tracked_paths = records.iter().map(|record| record.0.clone()).collect();
    Ok(MustPassSourceSnapshot {
        git_commit,
        source_tree_sha256: source_tree_sha256(&records),
        inclusion_sha256: format!("{:x}", Sha256::digest(&inclusion_contents)),
        manifest_sha256: format!("{:x}", Sha256::digest(&manifest_contents)),
        inclusion_contents,
        manifest_contents,
        tracked_paths,
    })
}

fn validate_snapshot_artifact_paths(snapshot: &MustPassSourceSnapshot) -> Result<(), String> {
    let manifest: V = serde_json::from_slice(&snapshot.manifest_contents)
        .map_err(|err| format!("failed to parse {MUST_PASS_MANIFEST_PATH}: {err}"))?;
    let extensions = manifest
        .get("extensions")
        .and_then(V::as_array)
        .ok_or_else(|| format!("{MUST_PASS_MANIFEST_PATH} missing extensions array"))?;
    for (index, extension) in extensions.iter().enumerate() {
        let id = extension.get("id").and_then(V::as_str).unwrap_or("unknown");
        let relative = extension
            .get("entry_path")
            .and_then(V::as_str)
            .ok_or_else(|| {
                format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] missing artifact entry_path")
            })?;
        if !is_canonical_artifact_entry_path(relative) {
            return Err(format!(
                "manifest entry {id} has a non-canonical artifact entry_path: {relative:?}"
            ));
        }
        let artifact_path = format!("{MUST_PASS_ARTIFACTS_PATH}/{relative}");
        if !snapshot.tracked_paths.contains(&artifact_path) {
            return Err(format!(
                "manifest entry {id} points to artifact input not tracked by canonical commit {}: {artifact_path}",
                snapshot.git_commit
            ));
        }
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

fn validate_evidence_source_commit(
    root: &Path,
    source_commit: &str,
    current_commit: &str,
) -> Result<(), String> {
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        || source_commit.bytes().any(|byte| byte.is_ascii_uppercase())
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
    if resolved_commit.trim() != source_commit {
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
    if source_commit == current_commit {
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

fn resolve_exact_commit(root: &Path, commit: &str, label: &str) -> Result<(), String> {
    if !matches!(commit.len(), 40 | 64)
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        || commit.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err(format!(
            "{label} is not a canonical full lowercase commit ID"
        ));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", &format!("{commit}^{{commit}}")])
        .output()
        .map_err(|err| format!("failed to resolve {label}: {err}"))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != commit {
        return Err(format!("{label} does not resolve exactly to {commit}"));
    }
    Ok(())
}

fn ensure_commit_ancestor(
    root: &Path,
    ancestor: &str,
    descendant: &str,
    label: &str,
) -> Result<(), String> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map_err(|err| format!("failed to inspect {label} ancestry: {err}"))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(format!(
            "{label} {ancestor} is not an ancestor of release HEAD {descendant}"
        )),
        code => Err(format!(
            "git merge-base failed while inspecting {label} ancestry (status {code:?})"
        )),
    }
}

fn changed_paths_between(root: &Path, source: &str, head: &str) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", "-z", "--no-renames", source, head])
        .output()
        .map_err(|err| format!("failed to inspect evidence-only follow-up paths: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff failed while inspecting evidence-only follow-up paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("git diff returned non-UTF-8 evidence paths: {err}"))
        .map(|paths| {
            paths
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .collect()
        })
}

fn source_package_include_patterns(root: &Path, source: &str) -> Result<Vec<String>, String> {
    let bytes = git_commit_file_contents(root, source, "Cargo.toml")?;
    let cargo_toml = std::str::from_utf8(&bytes)
        .map_err(|err| format!("source Cargo.toml is not UTF-8: {err}"))?
        .parse::<toml::Value>()
        .map_err(|err| format!("failed to parse source Cargo.toml: {err}"))?;
    cargo_toml
        .get("package")
        .and_then(|package| package.get("include"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "source Cargo.toml package.include must be an array".to_string())?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    "source Cargo.toml package.include entries must be non-empty strings"
                        .to_string()
                })
        })
        .collect()
}

fn product_package_includes(path: &str, patterns: &[String]) -> Result<bool, String> {
    patterns.iter().try_fold(false, |matched, raw_pattern| {
        let pattern = raw_pattern.strip_prefix('/').unwrap_or(raw_pattern);
        let compiled = glob::Pattern::new(pattern).map_err(|err| {
            format!("invalid source Cargo.toml include pattern {pattern:?}: {err}")
        })?;
        Ok(matched || compiled.matches(path))
    })
}

fn validate_conformance_source_binding(
    root: &Path,
    artifact: &CommittedArtifact,
    summary: &V,
) -> Result<(), String> {
    let source = get_str(summary, "/git_commit");
    resolve_exact_commit(root, source, "conformance source git_commit")?;
    ensure_commit_ancestor(
        root,
        source,
        &artifact.head_commit,
        "conformance source commit",
    )?;
    let expected_digest = canonical_git_tree_sha256(root, source)?;
    if get_str(summary, "/source_tree_sha256") != expected_digest {
        return Err(
            "conformance source_tree_sha256 does not match the canonical source tree byte stream"
                .to_string(),
        );
    }

    let package_patterns = source_package_include_patterns(root, source)?;
    for path in changed_paths_between(root, source, &artifact.head_commit)? {
        let evidence_only = path.starts_with("tests/e2e_results/")
            || path.starts_with("tests/ext_conformance/reports/")
            || path.starts_with("tests/certification/")
            || path.starts_with("docs/evidence/");
        if !evidence_only {
            return Err(format!(
                "non-evidence path changed after conformance source capture: {path}"
            ));
        }
        if path.starts_with("docs/evidence/") && product_package_includes(&path, &package_patterns)?
        {
            return Err(format!(
                "packaged or product-consumed evidence changed after conformance source capture: {path}"
            ));
        }
    }
    Ok(())
}

fn validate_certified_must_pass(root: &Path, verdict: &V) -> (Signal, String) {
    let evidence_before = match capture_committed_must_pass_evidence(root) {
        Ok(evidence) => evidence,
        Err(err) => return (Signal::Fail, err),
    };
    let committed_verdict: V = match serde_json::from_slice(&evidence_before.verdict_contents) {
        Ok(value) => value,
        Err(err) => {
            return (
                Signal::Fail,
                format!("failed to parse committed {MUST_PASS_VERDICT_PATH}: {err}"),
            );
        }
    };
    if &committed_verdict != verdict {
        return (
            Signal::Fail,
            format!(
                "validated {MUST_PASS_VERDICT_PATH} does not match the commit-bound artifact bytes"
            ),
        );
    }
    let events_contents = match std::str::from_utf8(&evidence_before.events_contents) {
        Ok(contents) => contents,
        Err(err) => {
            return (
                Signal::Fail,
                format!("committed {MUST_PASS_EVENTS_PATH} is not UTF-8: {err}"),
            );
        }
    };
    let snapshot = match capture_must_pass_source_snapshot(root) {
        Ok(snapshot) => snapshot,
        Err(err) => return (Signal::Fail, err),
    };
    if let Err(err) = validate_snapshot_artifact_paths(&snapshot) {
        return (Signal::Fail, err);
    }
    if let Err(err) =
        validate_evidence_source_commit(root, get_str(verdict, "/git_commit"), &snapshot.git_commit)
    {
        return (Signal::Fail, err);
    }
    let result = validate_certified_must_pass_against_contents(
        verdict,
        &snapshot.source_tree_sha256,
        &snapshot.inclusion_sha256,
        &snapshot.manifest_sha256,
        &snapshot.inclusion_contents,
        &snapshot.manifest_contents,
        events_contents,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    );
    let evidence_after = match capture_committed_must_pass_evidence(root) {
        Ok(evidence) => evidence,
        Err(err) => return (Signal::Fail, err),
    };
    if evidence_before != evidence_after {
        return (
            Signal::Fail,
            "committed must-pass evidence changed during release-readiness validation".to_string(),
        );
    }
    let snapshot_after = match capture_must_pass_source_snapshot(root) {
        Ok(snapshot) => snapshot,
        Err(err) => return (Signal::Fail, err),
    };
    if snapshot != snapshot_after {
        return (
            Signal::Fail,
            "must-pass source inputs changed during release-readiness validation".to_string(),
        );
    }
    result
}

fn validate_non_mock_rubric(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema == NON_MOCK_RUBRIC_SCHEMA {
        (Signal::Pass, format!("Non-mock rubric present: {schema}"))
    } else {
        (
            Signal::Fail,
            format!("Invalid schema: expected {NON_MOCK_RUBRIC_SCHEMA}, found {schema}"),
        )
    }
}

fn validate_conformance_summary_metadata(v: &V) -> Result<(), String> {
    let generated_at = v
        .get("generated_at")
        .and_then(V::as_str)
        .ok_or_else(|| "Missing required string: /generated_at".to_string())?;
    let parsed = chrono::DateTime::parse_from_rfc3339(generated_at)
        .map_err(|err| format!("Invalid /generated_at RFC3339 timestamp: {err}"))?;
    if parsed.offset().local_minus_utc() != 0
        || parsed.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) != generated_at
    {
        return Err("/generated_at must use canonical UTC second precision".to_string());
    }
    let generated_at = parsed.with_timezone(&chrono::Utc);
    let now = chrono::Utc::now();
    if generated_at > now + chrono::Duration::minutes(5) {
        return Err(
            "Conformance summary timestamp is more than five minutes in the future".to_string(),
        );
    }
    if now - generated_at > chrono::Duration::hours(CONFORMANCE_MAX_AGE_HOURS) {
        return Err(format!(
            "Conformance summary is stale (older than {CONFORMANCE_MAX_AGE_HOURS} hours)"
        ));
    }

    for pointer in ["/run_id", "/correlation_id"] {
        let value = v
            .pointer(pointer)
            .and_then(V::as_str)
            .ok_or_else(|| format!("Missing required string: {pointer}"))?;
        let valid = value.len() <= 256
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
            });
        if !valid {
            return Err(format!(
                "{pointer} must be a non-empty canonical lineage identifier"
            ));
        }
    }

    let git_commit = v
        .get("git_commit")
        .and_then(V::as_str)
        .ok_or_else(|| "Missing required string: /git_commit".to_string())?;
    if !matches!(git_commit.len(), 40 | 64)
        || !git_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        || git_commit.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err("/git_commit must be a canonical full lowercase object ID".to_string());
    }
    let source_tree_sha256 = v
        .get("source_tree_sha256")
        .and_then(V::as_str)
        .ok_or_else(|| "Missing required string: /source_tree_sha256".to_string())?;
    if source_tree_sha256.len() != 64
        || !source_tree_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || source_tree_sha256
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        return Err("/source_tree_sha256 must be a lowercase SHA-256 digest".to_string());
    }
    Ok(())
}

fn current_conformance_counts(v: &V) -> Result<(u64, u64, u64, u64, u64), String> {
    let count = |name: &str| {
        v.pointer(&format!("/counts/{name}"))
            .and_then(V::as_u64)
            .ok_or_else(|| format!("Missing required unsigned count: /counts/{name}"))
    };
    match (
        count("total"),
        count("tested"),
        count("pass"),
        count("fail"),
        count("na"),
    ) {
        (Ok(total), Ok(tested), Ok(passed), Ok(failed), Ok(not_applicable)) => {
            Ok((total, tested, passed, failed, not_applicable))
        }
        counts => {
            let errors = [counts.0, counts.1, counts.2, counts.3, counts.4]
                .into_iter()
                .filter_map(Result::err)
                .collect::<Vec<_>>();
            Err(errors.join("; "))
        }
    }
}

fn validate_current_conformance_summary(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema != CONFORMANCE_SUMMARY_SCHEMA {
        return (
            Signal::Fail,
            format!("Invalid schema: expected {CONFORMANCE_SUMMARY_SCHEMA}, found {schema}"),
        );
    }

    let (total, tested, passed, failed, not_applicable) = match current_conformance_counts(v) {
        Ok(counts) => counts,
        Err(error) => return (Signal::Fail, error),
    };

    if total == 0 {
        return (
            Signal::Fail,
            "Conformance summary total must be greater than zero".to_string(),
        );
    }
    if passed.checked_add(failed) != Some(tested) {
        return (
            Signal::Fail,
            format!(
                "Conformance tested-count mismatch: pass({passed}) + fail({failed}) != tested({tested})"
            ),
        );
    }
    if tested.checked_add(not_applicable) != Some(total) {
        return (
            Signal::Fail,
            format!(
                "Conformance total-count mismatch: tested({tested}) + na({not_applicable}) != total({total})"
            ),
        );
    }
    if failed > 0 {
        return (
            Signal::Fail,
            format!(
                "Current conformance: {passed}/{total} pass, {failed} fail, {not_applicable} not exercised"
            ),
        );
    }
    if tested != total || not_applicable != 0 {
        return (
            Signal::Fail,
            format!(
                "Current conformance incomplete: {tested}/{total} tested, {not_applicable} not exercised"
            ),
        );
    }

    let Some(pass_rate) = v.get("pass_rate_pct").and_then(V::as_f64) else {
        return (
            Signal::Fail,
            "Missing required numeric value: /pass_rate_pct".to_string(),
        );
    };
    if !pass_rate.is_finite() || pass_rate.to_bits() != 100.0_f64.to_bits() {
        return (
            Signal::Fail,
            format!("Conformance pass_rate_pct mismatch: expected 100, found {pass_rate}"),
        );
    }
    let negative_pass = v.pointer("/negative/pass").and_then(V::as_u64);
    let negative_fail = v.pointer("/negative/fail").and_then(V::as_u64);
    let (Some(negative_pass), Some(negative_fail)) = (negative_pass, negative_fail) else {
        return (
            Signal::Fail,
            "Missing required unsigned negative-test counts".to_string(),
        );
    };
    if negative_fail != 0 {
        return (
            Signal::Fail,
            format!("Conformance negative-policy tests contain {negative_fail} failure(s)"),
        );
    }
    if let Err(err) = validate_conformance_summary_metadata(v) {
        return (Signal::Fail, err);
    }

    (
        Signal::Pass,
        format!(
            "Current conformance complete: {passed}/{total} pass; negative tests: {negative_pass} pass"
        ),
    )
}

fn evaluate_committed_conformance_summary(root: &Path) -> (Signal, String, Option<String>) {
    let artifact_before = match capture_committed_artifact(root, CONFORMANCE_SUMMARY_PATH) {
        Ok(artifact) => artifact,
        Err(err) => return (Signal::Fail, err, None),
    };
    let summary: V = match serde_json::from_slice(&artifact_before.contents) {
        Ok(summary) => summary,
        Err(err) => {
            return (
                Signal::Fail,
                format!("Committed conformance summary is not valid JSON: {err}"),
                None,
            );
        }
    };
    if let Err(err) = validate_conformance_summary_metadata(&summary) {
        return (Signal::Fail, err, None);
    }
    if let Err(err) = validate_conformance_source_binding(root, &artifact_before, &summary) {
        return (Signal::Fail, err, None);
    }
    let (signal, detail) = validate_current_conformance_summary(&summary);
    let artifact_after = match capture_committed_artifact(root, CONFORMANCE_SUMMARY_PATH) {
        Ok(artifact) => artifact,
        Err(err) => return (Signal::Fail, err, None),
    };
    if artifact_before != artifact_after {
        return (
            Signal::Fail,
            "Committed conformance summary changed during validation".to_string(),
            None,
        );
    }
    let sha256 = format!("{:x}", Sha256::digest(&artifact_before.contents));
    (signal, detail, Some(sha256))
}

#[allow(clippy::too_many_lines)]
fn validate_full_suite_gate(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema != FULL_SUITE_GATE_SCHEMA {
        return (
            Signal::Fail,
            format!("Invalid schema: expected {FULL_SUITE_GATE_SCHEMA}, found {schema}"),
        );
    }

    let Some(gates) = v.pointer("/gates").and_then(V::as_array) else {
        return (Signal::Fail, "Missing required gates array".to_string());
    };
    let required_count = |pointer: &str| {
        v.pointer(pointer)
            .and_then(V::as_u64)
            .ok_or_else(|| format!("Missing required unsigned count: {pointer}"))
    };
    let (
        Ok(total),
        Ok(passed),
        Ok(failed),
        Ok(warned),
        Ok(skipped),
        Ok(blocking_pass),
        Ok(blocking_total),
    ) = (
        required_count("/summary/total_gates"),
        required_count("/summary/passed"),
        required_count("/summary/failed"),
        required_count("/summary/warned"),
        required_count("/summary/skipped"),
        required_count("/summary/blocking_pass"),
        required_count("/summary/blocking_total"),
    )
    else {
        let errors = [
            required_count("/summary/total_gates"),
            required_count("/summary/passed"),
            required_count("/summary/failed"),
            required_count("/summary/warned"),
            required_count("/summary/skipped"),
            required_count("/summary/blocking_pass"),
            required_count("/summary/blocking_total"),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
        return (Signal::Fail, errors.join("; "));
    };

    if total == 0 {
        return (
            Signal::Fail,
            "Full-suite summary total_gates must be greater than zero".to_string(),
        );
    }
    if u64::try_from(gates.len()) != Ok(total) {
        return (
            Signal::Fail,
            format!(
                "Full-suite gate count mismatch: summary total_gates={total}, gates={}",
                gates.len()
            ),
        );
    }

    let mut observed_passed = 0u64;
    let mut observed_failed = 0u64;
    let mut observed_warned = 0u64;
    let mut observed_skipped = 0u64;
    let mut observed_blocking_pass = 0u64;
    let mut observed_blocking_total = 0u64;
    for (index, gate) in gates.iter().enumerate() {
        let status = gate.get("status").and_then(V::as_str).unwrap_or("unknown");
        match status {
            "pass" => observed_passed += 1,
            "fail" => observed_failed += 1,
            "warn" => observed_warned += 1,
            "skip" => observed_skipped += 1,
            other => {
                return (
                    Signal::Fail,
                    format!("Invalid status for gate {index}: {other}"),
                );
            }
        }
        let Some(blocking) = gate.get("blocking").and_then(V::as_bool) else {
            return (
                Signal::Fail,
                format!("Missing boolean blocking flag for gate {index}"),
            );
        };
        if blocking {
            observed_blocking_total += 1;
            if status == "pass" {
                observed_blocking_pass += 1;
            }
        }
    }

    let summary_counts = (
        passed,
        failed,
        warned,
        skipped,
        blocking_pass,
        blocking_total,
    );
    let observed_counts = (
        observed_passed,
        observed_failed,
        observed_warned,
        observed_skipped,
        observed_blocking_pass,
        observed_blocking_total,
    );
    if summary_counts != observed_counts {
        return (
            Signal::Fail,
            format!(
                "Full-suite summary mismatch: summary={summary_counts:?}, observed={observed_counts:?}"
            ),
        );
    }

    let Some(all_blocking_pass) = v.pointer("/summary/all_blocking_pass").and_then(V::as_bool)
    else {
        return (
            Signal::Fail,
            "Missing required boolean: /summary/all_blocking_pass".to_string(),
        );
    };
    let observed_all_blocking_pass = blocking_pass == blocking_total;
    if all_blocking_pass != observed_all_blocking_pass {
        return (
            Signal::Fail,
            format!(
                "Full-suite blocking verdict mismatch: all_blocking_pass={all_blocking_pass}, observed={observed_all_blocking_pass}"
            ),
        );
    }

    let expected_verdict = if all_blocking_pass && failed == 0 {
        "pass"
    } else if all_blocking_pass {
        "warn"
    } else {
        "fail"
    };
    let verdict = get_str(v, "/verdict");
    if verdict != expected_verdict {
        return (
            Signal::Fail,
            format!("Full-suite verdict mismatch: expected {expected_verdict}, found {verdict}"),
        );
    }

    let detail = format!(
        "{passed}/{total} gates pass ({verdict}; blocking {blocking_pass}/{blocking_total})"
    );
    match verdict {
        "pass" => (Signal::Pass, detail),
        "warn" => (Signal::Warn, detail),
        "fail" => (Signal::Fail, detail),
        _ => unreachable!("verdict was validated against the computed verdict"),
    }
}

#[allow(clippy::too_many_lines)]
fn validate_practical_finish_checkpoint(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema != PRACTICAL_FINISH_CHECKPOINT_SCHEMA {
        return (
            Signal::Fail,
            format!(
                "Invalid schema: expected {PRACTICAL_FINISH_CHECKPOINT_SCHEMA}, found {schema}"
            ),
        );
    }

    let status = get_str(v, "/status");
    if status != "pass" && status != "fail" {
        return (
            Signal::Fail,
            format!("Invalid status: expected pass|fail, found {status}"),
        );
    }

    let detail = get_str(v, "/detail");
    if detail.trim().is_empty() || detail == "unknown" {
        return (
            Signal::Fail,
            "Missing required detail in practical-finish artifact".to_string(),
        );
    }

    let open_total = get_u64(v, "/open_perf3x_count");
    let technical = get_u64(v, "/technical_open_count");
    let docs_or_report = get_u64(v, "/docs_or_report_open_count");
    if open_total != technical + docs_or_report {
        return (
            Signal::Fail,
            format!(
                "Count mismatch: open_perf3x_count({open_total}) != technical_open_count({technical}) + docs_or_report_open_count({docs_or_report})"
            ),
        );
    }

    let Some(technical_issues) = v.pointer("/technical_open_issues").and_then(V::as_array) else {
        return (
            Signal::Fail,
            "Missing required array: /technical_open_issues".to_string(),
        );
    };
    let Some(docs_or_report_issues) = v
        .pointer("/docs_or_report_open_issues")
        .and_then(V::as_array)
    else {
        return (
            Signal::Fail,
            "Missing required array: /docs_or_report_open_issues".to_string(),
        );
    };

    let technical_issue_count = u64::try_from(technical_issues.len()).unwrap_or(u64::MAX);
    if technical_issue_count != technical {
        return (
            Signal::Fail,
            format!(
                "Count mismatch: technical_open_count({technical}) != technical_open_issues.len()({technical_issue_count})"
            ),
        );
    }
    let docs_issue_count = u64::try_from(docs_or_report_issues.len()).unwrap_or(u64::MAX);
    if docs_issue_count != docs_or_report {
        return (
            Signal::Fail,
            format!(
                "Count mismatch: docs_or_report_open_count({docs_or_report}) != docs_or_report_open_issues.len()({docs_issue_count})"
            ),
        );
    }

    let Some(technical_completion_reached) = v
        .pointer("/technical_completion_reached")
        .and_then(V::as_bool)
    else {
        return (
            Signal::Fail,
            "Missing required bool: /technical_completion_reached".to_string(),
        );
    };
    let residual_scope = get_str(v, "/residual_open_scope");
    let expected_scope = if technical > 0 {
        "technical_remaining"
    } else if docs_or_report > 0 {
        "docs_or_report_only"
    } else {
        "none"
    };
    if residual_scope != expected_scope {
        return (
            Signal::Fail,
            format!("Residual scope mismatch: expected {expected_scope}, found {residual_scope}"),
        );
    }
    if technical_completion_reached != (technical == 0) {
        return (
            Signal::Fail,
            format!(
                "technical_completion_reached mismatch: expected {}, found {technical_completion_reached}",
                technical == 0
            ),
        );
    }

    if status == "pass" && technical > 0 {
        return (
            Signal::Fail,
            format!("Invalid pass status: technical_open_count must be 0, found {technical}"),
        );
    }

    if status == "pass" {
        (
            Signal::Pass,
            format!(
                "Practical-finish checkpoint satisfied: {docs_or_report} docs/report residual issue(s)"
            ),
        )
    } else {
        (
            Signal::Fail,
            format!(
                "Practical-finish checkpoint blocked: technical_open_count={technical}, docs_or_report_open_count={docs_or_report}"
            ),
        )
    }
}

fn find_latest_parameter_sweeps(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for relative in [
        PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL,
        "tests/perf/runs/results/parameter_sweeps.json",
    ] {
        let path = root.join(relative);
        if path.is_file() {
            candidates.push(path);
        }
    }
    let e2e_root = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_root) {
        for entry in entries.flatten() {
            let path = entry.path().join("results/parameter_sweeps.json");
            if path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.into_iter().max()
}

fn parse_positive_u64(value: &V) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

#[allow(clippy::too_many_lines)]
fn validate_parameter_sweeps_artifact(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema != PARAMETER_SWEEPS_SCHEMA {
        return (
            Signal::Fail,
            format!("Invalid schema: expected {PARAMETER_SWEEPS_SCHEMA}, found {schema}"),
        );
    }

    let Some(source_identity) = v.pointer("/source_identity").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /source_identity".to_string(),
        );
    };
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if source_artifact != "phase1_matrix_validation" {
        return (
            Signal::Fail,
            format!(
                "source_identity.source_artifact must be phase1_matrix_validation, found {source_artifact}"
            ),
        );
    }
    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if !source_artifact_path.contains("phase1_matrix_validation.json") {
        return (
            Signal::Fail,
            "source_identity.source_artifact_path must reference phase1_matrix_validation.json"
                .to_string(),
        );
    }

    let Some(readiness) = v.pointer("/readiness").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /readiness".to_string(),
        );
    };
    let readiness_status = readiness
        .get("status")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    let Some(ready_for_phase5) = readiness.get("ready_for_phase5").and_then(V::as_bool) else {
        return (
            Signal::Fail,
            "readiness.ready_for_phase5 must be boolean".to_string(),
        );
    };
    let Some(blocking_reasons) = readiness.get("blocking_reasons").and_then(V::as_array) else {
        return (
            Signal::Fail,
            "readiness.blocking_reasons must be an array".to_string(),
        );
    };
    match readiness_status {
        "ready" => {
            if !ready_for_phase5 {
                return (
                    Signal::Fail,
                    "readiness.ready_for_phase5 must be true when status=ready".to_string(),
                );
            }
            if !blocking_reasons.is_empty() {
                return (
                    Signal::Fail,
                    "readiness.blocking_reasons must be empty when status=ready".to_string(),
                );
            }
        }
        "blocked" => {
            if ready_for_phase5 {
                return (
                    Signal::Fail,
                    "readiness.ready_for_phase5 must be false when status=blocked".to_string(),
                );
            }
            if blocking_reasons.is_empty() {
                return (
                    Signal::Fail,
                    "readiness.blocking_reasons must be non-empty when status=blocked".to_string(),
                );
            }
        }
        _ => {
            return (
                Signal::Fail,
                format!("readiness.status must be ready|blocked, found {readiness_status}"),
            );
        }
    }

    let Some(selected_defaults) = v.pointer("/selected_defaults").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /selected_defaults".to_string(),
        );
    };
    for required in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        let Some(value) = selected_defaults.get(required).and_then(parse_positive_u64) else {
            return (
                Signal::Fail,
                format!("selected_defaults.{required} must be a positive integer"),
            );
        };
        if value == 0 {
            return (
                Signal::Fail,
                format!("selected_defaults.{required} must be > 0"),
            );
        }
    }

    let Some(sweep_plan) = v.pointer("/sweep_plan").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /sweep_plan".to_string(),
        );
    };
    let Some(dimensions) = sweep_plan.get("dimensions").and_then(V::as_array) else {
        return (
            Signal::Fail,
            "sweep_plan.dimensions must be an array".to_string(),
        );
    };
    if dimensions.is_empty() {
        return (
            Signal::Fail,
            "sweep_plan.dimensions must be non-empty".to_string(),
        );
    }

    let mut seen_required = std::collections::BTreeSet::new();
    for dimension in dimensions {
        let Some(dimension_obj) = dimension.as_object() else {
            return (
                Signal::Fail,
                "sweep_plan.dimensions entries must be objects".to_string(),
            );
        };
        let name = dimension_obj
            .get("name")
            .and_then(V::as_str)
            .unwrap_or("unknown")
            .trim();
        if name.is_empty() || name == "unknown" {
            return (
                Signal::Fail,
                "sweep_plan.dimensions[].name must be non-empty".to_string(),
            );
        }
        let Some(candidate_values) = dimension_obj.get("candidate_values").and_then(V::as_array)
        else {
            return (
                Signal::Fail,
                format!("sweep_plan.dimensions[{name}].candidate_values must be an array"),
            );
        };
        if candidate_values.is_empty() {
            return (
                Signal::Fail,
                format!("sweep_plan.dimensions[{name}].candidate_values must be non-empty"),
            );
        }
        if candidate_values
            .iter()
            .any(|value| parse_positive_u64(value).is_none())
        {
            return (
                Signal::Fail,
                format!(
                    "sweep_plan.dimensions[{name}].candidate_values must contain only positive integers"
                ),
            );
        }
        if matches!(
            name,
            "flush_cadence_ms" | "queue_max_items" | "compaction_quota_mb"
        ) {
            seen_required.insert(name.to_string());
        }
    }
    for required in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        if !seen_required.contains(required) {
            return (
                Signal::Fail,
                format!("sweep_plan.dimensions missing required knob {required}"),
            );
        }
    }

    (
        Signal::Pass,
        format!(
            "Parameter sweeps contract valid: readiness={readiness_status}, dimensions={}",
            dimensions.len()
        ),
    )
}

fn check_parameter_sweeps_cert_gate(root: &Path) -> CertEvidence {
    let gate = "parameter_sweeps_integrity".to_string();
    let bead = "bd-3ar8v.6.5.1".to_string();
    let Some(path) = find_latest_parameter_sweeps(root) else {
        return CertEvidence {
            gate,
            bead,
            status: Signal::NoData,
            detail: format!(
                "Artifact not found: {PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL} (or alternate perf/e2e sweep locations)"
            ),
            artifact_path: Some(PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL.to_string()),
            artifact_sha256: None,
        };
    };

    let artifact_path = path
        .strip_prefix(root)
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/");
    let (status, detail, sha) = load_json(&path).map_or_else(
        || {
            (
                Signal::Fail,
                format!("parameter_sweeps artifact is not valid JSON: {artifact_path}"),
                None,
            )
        },
        |v| {
            let (sig, det) = validate_parameter_sweeps_artifact(&v);
            let sha = sha256_file(&path);
            (sig, det, sha)
        },
    );

    CertEvidence {
        gate,
        bead,
        status,
        detail,
        artifact_path: Some(artifact_path),
        artifact_sha256: sha,
    }
}

fn find_latest_opportunity_matrix(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for relative in [
        OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL,
        "tests/perf/runs/results/opportunity_matrix.json",
    ] {
        let path = root.join(relative);
        if path.is_file() {
            candidates.push(path);
        }
    }
    let e2e_root = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_root) {
        for entry in entries.flatten() {
            let path = entry.path().join("results/opportunity_matrix.json");
            if path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.into_iter().max()
}

#[allow(clippy::too_many_lines)]
fn validate_opportunity_matrix_artifact(v: &V) -> (Signal, String) {
    let schema = get_str(v, "/schema");
    if schema != OPPORTUNITY_MATRIX_SCHEMA {
        return (
            Signal::Fail,
            format!("Invalid schema: expected {OPPORTUNITY_MATRIX_SCHEMA}, found {schema}"),
        );
    }

    let Some(source_identity) = v.pointer("/source_identity").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /source_identity".to_string(),
        );
    };
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if source_artifact != "phase1_matrix_validation" {
        return (
            Signal::Fail,
            format!(
                "source_identity.source_artifact must be phase1_matrix_validation, found {source_artifact}"
            ),
        );
    }
    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if !source_artifact_path.contains("phase1_matrix_validation.json") {
        return (
            Signal::Fail,
            "source_identity.source_artifact_path must reference phase1_matrix_validation.json"
                .to_string(),
        );
    }
    let weighted_schema = source_identity
        .get("weighted_bottleneck_schema")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if weighted_schema != "pi.perf.phase1_weighted_bottleneck_attribution.v1" {
        return (
            Signal::Fail,
            format!(
                "source_identity.weighted_bottleneck_schema must be pi.perf.phase1_weighted_bottleneck_attribution.v1, found {weighted_schema}"
            ),
        );
    }
    let weighted_status = source_identity
        .get("weighted_bottleneck_status")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if !matches!(weighted_status, "computed" | "missing") {
        return (
            Signal::Fail,
            format!(
                "source_identity.weighted_bottleneck_status must be computed|missing, found {weighted_status}"
            ),
        );
    }

    let Some(readiness) = v.pointer("/readiness").and_then(V::as_object) else {
        return (
            Signal::Fail,
            "Missing required object: /readiness".to_string(),
        );
    };
    let readiness_status = readiness
        .get("status")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if !matches!(readiness_status, "ready" | "blocked") {
        return (
            Signal::Fail,
            format!("readiness.status must be ready|blocked, found {readiness_status}"),
        );
    }
    let decision = readiness
        .get("decision")
        .and_then(V::as_str)
        .unwrap_or("unknown");
    if !matches!(decision, "RANKED" | "NO_DECISION") {
        return (
            Signal::Fail,
            format!("readiness.decision must be RANKED|NO_DECISION, found {decision}"),
        );
    }
    let Some(ready_for_phase5) = readiness.get("ready_for_phase5").and_then(V::as_bool) else {
        return (
            Signal::Fail,
            "readiness.ready_for_phase5 must be boolean".to_string(),
        );
    };
    let Some(blocking_reasons) = readiness.get("blocking_reasons").and_then(V::as_array) else {
        return (
            Signal::Fail,
            "readiness.blocking_reasons must be an array".to_string(),
        );
    };
    match readiness_status {
        "ready" => {
            if !ready_for_phase5 {
                return (
                    Signal::Fail,
                    "readiness.ready_for_phase5 must be true when status=ready".to_string(),
                );
            }
            if decision != "RANKED" {
                return (
                    Signal::Fail,
                    "readiness.decision must be RANKED when status=ready".to_string(),
                );
            }
            if !blocking_reasons.is_empty() {
                return (
                    Signal::Fail,
                    "readiness.blocking_reasons must be empty when status=ready".to_string(),
                );
            }
        }
        "blocked" => {
            if ready_for_phase5 {
                return (
                    Signal::Fail,
                    "readiness.ready_for_phase5 must be false when status=blocked".to_string(),
                );
            }
            if decision != "NO_DECISION" {
                return (
                    Signal::Fail,
                    "readiness.decision must be NO_DECISION when status=blocked".to_string(),
                );
            }
            if blocking_reasons.is_empty() {
                return (
                    Signal::Fail,
                    "readiness.blocking_reasons must be non-empty when status=blocked".to_string(),
                );
            }
        }
        _ => {}
    }

    let Some(ranked) = v.pointer("/ranked_opportunities").and_then(V::as_array) else {
        return (
            Signal::Fail,
            "Missing required array: /ranked_opportunities".to_string(),
        );
    };
    if readiness_status == "ready" && ranked.is_empty() {
        return (
            Signal::Fail,
            "ranked_opportunities must be non-empty when readiness.status=ready".to_string(),
        );
    }
    if readiness_status == "blocked" && !ranked.is_empty() {
        return (
            Signal::Fail,
            "ranked_opportunities must be empty when readiness.status=blocked".to_string(),
        );
    }
    for (index, row) in ranked.iter().enumerate() {
        let Some(row_obj) = row.as_object() else {
            return (
                Signal::Fail,
                format!("ranked_opportunities[{index}] must be an object"),
            );
        };
        let expected_rank = u64::try_from(index + 1).unwrap_or(u64::MAX);
        let Some(rank) = row_obj.get("rank").and_then(V::as_u64) else {
            return (
                Signal::Fail,
                format!("ranked_opportunities[{index}].rank must be a positive integer"),
            );
        };
        if rank != expected_rank {
            return (
                Signal::Fail,
                format!(
                    "ranked_opportunities[{index}].rank expected {expected_rank}, found {rank}"
                ),
            );
        }
        let stage = row_obj
            .get("stage")
            .and_then(V::as_str)
            .unwrap_or("unknown")
            .trim();
        if stage.is_empty() || stage == "unknown" {
            return (
                Signal::Fail,
                format!("ranked_opportunities[{index}].stage must be non-empty"),
            );
        }
        let Some(priority_score) = row_obj.get("priority_score").and_then(V::as_f64) else {
            return (
                Signal::Fail,
                format!("ranked_opportunities[{index}].priority_score must be numeric"),
            );
        };
        if !priority_score.is_finite() || priority_score <= 0.0 {
            return (
                Signal::Fail,
                format!("ranked_opportunities[{index}].priority_score must be > 0"),
            );
        }
    }

    (
        Signal::Pass,
        format!(
            "Opportunity matrix contract valid: readiness={readiness_status}, ranked_opportunities={}",
            ranked.len()
        ),
    )
}

fn check_opportunity_matrix_cert_gate(root: &Path) -> CertEvidence {
    let gate = "opportunity_matrix_integrity".to_string();
    let bead = "bd-3ar8v.6.5.3".to_string();
    let Some(path) = find_latest_opportunity_matrix(root) else {
        return CertEvidence {
            gate,
            bead,
            status: Signal::NoData,
            detail: format!(
                "Artifact not found: {OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL} (or alternate perf/e2e opportunity_matrix locations)"
            ),
            artifact_path: Some(OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL.to_string()),
            artifact_sha256: None,
        };
    };

    let artifact_path = path
        .strip_prefix(root)
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/");
    let (status, detail, sha) = load_json(&path).map_or_else(
        || {
            (
                Signal::Fail,
                format!("opportunity_matrix artifact is not valid JSON: {artifact_path}"),
                None,
            )
        },
        |v| {
            let (sig, det) = validate_opportunity_matrix_artifact(&v);
            let sha = sha256_file(&path);
            (sig, det, sha)
        },
    );

    CertEvidence {
        gate,
        bead,
        status,
        detail,
        artifact_path: Some(artifact_path),
        artifact_sha256: sha,
    }
}

// ── Evidence collectors ─────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Option<V> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn no_data(name: &str, detail: &str) -> DimensionScore {
    DimensionScore {
        name: name.to_string(),
        signal: Signal::NoData,
        detail: detail.to_string(),
    }
}

fn collect_conformance(root: &Path) -> DimensionScore {
    let name = "Extension Conformance";
    let (signal, detail, _) = evaluate_committed_conformance_summary(root);
    DimensionScore {
        name: name.to_string(),
        signal,
        detail,
    }
}

fn validate_performance_advisory_source_binding(
    root: &Path,
    source_commit: &str,
) -> Result<(), String> {
    let head = current_git_commit(root)?;
    resolve_exact_commit(root, source_commit, "performance source_commit")?;
    ensure_commit_ancestor(root, source_commit, &head, "performance source_commit")?;
    if source_commit == head {
        return Ok(());
    }

    let changed_paths = changed_paths_between(root, source_commit, &head)?;
    if changed_paths.is_empty() {
        return Err(
            "performance source_commit differs from HEAD without a source diff".to_string(),
        );
    }
    let package_patterns = source_package_include_patterns(root, source_commit)?;
    for path in changed_paths {
        let evidence_only = path.starts_with("tests/perf/reports/")
            || path.starts_with("tests/e2e_results/")
            || path.starts_with("tests/ext_conformance/reports/")
            || path.starts_with("tests/certification/")
            || path.starts_with("docs/evidence/");
        if !evidence_only {
            return Err(format!(
                "non-evidence path changed after performance source capture: {path}"
            ));
        }
        if path.starts_with("docs/evidence/") && product_package_includes(&path, &package_patterns)?
        {
            return Err(format!(
                "packaged or product-consumed evidence changed after performance source capture: {path}"
            ));
        }
    }
    Ok(())
}

fn collect_performance_timestamp_errors(v: &V, errors: &mut Vec<String>) {
    let Some(raw) = v.get("generated_at").and_then(V::as_str) else {
        errors
            .push("generated_at must use canonical millisecond-precision UTC RFC3339".to_string());
        return;
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        errors
            .push("generated_at must use canonical millisecond-precision UTC RFC3339".to_string());
        return;
    };
    if parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) != raw {
        errors
            .push("generated_at must use canonical millisecond-precision UTC RFC3339".to_string());
    } else if parsed.with_timezone(&chrono::Utc)
        > chrono::Utc::now() + chrono::TimeDelta::minutes(5)
    {
        errors.push("generated_at is more than five minutes in the future".to_string());
    }
}

fn collect_performance_claim_readiness_errors(v: &V, errors: &mut Vec<String>) {
    let Some(claim) = v.get("claim_readiness").and_then(V::as_object) else {
        errors.push("claim_readiness must be an object".to_string());
        return;
    };
    if claim.get("status").and_then(V::as_str) != Some("claim_ready") {
        errors.push("claim_readiness.status must be claim_ready".to_string());
    }
    if claim
        .get("performance_claims_authorized")
        .and_then(V::as_bool)
        != Some(true)
    {
        errors.push("claim_readiness.performance_claims_authorized must be true".to_string());
    }
    match claim.get("blocking_reason_codes").and_then(V::as_array) {
        Some(blockers) if blockers.is_empty() => {}
        Some(blockers) => errors.push(format!(
            "claim_readiness.blocking_reason_codes must be empty, found {blockers:?}"
        )),
        None => {
            errors.push("claim_readiness.blocking_reason_codes must be an empty array".to_string());
        }
    }
}

fn validate_performance_budget_summary(root: &Path, v: &V) -> (Signal, String) {
    let mut errors = Vec::new();

    let schema = v.get("schema").and_then(V::as_str);
    if schema != Some(PERF_BUDGET_SUMMARY_SCHEMA) {
        errors.push(format!(
            "schema must be {PERF_BUDGET_SUMMARY_SCHEMA}, found {}",
            schema.unwrap_or("missing")
        ));
    }

    if v.get("strict_mode").and_then(V::as_bool) != Some(true) {
        errors.push("strict_mode must be true".to_string());
    }

    collect_performance_timestamp_errors(v, &mut errors);
    collect_performance_claim_readiness_errors(v, &mut errors);

    for pointer in ["/ci_fail", "/ci_no_data", "/data_contract_failures_count"] {
        match v.pointer(pointer).and_then(V::as_u64) {
            Some(0) => {}
            Some(value) => errors.push(format!("{pointer} must be 0, found {value}")),
            None => errors.push(format!("{pointer} must be the integer 0")),
        }
    }

    let total_budgets = v.get("total_budgets").and_then(V::as_u64);
    let pass = v.get("pass").and_then(V::as_u64);
    let fail = v.get("fail").and_then(V::as_u64);
    let no_data = v.get("no_data").and_then(V::as_u64);
    if total_budgets.is_none_or(|count| count == 0)
        || pass != total_budgets
        || fail != Some(0)
        || no_data != Some(0)
    {
        errors.push(format!(
            "all declared budgets must have data and pass: pass={pass:?}, fail={fail:?}, no_data={no_data:?}, total_budgets={total_budgets:?}"
        ));
    }

    let ci_enforced = v.get("ci_enforced").and_then(V::as_u64);
    let ci_with_data = v.get("ci_with_data").and_then(V::as_u64);
    if ci_enforced.is_none_or(|count| count == 0)
        || ci_with_data != ci_enforced
        || ci_enforced
            .zip(total_budgets)
            .is_some_and(|(ci_count, total)| ci_count > total)
    {
        errors.push(format!(
            "CI data coverage must be complete and non-zero: ci_with_data={ci_with_data:?}, ci_enforced={ci_enforced:?}"
        ));
    }

    let source_commit = v.get("source_commit").and_then(V::as_str);
    if source_commit.is_none_or(|commit| {
        commit.len() != 40
            || commit
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || commit.bytes().all(|byte| byte == b'0')
    }) {
        errors.push("source_commit must be a full non-zero lowercase Git SHA".to_string());
    } else if let Some(source_commit) = source_commit
        && let Err(error) = validate_performance_advisory_source_binding(root, source_commit)
    {
        errors.push(error);
    }

    let run_id = v.get("run_id").and_then(V::as_str);
    let correlation_id = v.get("correlation_id").and_then(V::as_str);
    if run_id.is_none_or(|run| run.is_empty() || run.trim() != run) || correlation_id != run_id {
        errors.push("run_id and correlation_id must be identical non-empty strings".to_string());
    }

    if errors.is_empty() {
        let total = total_budgets.unwrap_or_default();
        let pass = pass.unwrap_or_default();
        let ci_enforced = ci_enforced.unwrap_or_default();
        (
            Signal::Pass,
            format!(
                "advisory v2 claim-readiness subset passed: {pass}/{total} pass; {ci_enforced}/{ci_enforced} CI budgets have data; final release evidence gate proof is still required"
            ),
        )
    } else {
        (
            Signal::Fail,
            format!(
                "performance claim evidence is not authorized: {}",
                errors.join("; ")
            ),
        )
    }
}

fn collect_performance(root: &Path) -> DimensionScore {
    let name = "Performance Budgets";
    let path = root.join("tests/perf/reports/budget_summary.json");
    load_json(&path).map_or_else(
        || no_data(name, "budget_summary.json not found"),
        |v| {
            let (signal, detail) = validate_performance_budget_summary(root, &v);
            DimensionScore {
                name: name.to_string(),
                signal,
                detail,
            }
        },
    )
}

fn collect_security(root: &Path) -> DimensionScore {
    let name = "Security & Licensing";
    let path = root.join("tests/ext_conformance/artifacts/RISK_REVIEW.json");
    load_json(&path).map_or_else(
        || no_data(name, "RISK_REVIEW.json not found"),
        |v| {
            let total = get_u64(&v, "/summary/total_artifacts");
            let critical = get_u64(&v, "/summary/security_critical");
            let warnings = get_u64(&v, "/summary/security_warnings");
            let license_clear = get_u64(&v, "/summary/license_clear");
            let license_unknown = get_u64(&v, "/summary/license_unknown");
            let overall_risk = get_str(&v, "/summary/overall_risk");

            let signal = if critical > 0 {
                Signal::Fail
            } else if warnings > 0 || license_unknown > 0 {
                Signal::Warn
            } else {
                Signal::Pass
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{total} artifacts: {license_clear} license-clear, {license_unknown} unknown; {critical} critical, {warnings} warnings; risk={overall_risk}"
                ),
            }
        },
    )
}

fn collect_provenance(root: &Path) -> DimensionScore {
    let name = "Provenance Integrity";
    let path = root.join("tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json");
    load_json(&path).map_or_else(
        || no_data(name, "PROVENANCE_VERIFICATION.json not found"),
        |v| {
            let total = get_u64(&v, "/summary/total_artifacts");
            let verified = get_u64(&v, "/summary/verified_ok");
            let failed = get_u64(&v, "/summary/failed");
            let pass_rate = get_f64(&v, "/summary/pass_rate");

            let signal = if failed > 0 {
                Signal::Fail
            } else if pass_rate >= 1.0 {
                Signal::Pass
            } else {
                Signal::Warn
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{verified}/{total} verified ({:.0}%), {failed} failed",
                    pass_rate * 100.0
                ),
            }
        },
    )
}

fn collect_traceability(root: &Path) -> DimensionScore {
    let name = "Traceability";
    let path = root.join("docs/traceability_matrix.json");
    load_json(&path).map_or_else(
        || no_data(name, "traceability_matrix.json not found"),
        |v| {
            let requirements = v
                .get("requirements")
                .and_then(V::as_array)
                .map_or(0, Vec::len);
            let min_coverage = get_f64(&v, "/ci_policy/min_classified_trace_coverage_pct");

            let signal = if requirements > 0 {
                Signal::Pass
            } else {
                Signal::Fail
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{requirements} requirements traced; min coverage threshold: {min_coverage:.0}%"
                ),
            }
        },
    )
}

fn collect_baseline_delta(root: &Path) -> DimensionScore {
    let name = "Baseline Conformance";
    let path = root.join("tests/ext_conformance/reports/conformance_baseline.json");
    load_json(&path).map_or_else(
        || no_data(name, "conformance_baseline.json not found"),
        |v| {
            let pass_rate = get_f64(&v, "/extension_conformance/pass_rate_pct");
            let passed = get_u64(&v, "/extension_conformance/passed");
            let total = get_u64(&v, "/extension_conformance/manifest_count");
            let git_ref = get_str(&v, "/git_ref");
            let scenario_rate = get_f64(&v, "/scenario_conformance/pass_rate_pct");

            let signal = if pass_rate >= 90.0 && scenario_rate >= 80.0 {
                Signal::Pass
            } else if pass_rate >= 70.0 {
                Signal::Warn
            } else {
                Signal::Fail
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "ext: {passed}/{total} ({pass_rate:.1}%); scenarios: {scenario_rate:.1}%; ref={git_ref}"
                ),
            }
        },
    )
}

fn collect_known_issues(root: &Path) -> Vec<String> {
    let mut issues = Vec::new();

    // Conformance failures
    let baseline_path = root.join("tests/ext_conformance/reports/conformance_baseline.json");
    if let Some(v) = load_json(&baseline_path)
        && let Some(arr) = v
            .pointer("/scenario_conformance/failures")
            .and_then(V::as_array)
    {
        for f in arr {
            let id = get_str(f, "/id");
            let cause = get_str(f, "/cause");
            issues.push(format!("Scenario {id}: {cause}"));
        }
    }

    // Performance evidence that cannot authorize release-facing claims.
    let perf_path = root.join("tests/perf/reports/budget_summary.json");
    if let Some(v) = load_json(&perf_path) {
        let (signal, detail) = validate_performance_budget_summary(root, &v);
        if signal != Signal::Pass {
            issues.push(format!("Performance budgets: {detail}"));
        }
    }

    // Security warnings
    let risk_path = root.join("tests/ext_conformance/artifacts/RISK_REVIEW.json");
    if let Some(v) = load_json(&risk_path) {
        let warnings = get_u64(&v, "/summary/security_warnings");
        if warnings > 0 {
            issues.push(format!(
                "{warnings} extension artifacts have security warnings"
            ));
        }
        let unknown = get_u64(&v, "/summary/license_unknown");
        if unknown > 0 {
            issues.push(format!(
                "{unknown} extension artifacts have unknown licenses"
            ));
        }
    }

    issues
}

fn generate_report() -> ReleaseReadinessReport {
    let root = repo_root();

    let dimensions = vec![
        collect_conformance(&root),
        collect_baseline_delta(&root),
        collect_performance(&root),
        collect_security(&root),
        collect_provenance(&root),
        collect_traceability(&root),
    ];

    // Overall verdict: Fail if any dimension fails, Warn if any warns, else Pass
    let overall = if dimensions.iter().any(|d| d.signal == Signal::Fail) {
        Signal::Fail
    } else if dimensions.iter().any(|d| d.signal == Signal::Warn) {
        Signal::Warn
    } else if dimensions.iter().all(|d| d.signal == Signal::NoData) {
        Signal::NoData
    } else {
        Signal::Pass
    };

    let known_issues = collect_known_issues(&root);

    ReleaseReadinessReport {
        schema: REPORT_SCHEMA.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        overall_verdict: overall,
        dimensions,
        known_issues,
        reproduce_command: "./scripts/e2e/run_all.sh --profile ci".to_string(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn generate_release_readiness_report() {
    let report = generate_report();
    eprintln!("{}", report.render_markdown());

    assert_eq!(report.dimensions.len(), 6);
    assert_eq!(report.schema, REPORT_SCHEMA);

    let json = serde_json::to_string_pretty(&report).expect("serialize");
    let parsed: V = serde_json::from_str(&json).expect("parse");
    assert!(parsed.get("schema").is_some());
    assert!(parsed.get("overall_verdict").is_some());
    assert!(parsed.get("dimensions").is_some());
}

#[test]
fn conformance_dimension_has_data() {
    let dim = collect_conformance(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "conformance: {}", dim.detail);
    assert_eq!(
        dim.signal,
        Signal::Fail,
        "partial checked-in coverage must fail closed: {}",
        dim.detail
    );
    assert!(
        dim.detail.contains("git_commit") || dim.detail.contains("source_tree_sha256"),
        "checked-in summary must fail until regenerated with source provenance: {}",
        dim.detail
    );
}

fn complete_conformance_summary_fixture() -> V {
    serde_json::json!({
        "schema": CONFORMANCE_SUMMARY_SCHEMA,
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "run_id": "run-123",
        "correlation_id": "corr-123",
        "git_commit": "0123456789abcdef0123456789abcdef01234567",
        "source_tree_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "counts": { "total": 10, "tested": 10, "pass": 10, "fail": 0, "na": 0 },
        "pass_rate_pct": 100.0,
        "negative": { "pass": 1, "fail": 0 }
    })
}

#[test]
fn conformance_dimension_fail_closed_when_lineage_missing() {
    let mut summary = complete_conformance_summary_fixture();
    summary
        .as_object_mut()
        .expect("summary object")
        .remove("run_id");
    summary
        .as_object_mut()
        .expect("summary object")
        .remove("correlation_id");
    let detail = validate_conformance_summary_metadata(&summary)
        .expect_err("missing lineage must fail closed");
    assert!(
        detail.contains("run_id"),
        "expected missing lineage in detail, got: {detail}"
    );
}

#[test]
fn conformance_dimension_fail_closed_when_run_id_missing() {
    let mut summary = complete_conformance_summary_fixture();
    summary
        .as_object_mut()
        .expect("summary object")
        .remove("run_id");
    let detail = validate_conformance_summary_metadata(&summary)
        .expect_err("missing run_id must fail closed");
    assert!(
        detail.contains("run_id"),
        "expected missing run_id in detail, got: {detail}"
    );
}

#[test]
fn conformance_dimension_fail_closed_when_correlation_id_missing() {
    let mut summary = complete_conformance_summary_fixture();
    summary
        .as_object_mut()
        .expect("summary object")
        .remove("correlation_id");
    let detail = validate_conformance_summary_metadata(&summary)
        .expect_err("missing correlation_id must fail closed");
    assert!(
        detail.contains("correlation_id"),
        "expected missing correlation_id in detail, got: {detail}"
    );
}

#[test]
fn conformance_dimension_accepts_lineage_when_present() {
    let summary = complete_conformance_summary_fixture();
    let (signal, detail) = validate_current_conformance_summary(&summary);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn performance_dimension_has_data() {
    let dim = collect_performance(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "performance: {}", dim.detail);
}

fn claim_ready_performance_budget_fixture(source_commit: &str) -> V {
    serde_json::json!({
        "schema": PERF_BUDGET_SUMMARY_SCHEMA,
        "generated_at": chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "source_commit": source_commit,
        "run_id": "perf-run-123",
        "correlation_id": "perf-run-123",
        "strict_mode": true,
        "total_budgets": 2,
        "pass": 2,
        "fail": 0,
        "no_data": 0,
        "ci_enforced": 2,
        "ci_with_data": 2,
        "ci_fail": 0,
        "ci_no_data": 0,
        "data_contract_failures_count": 0,
        "claim_readiness": {
            "status": "claim_ready",
            "performance_claims_authorized": true,
            "blocking_reason_codes": []
        }
    })
}

fn performance_source_repository_fixture() -> tempfile::TempDir {
    let root = tempdir().expect("create performance source-binding fixture");
    run_evidence_binding_fixture_git(root.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.path().join("source.txt"), "fixture source\n")
        .expect("write performance source-binding fixture");
    run_evidence_binding_fixture_git(root.path(), &["add", "source.txt"]);
    run_evidence_binding_fixture_git(
        root.path(),
        &[
            "-c",
            "user.name=Pi Performance Fixture",
            "-c",
            "user.email=pi-performance@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture source",
        ],
    );
    root
}

#[test]
fn performance_budget_v2_claim_ready_contract_passes() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let summary = claim_ready_performance_budget_fixture(&source_commit);
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn performance_budget_v2_blocked_claim_fails_closed() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["claim_readiness"] = serde_json::json!({
        "status": "blocked",
        "performance_claims_authorized": false,
        "blocking_reason_codes": ["ci_budget_data_missing"]
    });
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains("claim_readiness.status"), "{detail}");
    assert!(detail.contains("ci_budget_data_missing"), "{detail}");
}

#[test]
fn performance_budget_legacy_v1_cannot_authorize_claims() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["schema"] = serde_json::json!("pi.perf.budget_summary.v1");
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains(PERF_BUDGET_SUMMARY_SCHEMA), "{detail}");
}

#[test]
fn performance_budget_future_timestamp_fails_closed() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["generated_at"] = serde_json::json!(
        (chrono::Utc::now() + chrono::TimeDelta::minutes(6))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(
        detail.contains("more than five minutes in the future"),
        "{detail}"
    );
}

#[test]
fn performance_budget_forged_source_commit_fails_closed() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["source_commit"] = serde_json::json!("fedcba9876543210fedcba9876543210fedcba98");
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(
        detail.contains("source_commit") || detail.contains("source object"),
        "{detail}"
    );
}

#[test]
fn performance_budget_global_no_data_fails_even_when_ci_subset_is_green() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["total_budgets"] = serde_json::json!(3);
    summary["pass"] = serde_json::json!(2);
    summary["no_data"] = serde_json::json!(1);
    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains("all declared budgets"), "{detail}");
}

#[test]
fn performance_budget_rejects_ci_count_larger_than_total_budget_count() {
    let root = performance_source_repository_fixture();
    let source_commit = current_git_commit(root.path()).expect("resolve fixture source commit");
    let mut summary = claim_ready_performance_budget_fixture(&source_commit);
    summary["ci_enforced"] = serde_json::json!(99);
    summary["ci_with_data"] = serde_json::json!(99);

    let (signal, detail) = validate_performance_budget_summary(root.path(), &summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains("CI data coverage"), "{detail}");
}

#[test]
fn security_dimension_has_data() {
    let dim = collect_security(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "security: {}", dim.detail);
}

#[test]
fn provenance_dimension_has_data() {
    let dim = collect_provenance(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "provenance: {}", dim.detail);
}

#[test]
fn traceability_dimension_has_data() {
    let dim = collect_traceability(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "traceability: {}", dim.detail);
}

#[test]
fn baseline_dimension_has_data() {
    let dim = collect_baseline_delta(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "baseline: {}", dim.detail);
}

#[test]
fn overall_verdict_reflects_dimensions() {
    let report = generate_report();
    let has_fail = report.dimensions.iter().any(|d| d.signal == Signal::Fail);
    let has_warn = report.dimensions.iter().any(|d| d.signal == Signal::Warn);

    if has_fail {
        assert_eq!(report.overall_verdict, Signal::Fail);
    } else if has_warn {
        assert_eq!(report.overall_verdict, Signal::Warn);
    } else {
        assert_eq!(report.overall_verdict, Signal::Pass);
    }
}

#[test]
fn known_issues_are_collected() {
    let issues = collect_known_issues(&repo_root());
    eprintln!("Known issues ({}):", issues.len());
    for issue in &issues {
        eprintln!("  - {issue}");
    }
}

#[test]
fn report_json_roundtrip() {
    let report = generate_report();
    let json = serde_json::to_string(&report).expect("serialize");
    let back: ReleaseReadinessReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.overall_verdict, report.overall_verdict);
    assert_eq!(back.dimensions.len(), report.dimensions.len());
}

#[test]
fn report_markdown_contains_all_dimensions() {
    let md = generate_report().render_markdown();
    assert!(md.contains("Extension Conformance"));
    assert!(md.contains("Performance Budgets"));
    assert!(md.contains("Security & Licensing"));
    assert!(md.contains("Provenance Integrity"));
    assert!(md.contains("Traceability"));
    assert!(md.contains("Baseline Conformance"));
    assert!(md.contains("Overall Verdict"));
}

#[test]
fn signal_display_format() {
    assert_eq!(Signal::Pass.to_string(), "PASS");
    assert_eq!(Signal::Warn.to_string(), "WARN");
    assert_eq!(Signal::Fail.to_string(), "FAIL");
    assert_eq!(Signal::NoData.to_string(), "NO_DATA");
}

#[test]
fn signal_serde_roundtrip() {
    for s in [Signal::Pass, Signal::Warn, Signal::Fail, Signal::NoData] {
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Signal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }
}

// ── Final QA Certification (bd-1f42.7.3) ────────────────────────────────────

const CERT_SCHEMA: &str = "pi.qa.final_certification.v1";
const GENERATE_FINAL_CERTIFICATION_ENV: &str = "PI_GENERATE_FINAL_CERTIFICATION";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CertEvidence {
    gate: String,
    bead: String,
    status: Signal,
    detail: String,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RiskEntry {
    id: String,
    severity: String,
    description: String,
    mitigation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FinalCertification {
    schema: String,
    generated_at: String,
    git_commit: String,
    source_tree_sha256: String,
    certification_verdict: Signal,
    evidence: Vec<CertEvidence>,
    risk_register: Vec<RiskEntry>,
    reproduce_commands: Vec<String>,
    ci_run_link_template: String,
}

fn aggregate_certification_signals(signals: &[Signal]) -> Signal {
    if signals.is_empty() {
        Signal::NoData
    } else if signals.contains(&Signal::Fail) {
        Signal::Fail
    } else if signals.contains(&Signal::Warn) {
        Signal::Warn
    } else if signals.contains(&Signal::NoData) {
        Signal::NoData
    } else {
        Signal::Pass
    }
}

const PHASE5_GO_NO_GO_GATES: &[&str] = &[
    "practical_finish_checkpoint",
    "extension_remediation_backlog",
    "parameter_sweeps_integrity",
    "opportunity_matrix_integrity",
];

#[derive(Debug, Clone)]
struct Phase5SnapshotRow {
    gate: &'static str,
    status: Signal,
    detail: String,
}

fn build_phase5_go_no_go_snapshot(
    cert: &FinalCertification,
) -> (Vec<Phase5SnapshotRow>, &'static str) {
    let mut rows = Vec::with_capacity(PHASE5_GO_NO_GO_GATES.len());
    let mut all_pass = true;

    for gate in PHASE5_GO_NO_GO_GATES {
        if let Some(evidence) = cert.evidence.iter().find(|entry| entry.gate == *gate) {
            if evidence.status != Signal::Pass {
                all_pass = false;
            }
            rows.push(Phase5SnapshotRow {
                gate,
                status: evidence.status,
                detail: evidence.detail.clone(),
            });
            continue;
        }

        all_pass = false;
        rows.push(Phase5SnapshotRow {
            gate,
            status: Signal::NoData,
            detail: "MISSING from certification evidence (fail-closed)".to_string(),
        });
    }

    let decision = if all_pass { "GO" } else { "NO-GO" };
    (rows, decision)
}

fn sha256_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(data)))
}

fn check_cert_gate(
    root: &Path,
    gate: &str,
    bead: &str,
    artifact_rel: &str,
    check: impl FnOnce(&V) -> (Signal, String),
) -> CertEvidence {
    let (status, detail, sha) = match capture_committed_artifact(root, artifact_rel) {
        Err(err) => (Signal::Fail, err, None),
        Ok(artifact) => match serde_json::from_slice::<V>(&artifact.contents) {
            Err(err) => (
                Signal::Fail,
                format!("Committed artifact is not valid JSON ({artifact_rel}): {err}"),
                None,
            ),
            Ok(value) => {
                let (signal, detail) = check(&value);
                let sha = format!("{:x}", Sha256::digest(&artifact.contents));
                (signal, detail, Some(sha))
            }
        },
    };
    CertEvidence {
        gate: gate.to_string(),
        bead: bead.to_string(),
        status,
        detail,
        artifact_path: Some(artifact_rel.to_string()),
        artifact_sha256: sha,
    }
}

fn check_conformance_cert_gate(root: &Path, gate: &str, bead: &str) -> CertEvidence {
    let (status, detail, artifact_sha256) = evaluate_committed_conformance_summary(root);
    CertEvidence {
        gate: gate.to_string(),
        bead: bead.to_string(),
        status,
        detail,
        artifact_path: Some(CONFORMANCE_SUMMARY_PATH.to_string()),
        artifact_sha256,
    }
}

#[allow(clippy::too_many_lines)]
fn generate_certification() -> FinalCertification {
    let root = repo_root();
    let git_commit = current_git_commit(&root).expect("resolve final-certification source commit");
    let source_tree_sha256 = canonical_git_tree_sha256(&root, &git_commit)
        .expect("hash final-certification source tree");
    let mut evidence = Vec::new();

    // 1. Non-mock unit compliance
    evidence.push(check_cert_gate(
        &root,
        "non_mock_compliance",
        "bd-1f42.2.6",
        "docs/non-mock-rubric.json",
        validate_non_mock_rubric,
    ));

    // 2. Full E2E evidence
    evidence.push(check_conformance_cert_gate(
        &root,
        "e2e_evidence",
        "bd-1f42.3",
    ));

    // 3. Exact proof for the current canonical extension inclusion-list set
    let must_pass_root = root.clone();
    evidence.push(check_cert_gate(
        &root,
        "must_pass_current",
        "bd-1f42.4",
        MUST_PASS_VERDICT_PATH,
        move |v| validate_certified_must_pass(&must_pass_root, v),
    ));

    // 4. Evidence bundle
    evidence.push(check_cert_gate(
        &root,
        "evidence_bundle",
        "bd-1f42.6.8",
        "tests/evidence_bundle/index.json",
        |v| {
            let schema = get_str(v, "/schema");
            let total = get_u64(v, "/summary/total_artifacts");
            let verdict = get_str(v, "/summary/verdict");
            if schema.starts_with("pi.ci.evidence_bundle") && total > 0 && verdict == "complete" {
                (
                    Signal::Pass,
                    format!("Evidence bundle: {total} artifacts collected ({verdict})"),
                )
            } else {
                (
                    Signal::Fail,
                    format!("Evidence bundle incomplete or missing ({verdict}, artifacts={total})"),
                )
            }
        },
    ));

    // 5. Cross-platform matrix
    let platform = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    };
    let xplat_path = format!("tests/cross_platform_reports/{platform}/platform_report.json");
    evidence.push(check_cert_gate(
        &root,
        "cross_platform",
        "bd-1f42.6.7",
        &xplat_path,
        |v| {
            let total = get_u64(v, "/summary/total_checks");
            let passed = get_u64(v, "/summary/passed");
            if total > 0 && passed == total {
                (
                    Signal::Pass,
                    format!("{passed}/{total} platform checks pass"),
                )
            } else if total > 0 {
                (
                    Signal::Warn,
                    format!("{passed}/{total} platform checks pass"),
                )
            } else {
                (Signal::NoData, "No platform checks found".to_string())
            }
        },
    ));

    // 6. Full-suite gate
    evidence.push(check_cert_gate(
        &root,
        "full_suite_gate",
        "bd-1f42.6.5",
        "tests/full_suite_gate/full_suite_verdict.json",
        validate_full_suite_gate,
    ));

    // 7. Conformance baseline delta
    evidence.push(check_cert_gate(
        &root,
        "extension_remediation_backlog",
        "bd-3ar8v.6.8.3",
        "tests/full_suite_gate/extension_remediation_backlog.json",
        |v| {
            let schema = get_str(v, "/schema");
            let entries = v
                .pointer("/entries")
                .and_then(V::as_array)
                .map_or(0u64, |items| u64::try_from(items.len()).unwrap_or(u64::MAX));
            let summary_total = get_u64(v, "/summary/total_non_pass_extensions");
            let actionable = get_u64(v, "/summary/actionable");
            let non_actionable = get_u64(v, "/summary/non_actionable");

            if schema != EXT_REMEDIATION_BACKLOG_SCHEMA {
                return (
                    Signal::Fail,
                    format!(
                        "Invalid schema: expected {EXT_REMEDIATION_BACKLOG_SCHEMA}, found {schema}"
                    ),
                );
            }
            if summary_total != entries {
                return (
                    Signal::Fail,
                    format!(
                        "Summary mismatch: total_non_pass_extensions={summary_total}, entries={entries}"
                    ),
                );
            }
            if actionable + non_actionable != summary_total {
                return (
                    Signal::Fail,
                    format!(
                        "Summary mismatch: actionable({actionable}) + non_actionable({non_actionable}) != total({summary_total})"
                    ),
                );
            }

            (
                Signal::Pass,
                format!(
                    "Remediation backlog valid: {entries} entries ({actionable} actionable, {non_actionable} non-actionable)"
                ),
            )
        },
    ));

    // 8. Practical-finish checkpoint (docs-only residual filter)
    evidence.push(check_cert_gate(
        &root,
        "practical_finish_checkpoint",
        "bd-3ar8v.6.9",
        "tests/full_suite_gate/practical_finish_checkpoint.json",
        validate_practical_finish_checkpoint,
    ));

    // 9. Parameter-sweeps certification linkage
    evidence.push(check_parameter_sweeps_cert_gate(&root));

    // 10. Opportunity-matrix certification linkage
    evidence.push(check_opportunity_matrix_cert_gate(&root));

    // 11. Current conformance health
    evidence.push(check_conformance_cert_gate(
        &root,
        "health_delta",
        "bd-1f42.4.5",
    ));

    // Build risk register from any non-pass evidence
    let mut risk_register = Vec::new();
    for ev in &evidence {
        match ev.status {
            Signal::Fail => {
                risk_register.push(RiskEntry {
                    id: ev.bead.clone(),
                    severity: "high".to_string(),
                    description: format!("{}: {}", ev.gate, ev.detail),
                    mitigation: format!("Investigate and fix before release (bead {})", ev.bead),
                });
            }
            Signal::Warn => {
                risk_register.push(RiskEntry {
                    id: ev.bead.clone(),
                    severity: "medium".to_string(),
                    description: format!("{}: {}", ev.gate, ev.detail),
                    mitigation: format!("Monitor and track in bead {}", ev.bead),
                });
            }
            Signal::NoData => {
                risk_register.push(RiskEntry {
                    id: ev.bead.clone(),
                    severity: "low".to_string(),
                    description: format!("{}: {}", ev.gate, ev.detail),
                    mitigation:
                        "Artifact generated by CI pipeline only; not available in local builds"
                            .to_string(),
                });
            }
            Signal::Pass => {}
        }
    }

    let evidence_signals = evidence.iter().map(|item| item.status).collect::<Vec<_>>();
    let cert_verdict = aggregate_certification_signals(&evidence_signals);

    FinalCertification {
        schema: CERT_SCHEMA.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        git_commit,
        source_tree_sha256,
        certification_verdict: cert_verdict,
        evidence,
        risk_register,
        reproduce_commands: vec![
            "cargo test --all-targets".to_string(),
            "./scripts/e2e/run_all.sh --profile ci".to_string(),
            "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture --exact".to_string(),
        ],
        ci_run_link_template: "https://github.com/<owner>/<repo>/actions/runs/<run_id>"
            .to_string(),
    }
}

fn render_certification_markdown(cert: &FinalCertification) -> String {
    let mut out = String::new();
    out.push_str("# Final QA Certification Report\n\n");
    let _ = writeln!(out, "**Schema**: {}", cert.schema);
    let _ = writeln!(out, "**Generated**: {}", cert.generated_at);
    let _ = writeln!(out, "**Source Commit**: {}", cert.git_commit);
    let _ = writeln!(out, "**Source Tree SHA-256**: {}", cert.source_tree_sha256);
    let _ = writeln!(
        out,
        "**Certification Verdict**: {}\n",
        cert.certification_verdict
    );

    out.push_str("## Evidence Gates\n\n");
    out.push_str("| Gate | Bead | Status | Artifact | Detail |\n");
    out.push_str("|------|------|--------|----------|--------|\n");
    for ev in &cert.evidence {
        let artifact = ev.artifact_path.as_deref().unwrap_or("-");
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            ev.gate, ev.bead, ev.status, artifact, ev.detail
        );
    }
    out.push('\n');

    let (phase5_snapshot, phase5_decision) = build_phase5_go_no_go_snapshot(cert);
    out.push_str("## Phase-5 Go/No-Go Snapshot\n\n");
    out.push_str("| Gate | Status | Detail |\n");
    out.push_str("|------|--------|--------|\n");
    for row in &phase5_snapshot {
        let detail = row.detail.replace('|', "\\|");
        let _ = writeln!(out, "| {} | {} | {} |", row.gate, row.status, detail);
    }
    out.push('\n');
    let _ = writeln!(out, "**Snapshot Decision**: {phase5_decision}");
    out.push_str("**Fail-Closed Rule**: missing gate or non-PASS status => NO-GO\n\n");

    if !cert.risk_register.is_empty() {
        out.push_str("## Risk Register\n\n");
        out.push_str("| ID | Severity | Description | Mitigation |\n");
        out.push_str("|----|----------|-------------|------------|\n");
        for risk in &cert.risk_register {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                risk.id, risk.severity, risk.description, risk.mitigation
            );
        }
        out.push('\n');
    }

    out.push_str("## Reproduction Commands\n\n");
    for cmd in &cert.reproduce_commands {
        let _ = writeln!(out, "```\n{cmd}\n```");
    }
    out
}

fn final_certification_generation_requested(value: Option<&str>) -> bool {
    value.is_some_and(|candidate| candidate.trim() == "1")
}

fn write_final_certification_artifacts(
    root: &Path,
    cert: &FinalCertification,
    markdown: &str,
    requested: bool,
) -> Result<bool, String> {
    if !requested {
        return Ok(false);
    }
    if cert.certification_verdict != Signal::Pass {
        return Err(format!(
            "refusing to generate final certification artifacts with verdict {}",
            cert.certification_verdict
        ));
    }

    let out_dir = root.join("tests/certification");
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create final certification directory: {err}"))?;
    let json = serde_json::to_string_pretty(cert)
        .map_err(|err| format!("failed to serialize final certification: {err}"))?;
    std::fs::write(out_dir.join("final_certification.json"), json)
        .map_err(|err| format!("failed to write final certification JSON: {err}"))?;
    std::fs::write(out_dir.join("final_certification.md"), markdown)
        .map_err(|err| format!("failed to write final certification markdown: {err}"))?;

    let mut events = String::new();
    for evidence in &cert.evidence {
        let event = serde_json::json!({
            "schema": "pi.qa.certification_event.v1",
            "timestamp": cert.generated_at,
            "git_commit": cert.git_commit,
            "source_tree_sha256": cert.source_tree_sha256,
            "gate": evidence.gate,
            "bead": evidence.bead,
            "status": evidence.status,
            "detail": evidence.detail,
            "artifact_sha256": evidence.artifact_sha256,
        });
        writeln!(
            events,
            "{}",
            serde_json::to_string(&event)
                .map_err(|err| format!("failed to serialize certification event: {err}"))?
        )
        .map_err(|err| format!("failed to render certification event: {err}"))?;
    }
    std::fs::write(out_dir.join("certification_events.jsonl"), events)
        .map_err(|err| format!("failed to write certification events: {err}"))?;
    Ok(true)
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_qa_certification() {
    let cert = generate_certification();
    let md = render_certification_markdown(&cert);
    eprintln!("{md}");

    // Schema
    assert_eq!(cert.schema, CERT_SCHEMA);

    // 11 evidence gates
    assert_eq!(cert.evidence.len(), 11, "Expected 11 evidence gates");

    // Verify gate IDs
    let gate_ids: Vec<&str> = cert.evidence.iter().map(|e| e.gate.as_str()).collect();
    assert!(
        gate_ids.contains(&"non_mock_compliance"),
        "Missing non_mock_compliance gate"
    );
    assert!(
        gate_ids.contains(&"e2e_evidence"),
        "Missing e2e_evidence gate"
    );
    assert!(
        gate_ids.contains(&"must_pass_current"),
        "Missing must_pass_current gate"
    );
    assert!(
        gate_ids.contains(&"evidence_bundle"),
        "Missing evidence_bundle gate"
    );
    assert!(
        gate_ids.contains(&"cross_platform"),
        "Missing cross_platform gate"
    );
    assert!(
        gate_ids.contains(&"full_suite_gate"),
        "Missing full_suite_gate gate"
    );
    assert!(
        gate_ids.contains(&"extension_remediation_backlog"),
        "Missing extension_remediation_backlog gate"
    );
    assert!(
        gate_ids.contains(&"practical_finish_checkpoint"),
        "Missing practical_finish_checkpoint gate"
    );
    assert!(
        gate_ids.contains(&"parameter_sweeps_integrity"),
        "Missing parameter_sweeps_integrity gate"
    );
    assert!(
        gate_ids.contains(&"opportunity_matrix_integrity"),
        "Missing opportunity_matrix_integrity gate"
    );
    assert!(
        gate_ids.contains(&"health_delta"),
        "Missing health_delta gate"
    );

    assert!(
        md.contains("## Phase-5 Go/No-Go Snapshot"),
        "final report markdown must include go/no-go snapshot section"
    );
    for gate in PHASE5_GO_NO_GO_GATES {
        assert!(
            md.contains(gate),
            "final report markdown missing phase-5 go/no-go gate marker: {gate}"
        );
    }
    assert!(
        md.contains("**Snapshot Decision**:"),
        "final report markdown must include explicit snapshot decision marker"
    );
    assert!(
        md.contains("missing gate or non-PASS status => NO-GO"),
        "final report markdown must include fail-closed go/no-go rule marker"
    );

    // Each evidence has an artifact path
    for ev in &cert.evidence {
        assert!(
            ev.artifact_path.is_some(),
            "Gate {} missing artifact path",
            ev.gate
        );
    }

    // Verdict consistency
    let has_fail = cert.evidence.iter().any(|e| e.status == Signal::Fail);
    let has_warn = cert.evidence.iter().any(|e| e.status == Signal::Warn);
    if has_fail {
        assert_eq!(cert.certification_verdict, Signal::Fail);
    } else if has_warn {
        assert_eq!(cert.certification_verdict, Signal::Warn);
    }

    // Risk register entries match non-pass evidence
    let non_pass_count = cert
        .evidence
        .iter()
        .filter(|e| e.status != Signal::Pass)
        .count();
    assert_eq!(
        cert.risk_register.len(),
        non_pass_count,
        "Risk register should have one entry per non-pass evidence gate"
    );

    // Repro commands present
    assert!(!cert.reproduce_commands.is_empty());

    let requested = final_certification_generation_requested(
        std::env::var(GENERATE_FINAL_CERTIFICATION_ENV)
            .ok()
            .as_deref(),
    );
    let wrote = write_final_certification_artifacts(&repo_root(), &cert, &md, requested)
        .unwrap_or_else(|err| panic!("final certification generation refused: {err}"));
    if wrote {
        eprintln!("Final certification artifacts generated under tests/certification");
    } else {
        eprintln!(
            "Read-only by default. Generate passing final certification evidence explicitly with: {GENERATE_FINAL_CERTIFICATION_ENV}=1 cargo test --locked --test release_readiness final_qa_certification -- --exact --nocapture"
        );
    }
}

#[test]
fn certification_report_schema_valid() {
    let cert = generate_certification();
    let json = serde_json::to_string_pretty(&cert).expect("serialize");
    let parsed: V = serde_json::from_str(&json).expect("parse");

    assert_eq!(parsed.get("schema").and_then(V::as_str), Some(CERT_SCHEMA));
    assert!(parsed.get("certification_verdict").is_some());
    assert!(parsed.get("evidence").and_then(V::as_array).is_some());
    assert!(parsed.get("risk_register").and_then(V::as_array).is_some());
    assert!(
        parsed
            .get("reproduce_commands")
            .and_then(V::as_array)
            .is_some()
    );
    assert!(
        parsed
            .get("ci_run_link_template")
            .and_then(V::as_str)
            .is_some()
    );
}

#[test]
fn phase5_go_no_go_snapshot_fails_closed_when_gate_missing() {
    let mut cert = generate_certification();
    cert.evidence
        .retain(|entry| entry.gate != "parameter_sweeps_integrity");

    let md = render_certification_markdown(&cert);
    assert!(
        md.contains(
            "| parameter_sweeps_integrity | NO_DATA | MISSING from certification evidence (fail-closed) |"
        ),
        "missing go/no-go gate must render NO_DATA marker in snapshot table"
    );
    assert!(
        md.contains("**Snapshot Decision**: NO-GO"),
        "snapshot decision must fail closed to NO-GO when required gate evidence is missing"
    );
}

#[test]
fn parse_must_pass_gate_verdict_reads_current_schema() {
    let gate = serde_json::json!({
        "status": "pass",
        "observed": {
            "must_pass_total": 208,
            "must_pass_tested": 208,
            "must_pass_passed": 208,
            "must_pass_failed": 0,
            "must_pass_skipped": 0
        }
    });

    let (status, passed, total) = parse_must_pass_gate_verdict(&gate);
    assert_eq!(status, "pass");
    assert_eq!(passed, 208);
    assert_eq!(total, 208);
}

#[test]
fn parse_must_pass_gate_verdict_does_not_replace_current_zero_counts_with_legacy_values() {
    let gate = serde_json::json!({
        "status": "pass",
        "total": 208,
        "passed": 208,
        "observed": {
            "must_pass_total": 0,
            "must_pass_passed": 0
        }
    });

    let (status, passed, total) = parse_must_pass_gate_verdict(&gate);
    assert_eq!(status, "pass");
    assert_eq!(passed, 0);
    assert_eq!(total, 0);
}

#[test]
fn validate_must_pass_gate_metadata_accepts_current_schema() {
    let gate = serde_json::json!({
        "schema": "pi.ext.must_pass_gate.v1",
        "generated_at": "2026-02-17T03:06:08.928Z",
        "run_id": "local-20260217T030608928Z",
        "correlation_id": "must-pass-gate-local-20260217T030608928Z",
        "git_commit": "0123456789abcdef0123456789abcdef01234567",
        "source_tree_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "inclusion_sha256": "13579bdf02468ace13579bdf02468ace13579bdf02468ace13579bdf02468ace",
        "manifest_sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        "observed": {
            "must_pass_total": 208,
            "must_pass_tested": 208,
            "must_pass_passed": 208,
            "must_pass_failed": 0,
            "must_pass_skipped": 0
        }
    });

    let errors = validate_must_pass_gate_metadata(&gate);
    assert!(
        errors.is_empty(),
        "current-schema must-pass gate should be metadata-valid, got: {errors:?}"
    );
}

#[test]
fn validate_must_pass_gate_metadata_rejects_legacy_payload() {
    let gate = serde_json::json!({
        "verdict": "warn",
        "total": 208,
        "passed": 203
    });

    let errors = validate_must_pass_gate_metadata(&gate);
    assert!(
        !errors.is_empty(),
        "legacy payload without metadata should fail validation"
    );
    assert!(
        errors.iter().any(|msg| msg.contains("schema")),
        "expected schema validation error, got: {errors:?}"
    );
    assert!(
        errors.iter().any(|msg| msg.contains("/run_id")),
        "expected run_id validation error, got: {errors:?}"
    );
}

#[test]
fn validate_must_pass_gate_metadata_rejects_incoherent_counts() {
    let gate = serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "generated_at": "2026-02-17T03:06:08.928Z",
        "run_id": "local-20260217T030608928Z",
        "correlation_id": "must-pass-gate-local-20260217T030608928Z",
        "git_commit": "0123456789abcdef0123456789abcdef01234567",
        "source_tree_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "inclusion_sha256": "13579bdf02468ace13579bdf02468ace13579bdf02468ace13579bdf02468ace",
        "manifest_sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        "observed": {
            "must_pass_total": 208,
            "must_pass_tested": 207,
            "must_pass_passed": 208,
            "must_pass_failed": 0,
            "must_pass_skipped": 0
        }
    });

    let errors = validate_must_pass_gate_metadata(&gate);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("tested-count mismatch")),
        "incoherent observed counts must fail closed: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("total-count mismatch")),
        "incoherent observed totals must fail closed: {errors:?}"
    );
}

#[test]
fn validate_must_pass_gate_metadata_rejects_blank_lineage_and_invalid_timestamp() {
    let gate = serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "generated_at": "not-a-timestamp",
        "run_id": "   ",
        "correlation_id": "\t",
        "git_commit": "not-a-commit",
        "source_tree_sha256": "short",
        "inclusion_sha256": "also-short",
        "manifest_sha256": "also-short",
        "observed": {
            "must_pass_total": 1,
            "must_pass_tested": 1,
            "must_pass_passed": 1,
            "must_pass_failed": 0,
            "must_pass_skipped": 0
        }
    });

    let errors = validate_must_pass_gate_metadata(&gate);
    for marker in [
        "/run_id",
        "/correlation_id",
        "RFC3339",
        "/git_commit",
        "/source_tree_sha256",
        "/inclusion_sha256",
        "/manifest_sha256",
    ] {
        assert!(
            errors.iter().any(|error| error.contains(marker)),
            "expected {marker} validation error, got: {errors:?}"
        );
    }
}

#[test]
fn gate_hardening_rejects_abbreviated_evidence_commit_ids() {
    let gate = serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "generated_at": "2026-08-04T12:00:00Z",
        "run_id": "release-run",
        "correlation_id": "release-correlation",
        "git_commit": "abcdef0",
        "source_tree_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "inclusion_sha256": "13579bdf02468ace13579bdf02468ace13579bdf02468ace13579bdf02468ace",
        "manifest_sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        "observed": {
            "must_pass_total": 208,
            "must_pass_tested": 208,
            "must_pass_passed": 208,
            "must_pass_failed": 0,
            "must_pass_skipped": 0
        }
    });

    let errors = validate_must_pass_gate_metadata(&gate);
    assert!(
        errors.iter().any(|error| error.contains("full 40- or 64")),
        "abbreviated commit IDs must fail closed: {errors:?}"
    );
}

const TEST_MUST_PASS_GIT_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const TEST_MUST_PASS_SOURCE_SHA256: &str =
    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn write_must_pass_catalog_fixtures(root: &Path, canonical_ids: &[&str]) -> (String, String) {
    let inclusion_path = root.join(MUST_PASS_INCLUSION_PATH);
    let manifest_path = root.join(MUST_PASS_MANIFEST_PATH);
    let events_path = root.join(MUST_PASS_EVENTS_PATH);
    for path in [&inclusion_path, &manifest_path, &events_path] {
        std::fs::create_dir_all(path.parent().expect("must-pass fixture has parent"))
            .expect("create must-pass fixture directory");
    }

    let mut extensions = canonical_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            serde_json::json!({
                "id": id,
                "entry_path": format!("{id}.js"),
                "conformance_tier": if index % 2 == 0 { 1 } else { 3 },
            })
        })
        .collect::<Vec<_>>();
    extensions.push(serde_json::json!({
        "id": "stretch-only",
        "entry_path": "stretch-only.js",
        "conformance_tier": 3,
    }));
    let manifest = serde_json::json!({
        "schema": "pi.ext.validated-manifest.v1",
        "extensions": extensions,
    });
    let canonical_count =
        u64::try_from(canonical_ids.len()).expect("canonical fixture count fits u64");
    let inclusion = serde_json::json!({
        "schema": "pi.ext.inclusion_list.v1",
        "tier1": canonical_ids
            .iter()
            .map(|id| serde_json::json!({"id": id}))
            .collect::<Vec<_>>(),
        "tier1_review": [],
        "summary": {
            "tier1_count": canonical_count,
            "tier1_review_count": 0,
            "total_must_pass": canonical_count,
        },
    });
    std::fs::write(
        &inclusion_path,
        serde_json::to_vec_pretty(&inclusion).expect("serialize must-pass inclusion fixture"),
    )
    .expect("write must-pass inclusion fixture");
    let inclusion_sha256 = sha256_file(&inclusion_path).expect("hash must-pass inclusion fixture");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize must-pass manifest fixture"),
    )
    .expect("write must-pass manifest fixture");
    let manifest_sha256 = sha256_file(&manifest_path).expect("hash must-pass manifest fixture");

    (inclusion_sha256, manifest_sha256)
}

fn write_must_pass_events_fixture(
    root: &Path,
    canonical_ids: &[&str],
    observed_ids: &[&str],
    inclusion_sha256: &str,
    manifest_sha256: &str,
) {
    let events_path = root.join(MUST_PASS_EVENTS_PATH);

    let run_id = "local-20260804T120000000Z";
    let correlation_id = "must-pass-gate-local-20260804T120000000Z";
    let mut events = String::new();
    for id in observed_ids {
        let tier = canonical_ids
            .iter()
            .position(|candidate| candidate == id)
            .map_or(1, |index| if index % 2 == 0 { 1 } else { 3 });
        let event = serde_json::json!({
            "schema": "pi.ext.gate_event.v1",
            "run_id": run_id,
            "correlation_id": correlation_id,
            "git_commit": TEST_MUST_PASS_GIT_COMMIT,
            "source_tree_sha256": TEST_MUST_PASS_SOURCE_SHA256,
            "inclusion_sha256": inclusion_sha256,
            "manifest_sha256": manifest_sha256,
            "set": "must_pass",
            "status": "pass",
            "id": id,
            "tier": tier,
            "failure_reason": null,
            "duration_ms": 0,
            "ts": "2026-08-04T12:00:00Z",
        });
        events.push_str(&serde_json::to_string(&event).expect("serialize must-pass event fixture"));
        events.push('\n');
    }
    std::fs::write(&events_path, events).expect("write must-pass event fixture");
}

fn must_pass_fixture_verdict(
    observed_count: u64,
    inclusion_sha256: &str,
    manifest_sha256: &str,
) -> V {
    serde_json::json!({
        "schema": MUST_PASS_GATE_SCHEMA,
        "generated_at": "2026-08-04T12:00:00Z",
        "run_id": "local-20260804T120000000Z",
        "correlation_id": "must-pass-gate-local-20260804T120000000Z",
        "git_commit": TEST_MUST_PASS_GIT_COMMIT,
        "source_tree_sha256": TEST_MUST_PASS_SOURCE_SHA256,
        "inclusion_sha256": inclusion_sha256,
        "manifest_sha256": manifest_sha256,
        "mode": "strict",
        "status": "pass",
        "thresholds": {
            "min_pass_rate_pct": 100.0,
            "max_failures": 0,
        },
        "observed": {
            "must_pass_total": observed_count,
            "must_pass_tested": observed_count,
            "must_pass_passed": observed_count,
            "must_pass_failed": 0,
            "must_pass_skipped": 0,
            "must_pass_pass_rate_pct": 100.0,
            "stretch_total": 1,
            "stretch_tested": 0,
            "stretch_passed": 0,
            "stretch_failed": 0,
            "stretch_skipped": 1,
        },
        "checks": [
            {
                "id": "must_pass_rate",
                "actual": 100.0,
                "threshold": 100.0,
                "ok": true,
            },
            {
                "id": "must_pass_failure_count",
                "actual": 0,
                "threshold": 0,
                "ok": true,
            },
            {
                "id": "must_pass_complete_coverage",
                "actual": observed_count,
                "threshold": observed_count,
                "ok": true,
            },
        ],
        "blocking_failures": [],
        "stretch_set_summary": {
            "total": 1,
            "tested": 0,
            "passed": 0,
            "failed": 0,
            "skipped": 1,
        },
    })
}

fn write_must_pass_certification_fixture(
    root: &Path,
    canonical_ids: &[&str],
    observed_ids: &[&str],
) -> V {
    let (inclusion_sha256, manifest_sha256) = write_must_pass_catalog_fixtures(root, canonical_ids);
    write_must_pass_events_fixture(
        root,
        canonical_ids,
        observed_ids,
        &inclusion_sha256,
        &manifest_sha256,
    );
    let observed_count = u64::try_from(observed_ids.len()).expect("fixture count fits u64");
    must_pass_fixture_verdict(observed_count, &inclusion_sha256, &manifest_sha256)
}

#[test]
fn certified_must_pass_accepts_exact_current_canonical_set() {
    let root = tempdir().expect("tempdir");
    let verdict =
        write_must_pass_certification_fixture(root.path(), &["alpha", "beta"], &["alpha", "beta"]);

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        TEST_MUST_PASS_SOURCE_SHA256,
        get_str(&verdict, "/inclusion_sha256"),
        get_str(&verdict, "/manifest_sha256"),
        2,
    );
    assert_eq!(signal, Signal::Pass, "{detail}");
    assert!(
        detail.contains("exact canonical inclusion-list set"),
        "{detail}"
    );
}

#[test]
fn certified_must_pass_rejects_incoherent_strict_gate_contract() {
    let root = tempdir().expect("tempdir");
    let mut verdict = write_must_pass_certification_fixture(root.path(), &["alpha"], &["alpha"]);
    verdict["checks"][0]["ok"] = V::Bool(false);

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        TEST_MUST_PASS_SOURCE_SHA256,
        get_str(&verdict, "/inclusion_sha256"),
        get_str(&verdict, "/manifest_sha256"),
        1,
    );
    assert_eq!(signal, Signal::Fail);
    assert!(
        detail.contains("must-pass check must_pass_rate"),
        "{detail}"
    );
}

#[test]
fn certified_must_pass_rejects_incomplete_event_schema() {
    let root = tempdir().expect("tempdir");
    let verdict = write_must_pass_certification_fixture(root.path(), &["alpha"], &["alpha"]);
    let events_path = root.path().join(MUST_PASS_EVENTS_PATH);
    let mut event: V = serde_json::from_str(
        std::fs::read_to_string(&events_path)
            .expect("read must-pass event fixture")
            .trim(),
    )
    .expect("parse must-pass event fixture");
    event
        .as_object_mut()
        .expect("event fixture is an object")
        .remove("ts");
    std::fs::write(
        &events_path,
        format!(
            "{}\n",
            serde_json::to_string(&event).expect("serialize must-pass event fixture")
        ),
    )
    .expect("write must-pass event fixture");

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        TEST_MUST_PASS_SOURCE_SHA256,
        get_str(&verdict, "/inclusion_sha256"),
        get_str(&verdict, "/manifest_sha256"),
        1,
    );
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("RFC3339 timestamp"), "{detail}");
}

#[test]
fn certified_must_pass_rejects_stale_evidence_missing_new_canonical_id() {
    let root = tempdir().expect("tempdir");
    let verdict =
        write_must_pass_certification_fixture(root.path(), &["alpha", "beta"], &["alpha"]);

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        TEST_MUST_PASS_SOURCE_SHA256,
        get_str(&verdict, "/inclusion_sha256"),
        get_str(&verdict, "/manifest_sha256"),
        2,
    );
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("missing=[beta]"), "{detail}");
    assert!(
        detail.contains("verdict=1/1, expected=2"),
        "stale counts must not certify a smaller historical set: {detail}"
    );
}

#[test]
fn gate_hardening_rejects_a_self_consistent_smaller_denominator() {
    let root = tempdir().expect("tempdir");
    let _verdict =
        write_must_pass_certification_fixture(root.path(), &["alpha", "beta"], &["alpha", "beta"]);

    let error = canonical_must_pass_entries(root.path(), 3)
        .expect_err("an exact versioned denominator must not silently shrink");
    assert!(
        error.contains("unexpected versioned must-pass denominator"),
        "{error}"
    );
}

#[test]
fn gate_hardening_requires_explicit_inclusion_summary_counts() {
    let root = tempdir().expect("tempdir");
    let _verdict = write_must_pass_certification_fixture(root.path(), &["alpha"], &["alpha"]);
    let inclusion_path = root.path().join(MUST_PASS_INCLUSION_PATH);
    let mut inclusion: V = serde_json::from_slice(
        &std::fs::read(&inclusion_path).expect("read must-pass inclusion fixture"),
    )
    .expect("parse must-pass inclusion fixture");
    inclusion["summary"]
        .as_object_mut()
        .expect("summary fixture is an object")
        .remove("tier1_review_count");
    std::fs::write(
        &inclusion_path,
        serde_json::to_vec_pretty(&inclusion).expect("serialize must-pass inclusion fixture"),
    )
    .expect("write must-pass inclusion fixture");

    let error = canonical_must_pass_entries(root.path(), 1)
        .expect_err("a missing zero-valued summary field must fail closed");
    assert!(error.contains("tier1_review_count"), "{error}");
}

#[test]
fn gate_hardening_rejects_duplicate_manifest_artifact_paths() {
    let root = tempdir().expect("tempdir");
    let _verdict =
        write_must_pass_certification_fixture(root.path(), &["alpha", "beta"], &["alpha"]);
    let manifest_path = root.path().join(MUST_PASS_MANIFEST_PATH);
    let mut manifest: V = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("read must-pass manifest fixture"),
    )
    .expect("parse must-pass manifest fixture");
    manifest["extensions"][1]["entry_path"] = V::String("alpha.js".to_string());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize must-pass manifest fixture"),
    )
    .expect("write must-pass manifest fixture");

    let error = canonical_must_pass_entries(root.path(), 2)
        .expect_err("duplicate artifact paths must not inflate the canonical denominator");
    assert!(error.contains("duplicate artifact entry_path"), "{error}");
}

#[test]
fn gate_hardening_rejects_lexical_artifact_path_aliases() {
    assert!(is_canonical_artifact_entry_path("nested/entry.js"));
    for invalid_path in [
        "../entry.js",
        "nested//entry.js",
        "nested/./entry.js",
        "nested/entry.js/",
        "C:/entry.js",
        "nested\\entry.js",
    ] {
        assert!(
            !is_canonical_artifact_entry_path(invalid_path),
            "non-canonical artifact path must fail closed: {invalid_path:?}"
        );
    }
}

#[test]
fn certified_must_pass_rejects_stale_source_tree_digest() {
    let root = tempdir().expect("tempdir");
    let verdict = write_must_pass_certification_fixture(root.path(), &["alpha"], &["alpha"]);

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        get_str(&verdict, "/inclusion_sha256"),
        get_str(&verdict, "/manifest_sha256"),
        1,
    );
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("source-tree digest"), "{detail}");
}

#[test]
fn certified_must_pass_rejects_stale_inclusion_list_digest() {
    let root = tempdir().expect("tempdir");
    let verdict = write_must_pass_certification_fixture(root.path(), &["alpha"], &["alpha"]);

    let (signal, detail) = validate_certified_must_pass_against_source(
        root.path(),
        &verdict,
        TEST_MUST_PASS_SOURCE_SHA256,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        get_str(&verdict, "/manifest_sha256"),
        1,
    );
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("inclusion-list digest"), "{detail}");
}

#[test]
fn canonical_must_pass_rejects_tier_zero() {
    let root = tempdir().expect("tempdir");
    let inclusion_path = root.path().join(MUST_PASS_INCLUSION_PATH);
    let manifest_path = root.path().join(MUST_PASS_MANIFEST_PATH);
    std::fs::create_dir_all(inclusion_path.parent().expect("inclusion fixture parent"))
        .expect("create inclusion fixture directory");
    std::fs::create_dir_all(manifest_path.parent().expect("manifest fixture parent"))
        .expect("create manifest fixture directory");
    std::fs::write(
        &inclusion_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "pi.ext.inclusion_list.v1",
            "tier1": [{"id": "invalid-zero-tier"}],
            "tier1_review": [],
            "summary": {
                "tier1_count": 1,
                "tier1_review_count": 0,
                "total_must_pass": 1,
            }
        }))
        .expect("serialize inclusion fixture"),
    )
    .expect("write inclusion fixture");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "pi.ext.validated-manifest.v1",
            "extensions": [{
                "id": "invalid-zero-tier",
                "entry_path": "invalid-zero-tier.js",
                "conformance_tier": 0
            }]
        }))
        .expect("serialize manifest fixture"),
    )
    .expect("write manifest fixture");

    let error =
        canonical_must_pass_entries(root.path(), 1).expect_err("tier zero must fail closed");
    assert!(error.contains("expected 1..=5"), "{error}");
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
    attributes: Option<&str>,
    track_events: bool,
) -> tempfile::TempDir {
    let root = tempdir().expect("create committed-evidence fixture");
    run_evidence_binding_fixture_git(root.path(), &["init", "-q"]);
    let verdict_path = root.path().join(MUST_PASS_VERDICT_PATH);
    let events_path = root.path().join(MUST_PASS_EVENTS_PATH);
    std::fs::create_dir_all(verdict_path.parent().expect("verdict fixture parent"))
        .expect("create committed-evidence fixture directory");
    std::fs::write(&verdict_path, "{}\n").expect("write verdict fixture");
    std::fs::write(&events_path, "{}\n").expect("write events fixture");
    if let Some(contents) = attributes {
        std::fs::write(root.path().join(".gitattributes"), contents)
            .expect("write evidence attributes fixture");
        run_evidence_binding_fixture_git(root.path(), &["add", ".gitattributes"]);
    }
    run_evidence_binding_fixture_git(root.path(), &["add", MUST_PASS_VERDICT_PATH]);
    if track_events {
        run_evidence_binding_fixture_git(root.path(), &["add", MUST_PASS_EVENTS_PATH]);
    }
    run_evidence_binding_fixture_git(
        root.path(),
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
fn committed_must_pass_evidence_requires_both_head_blobs() {
    let root = committed_evidence_binding_fixture(None, false);
    let error = capture_committed_must_pass_evidence(root.path())
        .expect_err("an untracked event log must fail closed");
    assert!(
        error.contains("untracked files") || error.contains("not tracked"),
        "{error}"
    );
}

#[test]
fn committed_must_pass_evidence_rejects_staged_and_worktree_drift() {
    let staged = committed_evidence_binding_fixture(None, true);
    capture_committed_must_pass_evidence(staged.path())
        .expect("clean committed evidence must be accepted");
    std::fs::write(
        staged.path().join(MUST_PASS_VERDICT_PATH),
        "{\"staged\":true}\n",
    )
    .expect("write staged verdict drift");
    run_evidence_binding_fixture_git(staged.path(), &["add", MUST_PASS_VERDICT_PATH]);
    let error = capture_committed_must_pass_evidence(staged.path())
        .expect_err("staged evidence drift must fail closed");
    assert!(error.contains("differ in the index"), "{error}");

    let worktree = committed_evidence_binding_fixture(None, true);
    std::fs::write(
        worktree.path().join(MUST_PASS_EVENTS_PATH),
        "{\"worktree\":true}\n",
    )
    .expect("write worktree event drift");
    let error = capture_committed_must_pass_evidence(worktree.path())
        .expect_err("unstaged evidence drift must fail closed");
    assert!(error.contains("differ in the worktree"), "{error}");
}

#[test]
fn committed_must_pass_evidence_rejects_noncanonical_index_flags() {
    let root = committed_evidence_binding_fixture(None, true);
    run_evidence_binding_fixture_git(
        root.path(),
        &["update-index", "--assume-unchanged", MUST_PASS_VERDICT_PATH],
    );
    let error = capture_committed_must_pass_evidence(root.path())
        .expect_err("assume-unchanged evidence must fail closed");
    assert!(error.contains("index flags"), "{error}");
}

#[test]
fn committed_must_pass_evidence_rejects_filter_hidden_byte_drift() {
    let root =
        committed_evidence_binding_fixture(Some("*.json text eol=lf\n*.jsonl text eol=lf\n"), true);
    std::fs::write(root.path().join(MUST_PASS_EVENTS_PATH), "{}\r\n")
        .expect("write filter-hidden event drift");
    let diff_status = std::process::Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["diff", "--quiet", "--", MUST_PASS_EVENTS_PATH])
        .status()
        .expect("check filter-hidden evidence fixture");
    assert!(
        diff_status.success(),
        "fixture must demonstrate evidence drift hidden by Git clean filtering"
    );
    let error = capture_committed_must_pass_evidence(root.path())
        .expect_err("raw evidence byte drift must fail closed");
    assert!(error.contains("worktree bytes differ"), "{error}");
}

#[test]
fn gate_hardening_source_digest_rejects_hidden_and_tracked_dirt() {
    let root = tempdir().expect("tempdir");
    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(args)
            .output()
            .expect("run git for source-digest fixture");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run_git(&["init", "-q"]);
    std::fs::create_dir_all(root.path().join("artifacts"))
        .expect("create source-digest fixture directory");
    std::fs::write(root.path().join(".gitignore"), "artifacts/*.log\n")
        .expect("write source-digest ignore fixture");
    std::fs::write(
        root.path().join(".gitattributes"),
        "tracked.txt text eol=lf\n",
    )
    .expect("write source-digest attributes fixture");
    std::fs::write(root.path().join("tracked.txt"), "tracked\n")
        .expect("write tracked source-digest fixture");
    std::fs::write(root.path().join("artifacts/tracked.js"), "export {};\n")
        .expect("write tracked artifact fixture");
    run_git(&[
        "add",
        ".gitattributes",
        ".gitignore",
        "tracked.txt",
        "artifacts/tracked.js",
    ]);
    run_git(&[
        "-c",
        "user.name=Pi Test",
        "-c",
        "user.email=pi-test@example.invalid",
        "commit",
        "-q",
        "-m",
        "fixture",
    ]);

    let source_paths = [".gitattributes", ".gitignore", "tracked.txt", "artifacts"];
    let before = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect("hash clean tracked source fixture");
    std::fs::write(
        root.path().join("artifacts/debug.log"),
        "ignored local debug output\n",
    )
    .expect("write ignored source-digest fixture");
    let error = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect_err("ignored untracked source input must fail closed");
    assert!(error.contains("untracked files"), "{error}");
    run_git(&["add", "-f", "artifacts/debug.log"]);
    run_git(&[
        "-c",
        "user.name=Pi Test",
        "-c",
        "user.email=pi-test@example.invalid",
        "commit",
        "-q",
        "-m",
        "track ignored fixture",
    ]);
    let after_tracking = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect("hash source fixture after tracking former ignored file");
    assert_ne!(
        before, after_tracking,
        "new tracked release input must change the provenance digest"
    );

    run_git(&["update-index", "--assume-unchanged", "tracked.txt"]);
    std::fs::write(root.path().join("tracked.txt"), "changed\n")
        .expect("modify tracked source-digest fixture");
    let error = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect_err("assume-unchanged must not hide tracked source dirt");
    assert!(error.contains("index flags"), "{error}");

    run_git(&["update-index", "--no-assume-unchanged", "tracked.txt"]);
    std::fs::write(root.path().join("tracked.txt"), "tracked\n")
        .expect("restore tracked source-digest fixture contents");
    must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect("restored source fixture must be clean");

    run_git(&["update-index", "--skip-worktree", "tracked.txt"]);
    let error = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect_err("skip-worktree must fail closed even before source dirt is introduced");
    assert!(error.contains("index flags"), "{error}");
    run_git(&["update-index", "--no-skip-worktree", "tracked.txt"]);

    std::fs::write(root.path().join("tracked.txt"), "tracked\r\n")
        .expect("write filter-normalized source-digest fixture");
    let diff_status = std::process::Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["diff", "--quiet", "--", "tracked.txt"])
        .status()
        .expect("run git diff for filter-normalized fixture");
    assert!(
        diff_status.success(),
        "fixture must demonstrate a byte change hidden by Git clean filtering"
    );
    let error = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect_err("worktree bytes hidden by Git clean filtering must fail closed");
    assert!(error.contains("worktree bytes differ"), "{error}");

    std::fs::write(root.path().join("tracked.txt"), "tracked\n")
        .expect("restore source fixture after filter-normalization test");
    std::fs::write(root.path().join("tracked.txt"), "changed\n")
        .expect("modify tracked source-digest fixture without hidden index flags");
    let error = must_pass_source_tree_sha256_for_paths(root.path(), &source_paths)
        .expect_err("ordinary tracked source dirt must fail closed");
    assert!(error.contains("differ in the worktree"), "{error}");
}

#[test]
fn gate_hardening_source_commit_requires_real_ancestry_and_evidence_only_followups() {
    let root = tempdir().expect("tempdir");
    let run_git = |args: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(args)
            .output()
            .expect("run git for evidence-source fixture");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git fixture output is UTF-8")
            .trim()
            .to_string()
    };
    let commit = || {
        run_git(&[
            "-c",
            "user.name=Pi Test",
            "-c",
            "user.email=pi-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ]);
    };

    run_git(&["init", "-q"]);
    std::fs::write(root.path().join("source.txt"), "tested source\n")
        .expect("write tested source fixture");
    run_git(&["add", "source.txt"]);
    commit();
    let source_commit = run_git(&["rev-parse", "HEAD"]);

    std::fs::create_dir_all(root.path().join("tests/evidence_bundle"))
        .expect("create evidence fixture directory");
    std::fs::write(root.path().join("tests/evidence_bundle/index.json"), "{}\n")
        .expect("write evidence fixture");
    run_git(&["add", "tests/evidence_bundle/index.json"]);
    commit();
    let evidence_commit = run_git(&["rev-parse", "HEAD"]);
    validate_evidence_source_commit(root.path(), &source_commit, &evidence_commit)
        .expect("an evidence-only descendant commit must preserve source provenance");

    std::fs::write(
        root.path().join("source.txt"),
        "temporary non-evidence change\n",
    )
    .expect("write non-evidence fixture");
    run_git(&["add", "source.txt"]);
    commit();
    std::fs::write(root.path().join("source.txt"), "tested source\n")
        .expect("restore non-evidence fixture contents");
    run_git(&["add", "source.txt"]);
    commit();
    let reverted_non_evidence_commit = run_git(&["rev-parse", "HEAD"]);
    let error =
        validate_evidence_source_commit(root.path(), &source_commit, &reverted_non_evidence_commit)
            .expect_err("even a reverted non-evidence descendant change must fail closed");
    assert!(error.contains("non-evidence paths"), "{error}");

    let nonexistent = "0000000000000000000000000000000000000000";
    let error =
        validate_evidence_source_commit(root.path(), nonexistent, &reverted_non_evidence_commit)
            .expect_err("a syntactically valid but nonexistent commit must fail closed");
    assert!(error.contains("does not resolve"), "{error}");
}

#[test]
fn artifact_sha256_is_a_real_full_content_digest() {
    let dir = tempdir().expect("tempdir");
    let artifact = dir.path().join("artifact.txt");
    std::fs::write(&artifact, b"abc").expect("write digest fixture");

    assert_eq!(
        sha256_file(&artifact).as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn non_mock_rubric_accepts_current_schema() {
    let rubric: V = serde_json::from_str(include_str!("../docs/non-mock-rubric.json"))
        .expect("parse checked-in non-mock rubric");

    let (signal, detail) = validate_non_mock_rubric(&rubric);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn non_mock_rubric_rejects_legacy_schema() {
    let rubric = serde_json::json!({"schema": "pi.test.non_mock_rubric.v1"});

    let (signal, detail) = validate_non_mock_rubric(&rubric);
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains(NON_MOCK_RUBRIC_SCHEMA), "{detail}");
}

#[test]
fn current_conformance_summary_fails_closed_on_partial_coverage() {
    let summary: V = serde_json::from_str(include_str!(
        "ext_conformance/reports/conformance_summary.json"
    ))
    .expect("parse checked-in conformance summary");

    let (signal, detail) = validate_current_conformance_summary(&summary);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains("60/226 tested"), "{detail}");
    assert!(detail.contains("166 not exercised"), "{detail}");
}

#[test]
fn current_conformance_summary_accepts_complete_reconciled_counts() {
    let summary = complete_conformance_summary_fixture();

    let (signal, detail) = validate_current_conformance_summary(&summary);
    assert_eq!(signal, Signal::Pass, "{detail}");
    assert!(detail.contains("10/10 pass"), "{detail}");
}

#[test]
fn current_conformance_summary_rejects_incoherent_counts() {
    let summary = serde_json::json!({
        "schema": "pi.ext.conformance_summary.v2",
        "counts": {"total": 2, "tested": 2, "pass": 1, "fail": 0, "na": 0}
    });

    let (signal, detail) = validate_current_conformance_summary(&summary);
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("tested-count mismatch"), "{detail}");
}

#[test]
fn certification_signal_does_not_turn_partial_no_data_into_pass() {
    assert_eq!(aggregate_certification_signals(&[]), Signal::NoData);
    assert_eq!(
        aggregate_certification_signals(&[Signal::Pass, Signal::NoData, Signal::Pass]),
        Signal::NoData
    );
    assert_eq!(
        aggregate_certification_signals(&[Signal::Pass, Signal::NoData, Signal::Warn]),
        Signal::Warn
    );
    assert_eq!(
        aggregate_certification_signals(&[Signal::Pass, Signal::NoData, Signal::Fail]),
        Signal::Fail
    );
}

#[test]
fn full_suite_gate_reads_current_total_gates_field() {
    let gate: V = serde_json::from_str(include_str!("full_suite_gate/full_suite_verdict.json"))
        .expect("parse checked-in full-suite verdict");

    let (signal, detail) = validate_full_suite_gate(&gate);
    assert_eq!(signal, Signal::Fail, "{detail}");
    assert!(detail.contains("17/20 gates pass"), "{detail}");
    assert!(!detail.contains("17/0"), "{detail}");
}

#[test]
fn full_suite_gate_fails_closed_without_total_gates() {
    let gate = serde_json::json!({
        "schema": FULL_SUITE_GATE_SCHEMA,
        "verdict": "fail",
        "gates": [{"status": "pass", "blocking": true}],
        "summary": {
            "passed": 1,
            "failed": 0,
            "warned": 0,
            "skipped": 0,
            "blocking_pass": 1,
            "blocking_total": 1,
            "all_blocking_pass": true
        }
    });

    let (signal, detail) = validate_full_suite_gate(&gate);
    assert_eq!(signal, Signal::Fail);
    assert!(detail.contains("/summary/total_gates"), "{detail}");
    assert!(!detail.contains("1/0 gates pass"), "{detail}");
}

#[test]
fn practical_finish_checkpoint_accepts_docs_only_residual_contract() {
    let artifact = serde_json::json!({
        "schema": "pi.perf3x.practical_finish_checkpoint.v1",
        "generated_at": "2026-02-17T04:00:00.000Z",
        "status": "pass",
        "detail": "Practical-finish checkpoint reached: technical PERF-3X scope complete; 1 docs/report issue(s) remain.",
        "open_perf3x_count": 1,
        "technical_open_count": 0,
        "docs_or_report_open_count": 1,
        "technical_completion_reached": true,
        "residual_open_scope": "docs_or_report_only",
        "technical_open_issues": [],
        "docs_or_report_open_issues": [
            {
                "id": "bd-3ar8v.6.5",
                "title": "Final report polish",
                "status": "open",
                "issue_type": "docs",
                "labels": ["docs", "report"]
            }
        ]
    });

    let (signal, detail) = validate_practical_finish_checkpoint(&artifact);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn practical_finish_checkpoint_rejects_residual_count_mismatch() {
    let artifact = serde_json::json!({
        "schema": "pi.perf3x.practical_finish_checkpoint.v1",
        "generated_at": "2026-02-17T04:00:00.000Z",
        "status": "pass",
        "detail": "Practical-finish checkpoint reached: technical PERF-3X scope complete; 1 docs/report issue(s) remain.",
        "open_perf3x_count": 2,
        "technical_open_count": 0,
        "docs_or_report_open_count": 1,
        "technical_completion_reached": true,
        "residual_open_scope": "docs_or_report_only",
        "technical_open_issues": [],
        "docs_or_report_open_issues": [
            {
                "id": "bd-3ar8v.6.5",
                "title": "Final report polish",
                "status": "open",
                "issue_type": "docs",
                "labels": ["docs", "report"]
            }
        ]
    });

    let (signal, detail) = validate_practical_finish_checkpoint(&artifact);
    assert_eq!(signal, Signal::Fail);
    assert!(
        detail.contains("open_perf3x_count"),
        "expected mismatch detail, got: {detail}"
    );
}

#[test]
fn parameter_sweeps_contract_accepts_consistent_shape() {
    let artifact = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json"
        },
        "readiness": {
            "status": "ready",
            "ready_for_phase5": true,
            "blocking_reasons": []
        },
        "selected_defaults": {
            "flush_cadence_ms": 125,
            "queue_max_items": 64,
            "compaction_quota_mb": 8
        },
        "sweep_plan": {
            "dimensions": [
                {
                    "name": "flush_cadence_ms",
                    "candidate_values": [50, 125, 250]
                },
                {
                    "name": "queue_max_items",
                    "candidate_values": [32, 64, 128]
                },
                {
                    "name": "compaction_quota_mb",
                    "candidate_values": [4, 8, 12]
                }
            ]
        }
    });

    let (signal, detail) = validate_parameter_sweeps_artifact(&artifact);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn parameter_sweeps_contract_rejects_readiness_incoherence() {
    let artifact = serde_json::json!({
        "schema": "pi.perf.parameter_sweeps.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json"
        },
        "readiness": {
            "status": "ready",
            "ready_for_phase5": false,
            "blocking_reasons": ["awaiting artifact"]
        },
        "selected_defaults": {
            "flush_cadence_ms": 125,
            "queue_max_items": 64,
            "compaction_quota_mb": 8
        },
        "sweep_plan": {
            "dimensions": [
                {
                    "name": "flush_cadence_ms",
                    "candidate_values": [50, 125, 250]
                },
                {
                    "name": "queue_max_items",
                    "candidate_values": [32, 64, 128]
                },
                {
                    "name": "compaction_quota_mb",
                    "candidate_values": [4, 8, 12]
                }
            ]
        }
    });

    let (signal, detail) = validate_parameter_sweeps_artifact(&artifact);
    assert_eq!(signal, Signal::Fail);
    assert!(
        detail.contains("ready_for_phase5"),
        "expected readiness coherence failure detail, got: {detail}"
    );
}

#[test]
fn opportunity_matrix_contract_accepts_consistent_shape() {
    let artifact = serde_json::json!({
        "schema": "pi.perf.opportunity_matrix.v1",
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": "tests/perf/reports/phase1_matrix_validation.json",
            "weighted_bottleneck_schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
            "weighted_bottleneck_status": "computed"
        },
        "readiness": {
            "status": "ready",
            "decision": "RANKED",
            "ready_for_phase5": true,
            "blocking_reasons": []
        },
        "ranked_opportunities": [
            {
                "rank": 1,
                "stage": "phase2_persistence",
                "priority_score": 2.5
            }
        ]
    });

    let (signal, detail) = validate_opportunity_matrix_artifact(&artifact);
    assert_eq!(signal, Signal::Pass, "{detail}");
}

#[test]
fn opportunity_matrix_contract_rejects_readiness_incoherence() {
    let artifact = serde_json::json!({
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
            "ready_for_phase5": false,
            "blocking_reasons": ["phase1_matrix_not_ready_for_phase5"]
        },
        "ranked_opportunities": [
            {
                "rank": 1,
                "stage": "phase2_persistence",
                "priority_score": 2.5
            }
        ]
    });

    let (signal, detail) = validate_opportunity_matrix_artifact(&artifact);
    assert_eq!(signal, Signal::Fail);
    assert!(
        detail.contains("ready_for_phase5") || detail.contains("decision"),
        "expected readiness coherence failure detail, got: {detail}"
    );
}
