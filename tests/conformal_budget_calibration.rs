#![forbid(unsafe_code)]
#![allow(
    clippy::suboptimal_flops,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::redundant_closure_for_method_calls,
    clippy::float_cmp,
    clippy::too_many_lines
)]

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFORMAL_CONTRACT_SCHEMA: &str = "pi.conformal_calibration.contract.v1";
pub const CONFORMAL_ARTIFACT_SCHEMA: &str = "pi.conformal_calibration.v1";

#[derive(Debug, Parser)]
#[command(name = "conformal_budget_calibration")]
#[command(about = "Data-derived conformal calibration and amendment for performance budgets")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Calibrate budget thresholds using conformal quantiles from historical measurement series.
    Calibrate(CalibrateArgs),
    /// Verify a conformal calibration artifact against contract rules.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct CalibrateArgs {
    /// Input series JSON file.
    #[arg(long, default_value = "docs/evidence/nrun-measurement-series.json")]
    input: PathBuf,
    /// Output path for the calibration artifact.
    #[arg(
        long,
        default_value = "docs/evidence/conformal-budget-calibration.json"
    )]
    output: PathBuf,
    /// Target empirical coverage level (e.g. 0.95 for 95% coverage).
    #[arg(long, default_value_t = 0.95)]
    target_coverage: f64,
    /// Minimum required calibration sample size.
    #[arg(long, default_value_t = 10)]
    min_samples: usize,
    /// Conformal safety padding multiplier.
    #[arg(long, default_value_t = 1.10)]
    padding_multiplier: f64,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to the conformal calibration artifact.
    #[arg(
        long,
        default_value = "docs/evidence/conformal-budget-calibration.json"
    )]
    input: PathBuf,
    /// Contract path for conformal calibration rules.
    #[arg(
        long,
        default_value = "docs/contracts/conformal-budget-calibration-contract.json"
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
pub struct ConformalCalibrationConfig {
    pub target_coverage: f64,
    pub significance_alpha: f64,
    pub min_calibration_samples: usize,
    pub conformal_padding_multiplier: f64,
}

impl Default for ConformalCalibrationConfig {
    fn default() -> Self {
        Self {
            target_coverage: 0.95,
            significance_alpha: 0.05,
            min_calibration_samples: 10,
            conformal_padding_multiplier: 1.10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalibratedBudget {
    pub budget_name: String,
    pub category: String,
    pub comparison: String,
    pub unit: String,
    pub folklore_threshold: f64,
    pub calibrated_threshold: f64,
    pub basis_type: String,
    pub sample_size: usize,
    pub coverage_guarantee: f64,
    pub conformal_quantile_index: usize,
    pub empirical_p95: f64,
    pub justification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetAmendmentRecord {
    pub budget_name: String,
    pub original_threshold: f64,
    pub amended_threshold: f64,
    pub unit: String,
    pub basis: String,
    pub reason: String,
    pub approver_role: String,
    pub evidence_provenance_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformalCalibrationArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub config: ConformalCalibrationConfig,
    pub total_budgets: usize,
    pub data_derived_count: usize,
    pub folklore_policy_count: usize,
    pub calibrated_budgets: Vec<CalibratedBudget>,
    pub amendment_dry_runs: Vec<BudgetAmendmentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub amendment_records: usize,
    pub errors: Vec<String>,
}

/// Computes the split-conformal calibration quantile threshold with finite-sample coverage guarantee.
pub fn compute_conformal_threshold(
    raw_samples: &[f64],
    comparison: &str,
    target_coverage: f64,
    padding_multiplier: f64,
) -> Result<(f64, usize, f64)> {
    let n = raw_samples.len();
    if n == 0 {
        bail!("cannot calibrate empty sample set");
    }

    let mut sorted = raw_samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let alpha = (1.0 - target_coverage).clamp(0.001, 0.5);

    if comparison == "minimum" {
        // Lower quantile bound for throughput/minimums
        let q_idx = (((n + 1) as f64) * alpha).floor().clamp(1.0, n as f64) as usize;
        let base_val = sorted.get(q_idx.saturating_sub(1)).copied().unwrap_or(0.0);
        let threshold = (base_val / padding_multiplier.max(1.0)).max(0.001);
        Ok((threshold, q_idx, base_val))
    } else {
        // Upper quantile bound for latency/maximums: index = ceil((n + 1) * (1 - alpha))
        let q_idx = (((n + 1) as f64) * (1.0 - alpha))
            .ceil()
            .clamp(1.0, n as f64) as usize;
        let base_val = sorted.get(q_idx.saturating_sub(1)).copied().unwrap_or(0.0);
        let threshold = base_val * padding_multiplier.max(1.0);
        Ok((threshold, q_idx, base_val))
    }
}

pub fn calibrate_series(
    series: &BudgetMeasurementSeries,
    config: &ConformalCalibrationConfig,
) -> Result<CalibratedBudget> {
    if series.samples.len() < config.min_calibration_samples {
        bail!(
            "insufficient samples for {}: {} < {}",
            series.budget_name,
            series.samples.len(),
            config.min_calibration_samples
        );
    }

    let raw_vals: Vec<f64> = series.samples.iter().map(|s| s.raw_value).collect();
    let (cal_thresh, q_idx, p95) = compute_conformal_threshold(
        &raw_vals,
        &series.comparison,
        config.target_coverage,
        config.conformal_padding_multiplier,
    )?;

    // Classify whether budget is calibrated via data or retained as folklore policy choice
    let (basis_type, justification) = if series.budget_name == "binary_size_mb" {
        (
            "FOLKLORE_POLICY_CHOICE".to_string(),
            "Retained as hard architectural release policy gate (48 MiB binary budget)".to_string(),
        )
    } else {
        (
            "DATA_DERIVED_CONFORMAL".to_string(),
            format!(
                "Statistically calibrated from N={} runs with {:.1}% finite-sample conformal coverage guarantee",
                series.samples.len(),
                config.target_coverage * 100.0
            ),
        )
    };

    Ok(CalibratedBudget {
        budget_name: series.budget_name.clone(),
        category: series.category.clone(),
        comparison: series.comparison.clone(),
        unit: series.unit.clone(),
        folklore_threshold: series.threshold,
        calibrated_threshold: cal_thresh,
        basis_type,
        sample_size: series.samples.len(),
        coverage_guarantee: config.target_coverage,
        conformal_quantile_index: q_idx,
        empirical_p95: p95,
        justification,
    })
}

enum CalibIssue<'a> {
    SampleSizeLow(&'a str, usize, usize),
    CoverageLow(&'a str, f64, f64),
    UnknownBasis(&'a str, &'a str),
}

impl std::fmt::Display for CalibIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SampleSizeLow(name, sz, min_sz) => {
                write!(f, "sample size {sz} < min {min_sz} in budget {name}")
            }
            Self::CoverageLow(name, cov, req) => {
                write!(f, "coverage {cov} < required {req} in budget {name}")
            }
            Self::UnknownBasis(name, b) => write!(f, "unknown basis type {b} in budget {name}"),
        }
    }
}

pub fn verify_conformal_artifact(
    artifact: &ConformalCalibrationArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != CONFORMAL_ARTIFACT_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            CONFORMAL_ARTIFACT_SCHEMA, artifact.schema
        ));
    }

    if artifact.total_budgets != artifact.calibrated_budgets.len() {
        errors.push(format!(
            "total_budgets mismatch: declared {}, actual {}",
            artifact.total_budgets,
            artifact.calibrated_budgets.len()
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut data_derived = 0;
    let mut folklore = 0;
    let mut issues = Vec::new();

    for budget in &artifact.calibrated_budgets {
        if budget.sample_size < artifact.config.min_calibration_samples {
            issues.push(CalibIssue::SampleSizeLow(
                &budget.budget_name,
                budget.sample_size,
                artifact.config.min_calibration_samples,
            ));
        }

        if budget.coverage_guarantee < artifact.config.target_coverage {
            issues.push(CalibIssue::CoverageLow(
                &budget.budget_name,
                budget.coverage_guarantee,
                artifact.config.target_coverage,
            ));
        }

        match budget.basis_type.as_str() {
            "DATA_DERIVED_CONFORMAL" => data_derived += 1,
            "FOLKLORE_POLICY_CHOICE" => folklore += 1,
            other => issues.push(CalibIssue::UnknownBasis(&budget.budget_name, other)),
        }
    }

    errors.extend(issues.into_iter().map(|i| i.to_string()));

    if data_derived != artifact.data_derived_count {
        errors.push(format!(
            "data_derived_count mismatch: declared {}, actual {}",
            artifact.data_derived_count, data_derived
        ));
    }

    if folklore != artifact.folklore_policy_count {
        errors.push(format!(
            "folklore_policy_count mismatch: declared {}, actual {}",
            artifact.folklore_policy_count, folklore
        ));
    }

    if artifact.amendment_dry_runs.is_empty() {
        errors.push("amendment_dry_runs must contain at least one amendment record".to_string());
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.conformal_calibration.verification_report.v1".to_string(),
        status,
        evaluated_budgets: artifact.calibrated_budgets.len(),
        amendment_records: artifact.amendment_dry_runs.len(),
        errors,
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<()> {
    let contract_path = Path::new("docs/contracts/conformal-budget-calibration-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = fs::read_to_string(contract_path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;

    assert_eq!(
        val.get("schema").and_then(|v| v.as_str()),
        Some(CONFORMAL_CONTRACT_SCHEMA)
    );
    assert_eq!(
        val.get("bead_id").and_then(|v| v.as_str()),
        Some("bd-sog97.15")
    );
    assert_eq!(
        val.get("calibration_parameters")
            .and_then(|cp| cp.get("target_coverage"))
            .and_then(|c| c.as_f64()),
        Some(0.95)
    );
    assert_eq!(
        val.get("calibration_parameters")
            .and_then(|cp| cp.get("min_calibration_samples"))
            .and_then(|m| m.as_u64()),
        Some(10)
    );

    Ok(())
}

#[test]
fn conformal_coverage_finite_sample_guarantee() -> Result<()> {
    // Generate synthetic calibration set of n=100 from Exponential(mean=10)
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let mut next_u64 = || -> u64 {
        let mut x = state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state = x;
        x
    };

    let n_calib = 100;
    let mut calib_samples = Vec::with_capacity(n_calib);
    for _ in 0..n_calib {
        let u = ((next_u64() as f64) + 1.0) / (u64::MAX as f64 + 2.0);
        let val = -10.0 * (1.0 - u).ln(); // Exp(10)
        calib_samples.push(val);
    }

    let target_coverage = 0.95;
    let (calibrated_threshold, _, _) =
        compute_conformal_threshold(&calib_samples, "maximum", target_coverage, 1.0)?;

    // Test on 1000 independent test points
    let n_test = 1000;
    let mut covered = 0;
    for _ in 0..n_test {
        let u = ((next_u64() as f64) + 1.0) / (u64::MAX as f64 + 2.0);
        let test_val = -10.0 * (1.0 - u).ln();
        if test_val <= calibrated_threshold {
            covered += 1;
        }
    }

    let empirical_coverage = (covered as f64) / (n_test as f64);
    assert!(
        empirical_coverage >= 0.93,
        "empirical coverage {empirical_coverage} should be near/above nominal 0.95"
    );

    Ok(())
}

#[test]
fn conformal_quantile_index_formula() -> Result<()> {
    // Test exact finite sample formula ceil((n + 1) * (1 - alpha))
    let samples: Vec<f64> = (1..=20).map(|x| x as f64).collect();
    let (thresh_max, q_idx_max, base_max) =
        compute_conformal_threshold(&samples, "maximum", 0.95, 1.0)?;
    assert_eq!(q_idx_max, 20); // ceil(21 * 0.95) = ceil(19.95) = 20
    assert_eq!(base_max, 20.0);
    assert_eq!(thresh_max, 20.0);

    let (thresh_min, q_idx_min, base_min) =
        compute_conformal_threshold(&samples, "minimum", 0.95, 1.0)?;
    assert_eq!(q_idx_min, 1); // floor(21 * 0.05) = floor(1.05) = 1
    assert_eq!(base_min, 1.0);
    assert_eq!(thresh_min, 1.0);

    Ok(())
}

#[test]
fn live_conformal_artifact_passes_verification() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/conformal-budget-calibration-contract.json");
    assert!(series_path.exists());
    assert!(contract_path.exists());

    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let config = ConformalCalibrationConfig::default();
    let mut calibrated_budgets = Vec::new();
    let mut data_derived = 0;
    let mut folklore = 0;

    for series in &series_input.series {
        let cal = calibrate_series(series, &config)?;
        if cal.basis_type == "DATA_DERIVED_CONFORMAL" {
            data_derived += 1;
        } else {
            folklore += 1;
        }
        calibrated_budgets.push(cal);
    }

    assert!(
        data_derived >= 3,
        "must calibrate >= 3 budgets via data-derived quantiles"
    );

    let amendment_dry_runs = vec![BudgetAmendmentRecord {
        budget_name: "ext_cold_load_simple_p95".to_string(),
        original_threshold: 5.0,
        amended_threshold: 8.5,
        unit: "ms".to_string(),
        basis: "DATA_DERIVED_CONFORMAL".to_string(),
        reason: "RI-CONFORMAL amendment dry-run: calibrated against empirical N=15 run series with 95% coverage guarantee".to_string(),
        approver_role: "Release Engineering / Runtime Autonomy".to_string(),
        evidence_provenance_hash: "sha256:c8ce94c3_nrun_series_v1".to_string(),
    }];

    let artifact = ConformalCalibrationArtifact {
        schema: CONFORMAL_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        config,
        total_budgets: calibrated_budgets.len(),
        data_derived_count: data_derived,
        folklore_policy_count: folklore,
        calibrated_budgets,
        amendment_dry_runs,
    };

    let report = verify_conformal_artifact(&artifact, contract_path);
    assert_eq!(report.status, "pass");
    assert!(
        report.errors.is_empty(),
        "expected 0 errors, got {:?}",
        report.errors
    );
    assert_eq!(report.evaluated_budgets, 6);
    assert_eq!(report.amendment_records, 1);

    Ok(())
}

#[test]
fn tamper_detection_in_conformal_artifact() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/conformal-budget-calibration-contract.json");
    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let config = ConformalCalibrationConfig::default();
    let mut calibrated_budgets = Vec::new();
    for series in &series_input.series {
        calibrated_budgets.push(calibrate_series(series, &config)?);
    }

    let mut artifact = ConformalCalibrationArtifact {
        schema: CONFORMAL_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        config,
        total_budgets: calibrated_budgets.len(),
        data_derived_count: 5,
        folklore_policy_count: 1,
        calibrated_budgets,
        amendment_dry_runs: vec![BudgetAmendmentRecord {
            budget_name: "test".to_string(),
            original_threshold: 5.0,
            amended_threshold: 8.5,
            unit: "ms".to_string(),
            basis: "DATA_DERIVED_CONFORMAL".to_string(),
            reason: "test".to_string(),
            approver_role: "test".to_string(),
            evidence_provenance_hash: "test".to_string(),
        }],
    };

    // Tamper with data derived count
    artifact.data_derived_count = 10;
    let report = verify_conformal_artifact(&artifact, contract_path);
    assert_eq!(report.status, "fail");
    assert!(!report.errors.is_empty());

    Ok(())
}

#[test]
fn approved_cold_load_amendment_is_hash_bound_and_non_vacuous() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    let contract: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        "docs/contracts/conformal-budget-calibration-contract.json",
    )?)?;
    let amendment_path = contract
        .get("formal_amendment_path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("contract must name the formal amendment path"))?;
    assert_eq!(
        amendment_path,
        "docs/evidence/ext-cold-load-budget-amendment.json"
    );

    let amendment: serde_json::Value = serde_json::from_str(&fs::read_to_string(amendment_path)?)?;
    assert_eq!(
        amendment.get("schema").and_then(serde_json::Value::as_str),
        Some("pi.conformal_budget_amendment.v1")
    );
    assert_eq!(
        amendment
            .get("decision")
            .and_then(serde_json::Value::as_str),
        Some("APPROVED")
    );
    assert_eq!(
        amendment
            .get("budget_name")
            .and_then(serde_json::Value::as_str),
        Some("ext_cold_load_simple_p95")
    );
    assert_eq!(
        amendment.get("bead_id").and_then(serde_json::Value::as_str),
        Some("bd-sog97.5")
    );
    let measurement = amendment
        .get("measurement")
        .ok_or_else(|| anyhow::anyhow!("amendment must contain measurement evidence"))?;
    assert_eq!(
        amendment
            .get("source_commit")
            .and_then(serde_json::Value::as_str),
        measurement
            .get("source_commit")
            .and_then(serde_json::Value::as_str)
    );

    let previous = amendment
        .get("previous_threshold")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("previous threshold must be numeric"))?;
    let amended = amendment
        .get("amended_threshold")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("amended threshold must be numeric"))?;
    assert_eq!(
        measurement
            .get("source_dirty")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        measurement
            .get("noise_score")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    for (field, expected) in [
        (
            "evidence_manifest_sha256",
            "sha256:2f821475ee38d7f3b75a566b78e3579013b5d9cdabf8e880199973562fa192f1",
        ),
        (
            "bench_env_json_sha256",
            "sha256:3adc5af4d48e29e1d72a80579445ee682ad4f24b4907530d99032e8fb4387ac7",
        ),
        (
            "original_host_state_sha256",
            "sha256:586f7c7aa12a4404d043a8435c4407b1e3967745fa55d0a20eaa4843faea8371",
        ),
        (
            "source_runs_jsonl_sha256",
            "sha256:76b07b716c867564a48cf320974dfd7cf206b4a06f3627cf195b2ef8de4e7dd7",
        ),
    ] {
        let hash = measurement
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("{field} must be a string"))?;
        assert_eq!(
            hash, expected,
            "{field} must bind the accepted raw evidence"
        );
        assert_eq!(hash.len(), 71, "{field} must be a tagged SHA-256");
        let digest = hash
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("{field} must be tagged as SHA-256"))?;
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    let series = measurement
        .get("series_ms")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("measurement series must be an array"))?;
    assert_eq!(series.len(), 20);
    assert_eq!(
        measurement
            .get("sample_processes")
            .and_then(serde_json::Value::as_u64),
        Some(20)
    );
    assert_eq!(
        measurement
            .get("series_canonicalization")
            .and_then(serde_json::Value::as_str),
        Some("python_json.dumps(separators=(',', ':'), ensure_ascii=False)")
    );
    let series_json = measurement
        .get("series_values_canonical_json")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("canonical series text must be a string"))?;
    let canonical_series: serde_json::Value = serde_json::from_str(series_json)?;
    assert_eq!(
        canonical_series.as_array().map(Vec::as_slice),
        Some(series.as_slice())
    );
    let series_sha = format!(
        "sha256:{}",
        pi::package_manager::hex_encode(&Sha256::digest(series_json.as_bytes()))
    );
    assert_eq!(
        measurement
            .get("series_values_sha256")
            .and_then(serde_json::Value::as_str),
        Some(series_sha.as_str())
    );

    let values = series
        .iter()
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("series values must be numeric"))
        })
        .collect::<Result<Vec<_>>>()?;
    let max_value = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let expected_threshold = (max_value * 1.10 * 1_000_000.0).ceil() / 1_000_000.0;
    assert!((amended - expected_threshold).abs() <= 1e-12);
    let statistical_justification = amendment
        .get("statistical_justification")
        .ok_or_else(|| anyhow::anyhow!("amendment must contain statistical justification"))?;
    let split_conformal = statistical_justification
        .get("split_conformal")
        .ok_or_else(|| {
            anyhow::anyhow!("statistical justification must contain split conformal evidence")
        })?;
    assert_eq!(
        split_conformal
            .get("quantile_index")
            .and_then(serde_json::Value::as_u64),
        Some(20)
    );

    let bootstrap = statistical_justification
        .get("bootstrap_p95_ci95")
        .ok_or_else(|| {
            anyhow::anyhow!("statistical justification must contain bootstrap evidence")
        })?;
    let lower = bootstrap
        .get("lower_ms")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("bootstrap lower bound must be numeric"))?;
    let upper = bootstrap
        .get("upper_ms")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("bootstrap upper bound must be numeric"))?;
    assert!(
        previous < lower,
        "bootstrap CI must exclude the old threshold"
    );
    assert!(lower <= upper);
    assert!(upper <= amended);

    for field in ["criterion_estimates_sha256", "run_logs_sha256"] {
        let hashes = measurement
            .get(field)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("{field} must be an array"))?;
        assert_eq!(hashes.len(), series.len());
        let unique = hashes
            .iter()
            .map(|hash| hash.as_str().unwrap_or_default())
            .collect::<HashSet<_>>();
        assert_eq!(unique.len(), series.len(), "{field} must be unique per run");
        assert!(
            unique.iter().all(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        );
    }

    let approval = amendment
        .get("approval")
        .ok_or_else(|| anyhow::anyhow!("amendment must contain approval evidence"))?;
    assert_eq!(
        approval
            .get("approver_role")
            .and_then(serde_json::Value::as_str),
        Some("Release Engineering / Runtime Autonomy")
    );
    assert!(
        approval
            .get("approver_identity")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| !v.is_empty())
    );
    assert!(
        approval
            .get("agent_mail_approval_message_id")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|id| id > 0)
    );

    Ok(())
}
