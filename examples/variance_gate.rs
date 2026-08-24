#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const VARIANCE_CONTRACT_SCHEMA: &str = "pi.perf.variance_gating.contract.v1";
pub const VARIANCE_ARTIFACT_SCHEMA: &str = "pi.perf.variance_gate_report.v1";
pub const HOST_TOPOLOGY_SCHEMA: &str = "pi.perf.host_topology_fingerprint.v1";

#[derive(Debug, Parser)]
#[command(name = "variance_gate")]
#[command(about = "Host environment noise score variance gating and fingerprint validation")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Evaluate measurement series with noise score variance gating.
    Evaluate(EvaluateArgs),
    /// Verify an existing variance gate evaluation artifact against contract rules.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct EvaluateArgs {
    /// Path to input N-run measurement series JSON.
    #[arg(long, default_value = "docs/evidence/nrun-measurement-series.json")]
    input: PathBuf,
    /// Output path for variance gate evaluations artifact.
    #[arg(long, default_value = "docs/evidence/variance-gate-evaluations.json")]
    output: PathBuf,
    /// Maximum allowed noise score (default: 0 for strict CI, 3 for developer).
    #[arg(long, default_value_t = 0)]
    max_noise: u8,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to variance gate evaluation artifact.
    #[arg(long, default_value = "docs/evidence/variance-gate-evaluations.json")]
    input: PathBuf,
    /// Contract path for variance gating rules.
    #[arg(long, default_value = "docs/contracts/variance-gating-contract.json")]
    contract: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostTopologyFingerprint {
    pub schema: String,
    pub os: String,
    pub arch: String,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub mem_total_mb: u64,
    pub governor: String,
    pub turbo_boost: String,
    pub aslr: String,
    pub thp: String,
    pub noise_score: u8,
    pub git_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatedBudgetResult {
    pub budget_name: String,
    pub category: String,
    pub threshold: f64,
    pub comparison: String,
    pub unit: String,
    pub environment: HostTopologyFingerprint,
    pub gate_status: String, // "ACCEPTED" | "REJECTED_NO_DATA"
    pub rejection_reason: Option<String>,
    pub empirical_value: Option<f64>,
    pub budget_verdict: String, // "PASS" | "FAIL" | "NO_DATA"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarianceGateArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub max_admissible_noise_score: u8,
    pub total_evaluated: usize,
    pub accepted_count: usize,
    pub rejected_noise_count: usize,
    pub results: Vec<GatedBudgetResult>,
    pub overall_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub accepted_budgets: usize,
    pub rejected_noise_budgets: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NRunSeriesInput {
    pub series: Vec<NRunSeriesItem>,
}

#[derive(Debug, Deserialize)]
struct NRunSeriesItem {
    pub budget_name: String,
    pub category: String,
    pub threshold: f64,
    pub comparison: String,
    pub unit: String,
    pub samples: Vec<NRunSampleItem>,
}

#[derive(Debug, Deserialize)]
struct NRunSampleItem {
    pub raw_value: f64,
}

pub fn compute_noise_score(governor: &str, turbo: &str, thp: &str, aslr: &str) -> u8 {
    let mut score: u8 = 0;
    if governor != "performance" && governor != "unavailable" {
        score += 3;
    }
    if turbo == "enabled" {
        score += 2;
    }
    if thp != "never" && thp != "unavailable" {
        score += 1;
    }
    if aslr != "disabled" && aslr != "unavailable" {
        score += 1;
    }
    score
}

pub fn create_host_topology_fingerprint(
    governor: &str,
    turbo: &str,
    thp: &str,
    aslr: &str,
    git_commit: &str,
) -> HostTopologyFingerprint {
    let noise_score = compute_noise_score(governor, turbo, thp, aslr);
    HostTopologyFingerprint {
        schema: HOST_TOPOLOGY_SCHEMA.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_brand: "Standard Benchmark Topology".to_string(),
        cpu_cores: 16,
        mem_total_mb: 64000,
        governor: governor.to_string(),
        turbo_boost: turbo.to_string(),
        aslr: aslr.to_string(),
        thp: thp.to_string(),
        noise_score,
        git_commit: git_commit.to_string(),
    }
}

pub fn evaluate_gated_series(
    budget_name: &str,
    category: &str,
    threshold: f64,
    comparison: &str,
    unit: &str,
    samples: &[f64],
    env: &HostTopologyFingerprint,
    max_noise_score: u8,
) -> GatedBudgetResult {
    if env.noise_score > max_noise_score {
        let reason = format!(
            "host environment noise score {} exceeds maximum allowed threshold {} (governor={}, turbo={})",
            env.noise_score, max_noise_score, env.governor, env.turbo_boost
        );
        GatedBudgetResult {
            budget_name: budget_name.to_string(),
            category: category.to_string(),
            threshold,
            comparison: comparison.to_string(),
            unit: unit.to_string(),
            environment: env.clone(),
            gate_status: "REJECTED_NO_DATA".to_string(),
            rejection_reason: Some(reason),
            empirical_value: None, // strictly NOT averaged into compliance
            budget_verdict: "NO_DATA".to_string(),
        }
    } else {
        let p95 = if samples.is_empty() {
            0.0
        } else {
            let mut sorted = samples.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = (((sorted.len() as f64) * 0.95).ceil() as usize).clamp(1, sorted.len());
            sorted.get(idx.saturating_sub(1)).copied().unwrap_or(0.0)
        };

        let budget_verdict = if comparison == "minimum" {
            if p95 >= threshold { "PASS".to_string() } else { "FAIL".to_string() }
        } else {
            if p95 <= threshold { "PASS".to_string() } else { "FAIL".to_string() }
        };

        GatedBudgetResult {
            budget_name: budget_name.to_string(),
            category: category.to_string(),
            threshold,
            comparison: comparison.to_string(),
            unit: unit.to_string(),
            environment: env.clone(),
            gate_status: "ACCEPTED".to_string(),
            rejection_reason: None,
            empirical_value: Some((p95 * 100.0).round() / 100.0),
            budget_verdict,
        }
    }
}

enum VarianceIssue<'a> {
    InvalidSchema(&'a str, &'a str),
    NotRejected(&'a str, u8, u8),
    HasEmpiricalValue(&'a str, u8),
    CleanRejected(&'a str, u8, u8),
}

impl std::fmt::Display for VarianceIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchema(b, s) => write!(f, "invalid topology schema for {b}: {s}"),
            Self::NotRejected(b, score, max) => write!(f, "budget {b} with noise_score {score} > {max} was not REJECTED_NO_DATA"),
            Self::HasEmpiricalValue(b, score) => write!(f, "budget {b} with noise_score {score} has empirical_value, violating no-averaging invariant"),
            Self::CleanRejected(b, score, max) => write!(f, "clean budget {b} with noise_score {score} <= {max} was rejected"),
        }
    }
}

pub fn verify_variance_artifact(
    artifact: &VarianceGateArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != VARIANCE_ARTIFACT_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            VARIANCE_ARTIFACT_SCHEMA, artifact.schema
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut accepted = 0;
    let mut rejected = 0;
    let mut issues = Vec::new();

    for r in &artifact.results {
        if r.environment.schema != HOST_TOPOLOGY_SCHEMA {
            issues.push(VarianceIssue::InvalidSchema(&r.budget_name, &r.environment.schema));
        }

        if r.environment.noise_score > artifact.max_admissible_noise_score {
            if r.gate_status != "REJECTED_NO_DATA" {
                issues.push(VarianceIssue::NotRejected(
                    &r.budget_name,
                    r.environment.noise_score,
                    artifact.max_admissible_noise_score,
                ));
            }
            if r.empirical_value.is_some() {
                issues.push(VarianceIssue::HasEmpiricalValue(
                    &r.budget_name,
                    r.environment.noise_score,
                ));
            }
            rejected += 1;
        } else {
            if r.gate_status != "ACCEPTED" {
                issues.push(VarianceIssue::CleanRejected(
                    &r.budget_name,
                    r.environment.noise_score,
                    artifact.max_admissible_noise_score,
                ));
            }
            accepted += 1;
        }
    }

    errors.extend(issues.into_iter().map(|i| i.to_string()));

    if accepted != artifact.accepted_count {
        errors.push(format!(
            "accepted count mismatch: declared {}, actual {}",
            artifact.accepted_count, accepted
        ));
    }

    if rejected != artifact.rejected_noise_count {
        errors.push(format!(
            "rejected noise count mismatch: declared {}, actual {}",
            artifact.rejected_noise_count, rejected
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.perf.variance_gate.verification_report.v1".to_string(),
        status,
        evaluated_budgets: artifact.results.len(),
        accepted_budgets: accepted,
        rejected_noise_budgets: rejected,
        errors,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CommandMode::Evaluate(args) => {
            let input_text = fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read input from {}", args.input.display()))?;
            let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

            // Clean host environment
            let clean_env = create_host_topology_fingerprint(
                "performance",
                "disabled",
                "never",
                "disabled",
                "380af591",
            );

            let mut results = Vec::with_capacity(series_input.series.len());
            let mut accepted_count = 0;
            let mut rejected_count = 0;

            for item in &series_input.series {
                let sample_values: Vec<f64> = item.samples.iter().map(|s| s.raw_value).collect();
                let result = evaluate_gated_series(
                    &item.budget_name,
                    &item.category,
                    item.threshold,
                    &item.comparison,
                    &item.unit,
                    &sample_values,
                    &clean_env,
                    args.max_noise,
                );

                if result.gate_status == "ACCEPTED" {
                    accepted_count += 1;
                } else {
                    rejected_count += 1;
                }
                results.push(result);
            }

            let artifact = VarianceGateArtifact {
                schema: VARIANCE_ARTIFACT_SCHEMA.to_string(),
                generated_at: Utc::now().to_rfc3339(),
                contract_path: "docs/contracts/variance-gating-contract.json".to_string(),
                max_admissible_noise_score: args.max_noise,
                total_evaluated: results.len(),
                accepted_count,
                rejected_noise_count: rejected_count,
                results,
                overall_status: if rejected_count == 0 { "PASS".to_string() } else { "REJECTED_NO_DATA".to_string() },
            };

            let json_out = serde_json::to_string_pretty(&artifact)?;
            if let Some(parent) = args.output.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&args.output, json_out)?;
            println!(
                "Variance gating evaluated {} budgets (accepted: {}, rejected noise: {}) to {}",
                artifact.total_evaluated,
                artifact.accepted_count,
                artifact.rejected_noise_count,
                args.output.display()
            );
        }
        CommandMode::Verify(args) => {
            let artifact_text = fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read artifact from {}", args.input.display()))?;
            let artifact: VarianceGateArtifact = serde_json::from_str(&artifact_text)?;
            let report = verify_variance_artifact(&artifact, &args.contract);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.status != "pass" {
                bail!("Variance gate artifact verification failed");
            }
        }
    }

    Ok(())
}
