#![forbid(unsafe_code)]

//! CLI binary: Generate conformance test matrix from inclusion list + API matrix.
//!
//! ```text
//! cargo run --bin ext_conformance_matrix -- \
//!   --inclusion docs/extension-inclusion-list.json \
//!   --api-matrix docs/extension-api-matrix.json \
//!   --out docs/extension-conformance-matrix.json
//! ```

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pi::extension_conformance_matrix::{ApiMatrix, build_test_plan};
use pi::extension_inclusion::InclusionList;

#[derive(Debug, Parser)]
#[command(name = "ext_conformance_matrix")]
#[command(about = "Generate extension conformance test matrix + test plan")]
struct Args {
    /// Path to inclusion list JSON.
    #[arg(long)]
    inclusion: PathBuf,

    /// Path to API matrix JSON (optional).
    #[arg(long)]
    api_matrix: Option<PathBuf>,

    /// Output path for conformance matrix JSON.
    #[arg(long)]
    out: PathBuf,

    /// Task ID for provenance tracking.
    #[arg(long, default_value = "bd-2kyq")]
    task_id: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Load inclusion list.
    let inclusion_text = fs::read_to_string(&args.inclusion)
        .with_context(|| format!("reading inclusion list from {}", args.inclusion.display()))?;
    let inclusion: InclusionList = serde_json::from_str(&inclusion_text)
        .with_context(|| format!("parsing inclusion list from {}", args.inclusion.display()))?;

    // Load API matrix (optional).
    let api_matrix: Option<ApiMatrix> = args
        .api_matrix
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading API matrix from {}", p.display()))?;
            let matrix: ApiMatrix = serde_json::from_str(&text)
                .with_context(|| format!("parsing API matrix from {}", p.display()))?;
            Ok::<_, anyhow::Error>(matrix)
        })
        .transpose()?;

    // Build the test plan.
    let plan = build_test_plan(&inclusion, api_matrix.as_ref(), &args.task_id);

    // Write output.
    let json = serde_json::to_string_pretty(&plan).context("serializing conformance matrix")?;
    fs::write(&args.out, format!("{json}\n"))
        .with_context(|| format!("writing output to {}", args.out.display()))?;

    // Print summary.
    eprintln!("=== Conformance Test Matrix ===");
    eprintln!("Matrix cells:        {}", plan.coverage.total_cells);
    eprintln!("  Required:          {}", plan.coverage.required_cells);
    eprintln!("  Covered:           {}", plan.coverage.covered_cells);
    eprintln!(
        "  Uncovered req:     {}",
        plan.coverage.uncovered_required_cells
    );
    eprintln!();
    eprintln!(
        "Exemplar extensions: {}",
        plan.coverage.total_exemplar_extensions
    );
    eprintln!("Categories covered:  {}", plan.coverage.categories_covered);
    eprintln!(
        "Capabilities covered:{}",
        plan.coverage.capabilities_covered
    );
    eprintln!();

    // Fixture coverage
    let uncovered: Vec<_> = plan
        .fixture_assignments
        .iter()
        .filter(|a| !a.coverage_met)
        .collect();
    if uncovered.is_empty() {
        eprintln!("All cells have adequate fixture coverage.");
    } else {
        eprintln!("Cells needing more fixtures ({}):", uncovered.len());
        for a in &uncovered {
            eprintln!(
                "  {} — have {}, need {}",
                a.cell_key,
                a.fixture_extensions.len(),
                a.min_fixtures
            );
        }
    }
    eprintln!();

    // Category criteria summary
    eprintln!("Category criteria ({}):", plan.category_criteria.len());
    for cc in &plan.category_criteria {
        eprintln!(
            "  {:?}: {} must-pass, {} failure conditions",
            cc.category,
            cc.must_pass.len(),
            cc.failure_conditions.len()
        );
    }

    eprintln!("\nOutput written to: {}", args.out.display());

    Ok(())
}
