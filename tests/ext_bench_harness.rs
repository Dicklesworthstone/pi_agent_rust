#![cfg(feature = "ext-conformance")]
//! Unified benchmark harness for extension startup/exec (bd-20s9, bd-xs79).
//!
//! A single entry point that runs all extension benchmark scenarios
//! and emits JSONL with environment fingerprint, statistics, and
//! budget regression detection.
//!
//! ## Modes
//!
//! - `PI_BENCH_MODE=pr`      — diverse subset (10 extensions, 10 iterations) for PR CI
//! - `PI_BENCH_MODE=nightly`  — full corpus (all safe extensions, 50 iterations)
//! - `PI_BENCH_MODE=custom`   — use `PI_BENCH_MAX` and `PI_BENCH_ITERATIONS`
//!
//! ## PR Subset Selection Policy (bd-2mb1)
//!
//! PR mode selects a diverse representative subset to maximize API surface coverage:
//! - 2 official extensions (1 with tools, 1 with events)
//! - 2 community extensions (1 with commands+events, 1 with tools+commands+flags)
//! - 2 npm extensions (1 with commands, 1 with events)
//! - Remaining slots filled from safe pool in manifest order
//!
//! Per-extension timeout: `PI_BENCH_TIMEOUT_SECS` (default 30s) aborts slow extensions.
//!
//! ## Scenarios
//!
//! 1. `cold_load`: Fresh runtime + context per load iteration
//! 2. `warm_load`: Shared runtime, repeated loads after warmup
//! 3. `tool_dispatch`: Extension-registered tool call overhead
//! 4. `event_dispatch`: Event hook dispatch latency (`AgentStart`)
//!
//! ## Output
//!
//! - JSONL: `target/perf/ext_bench_harness.jsonl` (one record per extension per scenario)
//! - Summary: `target/perf/ext_bench_harness_report.json`
//! - Markdown: `target/perf/BENCH_HARNESS_REPORT.md`
//!
//! ## Run
//!
//! ```bash
//! # PR mode (quick)
//! PI_BENCH_MODE=pr cargo test --test ext_bench_harness --features ext-conformance -- --nocapture
//!
//! # Nightly mode (full)
//! PI_BENCH_MODE=nightly cargo test --test ext_bench_harness --features ext-conformance -- --nocapture
//! ```

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

// ─── Configuration ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchMode {
    Pr,
    Nightly,
    Custom,
}

fn bench_mode() -> BenchMode {
    match std::env::var("PI_BENCH_MODE")
        .unwrap_or_else(|_| "pr".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "nightly" | "full" => BenchMode::Nightly,
        "custom" => BenchMode::Custom,
        _ => BenchMode::Pr,
    }
}

fn max_extensions() -> usize {
    std::env::var("PI_BENCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| match bench_mode() {
            BenchMode::Pr => 10,
            BenchMode::Nightly => 200,
            BenchMode::Custom => 20,
        })
}

fn iterations() -> usize {
    std::env::var("PI_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| match bench_mode() {
            BenchMode::Pr => 10,
            BenchMode::Nightly => 100,
            BenchMode::Custom => 20,
        })
}

fn event_dispatch_count() -> usize {
    std::env::var("PI_BENCH_EVENT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| match bench_mode() {
            BenchMode::Pr => 50,
            BenchMode::Nightly => 200,
            BenchMode::Custom => 100,
        })
}

/// Per-extension timeout: if a single extension's benchmark exceeds this, skip it.
fn per_extension_timeout() -> Duration {
    let secs: u64 = std::env::var("PI_BENCH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

// ─── Environment Fingerprint ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct EnvFingerprint {
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    mem_total_mb: u64,
    build_profile: String,
    git_commit: String,
    features: Vec<String>,
    config_hash: String,
}

fn collect_env_fingerprint() -> EnvFingerprint {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let cpu_model = read_cpu_model();
    let cpu_cores =
        u32::try_from(std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get))
            .unwrap_or(1);

    let sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram()),
    );
    let mem_total_mb = sys.total_memory() / (1024 * 1024);

    let build_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
    .to_string();

    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());

    let features = vec!["ext-conformance".to_string()];

    // Build config hash from env fields
    let hash_input =
        format!("{os}|{arch}|{cpu_model}|{cpu_cores}|{mem_total_mb}|{build_profile}|{git_commit}");
    let config_hash = format!("{:x}", simple_hash(hash_input.as_bytes()));

    EnvFingerprint {
        os,
        arch,
        cpu_model,
        cpu_cores,
        mem_total_mb,
        build_profile,
        git_commit,
        features,
        config_hash,
    }
}

fn read_cpu_model() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in cpuinfo.lines() {
                if line.starts_with("model name") {
                    if let Some(model) = line.split(':').nth(1) {
                        return model.trim().to_string();
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

/// Simple FNV-1a hash for config fingerprinting (not cryptographic).
fn simple_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

// ─── Manifest Loading ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    conformance_tier: u32,
    source_tier: String,
    is_multi_file: bool,
    uses_exec: bool,
    registers_tools: bool,
    registers_commands: bool,
    registers_flags: bool,
    #[allow(dead_code)]
    registers_providers: bool,
    subscribes_events: usize,
    #[allow(dead_code)]
    uses_session: bool,
}

impl ManifestEntry {
    const fn is_safe(&self) -> bool {
        !self.is_multi_file && !self.uses_exec
    }
}

struct Manifest {
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    /// Return safe extensions suitable for benchmarking (single-file, no exec).
    fn safe_extensions(&self, max: usize) -> Vec<&ManifestEntry> {
        self.extensions
            .iter()
            .filter(|e| e.is_safe())
            .take(max)
            .collect()
    }

    /// Return a diverse PR subset: mix of official, community, npm; tools, commands,
    /// events, flags, providers, session. Ensures breadth of API surface coverage.
    ///
    /// Selection policy (documented per bd-2mb1):
    /// - 2 official extensions (1 with tools, 1 with events)
    /// - 2 community extensions (1 with commands+events, 1 with tools+commands+flags)
    /// - 2 npm extensions (1 with commands, 1 with events)
    /// - Fill remaining slots from safe pool in manifest order
    fn pr_subset(&self, max: usize) -> Vec<&ManifestEntry> {
        let safe: Vec<&ManifestEntry> = self.extensions.iter().filter(|e| e.is_safe()).collect();

        let mut selected: Vec<&ManifestEntry> = Vec::with_capacity(max);
        let mut used_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();

        // Helper: pick first match from safe set
        let mut pick = |predicate: &dyn Fn(&&ManifestEntry) -> bool| -> bool {
            if selected.len() >= max {
                return false;
            }
            if let Some(e) = safe
                .iter()
                .find(|e| !used_ids.contains(e.id.as_str()) && predicate(e))
            {
                used_ids.insert(&e.id);
                selected.push(e);
                return true;
            }
            false
        };

        // Official: tool-registering
        pick(&|e: &&ManifestEntry| e.source_tier == "official-pi-mono" && e.registers_tools);
        // Official: event-subscribing
        pick(&|e: &&ManifestEntry| e.source_tier == "official-pi-mono" && e.subscribes_events > 0);
        // Community: commands + events
        pick(&|e: &&ManifestEntry| {
            e.source_tier == "community" && e.registers_commands && e.subscribes_events > 0
        });
        // Community: tools + commands + flags (complex registration)
        pick(&|e: &&ManifestEntry| {
            e.source_tier == "community" && e.registers_tools && e.registers_flags
        });
        // npm: commands
        pick(&|e: &&ManifestEntry| e.source_tier == "npm-registry" && e.registers_commands);
        // npm: events
        pick(&|e: &&ManifestEntry| e.source_tier == "npm-registry" && e.subscribes_events > 0);

        // Fill remaining from safe pool
        for e in &safe {
            if selected.len() >= max {
                break;
            }
            if !used_ids.contains(e.id.as_str()) {
                used_ids.insert(&e.id);
                selected.push(e);
            }
        }

        selected
    }
}

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn output_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/perf")
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/ext_conformance/VALIDATED_MANIFEST.json");
        let data = std::fs::read_to_string(&path).expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value = serde_json::from_str(&data).expect("Failed to parse manifest");
        let extensions = json["extensions"]
            .as_array()
            .expect("extensions array")
            .iter()
            .map(|e| {
                let caps = &e["capabilities"];
                ManifestEntry {
                    id: e["id"].as_str().unwrap_or("").to_string(),
                    entry_path: e["entry_path"].as_str().unwrap_or("").to_string(),
                    conformance_tier: u32::try_from(e["conformance_tier"].as_u64().unwrap_or(0))
                        .unwrap_or(0),
                    source_tier: e["source_tier"].as_str().unwrap_or("").to_string(),
                    is_multi_file: caps["is_multi_file"].as_bool().unwrap_or(false),
                    uses_exec: caps["uses_exec"].as_bool().unwrap_or(false),
                    registers_tools: caps["registers_tools"].as_bool().unwrap_or(false),
                    registers_commands: caps["registers_commands"].as_bool().unwrap_or(false),
                    registers_flags: caps["registers_flags"].as_bool().unwrap_or(false),
                    registers_providers: caps["registers_providers"].as_bool().unwrap_or(false),
                    subscribes_events: caps["subscribes_events"].as_array().map_or(0, Vec::len),
                    uses_session: caps["uses_session"].as_bool().unwrap_or(false),
                }
            })
            .collect();
        Manifest { extensions }
    })
}

// ─── Statistics ─────────────────────────────────────────────────────────────

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Debug, Clone, Serialize)]
struct Stats {
    count: usize,
    min_us: u64,
    max_us: u64,
    mean_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
}

impl Stats {
    fn from_micros(samples: &[u64]) -> Self {
        if samples.is_empty() {
            return Self {
                count: 0,
                min_us: 0,
                max_us: 0,
                mean_us: 0,
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let sum: u128 = sorted.iter().map(|&v| u128::from(v)).sum();
        let mean = u64::try_from(sum / sorted.len() as u128).unwrap_or(u64::MAX);
        Self {
            count: sorted.len(),
            min_us: sorted[0],
            max_us: sorted[sorted.len() - 1],
            mean_us: mean,
            p50_us: percentile(&sorted, 50.0),
            p95_us: percentile(&sorted, 95.0),
            p99_us: percentile(&sorted, 99.0),
        }
    }
}

// ─── Scenario Results ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ScenarioResult {
    schema: String,
    runtime: String,
    scenario: String,
    extension: String,
    group: String,
    tier: u32,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    stats: Stats,
    env: EnvFingerprint,
}

// ─── Benchmark Runner ───────────────────────────────────────────────────────

fn group_for(entry: &ManifestEntry) -> &'static str {
    if entry.source_tier == "official-pi-mono" {
        if entry.conformance_tier <= 3 {
            "official-simple"
        } else {
            "official-complex"
        }
    } else {
        "community"
    }
}

/// Benchmark cold-load: fresh runtime + context per iteration.
#[allow(clippy::too_many_lines)]
fn bench_cold_load(entry: &ManifestEntry, n: usize, env: &EnvFingerprint) -> ScenarioResult {
    let entry_file = artifacts_dir().join(&entry.entry_path);
    let group = group_for(entry);

    if !entry_file.exists() {
        return ScenarioResult {
            schema: "pi.ext.rust_bench.v1".to_string(),
            runtime: "pi_agent_rust".to_string(),
            scenario: "cold_load".to_string(),
            extension: entry.id.clone(),
            group: group.to_string(),
            tier: entry.conformance_tier,
            success: false,
            error: Some(format!("Artifact not found: {}", entry_file.display())),
            stats: Stats::from_micros(&[]),
            env: env.clone(),
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ScenarioResult {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "cold_load".to_string(),
                extension: entry.id.clone(),
                group: group.to_string(),
                tier: entry.conformance_tier,
                success: false,
                error: Some(format!("Load spec error: {e}")),
                stats: Stats::from_micros(&[]),
                env: env.clone(),
            };
        }
    };

    let cwd = std::env::temp_dir().join(format!("pi-bench-{}", entry.id.replace('/', "_")));
    let _ = std::fs::create_dir_all(&cwd);
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let mut samples_us = Vec::with_capacity(n);
    let mut last_error = None::<String>;
    let timeout = per_extension_timeout();
    let wall_start = Instant::now();

    for _ in 0..n {
        if wall_start.elapsed() > timeout {
            last_error = Some(format!(
                "Timeout: exceeded {}s budget after {} of {n} iterations",
                timeout.as_secs(),
                samples_us.len()
            ));
            break;
        }

        let manager = ExtensionManager::new();
        let start = Instant::now();

        let runtime_result = common::run_async({
            let manager = manager.clone();
            let tools = Arc::clone(&tools);
            let js_config = js_config.clone();
            async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
        });
        let runtime = match runtime_result {
            Ok(rt) => rt,
            Err(e) => {
                last_error = Some(format!("Runtime start error: {e}"));
                continue;
            }
        };
        manager.set_js_runtime(runtime);

        let load_result = common::run_async({
            let manager = manager.clone();
            let spec = spec.clone();
            async move { manager.load_js_extensions(vec![spec]).await }
        });

        match load_result {
            Ok(()) => {
                let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                samples_us.push(elapsed_us);
            }
            Err(e) => {
                last_error = Some(format!("Load error: {e}"));
            }
        }

        common::run_async({
            async move {
                let _ = manager.shutdown(Duration::from_millis(250)).await;
            }
        });
    }

    let success = samples_us.len() == n;
    ScenarioResult {
        schema: "pi.ext.rust_bench.v1".to_string(),
        runtime: "pi_agent_rust".to_string(),
        scenario: "cold_load".to_string(),
        extension: entry.id.clone(),
        group: group.to_string(),
        tier: entry.conformance_tier,
        success,
        error: if success { None } else { last_error },
        stats: Stats::from_micros(&samples_us),
        env: env.clone(),
    }
}

/// Benchmark warm-load: single runtime, repeated loads after warmup.
#[allow(clippy::too_many_lines)]
fn bench_warm_load(entry: &ManifestEntry, n: usize, env: &EnvFingerprint) -> ScenarioResult {
    let entry_file = artifacts_dir().join(&entry.entry_path);
    let group = group_for(entry);

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ScenarioResult {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "warm_load".to_string(),
                extension: entry.id.clone(),
                group: group.to_string(),
                tier: entry.conformance_tier,
                success: false,
                error: Some(format!("Load spec error: {e}")),
                stats: Stats::from_micros(&[]),
                env: env.clone(),
            };
        }
    };

    let cwd = std::env::temp_dir().join(format!("pi-bench-warm-{}", entry.id.replace('/', "_")));
    let _ = std::fs::create_dir_all(&cwd);
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let manager = ExtensionManager::new();
    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ScenarioResult {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "warm_load".to_string(),
                extension: entry.id.clone(),
                group: group.to_string(),
                tier: entry.conformance_tier,
                success: false,
                error: Some(format!("Warm runtime start error: {e}")),
                stats: Stats::from_micros(&[]),
                env: env.clone(),
            };
        }
    };
    manager.set_js_runtime(runtime);

    // Warmup load
    let warmup_result = common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    if let Err(e) = warmup_result {
        common::run_async({
            async move {
                let _ = manager.shutdown(Duration::from_millis(250)).await;
            }
        });
        return ScenarioResult {
            schema: "pi.ext.rust_bench.v1".to_string(),
            runtime: "pi_agent_rust".to_string(),
            scenario: "warm_load".to_string(),
            extension: entry.id.clone(),
            group: group.to_string(),
            tier: entry.conformance_tier,
            success: false,
            error: Some(format!("Warmup load error: {e}")),
            stats: Stats::from_micros(&[]),
            env: env.clone(),
        };
    }

    // Measured warm loads
    let timeout = per_extension_timeout();
    let wall_start = Instant::now();
    let mut samples_us = Vec::with_capacity(n);
    for _ in 0..n {
        if wall_start.elapsed() > timeout {
            break;
        }
        let start = Instant::now();
        let load_result = common::run_async({
            let manager = manager.clone();
            let spec = spec.clone();
            async move { manager.load_js_extensions(vec![spec]).await }
        });
        if load_result.is_ok() {
            let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
            samples_us.push(elapsed_us);
        }
    }

    common::run_async({
        async move {
            let _ = manager.shutdown(Duration::from_millis(250)).await;
        }
    });

    let success = samples_us.len() == n;
    ScenarioResult {
        schema: "pi.ext.rust_bench.v1".to_string(),
        runtime: "pi_agent_rust".to_string(),
        scenario: "warm_load".to_string(),
        extension: entry.id.clone(),
        group: group.to_string(),
        tier: entry.conformance_tier,
        success,
        error: None,
        stats: Stats::from_micros(&samples_us),
        env: env.clone(),
    }
}

/// Benchmark event dispatch: fire events at loaded extensions and measure latency.
#[allow(clippy::too_many_lines)]
fn bench_event_dispatch(
    entries: &[&ManifestEntry],
    count: usize,
    env: &EnvFingerprint,
) -> ScenarioResult {
    let cwd = std::env::temp_dir().join("pi-bench-event-dispatch");
    let _ = std::fs::create_dir_all(&cwd);
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let manager = ExtensionManager::new();
    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ScenarioResult {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "event_dispatch".to_string(),
                extension: format!("{}_extensions", entries.len()),
                group: "aggregate".to_string(),
                tier: 0,
                success: false,
                error: Some(format!("Runtime start error: {e}")),
                stats: Stats::from_micros(&[]),
                env: env.clone(),
            };
        }
    };
    manager.set_js_runtime(runtime);

    // Load all extensions
    let mut specs = Vec::new();
    for entry in entries {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if let Ok(spec) = JsExtensionLoadSpec::from_entry_path(&entry_file) {
            specs.push(spec);
        }
    }

    let loaded_count = specs.len();
    let load_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await }
    });
    if let Err(e) = load_result {
        common::run_async({
            async move {
                let _ = manager.shutdown(Duration::from_millis(500)).await;
            }
        });
        return ScenarioResult {
            schema: "pi.ext.rust_bench.v1".to_string(),
            runtime: "pi_agent_rust".to_string(),
            scenario: "event_dispatch".to_string(),
            extension: format!("{loaded_count}_extensions"),
            group: "aggregate".to_string(),
            tier: 0,
            success: false,
            error: Some(format!("Load error: {e}")),
            stats: Stats::from_micros(&[]),
            env: env.clone(),
        };
    }

    // Dispatch events and measure
    let payload = json!({
        "systemPrompt": "You are Pi.",
        "model": "claude-sonnet-4-5",
    });

    let mut samples_us = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let _ = common::run_async({
            let manager = manager.clone();
            let payload = Some(payload.clone());
            async move {
                manager
                    .dispatch_event(ExtensionEventName::AgentStart, payload)
                    .await
            }
        });
        let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        samples_us.push(elapsed_us);
    }

    common::run_async({
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });

    ScenarioResult {
        schema: "pi.ext.rust_bench.v1".to_string(),
        runtime: "pi_agent_rust".to_string(),
        scenario: "event_dispatch".to_string(),
        extension: format!("{loaded_count}_extensions"),
        group: "aggregate".to_string(),
        tier: 0,
        success: true,
        error: None,
        stats: Stats::from_micros(&samples_us),
        env: env.clone(),
    }
}

// ─── Report Generation ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct HarnessReport {
    schema: String,
    generated_at: String,
    mode: String,
    config: HarnessConfig,
    env: EnvFingerprint,
    summary: HarnessSummary,
    by_scenario: BTreeMap<String, ScenarioSummary>,
    budget_checks: Vec<BudgetCheck>,
    /// Per-extension scenario results for detailed per-extension reporting.
    results: Vec<ScenarioResult>,
}

#[derive(Debug, Serialize)]
struct HarnessConfig {
    max_extensions: usize,
    iterations: usize,
    event_dispatch_count: usize,
    debug_build: bool,
}

#[derive(Debug, Serialize)]
struct HarnessSummary {
    total_scenarios: usize,
    total_passed: usize,
    total_failed: usize,
    budgets_passed: usize,
    budgets_failed: usize,
    budgets_no_data: usize,
}

#[derive(Debug, Serialize)]
struct ScenarioSummary {
    scenario: String,
    extensions_tested: usize,
    passed: usize,
    failed: usize,
    aggregate_stats: Stats,
}

#[derive(Debug, Clone, Serialize)]
struct BudgetCheck {
    budget_name: String,
    threshold_us: u64,
    actual_us: Option<u64>,
    status: String,
    /// Extension that triggered the worst value (for per-extension budgets).
    #[serde(skip_serializing_if = "Option::is_none")]
    worst_extension: Option<String>,
}

#[allow(clippy::too_many_lines)]
fn check_budgets(results: &[ScenarioResult]) -> Vec<BudgetCheck> {
    let mut checks = Vec::new();

    // Cold load P95 budget: 200ms for simple extensions (per BENCHMARKS.md)
    let cold_threshold: u64 = 200_000;
    let cold_simple: Vec<u64> = results
        .iter()
        .filter(|r| r.scenario == "cold_load" && r.group == "official-simple" && r.success)
        .flat_map(|r| std::iter::once(r.stats.p95_us))
        .collect();
    let cold_simple_p95 = if cold_simple.is_empty() {
        None
    } else {
        let mut sorted = cold_simple;
        sorted.sort_unstable();
        Some(percentile(&sorted, 95.0))
    };
    checks.push(BudgetCheck {
        budget_name: "ext_cold_load_simple_p95".to_string(),
        threshold_us: cold_threshold,
        actual_us: cold_simple_p95,
        status: cold_simple_p95.map_or_else(
            || "NO_DATA".to_string(),
            |v| if v <= cold_threshold { "PASS" } else { "FAIL" }.to_string(),
        ),
        worst_extension: None,
    });

    // Per-extension cold load P99 budget: 100ms — every extension must load
    // within 100ms at P99 (bd-xs79). Report the worst offender.
    let per_ext_threshold: u64 = 100_000;
    let worst_cold_p99: Option<(u64, String)> = results
        .iter()
        .filter(|r| r.scenario == "cold_load" && r.success)
        .map(|r| (r.stats.p99_us, r.extension.clone()))
        .max_by_key(|(p99, _)| *p99);
    checks.push(BudgetCheck {
        budget_name: "ext_cold_load_per_ext_p99".to_string(),
        threshold_us: per_ext_threshold,
        actual_us: worst_cold_p99.as_ref().map(|(p99, _)| *p99),
        status: worst_cold_p99.as_ref().map_or_else(
            || "NO_DATA".to_string(),
            |(p99, _)| {
                if *p99 <= per_ext_threshold {
                    "PASS"
                } else {
                    "FAIL"
                }
                .to_string()
            },
        ),
        worst_extension: worst_cold_p99.as_ref().map(|(_, ext)| ext.clone()),
    });

    // Per-extension warm load P99 budget: 100ms — same per-extension gate for warm loads.
    let worst_warm_p99: Option<(u64, String)> = results
        .iter()
        .filter(|r| r.scenario == "warm_load" && r.success)
        .map(|r| (r.stats.p99_us, r.extension.clone()))
        .max_by_key(|(p99, _)| *p99);
    checks.push(BudgetCheck {
        budget_name: "ext_warm_load_per_ext_p99".to_string(),
        threshold_us: per_ext_threshold,
        actual_us: worst_warm_p99.as_ref().map(|(p99, _)| *p99),
        status: worst_warm_p99.as_ref().map_or_else(
            || "NO_DATA".to_string(),
            |(p99, _)| {
                if *p99 <= per_ext_threshold {
                    "PASS"
                } else {
                    "FAIL"
                }
                .to_string()
            },
        ),
        worst_extension: worst_warm_p99.as_ref().map(|(_, ext)| ext.clone()),
    });

    // Event dispatch P99 budget: 5ms
    let event_dispatch_result = results
        .iter()
        .find(|r| r.scenario == "event_dispatch" && r.success);
    let event_p99 = event_dispatch_result.map(|r| r.stats.p99_us);
    checks.push(BudgetCheck {
        budget_name: "event_dispatch_p99".to_string(),
        threshold_us: 5_000,
        actual_us: event_p99,
        status: event_p99.map_or_else(
            || "NO_DATA".to_string(),
            |v| if v <= 5_000 { "PASS" } else { "FAIL" }.to_string(),
        ),
        worst_extension: None,
    });

    // Warm load P95 budget: generous 100ms (cold path amortization)
    let warm_p95: Vec<u64> = results
        .iter()
        .filter(|r| r.scenario == "warm_load" && r.success)
        .flat_map(|r| std::iter::once(r.stats.p95_us))
        .collect();
    let warm_agg_p95 = if warm_p95.is_empty() {
        None
    } else {
        let mut sorted = warm_p95;
        sorted.sort_unstable();
        Some(percentile(&sorted, 95.0))
    };
    checks.push(BudgetCheck {
        budget_name: "ext_warm_load_p95".to_string(),
        threshold_us: 100_000,
        actual_us: warm_agg_p95,
        status: warm_agg_p95.map_or_else(
            || "NO_DATA".to_string(),
            |v| if v <= 100_000 { "PASS" } else { "FAIL" }.to_string(),
        ),
        worst_extension: None,
    });

    checks
}

#[allow(clippy::too_many_lines)]
fn generate_markdown(report: &HarnessReport) -> String {
    let mut md = String::with_capacity(8192);
    writeln!(md, "# Extension Benchmark Harness Report").unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "> Generated: {} | Mode: {} | Build: {}",
        report.generated_at,
        report.mode,
        if report.config.debug_build {
            "debug"
        } else {
            "release"
        }
    )
    .unwrap();
    writeln!(md).unwrap();

    // Config
    writeln!(md, "## Configuration").unwrap();
    writeln!(md).unwrap();
    writeln!(md, "| Setting | Value |").unwrap();
    writeln!(md, "|---------|-------|").unwrap();
    writeln!(md, "| Max extensions | {} |", report.config.max_extensions).unwrap();
    writeln!(md, "| Iterations | {} |", report.config.iterations).unwrap();
    writeln!(
        md,
        "| Event dispatch count | {} |",
        report.config.event_dispatch_count
    )
    .unwrap();
    writeln!(md, "| CPU | {} |", report.env.cpu_model).unwrap();
    writeln!(md, "| CPU cores | {} |", report.env.cpu_cores).unwrap();
    writeln!(md, "| OS | {} |", report.env.os).unwrap();
    writeln!(md, "| Git commit | {} |", report.env.git_commit).unwrap();
    writeln!(md).unwrap();

    // Summary
    writeln!(md, "## Summary").unwrap();
    writeln!(md).unwrap();
    writeln!(md, "| Metric | Value |").unwrap();
    writeln!(md, "|--------|-------|").unwrap();
    writeln!(
        md,
        "| Total scenarios | {} |",
        report.summary.total_scenarios
    )
    .unwrap();
    writeln!(md, "| Passed | {} |", report.summary.total_passed).unwrap();
    writeln!(md, "| Failed | {} |", report.summary.total_failed).unwrap();
    writeln!(md, "| Budgets passed | {} |", report.summary.budgets_passed).unwrap();
    writeln!(md, "| Budgets failed | {} |", report.summary.budgets_failed).unwrap();
    writeln!(md).unwrap();

    // Per-scenario
    writeln!(md, "## Per-Scenario Breakdown").unwrap();
    writeln!(md).unwrap();
    writeln!(md, "| Scenario | Tested | Pass | Fail | P50 | P95 | P99 |").unwrap();
    writeln!(md, "|----------|--------|------|------|-----|-----|-----|").unwrap();
    for ss in report.by_scenario.values() {
        writeln!(
            md,
            "| {} | {} | {} | {} | {}us | {}us | {}us |",
            ss.scenario,
            ss.extensions_tested,
            ss.passed,
            ss.failed,
            ss.aggregate_stats.p50_us,
            ss.aggregate_stats.p95_us,
            ss.aggregate_stats.p99_us,
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    // Budget checks
    writeln!(md, "## Budget Checks").unwrap();
    writeln!(md).unwrap();
    writeln!(md, "| Budget | Threshold | Actual | Worst Ext | Status |").unwrap();
    writeln!(md, "|--------|-----------|--------|-----------|--------|").unwrap();
    for check in &report.budget_checks {
        let actual_str = check
            .actual_us
            .map_or_else(|| "-".to_string(), |v| format!("{v}us"));
        let ext_str = check.worst_extension.as_deref().unwrap_or("-");
        writeln!(
            md,
            "| {} | {}us | {} | {} | {} |",
            check.budget_name, check.threshold_us, actual_str, ext_str, check.status
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    // Per-extension load times (bd-xs79: per-extension detail)
    writeln!(md, "## Per-Extension Load Times").unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "| Extension | Tier | Cold P50 | Cold P95 | Cold P99 | Warm P50 | Warm P95 | Warm P99 |"
    )
    .unwrap();
    writeln!(
        md,
        "|-----------|------|----------|----------|----------|----------|----------|----------|"
    )
    .unwrap();

    // Collect per-extension cold and warm stats
    let mut ext_cold: BTreeMap<&str, &Stats> = BTreeMap::new();
    let mut ext_warm: BTreeMap<&str, &Stats> = BTreeMap::new();
    let mut ext_tier: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &report.results {
        if r.success {
            if r.scenario == "cold_load" {
                ext_cold.insert(&r.extension, &r.stats);
                ext_tier.insert(&r.extension, r.tier);
            } else if r.scenario == "warm_load" {
                ext_warm.insert(&r.extension, &r.stats);
            }
        }
    }
    for (ext, cold) in &ext_cold {
        let tier = ext_tier.get(ext).copied().unwrap_or(0);
        let warm = ext_warm.get(ext);
        writeln!(
            md,
            "| {} | {} | {}us | {}us | {}us | {} | {} | {} |",
            ext,
            tier,
            cold.p50_us,
            cold.p95_us,
            cold.p99_us,
            warm.map_or_else(|| "-".to_string(), |w| format!("{}us", w.p50_us)),
            warm.map_or_else(|| "-".to_string(), |w| format!("{}us", w.p95_us)),
            warm.map_or_else(|| "-".to_string(), |w| format!("{}us", w.p99_us)),
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    md
}

// ─── Test Entry Point ───────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn ext_bench_harness() {
    let mode = bench_mode();
    let max = max_extensions();
    let n = iterations();
    let event_count = event_dispatch_count();
    let env = collect_env_fingerprint();

    let mode_str = match mode {
        BenchMode::Pr => "pr",
        BenchMode::Nightly => "nightly",
        BenchMode::Custom => "custom",
    };

    eprintln!(
        "\n[bench-harness] mode={mode_str} max_extensions={max} iterations={n} events={event_count}"
    );
    eprintln!(
        "[bench-harness] env: {} {} {} cores={} mem={}MB commit={}",
        env.os, env.arch, env.cpu_model, env.cpu_cores, env.mem_total_mb, env.git_commit
    );

    let manifest = load_manifest();
    let entries = if mode == BenchMode::Pr {
        manifest.pr_subset(max)
    } else {
        manifest.safe_extensions(max)
    };

    eprintln!(
        "[bench-harness] selected {} extensions for benchmarking",
        entries.len()
    );

    // ── Run scenarios ──

    let mut all_results: Vec<ScenarioResult> = Vec::new();

    // 1. Cold load
    eprintln!("\n--- Scenario: cold_load ({n} iterations per extension) ---");
    for (i, entry) in entries.iter().enumerate() {
        eprint!(
            "  [{}/{}] {} cold_load ... ",
            i + 1,
            entries.len(),
            entry.id
        );
        let result = bench_cold_load(entry, n, &env);
        if result.success {
            eprintln!(
                "P50={}us P95={}us P99={}us",
                result.stats.p50_us, result.stats.p95_us, result.stats.p99_us
            );
        } else {
            eprintln!("FAILED: {}", result.error.as_deref().unwrap_or("unknown"));
        }
        all_results.push(result);
    }

    // 2. Warm load
    eprintln!("\n--- Scenario: warm_load ({n} iterations per extension) ---");
    for (i, entry) in entries.iter().enumerate() {
        eprint!(
            "  [{}/{}] {} warm_load ... ",
            i + 1,
            entries.len(),
            entry.id
        );
        let result = bench_warm_load(entry, n, &env);
        if result.success {
            eprintln!(
                "P50={}us P95={}us P99={}us",
                result.stats.p50_us, result.stats.p95_us, result.stats.p99_us
            );
        } else {
            eprintln!("FAILED: {}", result.error.as_deref().unwrap_or("unknown"));
        }
        all_results.push(result);
    }

    // 3. Event dispatch (aggregate across loaded extensions)
    eprintln!(
        "\n--- Scenario: event_dispatch ({event_count} events across {} extensions) ---",
        entries.len()
    );
    let event_result = bench_event_dispatch(&entries, event_count, &env);
    eprintln!(
        "  P50={}us P95={}us P99={}us (success={})",
        event_result.stats.p50_us,
        event_result.stats.p95_us,
        event_result.stats.p99_us,
        event_result.success
    );
    all_results.push(event_result);

    // ── JSONL output ──

    let out_dir = output_dir();
    let _ = std::fs::create_dir_all(&out_dir);

    let jsonl_path = out_dir.join("ext_bench_harness.jsonl");
    let jsonl: String = all_results
        .iter()
        .filter_map(|r| serde_json::to_string(r).ok())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&jsonl_path, format!("{jsonl}\n")).expect("write JSONL");
    eprintln!("\n[bench-harness] JSONL: {}", jsonl_path.display());

    // ── Budget checks ──

    let budget_checks = check_budgets(&all_results);
    let budgets_passed = budget_checks.iter().filter(|c| c.status == "PASS").count();
    let budgets_failed = budget_checks.iter().filter(|c| c.status == "FAIL").count();
    let budgets_no_data = budget_checks
        .iter()
        .filter(|c| c.status == "NO_DATA")
        .count();

    // ── Per-scenario summaries ──

    let mut by_scenario: BTreeMap<String, ScenarioSummary> = BTreeMap::new();
    for scenario_name in &["cold_load", "warm_load", "event_dispatch"] {
        let scenario_results: Vec<&ScenarioResult> = all_results
            .iter()
            .filter(|r| r.scenario == *scenario_name)
            .collect();

        let passed = scenario_results.iter().filter(|r| r.success).count();
        let failed = scenario_results.iter().filter(|r| !r.success).count();

        let all_samples: Vec<u64> = scenario_results
            .iter()
            .filter(|r| r.success)
            .flat_map(|r| {
                // Use per-extension p50 as representative sample for aggregate
                std::iter::once(r.stats.p50_us)
            })
            .collect();

        by_scenario.insert(
            (*scenario_name).to_string(),
            ScenarioSummary {
                scenario: (*scenario_name).to_string(),
                extensions_tested: scenario_results.len(),
                passed,
                failed,
                aggregate_stats: Stats::from_micros(&all_samples),
            },
        );
    }

    let total_passed = all_results.iter().filter(|r| r.success).count();
    let total_failed = all_results.iter().filter(|r| !r.success).count();

    // ── Report ──

    let report = HarnessReport {
        schema: "pi.bench.harness_report.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        mode: mode_str.to_string(),
        config: HarnessConfig {
            max_extensions: max,
            iterations: n,
            event_dispatch_count: event_count,
            debug_build: cfg!(debug_assertions),
        },
        env,
        summary: HarnessSummary {
            total_scenarios: all_results.len(),
            total_passed,
            total_failed,
            budgets_passed,
            budgets_failed,
            budgets_no_data,
        },
        by_scenario,
        budget_checks: budget_checks.clone(),
        results: all_results.clone(),
    };

    // Write JSON report
    let report_path = out_dir.join("ext_bench_harness_report.json");
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .expect("write report JSON");
    eprintln!("[bench-harness] Report: {}", report_path.display());

    // Write Markdown report
    let md_path = out_dir.join("BENCH_HARNESS_REPORT.md");
    let md = generate_markdown(&report);
    std::fs::write(&md_path, &md).expect("write markdown report");
    eprintln!("[bench-harness] Markdown: {}", md_path.display());

    // ── Final summary ──

    eprintln!("\n[bench-harness] === SUMMARY ===");
    eprintln!(
        "  Scenarios: {} total, {} pass, {} fail",
        all_results.len(),
        total_passed,
        total_failed
    );
    eprintln!("  Budgets: {budgets_passed} pass, {budgets_failed} fail, {budgets_no_data} no_data");

    for check in &budget_checks {
        let actual_str = check
            .actual_us
            .map_or_else(|| "-".to_string(), |v| format!("{v}us"));
        eprintln!(
            "  {} threshold={}us actual={} → {}",
            check.budget_name, check.threshold_us, actual_str, check.status
        );
    }

    // Assertion: no budget failures in CI-enforced budgets (release only —
    // debug builds are naturally slower and should not gate CI).
    if !cfg!(debug_assertions) {
        let ci_failures: Vec<&BudgetCheck> = budget_checks
            .iter()
            .filter(|c| c.status == "FAIL")
            .collect();
        assert!(
            ci_failures.is_empty(),
            "Budget regressions detected: {:?}",
            ci_failures
                .iter()
                .map(|c| format!(
                    "{}: actual={}us > threshold={}us",
                    c.budget_name,
                    c.actual_us.unwrap_or(0),
                    c.threshold_us
                ))
                .collect::<Vec<_>>()
        );
    }
}
