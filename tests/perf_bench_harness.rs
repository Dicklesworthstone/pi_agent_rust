//! Deterministic benchmark harness for extension startup/exec (bd-20s9).
//!
//! Runs cold start, warm start, tool call overhead, and event hook dispatch
//! scenarios against real extensions from the conformance artifact corpus.
//! Emits JSONL records using `pi.ext.rust_bench.v1` schema with environment
//! fingerprint for repeatable, machine-readable performance tracking.
//!
//! Environment variables:
//!   BENCH_QUICK=1          — PR-safe subset (3 extensions, fewer iterations)
//!   BENCH_ITERATIONS=N     — Override iterations per scenario (default: 20/5)
//!   BENCH_OUTPUT_DIR=path  — Override JSONL output directory
//!
//! Run:
//!   cargo test --test perf_bench_harness -- --nocapture
//!   BENCH_QUICK=1 cargo test --test perf_bench_harness -- --nocapture

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sysinfo::System;

// ─── Configuration ───────────────────────────────────────────────────────────

fn is_quick_mode() -> bool {
    std::env::var("BENCH_QUICK")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn iterations_override() -> Option<usize> {
    std::env::var("BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
}

fn output_dir() -> PathBuf {
    std::env::var("BENCH_OUTPUT_DIR").ok().map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/perf"),
        PathBuf::from,
    )
}

fn artifacts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

/// Extensions used in quick (PR) mode: one simple, one complex.
const QUICK_EXTENSIONS: &[&str] = &["hello", "pirate", "diff"];

/// Extensions used in full (nightly) mode: broader coverage.
const FULL_EXTENSIONS: &[&str] = &[
    "hello",
    "pirate",
    "diff",
    "bookmark",
    "custom-header",
    "custom-footer",
    "confirm-destructive",
    "dirty-repo-guard",
];

// ─── Environment Fingerprint ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvFingerprint {
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    mem_total_mb: u64,
    build_profile: String,
    git_commit: String,
    #[serde(default)]
    features: Vec<String>,
    config_hash: String,
}

fn collect_env_fingerprint() -> EnvFingerprint {
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
    let build_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
    .to_string();
    let git_commit = option_env!("VERGEN_GIT_SHA")
        .unwrap_or("unknown")
        .to_string();

    let config_str = format!(
        "os={os} arch={arch} cpu={cpu_model} cores={cpu_cores} mem={mem_total_mb} profile={build_profile} git={git_commit}"
    );
    let config_hash = sha256_hex(&config_str);

    EnvFingerprint {
        os,
        arch,
        cpu_model,
        cpu_cores,
        mem_total_mb,
        build_profile,
        git_commit,
        features: Vec::new(),
        config_hash,
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ─── Statistics ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Summary {
    count: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
}

fn compute_summary(samples_us: &[f64]) -> Summary {
    if samples_us.is_empty() {
        return Summary {
            count: 0,
            min_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
            mean_ms: 0.0,
        };
    }

    let mut sorted: Vec<f64> = samples_us.iter().map(|us| us / 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let count = sorted.len();
    let sum: f64 = sorted.iter().sum();

    Summary {
        count,
        min_ms: sorted[0],
        p50_ms: percentile_f64(&sorted, 50.0),
        p95_ms: percentile_f64(&sorted, 95.0),
        p99_ms: percentile_f64(&sorted, 99.0),
        max_ms: sorted[count - 1],
        mean_ms: sum / count as f64,
    }
}

fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ─── JSONL Record ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct BenchRecord {
    schema: String,
    runtime: String,
    scenario: String,
    extension: String,
    runs: usize,
    summary: Summary,
    elapsed_ms: f64,
    per_call_us: f64,
    calls_per_sec: f64,
    env: EnvFingerprint,
    timestamp: String,
}

fn emit_jsonl_line(record: &BenchRecord) -> String {
    serde_json::to_string(record).unwrap_or_default()
}

// ─── Extension Helpers ───────────────────────────────────────────────────────

fn find_entry_path(ext_name: &str) -> Option<PathBuf> {
    let dir = artifacts_dir().join(ext_name);
    if !dir.exists() {
        return None;
    }
    // Look for <name>.ts or index.ts
    let ts_file = dir.join(format!("{ext_name}.ts"));
    if ts_file.exists() {
        return Some(ts_file);
    }
    let index_file = dir.join("index.ts");
    if index_file.exists() {
        return Some(index_file);
    }
    // Check for package.json with main field
    let pkg_json = dir.join("package.json");
    if pkg_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            if let Ok(pkg) = serde_json::from_str::<Value>(&content) {
                if let Some(main) = pkg.get("main").and_then(Value::as_str) {
                    let main_path = dir.join(main);
                    if main_path.exists() {
                        return Some(main_path);
                    }
                }
            }
        }
    }
    None
}

fn create_runtime_and_load(
    ext_name: &str,
    entry_path: &Path,
    cwd: &Path,
) -> Option<(ExtensionManager, JsExtensionLoadSpec)> {
    let spec = JsExtensionLoadSpec::from_entry_path(entry_path).ok()?;

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .ok()
        }
    })?;

    manager.set_js_runtime(runtime);

    let load_result = common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });

    if load_result.is_err() {
        eprintln!("[bench] Failed to load {ext_name}: {load_result:?}");
        shutdown_manager(&manager);
        return None;
    }

    Some((manager, spec))
}

fn shutdown_manager(manager: &ExtensionManager) {
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_millis(250)).await }
    });
}

// ─── Scenario Runners ────────────────────────────────────────────────────────

/// Cold start: create fresh runtime + manager, load extension, measure total.
fn run_cold_start(_ext_name: &str, entry_path: &Path, cwd: &Path, iterations: usize) -> Vec<f64> {
    let mut samples_us = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let Ok(spec) = JsExtensionLoadSpec::from_entry_path(entry_path) else {
            continue;
        };

        let manager = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
        let js_config = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            ..Default::default()
        };

        let start = Instant::now();

        let runtime_ok = common::run_async({
            let manager = manager.clone();
            let tools = Arc::clone(&tools);
            async move {
                JsExtensionRuntimeHandle::start(js_config, tools, manager)
                    .await
                    .ok()
            }
        });

        let Some(runtime) = runtime_ok else {
            continue;
        };
        manager.set_js_runtime(runtime);

        let load_ok = common::run_async({
            let manager = manager.clone();
            async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
        });

        let elapsed_us = start.elapsed().as_micros() as f64;
        if load_ok {
            samples_us.push(elapsed_us);
        }

        shutdown_manager(&manager);
    }

    samples_us
}

/// Warm start: reuse existing runtime, reload extension.
fn run_warm_start(
    _ext_name: &str, // used for logging only in callers
    entry_path: &Path,
    cwd: &Path,
    iterations: usize,
) -> Vec<f64> {
    let Ok(spec) = JsExtensionLoadSpec::from_entry_path(entry_path) else {
        return Vec::new();
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime_ok = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .ok()
        }
    });

    let Some(runtime) = runtime_ok else {
        return Vec::new();
    };
    manager.set_js_runtime(runtime);

    // Warmup: load once to prime caches.
    let warmup_ok = common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
    });
    if !warmup_ok {
        shutdown_manager(&manager);
        return Vec::new();
    }

    // Measure subsequent loads.
    let mut samples_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let ok = common::run_async({
            let manager = manager.clone();
            let spec = spec.clone();
            async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
        });
        if ok {
            samples_us.push(start.elapsed().as_micros() as f64);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

/// Tool call overhead: load extension, then call its tool N times.
fn run_tool_call(ext_name: &str, entry_path: &Path, cwd: &Path, iterations: usize) -> Vec<f64> {
    let Some((manager, _spec)) = create_runtime_and_load(ext_name, entry_path, cwd) else {
        return Vec::new();
    };

    let Some(runtime) = manager.js_runtime() else {
        shutdown_manager(&manager);
        return Vec::new();
    };

    // Determine tool name: use the extension name as a best guess.
    let tool_name = ext_name.to_string();
    let ctx_payload = json!({ "hasUI": false, "cwd": cwd.display().to_string() });

    let mut samples_us = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let call_id = format!("bench-{i}");
        let input = json!({"name": "bench"});

        let start = Instant::now();
        let result = futures::executor::block_on(runtime.execute_tool(
            tool_name.clone(),
            call_id,
            input,
            ctx_payload.clone(),
            5_000,
        ));

        let elapsed_us = start.elapsed().as_micros() as f64;
        // Record even if the tool returns an error — we measure dispatch overhead.
        if result.is_ok() {
            samples_us.push(elapsed_us);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

/// Event hook dispatch: load extension, dispatch events N times.
fn run_event_dispatch(
    ext_name: &str,
    entry_path: &Path,
    cwd: &Path,
    iterations: usize,
) -> Vec<f64> {
    let Some((manager, _spec)) = create_runtime_and_load(ext_name, entry_path, cwd) else {
        return Vec::new();
    };

    let event_payload = json!({"systemPrompt": "You are Pi."});

    let mut samples_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let result = common::run_async({
            let manager = manager.clone();
            let payload = event_payload.clone();
            async move {
                manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(payload),
                        5_000,
                    )
                    .await
            }
        });

        let elapsed_us = start.elapsed().as_micros() as f64;
        if result.is_ok() {
            samples_us.push(elapsed_us);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

// ─── Main Harness ────────────────────────────────────────────────────────────

struct HarnessConfig {
    extensions: Vec<String>,
    cold_iterations: usize,
    warm_iterations: usize,
    tool_iterations: usize,
    event_iterations: usize,
}

fn harness_config() -> HarnessConfig {
    let quick = is_quick_mode();
    let base = if quick { 5 } else { 20 };
    let iter_override = iterations_override();

    let extensions: Vec<String> = if quick {
        QUICK_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
    } else {
        FULL_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
    };

    // Filter to extensions that actually exist.
    let extensions: Vec<String> = extensions
        .into_iter()
        .filter(|name| find_entry_path(name).is_some())
        .collect();

    HarnessConfig {
        extensions,
        cold_iterations: iter_override.unwrap_or(base),
        warm_iterations: iter_override.unwrap_or(base),
        tool_iterations: iter_override.unwrap_or(base * 5),
        event_iterations: iter_override.unwrap_or(base * 5),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn bench_extension_scenarios() {
    let config = harness_config();
    let env = collect_env_fingerprint();
    let out_dir = output_dir();
    let _ = std::fs::create_dir_all(&out_dir);

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    eprintln!("\n══════════════════════════════════════════════════════════");
    eprintln!("  Extension Benchmark Harness (bd-20s9)");
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!(
        "  Mode:       {}",
        if is_quick_mode() {
            "QUICK (PR)"
        } else {
            "FULL (nightly)"
        }
    );
    eprintln!("  Extensions: {}", config.extensions.len());
    eprintln!("  Cold:       {} iterations", config.cold_iterations);
    eprintln!("  Warm:       {} iterations", config.warm_iterations);
    eprintln!("  Tool call:  {} iterations", config.tool_iterations);
    eprintln!("  Event hook: {} iterations", config.event_iterations);
    eprintln!(
        "  Env:        {} {} {} cores, {}MB RAM",
        env.os, env.arch, env.cpu_cores, env.mem_total_mb
    );
    eprintln!("  Config:     {}", &env.config_hash[..16]);
    eprintln!("──────────────────────────────────────────────────────────\n");

    let mut records: Vec<BenchRecord> = Vec::new();

    for ext_name in &config.extensions {
        let Some(entry_path) = find_entry_path(ext_name) else {
            eprintln!("[bench] SKIP {ext_name}: no entry file found");
            continue;
        };

        let cwd = std::env::temp_dir().join(format!("pi-bench-harness-{ext_name}"));
        let _ = std::fs::create_dir_all(&cwd);

        // ── Cold Start ──
        {
            let samples = run_cold_start(ext_name, &entry_path, &cwd, config.cold_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[cold_start]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "cold_start".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Warm Start ──
        {
            let samples = run_warm_start(ext_name, &entry_path, &cwd, config.warm_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[warm_start]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "warm_start".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Tool Call Overhead ──
        {
            let samples = run_tool_call(ext_name, &entry_path, &cwd, config.tool_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[tool_call]   {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "tool_call".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Event Hook Dispatch ──
        {
            let samples = run_event_dispatch(ext_name, &entry_path, &cwd, config.event_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[event_hook]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                scenario: "event_hook".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        eprintln!();
    }

    // ── Write JSONL ──
    let jsonl_path = out_dir.join("extension_bench.jsonl");
    let jsonl: String = records
        .iter()
        .map(emit_jsonl_line)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&jsonl_path, format!("{jsonl}\n")).expect("write extension_bench.jsonl");

    // ── Write summary ──
    let scenarios = ["cold_start", "warm_start", "tool_call", "event_hook"];
    let mut summary_text = String::with_capacity(4096);
    summary_text.push_str("# Extension Benchmark Summary\n\n");
    let _ = writeln!(summary_text, "> Generated: {now}\n");
    let _ = writeln!(
        summary_text,
        "Mode: {}\n",
        if is_quick_mode() {
            "QUICK (PR)"
        } else {
            "FULL (nightly)"
        }
    );

    for scenario in &scenarios {
        let _ = writeln!(summary_text, "## {scenario}\n");
        summary_text
            .push_str("| Extension | Runs | p50 (ms) | p95 (ms) | p99 (ms) | Mean (ms) |\n");
        summary_text.push_str("|---|---|---|---|---|---|\n");

        for record in records.iter().filter(|r| r.scenario == *scenario) {
            let _ = writeln!(
                summary_text,
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} |",
                record.extension,
                record.summary.count,
                record.summary.p50_ms,
                record.summary.p95_ms,
                record.summary.p99_ms,
                record.summary.mean_ms,
            );
        }
        summary_text.push('\n');
    }

    let summary_path = out_dir.join("extension_bench_summary.md");
    std::fs::write(&summary_path, &summary_text).expect("write summary");

    // ── Print final summary ──
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!(
        "  Results: {} records across {} extensions",
        records.len(),
        config.extensions.len()
    );
    eprintln!("  JSONL:   {}", jsonl_path.display());
    eprintln!("  Summary: {}", summary_path.display());
    eprintln!("══════════════════════════════════════════════════════════");

    // ── Assertions ──
    // Verify we got at least some data (not all failures).
    assert!(
        records
            .iter()
            .any(|r| r.scenario == "cold_start" && r.summary.count > 0),
        "expected at least one successful cold_start measurement"
    );

    // Budget gate: cold start p99 < 50ms for simple extensions (hello).
    // Only enforced in release builds — debug builds are ~2x slower.
    if !cfg!(debug_assertions) {
        if let Some(hello_cold) = records
            .iter()
            .find(|r| r.scenario == "cold_start" && r.extension == "hello")
        {
            assert!(
                hello_cold.summary.p99_ms < 50.0,
                "hello cold start p99 ({:.2}ms) exceeds 50ms budget",
                hello_cold.summary.p99_ms,
            );
        }
    }
}

/// Validate that the JSONL output conforms to the schema.
#[test]
fn bench_jsonl_schema_valid() {
    let jsonl_path = output_dir().join("extension_bench.jsonl");
    if !jsonl_path.exists() {
        eprintln!("[schema] No extension_bench.jsonl — run bench_extension_scenarios first");
        return;
    }

    let content = std::fs::read_to_string(&jsonl_path).expect("read JSONL");
    let required_fields = [
        "schema",
        "runtime",
        "scenario",
        "extension",
        "runs",
        "summary",
        "env",
    ];

    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i}: invalid JSON: {e}"));

        for field in &required_fields {
            assert!(
                record.get(*field).is_some(),
                "line {i}: missing required field '{field}'"
            );
        }

        assert_eq!(
            record.get("schema").and_then(Value::as_str),
            Some("pi.ext.rust_bench.v1"),
            "line {i}: wrong schema"
        );
        assert_eq!(
            record.get("runtime").and_then(Value::as_str),
            Some("pi_agent_rust"),
            "line {i}: wrong runtime"
        );

        // Validate env fingerprint.
        let env_obj = record.get("env").expect("env field");
        for env_field in &["os", "arch", "cpu_model", "cpu_cores", "config_hash"] {
            assert!(
                env_obj.get(*env_field).is_some(),
                "line {i}: env missing '{env_field}'"
            );
        }

        // Validate summary.
        let summary = record.get("summary").expect("summary field");
        for stat_field in &["count", "min_ms", "p50_ms", "p95_ms", "p99_ms", "max_ms"] {
            assert!(
                summary.get(*stat_field).is_some(),
                "line {i}: summary missing '{stat_field}'"
            );
        }
    }

    let line_count = content.lines().filter(|l| !l.trim().is_empty()).count();
    eprintln!(
        "[schema] Validated {} JSONL records in {}",
        line_count,
        jsonl_path.display()
    );
}
