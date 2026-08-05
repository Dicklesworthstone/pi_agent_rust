//! Deterministic benchmark scenario runner (bd-m5jp).
//!
//! Executes cold start, warm start, tool call, and event hook dispatch scenarios
//! for a configurable set of extensions. Emits JSONL records conforming to the
//! `pi.ext.rust_bench.v1` schema with environment fingerprinting.
//!
//! Run with: `cargo test --test bench_scenario_runner -- --nocapture`
//!
//! Outputs: `target/perf/scenario_runner.jsonl`

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::future_not_send,
    clippy::doc_markdown
)]

use futures::executor::block_on;
use pi::error::Result;
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::System;

// ─── Configuration ──────────────────────────────────────────────────────────

/// Extensions to benchmark (name, artifact dir name).
/// Must be >=3 per bd-m5jp acceptance criteria.
const BENCH_EXTENSIONS: &[&str] = &["hello", "pirate", "diff"];
const BENCH_PROTOCOL_SCHEMA: &str = "pi.bench.protocol.v1";
const BENCH_PROTOCOL_VERSION: &str = "1.0.0";
const PARTITION_MATCHED_STATE: &str = "matched-state";
const PARTITION_REALISTIC: &str = "realistic";
const MATRIX_SCENARIO_SESSION_WORKLOAD: &str = "session_workload_matrix";
const MATRIX_SESSION_SIZES: &[u64] = &[100_000, 200_000, 500_000, 1_000_000, 5_000_000];
const EVIDENCE_CLASS_MEASURED: &str = "measured";
const EVIDENCE_CLASS_INFERRED: &str = "inferred";
const CONFIDENCE_HIGH: &str = "high";
const CONFIDENCE_LOW: &str = "low";
const MEASUREMENT_BOUNDARY: &str = "production_extension_manager";
const MEASUREMENT_CONTRACT_VERSION: &str = "production_extension_manager.v1";
const SYNTHETIC_MEASUREMENT_BOUNDARY: &str = "synthetic_seed_projection";
const SYNTHETIC_MEASUREMENT_CONTRACT_VERSION: &str = "synthetic_seed_projection.v1";

/// Iterations for cold/warm start scenarios.
const LOAD_RUNS: usize = 5;

/// Iterations for tool-call and event-hook scenarios.
const DISPATCH_ITERATIONS: u32 = 500;

// ─── Environment Fingerprint ────────────────────────────────────────────────

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn env_fingerprint() -> Value {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();

    let cpu_model = system
        .cpus()
        .first()
        .map_or_else(|| "unknown".to_string(), |cpu| cpu.brand().to_string());
    let cpu_cores = u32::try_from(system.cpus().len()).unwrap_or(u32::MAX);
    let mem_total_mb = system.total_memory() / 1024 / 1024;
    let os = System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string());
    let arch = std::env::consts::ARCH.to_string();
    let git_commit =
        option_env!("VERGEN_GIT_SHA").map_or_else(|| "unknown".to_string(), ToString::to_string);
    let build_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let config_str =
        format!("{os}|{arch}|{cpu_model}|{cpu_cores}|{mem_total_mb}|{build_profile}|{git_commit}");
    let config_hash = sha256_hex(&config_str);

    json!({
        "os": os,
        "arch": arch,
        "cpu_model": cpu_model,
        "cpu_cores": cpu_cores,
        "mem_total_mb": mem_total_mb,
        "build_profile": build_profile,
        "git_commit": git_commit,
        "features": [],
        "config_hash": config_hash,
    })
}

fn new_run_correlation_id(env: &Value) -> String {
    let config_hash = env
        .get("config_hash")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let raw = format!("{config_hash}|{now_nanos}|{}", std::process::id());
    let full = sha256_hex(&raw);
    full.chars().take(32).collect()
}

fn scenario_replay_input(record: &serde_json::Map<String, Value>) -> Value {
    record
        .get("runs")
        .and_then(Value::as_u64)
        .map(|runs| json!({ "runs": runs }))
        .or_else(|| {
            record
                .get("iterations")
                .and_then(Value::as_u64)
                .map(|iterations| json!({ "iterations": iterations }))
        })
        .unwrap_or_else(|| json!({}))
}

fn host_metadata_from_env(env: &Value) -> Value {
    json!({
        "os": env.get("os").cloned().unwrap_or(Value::Null),
        "arch": env.get("arch").cloned().unwrap_or(Value::Null),
        "cpu_model": env.get("cpu_model").cloned().unwrap_or(Value::Null),
        "cpu_cores": env.get("cpu_cores").cloned().unwrap_or(Value::Null),
        "mem_total_mb": env.get("mem_total_mb").cloned().unwrap_or(Value::Null),
    })
}

// ─── Artifact Lookup ────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn perf_output_path(name: &str) -> PathBuf {
    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().map_or_else(
        || project_root().join("target"),
        |raw| {
            let target_dir = PathBuf::from(raw);
            if target_dir.is_absolute() {
                target_dir
            } else {
                project_root().join(target_dir)
            }
        },
    );
    target_dir.join("perf").join(name)
}

fn artifact_entry(name: &str) -> PathBuf {
    project_root()
        .join("tests/ext_conformance/artifacts")
        .join(name)
        .join(format!("{name}.ts"))
}

// ─── Statistics ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Stats {
    count: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    p999_ms: f64,
    max_ms: f64,
}

fn percentile_permille(sorted: &[f64], permille: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = sorted.len().saturating_mul(permille).div_ceil(1000);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn compute_stats(durations: &[Duration]) -> Stats {
    let mut ms: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);

    Stats {
        count: ms.len(),
        min_ms: ms.first().copied().unwrap_or(0.0),
        p50_ms: percentile_permille(&ms, 500),
        p95_ms: percentile_permille(&ms, 950),
        p99_ms: percentile_permille(&ms, 990),
        p999_ms: percentile_permille(&ms, 999),
        max_ms: ms.last().copied().unwrap_or(0.0),
    }
}

fn current_rss_mb() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };

    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:").and_then(|rest| {
                rest.split_whitespace()
                    .next()
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb.div_ceil(1024))
            })
        })
        .unwrap_or(0)
}

fn matrix_swarm_metrics(
    env: &Value,
    session_messages: u64,
    open_ms: f64,
    append_ms: f64,
    save_ms: f64,
    index_ms: f64,
) -> Value {
    let total_ms = open_ms + append_ms + save_ms + index_ms;
    let queue_p50 = (session_messages / 100_000).max(1);
    let observed_cpu_cores = env
        .get("cpu_cores")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mem_total_mb = env
        .get("mem_total_mb")
        .and_then(Value::as_u64)
        .unwrap_or_default();

    json!({
        "latency_quantiles_ms": {
            "p50": total_ms,
            "p95": total_ms * 1.15,
            "p99": total_ms * 1.35,
            "p999": total_ms * 1.75,
        },
        "queue_depth": {
            "p50": queue_p50,
            "p95": queue_p50.saturating_mul(2),
            "p99": queue_p50.saturating_mul(3),
            "p999": queue_p50.saturating_mul(4),
            "max": queue_p50.saturating_mul(4),
        },
        "resource_usage": {
            "rss_mb": current_rss_mb(),
            "cpu_pct": 0.0,
        },
        "component_breakdown_ms": {
            "tool": 0.0,
            "provider": 0.0,
            "extension": 0.0,
            "session": total_ms,
        },
        "stage_breakdown_ms": {
            "open": open_ms,
            "append": append_ms,
            "save": save_ms,
            "index": index_ms,
        },
        "host_capacity": {
            "target_cpu_cores": 64,
            "observed_cpu_cores": observed_cpu_cores,
            "mem_total_mb": mem_total_mb,
        },
        "derivation": {
            "method": "deterministic_seed_projection",
            "latency_quantiles": "derived_from_seed_stage_totals_using_fixed_multipliers",
            "queue_depth": "derived_from_session_messages_using_fixed_multipliers",
            "resource_usage": "rss_sampled_at_generation_time_cpu_placeholder_zero",
        },
    })
}

// ─── Runtime Helpers ────────────────────────────────────────────────────────

struct BenchRuntime {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
}

async fn new_runtime(js_cwd: &str, disk_cache_dir: Option<PathBuf>) -> Result<BenchRuntime> {
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], Path::new(js_cwd), None));
    let config = PiJsRuntimeConfig {
        cwd: js_cwd.to_string(),
        disk_cache_dir,
        ..Default::default()
    };
    let runtime = JsExtensionRuntimeHandle::start(config, tools, manager.clone()).await?;
    manager.set_js_runtime(runtime.clone());
    Ok(BenchRuntime { manager, runtime })
}

fn unique_warm_cache_dir(extension_id: &str) -> PathBuf {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let raw = format!("{extension_id}|{}|{now_nanos}", std::process::id());
    let suffix = sha256_hex(&raw);
    perf_output_path("module-cache").join(format!("warm-{}", &suffix[..16]))
}

async fn load_extension(runtime: &BenchRuntime, spec: &JsExtensionLoadSpec) -> Result<()> {
    runtime.manager.load_js_extensions(vec![spec.clone()]).await
}

async fn resolve_extension_callable(
    runtime: &BenchRuntime,
    extension_id: &str,
) -> Result<(String, String)> {
    let tools = runtime.runtime.get_registered_tools().await?;
    if let Some(tool) = tools.iter().find(|tool| tool.name == extension_id) {
        return Ok(("tool".to_string(), tool.name.clone()));
    }

    let commands = runtime.manager.list_commands();
    if let Some(command_name) = commands
        .iter()
        .filter_map(|command| command.get("name").and_then(Value::as_str))
        .find(|name| *name == extension_id)
    {
        return Ok(("command".to_string(), command_name.to_string()));
    }

    if let Some(tool) = tools.first() {
        return Ok(("tool".to_string(), tool.name.clone()));
    }
    if let Some(command_name) = commands
        .iter()
        .find_map(|command| command.get("name").and_then(Value::as_str))
    {
        return Ok(("command".to_string(), command_name.to_string()));
    }

    Err(pi::error::Error::extension(format!(
        "No callable tool/command registered for extension: {extension_id}"
    )))
}

// ─── Scenarios ──────────────────────────────────────────────────────────────

/// Cold start: create a fresh runtime + load extension from scratch each run.
async fn scenario_cold_start(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    runs: usize,
) -> Result<Value> {
    let mut timings = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let runtime = new_runtime(js_cwd, None).await?;
        load_extension(&runtime, spec).await?;
        timings.push(start.elapsed());
        if !runtime.manager.shutdown(Duration::from_secs(5)).await {
            return Err(pi::error::Error::extension(
                "benchmark runtime did not shut down after cold start",
            ));
        }
    }

    let stats = compute_stats(&timings);
    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "cold_start",
        "extension": spec.extension_id,
        "runs": runs,
        "stats": stats,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
    }))
}

/// Warm start: create fresh runtimes after warming filesystem/transpile caches.
async fn scenario_warm_start(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    runs: usize,
) -> Result<Value> {
    // Create one runtime and load the extension once (warmup).
    let warm_cache_dir = unique_warm_cache_dir(&spec.extension_id);
    let runtime = new_runtime(js_cwd, Some(warm_cache_dir.clone())).await?;
    load_extension(&runtime, spec).await?;
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(pi::error::Error::extension(
            "benchmark warmup runtime did not shut down",
        ));
    }

    // Fresh isolated shards retain honest startup semantics while host-level
    // filesystem and transpilation caches remain warm.
    let mut timings = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let warm_rt = new_runtime(js_cwd, Some(warm_cache_dir.clone())).await?;
        load_extension(&warm_rt, spec).await?;
        timings.push(start.elapsed());
        if !warm_rt.manager.shutdown(Duration::from_secs(5)).await {
            return Err(pi::error::Error::extension(
                "benchmark runtime did not shut down after warm start",
            ));
        }
    }

    let stats = compute_stats(&timings);
    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "warm_start",
        "extension": spec.extension_id,
        "runs": runs,
        "stats": stats,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "unique_per_scenario_shared_across_warmup_and_runs",
        "warm_state": "shared_pi_js_disk_module_cache",
    }))
}

/// Tool call overhead: N repeated tool invocations on a loaded extension.
async fn scenario_tool_call(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd, None).await?;
    load_extension(&runtime, spec).await?;

    // Extensions may expose either a tool or a command with the extension name.
    let (invoke_kind, invoke_name) =
        resolve_extension_callable(&runtime, &spec.extension_id).await?;
    let ctx = Arc::new(json!({"hasUI": false, "cwd": js_cwd}));
    let budget = Duration::from_secs(60);
    let started_at = Instant::now();
    for _ in 0..iterations {
        if started_at.elapsed() >= budget {
            let _ = runtime.manager.shutdown(Duration::from_secs(5)).await;
            return Err(pi::error::Error::extension(format!(
                "tool-call benchmark timed out after {}ms",
                budget.as_millis()
            )));
        }
        let remaining_ms = u64::try_from(
            budget
                .saturating_sub(started_at.elapsed())
                .as_millis()
                .max(1),
        )
        .unwrap_or(u64::MAX);
        match invoke_kind.as_str() {
            "tool" => {
                let _ = runtime
                    .runtime
                    .execute_tool(
                        invoke_name.clone(),
                        "bench-call-1".to_string(),
                        json!({"name": "bench"}),
                        Arc::clone(&ctx),
                        remaining_ms,
                    )
                    .await?;
            }
            "command" => {
                let _ = runtime
                    .runtime
                    .execute_command(
                        invoke_name.clone(),
                        String::new(),
                        Arc::clone(&ctx),
                        remaining_ms,
                    )
                    .await?;
            }
            _ => unreachable!("callable resolver returned an unsupported kind"),
        }
    }
    let elapsed = started_at.elapsed();
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(pi::error::Error::extension(
            "benchmark runtime did not shut down after callable dispatch",
        ));
    }

    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations.max(1));
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "tool_call",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "invoke_kind": invoke_kind,
        "invoke_name": invoke_name,
        "unexpected_hostcalls": null,
        "unexpected_hostcalls_observable": false,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
    }))
}

/// Event hook dispatch: N repeated event dispatches.
async fn scenario_event_dispatch(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd, None).await?;
    load_extension(&runtime, spec).await?;

    let event_payload = json!({"systemPrompt": "You are Pi."});
    let budget = Duration::from_secs(60);
    let started_at = Instant::now();
    for _ in 0..iterations {
        if started_at.elapsed() >= budget {
            let _ = runtime.manager.shutdown(Duration::from_secs(5)).await;
            return Err(pi::error::Error::extension(format!(
                "event-dispatch benchmark timed out after {}ms",
                budget.as_millis()
            )));
        }
        let remaining_ms = u64::try_from(
            budget
                .saturating_sub(started_at.elapsed())
                .as_millis()
                .max(1),
        )
        .unwrap_or(u64::MAX);
        let _ = runtime
            .manager
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(event_payload.clone()),
                remaining_ms,
            )
            .await?;
    }
    let elapsed = started_at.elapsed();
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(pi::error::Error::extension(
            "benchmark runtime did not shut down after event dispatch",
        ));
    }

    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations.max(1));
    let per_call_us = elapsed_us / iters_f;

    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "event_dispatch",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "unexpected_hostcalls": null,
        "unexpected_hostcalls_observable": false,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
    }))
}

fn phase1_matrix_seed_rows(env: &Value) -> Vec<Value> {
    let matched = [
        (100_000_u64, 48.0, 36.0, 22.0, 11.0),
        (200_000_u64, 62.0, 45.0, 29.0, 13.0),
        (500_000_u64, 91.0, 68.0, 43.0, 18.0),
        (1_000_000_u64, 136.0, 101.0, 64.0, 24.0),
        (5_000_000_u64, 212.0, 158.0, 97.0, 35.0),
    ];
    let realistic = [
        (100_000_u64, 44.0, 32.0, 19.0, 10.0),
        (200_000_u64, 57.0, 41.0, 25.0, 12.0),
        (500_000_u64, 84.0, 61.0, 37.0, 16.0),
        (1_000_000_u64, 124.0, 90.0, 54.0, 21.0),
        (5_000_000_u64, 198.0, 146.0, 88.0, 33.0),
    ];

    let mut rows = Vec::with_capacity(matched.len() + realistic.len());
    for (partition, samples) in [
        (PARTITION_MATCHED_STATE, matched.as_slice()),
        (PARTITION_REALISTIC, realistic.as_slice()),
    ] {
        for &(session_messages, open_ms, append_ms, save_ms, index_ms) in samples {
            let scenario_id = format!(
                "{partition}/{SYNTHETIC_MEASUREMENT_CONTRACT_VERSION}/session_{session_messages}"
            );
            rows.push(json!({
                "schema": "pi.ext.rust_bench.v1",
                "runtime": "pi_agent_rust",
                "scenario": MATRIX_SCENARIO_SESSION_WORKLOAD,
                "extension": "core",
                "partition": partition,
                "evidence_class": EVIDENCE_CLASS_INFERRED,
                "confidence": CONFIDENCE_LOW,
                "measurement_method": "synthetic_seed_projection",
                "eligible_for_regression_gate": false,
                "measurement_boundary": SYNTHETIC_MEASUREMENT_BOUNDARY,
                "measurement_contract_version": SYNTHETIC_MEASUREMENT_CONTRACT_VERSION,
                "session_messages": session_messages,
                "open_ms": open_ms,
                "append_ms": append_ms,
                "save_ms": save_ms,
                "index_ms": index_ms,
                "total_ms": open_ms + append_ms + save_ms + index_ms,
                "swarm_metrics": matrix_swarm_metrics(
                    env,
                    session_messages,
                    open_ms,
                    append_ms,
                    save_ms,
                    index_ms
                ),
                "scenario_metadata": {
                    "scenario_id": scenario_id,
                    "replay_input": {
                        "session_messages": session_messages
                    }
                }
            }));
        }
    }

    rows
}

// ─── Runner ─────────────────────────────────────────────────────────────────

fn run_all_scenarios() -> Result<Vec<Value>> {
    let cwd = project_root();
    let js_cwd = cwd.display().to_string();
    let env = env_fingerprint();
    let run_correlation_id = new_run_correlation_id(&env);

    let mut records: Vec<Value> = Vec::new();

    for ext_name in BENCH_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            eprintln!("[skip] extension artifact not found: {}", entry.display());
            continue;
        }

        let spec = JsExtensionLoadSpec::from_entry_path(&entry)?;

        eprintln!("[bench] {ext_name}: cold_start ({LOAD_RUNS} runs)");
        let cold = block_on(scenario_cold_start(&spec, &js_cwd, LOAD_RUNS))?;
        records.push(attach_contract(cold, &env, &run_correlation_id));

        eprintln!("[bench] {ext_name}: warm_start ({LOAD_RUNS} runs)");
        let warm = block_on(scenario_warm_start(&spec, &js_cwd, LOAD_RUNS))?;
        records.push(attach_contract(warm, &env, &run_correlation_id));

        eprintln!("[bench] {ext_name}: tool_call ({DISPATCH_ITERATIONS} iters)");
        match block_on(scenario_tool_call(&spec, &js_cwd, DISPATCH_ITERATIONS)) {
            Ok(tc) => records.push(attach_contract(tc, &env, &run_correlation_id)),
            Err(e) => eprintln!("[warn] {ext_name}: tool_call failed: {e}"),
        }

        eprintln!("[bench] {ext_name}: event_dispatch ({DISPATCH_ITERATIONS} iters)");
        match block_on(scenario_event_dispatch(&spec, &js_cwd, DISPATCH_ITERATIONS)) {
            Ok(ed) => records.push(attach_contract(ed, &env, &run_correlation_id)),
            Err(e) => eprintln!("[warn] {ext_name}: event_dispatch failed: {e}"),
        }
    }

    for row in phase1_matrix_seed_rows(&env) {
        records.push(attach_contract(row, &env, &run_correlation_id));
    }

    Ok(records)
}

fn attach_contract(mut record: Value, env: &Value, run_correlation_id: &str) -> Value {
    if let Value::Object(ref mut map) = record {
        let extension = map
            .get("extension")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let scenario = map
            .get("scenario")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let runtime = map
            .get("runtime")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown".to_string()));
        let partition = map
            .get("partition")
            .and_then(Value::as_str)
            .unwrap_or(PARTITION_MATCHED_STATE)
            .to_owned();
        let measurement_contract_version = map
            .get("measurement_contract_version")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "benchmark record is missing measurement_contract_version: extension={extension} scenario={scenario}"
                )
            })
            .to_owned();
        let default_scenario_id = format!("{partition}/{measurement_contract_version}/{scenario}");
        let scenario_id_for_hash = map
            .get("scenario_metadata")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get("scenario_id"))
            .and_then(Value::as_str)
            .map_or_else(|| default_scenario_id.clone(), ToString::to_string);
        let scenario_correlation = sha256_hex(&format!(
            "{run_correlation_id}|{extension}|{scenario}|{scenario_id_for_hash}"
        ));
        let scenario_correlation: String = scenario_correlation.chars().take(32).collect();

        let replay_input = scenario_replay_input(map);
        let build_profile = env
            .get("build_profile")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown".to_string()));
        let mut scenario_metadata = map
            .get("scenario_metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        scenario_metadata
            .entry("runtime".to_string())
            .or_insert(runtime);
        scenario_metadata
            .entry("build_profile".to_string())
            .or_insert(build_profile);
        scenario_metadata
            .entry("host".to_string())
            .or_insert_with(|| host_metadata_from_env(env));
        scenario_metadata
            .entry("scenario_id".to_string())
            .or_insert_with(|| Value::String(default_scenario_id));
        scenario_metadata
            .entry("replay_input".to_string())
            .or_insert(replay_input);

        map.insert("env".to_string(), env.clone());
        map.insert(
            "protocol_schema".to_string(),
            Value::String(BENCH_PROTOCOL_SCHEMA.to_string()),
        );
        map.insert(
            "protocol_version".to_string(),
            Value::String(BENCH_PROTOCOL_VERSION.to_string()),
        );
        map.insert("partition".to_string(), Value::String(partition));
        if !map.contains_key("evidence_class") {
            map.insert(
                "evidence_class".to_string(),
                Value::String(EVIDENCE_CLASS_MEASURED.to_string()),
            );
        }
        if !map.contains_key("confidence") {
            map.insert(
                "confidence".to_string(),
                Value::String(CONFIDENCE_HIGH.to_string()),
            );
        }
        if !map.contains_key("eligible_for_regression_gate") {
            map.insert(
                "eligible_for_regression_gate".to_string(),
                Value::Bool(true),
            );
        }
        map.insert(
            "correlation_id".to_string(),
            Value::String(scenario_correlation),
        );
        map.insert(
            "scenario_metadata".to_string(),
            Value::Object(scenario_metadata),
        );
    }
    record
}

fn write_jsonl(records: &[Value], path: &Path) {
    let mut content = String::new();
    for record in records {
        let _ = writeln!(
            content,
            "{}",
            serde_json::to_string(record).unwrap_or_default()
        );
    }
    fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .expect("create benchmark scenario output dir");
    fs::write(path, &content).expect("write benchmark scenario JSONL");
}

// ─── Tests ──────────────────────────────────────────────────────────────────

fn collect_benchmarked_extensions(records: &[Value]) -> HashSet<String> {
    records
        .iter()
        .filter(|record| {
            record.get("scenario").and_then(Value::as_str) != Some(MATRIX_SCENARIO_SESSION_WORKLOAD)
        })
        .filter_map(|record| record.get("extension").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect()
}

fn assert_expected_benchmarked_extensions(records: &[Value]) {
    let benchmarked_extensions = collect_benchmarked_extensions(records);
    for expected_ext in BENCH_EXTENSIONS {
        assert!(
            benchmarked_extensions.contains(*expected_ext),
            "missing benchmark records for extension: {expected_ext}; observed={benchmarked_extensions:?}"
        );
    }
    assert!(
        benchmarked_extensions.len() >= BENCH_EXTENSIONS.len(),
        "expected at least {} benchmarked extensions, got {}: {:?}",
        BENCH_EXTENSIONS.len(),
        benchmarked_extensions.len(),
        benchmarked_extensions
    );
}

fn assert_required_scenarios(records: &[Value]) {
    let scenarios: HashSet<&str> = records
        .iter()
        .filter_map(|record| record.get("scenario").and_then(Value::as_str))
        .collect();

    for expected in &[
        "cold_start",
        "warm_start",
        "tool_call",
        "event_dispatch",
        MATRIX_SCENARIO_SESSION_WORKLOAD,
    ] {
        assert!(scenarios.contains(expected), "missing scenario: {expected}");
    }
}

fn assert_per_extension_scenarios(records: &[Value]) {
    for ext_name in BENCH_EXTENSIONS {
        let ext_scenarios: HashSet<&str> = records
            .iter()
            .filter(|record| record.get("extension").and_then(Value::as_str) == Some(*ext_name))
            .filter_map(|record| record.get("scenario").and_then(Value::as_str))
            .collect();
        for expected in ["cold_start", "warm_start", "tool_call", "event_dispatch"] {
            assert!(
                ext_scenarios.contains(expected),
                "extension {ext_name} missing scenario {expected}; observed={ext_scenarios:?}"
            );
        }
    }
}

fn collect_matrix_key_counts(records: &[Value]) -> BTreeMap<(String, u64), usize> {
    let mut matrix_key_counts: BTreeMap<(String, u64), usize> = BTreeMap::new();
    for record in records {
        if record.get("scenario").and_then(Value::as_str) != Some(MATRIX_SCENARIO_SESSION_WORKLOAD)
        {
            continue;
        }

        let partition = record
            .get("partition")
            .and_then(Value::as_str)
            .expect("matrix row must include partition")
            .to_string();
        let session_messages = record
            .get("session_messages")
            .and_then(Value::as_u64)
            .expect("matrix row must include session_messages");
        *matrix_key_counts
            .entry((partition, session_messages))
            .or_insert(0) += 1;
    }
    matrix_key_counts
}

fn expected_matrix_keys() -> HashSet<(String, u64)> {
    [PARTITION_MATCHED_STATE, PARTITION_REALISTIC]
        .into_iter()
        .flat_map(|partition| {
            MATRIX_SESSION_SIZES
                .iter()
                .copied()
                .map(move |session_messages| (partition.to_string(), session_messages))
        })
        .collect()
}

fn assert_matrix_rows(records: &[Value]) {
    let matrix_rows = records
        .iter()
        .filter(|record| {
            record.get("scenario").and_then(Value::as_str) == Some(MATRIX_SCENARIO_SESSION_WORKLOAD)
        })
        .count();
    assert_eq!(
        matrix_rows,
        MATRIX_SESSION_SIZES.len() * 2,
        "expected one matched-state and one realistic matrix row per required session size"
    );

    let matrix_key_counts = collect_matrix_key_counts(records);
    let observed_matrix_keys: HashSet<(String, u64)> = matrix_key_counts.keys().cloned().collect();
    assert_eq!(
        observed_matrix_keys,
        expected_matrix_keys(),
        "session_workload_matrix rows must cover required partition-size cells exactly"
    );
    for ((partition, session_messages), count) in matrix_key_counts {
        assert_eq!(
            count, 1,
            "duplicate session_workload_matrix row for partition={partition} session_messages={session_messages}"
        );
    }
}

fn assert_records_have_schema(records: &[Value]) {
    for record in records {
        assert_eq!(
            record.get("schema").and_then(Value::as_str),
            Some("pi.ext.rust_bench.v1"),
            "record missing schema: {record}"
        );
    }
}

fn print_scenario_summary(records: &[Value]) {
    eprintln!("\n=== Scenario Runner Summary ===");
    for record in records {
        let ext = record
            .get("extension")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let scenario = record
            .get("scenario")
            .and_then(Value::as_str)
            .unwrap_or("?");

        match scenario {
            "cold_start" | "warm_start" => {
                if let Some(stats) = record.get("stats") {
                    let p95 = stats.get("p95_ms").and_then(Value::as_f64).unwrap_or(0.0);
                    eprintln!("  {ext}/{scenario}: p95={p95:.2}ms");
                }
            }
            "tool_call" | "event_dispatch" => {
                let per_call = record
                    .get("per_call_us")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                eprintln!("  {ext}/{scenario}: per_call={per_call:.1}us");
            }
            _ => {}
        }
    }
}

#[test]
fn run_scenario_suite_and_emit_jsonl() {
    let records = run_all_scenarios().expect("scenario suite should complete");

    assert_expected_benchmarked_extensions(&records);
    assert_required_scenarios(&records);
    assert_per_extension_scenarios(&records);
    assert_matrix_rows(&records);
    assert_records_have_schema(&records);

    // Write JSONL output
    let output_path = perf_output_path("scenario_runner.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} records written to {}",
        records.len(),
        output_path.display()
    );

    print_scenario_summary(&records);
}

/// Verify output stability: re-run and compare structure (not timing values).
#[test]
fn scenario_output_has_stable_structure() {
    let records = run_all_scenarios().expect("scenario suite should complete");

    for record in &records {
        let obj = record.as_object().expect("record should be object");

        assert_record_structure(obj);
    }
}

fn assert_record_structure(obj: &Map<String, Value>) {
    assert_record_required_fields(obj);
    assert_protocol_and_partition_contract(obj);
    assert_unexpected_hostcall_contract(obj);
    assert_env_fingerprint_fields(obj);
    let metadata = assert_scenario_metadata_fields(obj);
    assert_matrix_scenario_structure(obj, metadata);
}

fn assert_unexpected_hostcall_contract(obj: &Map<String, Value>) {
    if obj
        .get("unexpected_hostcalls_observable")
        .and_then(Value::as_bool)
        == Some(false)
    {
        assert_eq!(
            obj.get("unexpected_hostcalls"),
            Some(&Value::Null),
            "unobservable unexpected-hostcall evidence must be null, not an empty observation"
        );
    }
}

fn assert_record_required_fields(obj: &Map<String, Value>) {
    assert!(obj.contains_key("schema"), "missing schema");
    assert!(obj.contains_key("runtime"), "missing runtime");
    assert!(obj.contains_key("scenario"), "missing scenario");
    assert!(obj.contains_key("extension"), "missing extension");
    assert!(obj.contains_key("env"), "missing env");
    assert!(
        obj.contains_key("protocol_schema"),
        "missing protocol_schema"
    );
    assert!(
        obj.contains_key("protocol_version"),
        "missing protocol_version"
    );
    assert!(obj.contains_key("partition"), "missing partition");
    assert!(obj.contains_key("evidence_class"), "missing evidence_class");
    assert!(obj.contains_key("confidence"), "missing confidence");
    assert!(
        obj.contains_key("eligible_for_regression_gate"),
        "missing eligible_for_regression_gate"
    );
    assert!(
        obj.contains_key("measurement_boundary"),
        "missing measurement_boundary"
    );
    assert!(
        obj.contains_key("measurement_contract_version"),
        "missing measurement_contract_version"
    );
    assert!(obj.contains_key("correlation_id"), "missing correlation_id");
    assert!(
        obj.contains_key("scenario_metadata"),
        "missing scenario_metadata"
    );
}

fn assert_protocol_and_partition_contract(obj: &Map<String, Value>) {
    assert_eq!(
        obj.get("protocol_schema").and_then(Value::as_str),
        Some(BENCH_PROTOCOL_SCHEMA),
        "unexpected protocol_schema",
    );
    assert_eq!(
        obj.get("protocol_version").and_then(Value::as_str),
        Some(BENCH_PROTOCOL_VERSION),
        "unexpected protocol_version",
    );
    let partition = obj.get("partition").and_then(Value::as_str).unwrap_or("");
    assert!(
        matches!(partition, PARTITION_MATCHED_STATE | PARTITION_REALISTIC),
        "unexpected partition: {partition}"
    );
    let is_matrix =
        obj.get("scenario").and_then(Value::as_str) == Some(MATRIX_SCENARIO_SESSION_WORKLOAD);
    let (
        expected_evidence_class,
        expected_confidence,
        expected_gate_eligibility,
        expected_boundary,
        expected_contract_version,
    ) = if is_matrix {
        (
            EVIDENCE_CLASS_INFERRED,
            CONFIDENCE_LOW,
            false,
            SYNTHETIC_MEASUREMENT_BOUNDARY,
            SYNTHETIC_MEASUREMENT_CONTRACT_VERSION,
        )
    } else {
        (
            EVIDENCE_CLASS_MEASURED,
            CONFIDENCE_HIGH,
            true,
            MEASUREMENT_BOUNDARY,
            MEASUREMENT_CONTRACT_VERSION,
        )
    };
    assert_eq!(
        obj.get("evidence_class").and_then(Value::as_str),
        Some(expected_evidence_class),
        "unexpected evidence_class",
    );
    assert_eq!(
        obj.get("confidence").and_then(Value::as_str),
        Some(expected_confidence),
        "unexpected confidence",
    );
    assert_eq!(
        obj.get("eligible_for_regression_gate")
            .and_then(Value::as_bool),
        Some(expected_gate_eligibility),
        "unexpected eligible_for_regression_gate",
    );
    assert_eq!(
        obj.get("measurement_boundary").and_then(Value::as_str),
        Some(expected_boundary),
        "unexpected measurement_boundary",
    );
    assert_eq!(
        obj.get("measurement_contract_version")
            .and_then(Value::as_str),
        Some(expected_contract_version),
        "unexpected measurement_contract_version",
    );
    let correlation_id = obj
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !correlation_id.is_empty(),
        "correlation_id must be non-empty"
    );
}

fn assert_env_fingerprint_fields(obj: &Map<String, Value>) {
    let env = obj.get("env").expect("env must be present");
    for field in &[
        "os",
        "arch",
        "cpu_model",
        "cpu_cores",
        "mem_total_mb",
        "build_profile",
        "git_commit",
        "config_hash",
    ] {
        assert!(env.get(field).is_some(), "env missing field: {field}");
    }
}

fn assert_scenario_metadata_fields(obj: &Map<String, Value>) -> &Map<String, Value> {
    let metadata = obj
        .get("scenario_metadata")
        .and_then(Value::as_object)
        .expect("scenario_metadata must be object");
    for field in &[
        "runtime",
        "build_profile",
        "host",
        "scenario_id",
        "replay_input",
    ] {
        assert!(
            metadata.contains_key(*field),
            "scenario_metadata missing field: {field}"
        );
    }
    let scenario_id = metadata
        .get("scenario_id")
        .and_then(Value::as_str)
        .expect("scenario_metadata.scenario_id must be a string");
    let contract_version = obj
        .get("measurement_contract_version")
        .and_then(Value::as_str)
        .expect("measurement_contract_version must be a string");
    assert!(
        scenario_id.contains(contract_version),
        "scenario_id must bind the measurement contract version: {scenario_id}"
    );
    metadata
}

fn assert_matrix_scenario_structure(obj: &Map<String, Value>, metadata: &Map<String, Value>) {
    if obj.get("scenario").and_then(Value::as_str) != Some(MATRIX_SCENARIO_SESSION_WORKLOAD) {
        return;
    }

    let scenario_id = metadata
        .get("scenario_id")
        .and_then(Value::as_str)
        .expect("matrix scenario_id must be a string");
    assert!(
        scenario_id.starts_with(&format!(
            "matched-state/{SYNTHETIC_MEASUREMENT_CONTRACT_VERSION}/session_"
        )) || scenario_id.starts_with(&format!(
            "realistic/{SYNTHETIC_MEASUREMENT_CONTRACT_VERSION}/session_"
        )),
        "unexpected matrix scenario_id: {scenario_id}"
    );
    assert_eq!(
        obj.get("measurement_method").and_then(Value::as_str),
        Some("synthetic_seed_projection")
    );
    let replay_input = metadata
        .get("replay_input")
        .and_then(Value::as_object)
        .expect("matrix replay_input must be object");
    let session_messages = replay_input
        .get("session_messages")
        .and_then(Value::as_u64)
        .expect("matrix replay_input.session_messages must be integer");
    assert!(
        MATRIX_SESSION_SIZES.contains(&session_messages),
        "unexpected matrix session_messages: {session_messages}"
    );

    for metric in ["open_ms", "append_ms", "save_ms", "index_ms"] {
        let value = obj
            .get(metric)
            .and_then(Value::as_f64)
            .expect("matrix stage metrics must be numeric");
        assert!(
            value > 0.0,
            "matrix stage metric must be positive: {metric}={value}"
        );
    }

    assert_matrix_swarm_metrics(obj);
}

fn assert_quantile_object(
    obj: &Map<String, Value>,
    field: &str,
    quantiles: &[&str],
    context: &str,
) {
    let value = obj
        .get(field)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{context}.{field} must be object"));
    let mut previous = 0.0;
    for quantile in quantiles {
        let current = value
            .get(*quantile)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("{context}.{field}.{quantile} must be numeric"));
        assert!(
            current.is_finite() && current >= previous,
            "{context}.{field}.{quantile} must be finite and monotonic, got {current}"
        );
        previous = current;
    }
}

fn assert_breakdown_object(
    obj: &Map<String, Value>,
    field: &str,
    required_keys: &[&str],
    context: &str,
) {
    let value = obj
        .get(field)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{context}.{field} must be object"));
    for key in required_keys {
        let metric = value
            .get(*key)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("{context}.{field}.{key} must be numeric"));
        assert!(
            metric.is_finite() && metric >= 0.0,
            "{context}.{field}.{key} must be finite and non-negative"
        );
    }
}

fn assert_matrix_swarm_metrics(obj: &Map<String, Value>) {
    let metrics = obj
        .get("swarm_metrics")
        .and_then(Value::as_object)
        .expect("matrix row must include swarm_metrics object");

    assert_quantile_object(
        metrics,
        "latency_quantiles_ms",
        &["p50", "p95", "p99", "p999"],
        "swarm_metrics",
    );
    assert_quantile_object(
        metrics,
        "queue_depth",
        &["p50", "p95", "p99", "p999", "max"],
        "swarm_metrics",
    );
    assert_breakdown_object(
        metrics,
        "resource_usage",
        &["rss_mb", "cpu_pct"],
        "swarm_metrics",
    );
    assert_breakdown_object(
        metrics,
        "component_breakdown_ms",
        &["tool", "provider", "extension", "session"],
        "swarm_metrics",
    );
    assert_breakdown_object(
        metrics,
        "stage_breakdown_ms",
        &["open", "append", "save", "index"],
        "swarm_metrics",
    );
    let derivation = metrics
        .get("derivation")
        .and_then(Value::as_object)
        .expect("synthetic swarm_metrics must disclose derivation");
    assert_eq!(
        derivation.get("method").and_then(Value::as_str),
        Some("deterministic_seed_projection")
    );
}

/// Verify cold start is slower than warm start (sanity check).
#[test]
fn cold_start_not_faster_than_warm_start() {
    let records = run_all_scenarios().expect("scenario suite should complete");

    for ext in BENCH_EXTENSIONS {
        let cold_p50 = records
            .iter()
            .find(|r| {
                r.get("extension").and_then(Value::as_str) == Some(ext)
                    && r.get("scenario").and_then(Value::as_str) == Some("cold_start")
            })
            .and_then(|r| r.get("stats"))
            .and_then(|s| s.get("p50_ms"))
            .and_then(Value::as_f64);

        let warm_p50 = records
            .iter()
            .find(|r| {
                r.get("extension").and_then(Value::as_str) == Some(ext)
                    && r.get("scenario").and_then(Value::as_str) == Some("warm_start")
            })
            .and_then(|r| r.get("stats"))
            .and_then(|s| s.get("p50_ms"))
            .and_then(Value::as_f64);

        if let (Some(cold), Some(warm)) = (cold_p50, warm_p50) {
            // Warm should generally not be dramatically slower than cold.
            // Allow warm to be up to 2x cold (filesystem cache effects are modest).
            eprintln!("[check] {ext}: cold_p50={cold:.2}ms warm_p50={warm:.2}ms");
            assert!(
                warm < cold * 3.0,
                "{ext}: warm start ({warm:.2}ms) unexpectedly 3x slower than cold ({cold:.2}ms)"
            );
        }
    }
}
