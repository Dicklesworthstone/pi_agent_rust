#![forbid(unsafe_code)]

//! CLI binary: Build tiered corpus from validated + scored + license-screened candidates.
//!
//! Merges signals from validated-dedup, candidate pool, and license report
//! to produce fully enriched `CandidateInput` records, scores them, and
//! outputs a tiered corpus selection.
//!
//! ```text
//! cargo run --bin ext_tiered_corpus -- \
//!   --validated docs/extension-validated-dedup.json \
//!   --candidate-pool docs/extension-candidate-pool.json \
//!   --license-report docs/extension-license-report.json \
//!   --out docs/extension-tiered-corpus.json \
//!   --summary-out docs/extension-tiered-summary.json
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use pi::extension_license::{ScreeningReport, VerdictStatus};
use pi::extension_popularity::{CandidateItem, CandidatePool};
use pi::extension_scoring::{
    CandidateInput, CompatStatus, Compatibility, Gates, LicenseInfo, MarketplaceSignals, Recency,
    Redistribution, RiskInfo, Signals, Tags, score_candidates,
};
use pi::extension_validation::{ValidationReport, ValidationStatus};

#[derive(Debug, Parser)]
#[command(name = "ext_tiered_corpus")]
#[command(about = "Build tiered extension corpus from merged research signals")]
struct Args {
    /// Path to validated-dedup JSON (registration + classification data).
    #[arg(long)]
    validated: PathBuf,

    /// Path to candidate pool JSON (popularity + artifact data).
    #[arg(long)]
    candidate_pool: Option<PathBuf>,

    /// Path to license screening report JSON.
    #[arg(long)]
    license_report: Option<PathBuf>,

    /// Output path for scored + tiered corpus JSON.
    #[arg(long)]
    out: PathBuf,

    /// Output path for summary JSON.
    #[arg(long)]
    summary_out: Option<PathBuf>,

    /// Output path for JSONL decision log.
    #[arg(long)]
    log_out: Option<PathBuf>,

    /// Reference date for scoring (RFC3339). Defaults to now.
    #[arg(long)]
    as_of: Option<String>,

    /// Top N for summary sections.
    #[arg(long, default_value_t = 20)]
    top_n: usize,

    /// Task ID for provenance tracking.
    #[arg(long, default_value = "bd-34io")]
    task_id: String,
}

/// Merged data for a single candidate from all sources.
struct MergedCandidate {
    canonical_id: String,
    name: String,
    source_tier: Option<String>,
    registrations: Vec<String>,
    repository_url: Option<String>,
    npm_package: Option<String>,
    pool_item: Option<CandidateItem>,
    license_verdict: Option<VerdictStatus>,
    license_spdx: Option<String>,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();

    // Load validated report.
    let validated_text = fs::read_to_string(&args.validated)
        .with_context(|| format!("reading validated report from {}", args.validated.display()))?;
    let validated: ValidationReport = serde_json::from_str(&validated_text)
        .with_context(|| format!("parsing validated report from {}", args.validated.display()))?;

    // Load candidate pool (popularity + artifact data).
    let pool_map: HashMap<String, CandidateItem> = args
        .candidate_pool
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading candidate pool from {}", p.display()))?;
            let pool: CandidatePool = serde_json::from_str(&text)
                .with_context(|| format!("parsing candidate pool from {}", p.display()))?;
            let mut map = HashMap::new();
            for item in pool.items {
                map.insert(item.id.clone(), item.clone());
                map.insert(item.name.clone(), item);
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Load license report.
    let license_map: HashMap<String, (VerdictStatus, String)> = args
        .license_report
        .as_ref()
        .map(|p| {
            let text = fs::read_to_string(p)
                .with_context(|| format!("reading license report from {}", p.display()))?;
            let report: ScreeningReport = serde_json::from_str(&text)
                .with_context(|| format!("parsing license report from {}", p.display()))?;
            let mut map = HashMap::new();
            for v in report.verdicts {
                map.insert(v.canonical_id, (v.verdict, v.license));
            }
            Ok::<_, anyhow::Error>(map)
        })
        .transpose()?
        .unwrap_or_default();

    // Merge all sources into enriched candidates.
    let merged: Vec<MergedCandidate> = validated
        .candidates
        .iter()
        .filter(|c| c.status == ValidationStatus::TrueExtension)
        .map(|c| {
            let pool_item = pool_map
                .get(&c.canonical_id)
                .or_else(|| pool_map.get(&c.name))
                .cloned();

            let (license_verdict, license_spdx) = license_map
                .get(&c.canonical_id)
                .map(|(v, s)| (Some(*v), Some(s.clone())))
                .unwrap_or((None, None));

            MergedCandidate {
                canonical_id: c.canonical_id.clone(),
                name: c.name.clone(),
                source_tier: c.source_tier.clone(),
                registrations: c.evidence.registrations.clone(),
                repository_url: c.repository_url.clone(),
                npm_package: c.npm_package.clone(),
                pool_item,
                license_verdict,
                license_spdx,
            }
        })
        .collect();

    eprintln!("Merged {} true extension candidates", merged.len());

    // Convert to CandidateInput for scoring.
    let inputs: Vec<CandidateInput> = merged.iter().map(build_candidate_input).collect();

    let as_of = args
        .as_of
        .as_ref()
        .map(|s| DateTime::parse_from_rfc3339(s).context("parse as_of"))
        .transpose()?
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let report = score_candidates(&inputs, as_of, as_of, args.top_n);

    // Write main output.
    let json = serde_json::to_string_pretty(&report).context("serializing scoring report")?;
    let json = format!("{json}\n");
    fs::write(&args.out, &json)
        .with_context(|| format!("writing output to {}", args.out.display()))?;

    // Write summary.
    if let Some(summary_path) = &args.summary_out {
        let summary_json =
            serde_json::to_string_pretty(&report.summary).context("serialize summary")?;
        fs::write(summary_path, format!("{summary_json}\n"))
            .with_context(|| format!("write {}", summary_path.display()))?;
    }

    // Write JSONL decision log.
    if let Some(log_path) = &args.log_out {
        let mut log_file = fs::File::create(log_path)
            .with_context(|| format!("creating log file {}", log_path.display()))?;
        for item in &report.items {
            let line = serde_json::to_string(item).context("serializing log entry")?;
            writeln!(log_file, "{line}").context("writing log entry")?;
        }
    }

    // Print tier summary.
    let tier0 = report.items.iter().filter(|i| i.tier == "tier-0").count();
    let tier1 = report.items.iter().filter(|i| i.tier == "tier-1").count();
    let tier2 = report.items.iter().filter(|i| i.tier == "tier-2").count();
    let excluded = report
        .items
        .iter()
        .filter(|i| i.tier == "excluded")
        .count();

    eprintln!("=== Tiered Corpus Selection ({}) ===", args.task_id);
    eprintln!("Total scored:   {}", report.items.len());
    eprintln!("  Tier-0:       {tier0} (official baseline)");
    eprintln!("  Tier-1:       {tier1} (must-pass, score ≥ 70)");
    eprintln!("  Tier-2:       {tier2} (stretch, score 50-69)");
    eprintln!("  Excluded:     {excluded} (score < 50 or gate fail)");
    eprintln!();

    // Type coverage summary.
    let mut type_counts: HashMap<String, usize> = HashMap::new();
    for m in &merged {
        if m.registrations.is_empty() {
            *type_counts.entry("(no registrations)".to_string()).or_insert(0) += 1;
        }
        for reg in &m.registrations {
            *type_counts.entry(reg.clone()).or_insert(0) += 1;
        }
    }
    let mut type_list: Vec<_> = type_counts.iter().collect();
    type_list.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("Extension type coverage:");
    for (ext_type, count) in &type_list {
        eprintln!("  {ext_type:<25} {count}");
    }

    eprintln!("\nOutput written to: {}", args.out.display());

    Ok(())
}

/// Build a `CandidateInput` from merged signals.
fn build_candidate_input(m: &MergedCandidate) -> CandidateInput {
    let pool = m.pool_item.as_ref();

    // Signals from pool popularity data.
    let signals = pool.map_or_else(Signals::default, |item| {
        let is_official = m
            .source_tier
            .as_deref()
            .is_some_and(|t| t == "official-pi-mono");
        Signals {
            official_listing: Some(is_official),
            pi_mono_example: Some(is_official),
            badlogic_gist: m
                .repository_url
                .as_deref()
                .map(|url| url.contains("gist.github.com") && url.contains("badlogic"))
                .or(Some(false)),
            github_stars: item.popularity.github_stars,
            github_forks: item.popularity.github_forks,
            npm_downloads_month: item.popularity.npm_downloads_monthly,
            references: item
                .popularity
                .mentions_sources
                .clone()
                .unwrap_or_default(),
            marketplace: Some(MarketplaceSignals {
                rank: item.popularity.marketplace_rank,
                installs_month: item.popularity.marketplace_installs_monthly,
                featured: item.popularity.marketplace_featured,
            }),
        }
    });

    // Tags: infer from registrations and source tier.
    let interaction = registrations_to_interactions(&m.registrations);
    let capabilities = registrations_to_capabilities(&m.registrations);
    let runtime = infer_runtime(m);

    let tags = Tags {
        runtime,
        interaction,
        capabilities,
    };

    // Recency from pool data.
    let recency = pool.map_or_else(Recency::default, |item| Recency {
        updated_at: item
            .popularity
            .github_last_commit
            .clone()
            .or_else(|| item.popularity.npm_last_publish.clone())
            .or_else(|| item.retrieved.clone()),
    });

    // Compatibility: vendored items with artifacts are unmodified-compatible.
    let compat_status = pool.map_or(Some(CompatStatus::RequiresShims), |item| {
        match item.status.as_str() {
            "vendored" => Some(CompatStatus::Unmodified),
            "unvendored" | "excluded" => Some(CompatStatus::Blocked),
            _ => Some(CompatStatus::RequiresShims),
        }
    });
    let compat = Compatibility {
        status: compat_status,
        ..Compatibility::default()
    };

    // License from screening report or pool.
    let spdx = m
        .license_spdx
        .clone()
        .or_else(|| pool.map(|item| item.license.clone()));
    let redistribution = spdx
        .as_deref()
        .map(infer_redistribution)
        .unwrap_or(Redistribution::Unknown);
    let license_ok = matches!(
        m.license_verdict,
        Some(VerdictStatus::Pass) | Some(VerdictStatus::PassWithWarnings)
    );

    let license = LicenseInfo {
        spdx,
        redistribution: Some(redistribution),
        notes: None,
    };

    // Gates.
    let provenance_pinned = pool.map(|item| item.checksum.is_some());
    let deterministic = pool.map(|item| item.status != "unvendored");
    let gates = Gates {
        provenance_pinned,
        deterministic,
    };

    // Risk: default (low).
    let risk = RiskInfo::default();

    CandidateInput {
        id: m.canonical_id.clone(),
        name: Some(m.name.clone()),
        source_tier: m.source_tier.clone(),
        signals,
        tags,
        recency,
        compat,
        license,
        gates,
        risk,
        manual_override: if !license_ok && m.license_verdict.is_some() {
            // If license fails, no manual override — the gate check handles it.
            None
        } else {
            None
        },
    }
}

/// Map registration types to interaction tags for coverage scoring.
fn registrations_to_interactions(registrations: &[String]) -> Vec<String> {
    let mut interactions = Vec::new();
    for reg in registrations {
        let tag = match reg.as_str() {
            "registerProvider" => "provider",
            "registerTool" => "tool_only",
            "registerCommand" | "registerSlashCommand" => "slash_command",
            "registerEvent" | "registerEventHook" => "event_hook",
            "registerMessageRenderer" => "ui_integration",
            "registerFlag" => "slash_command",
            "registerShortcut" => "slash_command",
            _ => continue,
        };
        if !interactions.contains(&tag.to_string()) {
            interactions.push(tag.to_string());
        }
    }
    if interactions.is_empty() {
        // Default: treat as tool extension if no specific registrations.
        interactions.push("tool_only".to_string());
    }
    interactions
}

/// Map registration types to capability tags.
fn registrations_to_capabilities(registrations: &[String]) -> Vec<String> {
    let mut caps = Vec::new();
    for reg in registrations {
        let tag = match reg.as_str() {
            "registerTool" => "exec",
            "registerProvider" => "http",
            "registerMessageRenderer" => "ui",
            "registerCommand" | "registerSlashCommand" => "session",
            _ => continue,
        };
        if !caps.contains(&tag.to_string()) {
            caps.push(tag.to_string());
        }
    }
    caps
}

/// Infer runtime tier from source data.
fn infer_runtime(m: &MergedCandidate) -> Option<String> {
    // npm packages with deps → pkg-with-deps.
    if m.npm_package.is_some() {
        return Some("pkg-with-deps".to_string());
    }
    // Provider extensions are often more complex.
    if m.registrations.iter().any(|r| r == "registerProvider") {
        return Some("provider-ext".to_string());
    }
    // Default: legacy single-file.
    Some("legacy-js".to_string())
}

/// Infer `Redistribution` from SPDX string.
fn infer_redistribution(spdx: &str) -> Redistribution {
    let up = spdx.trim().to_ascii_uppercase();
    if up.is_empty() || matches!(up.as_str(), "UNKNOWN" | "UNLICENSED") {
        return Redistribution::Unknown;
    }
    if up.contains("GPL") || up.contains("AGPL") {
        return Redistribution::Restricted;
    }
    Redistribution::Ok
}
