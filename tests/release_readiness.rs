//! Release-readiness verification report generator (bd-k5q5.7.11).
//!
//! Aggregates evidence from conformance, performance, security, and traceability
//! into a single user-focused release-readiness summary.

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::path::{Path, PathBuf};

const REPORT_SCHEMA: &str = "pi.release_readiness.v1";

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
            let pass_rate = get_f64(&v, "/pass_rate_pct");
            let pass = get_u64(&v, "/counts/pass");
            let fail = get_u64(&v, "/counts/fail");
            let total = get_u64(&v, "/counts/total");
            let neg_pass = get_u64(&v, "/negative/pass");
            let neg_fail = get_u64(&v, "/negative/fail");

            let signal = if fail == 0 {
                Signal::Pass
            } else if pass_rate >= 90.0 {
                Signal::Warn
            } else {
                Signal::Fail
            };

            DimensionScore {
                name: name.to_string(),
                signal,
                detail: format!(
                    "{pass}/{total} pass ({pass_rate:.1}%), {fail} fail; negative tests: {neg_pass} pass, {neg_fail} fail"
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
