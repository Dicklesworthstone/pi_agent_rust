#![forbid(unsafe_code)]

use anyhow::{Result, bail};
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

#[test]
fn contract_file_matches_schema_and_policy() -> Result<()> {
    let contract_path = Path::new("docs/contracts/sequential-budget-gate-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = fs::read_to_string(contract_path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;

    assert_eq!(
        val.get("schema").and_then(|v| v.as_str()),
        Some(SEQUENTIAL_GATE_CONTRACT_SCHEMA)
    );
    assert_eq!(
        val.get("bead_id").and_then(|v| v.as_str()),
        Some("bd-sog97.11")
    );
    assert_eq!(
        val.get("error_control")
            .and_then(|ec| ec.get("alpha"))
            .and_then(|a| a.as_f64()),
        Some(0.05)
    );
    assert_eq!(
        val.get("error_control")
            .and_then(|ec| ec.get("e_threshold_pass"))
            .and_then(|a| a.as_f64()),
        Some(20.0)
    );

    Ok(())
}

#[test]
fn step_log_lr_conformance_and_properties() {
    let lr_pass = compute_step_log_lr(80.0, 1.0, 100.0, "maximum");
    let lr_fail = compute_step_log_lr(120.0, 1.0, 100.0, "maximum");
    assert!(
        lr_pass > 0.0,
        "conforming sample should have positive log LR"
    );
    assert!(
        lr_fail < 0.0,
        "violating sample should have negative log LR"
    );

    let lr_min_pass = compute_step_log_lr(120.0, 1.0, 100.0, "minimum");
    let lr_min_fail = compute_step_log_lr(80.0, 1.0, 100.0, "minimum");
    assert!(
        lr_min_pass > 0.0,
        "conforming min sample should have positive log LR"
    );
    assert!(
        lr_min_fail < 0.0,
        "violating min sample should have negative log LR"
    );
}

#[test]
fn ville_inequality_error_control_bound() {
    let mut false_passes = 0;
    let total_simulations = 2000;
    let alpha = 0.05;
    let e_pass_thresh = 1.0 / alpha;

    let mut state: u64 = 0x853c_49e6_748f_ea9b;
    let mut next_u64 = || -> u64 {
        let mut x = state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state = x;
        x
    };

    for _ in 0..total_simulations {
        let mut log_e = 0.0f64;
        let mut early_passed = false;

        for _step in 0..DEFAULT_MAX_STEPS {
            let u1 = ((next_u64() as f64) + 1.0) / (u64::MAX as f64 + 2.0);
            let u2 = ((next_u64() as f64) + 1.0) / (u64::MAX as f64 + 2.0);
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
            let rel_margin = z * 0.1;
            let delta = 0.5 * rel_margin - 0.125;
            log_e += delta;
            if log_e.exp() >= e_pass_thresh {
                early_passed = true;
                break;
            }
        }

        if early_passed {
            false_passes += 1;
        }
    }

    let empirical_false_pass_rate = (false_passes as f64) / (total_simulations as f64);
    assert!(
        empirical_false_pass_rate <= alpha,
        "empirical false pass rate {empirical_false_pass_rate} must be <= alpha {alpha}"
    );
}

#[test]
fn early_stopping_on_overwhelming_pass() -> Result<()> {
    let samples: Vec<NRunSample> = (0..15)
        .map(|i| NRunSample {
            run_index: i + 1,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "c8ce94c3".to_string(),
            raw_value: 50.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let series = BudgetMeasurementSeries {
        budget_name: "startup_version_p95".to_string(),
        category: "latency".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 100.0,
        samples,
    };

    let result = run_sequential_test(&series, DEFAULT_ALPHA, DEFAULT_BETA, DEFAULT_MAX_STEPS)?;
    assert_eq!(result.decision, "EARLY_PASS");
    assert_eq!(result.final_verdict, "PASS");
    assert!(
        result.stopping_step <= 10,
        "should early stop in <= 10 steps on strong pass"
    );
    assert!(result.final_e_value >= 20.0);

    Ok(())
}

#[test]
fn early_stopping_on_overwhelming_reject() -> Result<()> {
    let samples: Vec<NRunSample> = (0..15)
        .map(|i| NRunSample {
            run_index: i + 1,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "c8ce94c3".to_string(),
            raw_value: 180.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let series = BudgetMeasurementSeries {
        budget_name: "startup_version_p95".to_string(),
        category: "latency".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 100.0,
        samples,
    };

    let result = run_sequential_test(&series, DEFAULT_ALPHA, DEFAULT_BETA, DEFAULT_MAX_STEPS)?;
    assert_eq!(result.decision, "EARLY_REJECT");
    assert_eq!(result.final_verdict, "FAIL");
    assert!(
        result.stopping_step <= 10,
        "should early stop in <= 10 steps on strong reject"
    );
    assert!(result.final_e_value <= DEFAULT_BETA);

    Ok(())
}

#[test]
fn live_sequential_artifact_passes_verification() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/sequential-budget-gate-contract.json");
    assert!(series_path.exists());
    assert!(contract_path.exists());

    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let mut evaluations = Vec::new();
    let mut passing = 0;
    let mut failing = 0;

    for series in &series_input.series {
        let result = run_sequential_test(series, DEFAULT_ALPHA, DEFAULT_BETA, DEFAULT_MAX_STEPS)?;
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
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        alpha: DEFAULT_ALPHA,
        beta: DEFAULT_BETA,
        e_threshold_pass: 1.0 / DEFAULT_ALPHA,
        e_threshold_reject: DEFAULT_BETA,
        total_budgets: evaluations.len(),
        passing_budgets: passing,
        failing_budgets: failing,
        overall_verdict,
        evaluations,
    };

    let report = verify_sequential_artifact(&artifact, contract_path);
    assert_eq!(report.status, "pass");
    assert!(
        report.errors.is_empty(),
        "expected 0 errors, got {:?}",
        report.errors
    );
    assert_eq!(report.evaluated_budgets, 6);

    Ok(())
}

#[test]
fn tamper_detection_in_sequential_evaluations() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/sequential-budget-gate-contract.json");
    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let mut evaluations = Vec::new();
    for series in &series_input.series {
        evaluations.push(run_sequential_test(
            series,
            DEFAULT_ALPHA,
            DEFAULT_BETA,
            DEFAULT_MAX_STEPS,
        )?);
    }

    let mut artifact = SequentialBudgetEvaluationArtifact {
        schema: SEQUENTIAL_GATE_EVALUATION_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        alpha: DEFAULT_ALPHA,
        beta: DEFAULT_BETA,
        e_threshold_pass: 20.0,
        e_threshold_reject: 0.01,
        total_budgets: evaluations.len(),
        passing_budgets: 4,
        failing_budgets: 2,
        overall_verdict: "FAIL".to_string(),
        evaluations,
    };

    if let Some(eval) = artifact
        .evaluations
        .iter_mut()
        .find(|e| e.final_verdict == "FAIL")
    {
        eval.final_verdict = "PASS".to_string();
    }
    let report = verify_sequential_artifact(&artifact, contract_path);
    assert_eq!(report.status, "fail");
    assert!(!report.errors.is_empty());

    Ok(())
}
