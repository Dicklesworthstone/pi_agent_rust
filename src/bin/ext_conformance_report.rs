//! Generate extension conformance reports (bd-2jha).
//!
//! This binary is intentionally small: it reads per-extension results as JSON,
//! computes summary statistics, and writes both JSON and Markdown reports.
//!
//! It also archives per-run reports and produces a simple trend file (bd-7rmt).

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Parser, ValueEnum};
use pi::conformance::report::{
    ConformanceRegression, ConformanceReport, ExtensionConformanceResult, compute_regression,
    generate_report,
};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum RegressionMode {
    Ignore,
    Warn,
    Fail,
}

#[derive(Debug, Parser)]
#[command(name = "ext_conformance_report")]
#[command(about = "Generate extension conformance report JSON + Markdown")]
struct Args {
    /// Path to a JSON file containing `ExtensionConformanceResult[]`.
    #[arg(long)]
    input: PathBuf,

    /// Output directory. Files written: `conformance_report.json`, `conformance_report.md`
    #[arg(long, default_value = "tests/ext_conformance/reports")]
    out_dir: PathBuf,

    /// Optional run id (default: run-<uuid>).
    #[arg(long)]
    run_id: Option<String>,

    /// Optional RFC3339 timestamp to embed (default: now).
    #[arg(long)]
    timestamp: Option<String>,

    /// How to handle regressions compared to the previous archived report.
    #[arg(long, value_enum, default_value = "warn")]
    regression_mode: RegressionMode,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConformanceTrendPoint {
    run_id: String,
    timestamp: String,
    total: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    errors: u64,
    pass_rate: f64,
    archive: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConformanceTrend {
    schema: &'static str,
    generated_at: String,
    runs: Vec<ConformanceTrendPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_regression: Option<ConformanceRegression>,
}

fn parse_report_timestamp(report: &ConformanceReport) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&report.timestamp)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

fn report_sort_key(report: &ConformanceReport) -> (i64, &str) {
    let timestamp = parse_report_timestamp(report).map_or(0, |ts| ts.timestamp());
    (timestamp, report.run_id.as_str())
}

fn archive_path_for_report(out_dir: &Path, report: &ConformanceReport) -> PathBuf {
    let ts = parse_report_timestamp(report);
    let date = ts.map_or_else(
        || "unknown-date".to_string(),
        |ts| ts.format("%Y-%m-%d").to_string(),
    );
    let time = ts.map_or_else(
        || "unknown-time".to_string(),
        |ts| ts.format("%H%M%SZ").to_string(),
    );

    let date_only = out_dir.join(format!("conformance_{date}.json"));
    if !date_only.exists() {
        return date_only;
    }

    // Multiple runs per day: add time and run id to keep archives unique.
    out_dir.join(format!("conformance_{date}_{time}_{}.json", report.run_id))
}

fn is_archive_filename(name: &str) -> bool {
    if !name.starts_with("conformance_")
        || !Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        return false;
    }
    if name == "conformance_report.json" || name == "conformance_trend.json" {
        return false;
    }
    true
}

fn read_archives(out_dir: &Path) -> Result<Vec<(PathBuf, ConformanceReport)>> {
    let mut reports = Vec::new();
    if !out_dir.exists() {
        return Ok(reports);
    }

    for entry in std::fs::read_dir(out_dir)
        .with_context(|| format!("list conformance report directory: {}", out_dir.display()))?
    {
        let entry = entry.context("read_dir entry")?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_archive_filename(name) {
            continue;
        }

        let bytes = std::fs::read(&path)
            .with_context(|| format!("read archived report: {}", path.display()))?;
        let report: ConformanceReport =
            serde_json::from_slice(&bytes).context("parse archived report JSON")?;
        reports.push((path, report));
    }

    reports.sort_by(|(_, left), (_, right)| report_sort_key(left).cmp(&report_sort_key(right)));

    Ok(reports)
}

fn build_trend(
    archives: &[(PathBuf, ConformanceReport)],
    latest_regression: Option<ConformanceRegression>,
) -> ConformanceTrend {
    let runs = archives
        .iter()
        .map(|(path, report)| ConformanceTrendPoint {
            run_id: report.run_id.clone(),
            timestamp: report.timestamp.clone(),
            total: report.summary.total,
            passed: report.summary.passed,
            failed: report.summary.failed,
            skipped: report.summary.skipped,
            errors: report.summary.errors,
            pass_rate: report.summary.pass_rate,
            archive: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
        })
        .collect::<Vec<_>>();

    ConformanceTrend {
        schema: "pi.ext.conformance_trend.v1",
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        runs,
        latest_regression,
    }
}

fn print_regression(regression: &ConformanceRegression) {
    eprintln!(
        "Conformance regression: pass rate {:.2}% → {:.2}% (Δ {:.2}%) on {} prior extensions",
        regression.previous_pass_rate * 100.0,
        regression.current_pass_rate * 100.0,
        regression.pass_rate_delta * 100.0,
        regression.compared_total
    );

    for entry in &regression.regressed_extensions {
        let current = entry
            .current
            .map_or_else(|| "missing".to_string(), |s| s.as_upper_str().to_string());
        eprintln!("  - {}: PASS → {}", entry.id, current);
        // GitHub Actions annotation (harmless outside Actions).
        eprintln!(
            "::error title=Conformance regression::{} regressed (PASS → {})",
            entry.id, current
        );
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let bytes = std::fs::read(&args.input)
        .with_context(|| format!("read input JSON: {}", args.input.display()))?;
    let results: Vec<ExtensionConformanceResult> =
        serde_json::from_slice(&bytes).context("parse input JSON")?;

    let run_id = args
        .run_id
        .unwrap_or_else(|| format!("run-{}", Uuid::new_v4()));
    let report = generate_report(run_id, args.timestamp, results);

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create output dir: {}", args.out_dir.display()))?;

    let json_path = args.out_dir.join("conformance_report.json");
    let md_path = args.out_dir.join("conformance_report.md");

    let json = serde_json::to_string_pretty(&report).context("serialize report JSON")?;
    std::fs::write(&json_path, json.as_bytes())
        .with_context(|| format!("write {}", json_path.display()))?;

    let md = report.render_markdown();
    std::fs::write(&md_path, md.as_bytes())
        .with_context(|| format!("write {}", md_path.display()))?;

    let archive_path = archive_path_for_report(&args.out_dir, &report);
    std::fs::write(&archive_path, json.as_bytes())
        .with_context(|| format!("write archive {}", archive_path.display()))?;

    // Build/update trend + regression signals.
    let mut archives = read_archives(&args.out_dir)?;
    if !archives.iter().any(|(path, _)| path == &archive_path) {
        archives.push((archive_path.clone(), report.clone()));
    }
    archives.sort_by(|(_, left), (_, right)| report_sort_key(left).cmp(&report_sort_key(right)));

    let latest_regression = if archives.len() >= 2 {
        let (_, previous) = &archives[archives.len() - 2];
        let regression = compute_regression(previous, &report);
        if regression.has_regression() {
            Some(regression)
        } else {
            None
        }
    } else {
        None
    };

    let trend = build_trend(&archives, latest_regression.clone());
    let trend_path = args.out_dir.join("conformance_trend.json");
    let trend_json = serde_json::to_string_pretty(&trend).context("serialize trend JSON")?;
    std::fs::write(&trend_path, trend_json.as_bytes())
        .with_context(|| format!("write {}", trend_path.display()))?;

    println!("Wrote: {}", json_path.display());
    println!("Wrote: {}", md_path.display());
    println!("Archived: {}", archive_path.display());
    println!("Wrote: {}", trend_path.display());

    if let Some(regression) = latest_regression {
        print_regression(&regression);
        if args.regression_mode == RegressionMode::Fail {
            anyhow::bail!("conformance regression detected");
        }
    }
    Ok(())
}
