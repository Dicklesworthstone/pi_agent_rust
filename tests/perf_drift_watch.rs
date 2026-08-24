#![forbid(unsafe_code)]

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DRIFT_WATCH_CONTRACT_SCHEMA: &str = "pi.perf.drift_watch.contract.v1";
pub const DRIFT_WATCH_ARTIFACT_SCHEMA: &str = "pi.perf.drift_watch.v1";

#[derive(Debug, Parser)]
#[command(name = "perf_drift_watch")]
#[command(about = "Performance budget CUSUM + BOCPD regime drift & shift detector")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Analyze budget history for persistent drift and sudden regime shifts.
    Analyze(AnalyzeArgs),
    /// Verify an existing drift watch artifact against contract rules.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct AnalyzeArgs {
    /// Input series JSON file.
    #[arg(long, default_value = "docs/evidence/nrun-measurement-series.json")]
    input: PathBuf,
    /// Output path for the drift watch artifact.
    #[arg(long, default_value = "docs/evidence/perf-drift-watch.json")]
    output: PathBuf,
    /// CUSUM allowance parameter k (in multiples of sigma).
    #[arg(long, default_value_t = 0.5)]
    cusum_k: f64,
    /// CUSUM threshold parameter h (in multiples of sigma).
    #[arg(long, default_value_t = 4.0)]
    cusum_h: f64,
    /// BOCPD hazard lambda (expected run length between change points).
    #[arg(long, default_value_t = 50.0)]
    bocpd_lambda: f64,
    /// BOCPD change-point probability alarm threshold.
    #[arg(long, default_value_t = 0.5)]
    bocpd_threshold: f64,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to the drift watch artifact.
    #[arg(long, default_value = "docs/evidence/perf-drift-watch.json")]
    input: PathBuf,
    /// Contract path for drift watch rules.
    #[arg(long, default_value = "docs/contracts/drift-watch-contract.json")]
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
pub struct DriftWatchConfig {
    pub cusum_k: f64,
    pub cusum_h: f64,
    pub bocpd_hazard_lambda: f64,
    pub bocpd_changepoint_threshold: f64,
    pub min_baseline_samples: usize,
}

impl Default for DriftWatchConfig {
    fn default() -> Self {
        Self {
            cusum_k: 0.5,
            cusum_h: 4.0,
            bocpd_hazard_lambda: 50.0,
            bocpd_changepoint_threshold: 0.5,
            min_baseline_samples: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetDriftAnalysis {
    pub budget_name: String,
    pub category: String,
    pub comparison: String,
    pub unit: String,
    pub threshold: f64,
    pub sample_count: usize,
    pub baseline_mean: f64,
    pub baseline_stddev: f64,
    pub latest_value: f64,
    pub cusum_upper: f64,
    pub cusum_lower: f64,
    pub cusum_alarm: bool,
    pub bocpd_changepoint_prob: f64,
    pub bocpd_alarm: bool,
    pub drift_status: String,
    pub advisory_message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfDriftWatchArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub config: DriftWatchConfig,
    pub total_budgets: usize,
    pub healthy_budgets: usize,
    pub warning_budgets: usize,
    pub regime_shift_budgets: usize,
    pub overall_status: String,
    pub analyses: Vec<BudgetDriftAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub errors: Vec<String>,
}

/// Online two-sided CUSUM detector for persistent positive or negative drift.
#[derive(Debug, Clone)]
pub struct CusumDetector {
    count: usize,
    mean: f64,
    m2: f64,
    upper: f64,
    lower: f64,
    k: f64,
    h: f64,
}

impl CusumDetector {
    pub fn new(k: f64, h: f64) -> Self {
        Self {
            count: 0,
            mean: 0.0,
            m2: 0.0,
            upper: 0.0,
            lower: 0.0,
            k,
            h,
        }
    }

    pub fn observe(&mut self, value: f64) -> (f64, f64, bool) {
        self.count += 1;
        let prior_mean = self.mean;
        let prior_stddev = if self.count > 2 {
            (self.m2 / ((self.count - 1) as f64)).sqrt().max(1e-4)
        } else {
            1.0
        };

        let delta = value - self.mean;
        self.mean += delta / (self.count as f64);
        let delta2 = value - self.mean;
        self.m2 += delta * delta2;

        if self.count > 5 {
            let z = (value - prior_mean) / prior_stddev.max(1e-2);
            self.upper = (self.upper + z - self.k).max(0.0);
            self.lower = (self.lower - z - self.k).max(0.0);
        }

        let alarmed = self.upper >= self.h || self.lower >= self.h;
        (self.upper, self.lower, alarmed)
    }

    pub fn baseline_mean(&self) -> f64 {
        self.mean
    }

    pub fn baseline_stddev(&self) -> f64 {
        if self.count > 1 {
            (self.m2 / ((self.count - 1) as f64)).sqrt().max(1e-6)
        } else {
            0.0
        }
    }
}

/// Simplified Bayesian Online Changepoint Detection (BOCPD) tracking hazard posterior.
#[derive(Debug, Clone)]
pub struct BocpdDetector {
    hazard_lambda: f64,
    threshold: f64,
    posterior: f64,
    mean: f64,
    var: f64,
    count: usize,
}

impl BocpdDetector {
    pub fn new(hazard_lambda: f64, threshold: f64) -> Self {
        Self {
            hazard_lambda: hazard_lambda.max(1.0),
            threshold,
            posterior: 0.0,
            mean: 0.0,
            var: 1.0,
            count: 0,
        }
    }

    pub fn observe(&mut self, value: f64) -> (f64, bool) {
        self.count += 1;
        if self.count == 1 {
            self.mean = value;
            self.var = 1.0;
            return (0.0, false);
        }

        let diff = value - self.mean;
        let stddev = self.var.sqrt().max(1e-4);
        let z = diff / stddev;

        // Exact BOCPD Gaussian likelihood ratio formulation
        let l_same = (-0.5 * (z * z).min(50.0)).exp();
        let h = (1.0 / self.hazard_lambda).clamp(0.001, 0.5);
        let post = h / (h + (1.0 - h) * l_same);

        self.posterior = post.clamp(0.0, 1.0);

        // Adapt running estimate
        let alpha = if self.posterior >= self.threshold {
            0.8
        } else {
            0.15
        };
        self.mean = (1.0 - alpha) * self.mean + alpha * value;
        self.var = (1.0 - alpha) * self.var + alpha * (diff * diff).max(1e-4);

        let alarmed = self.posterior >= self.threshold;
        (self.posterior, alarmed)
    }
}

pub fn analyze_series_drift(
    series: &BudgetMeasurementSeries,
    config: &DriftWatchConfig,
) -> Result<BudgetDriftAnalysis> {
    if series.samples.is_empty() {
        bail!("empty series for {}", series.budget_name);
    }

    let mut cusum = CusumDetector::new(config.cusum_k, config.cusum_h);
    let mut bocpd = BocpdDetector::new(
        config.bocpd_hazard_lambda,
        config.bocpd_changepoint_threshold,
    );

    let mut latest_val = 0.0;
    let mut final_upper = 0.0;
    let mut final_lower = 0.0;
    let mut cusum_alarm = false;
    let mut bocpd_prob = 0.0;
    let mut bocpd_alarm = false;

    for sample in &series.samples {
        latest_val = sample.raw_value;
        let (u, l, ca) = cusum.observe(sample.raw_value);
        final_upper = u;
        final_lower = l;
        if ca {
            cusum_alarm = true;
        }

        let (prob, ba) = bocpd.observe(sample.raw_value);
        bocpd_prob = prob;
        if ba {
            bocpd_alarm = true;
        }
    }

    let (drift_status, advisory_message) = if cusum_alarm && bocpd_alarm {
        (
            "CRITICAL_DRIFT".to_string(),
            format!(
                "Critical: persistent CUSUM drift (u={final_upper:.2}, l={final_lower:.2}) AND sudden BOCPD regime shift (p={bocpd_prob:.2})"
            ),
        )
    } else if bocpd_alarm {
        (
            "REGIME_SHIFT".to_string(),
            format!(
                "Regime Shift: sudden change-point detected by BOCPD (posterior p={bocpd_prob:.2} >= {})",
                config.bocpd_changepoint_threshold
            ),
        )
    } else if cusum_alarm {
        (
            "DRIFT_WARNING".to_string(),
            format!(
                "Drift Warning: persistent CUSUM trend accumulating (upper={final_upper:.2}, lower={final_lower:.2} >= {})",
                config.cusum_h
            ),
        )
    } else {
        (
            "HEALTHY".to_string(),
            "Nominal: no persistent drift or regime change detected".to_string(),
        )
    };

    Ok(BudgetDriftAnalysis {
        budget_name: series.budget_name.clone(),
        category: series.category.clone(),
        comparison: series.comparison.clone(),
        unit: series.unit.clone(),
        threshold: series.threshold,
        sample_count: series.samples.len(),
        baseline_mean: cusum.baseline_mean(),
        baseline_stddev: cusum.baseline_stddev(),
        latest_value: latest_val,
        cusum_upper: final_upper,
        cusum_lower: final_lower,
        cusum_alarm,
        bocpd_changepoint_prob: bocpd_prob,
        bocpd_alarm,
        drift_status,
        advisory_message,
    })
}

enum DriftIssue<'a> {
    InvalidCount(&'a str, usize),
    InvalidProb(&'a str, f64),
    UnknownStatus(&'a str, &'a str),
}

impl std::fmt::Display for DriftIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCount(name, cnt) => write!(f, "zero samples in budget {name}: {cnt}"),
            Self::InvalidProb(name, p) => {
                write!(f, "invalid BOCPD probability {p} in budget {name}")
            }
            Self::UnknownStatus(name, st) => {
                write!(f, "unknown drift status {st} in budget {name}")
            }
        }
    }
}

pub fn verify_drift_artifact(
    artifact: &PerfDriftWatchArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != DRIFT_WATCH_ARTIFACT_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            DRIFT_WATCH_ARTIFACT_SCHEMA, artifact.schema
        ));
    }

    if artifact.total_budgets != artifact.analyses.len() {
        errors.push(format!(
            "total_budgets mismatch: declared {}, actual {}",
            artifact.total_budgets,
            artifact.analyses.len()
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut healthy = 0;
    let mut warnings = 0;
    let mut regime_shifts = 0;
    let mut issues = Vec::new();

    for analysis in &artifact.analyses {
        if analysis.sample_count == 0 {
            issues.push(DriftIssue::InvalidCount(
                &analysis.budget_name,
                analysis.sample_count,
            ));
        }

        if analysis.bocpd_changepoint_prob < 0.0 || analysis.bocpd_changepoint_prob > 1.0 {
            issues.push(DriftIssue::InvalidProb(
                &analysis.budget_name,
                analysis.bocpd_changepoint_prob,
            ));
        }

        match analysis.drift_status.as_str() {
            "HEALTHY" => healthy += 1,
            "DRIFT_WARNING" => warnings += 1,
            "REGIME_SHIFT" | "CRITICAL_DRIFT" => regime_shifts += 1,
            other => issues.push(DriftIssue::UnknownStatus(&analysis.budget_name, other)),
        }
    }

    errors.extend(issues.into_iter().map(|i| i.to_string()));

    if healthy != artifact.healthy_budgets {
        errors.push(format!(
            "healthy_budgets mismatch: declared {}, actual {}",
            artifact.healthy_budgets, healthy
        ));
    }

    if warnings != artifact.warning_budgets {
        errors.push(format!(
            "warning_budgets mismatch: declared {}, actual {}",
            artifact.warning_budgets, warnings
        ));
    }

    if regime_shifts != artifact.regime_shift_budgets {
        errors.push(format!(
            "regime_shift_budgets mismatch: declared {}, actual {}",
            artifact.regime_shift_budgets, regime_shifts
        ));
    }

    let expected_status = if regime_shifts > 0 {
        "REGIME_SHIFT"
    } else if warnings > 0 {
        "DRIFT_WARNING"
    } else {
        "HEALTHY"
    };

    if artifact.overall_status != expected_status {
        errors.push(format!(
            "overall_status mismatch: expected {expected_status}, got {}",
            artifact.overall_status
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.perf.drift_watch.verification_report.v1".to_string(),
        status,
        evaluated_budgets: artifact.analyses.len(),
        errors,
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<()> {
    let contract_path = Path::new("docs/contracts/drift-watch-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = fs::read_to_string(contract_path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;

    assert_eq!(
        val.get("schema").and_then(|v| v.as_str()),
        Some(DRIFT_WATCH_CONTRACT_SCHEMA)
    );
    assert_eq!(
        val.get("bead_id").and_then(|v| v.as_str()),
        Some("bd-sog97.14")
    );
    assert_eq!(
        val.get("detector_parameters")
            .and_then(|dp| dp.get("cusum_k"))
            .and_then(|k| k.as_f64()),
        Some(0.5)
    );
    assert_eq!(
        val.get("detector_parameters")
            .and_then(|dp| dp.get("cusum_h"))
            .and_then(|h| h.as_f64()),
        Some(4.0)
    );

    Ok(())
}

#[test]
fn cusum_detector_healthy_on_stationary_noise() {
    let mut detector = CusumDetector::new(0.5, 4.0);
    // 50 stationary measurements oscillating around 100 with small variance (std ~ 0.7)
    for i in 0..50 {
        let val = 100.0 + ((i % 5) as f64 - 2.0) * 0.5;
        let (_u, _l, alarmed) = detector.observe(val);
        assert!(
            !alarmed,
            "stationary oscillations should not trigger CUSUM alarm at step {i}"
        );
    }
}

#[test]
fn cusum_detector_alarms_on_persistent_drift() {
    let mut detector = CusumDetector::new(0.5, 4.0);
    // Baseline 20 points around 100
    for _ in 0..20 {
        detector.observe(100.0);
    }
    // Persistent upward drift: 105, 110, 115, 120...
    let mut alarmed = false;
    for i in 1..=20 {
        let (_u, _l, a) = detector.observe(100.0 + (i as f64) * 2.0);
        if a {
            alarmed = true;
            break;
        }
    }
    assert!(
        alarmed,
        "persistent upward drift should trigger CUSUM alarm"
    );
}

#[test]
fn two_sided_cusum_detects_downward_drift() {
    let mut detector = CusumDetector::new(0.5, 4.0);
    // Baseline 20 points around 1000
    for _ in 0..20 {
        detector.observe(1000.0);
    }
    // Persistent downward drift: 980, 960, 940...
    let mut alarmed = false;
    for i in 1..=20 {
        let (_u, l, a) = detector.observe(1000.0 - (i as f64) * 15.0);
        if a && l >= 4.0 {
            alarmed = true;
            break;
        }
    }
    assert!(
        alarmed,
        "persistent downward drift should trigger lower CUSUM alarm"
    );
}

#[test]
fn bocpd_detector_alarms_on_sudden_regime_shift() {
    let mut detector = BocpdDetector::new(50.0, 0.5);
    // Baseline 20 points around 50.0
    for _ in 0..20 {
        detector.observe(50.0);
    }
    // Sudden jump to 120.0
    let (prob1, a1) = detector.observe(120.0);
    let (prob2, a2) = detector.observe(122.0);
    let (prob3, a3) = detector.observe(125.0);

    let max_prob = prob1.max(prob2).max(prob3);
    assert!(
        a1 || a2 || a3 || max_prob >= 0.5,
        "sudden shift should trigger BOCPD change-point alert (max prob = {max_prob})"
    );
}

#[test]
fn live_drift_watch_artifact_passes_verification() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/drift-watch-contract.json");
    assert!(series_path.exists());
    assert!(contract_path.exists());

    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let config = DriftWatchConfig::default();
    let mut analyses = Vec::new();
    let mut healthy = 0;
    let mut warnings = 0;
    let mut shifts = 0;

    for series in &series_input.series {
        let analysis = analyze_series_drift(series, &config)?;
        match analysis.drift_status.as_str() {
            "HEALTHY" => healthy += 1,
            "DRIFT_WARNING" => warnings += 1,
            _ => shifts += 1,
        }
        analyses.push(analysis);
    }

    let overall_status = if shifts > 0 {
        "REGIME_SHIFT".to_string()
    } else if warnings > 0 {
        "DRIFT_WARNING".to_string()
    } else {
        "HEALTHY".to_string()
    };

    let artifact = PerfDriftWatchArtifact {
        schema: DRIFT_WATCH_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        config,
        total_budgets: analyses.len(),
        healthy_budgets: healthy,
        warning_budgets: warnings,
        regime_shift_budgets: shifts,
        overall_status,
        analyses,
    };

    let report = verify_drift_artifact(&artifact, contract_path);
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
fn tamper_detection_in_drift_artifact() -> Result<()> {
    let series_path = Path::new("docs/evidence/nrun-measurement-series.json");
    let contract_path = Path::new("docs/contracts/drift-watch-contract.json");
    let input_text = fs::read_to_string(series_path)?;
    let series_input: NRunSeriesInput = serde_json::from_str(&input_text)?;

    let config = DriftWatchConfig::default();
    let mut analyses = Vec::new();
    for series in &series_input.series {
        analyses.push(analyze_series_drift(series, &config)?);
    }

    let mut artifact = PerfDriftWatchArtifact {
        schema: DRIFT_WATCH_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        config,
        total_budgets: analyses.len(),
        healthy_budgets: 6,
        warning_budgets: 0,
        regime_shift_budgets: 0,
        overall_status: "HEALTHY".to_string(),
        analyses,
    };

    // Tamper count
    artifact.healthy_budgets = 5;
    let report = verify_drift_artifact(&artifact, contract_path);
    assert_eq!(report.status, "fail");
    assert!(!report.errors.is_empty());

    Ok(())
}
