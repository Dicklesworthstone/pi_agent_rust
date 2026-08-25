#![forbid(unsafe_code)]
#![allow(
    clippy::suboptimal_flops,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::too_many_lines
)]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const NRUN_EVIDENCE_PROTOCOL_SCHEMA: &str = "pi.nrun.evidence_protocol.contract.v1";
pub const NRUN_BUDGET_EVALUATION_SCHEMA: &str = "pi.nrun.budget_evaluation.v1";
pub const MIN_REPETITIONS: usize = 10;
pub const BOOTSTRAP_RESAMPLES: usize = 1000;
pub const MAX_ALLOWED_NOISE_SCORE: u8 = 15;
pub const DEFAULT_SEED: u64 = 0x5EED_C0DE_1234_5678;

#[derive(Debug, Parser)]
#[command(name = "nrun_evidence_evaluator")]
#[command(about = "Evaluate multi-run evidence protocols with bootstrap confidence intervals")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Evaluate a multi-run measurement series file and generate budget evaluation artifact.
    Evaluate(EvaluateArgs),
    /// Verify an existing N-run budget evaluation artifact against contract requirements.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct EvaluateArgs {
    /// Input series JSON file containing measurement runs.
    #[arg(long, default_value = "docs/evidence/nrun-measurement-series.json")]
    input: PathBuf,
    /// Output path for the evaluated budget artifact.
    #[arg(long, default_value = "docs/evidence/nrun-budget-evaluation.json")]
    output: PathBuf,
    /// Random seed for deterministic bootstrap resampling.
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to the N-run budget evaluation artifact.
    #[arg(long, default_value = "docs/evidence/nrun-budget-evaluation.json")]
    input: PathBuf,
    /// Contract path for protocol rules.
    #[arg(
        long,
        default_value = "docs/contracts/nrun-evidence-protocol-contract.json"
    )]
    contract: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NRunSample {
    pub run_index: usize,
    pub correlation_id: String,
    pub timestamp: String,
    pub git_commit: String,
    pub raw_value: f64,
    pub weight: f64,
    pub noise_score: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetMeasurementSeries {
    pub budget_name: String,
    pub category: String,
    pub comparison: String,
    pub unit: String,
    pub threshold: f64,
    pub samples: Vec<NRunSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NRunSeriesInput {
    pub schema: String,
    pub generated_at: String,
    pub series: Vec<BudgetMeasurementSeries>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetEvaluationResult {
    pub budget_name: String,
    pub category: String,
    pub comparison: String,
    pub unit: String,
    pub threshold: f64,
    pub repetition_count: usize,
    pub n_eff: f64,
    pub weighted_mean: f64,
    pub ci_95_lower: f64,
    pub ci_95_upper: f64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NRunBudgetEvaluationArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub min_repetitions_enforced: usize,
    pub total_budgets: usize,
    pub passing_budgets: usize,
    pub failing_budgets: usize,
    pub overall_status: String,
    pub evaluations: Vec<BudgetEvaluationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub errors: Vec<String>,
}

/// Simple fast deterministic 64-bit Xorshift PRNG for reproducible bootstrap.
#[derive(Debug, Clone)]
pub struct DeterministicPrng {
    state: u64,
}

impl DeterministicPrng {
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x5EED_1234_5678_ABCD
        } else {
            seed
        };
        Self { state }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    pub fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % (bound as u64)) as usize
    }
}

pub fn calculate_weighted_mean(samples: &[NRunSample]) -> Result<f64> {
    if samples.is_empty() {
        bail!("cannot compute weighted mean of empty sample set");
    }
    let mut sum_weighted = 0.0;
    let mut sum_weights = 0.0;
    for sample in samples {
        sum_weighted += sample.raw_value * sample.weight;
        sum_weights += sample.weight;
    }
    if sum_weights <= 0.0 {
        bail!("sum of sample weights must be positive");
    }
    Ok(sum_weighted / sum_weights)
}

pub fn calculate_n_eff(samples: &[NRunSample]) -> Result<f64> {
    if samples.is_empty() {
        bail!("cannot compute n_eff of empty sample set");
    }
    let mut sum_w = 0.0;
    let mut sum_w_sq = 0.0;
    for sample in samples {
        sum_w += sample.weight;
        sum_w_sq += sample.weight * sample.weight;
    }
    if sum_w_sq <= 0.0 {
        bail!("sum of squared weights must be positive");
    }
    Ok((sum_w * sum_w) / sum_w_sq)
}

#[allow(
    clippy::suboptimal_flops,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn compute_bootstrap_ci95(
    samples: &[NRunSample],
    resamples: usize,
    seed: u64,
) -> Result<(f64, f64)> {
    let n = samples.len();
    if n == 0 {
        bail!("cannot bootstrap empty sample set");
    }

    let mut prng = DeterministicPrng::new(seed);
    let mut replicate_means = Vec::with_capacity(resamples);

    for _ in 0..resamples {
        let mut sum_weighted = 0.0;
        let mut sum_weights = 0.0;
        for _ in 0..n {
            let idx = prng.next_usize(n);
            if let Some(sample) = samples.get(idx) {
                sum_weighted += sample.raw_value * sample.weight;
                sum_weights += sample.weight;
            }
        }
        if sum_weights > 0.0 {
            replicate_means.push(sum_weighted / sum_weights);
        }
    }

    if replicate_means.is_empty() {
        bail!("no valid replicate means computed in bootstrap");
    }

    replicate_means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let lower_idx = ((replicate_means.len() as f64) * 0.025).floor() as usize;
    let upper_idx = ((replicate_means.len() as f64) * 0.975).floor() as usize;

    let lower = replicate_means
        .get(lower_idx)
        .copied()
        .or_else(|| replicate_means.first().copied())
        .unwrap_or(0.0);
    let upper = replicate_means
        .get(upper_idx)
        .copied()
        .or_else(|| replicate_means.last().copied())
        .unwrap_or(0.0);

    Ok((lower, upper))
}

pub fn evaluate_budget_series(
    series: &BudgetMeasurementSeries,
    min_repetitions: usize,
    seed: u64,
) -> Result<BudgetEvaluationResult> {
    if series.samples.len() < min_repetitions {
        bail!(
            "insufficient repetitions for {}: required {}, provided {}",
            series.budget_name,
            min_repetitions,
            series.samples.len()
        );
    }

    let mut duplicate_correlation = None;
    let mut noisy_score = None;

    let mut distinct_correlations = HashSet::with_capacity(series.samples.len());
    for sample in &series.samples {
        if !distinct_correlations.insert(sample.correlation_id.as_str()) {
            duplicate_correlation = Some(sample.correlation_id.as_str());
            break;
        }
        if sample.noise_score > MAX_ALLOWED_NOISE_SCORE {
            noisy_score = Some(sample.noise_score);
            break;
        }
    }

    if let Some(dup) = duplicate_correlation {
        bail!(
            "duplicate correlation_id found in series {}: {dup}",
            series.budget_name
        );
    }
    if let Some(score) = noisy_score {
        bail!(
            "sample noise_score {score} exceeds max {MAX_ALLOWED_NOISE_SCORE} in series {}",
            series.budget_name
        );
    }

    let repetition_count = series.samples.len();
    let weighted_mean = calculate_weighted_mean(&series.samples)?;
    let n_eff = calculate_n_eff(&series.samples)?;
    let (ci_95_lower, ci_95_upper) =
        compute_bootstrap_ci95(&series.samples, BOOTSTRAP_RESAMPLES, seed)?;

    let status = match series.comparison.as_str() {
        "maximum" => {
            if ci_95_upper <= series.threshold {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            }
        }
        "minimum" => {
            if ci_95_lower >= series.threshold {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            }
        }
        other => bail!("unrecognized budget comparison rule: {other}"),
    };

    Ok(BudgetEvaluationResult {
        budget_name: series.budget_name.clone(),
        category: series.category.clone(),
        comparison: series.comparison.clone(),
        unit: series.unit.clone(),
        threshold: series.threshold,
        repetition_count,
        n_eff,
        weighted_mean,
        ci_95_lower,
        ci_95_upper,
        status,
    })
}

enum NRunIssue<'a> {
    InsufficientRepetitions(&'a str, usize),
    InvalidCi(&'a str, f64, f64),
    StatusMismatch(&'a str, &'static str, &'a str),
    UnknownRule(&'a str, &'a str),
}

impl std::fmt::Display for NRunIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientRepetitions(name, cnt) => {
                write!(f, "budget {name} evaluated with {cnt} repetitions (< 10)")
            }
            Self::InvalidCi(name, lower, upper) => write!(
                f,
                "invalid CI bounds for {name}: lower {lower} > upper {upper}"
            ),
            Self::StatusMismatch(name, expected, recorded) => write!(
                f,
                "status mismatch for {name}: expected {expected}, recorded {recorded}"
            ),
            Self::UnknownRule(name, rule) => {
                write!(f, "unknown comparison rule {rule} in budget {name}")
            }
        }
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn verify_nrun_artifact(
    artifact: &NRunBudgetEvaluationArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != NRUN_BUDGET_EVALUATION_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            NRUN_BUDGET_EVALUATION_SCHEMA, artifact.schema
        ));
    }

    if artifact.min_repetitions_enforced < MIN_REPETITIONS {
        errors.push(format!(
            "min_repetitions_enforced {} is below protocol minimum {}",
            artifact.min_repetitions_enforced, MIN_REPETITIONS
        ));
    }

    if artifact.evaluations.len() != artifact.total_budgets {
        errors.push(format!(
            "total_budgets mismatch: declared {}, actual {}",
            artifact.total_budgets,
            artifact.evaluations.len()
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "referenced contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut passing = 0;
    let mut failing = 0;
    let mut issues = Vec::new();

    for eval in &artifact.evaluations {
        if eval.repetition_count < MIN_REPETITIONS {
            issues.push(NRunIssue::InsufficientRepetitions(
                &eval.budget_name,
                eval.repetition_count,
            ));
        }

        if eval.ci_95_lower > eval.ci_95_upper {
            issues.push(NRunIssue::InvalidCi(
                &eval.budget_name,
                eval.ci_95_lower,
                eval.ci_95_upper,
            ));
        }

        match eval.comparison.as_str() {
            "maximum" => {
                let expected_status = if eval.ci_95_upper <= eval.threshold {
                    "PASS"
                } else {
                    "FAIL"
                };
                if eval.status != expected_status {
                    issues.push(NRunIssue::StatusMismatch(
                        &eval.budget_name,
                        expected_status,
                        &eval.status,
                    ));
                }
            }
            "minimum" => {
                let expected_status = if eval.ci_95_lower >= eval.threshold {
                    "PASS"
                } else {
                    "FAIL"
                };
                if eval.status != expected_status {
                    issues.push(NRunIssue::StatusMismatch(
                        &eval.budget_name,
                        expected_status,
                        &eval.status,
                    ));
                }
            }
            other => {
                issues.push(NRunIssue::UnknownRule(&eval.budget_name, other));
            }
        }

        if eval.status == "PASS" {
            passing += 1;
        } else {
            failing += 1;
        }
    }

    errors.extend(issues.into_iter().map(|i| i.to_string()));

    if passing != artifact.passing_budgets {
        errors.push(format!(
            "passing_budgets mismatch: declared {}, actual {}",
            artifact.passing_budgets, passing
        ));
    }

    if failing != artifact.failing_budgets {
        errors.push(format!(
            "failing_budgets mismatch: declared {}, actual {}",
            artifact.failing_budgets, failing
        ));
    }

    let expected_overall = if failing == 0 { "PASS" } else { "FAIL" };
    if artifact.overall_status != expected_overall {
        errors.push(format!(
            "overall_status mismatch: expected {expected_overall}, recorded {}",
            artifact.overall_status
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.nrun.verification_report.v1".to_string(),
        status,
        evaluated_budgets: artifact.evaluations.len(),
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

            let mut evaluations = Vec::with_capacity(series_input.series.len());
            let mut passing = 0;
            let mut failing = 0;

            for (idx, series) in series_input.series.iter().enumerate() {
                let series_seed = args
                    .seed
                    .wrapping_add((idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let result = evaluate_budget_series(series, MIN_REPETITIONS, series_seed)?;
                if result.status == "PASS" {
                    passing += 1;
                } else {
                    failing += 1;
                }
                evaluations.push(result);
            }

            let overall_status = if failing == 0 {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            };

            let artifact = NRunBudgetEvaluationArtifact {
                schema: NRUN_BUDGET_EVALUATION_SCHEMA.to_string(),
                generated_at: Utc::now().to_rfc3339(),
                contract_path: "docs/contracts/nrun-evidence-protocol-contract.json".to_string(),
                min_repetitions_enforced: MIN_REPETITIONS,
                total_budgets: evaluations.len(),
                passing_budgets: passing,
                failing_budgets: failing,
                overall_status,
                evaluations,
            };

            let json_out = serde_json::to_string_pretty(&artifact)?;
            if let Some(parent) = args.output.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&args.output, json_out)?;
            println!(
                "Evaluated {} budgets (passing: {}, failing: {}) to {}",
                artifact.total_budgets,
                artifact.passing_budgets,
                artifact.failing_budgets,
                args.output.display()
            );
        }
        CommandMode::Verify(args) => {
            let artifact_text = fs::read_to_string(&args.input).with_context(|| {
                format!("failed to read artifact from {}", args.input.display())
            })?;
            let artifact: NRunBudgetEvaluationArtifact = serde_json::from_str(&artifact_text)?;
            let report = verify_nrun_artifact(&artifact, &args.contract);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.status != "pass" {
                bail!("N-run budget evaluation verification failed");
            }
        }
    }

    Ok(())
}
