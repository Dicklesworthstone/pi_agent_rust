#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub const NRUN_EVIDENCE_PROTOCOL_SCHEMA: &str = "pi.nrun.evidence_protocol.contract.v1";
pub const NRUN_BUDGET_EVALUATION_SCHEMA: &str = "pi.nrun.budget_evaluation.v1";
pub const MIN_REPETITIONS: usize = 10;
pub const BOOTSTRAP_RESAMPLES: usize = 1000;
pub const MAX_ALLOWED_NOISE_SCORE: u8 = 15;

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

#[derive(Debug, Clone)]
pub struct DeterministicPrng {
    state: u64,
}

impl DeterministicPrng {
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 { 0x5EED_1234_5678_ABCD } else { seed };
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

pub fn calculate_weighted_mean(samples: &[NRunSample]) -> Result<f64, Box<dyn Error>> {
    if samples.is_empty() {
        return Err("cannot compute weighted mean of empty sample set".into());
    }
    let mut sum_weighted = 0.0;
    let mut sum_weights = 0.0;
    for sample in samples {
        sum_weighted += sample.raw_value * sample.weight;
        sum_weights += sample.weight;
    }
    if sum_weights <= 0.0 {
        return Err("sum of sample weights must be positive".into());
    }
    Ok(sum_weighted / sum_weights)
}

pub fn calculate_n_eff(samples: &[NRunSample]) -> Result<f64, Box<dyn Error>> {
    if samples.is_empty() {
        return Err("cannot compute n_eff of empty sample set".into());
    }
    let mut sum_w = 0.0;
    let mut sum_w_sq = 0.0;
    for sample in samples {
        sum_w += sample.weight;
        sum_w_sq += sample.weight * sample.weight;
    }
    if sum_w_sq <= 0.0 {
        return Err("sum of squared weights must be positive".into());
    }
    Ok((sum_w * sum_w) / sum_w_sq)
}

pub fn compute_bootstrap_ci95(
    samples: &[NRunSample],
    resamples: usize,
    seed: u64,
) -> Result<(f64, f64), Box<dyn Error>> {
    let n = samples.len();
    if n == 0 {
        return Err("cannot bootstrap empty sample set".into());
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
        return Err("no valid replicate means computed in bootstrap".into());
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
) -> Result<BudgetEvaluationResult, Box<dyn Error>> {
    if series.samples.len() < min_repetitions {
        return Err(format!(
            "insufficient repetitions for {}: required {}, provided {}",
            series.budget_name,
            min_repetitions,
            series.samples.len()
        ).into());
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
        return Err(format!("duplicate correlation_id found in series {}: {dup}", series.budget_name).into());
    }
    if let Some(score) = noisy_score {
        return Err(format!("sample noise_score {score} exceeds max {MAX_ALLOWED_NOISE_SCORE} in series {}", series.budget_name).into());
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
        other => return Err(format!("unrecognized budget comparison rule: {other}").into()),
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
            Self::InsufficientRepetitions(name, cnt) => write!(f, "budget {name} evaluated with {cnt} repetitions (< 10)"),
            Self::InvalidCi(name, lower, upper) => write!(f, "invalid CI bounds for {name}: lower {lower} > upper {upper}"),
            Self::StatusMismatch(name, expected, recorded) => write!(f, "status mismatch for {name}: expected {expected}, recorded {recorded}"),
            Self::UnknownRule(name, rule) => write!(f, "unknown comparison rule {rule} in budget {name}"),
        }
    }
}

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
            issues.push(NRunIssue::InsufficientRepetitions(&eval.budget_name, eval.repetition_count));
        }

        if eval.ci_95_lower > eval.ci_95_upper {
            issues.push(NRunIssue::InvalidCi(&eval.budget_name, eval.ci_95_lower, eval.ci_95_upper));
        }

        match eval.comparison.as_str() {
            "maximum" => {
                let expected_status = if eval.ci_95_upper <= eval.threshold {
                    "PASS"
                } else {
                    "FAIL"
                };
                if eval.status != expected_status {
                    issues.push(NRunIssue::StatusMismatch(&eval.budget_name, expected_status, &eval.status));
                }
            }
            "minimum" => {
                let expected_status = if eval.ci_95_lower >= eval.threshold {
                    "PASS"
                } else {
                    "FAIL"
                };
                if eval.status != expected_status {
                    issues.push(NRunIssue::StatusMismatch(&eval.budget_name, expected_status, &eval.status));
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

#[test]
fn contract_file_matches_schema_and_policy() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let contract_path = root.join("docs/contracts/nrun-evidence-protocol-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = std::fs::read_to_string(&contract_path)?;
    let contract: Value = serde_json::from_str(&text)?;

    assert_eq!(
        contract.get("schema").and_then(Value::as_str),
        Some(NRUN_EVIDENCE_PROTOCOL_SCHEMA)
    );
    assert_eq!(
        contract.get("bead_id").and_then(Value::as_str),
        Some("bd-sog97.10")
    );

    let reqs = contract.get("protocol_requirements").ok_or("missing protocol_requirements")?;
    assert_eq!(reqs.get("min_repetitions").and_then(Value::as_u64), Some(10));
    assert_eq!(reqs.get("confidence_level").and_then(Value::as_f64), Some(0.95));
    assert_eq!(reqs.get("require_distinct_correlation_ids").and_then(Value::as_bool), Some(true));

    let enforced = contract.get("enforced_budgets").and_then(Value::as_array).ok_or("missing enforced_budgets")?;
    assert!(enforced.len() >= 5);
    Ok(())
}

#[test]
fn live_nrun_artifact_passes_verification() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let artifact_path = root.join("docs/evidence/nrun-budget-evaluation.json");
    let contract_path = root.join("docs/contracts/nrun-evidence-protocol-contract.json");
    assert!(artifact_path.exists(), "nrun artifact must exist");

    let text = std::fs::read_to_string(&artifact_path)?;
    let artifact: NRunBudgetEvaluationArtifact = serde_json::from_str(&text)?;

    let report = verify_nrun_artifact(&artifact, &contract_path);
    assert_eq!(report.status, "pass", "verification report errors: {:?}", report.errors);
    assert!(report.errors.is_empty());
    assert!(report.evaluated_budgets >= 5);
    Ok(())
}

#[test]
fn weighted_mean_and_neff_exactness() -> Result<(), Box<dyn Error>> {
    let samples = vec![
        NRunSample {
            run_index: 0,
            correlation_id: "corr-1".to_string(),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 10.0,
            weight: 1.0,
            noise_score: 0,
        },
        NRunSample {
            run_index: 1,
            correlation_id: "corr-2".to_string(),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 20.0,
            weight: 3.0,
            noise_score: 0,
        },
    ];

    let mean = calculate_weighted_mean(&samples)?;
    // (10*1 + 20*3) / (1+3) = 70 / 4 = 17.5
    assert!((mean - 17.5).abs() < 1e-9);

    let neff = calculate_n_eff(&samples)?;
    // (1+3)^2 / (1^2 + 3^2) = 16 / 10 = 1.6
    assert!((neff - 1.6).abs() < 1e-9);
    Ok(())
}

#[test]
fn bootstrap_ci95_determinism() -> Result<(), Box<dyn Error>> {
    let samples: Vec<NRunSample> = (0..12)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 100.0 + (i as f64) * 2.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let (lower1, upper1) = compute_bootstrap_ci95(&samples, BOOTSTRAP_RESAMPLES, 42)?;
    let (lower2, upper2) = compute_bootstrap_ci95(&samples, BOOTSTRAP_RESAMPLES, 42)?;
    assert_eq!(lower1, lower2);
    assert_eq!(upper1, upper2);
    assert!(lower1 <= upper1);
    assert!(lower1 >= 100.0);
    assert!(upper1 <= 124.0);
    Ok(())
}

#[test]
fn rejection_of_insufficient_repetitions() {
    let samples: Vec<NRunSample> = (0..5)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 10.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let series = BudgetMeasurementSeries {
        budget_name: "test_insufficient".to_string(),
        category: "test".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 20.0,
        samples,
    };

    let result = evaluate_budget_series(&series, MIN_REPETITIONS, 12345);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("insufficient repetitions"));
}

#[test]
fn rejection_of_duplicate_correlation_ids() {
    let mut samples: Vec<NRunSample> = (0..10)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 10.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    if let Some(s) = samples.get_mut(5) { s.correlation_id = "corr-0".to_string(); }

    let series = BudgetMeasurementSeries {
        budget_name: "test_duplicate".to_string(),
        category: "test".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 20.0,
        samples,
    };

    let result = evaluate_budget_series(&series, MIN_REPETITIONS, 12345);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("duplicate correlation_id"));
}

#[test]
fn rejection_of_noisy_samples() {
    let mut samples: Vec<NRunSample> = (0..10)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 10.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    if let Some(s) = samples.get_mut(3) { s.noise_score = 25; } // exceeds max 15

    let series = BudgetMeasurementSeries {
        budget_name: "test_noisy".to_string(),
        category: "test".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 20.0,
        samples,
    };

    let result = evaluate_budget_series(&series, MIN_REPETITIONS, 12345);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("noise_score 25 exceeds max 15"));
}

#[test]
fn maximum_and_minimum_decision_rules() -> Result<(), Box<dyn Error>> {
    // Maximum rule: CI upper bound <= threshold
    let passing_max_samples: Vec<NRunSample> = (0..10)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-pass-max-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 5.0 + (i as f64) * 0.1,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let passing_max = BudgetMeasurementSeries {
        budget_name: "pass_max".to_string(),
        category: "test".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 10.0,
        samples: passing_max_samples,
    };
    let res = evaluate_budget_series(&passing_max, MIN_REPETITIONS, 42)?;
    assert_eq!(res.status, "PASS");

    // Failing max rule: CI upper bound > threshold
    let failing_max = BudgetMeasurementSeries {
        budget_name: "fail_max".to_string(),
        category: "test".to_string(),
        comparison: "maximum".to_string(),
        unit: "ms".to_string(),
        threshold: 5.2, // upper bound around 5.8 > 5.2
        samples: passing_max.samples.clone(),
    };
    let res = evaluate_budget_series(&failing_max, MIN_REPETITIONS, 42)?;
    assert_eq!(res.status, "FAIL");

    // Minimum rule: CI lower bound >= threshold
    let min_samples: Vec<NRunSample> = (0..10)
        .map(|i| NRunSample {
            run_index: i,
            correlation_id: format!("corr-min-{i}"),
            timestamp: "2026-08-24T00:00:00Z".to_string(),
            git_commit: "head".to_string(),
            raw_value: 5000.0 + (i as f64) * 50.0,
            weight: 1.0,
            noise_score: 0,
        })
        .collect();

    let passing_min = BudgetMeasurementSeries {
        budget_name: "pass_min".to_string(),
        category: "test".to_string(),
        comparison: "minimum".to_string(),
        unit: "calls/sec".to_string(),
        threshold: 4800.0,
        samples: min_samples.clone(),
    };
    let res = evaluate_budget_series(&passing_min, MIN_REPETITIONS, 42)?;
    assert_eq!(res.status, "PASS");

    let failing_min = BudgetMeasurementSeries {
        budget_name: "fail_min".to_string(),
        category: "test".to_string(),
        comparison: "minimum".to_string(),
        unit: "calls/sec".to_string(),
        threshold: 5300.0, // mean is ~5225, so lower bound is ~5120 < 5300
        samples: min_samples,
    };
    let res = evaluate_budget_series(&failing_min, MIN_REPETITIONS, 42)?;
    assert_eq!(res.status, "FAIL");

    Ok(())
}
