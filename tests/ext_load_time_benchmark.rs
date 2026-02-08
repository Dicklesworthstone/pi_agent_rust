#![cfg(feature = "ext-conformance")]
//! Extension load-time benchmarks (bd-xs79).
//!
//! Measures extension load time for every extension in the conformance suite.
//! Each extension is measured in two modes:
//! - cold start: fresh `QuickJS` runtime + context (includes context creation)
//! - warm start: same runtime/context after one warmup load (cached module/loader)
//!
//! P50/P95/P99 statistics are computed per extension and aggregated by tier and
//! by "group" (official-simple / official-complex / community).
//!
//! Run:
//!   `cargo test --test ext_load_time_benchmark --features ext-conformance -- --nocapture`
//!
//! Environment variables:
//!   `PI_LOAD_BENCH_ITERATIONS`  - iterations per extension (default: 100)
//!   `PI_LOAD_BENCH_WARMUP`      - warmup loads before warm-start sampling (default: 1)
//!   `PI_LOAD_BENCH_BUDGET_MS`   - P99 budget in ms (default: 100)
//!   `PI_LOAD_BENCH_SCOPE`       - "all" (default) or "official"
//!   `PI_LOAD_BENCH_MAX`         - limit to first N extensions after filtering

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::sync::Arc;

// ─── Configuration ──────────────────────────────────────────────────────────

fn iterations() -> usize {
    std::env::var("PI_LOAD_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn warmup_iterations() -> usize {
    std::env::var("PI_LOAD_BENCH_WARMUP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

fn p99_budget_ms() -> u64 {
    std::env::var("PI_LOAD_BENCH_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchScope {
    All,
    Official,
}

fn scope() -> BenchScope {
    match std::env::var("PI_LOAD_BENCH_SCOPE")
        .ok()
        .unwrap_or_else(|| "all".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "official" => BenchScope::Official,
        _ => BenchScope::All,
    }
}

fn max_extensions() -> Option<usize> {
    std::env::var("PI_LOAD_BENCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        // Legacy alias (prior to scope support).
        .or_else(|| {
            std::env::var("PI_OFFICIAL_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
        })
}

// ─── Manifest types (shared with ext_conformance_generated) ─────────────────

#[derive(Debug, Clone)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    conformance_tier: u32,
}

struct Manifest {
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    fn all(&self) -> Vec<&ManifestEntry> {
        self.extensions.iter().collect()
    }

    fn official(&self) -> Vec<&ManifestEntry> {
        self.extensions
            .iter()
            .filter(|e| {
                !e.id.starts_with("community/")
                    && !e.id.starts_with("npm/")
                    && !e.id.starts_with("third-party/")
                    && !e.id.starts_with("agents-")
            })
            .collect()
    }
}

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

fn reports_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/reports")
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let data = std::fs::read_to_string(manifest_path())
            .expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value =
            serde_json::from_str(&data).expect("Failed to parse VALIDATED_MANIFEST.json");
        let extensions = json["extensions"]
            .as_array()
            .expect("manifest.extensions should be an array")
            .iter()
            .map(|e| ManifestEntry {
                id: e["id"].as_str().unwrap_or("").to_string(),
                entry_path: e["entry_path"].as_str().unwrap_or("").to_string(),
                conformance_tier: u32::try_from(e["conformance_tier"].as_u64().unwrap_or(0))
                    .unwrap_or(0),
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

#[derive(Debug, Clone, serde::Serialize)]
struct LoadStats {
    iterations: usize,
    min_ms: u64,
    max_ms: u64,
    mean_ms: u64,
    p50_ms: u64,
    p95_ms: u64,
    p99_ms: u64,
}

impl LoadStats {
    fn from_samples(samples: &[u64]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let sum: u64 = sorted.iter().sum();
        let count = sorted.len().max(1) as u64;
        Self {
            iterations: sorted.len(),
            min_ms: sorted.first().copied().unwrap_or(0),
            max_ms: sorted.last().copied().unwrap_or(0),
            mean_ms: sum / count,
            p50_ms: percentile(&sorted, 50.0),
            p95_ms: percentile(&sorted, 95.0),
            p99_ms: percentile(&sorted, 99.0),
        }
    }
}

// ─── Per-extension result ───────────────────────────────────────────────────

fn is_official_id(id: &str) -> bool {
    !id.starts_with("community/")
        && !id.starts_with("npm/")
        && !id.starts_with("third-party/")
        && !id.starts_with("agents-")
}

fn group_for(entry: &ManifestEntry) -> String {
    if !is_official_id(&entry.id) {
        return "community".to_string();
    }

    if entry.conformance_tier <= 3 {
        "official-simple".to_string()
    } else {
        "official-complex".to_string()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct LoadPhase {
    stats: LoadStats,
    samples_ms: Vec<u64>,
    failures: usize,
}

impl LoadPhase {
    fn from_samples(samples_ms: Vec<u64>, failures: usize) -> Self {
        let stats = LoadStats::from_samples(&samples_ms);
        Self {
            stats,
            samples_ms,
            failures,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct ExtLoadResult {
    id: String,
    tier: u32,
    group: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    cold: LoadPhase,
    warm: LoadPhase,
}

// ─── Core benchmark runner ──────────────────────────────────────────────────

/// Benchmark one extension in both cold-start and warm-start modes.
///
/// Cold-start samples create a fresh `QuickJS` runtime+context each iteration.
/// Warm-start samples reuse a single runtime+context after `warmup` loads.
#[allow(clippy::too_many_lines)]
fn benchmark_extension(entry: &ManifestEntry, warmup: usize, n: usize) -> ExtLoadResult {
    let entry_file = artifacts_dir().join(&entry.entry_path);
    let group = group_for(entry);
    if !entry_file.exists() {
        return ExtLoadResult {
            id: entry.id.clone(),
            tier: entry.conformance_tier,
            group,
            success: false,
            error: Some(format!("Artifact not found: {}", entry_file.display())),
            cold: LoadPhase::from_samples(vec![], 0),
            warm: LoadPhase::from_samples(vec![], 0),
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ExtLoadResult {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                group,
                success: false,
                error: Some(format!("Load spec error: {e}")),
                cold: LoadPhase::from_samples(vec![], 0),
                warm: LoadPhase::from_samples(vec![], 0),
            };
        }
    };

    let cwd = std::env::temp_dir().join(format!("pi-loadbench-{}", entry.id.replace('/', "_")));
    let _ = std::fs::create_dir_all(&cwd);

    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    // ── Cold-start ─────────────────────────────────────────────────────────

    let mut cold_samples = Vec::with_capacity(n);
    let mut cold_failures = 0usize;
    let mut last_error = None::<String>;

    for _ in 0..n {
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
                cold_failures += 1;
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
                #[allow(clippy::cast_possible_truncation)]
                let elapsed_ms = start.elapsed().as_millis() as u64;
                cold_samples.push(elapsed_ms);
            }
            Err(e) => {
                cold_failures += 1;
                last_error = Some(format!("Load error: {e}"));
            }
        }

        // Shut down to avoid thread leaks.
        common::run_async({
            async move {
                let _ = manager.shutdown(Duration::from_millis(250)).await;
            }
        });
    }

    // ── Warm-start ─────────────────────────────────────────────────────────

    let mut warm_samples = Vec::with_capacity(n);
    let mut warm_failures = 0usize;

    // Start one runtime/context, warm it, then sample repeated loads.
    let warm_manager = ExtensionManager::new();
    let warm_runtime_result = common::run_async({
        let manager = warm_manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    match warm_runtime_result {
        Ok(rt) => warm_manager.set_js_runtime(rt),
        Err(e) => {
            last_error = Some(format!("Warm runtime start error: {e}"));
            warm_failures += n.max(1);
        }
    }

    if warm_manager.js_runtime().is_some() {
        for _ in 0..warmup {
            let load_result = common::run_async({
                let manager = warm_manager.clone();
                let spec = spec.clone();
                async move { manager.load_js_extensions(vec![spec]).await }
            });
            if let Err(e) = load_result {
                last_error = Some(format!("Warmup load error: {e}"));
                warm_failures += n.max(1);
                break;
            }
        }

        if warm_failures == 0 {
            for _ in 0..n {
                let start = Instant::now();
                let load_result = common::run_async({
                    let manager = warm_manager.clone();
                    let spec = spec.clone();
                    async move { manager.load_js_extensions(vec![spec]).await }
                });
                match load_result {
                    Ok(()) => {
                        #[allow(clippy::cast_possible_truncation)]
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        warm_samples.push(elapsed_ms);
                    }
                    Err(e) => {
                        warm_failures += 1;
                        last_error = Some(format!("Warm load error: {e}"));
                    }
                }
            }
        }

        common::run_async({
            async move {
                let _ = warm_manager.shutdown(Duration::from_millis(250)).await;
            }
        });
    }

    let success = cold_failures == 0
        && warm_failures == 0
        && cold_samples.len() == n
        && warm_samples.len() == n;
    ExtLoadResult {
        id: entry.id.clone(),
        tier: entry.conformance_tier,
        group,
        success,
        error: if success { None } else { last_error },
        cold: LoadPhase::from_samples(cold_samples, cold_failures),
        warm: LoadPhase::from_samples(warm_samples, warm_failures),
    }
}

// ─── Tier aggregation ───────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
struct TierStats {
    tier: u32,
    count: usize,
    cold: LoadStats,
    warm: LoadStats,
    over_budget_cold: Vec<String>,
    over_budget_warm: Vec<String>,
}

fn aggregate_by_tier(results: &[ExtLoadResult], budget_ms: u64) -> Vec<TierStats> {
    let mut by_tier: BTreeMap<u32, Vec<&ExtLoadResult>> = BTreeMap::new();
    for r in results {
        by_tier.entry(r.tier).or_default().push(r);
    }

    by_tier
        .into_iter()
        .map(|(tier, exts)| {
            let all_cold_samples: Vec<u64> = exts
                .iter()
                .filter(|e| e.success)
                .flat_map(|e| e.cold.samples_ms.iter().copied())
                .collect();
            let all_warm_samples: Vec<u64> = exts
                .iter()
                .filter(|e| e.success)
                .flat_map(|e| e.warm.samples_ms.iter().copied())
                .collect();

            let over_budget_cold: Vec<String> = exts
                .iter()
                .filter(|e| e.success && e.cold.stats.p99_ms > budget_ms)
                .map(|e| format!("{} (P99={}ms)", e.id, e.cold.stats.p99_ms))
                .collect();
            let over_budget_warm: Vec<String> = exts
                .iter()
                .filter(|e| e.success && e.warm.stats.p99_ms > budget_ms)
                .map(|e| format!("{} (P99={}ms)", e.id, e.warm.stats.p99_ms))
                .collect();
            TierStats {
                tier,
                count: exts.len(),
                cold: LoadStats::from_samples(&all_cold_samples),
                warm: LoadStats::from_samples(&all_warm_samples),
                over_budget_cold,
                over_budget_warm,
            }
        })
        .collect()
}

// ─── Group aggregation (official-simple / official-complex / community) ─────

#[derive(Debug, Clone, serde::Serialize)]
struct GroupStats {
    group: String,
    count: usize,
    cold: LoadStats,
    warm: LoadStats,
    over_budget_cold: Vec<String>,
    over_budget_warm: Vec<String>,
}

fn aggregate_by_group(results: &[ExtLoadResult], budget_ms: u64) -> Vec<GroupStats> {
    let mut by_group: BTreeMap<String, Vec<&ExtLoadResult>> = BTreeMap::new();
    for r in results {
        by_group.entry(r.group.clone()).or_default().push(r);
    }

    by_group
        .into_iter()
        .map(|(group, exts)| {
            let cold_samples: Vec<u64> = exts
                .iter()
                .filter(|e| e.success)
                .flat_map(|e| e.cold.samples_ms.iter().copied())
                .collect();
            let warm_samples: Vec<u64> = exts
                .iter()
                .filter(|e| e.success)
                .flat_map(|e| e.warm.samples_ms.iter().copied())
                .collect();

            let over_budget_cold: Vec<String> = exts
                .iter()
                .filter(|e| e.success && e.cold.stats.p99_ms > budget_ms)
                .map(|e| format!("{} (P99={}ms)", e.id, e.cold.stats.p99_ms))
                .collect();
            let over_budget_warm: Vec<String> = exts
                .iter()
                .filter(|e| e.success && e.warm.stats.p99_ms > budget_ms)
                .map(|e| format!("{} (P99={}ms)", e.id, e.warm.stats.p99_ms))
                .collect();

            GroupStats {
                group,
                count: exts.len(),
                cold: LoadStats::from_samples(&cold_samples),
                warm: LoadStats::from_samples(&warm_samples),
                over_budget_cold,
                over_budget_warm,
            }
        })
        .collect()
}

// ─── Report generation ──────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct BenchmarkReport {
    generated_at: String,
    config: BenchmarkConfig,
    summary: BenchmarkSummary,
    tiers: Vec<TierStats>,
    groups: Vec<GroupStats>,
    results: Vec<ExtLoadResult>,
}

#[derive(Debug, serde::Serialize)]
struct BenchmarkConfig {
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_extensions: Option<usize>,
    iterations: usize,
    warmup_iterations: usize,
    budget_ms: u64,
    debug_build: bool,
}

#[derive(Debug, serde::Serialize)]
struct BenchmarkSummary {
    total: usize,
    success: usize,
    failed: usize,
    over_budget_any: usize,
    over_budget_cold: usize,
    over_budget_warm: usize,
    global_cold_p50_ms: u64,
    global_cold_p95_ms: u64,
    global_cold_p99_ms: u64,
    global_warm_p50_ms: u64,
    global_warm_p95_ms: u64,
    global_warm_p99_ms: u64,
}

#[allow(clippy::too_many_lines)]
fn generate_markdown(report: &BenchmarkReport) -> String {
    let mut md = String::with_capacity(8192);
    writeln!(md, "# Extension Load-Time Benchmark Report").unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "Generated: {} | Scope: {}{} | Iterations: {} | Warmup: {} | Budget (P99): {}ms | Build: {}",
        report.generated_at,
        report.config.scope,
        report
            .config
            .max_extensions
            .map_or_else(String::new, |max| format!(" (max {max})")),
        report.config.iterations,
        report.config.warmup_iterations,
        report.config.budget_ms,
        if report.config.debug_build {
            "debug"
        } else {
            "release"
        }
    )
    .unwrap();
    writeln!(md).unwrap();

    // Summary
    writeln!(md, "## Summary").unwrap();
    writeln!(md).unwrap();
    writeln!(md, "| Metric | Value |").unwrap();
    writeln!(md, "|--------|-------|").unwrap();
    writeln!(md, "| Total extensions | {} |", report.summary.total).unwrap();
    writeln!(md, "| Loaded successfully | {} |", report.summary.success).unwrap();
    writeln!(md, "| Failed to load | {} |", report.summary.failed).unwrap();
    writeln!(
        md,
        "| Over budget (any P99 > {}ms) | {} |",
        report.config.budget_ms, report.summary.over_budget_any
    )
    .unwrap();
    writeln!(
        md,
        "| Over budget (cold) | {} |",
        report.summary.over_budget_cold
    )
    .unwrap();
    writeln!(
        md,
        "| Over budget (warm) | {} |",
        report.summary.over_budget_warm
    )
    .unwrap();
    writeln!(
        md,
        "| Global cold P50 | {}ms |",
        report.summary.global_cold_p50_ms
    )
    .unwrap();
    writeln!(
        md,
        "| Global cold P95 | {}ms |",
        report.summary.global_cold_p95_ms
    )
    .unwrap();
    writeln!(
        md,
        "| Global cold P99 | {}ms |",
        report.summary.global_cold_p99_ms
    )
    .unwrap();
    writeln!(
        md,
        "| Global warm P50 | {}ms |",
        report.summary.global_warm_p50_ms
    )
    .unwrap();
    writeln!(
        md,
        "| Global warm P95 | {}ms |",
        report.summary.global_warm_p95_ms
    )
    .unwrap();
    writeln!(
        md,
        "| Global warm P99 | {}ms |",
        report.summary.global_warm_p99_ms
    )
    .unwrap();
    writeln!(md).unwrap();

    // Per-tier
    writeln!(md, "## Per-Tier Breakdown").unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "| Tier | Count | Cold P50 | Cold P95 | Cold P99 | Warm P50 | Warm P95 | Warm P99 | Over Cold | Over Warm |"
    )
    .unwrap();
    writeln!(
        md,
        "|------|-------|----------|----------|----------|----------|----------|----------|----------|----------|"
    )
    .unwrap();
    for t in &report.tiers {
        writeln!(
            md,
            "| {} | {} | {}ms | {}ms | {}ms | {}ms | {}ms | {}ms | {} | {} |",
            t.tier,
            t.count,
            t.cold.p50_ms,
            t.cold.p95_ms,
            t.cold.p99_ms,
            t.warm.p50_ms,
            t.warm.p95_ms,
            t.warm.p99_ms,
            t.over_budget_cold.len(),
            t.over_budget_warm.len()
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    // Per-group
    writeln!(md, "## Per-Group Breakdown").unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "| Group | Count | Cold P50 | Cold P95 | Cold P99 | Warm P50 | Warm P95 | Warm P99 | Over Cold | Over Warm |"
    )
    .unwrap();
    writeln!(
        md,
        "|-------|-------|----------|----------|----------|----------|----------|----------|----------|----------|"
    )
    .unwrap();
    for g in &report.groups {
        writeln!(
            md,
            "| {} | {} | {}ms | {}ms | {}ms | {}ms | {}ms | {}ms | {} | {} |",
            g.group,
            g.count,
            g.cold.p50_ms,
            g.cold.p95_ms,
            g.cold.p99_ms,
            g.warm.p50_ms,
            g.warm.p95_ms,
            g.warm.p99_ms,
            g.over_budget_cold.len(),
            g.over_budget_warm.len()
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    // Per-extension table (sorted by max(cold P99, warm P99) desc for triage)
    writeln!(
        md,
        "## Per-Extension Results (sorted by max(P99 cold, P99 warm) desc)"
    )
    .unwrap();
    writeln!(md).unwrap();
    writeln!(
        md,
        "| Extension | Group | Tier | Cold P50 | Cold P95 | Cold P99 | Warm P50 | Warm P95 | Warm P99 | Status |"
    )
    .unwrap();
    writeln!(
        md,
        "|-----------|-------|------|----------|----------|----------|----------|----------|----------|--------|"
    )
    .unwrap();

    let mut sorted_results: Vec<&ExtLoadResult> = report.results.iter().collect();
    sorted_results.sort_by(|a, b| {
        let a_p99 = a.cold.stats.p99_ms.max(a.warm.stats.p99_ms);
        let b_p99 = b.cold.stats.p99_ms.max(b.warm.stats.p99_ms);
        b_p99.cmp(&a_p99)
    });

    for r in sorted_results {
        let status = if !r.success {
            "FAIL"
        } else if r.cold.stats.p99_ms > report.config.budget_ms
            || r.warm.stats.p99_ms > report.config.budget_ms
        {
            "OVER"
        } else {
            "OK"
        };
        writeln!(
            md,
            "| {} | {} | {} | {}ms | {}ms | {}ms | {}ms | {}ms | {}ms | {} |",
            r.id,
            r.group,
            r.tier,
            r.cold.stats.p50_ms,
            r.cold.stats.p95_ms,
            r.cold.stats.p99_ms,
            r.warm.stats.p50_ms,
            r.warm.stats.p95_ms,
            r.warm.stats.p99_ms,
            status
        )
        .unwrap();
    }

    md
}

// ─── Test entry point ───────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn load_time_benchmark() {
    let manifest = load_manifest();
    let n = iterations();
    let warmup = warmup_iterations();
    let budget_ms = p99_budget_ms();
    let max = max_extensions();
    let scope = scope();

    let mut entries: Vec<&ManifestEntry> = match scope {
        BenchScope::Official => manifest.official(),
        BenchScope::All => manifest.all(),
    };
    if let Some(limit) = max {
        entries.truncate(limit);
    }

    eprintln!(
        "[load-bench] scope={scope:?} extensions={} iterations={} warmup={} budget={}ms debug={}",
        entries.len(),
        n,
        warmup,
        budget_ms,
        cfg!(debug_assertions)
    );

    let mut results = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        eprint!("  [{}/{}] {} ... ", i + 1, entries.len(), entry.id);
        let result = benchmark_extension(entry, warmup, n);
        if result.success {
            eprintln!(
                "cold P50={}ms P99={}ms | warm P50={}ms P99={}ms",
                result.cold.stats.p50_ms,
                result.cold.stats.p99_ms,
                result.warm.stats.p50_ms,
                result.warm.stats.p99_ms
            );
        } else {
            eprintln!("FAILED: {}", result.error.as_deref().unwrap_or("unknown"));
        }
        results.push(result);
    }

    // Compute statistics.
    let all_cold_samples: Vec<u64> = results
        .iter()
        .filter(|r| r.success)
        .flat_map(|r| r.cold.samples_ms.iter().copied())
        .collect();
    let all_warm_samples: Vec<u64> = results
        .iter()
        .filter(|r| r.success)
        .flat_map(|r| r.warm.samples_ms.iter().copied())
        .collect();

    let global_cold_stats = LoadStats::from_samples(&all_cold_samples);
    let global_warm_stats = LoadStats::from_samples(&all_warm_samples);

    let tiers = aggregate_by_tier(&results, budget_ms);
    let groups = aggregate_by_group(&results, budget_ms);

    let success_count = results.iter().filter(|r| r.success).count();
    let failed_count = results.iter().filter(|r| !r.success).count();
    let over_budget_cold_count = results
        .iter()
        .filter(|r| r.success && r.cold.stats.p99_ms > budget_ms)
        .count();
    let over_budget_warm_count = results
        .iter()
        .filter(|r| r.success && r.warm.stats.p99_ms > budget_ms)
        .count();
    let over_budget_any_count = results
        .iter()
        .filter(|r| {
            r.success && (r.cold.stats.p99_ms > budget_ms || r.warm.stats.p99_ms > budget_ms)
        })
        .count();

    let report = BenchmarkReport {
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        config: BenchmarkConfig {
            scope: match scope {
                BenchScope::Official => "official".to_string(),
                BenchScope::All => "all".to_string(),
            },
            max_extensions: max,
            iterations: n,
            warmup_iterations: warmup,
            budget_ms,
            debug_build: cfg!(debug_assertions),
        },
        summary: BenchmarkSummary {
            total: results.len(),
            success: success_count,
            failed: failed_count,
            over_budget_any: over_budget_any_count,
            over_budget_cold: over_budget_cold_count,
            over_budget_warm: over_budget_warm_count,
            global_cold_p50_ms: global_cold_stats.p50_ms,
            global_cold_p95_ms: global_cold_stats.p95_ms,
            global_cold_p99_ms: global_cold_stats.p99_ms,
            global_warm_p50_ms: global_warm_stats.p50_ms,
            global_warm_p95_ms: global_warm_stats.p95_ms,
            global_warm_p99_ms: global_warm_stats.p99_ms,
        },
        tiers,
        groups,
        results,
    };

    // Write reports.
    let dir = reports_dir();
    let _ = std::fs::create_dir_all(&dir);

    let json_path = dir.join("load_time_benchmark_detailed.json");
    let json_data = serde_json::to_string_pretty(&report).expect("serialize report");
    std::fs::write(&json_path, &json_data).expect("write JSON report");
    eprintln!("\n  JSON: {}", json_path.display());

    let md_path = dir.join("LOAD_TIME_BENCHMARK.md");
    let md_data = generate_markdown(&report);
    std::fs::write(&md_path, &md_data).expect("write markdown report");
    eprintln!("  Markdown: {}", md_path.display());

    // Summary.
    eprintln!("\n[load-bench] SUMMARY:");
    eprintln!(
        "  Total: {} | Pass: {} | Fail: {} | Over budget: {}",
        report.summary.total,
        report.summary.success,
        report.summary.failed,
        report.summary.over_budget_any
    );
    eprintln!(
        "  Global cold P50={}ms P95={}ms P99={}ms",
        global_cold_stats.p50_ms, global_cold_stats.p95_ms, global_cold_stats.p99_ms
    );
    eprintln!(
        "  Global warm P50={}ms P95={}ms P99={}ms",
        global_warm_stats.p50_ms, global_warm_stats.p95_ms, global_warm_stats.p99_ms
    );

    if over_budget_any_count > 0 {
        eprintln!(
            "\n  OVER-BUDGET: {over_budget_any_count} extension(s) exceeded P99 budget of {budget_ms}ms:"
        );
        for r in &report.results {
            if r.success && (r.cold.stats.p99_ms > budget_ms || r.warm.stats.p99_ms > budget_ms) {
                eprintln!(
                    "    - {} (cold P99={}ms, warm P99={}ms)",
                    r.id, r.cold.stats.p99_ms, r.warm.stats.p99_ms
                );
            }
        }
    }

    // Hard assertions: all extensions must load successfully, and must meet the budget.
    assert_eq!(
        failed_count, 0,
        "{failed_count} extension(s) failed to load"
    );
    assert_eq!(
        over_budget_any_count, 0,
        "{over_budget_any_count} extension(s) exceeded the P99 budget of {budget_ms}ms"
    );
}
