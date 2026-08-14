//! Shared benchmark environment validation and fingerprinting.
//!
//! Provides standardized environment detection, noise scoring, and
//! fingerprinting for all Criterion benchmark suites. Include in each
//! benchmark via `#[path = "bench_env.rs"] mod bench_env;`.
//!
//! Environment checks:
//! - CPU frequency governor (performance vs powersave)
//! - Turbo boost status
//! - ASLR setting
//! - Transparent huge pages
//! - CPU frequency stability
//! - Available cores for isolation

use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use sysinfo::System;

/// Environment fingerprint emitted at the start of each benchmark suite.
#[derive(Debug)]
pub struct BenchEnvFingerprint {
    pub os: String,
    pub arch: &'static str,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub mem_total_mb: u64,
    pub config_hash: String,
    pub governor: String,
    pub turbo_boost: String,
    pub aslr: String,
    pub thp: String,
    pub noise_score: u8,
}

/// Read a sysfs file, returning None if unavailable.
fn read_sysfs(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn detect_governor() -> String {
    read_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .unwrap_or_else(|| "unavailable".to_string())
}

fn detect_turbo() -> String {
    // Intel pstate
    if let Some(val) = read_sysfs("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        return if val == "1" {
            "disabled".to_string()
        } else {
            "enabled".to_string()
        };
    }
    // AMD boost
    if let Some(val) = read_sysfs("/sys/devices/system/cpu/cpufreq/boost") {
        return if val == "0" {
            "disabled".to_string()
        } else {
            "enabled".to_string()
        };
    }
    "unavailable".to_string()
}

fn detect_aslr() -> String {
    match read_sysfs("/proc/sys/kernel/randomize_va_space").as_deref() {
        Some("0") => "disabled".to_string(),
        Some("1") => "partial".to_string(),
        Some("2") => "full".to_string(),
        Some(v) => format!("unknown({v})"),
        None => "unavailable".to_string(),
    }
}

fn detect_thp() -> String {
    read_sysfs("/sys/kernel/mm/transparent_hugepage/enabled")
        .and_then(|s| {
            // Extract active setting from "[always] madvise never" format
            s.split_whitespace()
                .find(|w| w.starts_with('['))
                .map(|w| w.trim_matches(|c| c == '[' || c == ']').to_string())
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

/// Compute a noise score (0 = optimal, higher = more variance expected).
///
/// Scoring:
/// - governor != performance: +3
/// - turbo enabled:           +2
/// - THP != never:            +1
/// - ASLR enabled:            +1 (minor, affects address layout only)
fn compute_noise_score(governor: &str, turbo: &str, thp: &str, aslr: &str) -> u8 {
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

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    pi::package_manager::hex_encode(&hasher.finalize())
}

/// Collect the full environment fingerprint.
#[must_use]
pub fn collect_fingerprint() -> BenchEnvFingerprint {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();

    let cpu_brand = system
        .cpus()
        .first()
        .map_or_else(|| "unknown".to_string(), |cpu| cpu.brand().to_string());

    let config = format!(
        "pkg={} git_sha={} build_ts={}",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
    );
    let config_hash = sha256_hex(&config);

    let governor = detect_governor();
    let turbo_boost = detect_turbo();
    let aslr = detect_aslr();
    let thp = detect_thp();
    let noise_score = compute_noise_score(&governor, &turbo_boost, &thp, &aslr);

    BenchEnvFingerprint {
        os: System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string()),
        arch: std::env::consts::ARCH,
        cpu_brand,
        cpu_cores: system.cpus().len(),
        mem_total_mb: system.total_memory() / 1024 / 1024,
        config_hash,
        governor,
        turbo_boost,
        aslr,
        thp,
        noise_score,
    }
}

/// Print the benchmark environment banner once per process.
///
/// Emits a structured `[bench-env]` line to stderr with all environment
/// parameters. Warns when the noise score is above 0.
pub fn print_env_banner_once() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let fp = collect_fingerprint();

        eprintln!(
            "[bench-env] os={} arch={} cpu=\"{}\" cores={} mem_mb={} \
             governor={} turbo={} aslr={} thp={} noise_score={} config_hash={}",
            fp.os,
            fp.arch,
            fp.cpu_brand,
            fp.cpu_cores,
            fp.mem_total_mb,
            fp.governor,
            fp.turbo_boost,
            fp.aslr,
            fp.thp,
            fp.noise_score,
            fp.config_hash,
        );

        if fp.noise_score > 0 {
            eprintln!(
                "[bench-env] WARNING: noise_score={} — results may have higher variance. \
                 Run `scripts/bench_env_setup.sh apply` to optimize.",
                fp.noise_score
            );
            if fp.governor != "performance" && fp.governor != "unavailable" {
                eprintln!(
                    "[bench-env]   - CPU governor is '{}' (want 'performance')",
                    fp.governor
                );
            }
            if fp.turbo_boost == "enabled" {
                eprintln!("[bench-env]   - Turbo boost is enabled (disable for stable frequency)");
            }
            if fp.thp != "never" && fp.thp != "unavailable" {
                eprintln!(
                    "[bench-env]   - THP is '{}' (want 'never' to avoid latency spikes)",
                    fp.thp
                );
            }
        }
    });
}

/// Return a Criterion configuration with the environment banner printed.
///
/// This is the standard entry point for all benchmark suites.
#[must_use]
#[allow(dead_code)]
pub fn criterion_config() -> criterion::Criterion {
    print_env_banner_once();
    criterion::Criterion::default()
}

/// Return a Criterion configuration for system-level benchmarks
/// (fewer samples, longer measurement time).
#[must_use]
#[allow(dead_code)]
pub fn criterion_config_system() -> criterion::Criterion {
    print_env_banner_once();
    criterion::Criterion::default()
        .sample_size(20)
        .measurement_time(std::time::Duration::from_secs(10))
}
