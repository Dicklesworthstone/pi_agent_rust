#![forbid(unsafe_code)]

//! CLI binary: Validate and deduplicate Pi extension candidates from multiple
//! research sources (code search, repo search, npm scan, curated lists, existing pool).
//!
//! Usage:
//! ```text
//! cargo run --bin ext_validate_dedup -- \
//!   --code-search docs/extension-code-search-inventory.json \
//!   --repo-search docs/extension-repo-search-summary.json \
//!   --npm-scan docs/extension-npm-scan-summary.json \
//!   --curated-list docs/extension-curated-list-summary.json \
//!   --candidate-pool docs/extension-candidate-pool.json \
//!   --out docs/extension-validated-dedup.json \
//!   --log-out /tmp/validation.jsonl
//! ```

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pi::extension_popularity::CandidatePool;
use pi::extension_validation::{
    CodeSearchInventory, CuratedListSummary, NpmScanSummary, RepoSearchSummary, ValidationConfig,
    ValidationStatus, run_validation_pipeline,
};

#[derive(Debug, Parser)]
#[command(name = "ext_validate_dedup")]
#[command(about = "Validate and deduplicate Pi extension candidates")]
struct Args {
    /// Path to GitHub code search inventory JSON.
    #[arg(long)]
    code_search: Option<PathBuf>,

    /// Path to GitHub repo search summary JSON.
    #[arg(long)]
    repo_search: Option<PathBuf>,

    /// Path to npm scan summary JSON.
    #[arg(long)]
    npm_scan: Option<PathBuf>,

    /// Path to curated list summary JSON.
    #[arg(long)]
    curated_list: Option<PathBuf>,

    /// Path to existing candidate pool JSON.
    #[arg(long)]
    candidate_pool: Option<PathBuf>,

    /// Output path for validated + deduped JSON.
    #[arg(long)]
    out: PathBuf,

    /// Output path for JSONL decision log.
    #[arg(long)]
    log_out: Option<PathBuf>,

    /// Task ID for provenance tracking.
    #[arg(long, default_value = "bd-28ov")]
    task_id: String,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();

    // Load input files.
    let code_search: Option<CodeSearchInventory> = args
        .code_search
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading code search from {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing code search from {}", p.display()))
        })
        .transpose()?;

    let repo_search: Option<RepoSearchSummary> = args
        .repo_search
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading repo search from {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing repo search from {}", p.display()))
        })
        .transpose()?;

    let npm_scan: Option<NpmScanSummary> = args
        .npm_scan
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading npm scan from {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing npm scan from {}", p.display()))
        })
        .transpose()?;

    let curated_list: Option<CuratedListSummary> = args
        .curated_list
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading curated list from {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing curated list from {}", p.display()))
        })
        .transpose()?;

    let candidate_pool: Option<CandidatePool> = args
        .candidate_pool
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading candidate pool from {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing candidate pool from {}", p.display()))
        })
        .transpose()?;

    let config = ValidationConfig {
        task_id: args.task_id,
    };

    // Run pipeline.
    let report = run_validation_pipeline(
        code_search.as_ref(),
        repo_search.as_ref(),
        npm_scan.as_ref(),
        curated_list.as_ref(),
        candidate_pool.as_ref(),
        &config,
    );

    // Write output.
    let json = serde_json::to_string_pretty(&report).context("serializing validation report")?;
    fs::write(&args.out, &json)
        .with_context(|| format!("writing output to {}", args.out.display()))?;

    // Write JSONL decision log (one line per candidate).
    if let Some(log_path) = &args.log_out {
        let mut log_file = fs::File::create(log_path)
            .with_context(|| format!("creating log file {}", log_path.display()))?;
        for candidate in &report.candidates {
            let line = serde_json::to_string(candidate).context("serializing log entry")?;
            writeln!(log_file, "{line}").context("writing log entry")?;
        }
    }

    // Print summary.
    eprintln!("=== Validation + Dedup Report ===");
    eprintln!(
        "Total input candidates: {}",
        report.stats.total_input_candidates
    );
    eprintln!("After dedup:            {}", report.stats.after_dedup);
    eprintln!("  True extensions:      {}", report.stats.true_extension);
    eprintln!("  Mention-only:         {}", report.stats.mention_only);
    eprintln!("  Unknown:              {}", report.stats.unknown);
    eprintln!("Sources merged:         {}", report.stats.sources_merged);

    // Classification coverage check.
    let classified = report.stats.true_extension + report.stats.mention_only;
    let total = report.stats.after_dedup;
    #[allow(clippy::cast_precision_loss)]
    let coverage_pct = if total > 0 {
        classified as f64 / total as f64 * 100.0
    } else {
        100.0
    };
    eprintln!("Classification coverage: {classified}/{total} ({coverage_pct:.1}%)");

    if coverage_pct < 95.0 {
        eprintln!("WARNING: classification coverage below 95% threshold");
    }

    // Validate status distribution.
    let true_ext_count = report
        .candidates
        .iter()
        .filter(|c| c.status == ValidationStatus::TrueExtension)
        .count();
    eprintln!("\nOutput written to: {}", args.out.display());
    eprintln!("True extensions available for conformance testing: {true_ext_count}");

    Ok(())
}
