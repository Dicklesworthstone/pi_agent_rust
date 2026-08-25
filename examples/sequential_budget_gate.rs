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
use std::fs;
use std::path::{Path, PathBuf};

pub const SEQUENTIAL_GATE_CONTRACT_SCHEMA: &str = "pi.sequential_gate.contract.v1";
pub const SEQUENTIAL_GATE_EVALUATION_SCHEMA: &str = "pi.sequential_gate.evaluation.v1";
pub const DEFAULT_ALPHA: f64 = 0.05;
pub const DEFAULT_BETA: f64 = 0.01;
pub const DEFAULT_MAX_STEPS: usize = 50;

#[derive(Debug, Parser)]
#[command(name = "sequential_budget_gate")]
#[command(about = "Anytime-valid sequential budget testing with e-processes and error control")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Evaluate a multi-run measurement series sequentially with early-stopping.
    Evaluate(EvaluateArgs),
    /// Verify an existing sequential budget evaluation artifact against contract rules.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct EvaluateArgs {
    /// Input series JSON file containing measurement samples.
    #[arg(long, default_value = "docs/evidence/nrun-measurement-series.json")]
    input: PathBuf,
    /// Output path for the sequential evaluation artifact.
    #[arg(
        long,
        default_value = "docs/evidence/sequential-budget-gate-evaluations.json"
    )]
    output: PathBuf,
    /// Type I error budget (alpha).
    #[arg(long, default_value_t = DEFAULT_ALPHA)]
    alpha: f64,
    /// Type II error threshold (beta) for early rejection.
    #[arg(long, default_value_t = DEFAULT_BETA)]
    beta: f64,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to the sequential budget evaluation artifact.
    #[arg(
        long,
        default_value = "docs/evidence/sequential-budget-gate-evaluations.json"
    )]
    input: PathBuf,
    /// Contract path for protocol rules.
    #[arg(
        long,
        default_value = "docs/contracts/sequential-budget-gate-contract.json"
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
pub struct SequentialStepTrace {
    pub step: usize,
    pub sample_value: f64,
    pub weight: f64,
    pub log_likelihood_ratio: f64,
    pub accumulated_log_e: f64,
    pub e_value: f64,
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SequentialBudgetResult {
    pub budget_name: String,
    pub category: String,
    pub comparison: String,
    pub unit: String,
    pub threshold: f64,
    pub total_samples_available: usize,
    pub stopping_step: usize,
    pub final_e_value: f64,
    pub decision: String,
    pub final_verdict: String,
    pub trajectory: Vec<SequentialStepTrace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequentialBudgetEvaluationArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub alpha: f64,
    pub beta: f64,
    pub e_threshold_pass: f64,
    pub e_threshold_reject: f64,
    pub total_budgets: usize,
    pub passing_budgets: usize,
    pub failing_budgets: usize,
    pub overall_verdict: String,
    pub evaluations: Vec<SequentialBudgetResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub errors: Vec<String>,
}

pub fn compute_step_log_lr(
    sample_value: f64,
    weight: f64,
    threshold: f64,
    comparison: &str,
) -> f64 {
    let rel_margin = match comparison {
        "maximum" => {
            if threshold > 0.0 {
                (threshold - sample_value) / threshold
            } else {
                threshold - sample_value
            }
        }
        "minimum" => {
            if threshold > 0.0 {
                (sample_value - threshold) / threshold
            } else {
                sample_value - threshold
            }
        }
        _ => 0.0,
    };

    let mu0 = 0.0;
    let mu1 = 0.5;
    // Standardize by baseline relative std-dev sigma_0 = 0.10 (10% relative variance)
    let sigma0 = 0.10;
    let z = (rel_margin / sigma0).clamp(-10.0, 10.0);
    let delta = (mu1 - mu0) * z - 0.5 * (mu1 * mu1 - mu0 * mu0);
    delta * weight.clamp(0.1, 2.0)
}

pub fn run_sequential_test(
    series: &BudgetMeasurementSeries,
    alpha: f64,
    beta: f64,
    max_steps: usize,
) -> Result<SequentialBudgetResult> {
    if series.samples.is_empty() {
        bail!(
            "cannot run sequential test on empty series {}",
            series.budget_name
        );
    }

    let e_pass_threshold = 1.0 / alpha.clamp(0.0001, 0.5);
    let e_reject_threshold = beta.clamp(0.0001, 0.5);

    let mut log_e = 0.0f64;
    let mut intermediate_traces = Vec::with_capacity(series.samples.len());
    let mut stopping_step = series.samples.len();
    let mut final_decision_static = "CONTINUE";
    let mut final_verdict_static = "INCONCLUSIVE";

    for (step_idx, sample) in series.samples.iter().enumerate() {
        let step = step_idx + 1;
        let step_log_lr = compute_step_log_lr(
            sample.raw_value,
            sample.weight,
            series.threshold,
            &series.comparison,
        );

        log_e = (log_e + step_log_lr).clamp(-100.0, 100.0);
        let e_val = log_e.exp();

        let step_decision = if e_val >= e_pass_threshold {
            "EARLY_PASS"
        } else if e_val <= e_reject_threshold {
            "EARLY_REJECT"
        } else if step >= max_steps || step == series.samples.len() {
            if e_val >= 1.0 {
                "MAX_STEPS_PASS"
            } else {
                "MAX_STEPS_REJECT"
            }
        } else {
            "CONTINUE"
        };

        intermediate_traces.push((
            step,
            sample.raw_value,
            sample.weight,
            step_log_lr,
            log_e,
            e_val,
            step_decision,
        ));

        if step_decision != "CONTINUE" {
            stopping_step = step;
            final_decision_static = step_decision;
            final_verdict_static = match step_decision {
                "EARLY_PASS" | "MAX_STEPS_PASS" => "PASS",
                "EARLY_REJECT" | "MAX_STEPS_REJECT" => "FAIL",
                _ => "INCONCLUSIVE",
            };
            break;
        }
    }

    let trajectory = intermediate_traces
        .into_iter()
        .map(
            |(
                step,
                sample_value,
                weight,
                log_likelihood_ratio,
                accumulated_log_e,
                e_value,
                decision,
            )| {
                SequentialStepTrace {
                    step,
                    sample_value,
                    weight,
                    log_likelihood_ratio,
                    accumulated_log_e,
                    e_value,
                    decision: decision.to_string(),
                }
            },
        )
        .collect();

    Ok(SequentialBudgetResult {
        budget_name: series.budget_name.clone(),
        category: series.category.clone(),
        comparison: series.comparison.clone(),
        unit: series.unit.clone(),
        threshold: series.threshold,
        total_samples_available: series.samples.len(),
        stopping_step,
        final_e_value: log_e.exp(),
        decision: final_decision_static.to_string(),
        final_verdict: final_verdict_static.to_string(),
        trajectory,
    })
}

enum SeqIssue<'a> {
    InvalidEValue(&'a str, f64),
    VerdictMismatch(&'a str, &'static str, &'a str),
    TrajectoryEmpty(&'a str),
}

impl std::fmt::Display for SeqIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEValue(name, val) => write!(f, "invalid e-value {val} in budget {name}"),
            Self::VerdictMismatch(name, expected, recorded) => write!(
                f,
                "verdict mismatch for {name}: expected {expected}, recorded {recorded}"
            ),
            Self::TrajectoryEmpty(name) => write!(f, "empty trajectory for budget {name}"),
        }
    }
}

pub fn verify_sequential_artifact(
    artifact: &SequentialBudgetEvaluationArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != SEQUENTIAL_GATE_EVALUATION_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            SEQUENTIAL_GATE_EVALUATION_SCHEMA, artifact.schema
        ));
    }

    if artifact.alpha <= 0.0 || artifact.alpha >= 1.0 {
        errors.push(format!("invalid alpha: {}", artifact.alpha));
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
            "contract path does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut passing = 0;
    let mut failing = 0;
    let mut issues = Vec::new();

    for eval in &artifact.evaluations {
        if eval.trajectory.is_empty() {
            issues.push(SeqIssue::TrajectoryEmpty(&eval.budget_name));
        }

        if eval.final_e_value.is_nan() || eval.final_e_value < 0.0 {
            issues.push(SeqIssue::InvalidEValue(
                &eval.budget_name,
                eval.final_e_value,
            ));
        }

        let expected_verdict = match eval.decision.as_str() {
            "EARLY_PASS" | "MAX_STEPS_PASS" => "PASS",
            "EARLY_REJECT" | "MAX_STEPS_REJECT" => "FAIL",
            _ => "INCONCLUSIVE",
        };

        if eval.final_verdict != expected_verdict {
            issues.push(SeqIssue::VerdictMismatch(
                &eval.budget_name,
                expected_verdict,
                &eval.final_verdict,
            ));
        }

        if eval.final_verdict == "PASS" {
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
    if artifact.overall_verdict != expected_overall {
        errors.push(format!(
            "overall_verdict mismatch: expected {expected_overall}, recorded {}",
            artifact.overall_verdict
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.sequential_gate.verification_report.v1".to_string(),
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

            for series in &series_input.series {
                let result = run_sequential_test(series, args.alpha, args.beta, DEFAULT_MAX_STEPS)?;
                if result.final_verdict == "PASS" {
                    passing += 1;
                } else {
                    failing += 1;
                }
                evaluations.push(result);
            }

            let overall_verdict = if failing == 0 {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            };

            let artifact = SequentialBudgetEvaluationArtifact {
                schema: SEQUENTIAL_GATE_EVALUATION_SCHEMA.to_string(),
                generated_at: Utc::now().to_rfc3339(),
                contract_path: "docs/contracts/sequential-budget-gate-contract.json".to_string(),
                alpha: args.alpha,
                beta: args.beta,
                e_threshold_pass: 1.0 / args.alpha,
                e_threshold_reject: args.beta,
                total_budgets: evaluations.len(),
                passing_budgets: passing,
                failing_budgets: failing,
                overall_verdict,
                evaluations,
            };

            let json_out = serde_json::to_string_pretty(&artifact)?;
            if let Some(parent) = args.output.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&args.output, json_out)?;
            println!(
                "Evaluated {} sequential budget gates (passing: {}, failing: {}) to {}",
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
            let artifact: SequentialBudgetEvaluationArtifact =
                serde_json::from_str(&artifact_text)?;
            let report = verify_sequential_artifact(&artifact, &args.contract);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.status != "pass" {
                bail!("Sequential budget evaluation verification failed");
            }
        }
    }

    Ok(())
}
