//! Integration tests for extension preflight analysis.
//!
//! This suite validates end-to-end behavior across:
//! - `CompatibilityScanner` extraction
//! - `PreflightAnalyzer` policy + compatibility evaluation
//! - structured report categories and verdicts

use pi::extension_preflight::{
    FindingCategory, FindingSeverity, PreflightAnalyzer, PreflightReport, PreflightVerdict,
};
use pi::extensions::{CompatibilityScanner, ExtensionPolicy};
use std::fs;

fn analyze_source(source: &str) -> PreflightReport {
    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("test-ext"));
    analyzer.analyze_source("test-ext", source)
}

fn analyze_path_source(source: &str) -> PreflightReport {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("extension.ts");
    fs::write(&entry, source).expect("write extension file");

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("test-ext"));
    analyzer.analyze(&entry)
}

#[test]
fn empty_extension_reports_pass() {
    let report = analyze_source("// empty extension\n");
    assert_eq!(report.verdict, PreflightVerdict::Pass);
    assert_eq!(report.summary.errors, 0);
    assert_eq!(report.summary.warnings, 0);
    assert!(report.findings.is_empty());
}

#[test]
fn forbidden_builtin_import_reports_error_and_fail() {
    let report = analyze_path_source(
        r#"
import { createContext } from "vm";
const ctx = createContext({});
"#,
    );

    assert_eq!(report.verdict, PreflightVerdict::Fail);
    assert!(report.findings.iter().any(|finding| {
        finding.category == FindingCategory::ForbiddenPattern
            && finding.severity == FindingSeverity::Error
            && finding.message.contains("forbidden")
    }));
}

#[test]
fn flagged_eval_reports_warning() {
    let report = analyze_path_source(
        r#"
const v = eval("1 + 1");
"#,
    );

    assert_eq!(report.verdict, PreflightVerdict::Warn);
    assert!(report.findings.iter().any(|finding| {
        finding.category == FindingCategory::FlaggedPattern
            && finding.severity == FindingSeverity::Warning
            && finding.message.contains("eval")
    }));
}

#[test]
fn denied_exec_capability_is_reported() {
    let report = analyze_source(
        r#"
import { spawn } from "node:child_process";
spawn("echo", ["hello"]);
"#,
    );

    assert_eq!(report.verdict, PreflightVerdict::Fail);
    assert!(report.findings.iter().any(|finding| {
        finding.category == FindingCategory::CapabilityPolicy
            && finding.severity == FindingSeverity::Error
            && finding.message.contains("exec")
    }));
}

#[test]
fn commented_patterns_do_not_create_false_findings() {
    let report = analyze_path_source(
        r#"
// import { createContext } from "vm";
/*
process.binding("fs");
const bad = eval("2 + 2");
*/
import fs from "fs";
const data = fs.readFileSync("/tmp/demo", "utf8");
"#,
    );

    // No forbidden/flagged findings should be produced from commented patterns.
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.message.contains("process.binding"))
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.message.contains("eval("))
    );

    // Live fs import is fully supported, so this should remain PASS.
    assert_eq!(report.verdict, PreflightVerdict::Pass);
}

#[test]
fn scanner_and_preflight_category_counts_align() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("sample.ts");
    fs::write(
        &entry,
        r#"
import { createContext } from "vm";
const a = eval("1");
const b = new Function("return 2");
"#,
    )
    .expect("write sample file");

    let scanner = CompatibilityScanner::new(dir.path().to_path_buf());
    let ledger = scanner.scan_path(&entry).expect("scan");

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("test-ext"));
    let report = analyzer.analyze(&entry);

    let forbidden_count = report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::ForbiddenPattern)
        .count();
    let flagged_count = report
        .findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::FlaggedPattern)
        .count();

    assert_eq!(forbidden_count, ledger.forbidden.len());
    assert_eq!(flagged_count, ledger.flagged.len());
}

#[test]
fn report_json_contains_core_fields() {
    let report = analyze_source(
        r#"
import { spawn } from "node:child_process";
const x = eval("1");
"#,
    );

    let json = report.to_json().expect("serialize report");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse report json");

    assert!(value.get("schema").is_some());
    assert!(value.get("verdict").is_some());
    assert!(value.get("summary").is_some());
    assert!(value.get("findings").is_some());
}
