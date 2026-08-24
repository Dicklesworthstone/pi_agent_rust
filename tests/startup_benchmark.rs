#![forbid(unsafe_code)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const STARTUP_CONTRACT_SCHEMA: &str = "pi.perf.startup_benchmark.contract.v1";
pub const STARTUP_ARTIFACT_SCHEMA: &str = "pi.perf.startup_benchmark.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostEnvironmentFingerprint {
    pub os: String,
    pub arch: String,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub mem_total_mb: u64,
    pub noise_score: u8,
    pub git_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandSample {
    pub run_index: usize,
    pub correlation_id: String,
    pub latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandBenchmarkResult {
    pub command: String,
    pub metric_name: String,
    pub threshold_ms: f64,
    pub repetitions: usize,
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub ci95_lower_ms: f64,
    pub ci95_upper_ms: f64,
    pub status: String,
    pub samples: Vec<CommandSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BinarySizeResult {
    pub metric_name: String,
    pub binary_path: String,
    pub size_bytes: u64,
    pub size_mb: f64,
    pub threshold_mb: f64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupBenchmarkReportArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub environment: HostEnvironmentFingerprint,
    pub binary_size: BinarySizeResult,
    pub commands: Vec<CommandBenchmarkResult>,
    pub overall_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_commands: usize,
    pub binary_size_status: String,
    pub errors: Vec<String>,
}

pub fn compute_percentile(sorted_samples: &[f64], p: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let idx = (((sorted_samples.len() as f64) * p).ceil() as usize).clamp(1, sorted_samples.len());
    sorted_samples
        .get(idx.saturating_sub(1))
        .copied()
        .unwrap_or(0.0)
}

pub fn compute_bootstrap_ci(samples: &[f64], resamples: usize) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let n = samples.len();
    let mut state: u64 = 0x1234_5678_9ABC_DEF0;
    let mut next_idx = || -> usize {
        let mut x = state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state = x;
        (x as usize) % n
    };

    let mut means = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..n {
            sum += samples.get(next_idx()).copied().unwrap_or(0.0);
        }
        means.push(sum / (n as f64));
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let lower = compute_percentile(&means, 0.025);
    let upper = compute_percentile(&means, 0.975);
    (lower, upper)
}

enum StartupIssue<'a> {
    LowRepetitions(&'a str, usize),
    ExceedsThreshold(&'a str, f64, f64),
}

impl std::fmt::Display for StartupIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LowRepetitions(cmd, n) => write!(f, "command {cmd} repetitions {n} < min 10"),
            Self::ExceedsThreshold(cmd, p95, t) => {
                write!(f, "command {cmd} p95 {p95:.2}ms exceeds threshold {t:.2}ms")
            }
        }
    }
}

pub fn verify_startup_artifact(
    artifact: &StartupBenchmarkReportArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != STARTUP_ARTIFACT_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            STARTUP_ARTIFACT_SCHEMA, artifact.schema
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    if artifact.binary_size.size_mb > artifact.binary_size.threshold_mb {
        errors.push(format!(
            "binary size {:.2}MB exceeds threshold {:.2}MB",
            artifact.binary_size.size_mb, artifact.binary_size.threshold_mb
        ));
    }

    let mut issues = Vec::new();
    for cmd in &artifact.commands {
        if cmd.repetitions < 10 {
            issues.push(StartupIssue::LowRepetitions(&cmd.command, cmd.repetitions));
        }
        if cmd.p95_ms > cmd.threshold_ms {
            issues.push(StartupIssue::ExceedsThreshold(
                &cmd.command,
                cmd.p95_ms,
                cmd.threshold_ms,
            ));
        }
    }
    errors.extend(issues.into_iter().map(|i| i.to_string()));

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.perf.startup_benchmark.verification_report.v1".to_string(),
        status,
        evaluated_commands: artifact.commands.len(),
        binary_size_status: artifact.binary_size.status.clone(),
        errors,
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<()> {
    let contract_path = Path::new("docs/contracts/startup-benchmark-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = fs::read_to_string(contract_path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;

    assert_eq!(
        val.get("schema").and_then(|v| v.as_str()),
        Some(STARTUP_CONTRACT_SCHEMA)
    );
    assert_eq!(
        val.get("bead_id").and_then(|v| v.as_str()),
        Some("bd-sog97.17")
    );

    Ok(())
}

#[test]
fn percentile_and_bootstrap_ci_math() -> Result<()> {
    let sorted_samples: Vec<f64> = (1..=100).map(|x| x as f64).collect();
    let p95 = compute_percentile(&sorted_samples, 0.95);
    assert_eq!(p95, 95.0);

    let p50 = compute_percentile(&sorted_samples, 0.50);
    assert_eq!(p50, 50.0);

    let (lower, upper) = compute_bootstrap_ci(&sorted_samples, 500);
    assert!(
        lower >= 40.0 && lower <= 55.0,
        "lower {lower} in plausible range"
    );
    assert!(
        upper >= 45.0 && upper <= 60.0,
        "upper {upper} in plausible range"
    );

    Ok(())
}

#[test]
fn mock_artifact_passes_verification() -> Result<()> {
    let contract_path = Path::new("docs/contracts/startup-benchmark-contract.json");

    let artifact = StartupBenchmarkReportArtifact {
        schema: STARTUP_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        environment: HostEnvironmentFingerprint {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            cpu_brand: "AMD EPYC".to_string(),
            cpu_cores: 16,
            mem_total_mb: 64000,
            noise_score: 0,
            git_commit: "380af591".to_string(),
        },
        binary_size: BinarySizeResult {
            metric_name: "binary_size_mb".to_string(),
            binary_path: "target/release/pi".to_string(),
            size_bytes: 43_000_000,
            size_mb: 41.0,
            threshold_mb: 48.0,
            status: "PASS".to_string(),
        },
        commands: vec![
            CommandBenchmarkResult {
                command: "--version".to_string(),
                metric_name: "startup_version_p95".to_string(),
                threshold_ms: 100.0,
                repetitions: 15,
                mean_ms: 10.5,
                p95_ms: 12.1,
                ci95_lower_ms: 10.1,
                ci95_upper_ms: 11.0,
                status: "PASS".to_string(),
                samples: vec![],
            },
            CommandBenchmarkResult {
                command: "--help".to_string(),
                metric_name: "startup_help_p95".to_string(),
                threshold_ms: 150.0,
                repetitions: 15,
                mean_ms: 12.3,
                p95_ms: 14.5,
                ci95_lower_ms: 11.8,
                ci95_upper_ms: 13.0,
                status: "PASS".to_string(),
                samples: vec![],
            },
            CommandBenchmarkResult {
                command: "--list-models".to_string(),
                metric_name: "startup_list_models_p95".to_string(),
                threshold_ms: 200.0,
                repetitions: 15,
                mean_ms: 22.1,
                p95_ms: 25.4,
                ci95_lower_ms: 21.0,
                ci95_upper_ms: 23.5,
                status: "PASS".to_string(),
                samples: vec![],
            },
        ],
        overall_status: "PASS".to_string(),
    };

    let report = verify_startup_artifact(&artifact, contract_path);
    assert_eq!(report.status, "pass");
    assert_eq!(report.evaluated_commands, 3);
    assert_eq!(report.binary_size_status, "PASS");
    assert!(report.errors.is_empty());

    Ok(())
}

#[test]
fn tamper_detection_in_startup_artifact() -> Result<()> {
    let contract_path = Path::new("docs/contracts/startup-benchmark-contract.json");

    let artifact = StartupBenchmarkReportArtifact {
        schema: STARTUP_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        environment: HostEnvironmentFingerprint {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            cpu_brand: "AMD EPYC".to_string(),
            cpu_cores: 16,
            mem_total_mb: 64000,
            noise_score: 0,
            git_commit: "380af591".to_string(),
        },
        binary_size: BinarySizeResult {
            metric_name: "binary_size_mb".to_string(),
            binary_path: "target/release/pi".to_string(),
            size_bytes: 55_000_000,
            size_mb: 52.45,
            threshold_mb: 48.0,
            status: "FAIL".to_string(),
        },
        commands: vec![CommandBenchmarkResult {
            command: "--version".to_string(),
            metric_name: "startup_version_p95".to_string(),
            threshold_ms: 100.0,
            repetitions: 5, // under minimum 10
            mean_ms: 10.5,
            p95_ms: 112.1, // exceeds 100.0
            ci95_lower_ms: 10.1,
            ci95_upper_ms: 11.0,
            status: "FAIL".to_string(),
            samples: vec![],
        }],
        overall_status: "FAIL".to_string(),
    };

    let report = verify_startup_artifact(&artifact, contract_path);
    assert_eq!(report.status, "fail");
    assert!(report.errors.len() >= 3);

    Ok(())
}
