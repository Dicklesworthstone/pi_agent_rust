#![forbid(unsafe_code)]

//! CLI binary: Generate final inclusion list with version pins.
//!
//! ```text
//! cargo run --bin ext_inclusion_list -- \
//!   --tiered-corpus docs/extension-tiered-corpus.json \
//!   --candidate-pool docs/extension-candidate-pool.json \
//!   --validated docs/extension-validated-dedup.json \
//!   --license-report docs/extension-license-report.json \
//!   --out docs/extension-inclusion-list.json
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pi::extension_inclusion::{
    ExclusionNote, InclusionEntry, InclusionList, InclusionStats, VersionPin, build_rationale,
    classify_registrations,
};
use pi::extension_license::ScreeningReport;
use pi::extension_popularity::{CandidateItem, CandidatePool, CandidateSource};
use pi::extension_scoring::ScoringReport;
use pi::extension_validation::{ValidationReport, ValidationStatus};

#[derive(Debug, Parser)]
#[command(name = "ext_inclusion_list")]
#[command(about = "Generate final extension inclusion list with version pins")]
struct Args {
    /// Path to tiered corpus (scored + ranked).
    #[arg(long)]
    tiered_corpus: PathBuf,

    /// Path to candidate pool (provenance + artifacts).
    #[arg(long)]
    candidate_pool: Option<PathBuf>,

    /// Path to validated-dedup (registration types).
    #[arg(long)]
    validated: Option<PathBuf>,

    /// Path to license screening report.
    #[arg(long)]
    license_report: Option<PathBuf>,

    /// Output path for inclusion list JSON.
    #[arg(long)]
    out: PathBuf,

    /// Task ID for provenance tracking.
    #[arg(long, default_value = "bd-3vb8")]
    task_id: String,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();

    // Load tiered corpus.
    let tiered_text = fs::read_to_string(&args.tiered_corpus).with_context(|| {
        format!(
            "reading tiered corpus from {}",
            args.tiered_corpus.display()
        )
    })?;
    let tiered: ScoringReport = serde_json::from_str(&tiered_text).with_context(|| {
        format!(
            "parsing tiered corpus from {}",
            args.tiered_corpus.display()
        )
    })?;

    // Load candidate pool.
    let pool_map: HashMap<String, CandidateItem> = args
        .candidate_pool
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading pool from {}", p.display()))?;
            let pool: CandidatePool = serde_json::from_str(&text)
                .with_context(|| format!("parsing pool from {}", p.display()))?;
            let mut map = HashMap::new();
            for item in pool.items {
                map.insert(item.id.clone(), item.clone());
                map.insert(item.name.clone(), item);
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Load validated report for registration types.
    let validation_map: HashMap<String, Vec<String>> = args
        .validated
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading validated from {}", p.display()))?;
            let report: ValidationReport = serde_json::from_str(&text)
                .with_context(|| format!("parsing validated from {}", p.display()))?;
            let mut map = HashMap::new();
            for c in &report.candidates {
                if c.status == ValidationStatus::TrueExtension {
                    map.insert(c.canonical_id.clone(), c.evidence.registrations.clone());
                    map.insert(c.name.clone(), c.evidence.registrations.clone());
                }
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Load license report.
    let license_map: HashMap<String, String> = args
        .license_report
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading license from {}", p.display()))?;
            let report: ScreeningReport = serde_json::from_str(&text)
                .with_context(|| format!("parsing license from {}", p.display()))?;
            let mut map = HashMap::new();
            for v in report.verdicts {
                map.insert(v.canonical_id, v.license);
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Build inclusion entries.
    let mut tier0 = Vec::new();
    let mut tier1 = Vec::new();
    let mut tier2 = Vec::new();
    let mut exclusions = Vec::new();
    let mut category_coverage: HashMap<String, usize> = HashMap::new();
    let mut pinned_npm = 0_usize;
    let mut pinned_git = 0_usize;
    let mut pinned_url = 0_usize;
    let mut pinned_checksum = 0_usize;

    for item in &tiered.items {
        let pool_item = pool_map
            .get(&item.id)
            .or_else(|| item.name.as_deref().and_then(|name| pool_map.get(name)));

        let registrations = validation_map
            .get(&item.id)
            .or_else(|| item.name.as_ref().and_then(|n| validation_map.get(n)))
            .cloned()
            .unwrap_or_default();

        let category = classify_registrations(&registrations);
        let license = license_map.get(&item.id).cloned().unwrap_or_else(|| {
            pool_item.map_or_else(|| "UNKNOWN".to_string(), |p| p.license.clone())
        });

        let source_tier = item
            .source_tier
            .clone()
            .or_else(|| pool_item.map(|p| p.source_tier.clone()))
            .unwrap_or_else(|| "unknown".into());

        let (version_pin, sha256, artifact_path) =
            pool_item.map_or((VersionPin::Checksum, None, None), |p| {
                let pin = match &p.source {
                    CandidateSource::Npm {
                        package,
                        version,
                        url,
                    } => VersionPin::Npm {
                        package: package.clone(),
                        version: version.clone(),
                        registry_url: url.clone(),
                    },
                    CandidateSource::Git { repo, path } => VersionPin::Git {
                        repo: repo.clone(),
                        path: path.clone(),
                        commit: None,
                    },
                    CandidateSource::Url { url } => VersionPin::Url { url: url.clone() },
                };
                let sha = p.checksum.as_ref().map(|c| c.sha256.clone());
                let art = p.artifact_path.clone();
                (pin, sha, art)
            });

        match &version_pin {
            VersionPin::Npm { .. } => pinned_npm += 1,
            VersionPin::Git { .. } => pinned_git += 1,
            VersionPin::Url { .. } => pinned_url += 1,
            VersionPin::Checksum => pinned_checksum += 1,
        }

        let rationale = build_rationale(
            &item.tier,
            f64::from(item.score.final_total),
            &category,
            &source_tier,
        );

        let entry = InclusionEntry {
            id: item.id.clone(),
            name: Some(item.name.clone().unwrap_or_else(|| item.id.clone())),
            tier: Some(item.tier.clone()),
            score: Some(f64::from(item.score.final_total)),
            category: category.clone(),
            registrations,
            version_pin: Some(version_pin),
            sha256,
            artifact_path,
            license: Some(license),
            source_tier: Some(source_tier.clone()),
            rationale: Some(rationale),
            directory: None,
            provenance: None,
            capabilities: None,
            risk_level: None,
            inclusion_rationale: None,
        };

        let cat_key = format!("{category:?}");
        *category_coverage.entry(cat_key).or_insert(0) += 1;

        match item.tier.as_str() {
            "tier-0" => tier0.push(entry),
            "tier-1" => tier1.push(entry),
            "tier-2" => tier2.push(entry),
            _ => {
                // Top excluded items get exclusion notes.
                if item.score.final_total >= 30 {
                    let reason = if item.gates.passes {
                        format!("Score {}/100 below threshold (50)", item.score.final_total)
                    } else {
                        let mut gates = Vec::new();
                        if !item.gates.provenance_pinned {
                            gates.push("no provenance");
                        }
                        if !item.gates.license_ok {
                            gates.push("license unknown");
                        }
                        if !item.gates.deterministic {
                            gates.push("not deterministic");
                        }
                        if !item.gates.unmodified {
                            gates.push("requires modification");
                        }
                        format!("Gate failure: {}", gates.join(", "))
                    };
                    exclusions.push(ExclusionNote {
                        id: item.id.clone(),
                        score: f64::from(item.score.final_total),
                        reason,
                    });
                }
            }
        }
    }

    let total_included = tier0.len() + tier1.len() + tier2.len();
    let stats = InclusionStats {
        total_included,
        tier0_count: tier0.len(),
        tier1_count: tier1.len(),
        tier2_count: tier2.len(),
        excluded_count: exclusions.len(),
        pinned_npm,
        pinned_git,
        pinned_url,
        pinned_checksum_only: pinned_checksum,
    };

    let list = InclusionList {
        schema: "pi.ext.inclusion.v1".to_string(),
        generated_at: pi::extension_validation::chrono_now_iso(),
        task: Some(args.task_id),
        stats: Some(stats),
        tier0,
        tier1,
        tier2,
        exclusions,
        category_coverage,
        summary: None,
        tier1_review: vec![],
        coverage: None,
        exclusion_notes: vec![],
    };

    let json = serde_json::to_string_pretty(&list).context("serializing inclusion list")?;
    fs::write(&args.out, format!("{json}\n"))
        .with_context(|| format!("writing output to {}", args.out.display()))?;

    // Print summary.
    let stats_ref = list.stats.as_ref().expect("stats");
    eprintln!("=== Final Inclusion List ===");
    eprintln!("Total included:     {}", stats_ref.total_included);
    eprintln!("  Tier-0 (baseline):{}", stats_ref.tier0_count);
    eprintln!("  Tier-1 (must):    {}", stats_ref.tier1_count);
    eprintln!("  Tier-2 (stretch): {}", stats_ref.tier2_count);
    eprintln!("Exclusion notes:    {}", stats_ref.excluded_count);
    eprintln!();
    eprintln!("Version pins:");
    eprintln!("  npm:       {}", stats_ref.pinned_npm);
    eprintln!("  git:       {}", stats_ref.pinned_git);
    eprintln!("  url:       {}", stats_ref.pinned_url);
    eprintln!("  checksum:  {}", stats_ref.pinned_checksum_only);
    eprintln!();

    let mut cat_list: Vec<_> = list.category_coverage.iter().collect();
    cat_list.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("Category coverage:");
    for (cat, count) in &cat_list {
        eprintln!("  {cat:<20} {count}");
    }
    eprintln!("\nOutput written to: {}", args.out.display());

    Ok(())
}
