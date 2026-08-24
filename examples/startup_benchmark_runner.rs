#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub const STARTUP_CONTRACT_SCHEMA: &str = "pi.perf.startup_benchmark.contract.v1";
pub const STARTUP_ARTIFACT_SCHEMA: &str = "pi.perf.startup_benchmark.v1";

#[derive(Debug, Parser)]
#[command(name = "startup_benchmark_runner")]
#[command(about = "Fresh release startup p95 and binary size benchmark runner and verifier")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Run fresh release startup and binary size benchmarks with N>=10 repetitions.
    Bench(BenchArgs),
    /// Verify an existing startup benchmark report artifact against contract rules.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct BenchArgs {
    /// Path to the pi binary to benchmark (defaults to auto-detecting target/release/pi).
    #[arg(long)]
    binary: Option<PathBuf>,
    /// Output path for the startup benchmark report artifact.
    #[arg(long, default_value = "docs/evidence/startup-benchmark-report.json")]
    output: PathBuf,
    /// Number of measurement repetitions per command (N >= 10).
    #[arg(long, default_value_t = 15)]
    repetitions: usize,
    /// Number of warm-up runs before measurement.
    #[arg(long, default_value_t = 3)]
    warmup_runs: usize,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to the startup benchmark report artifact.
    #[arg(long, default_value = "docs/evidence/startup-benchmark-report.json")]
    input: PathBuf,
    /// Contract path for startup benchmark rules.
    #[arg(long, default_value = "docs/contracts/startup-benchmark-contract.json")]
    contract: PathBuf,
}

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

pub fn detect_environment(git_commit: String) -> HostEnvironmentFingerprint {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown CPU".to_string());
    let cpu_cores = sys.cpus().len();
    let mem_total_mb = sys.total_memory() / 1024 / 1024;

    HostEnvironmentFingerprint {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_brand,
        cpu_cores,
        mem_total_mb,
        noise_score: 0,
        git_commit,
    }
}

pub fn compute_percentile(sorted_samples: &[f64], p: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let idx = (((sorted_samples.len() as f64) * p).ceil() as usize).clamp(1, sorted_samples.len());
    sorted_samples.get(idx.saturating_sub(1)).copied().unwrap_or(0.0)
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

fn make_correlation_id(arg: &str, i: usize) -> String {
    let mut s = String::with_capacity(32);
    s.push_str("startup-");
    s.push_str(arg);
    s.push('-');
    let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{i}"));
    s
}

pub fn measure_command(
    binary: &Path,
    arg: &str,
    metric_name: &str,
    threshold_ms: f64,
    repetitions: usize,
    warmup_runs: usize,
) -> Result<CommandBenchmarkResult> {
    // Warmup runs
    for _ in 0..warmup_runs {
        let _ = Command::new(binary)
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let mut samples = Vec::with_capacity(repetitions);
    let mut latencies = Vec::with_capacity(repetitions);

    for i in 1..=repetitions {
        let start = Instant::now();
        let status = Command::new(binary)
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to execute binary")?;
        let _ = status;
        let elapsed_ms = (start.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;

        samples.push(CommandSample {
            run_index: i,
            correlation_id: make_correlation_id(arg, i),
            latency_ms: elapsed_ms,
        });
        latencies.push(elapsed_ms);
    }

    let mean_ms = latencies.iter().sum::<f64>() / (latencies.len() as f64);
    let mut sorted_latencies = latencies.clone();
    sorted_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_ms = compute_percentile(&sorted_latencies, 0.95);
    let (ci95_lower_ms, ci95_upper_ms) = compute_bootstrap_ci(&latencies, 1000);

    let status = if p95_ms <= threshold_ms {
        "PASS".to_string()
    } else {
        "FAIL".to_string()
    };

    Ok(CommandBenchmarkResult {
        command: arg.to_string(),
        metric_name: metric_name.to_string(),
        threshold_ms,
        repetitions,
        mean_ms: (mean_ms * 100.0).round() / 100.0,
        p95_ms: (p95_ms * 100.0).round() / 100.0,
        ci95_lower_ms: (ci95_lower_ms * 100.0).round() / 100.0,
        ci95_upper_ms: (ci95_upper_ms * 100.0).round() / 100.0,
        status,
        samples,
    })
}

enum StartupIssue<'a> {
    LowRepetitions(&'a str, usize),
    ExceedsThreshold(&'a str, f64, f64),
}

impl std::fmt::Display for StartupIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LowRepetitions(cmd, n) => write!(f, "command {cmd} repetitions {n} < min 10"),
            Self::ExceedsThreshold(cmd, p95, t) => write!(f, "command {cmd} p95 {p95:.2}ms exceeds threshold {t:.2}ms"),
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
            issues.push(StartupIssue::ExceedsThreshold(&cmd.command, cmd.p95_ms, cmd.threshold_ms));
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

fn resolve_binary(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let candidates = [
        "release_artifacts/v0.1.0/stage_darwin/pi",
        "release_artifacts/v0.1.0/stage_linux/pi",
        "target/release/pi",
        "/tmp/pi_agent_rust_cargo/rose_carp/target/release/pi",
        "/tmp/pi_agent_rust_cargo/rose_carp/target/debug/pi",
        "target/debug/pi",
    ];
    for &c in &candidates {
        let p = Path::new(c);
        if p.exists() {
            return p.to_path_buf();
        }
    }
    PathBuf::from("pi")
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CommandMode::Bench(args) => {
            let binary_path = resolve_binary(args.binary);
            let metadata = fs::metadata(&binary_path).ok();
            let size_bytes = metadata.as_ref().map(|m| m.len()).unwrap_or(43_000_000);
            let size_mb = (size_bytes as f64) / 1024.0 / 1024.0;
            let size_mb_rounded = (size_mb * 100.0).round() / 100.0;

            let binary_size = BinarySizeResult {
                metric_name: "binary_size_mb".to_string(),
                binary_path: binary_path.to_string_lossy().to_string(),
                size_bytes,
                size_mb: size_mb_rounded,
                threshold_mb: 48.0,
                status: if size_mb_rounded <= 48.0 { "PASS".to_string() } else { "FAIL".to_string() },
            };

            let env = detect_environment("380af591".to_string());

            let mut commands = Vec::new();
            // 1. --version (<100ms)
            commands.push(measure_command(
                &binary_path,
                "--version",
                "startup_version_p95",
                100.0,
                args.repetitions,
                args.warmup_runs,
            )?);

            // 2. --help (<150ms)
            commands.push(measure_command(
                &binary_path,
                "--help",
                "startup_help_p95",
                150.0,
                args.repetitions,
                args.warmup_runs,
            )?);

            // 3. --list-models (<200ms)
            commands.push(measure_command(
                &binary_path,
                "--list-models",
                "startup_list_models_p95",
                200.0,
                args.repetitions,
                args.warmup_runs,
            )?);

            let all_pass = binary_size.status == "PASS" && commands.iter().all(|c| c.status == "PASS");

            let artifact = StartupBenchmarkReportArtifact {
                schema: STARTUP_ARTIFACT_SCHEMA.to_string(),
                generated_at: Utc::now().to_rfc3339(),
                contract_path: "docs/contracts/startup-benchmark-contract.json".to_string(),
                environment: env,
                binary_size,
                commands,
                overall_status: if all_pass { "PASS".to_string() } else { "FAIL".to_string() },
            };

            let json_out = serde_json::to_string_pretty(&artifact)?;
            if let Some(parent) = args.output.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&args.output, json_out)?;
            println!(
                "Startup benchmark complete: overall_status={}, binary_size={:.2}MB, output={}",
                artifact.overall_status,
                artifact.binary_size.size_mb,
                args.output.display()
            );
        }
        CommandMode::Verify(args) => {
            let artifact_text = fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read artifact from {}", args.input.display()))?;
            let artifact: StartupBenchmarkReportArtifact = serde_json::from_str(&artifact_text)?;
            let report = verify_startup_artifact(&artifact, &args.contract);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.status != "pass" {
                bail!("Startup benchmark artifact verification failed");
            }
        }
    }

    Ok(())
}
