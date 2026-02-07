#![forbid(unsafe_code)]

//! CLI binary: Run license + policy screening on validated extension candidates.
//!
//! ```text
//! cargo run --bin ext_license_screen -- \
//!   --validated docs/extension-validated-dedup.json \
//!   --candidate-pool docs/extension-candidate-pool.json \
//!   --out docs/extension-license-report.json \
//!   --log-out /tmp/license-screening.jsonl
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pi::extension_license::{ScreeningInput, screen_extensions};
use pi::extension_popularity::CandidatePool;
use pi::extension_validation::ValidationReport;

#[derive(Debug, Parser)]
#[command(name = "ext_license_screen")]
#[command(about = "License + policy screening for Pi extension candidates")]
struct Args {
    /// Path to validated-dedup JSON (output of ext_validate_dedup).
    #[arg(long)]
    validated: PathBuf,

    /// Path to candidate pool JSON (contains license data).
    #[arg(long)]
    candidate_pool: Option<PathBuf>,

    /// Output path for screening report JSON.
    #[arg(long)]
    out: PathBuf,

    /// Output path for JSONL decision log.
    #[arg(long)]
    log_out: Option<PathBuf>,

    /// Task ID for provenance tracking.
    #[arg(long, default_value = "bd-250p")]
    task_id: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Load validated report.
    let validated_text = fs::read_to_string(&args.validated)
        .with_context(|| format!("reading validated report from {}", args.validated.display()))?;
    let validated: ValidationReport = serde_json::from_str(&validated_text)
        .with_context(|| format!("parsing validated report from {}", args.validated.display()))?;

    // Load candidate pool for license data.
    let license_map: HashMap<String, String> = args
        .candidate_pool
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading candidate pool from {}", p.display()))?;
            let pool: CandidatePool = serde_json::from_str(&text)
                .with_context(|| format!("parsing candidate pool from {}", p.display()))?;
            let mut map = HashMap::new();
            for item in &pool.items {
                if item.license != "UNKNOWN" && !item.license.is_empty() {
                    map.insert(item.id.clone(), item.license.clone());
                    // Also map by name for cross-reference.
                    map.insert(item.name.clone(), item.license.clone());
                }
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Build screening inputs from validated candidates marked as true_extension.
    let inputs: Vec<ScreeningInput> = validated
        .candidates
        .iter()
        .filter(|c| c.status == pi::extension_validation::ValidationStatus::TrueExtension)
        .map(|c| {
            // Try to find license: first by canonical_id, then by name.
            let known_license = license_map
                .get(&c.canonical_id)
                .or_else(|| license_map.get(&c.name))
                .cloned();

            ScreeningInput {
                canonical_id: c.canonical_id.clone(),
                known_license,
                source_tier: c.source_tier.clone(),
            }
        })
        .collect();

    eprintln!("Screening {} true extensions...", inputs.len());

    let report = screen_extensions(&inputs, &args.task_id);

    // Write output.
    let json = serde_json::to_string_pretty(&report).context("serializing screening report")?;
    fs::write(&args.out, &json)
        .with_context(|| format!("writing output to {}", args.out.display()))?;

    // Write JSONL decision log.
    if let Some(log_path) = &args.log_out {
        let mut log_file = fs::File::create(log_path)
            .with_context(|| format!("creating log file {}", log_path.display()))?;
        for verdict in &report.verdicts {
            let line = serde_json::to_string(verdict).context("serializing log entry")?;
            writeln!(log_file, "{line}").context("writing log entry")?;
        }
    }

    // Print summary.
    eprintln!("=== License Screening Report ===");
    eprintln!("Total screened:       {}", report.stats.total_screened);
    eprintln!("  Pass:               {}", report.stats.pass);
    eprintln!("  Pass w/ warnings:   {}", report.stats.pass_with_warnings);
    eprintln!("  Excluded:           {}", report.stats.excluded);
    eprintln!("  Needs review:       {}", report.stats.needs_review);

    // License distribution summary.
    let mut dist: Vec<_> = report.stats.license_distribution.iter().collect();
    dist.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("\nLicense distribution:");
    for (license, count) in &dist {
        eprintln!("  {license:<20} {count}");
    }

    eprintln!("\nOutput written to: {}", args.out.display());

    Ok(())
}
