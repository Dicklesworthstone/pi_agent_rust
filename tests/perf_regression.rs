//! Performance regression test suite (bd-1f42.5.3).
//!
//! Automated regression tests for startup latency, idle memory, binary size,
//! and interactive responsiveness against explicit thresholds from PERF_BUDGETS.md.
//!
//! Each test:
//! - Measures actual performance against stated budget thresholds
//! - Emits structured JSONL artifacts with hardware/context metadata
//! - Compares against stored baselines for trend delta detection
//! - Fails the build on CI-enforced budget violations
//!
//! Run:
//!   cargo test --test perf_regression -- --nocapture
//!   PERF_REGRESSION_FULL=1 cargo test --test perf_regression -- --nocapture

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names
)]

mod common;

use chrono::{SecondsFormat, Utc};
use common::harness::TestHarness;
use pi::perf_build;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use sysinfo::System;

// ─── Configuration ──────────────────────────────────────────────────────────

/// Serialize perf-sensitive tests to avoid scheduler noise.
static PERF_LOCK: Mutex<()> = Mutex::new(());
const PERF_RELEASE_BINARY_PATH_ENV: &str = "PERF_RELEASE_BINARY_PATH";
const PI_PERF_STRICT_ENV: &str = "PI_PERF_STRICT";

fn recover_poisoned_mutex_guard<'a, T>(
    lock: &'a Mutex<T>,
    warning_message: &str,
) -> std::sync::MutexGuard<'a, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            // Once we intentionally recover, clear poison so later checks don't
            // repeatedly report the same stale failure state.
            lock.clear_poison();
            eprintln!("{warning_message}");
            poisoned.into_inner()
        }
    }
}

fn perf_guard() -> std::sync::MutexGuard<'static, ()> {
    recover_poisoned_mutex_guard(
        &PERF_LOCK,
        "[perf_regression] WARN: perf test lock poisoned; continuing",
    )
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR").ok().map_or_else(
        || project_root().join("target"),
        |dir| {
            let target_dir = PathBuf::from(dir);
            if target_dir.is_absolute() {
                target_dir
            } else {
                project_root().join(target_dir)
            }
        },
    )
}

fn output_dir() -> PathBuf {
    let base = std::env::var("PERF_REGRESSION_OUTPUT")
        .ok()
        .map_or_else(|| target_dir().join("perf"), PathBuf::from);
    std::fs::create_dir_all(&base).expect("create perf regression output dir");
    base
}

fn baseline_dir() -> PathBuf {
    project_root().join("tests/perf/reports")
}

fn is_full_mode() -> bool {
    std::env::var("PERF_REGRESSION_FULL").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn perf_strict_mode_from(raw: Option<&str>) -> bool {
    raw.map(str::trim)
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn perf_strict_mode() -> bool {
    let strict_raw = std::env::var(PI_PERF_STRICT_ENV).ok();
    perf_strict_mode_from(strict_raw.as_deref())
}

/// Number of startup measurement iterations.
fn startup_runs() -> usize {
    if is_full_mode() { 15 } else { 7 }
}

/// Warmup runs before measurement.
fn warmup_runs() -> usize {
    if is_full_mode() { 5 } else { 2 }
}

fn format_candidate_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_pi_binary_candidates(
    target_dir: &Path,
    cargo_bin_override: Option<PathBuf>,
    detected_profile: &str,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let normalized_profile = detected_profile.trim();

    if let Some(path) = cargo_bin_override {
        candidates.push(path);
    }

    if !normalized_profile.is_empty() {
        candidates.push(target_dir.join(normalized_profile).join("pi"));
    }

    candidates.push(target_dir.join("release/pi"));
    candidates.push(target_dir.join("perf/pi"));
    candidates.push(target_dir.join("debug/pi"));

    let mut dedup = HashSet::new();
    candidates.retain(|path| dedup.insert(path.clone()));
    candidates
}

fn pi_binary_candidates() -> Vec<PathBuf> {
    let target_dir = target_dir();
    let cargo_bin_override = std::env::var_os("CARGO_BIN_EXE_pi").map(PathBuf::from);
    let detected_profile = perf_build::detect_build_profile();
    build_pi_binary_candidates(&target_dir, cargo_bin_override, &detected_profile)
}

/// Find the first available `pi` binary from test/build-profile candidates.
fn pi_binary() -> Option<PathBuf> {
    first_existing_candidate(pi_binary_candidates())
}

fn binary_size_release_override() -> Option<PathBuf> {
    std::env::var(PERF_RELEASE_BINARY_PATH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn build_binary_size_candidates(
    target_dir: &Path,
    release_binary_override: Option<PathBuf>,
    _detected_profile: &str,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = release_binary_override {
        candidates.push(path);
    }
    // Budget methodology is explicitly release-only; do not fall back to perf/debug.
    candidates.push(target_dir.join("release/pi"));

    let mut dedup = HashSet::new();
    candidates.retain(|path| dedup.insert(path.clone()));
    candidates
}

fn binary_size_candidates() -> Vec<PathBuf> {
    let target_dir = target_dir();
    let release_binary_override = binary_size_release_override();
    let detected_profile = perf_build::detect_build_profile();
    build_binary_size_candidates(&target_dir, release_binary_override, &detected_profile)
}

fn binary_size_binary() -> Option<PathBuf> {
    first_existing_candidate(binary_size_candidates())
}

fn binary_size_missing_release_outcome(strict_mode: bool, checked: &str) -> Result<(), String> {
    if strict_mode {
        Err(format!(
            "release binary not found (checked: {checked}); strict mode requires a release artifact via {PERF_RELEASE_BINARY_PATH_ENV} or target/release/pi"
        ))
    } else {
        Ok(())
    }
}

fn first_existing_candidate(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| candidate.exists())
}

// ─── Environment Fingerprint ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvFingerprint {
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    mem_total_mb: u64,
    build_profile: String,
    allocator_requested: String,
    allocator_effective: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    allocator_fallback_reason: Option<String>,
    git_commit: String,
    config_hash: String,
}

fn collect_fingerprint() -> EnvFingerprint {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();

    let cpu_model = system
        .cpus()
        .first()
        .map_or_else(|| "unknown".to_string(), |cpu| cpu.brand().to_string());
    let cpu_cores = system.cpus().len() as u32;
    let mem_total_mb = system.total_memory() / (1024 * 1024);
    let os = System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string());
    let arch = std::env::consts::ARCH.to_string();
    let build_profile = perf_build::detect_build_profile();
    let allocator = perf_build::resolve_bench_allocator();
    let git_commit = option_env!("VERGEN_GIT_SHA")
        .unwrap_or("unknown")
        .to_string();

    let config_str = format!(
        "os={os} arch={arch} cpu={cpu_model} cores={cpu_cores} mem={mem_total_mb} profile={build_profile} allocator={}",
        allocator.effective.as_str()
    );
    let config_hash = sha256_short(&config_str);

    EnvFingerprint {
        os,
        arch,
        cpu_model,
        cpu_cores,
        mem_total_mb,
        build_profile,
        allocator_requested: allocator.requested,
        allocator_effective: allocator.effective.as_str().to_string(),
        allocator_fallback_reason: allocator.fallback_reason,
        git_commit,
        config_hash,
    }
}

fn sha256_short(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{result:x}")[..16].to_string()
}

// ─── Statistics ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LatencyStats {
    count: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    stddev_ms: f64,
}

fn compute_stats(samples_ms: &[f64]) -> LatencyStats {
    if samples_ms.is_empty() {
        return LatencyStats {
            count: 0,
            min_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
            mean_ms: 0.0,
            stddev_ms: 0.0,
        };
    }

    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let count = sorted.len();
    let sum: f64 = sorted.iter().sum();
    let mean = sum / count as f64;
    let variance = sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / count as f64;
    let stddev = variance.sqrt();

    LatencyStats {
        count,
        min_ms: sorted[0],
        p50_ms: percentile(&sorted, 50.0),
        p95_ms: percentile(&sorted, 95.0),
        p99_ms: percentile(&sorted, 99.0),
        max_ms: sorted[count - 1],
        mean_ms: mean,
        stddev_ms: stddev,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ─── JSONL Records ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfRecord {
    schema: String,
    test: String,
    category: String,
    budget_name: String,
    budget_threshold: f64,
    budget_unit: String,
    actual_value: f64,
    status: String,
    stats: Option<LatencyStats>,
    env: EnvFingerprint,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_pct: Option<f64>,
}

fn emit_record(record: &PerfRecord) -> String {
    serde_json::to_string(record).unwrap_or_default()
}

fn check_threshold(actual: f64, threshold: f64, higher_is_better: bool) -> &'static str {
    if higher_is_better {
        if actual >= threshold { "PASS" } else { "FAIL" }
    } else if actual <= threshold {
        "PASS"
    } else {
        "FAIL"
    }
}

// ─── Baseline Comparison ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaselineEntry {
    budget_name: String,
    value: f64,
    env_hash: String,
    timestamp: String,
}

fn read_baseline(budget_name: &str) -> Option<BaselineEntry> {
    let path = baseline_dir().join("regression_baseline.json");
    let content = std::fs::read_to_string(path).ok()?;
    let baselines: Vec<BaselineEntry> = serde_json::from_str(&content).ok()?;
    baselines.into_iter().find(|b| b.budget_name == budget_name)
}

fn compute_delta(actual: f64, baseline: Option<&BaselineEntry>) -> (Option<f64>, Option<f64>) {
    baseline.map_or((None, None), |b| {
        let delta_pct = if b.value > 0.0 {
            Some(((actual - b.value) / b.value) * 100.0)
        } else {
            None
        };
        (Some(b.value), delta_pct)
    })
}

/// Regression threshold: fail if actual exceeds baseline by this percentage.
const REGRESSION_THRESHOLD_PCT: f64 = 25.0;

// ─── Startup Latency Tests ─────────────────────────────────────────────────

fn perf_binary_command(binary: &Path) -> Command {
    let command = Command::new(binary); // ubs:ignore false positive: perf harness binary path is resolved from release_binary_path/fixture override, not request input.
    command
}

/// Measure subprocess startup time for a given set of arguments.
fn measure_startup(binary: &Path, args: &[&str], runs: usize, warmup: usize) -> Vec<f64> {
    // Warmup runs (discard)
    for _ in 0..warmup {
        let _ = perf_binary_command(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }

    // Measurement runs
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let result = perf_binary_command(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let elapsed = start.elapsed();

        if let Ok(output) = result
            && output.status.success()
        {
            samples.push(elapsed.as_secs_f64() * 1000.0);
        }
    }
    samples
}

#[test]
fn startup_version_latency() {
    let _guard = perf_guard();
    let harness = TestHarness::new("startup_version_latency");

    let Some(binary) = pi_binary() else {
        let candidates = pi_binary_candidates();
        let checked = format_candidate_paths(&candidates);
        harness.log().info_ctx(
            "skip",
            "pi binary not found; skipping startup latency test",
            |ctx| {
                ctx.push(("checked_candidates".into(), checked.clone()));
            },
        );
        eprintln!("[perf_regression] SKIP: pi binary not found (checked: {checked})");
        return;
    };

    harness
        .log()
        .info_ctx("measure", "Measuring --version startup", |ctx| {
            ctx.push(("binary".into(), binary.display().to_string()));
            ctx.push(("runs".into(), startup_runs().to_string()));
            ctx.push(("warmup".into(), warmup_runs().to_string()));
        });

    let samples = measure_startup(&binary, &["--version"], startup_runs(), warmup_runs());
    assert!(
        !samples.is_empty(),
        "pi --version produced no successful runs"
    );

    let stats = compute_stats(&samples);
    let p95 = stats.p95_ms;
    let threshold = 100.0; // 100ms budget

    // Debug builds are ~5-10x slower; relax threshold
    let effective_threshold = if cfg!(debug_assertions) {
        threshold * 10.0
    } else {
        threshold
    };

    let status = check_threshold(p95, effective_threshold, false);
    let baseline = read_baseline("startup_version_p95");
    let (baseline_val, delta_pct) = compute_delta(p95, baseline.as_ref());

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "startup_version_latency".to_string(),
        category: "startup".to_string(),
        budget_name: "startup_version_p95".to_string(),
        budget_threshold: effective_threshold,
        budget_unit: "ms".to_string(),
        actual_value: p95,
        status: status.to_string(),
        stats: Some(stats.clone()),
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: baseline_val,
        delta_pct,
    };

    let out_path = output_dir().join("perf_regression.jsonl");
    append_jsonl(&out_path, &emit_record(&record));

    eprintln!("\n=== Startup --version Latency ===");
    eprintln!("  Runs:      {}", stats.count);
    eprintln!("  P50:       {:.1}ms", stats.p50_ms);
    eprintln!("  P95:       {p95:.1}ms (budget: {effective_threshold:.0}ms)");
    eprintln!("  P99:       {:.1}ms", stats.p99_ms);
    eprintln!("  Mean:      {:.1}ms", stats.mean_ms);
    eprintln!("  Stddev:    {:.2}ms", stats.stddev_ms);
    if let Some(delta) = delta_pct {
        eprintln!("  Delta:     {delta:+.1}% vs baseline");
    }
    eprintln!("  Status:    {status}");

    // Check for regression against baseline
    if let Some(delta) = delta_pct
        && delta > REGRESSION_THRESHOLD_PCT
    {
        eprintln!(
            "  WARNING: {delta:+.1}% regression exceeds {REGRESSION_THRESHOLD_PCT}% threshold"
        );
    }

    assert_eq!(
        status, "PASS",
        "startup_version_p95={p95:.1}ms exceeds budget {effective_threshold:.0}ms"
    );
}

#[test]
fn startup_help_latency() {
    let _guard = perf_guard();
    let harness = TestHarness::new("startup_help_latency");

    let Some(binary) = pi_binary() else {
        let checked = format_candidate_paths(&pi_binary_candidates());
        harness
            .log()
            .info_ctx("skip", "pi binary not found", |ctx| {
                ctx.push(("checked_candidates".into(), checked.clone()));
            });
        eprintln!("[perf_regression] SKIP: pi binary not found (checked: {checked})");
        return;
    };

    harness.log().info("measure", "Measuring --help startup");

    let samples = measure_startup(&binary, &["--help"], startup_runs(), warmup_runs());
    assert!(!samples.is_empty(), "pi --help produced no successful runs");

    let stats = compute_stats(&samples);
    let p95 = stats.p95_ms;

    // --help is slightly heavier than --version; use 150ms budget
    let threshold = if cfg!(debug_assertions) {
        1500.0
    } else {
        150.0
    };
    let status = check_threshold(p95, threshold, false);

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "startup_help_latency".to_string(),
        category: "startup".to_string(),
        budget_name: "startup_help_p95".to_string(),
        budget_threshold: threshold,
        budget_unit: "ms".to_string(),
        actual_value: p95,
        status: status.to_string(),
        stats: Some(stats),
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: None,
        delta_pct: None,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Startup --help Latency ===");
    eprintln!("  P95: {p95:.1}ms (budget: {threshold:.0}ms) — {status}");

    assert_eq!(
        status, "PASS",
        "startup_help_p95={p95:.1}ms exceeds {threshold:.0}ms"
    );
}

// ─── Memory Tests ───────────────────────────────────────────────────────────

#[test]
fn idle_memory_rss() {
    let _guard = perf_guard();
    let harness = TestHarness::new("idle_memory_rss");

    let Some(binary) = pi_binary() else {
        let checked = format_candidate_paths(&pi_binary_candidates());
        harness
            .log()
            .info_ctx("skip", "pi binary not found", |ctx| {
                ctx.push(("checked_candidates".into(), checked.clone()));
            });
        eprintln!("[perf_regression] SKIP: pi binary not found (checked: {checked})");
        return;
    };

    harness
        .log()
        .info("measure", "Measuring idle RSS of pi process");

    // Spawn pi --version and measure its peak RSS
    // We use /usr/bin/time if available for accurate maxrss
    let rss_mb = measure_process_rss(&binary, &["--version"]);

    let threshold = 50.0; // 50MB budget
    let status = check_threshold(rss_mb, threshold, false);

    let baseline = read_baseline("idle_memory_rss");
    let (baseline_val, delta_pct) = compute_delta(rss_mb, baseline.as_ref());

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "idle_memory_rss".to_string(),
        category: "memory".to_string(),
        budget_name: "idle_memory_rss".to_string(),
        budget_threshold: threshold,
        budget_unit: "MB".to_string(),
        actual_value: rss_mb,
        status: status.to_string(),
        stats: None,
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: baseline_val,
        delta_pct,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Idle Memory RSS ===");
    eprintln!("  RSS:       {rss_mb:.1}MB (budget: {threshold:.0}MB)");
    if let Some(delta) = delta_pct {
        eprintln!("  Delta:     {delta:+.1}% vs baseline");
    }
    eprintln!("  Status:    {status}");

    assert_eq!(
        status, "PASS",
        "idle_memory_rss={rss_mb:.1}MB exceeds {threshold:.0}MB"
    );
}

/// Measure RSS of a short-lived child process using /proc on Linux.
fn measure_process_rss(binary: &Path, args: &[&str]) -> f64 {
    // Spawn the process and read /proc/<pid>/status for VmRSS
    let child = perf_binary_command(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let Ok(mut child) = child else {
        return 0.0;
    };

    let pid = child.id();

    // Sample RSS from /proc a few times while process runs
    let mut max_rss_kb: u64 = 0;
    for _ in 0..20 {
        if let Ok(status_content) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status_content.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let trimmed = rest.trim().trim_end_matches("kB").trim();
                    if let Ok(kb) = trimmed.parse::<u64>() {
                        max_rss_kb = max_rss_kb.max(kb);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let _ = child.wait();
    max_rss_kb as f64 / 1024.0
}

fn exercise_sustained_memory_load() -> u64 {
    let mut accumulator: u64 = 0;
    for i in 0..100 {
        let data: Vec<u64> = (0..10_000).map(|j| j * (i + 1)).collect();
        accumulator = accumulator.wrapping_add(data.iter().sum::<u64>());
        std::hint::black_box(&data);
    }
    std::hint::black_box(accumulator)
}

#[test]
fn memory_sustained_load_growth() {
    let _guard = perf_guard();
    let harness = TestHarness::new("memory_sustained_load_growth");

    harness
        .log()
        .info("measure", "Measuring RSS growth under allocation pressure");

    // Warm the allocator before the baseline so this checks retained growth
    // under repeat load, not first-touch allocator expansion.
    std::hint::black_box(exercise_sustained_memory_load());

    // Measure test-process RSS before and after allocating + processing data
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = System::new();

    // Baseline RSS
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    let rss_before = system.process(pid).map_or(0, sysinfo::Process::memory);

    // Simulate sustained load: allocate and process vectors repeatedly.
    std::hint::black_box(exercise_sustained_memory_load());

    // Post-load RSS
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    let rss_after = system.process(pid).map_or(0, sysinfo::Process::memory);

    let growth_pct = if rss_before > 0 {
        ((rss_after as f64 - rss_before as f64) / rss_before as f64) * 100.0
    } else {
        0.0
    };

    let threshold = 5.0; // 5% growth budget
    let status = check_threshold(growth_pct.max(0.0), threshold, false);

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "memory_sustained_load_growth".to_string(),
        category: "memory".to_string(),
        budget_name: "sustained_load_rss_growth".to_string(),
        budget_threshold: threshold,
        budget_unit: "percent".to_string(),
        actual_value: growth_pct.max(0.0),
        status: status.to_string(),
        stats: None,
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: None,
        delta_pct: None,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Memory Sustained Load Growth ===");
    eprintln!("  Before:    {:.1}MB", rss_before as f64 / 1024.0 / 1024.0);
    eprintln!("  After:     {:.1}MB", rss_after as f64 / 1024.0 / 1024.0);
    eprintln!("  Growth:    {growth_pct:.1}% (budget: {threshold:.0}%)");
    eprintln!("  Status:    {status}");

    assert_eq!(
        status, "PASS",
        "sustained_load_rss_growth={growth_pct:.1}% exceeds {threshold:.0}%"
    );
}

// ─── Binary Size Test ───────────────────────────────────────────────────────

#[test]
fn binary_size_check() {
    let _guard = perf_guard();
    let harness = TestHarness::new("binary_size_check");

    let Some(release_path) = binary_size_binary() else {
        let checked = format_candidate_paths(&binary_size_candidates());
        let strict_mode = perf_strict_mode();
        match binary_size_missing_release_outcome(strict_mode, &checked) {
            Ok(()) => {
                harness.log().info("skip", "release binary not found");
                eprintln!("[perf_regression] SKIP: release binary not found (checked: {checked})");
                return;
            }
            Err(err) => {
                harness.log().info(
                    "missing_release_binary",
                    format!("strict_mode=true env={PI_PERF_STRICT_ENV} checked={checked}"),
                );
                assert!(!strict_mode, "{err}");
                return;
            }
        }
    };

    let meta = std::fs::metadata(&release_path).expect("stat release binary");
    let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
    let threshold = perf_build::BINARY_SIZE_RELEASE_BUDGET_MB;
    let status = check_threshold(size_mb, threshold, false);

    let baseline = read_baseline("binary_size_release");
    let (baseline_val, delta_pct) = compute_delta(size_mb, baseline.as_ref());

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "binary_size_check".to_string(),
        category: "binary".to_string(),
        budget_name: "binary_size_release".to_string(),
        budget_threshold: threshold,
        budget_unit: "MB".to_string(),
        actual_value: size_mb,
        status: status.to_string(),
        stats: None,
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: baseline_val,
        delta_pct,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Binary Size ===");
    eprintln!("  Size:      {size_mb:.1}MB (budget: {threshold:.0}MB)");
    if let Some(delta) = delta_pct {
        eprintln!("  Delta:     {delta:+.1}% vs baseline");
    }
    eprintln!("  Status:    {status}");

    assert_eq!(
        status, "PASS",
        "binary_size={size_mb:.1}MB exceeds {threshold:.0}MB"
    );
}

// ─── Protocol Parse Latency ─────────────────────────────────────────────────

#[test]
fn protocol_parse_latency() {
    let _guard = perf_guard();
    let harness = TestHarness::new("protocol_parse_latency");

    harness
        .log()
        .info("measure", "Measuring JSON protocol parse latency");

    // Simulate extension protocol message parsing
    let host_call_msg = r#"{"type":"host_call","id":"hc-1","method":"log","params":{"level":"info","message":"hello world","context":{"key":"value","nested":{"a":1,"b":"test"}}}}"#;
    let log_msg = r#"{"type":"log","level":"debug","message":"Extension loaded successfully","extension_id":"ext-hello","timestamp":"2026-01-01T00:00:00Z"}"#;
    let register_msg = r#"{"type":"register","extension_id":"ext-test","tools":[{"name":"greet","description":"Greet the user","parameters":{"type":"object","properties":{"name":{"type":"string"}}}}],"event_hooks":["before_agent_start","after_tool_call"]}"#;

    let messages = [host_call_msg, log_msg, register_msg];
    let iterations = 5000;
    let mut all_us: Vec<f64> = Vec::with_capacity(iterations * messages.len());

    // Warmup
    for _ in 0..100 {
        for msg in &messages {
            let _: Value = serde_json::from_str(msg).unwrap();
        }
    }

    // Measure
    for _ in 0..iterations {
        for msg in &messages {
            let start = Instant::now();
            let _: Value = serde_json::from_str(msg).unwrap();
            let elapsed = start.elapsed();
            all_us.push(elapsed.as_nanos() as f64 / 1000.0); // ns → us
        }
    }

    all_us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p99_us = percentile(&all_us, 99.0);

    let threshold = 50.0; // 50us budget
    // Debug builds are much slower at JSON parsing
    let effective_threshold = if cfg!(debug_assertions) {
        threshold * 20.0
    } else {
        threshold
    };

    let status = check_threshold(p99_us, effective_threshold, false);

    let stats_ms: Vec<f64> = all_us.iter().map(|us| us / 1000.0).collect();
    let stats = compute_stats(&stats_ms);

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "protocol_parse_latency".to_string(),
        category: "protocol".to_string(),
        budget_name: "protocol_parse_p99".to_string(),
        budget_threshold: effective_threshold,
        budget_unit: "us".to_string(),
        actual_value: p99_us,
        status: status.to_string(),
        stats: Some(stats),
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: None,
        delta_pct: None,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Protocol Parse Latency ===");
    eprintln!("  Iterations: {} x {} messages", iterations, messages.len());
    eprintln!("  P99:        {p99_us:.1}us (budget: {effective_threshold:.0}us)");
    eprintln!("  Status:     {status}");

    assert_eq!(
        status, "PASS",
        "protocol_parse_p99={p99_us:.1}us exceeds {effective_threshold:.0}us"
    );
}

// ─── SSE Parse Throughput ───────────────────────────────────────────────────

#[test]
fn sse_parse_throughput() {
    let _guard = perf_guard();
    let harness = TestHarness::new("sse_parse_throughput");

    harness
        .log()
        .info("measure", "Measuring SSE event parse throughput");

    // Construct realistic SSE data
    let mut sse_data = String::with_capacity(64 * 1024);
    for i in 0..500 {
        let _ = writeln!(
            sse_data,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"word{i} \"}}}}\n"
        );
    }
    let _ = writeln!(
        sse_data,
        "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n"
    );

    let iterations = 200;
    let mut parse_times_ms: Vec<f64> = Vec::with_capacity(iterations);

    // Warmup
    for _ in 0..10 {
        let count = sse_data.lines().filter(|l| l.starts_with("data: ")).count();
        std::hint::black_box(count);
    }

    // Measure
    for _ in 0..iterations {
        let start = Instant::now();
        let mut parsed = 0usize;
        for line in sse_data.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                let _: Result<Value, _> = serde_json::from_str(data);
                parsed += 1;
            }
        }
        let elapsed = start.elapsed();
        std::hint::black_box(parsed);
        parse_times_ms.push(elapsed.as_secs_f64() * 1000.0);
    }

    let stats = compute_stats(&parse_times_ms);
    let events_per_sec = if stats.mean_ms > 0.0 {
        501.0 / (stats.mean_ms / 1000.0)
    } else {
        0.0
    };

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "sse_parse_throughput".to_string(),
        category: "protocol".to_string(),
        budget_name: "sse_parse_throughput".to_string(),
        budget_threshold: 10000.0, // 10k events/sec minimum
        budget_unit: "events/sec".to_string(),
        actual_value: events_per_sec,
        status: check_threshold(events_per_sec, 10000.0, true).to_string(),
        stats: Some(stats.clone()),
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: None,
        delta_pct: None,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== SSE Parse Throughput ===");
    eprintln!("  Events/sec: {events_per_sec:.0}");
    eprintln!("  Mean parse: {:.2}ms (501 events)", stats.mean_ms);
    eprintln!("  Status:     {}", record.status);

    // SSE throughput should easily exceed 10k events/sec even in debug
    assert!(
        events_per_sec > 1000.0,
        "SSE parse throughput {events_per_sec:.0} events/sec is suspiciously low"
    );
}

// ─── Config Parse Latency ───────────────────────────────────────────────────

#[test]
fn config_parse_latency() {
    let _guard = perf_guard();
    let harness = TestHarness::new("config_parse_latency");

    harness
        .log()
        .info("measure", "Measuring config file parse latency");

    // Create a realistic config JSON
    let config = json!({
        "model": "claude-sonnet-4-5",
        "models": {
            "anthropic": {
                "api_key_env": "ANTHROPIC_API_KEY",
                "models": {
                    "claude-sonnet-4-5": { "max_tokens": 8192 },
                    "claude-opus-4-6": { "max_tokens": 16384 }
                }
            },
            "openai": {
                "api_key_env": "OPENAI_API_KEY",
                "base_url": "https://api.openai.com/v1",
                "models": {
                    "gpt-4o": { "max_tokens": 4096 },
                    "o1-preview": { "max_tokens": 8192 }
                }
            }
        },
        "tools": {
            "enabled": ["read", "write", "bash", "glob", "grep"],
            "disabled": []
        },
        "extensions": {
            "paths": ["/home/user/.pi/extensions"]
        }
    });
    let config_str = serde_json::to_string(&config).unwrap();

    let iterations = 10_000;
    let mut times_us: Vec<f64> = Vec::with_capacity(iterations);

    // Warmup
    for _ in 0..500 {
        let _: Value = serde_json::from_str(&config_str).unwrap();
    }

    // Measure
    for _ in 0..iterations {
        let start = Instant::now();
        let _: Value = serde_json::from_str(&config_str).unwrap();
        let elapsed = start.elapsed();
        times_us.push(elapsed.as_nanos() as f64 / 1000.0);
    }

    times_us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p99_us = percentile(&times_us, 99.0);

    // Config parse should be sub-100us
    let threshold_us = if cfg!(debug_assertions) { 500.0 } else { 100.0 };
    let status = check_threshold(p99_us, threshold_us, false);

    let env = collect_fingerprint();
    let record = PerfRecord {
        schema: "pi.perf.regression.v1".to_string(),
        test: "config_parse_latency".to_string(),
        category: "startup".to_string(),
        budget_name: "config_parse_p99".to_string(),
        budget_threshold: threshold_us,
        budget_unit: "us".to_string(),
        actual_value: p99_us,
        status: status.to_string(),
        stats: None,
        env,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        baseline_value: None,
        delta_pct: None,
    };

    append_jsonl(
        &output_dir().join("perf_regression.jsonl"),
        &emit_record(&record),
    );

    eprintln!("\n=== Config Parse Latency ===");
    eprintln!("  P99:    {p99_us:.1}us (budget: {threshold_us:.0}us)");
    eprintln!("  Status: {status}");

    assert_eq!(
        status, "PASS",
        "config_parse_p99={p99_us:.1}us exceeds {threshold_us:.0}us"
    );
}

// ─── Report Generation ──────────────────────────────────────────────────────

#[test]
fn generate_regression_report() {
    let _guard = perf_guard();

    let out = output_dir();
    let jsonl_path = out.join("perf_regression.jsonl");

    if !jsonl_path.exists() {
        eprintln!("[perf_regression] No JSONL data found; run other tests first");
        return;
    }

    let content = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
    let records: Vec<PerfRecord> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if records.is_empty() {
        eprintln!("[perf_regression] No records to report");
        return;
    }

    // Generate markdown report
    let mut md = String::with_capacity(4 * 1024);
    md.push_str("# Performance Regression Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}\n",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );

    // Summary table
    let pass_count = records.iter().filter(|r| r.status == "PASS").count();
    let fail_count = records.iter().filter(|r| r.status == "FAIL").count();

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|---|---|\n");
    let _ = writeln!(md, "| Tests run | {} |", records.len());
    let _ = writeln!(md, "| PASS | {pass_count} |");
    let _ = writeln!(md, "| FAIL | {fail_count} |");
    md.push('\n');

    // Detail table
    md.push_str("## Results\n\n");
    md.push_str("| Test | Category | Budget | Actual | Threshold | Unit | Status | Delta |\n");
    md.push_str("|---|---|---|---|---|---|---|---|\n");
    for r in &records {
        let delta_str = r
            .delta_pct
            .map_or_else(|| "-".to_string(), |d| format!("{d:+.1}%"));
        let _ = writeln!(
            md,
            "| {} | {} | `{}` | {:.2} | {} | {} | {} | {} |",
            r.test,
            r.category,
            r.budget_name,
            r.actual_value,
            r.budget_threshold,
            r.budget_unit,
            r.status,
            delta_str,
        );
    }
    md.push('\n');

    // Environment
    if let Some(first) = records.first() {
        md.push_str("## Environment\n\n");
        md.push_str("| Key | Value |\n");
        md.push_str("|---|---|\n");
        let _ = writeln!(md, "| OS | {} |", first.env.os);
        let _ = writeln!(md, "| Arch | {} |", first.env.arch);
        let _ = writeln!(md, "| CPU | {} |", first.env.cpu_model);
        let _ = writeln!(md, "| Cores | {} |", first.env.cpu_cores);
        let _ = writeln!(md, "| Memory | {}MB |", first.env.mem_total_mb);
        let _ = writeln!(md, "| Build | {} |", first.env.build_profile);
        let _ = writeln!(md, "| Git | {} |", first.env.git_commit);
    }

    let report_path = out.join("PERF_REGRESSION_REPORT.md");
    std::fs::write(&report_path, &md).expect("write regression report");

    // JSON summary
    let summary = json!({
        "schema": "pi.perf.regression_summary.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "tests": records.len(),
        "pass": pass_count,
        "fail": fail_count,
        "results": records.iter().map(|r| json!({
            "test": r.test,
            "budget": r.budget_name,
            "actual": r.actual_value,
            "threshold": r.budget_threshold,
            "unit": r.budget_unit,
            "status": r.status,
            "delta_pct": r.delta_pct,
        })).collect::<Vec<_>>(),
    });

    let summary_path = out.join("perf_regression_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    )
    .expect("write regression summary");

    eprintln!("\n=== Performance Regression Report ===");
    eprintln!("  Tests:  {}", records.len());
    eprintln!("  PASS:   {pass_count}");
    eprintln!("  FAIL:   {fail_count}");
    eprintln!("  Report: {}", report_path.display());
    eprintln!("  JSON:   {}", summary_path.display());
}

// ─── Baseline Management ────────────────────────────────────────────────────

/// Store current measurements as new baseline (run manually).
#[test]
fn update_baseline() {
    if std::env::var("PERF_UPDATE_BASELINE").ok().is_none() {
        eprintln!("[perf_regression] Set PERF_UPDATE_BASELINE=1 to update baseline");
        return;
    }

    let jsonl_path = output_dir().join("perf_regression.jsonl");
    if !jsonl_path.exists() {
        eprintln!("[perf_regression] No JSONL data; run tests first");
        return;
    }

    let content = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
    let records: Vec<PerfRecord> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let env = collect_fingerprint();
    let baselines: Vec<BaselineEntry> = records
        .iter()
        .filter(|r| r.status == "PASS")
        .map(|r| BaselineEntry {
            budget_name: r.budget_name.clone(),
            value: r.actual_value,
            env_hash: env.config_hash.clone(),
            timestamp: r.timestamp.clone(),
        })
        .collect();

    let baseline_path = baseline_dir().join("regression_baseline.json");
    std::fs::write(
        &baseline_path,
        serde_json::to_string_pretty(&baselines).unwrap_or_default(),
    )
    .expect("write baseline");

    eprintln!(
        "[perf_regression] Baseline updated with {} entries at {}",
        baselines.len(),
        baseline_path.display()
    );
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn append_jsonl(path: &Path, line: &str) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .expect("create JSONL parent directory");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open JSONL for append");
    let _ = writeln!(file, "{line}");
}

#[test]
fn pi_binary_candidate_builder_default_order_is_release_perf_debug() {
    let root = Path::new("/tmp/pi-agent-target");
    let candidates = build_pi_binary_candidates(root, None, "");
    assert_eq!(
        candidates,
        vec![
            root.join("release/pi"),
            root.join("perf/pi"),
            root.join("debug/pi"),
        ]
    );
}

#[test]
fn pi_binary_candidate_builder_includes_profile_before_release() {
    let root = Path::new("/tmp/pi-agent-target");
    let candidates = build_pi_binary_candidates(root, None, "bench-profile");
    assert_eq!(candidates[0], root.join("bench-profile/pi"));
    assert_eq!(candidates[1], root.join("release/pi"));
    assert_eq!(candidates[2], root.join("perf/pi"));
    assert_eq!(candidates[3], root.join("debug/pi"));
}

#[test]
fn pi_binary_candidate_builder_trims_detected_profile() {
    let root = Path::new("/tmp/pi-agent-target");
    let candidates = build_pi_binary_candidates(root, None, "  bench-profile  ");
    assert_eq!(candidates[0], root.join("bench-profile/pi"));
    assert_eq!(candidates[1], root.join("release/pi"));
    assert_eq!(candidates[2], root.join("perf/pi"));
    assert_eq!(candidates[3], root.join("debug/pi"));
}

#[test]
fn pi_binary_candidate_builder_ignores_whitespace_only_profile() {
    let root = Path::new("/tmp/pi-agent-target");
    let candidates = build_pi_binary_candidates(root, None, " \t ");
    assert_eq!(
        candidates,
        vec![
            root.join("release/pi"),
            root.join("perf/pi"),
            root.join("debug/pi"),
        ]
    );
}

#[test]
fn pi_binary_candidate_builder_env_override_wins_and_dedups() {
    let root = Path::new("/tmp/pi-agent-target");
    let override_path = root.join("release/pi");
    let candidates = build_pi_binary_candidates(root, Some(override_path.clone()), "release");

    assert_eq!(candidates.first(), Some(&override_path));
    assert_eq!(
        candidates
            .iter()
            .filter(|path| **path == override_path)
            .count(),
        1
    );
    assert_eq!(
        candidates,
        vec![override_path, root.join("perf/pi"), root.join("debug/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_release_only_default() {
    let root = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidates(root, None, "bench-profile");
    assert_eq!(candidates, vec![root.join("release/pi")]);
}

#[test]
fn binary_size_candidate_builder_prefers_release_override_then_release_default() {
    let root = Path::new("/tmp/pi-agent-target");
    let override_path = root.join("custom-release/pi");
    let candidates = build_binary_size_candidates(root, Some(override_path.clone()), "debug");
    assert_eq!(candidates, vec![override_path, root.join("release/pi")]);
}

#[test]
fn binary_size_candidate_builder_dedups_override_matching_release() {
    let root = Path::new("/tmp/pi-agent-target");
    let release = root.join("release/pi");
    let candidates = build_binary_size_candidates(root, Some(release.clone()), "release");
    assert_eq!(candidates, vec![release]);
}

#[test]
fn binary_size_candidate_selector_prefers_existing_release_override() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let root = temp.path();
    let release = root.join("release/pi");
    let override_path = root.join("custom-release/pi");

    std::fs::create_dir_all(release.parent().expect("release parent")).expect("mkdir release");
    std::fs::create_dir_all(override_path.parent().expect("override parent"))
        .expect("mkdir override");
    std::fs::write(&release, b"release").expect("write release binary");
    std::fs::write(&override_path, b"override").expect("write override binary");

    let selected = first_existing_candidate(build_binary_size_candidates(
        root,
        Some(override_path.clone()),
        "perf",
    ));
    assert_eq!(selected.as_deref(), Some(override_path.as_path()));
}

#[test]
fn binary_size_candidate_selector_falls_back_to_release_when_override_missing() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let root = temp.path();
    let release = root.join("release/pi");
    let override_path = root.join("custom-release/pi");

    std::fs::create_dir_all(release.parent().expect("release parent")).expect("mkdir release");
    std::fs::write(&release, b"release").expect("write release binary");

    let selected = first_existing_candidate(build_binary_size_candidates(
        root,
        Some(override_path),
        "perf",
    ));
    assert_eq!(selected.as_deref(), Some(release.as_path()));
}

#[test]
fn binary_size_candidate_selector_returns_none_when_release_candidates_missing() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let root = temp.path();
    let override_path = root.join("custom-release/pi");

    let selected = first_existing_candidate(build_binary_size_candidates(
        root,
        Some(override_path),
        "perf",
    ));
    assert_eq!(selected, None);
}

#[test]
fn binary_size_missing_release_outcome_skips_when_not_strict() {
    assert!(binary_size_missing_release_outcome(false, "a,b").is_ok());
}

#[test]
fn binary_size_missing_release_outcome_fails_closed_when_strict() {
    let err = binary_size_missing_release_outcome(true, "x/y/release/pi")
        .expect_err("strict mode must fail when release binary is missing");
    assert!(
        err.contains("x/y/release/pi"),
        "error should contain checked candidate paths: {err}"
    );
    assert!(
        err.contains(PERF_RELEASE_BINARY_PATH_ENV),
        "error should point to release-binary override env var: {err}"
    );
}

#[test]
fn recover_poisoned_mutex_guard_clears_poison_state() {
    let lock = Mutex::new(());
    let _ = std::panic::catch_unwind(|| {
        let _guard = lock.lock().expect("acquire lock before poison");
        std::panic::panic_any("intentional poison for regression coverage");
    });

    assert!(lock.is_poisoned(), "mutex should be poisoned after panic");
    drop(recover_poisoned_mutex_guard(
        &lock,
        "[perf_regression][test] recovering poisoned lock",
    ));
    assert!(
        !lock.is_poisoned(),
        "poison should be cleared after recovery"
    );
    assert!(
        lock.lock().is_ok(),
        "subsequent lock acquisitions should succeed after poison clear"
    );
}

#[test]
fn recover_poisoned_mutex_guard_handles_clean_lock() {
    let lock = Mutex::new(());
    drop(recover_poisoned_mutex_guard(
        &lock,
        "[perf_regression][test] clean lock",
    ));
    assert!(!lock.is_poisoned(), "clean lock should remain unpoisoned");
}

#[test]
fn perf_strict_mode_from_accepts_truthy_tokens() {
    assert!(perf_strict_mode_from(Some("1")));
    assert!(perf_strict_mode_from(Some("true")));
    assert!(perf_strict_mode_from(Some(" True ")));
}

#[test]
fn perf_strict_mode_from_rejects_non_truthy_tokens() {
    assert!(!perf_strict_mode_from(None));
    assert!(!perf_strict_mode_from(Some("")));
    assert!(!perf_strict_mode_from(Some("0")));
    assert!(!perf_strict_mode_from(Some("false")));
    assert!(!perf_strict_mode_from(Some("yes")));
}
