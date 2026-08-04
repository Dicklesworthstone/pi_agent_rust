//! Release-readiness verification report generator (bd-k5q5.7.11).
//!
//! Aggregates evidence from conformance, performance, security, and traceability
//! into a single user-focused release-readiness summary.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const REPORT_SCHEMA: &str = "pi.release_readiness.v1";
const MUST_PASS_GATE_SCHEMA: &str = "pi.ext.must_pass_gate.v1";
const CERTIFIED_MUST_PASS_MINIMUM: u64 = 208;
const NON_MOCK_RUBRIC_SCHEMA: &str = "pi.qa.non_mock_rubric.v1";
const FULL_SUITE_GATE_SCHEMA: &str = "pi.ci.full_suite_gate.v1";
const EXT_REMEDIATION_BACKLOG_SCHEMA: &str = "pi.qa.extension_remediation_backlog.v1";
const PRACTICAL_FINISH_CHECKPOINT_SCHEMA: &str = "pi.perf3x.practical_finish_checkpoint.v1";
const PARAMETER_SWEEPS_SCHEMA: &str = "pi.perf.parameter_sweeps.v1";
const PARAMETER_SWEEPS_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/parameter_sweeps.json";
const OPPORTUNITY_MATRIX_SCHEMA: &str = "pi.perf.opportunity_matrix.v1";
const OPPORTUNITY_MATRIX_PRIMARY_ARTIFACT_REL: &str = "tests/perf/reports/opportunity_matrix.json";

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
    let status = match get_str(v, "/status") {
        "unknown" => get_str(v, "/verdict").to_string(),
        value => value.to_string(),
    };

    let total = match get_u64(v, "/observed/must_pass_total") {
        0 => get_u64(v, "/total"),
        value => value,
    };
    let passed = match get_u64(v, "/observed/must_pass_passed") {
        0 => get_u64(v, "/passed"),
        value => value,
    };

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

    for field in ["/generated_at", "/run_id", "/correlation_id"] {
        if get_str(v, field) == "unknown" {
            errors.push(format!("missing required field: {field}"));
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

fn validate_current_conformance_summary(v: &V) -> (Signal, String) {
    const SCHEMA: &str = "pi.ext.conformance_summary.v2";
    let schema = get_str(v, "/schema");
    if schema != SCHEMA {
        return (
            Signal::Fail,
            format!("Invalid schema: expected {SCHEMA}, found {schema}"),
        );
    }

    let count = |name: &str| {
        v.pointer(&format!("/counts/{name}"))
            .and_then(V::as_u64)
            .ok_or_else(|| format!("Missing required unsigned count: /counts/{name}"))
    };
    let (total, tested, passed, failed, not_applicable) = match (
        count("total"),
        count("tested"),
        count("pass"),
        count("fail"),
        count("na"),
    ) {
        (Ok(total), Ok(tested), Ok(passed), Ok(failed), Ok(not_applicable)) => {
            (total, tested, passed, failed, not_applicable)
        }
        counts => {
            let errors = [counts.0, counts.1, counts.2, counts.3, counts.4]
                .into_iter()
                .filter_map(Result::err)
                .collect::<Vec<_>>();
            return (Signal::Fail, errors.join("; "));
        }
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

    (
        Signal::Pass,
        format!("Current conformance complete: {passed}/{total} pass"),
    )
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
    let path = root.join("tests/ext_conformance/reports/conformance_summary.json");
    load_json(&path).map_or_else(
        || no_data(name, "conformance_summary.json not found"),
        |v| {
            let run_id = v
                .pointer("/run_id")
                .and_then(V::as_str)
                .map_or("", str::trim);
            let correlation_id = v
                .pointer("/correlation_id")
                .and_then(V::as_str)
                .map_or("", str::trim);
            if run_id.is_empty() || correlation_id.is_empty() {
                let mut missing = Vec::new();
                if run_id.is_empty() {
                    missing.push("run_id");
                }
                if correlation_id.is_empty() {
                    missing.push("correlation_id");
                }
                return DimensionScore {
                    name: name.to_string(),
                    signal: Signal::Fail,
                    detail: format!(
                        "conformance summary missing required lineage field(s): {}",
                        missing.join(", ")
                    ),
                };
            }

            let (signal, validation_detail) = validate_current_conformance_summary(&v);
            let neg_pass = get_u64(&v, "/negative/pass");
            let neg_fail = get_u64(&v, "/negative/fail");

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{validation_detail}; negative tests: {neg_pass} pass, {neg_fail} fail"
                ),
            }
        },
    )
}

fn collect_performance(root: &Path) -> DimensionScore {
    let name = "Performance Budgets";
    let path = root.join("tests/perf/reports/budget_summary.json");
    load_json(&path).map_or_else(
        || no_data(name, "budget_summary.json not found"),
        |v| {
            let total = get_u64(&v, "/total_budgets");
            let pass = get_u64(&v, "/pass");
            let fail = get_u64(&v, "/fail");
            let ci_enforced = get_u64(&v, "/ci_enforced");
            let ci_fail = get_u64(&v, "/ci_fail");
            let no_data_count = get_u64(&v, "/no_data");

            let signal = if ci_fail > 0 {
                Signal::Fail
            } else if fail > 0 || no_data_count > total / 2 {
                Signal::Warn
            } else {
                Signal::Pass
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{pass}/{total} pass, {fail} fail, {no_data_count} no data; {ci_enforced} CI-enforced ({ci_fail} CI fail)"
                ),
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
    if let Some(v) = load_json(&baseline_path) {
        if let Some(arr) = v
            .pointer("/scenario_conformance/failures")
            .and_then(V::as_array)
        {
            for f in arr {
                let id = get_str(f, "/id");
                let cause = get_str(f, "/cause");
                issues.push(format!("Scenario {id}: {cause}"));
            }
        }
    }

    // Performance no-data budgets
    let perf_path = root.join("tests/perf/reports/budget_summary.json");
    if let Some(v) = load_json(&perf_path) {
        let nd = get_u64(&v, "/no_data");
        if nd > 0 {
            issues.push(format!(
                "{nd} performance budgets have no measured data yet"
            ));
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
    assert!(dim.detail.contains("60/226 tested"), "{}", dim.detail);
}

#[test]
fn conformance_dimension_fail_closed_when_lineage_missing() {
    let root = tempdir().expect("create tempdir");
    let reports_dir = root.path().join("tests/ext_conformance/reports");
    std::fs::create_dir_all(&reports_dir).expect("create conformance reports dir");
    let summary_path = reports_dir.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "generated_at": "2026-02-17T06:00:00Z",
            "counts": { "total": 10, "tested": 10, "pass": 10, "fail": 0, "na": 0 },
            "pass_rate_pct": 100.0,
            "negative": { "pass": 1, "fail": 0 }
        }))
        .expect("serialize fixture"),
    )
    .expect("write conformance summary fixture");

    let dim = collect_conformance(root.path());
    assert_eq!(dim.signal, Signal::Fail, "{}", dim.detail);
    assert!(
        dim.detail.contains("run_id") && dim.detail.contains("correlation_id"),
        "expected missing lineage fields in detail, got: {}",
        dim.detail
    );
}

#[test]
fn conformance_dimension_fail_closed_when_run_id_missing() {
    let root = tempdir().expect("create tempdir");
    let reports_dir = root.path().join("tests/ext_conformance/reports");
    std::fs::create_dir_all(&reports_dir).expect("create conformance reports dir");
    let summary_path = reports_dir.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "generated_at": "2026-02-17T06:00:00Z",
            "correlation_id": "corr-123",
            "counts": { "total": 10, "pass": 10, "fail": 0 },
            "pass_rate_pct": 100.0,
            "negative": { "pass": 1, "fail": 0 }
        }))
        .expect("serialize fixture"),
    )
    .expect("write conformance summary fixture");

    let dim = collect_conformance(root.path());
    assert_eq!(dim.signal, Signal::Fail, "{}", dim.detail);
    assert!(
        dim.detail.contains("run_id"),
        "expected missing run_id in detail, got: {}",
        dim.detail
    );
}

#[test]
fn conformance_dimension_fail_closed_when_correlation_id_missing() {
    let root = tempdir().expect("create tempdir");
    let reports_dir = root.path().join("tests/ext_conformance/reports");
    std::fs::create_dir_all(&reports_dir).expect("create conformance reports dir");
    let summary_path = reports_dir.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "generated_at": "2026-02-17T06:00:00Z",
            "run_id": "run-123",
            "counts": { "total": 10, "pass": 10, "fail": 0 },
            "pass_rate_pct": 100.0,
            "negative": { "pass": 1, "fail": 0 }
        }))
        .expect("serialize fixture"),
    )
    .expect("write conformance summary fixture");

    let dim = collect_conformance(root.path());
    assert_eq!(dim.signal, Signal::Fail, "{}", dim.detail);
    assert!(
        dim.detail.contains("correlation_id"),
        "expected missing correlation_id in detail, got: {}",
        dim.detail
    );
}

#[test]
fn conformance_dimension_accepts_lineage_when_present() {
    let root = tempdir().expect("create tempdir");
    let reports_dir = root.path().join("tests/ext_conformance/reports");
    std::fs::create_dir_all(&reports_dir).expect("create conformance reports dir");
    let summary_path = reports_dir.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "pi.ext.conformance_summary.v2",
            "generated_at": "2026-02-17T06:00:00Z",
            "run_id": "run-123",
            "correlation_id": "corr-123",
            "counts": { "total": 10, "tested": 10, "pass": 10, "fail": 0, "na": 0 },
            "pass_rate_pct": 100.0,
            "negative": { "pass": 1, "fail": 0 }
        }))
        .expect("serialize fixture"),
    )
    .expect("write conformance summary fixture");

    let dim = collect_conformance(root.path());
    assert_eq!(dim.signal, Signal::Pass, "{}", dim.detail);
}

#[test]
fn performance_dimension_has_data() {
    let dim = collect_performance(&repo_root());
    assert_ne!(dim.signal, Signal::NoData, "performance: {}", dim.detail);
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
    let artifact_path = root.join(artifact_rel);
    let (status, detail, sha) = load_json(&artifact_path).map_or_else(
        || {
            (
                Signal::NoData,
                format!("Artifact not found: {artifact_rel}"),
                None,
            )
        },
        |v| {
            let (sig, det) = check(&v);
            let sha = sha256_file(&artifact_path);
            (sig, det, sha)
        },
    );
    CertEvidence {
        gate: gate.to_string(),
        bead: bead.to_string(),
        status,
        detail,
        artifact_path: Some(artifact_rel.to_string()),
        artifact_sha256: sha,
    }
}

#[allow(clippy::too_many_lines)]
fn generate_certification() -> FinalCertification {
    let root = repo_root();
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
    evidence.push(check_cert_gate(
        &root,
        "e2e_evidence",
        "bd-1f42.3",
        "tests/ext_conformance/reports/conformance_summary.json",
        validate_current_conformance_summary,
    ));

    // 3. 208/208 must-pass proof
    evidence.push(check_cert_gate(
        &root,
        "must_pass_208",
        "bd-1f42.4",
        "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json",
        |v| {
            let metadata_errors = validate_must_pass_gate_metadata(v);
            if !metadata_errors.is_empty() {
                return (
                    Signal::Fail,
                    format!(
                        "Must-pass gate metadata invalid: {}",
                        metadata_errors.join("; ")
                    ),
                );
            }

            let (verdict, passed, total) = parse_must_pass_gate_verdict(v);
            if verdict == "pass"
                && passed >= CERTIFIED_MUST_PASS_MINIMUM
                && total >= CERTIFIED_MUST_PASS_MINIMUM
            {
                (Signal::Pass, format!("{passed}/{total} must-pass: PASS"))
            } else if verdict == "pass" {
                (
                    Signal::Fail,
                    format!(
                        "{passed}/{total} must-pass artifact says pass, but certification requires at least {CERTIFIED_MUST_PASS_MINIMUM} passing items"
                    ),
                )
            } else if verdict == "unknown" {
                (
                    Signal::Fail,
                    format!(
                        "Must-pass gate verdict missing status/verdict field ({passed}/{total} passed)"
                    ),
                )
            } else if passed >= 200 {
                (Signal::Warn, format!("{passed}/{total} must-pass ({verdict})"))
            } else {
                (Signal::Fail, format!("{passed}/{total} must-pass ({verdict})"))
            }
        },
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
    evidence.push(check_cert_gate(
        &root,
        "health_delta",
        "bd-1f42.4.5",
        "tests/ext_conformance/reports/conformance_summary.json",
        validate_current_conformance_summary,
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
        gate_ids.contains(&"must_pass_208"),
        "Missing must_pass_208 gate"
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

    // Write artifacts
    let out_dir = repo_root().join("tests/certification");
    let _ = std::fs::create_dir_all(&out_dir);

    let json_out = out_dir.join("final_certification.json");
    let json = serde_json::to_string_pretty(&cert).expect("serialize");
    std::fs::write(&json_out, &json).expect("write JSON");

    let md_out = out_dir.join("final_certification.md");
    std::fs::write(&md_out, &md).expect("write markdown");

    let events_out = out_dir.join("certification_events.jsonl");
    let mut events = String::new();
    for ev in &cert.evidence {
        let event = serde_json::json!({
            "schema": "pi.qa.certification_event.v1",
            "timestamp": cert.generated_at,
            "gate": ev.gate,
            "bead": ev.bead,
            "status": ev.status,
            "detail": ev.detail,
            "artifact_sha256": ev.artifact_sha256,
        });
        let _ = writeln!(events, "{}", serde_json::to_string(&event).expect("event"));
    }
    std::fs::write(&events_out, &events).expect("write events");

    eprintln!("Certification artifacts:");
    eprintln!("  JSON: {}", json_out.display());
    eprintln!("  MD:   {}", md_out.display());
    eprintln!("  JSONL: {}", events_out.display());
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
fn parse_must_pass_gate_verdict_falls_back_to_legacy_schema() {
    let gate = serde_json::json!({
        "verdict": "warn",
        "total": 208,
        "passed": 203
    });

    let (status, passed, total) = parse_must_pass_gate_verdict(&gate);
    assert_eq!(status, "warn");
    assert_eq!(passed, 203);
    assert_eq!(total, 208);
}

#[test]
fn validate_must_pass_gate_metadata_accepts_current_schema() {
    let gate = serde_json::json!({
        "schema": "pi.ext.must_pass_gate.v1",
        "generated_at": "2026-02-17T03:06:08.928Z",
        "run_id": "local-20260217T030608928Z",
        "correlation_id": "must-pass-gate-local-20260217T030608928Z",
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
    let summary = serde_json::json!({
        "schema": "pi.ext.conformance_summary.v2",
        "counts": {"total": 2, "tested": 2, "pass": 2, "fail": 0, "na": 0}
    });

    let (signal, detail) = validate_current_conformance_summary(&summary);
    assert_eq!(signal, Signal::Pass, "{detail}");
    assert!(detail.contains("2/2 pass"), "{detail}");
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
