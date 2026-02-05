//! Generate extension conformance reports (bd-2jha).
//!
//! This binary is intentionally small: it reads per-extension results as JSON,
//! computes summary statistics, and writes both JSON and Markdown reports.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pi::conformance::report::{ExtensionConformanceResult, generate_report};
use uuid::Uuid;

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

    println!("Wrote: {}", json_path.display());
    println!("Wrote: {}", md_path.display());
    Ok(())
}
