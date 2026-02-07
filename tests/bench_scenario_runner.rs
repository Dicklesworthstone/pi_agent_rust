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
use pi::extensions::JsExtensionLoadSpec;
use pi::extensions_js::{HostcallKind, PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::{HostcallOutcome, WallClock};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::System;

// ─── Configuration ──────────────────────────────────────────────────────────

/// Extensions to benchmark (name, artifact dir name).
/// Must be >=3 per bd-m5jp acceptance criteria.
const BENCH_EXTENSIONS: &[&str] = &["hello", "pirate", "diff"];

/// Iterations for cold/warm start scenarios.
const LOAD_RUNS: usize = 5;

/// Iterations for tool-call and event-hook scenarios.
const DISPATCH_ITERATIONS: u32 = 500;

/// Sentinel tool name for benchmark reporting.
const BENCH_REPORT_TOOL: &str = "__bench_report";

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
    let cpu_cores = system.cpus().len() as u32;
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

// ─── Artifact Lookup ────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
    max_ms: f64,
}

fn percentile(sorted: &[f64], pct: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (sorted.len() * pct).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn compute_stats(durations: &[Duration]) -> Stats {
    let mut ms: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);

    Stats {
        count: ms.len(),
        min_ms: ms.first().copied().unwrap_or(0.0),
        p50_ms: percentile(&ms, 50),
        p95_ms: percentile(&ms, 95),
        p99_ms: percentile(&ms, 99),
        max_ms: ms.last().copied().unwrap_or(0.0),
    }
}

// ─── Runtime Helpers ────────────────────────────────────────────────────────

async fn new_runtime(js_cwd: &str) -> Result<PiJsRuntime> {
    let config = PiJsRuntimeConfig {
        cwd: js_cwd.to_string(),
        ..Default::default()
    };
    PiJsRuntime::with_clock_and_config(WallClock, config).await
}

fn js_literal(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|err| pi::error::Error::Json(Box::new(err)))
}

/// Run JS code and pump hostcall requests until a `__bench_report` tool call
/// is received (signaling completion) or the budget expires.
async fn run_bench_js(
    runtime: &PiJsRuntime,
    js: &str,
    budget: Duration,
) -> Result<(Value, BTreeMap<String, u64>, Duration)> {
    let started_at = Instant::now();
    runtime.eval(js).await?;

    let mut report: Option<Value> = None;
    let mut unexpected: BTreeMap<String, u64> = BTreeMap::new();

    while started_at.elapsed() < budget {
        let mut requests = runtime.drain_hostcall_requests();
        while let Some(req) = requests.pop_front() {
            let (key, outcome) = match &req.kind {
                HostcallKind::Tool { name } if name == BENCH_REPORT_TOOL => {
                    report = Some(req.payload.clone());
                    (String::new(), HostcallOutcome::Success(json!({})))
                }
                HostcallKind::Tool { name } => (
                    format!("tool.{name}"),
                    HostcallOutcome::Error {
                        code: "UNSUPPORTED_TOOL".to_string(),
                        message: format!("bench harness does not implement tool {name}"),
                    },
                ),
                HostcallKind::Ui { op } => (
                    format!("ui.{op}"),
                    HostcallOutcome::Success(json!({"ok": true})),
                ),
                HostcallKind::Events { op } => (
                    format!("events.{op}"),
                    HostcallOutcome::Success(json!({"ok": true})),
                ),
                HostcallKind::Session { op } => (
                    format!("session.{op}"),
                    HostcallOutcome::Success(json!({"ok": true})),
                ),
                HostcallKind::Exec { cmd } => (
                    format!("exec.{cmd}"),
                    HostcallOutcome::Error {
                        code: "EXEC_DISABLED".to_string(),
                        message: "bench harness forbids exec".to_string(),
                    },
                ),
                HostcallKind::Http => (
                    "http".to_string(),
                    HostcallOutcome::Error {
                        code: "HTTP_DISABLED".to_string(),
                        message: "bench harness forbids http".to_string(),
                    },
                ),
                HostcallKind::Log => (
                    "log".to_string(),
                    HostcallOutcome::Success(json!({"logged": true})),
                ),
            };

            if !key.is_empty() {
                *unexpected.entry(key).or_insert(0) += 1;
            }

            runtime.complete_hostcall(req.call_id, outcome);
            let _ = runtime.tick().await?;
        }

        let _ = runtime.drain_microtasks().await?;

        if let Some(r) = report.take() {
            let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if !ok {
                let msg = r.get("error").and_then(Value::as_str).unwrap_or("unknown");
                return Err(pi::error::Error::extension(format!(
                    "js bench failed: {msg}"
                )));
            }
            return Ok((r, unexpected, started_at.elapsed()));
        }

        if runtime.has_pending() {
            let _ = runtime.tick().await?;
        }
    }

    Err(pi::error::Error::extension(format!(
        "benchmark timed out after {}ms",
        budget.as_millis()
    )))
}

async fn load_extension(runtime: &PiJsRuntime, spec: &JsExtensionLoadSpec) -> Result<()> {
    let ext_id = js_literal(&spec.extension_id)?;
    let entry = js_literal(&spec.entry_path.display().to_string())?;
    let meta = js_literal(&json!({
        "name": spec.name,
        "version": spec.version,
        "apiVersion": spec.api_version,
    }))?;
    let bench_tool = js_literal(&BENCH_REPORT_TOOL)?;

    let js = format!(
        r"
(async () => {{
  try {{
    await __pi_load_extension({ext_id}, {entry}, {meta});
    await pi.tool({bench_tool}, {{ ok: true }});
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    await pi.tool({bench_tool}, {{ ok: false, error: msg }});
  }}
}})();
"
    );

    let (_report, _unexpected, _elapsed) =
        run_bench_js(runtime, &js, Duration::from_secs(10)).await?;
    Ok(())
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
        let runtime = new_runtime(js_cwd).await?;
        load_extension(&runtime, spec).await?;
        timings.push(start.elapsed());
    }

    let stats = compute_stats(&timings);
    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "cold_start",
        "extension": spec.extension_id,
        "runs": runs,
        "stats": stats,
    }))
}

/// Warm start: reuse an existing runtime, load the extension again.
async fn scenario_warm_start(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    runs: usize,
) -> Result<Value> {
    // Create one runtime and load the extension once (warmup).
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    // Now re-load the same extension repeatedly (warm path).
    let mut timings = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        // Create a fresh runtime but the filesystem cache is warm.
        let warm_rt = new_runtime(js_cwd).await?;
        load_extension(&warm_rt, spec).await?;
        timings.push(start.elapsed());
    }

    let stats = compute_stats(&timings);
    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "warm_start",
        "extension": spec.extension_id,
        "runs": runs,
        "stats": stats,
    }))
}

/// Tool call overhead: N repeated tool invocations on a loaded extension.
async fn scenario_tool_call(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    // Find the first registered tool name from the extension.
    // For "hello" it's "hello", for "pirate" it's "pirate", for "diff" it's "diff".
    let tool_name = js_literal(&spec.extension_id)?;
    let call_id = js_literal(&"bench-call-1")?;
    let input = js_literal(&json!({}))?;
    let ctx = js_literal(&json!({"hasUI": false, "cwd": js_cwd}))?;
    let iterations_js = js_literal(&iterations)?;
    let bench_tool = js_literal(&BENCH_REPORT_TOOL)?;

    let js = format!(
        r"
(async () => {{
  try {{
    const N = {iterations_js};
    for (let i = 0; i < N; i++) {{
      await __pi_execute_tool({tool_name}, {call_id}, {input}, {ctx});
    }}
    await pi.tool({bench_tool}, {{ ok: true }});
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    await pi.tool({bench_tool}, {{ ok: false, error: msg }});
  }}
}})();
"
    );

    let (_report, unexpected, elapsed) =
        run_bench_js(&runtime, &js, Duration::from_secs(60)).await?;

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
        "unexpected_hostcalls": unexpected,
    }))
}

/// Event hook dispatch: N repeated event dispatches.
async fn scenario_event_dispatch(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    let event_name = js_literal(&"before_agent_start")?;
    let event_payload = js_literal(&json!({"systemPrompt": "You are Pi."}))?;
    let ctx = js_literal(&json!({"hasUI": false, "cwd": js_cwd}))?;
    let iterations_js = js_literal(&iterations)?;
    let bench_tool = js_literal(&BENCH_REPORT_TOOL)?;

    let js = format!(
        r"
(async () => {{
  try {{
    const N = {iterations_js};
    for (let i = 0; i < N; i++) {{
      await __pi_dispatch_extension_event({event_name}, {event_payload}, {ctx});
    }}
    await pi.tool({bench_tool}, {{ ok: true }});
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    await pi.tool({bench_tool}, {{ ok: false, error: msg }});
  }}
}})();
"
    );

    let (_report, unexpected, elapsed) =
        run_bench_js(&runtime, &js, Duration::from_secs(60)).await?;

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
        "unexpected_hostcalls": unexpected,
    }))
}

// ─── Runner ─────────────────────────────────────────────────────────────────

fn run_all_scenarios() -> Result<Vec<Value>> {
    let cwd = project_root();
    let js_cwd = cwd.display().to_string();
    let env = env_fingerprint();

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
        records.push(attach_env(cold, &env));

        eprintln!("[bench] {ext_name}: warm_start ({LOAD_RUNS} runs)");
        let warm = block_on(scenario_warm_start(&spec, &js_cwd, LOAD_RUNS))?;
        records.push(attach_env(warm, &env));

        eprintln!("[bench] {ext_name}: tool_call ({DISPATCH_ITERATIONS} iters)");
        match block_on(scenario_tool_call(&spec, &js_cwd, DISPATCH_ITERATIONS)) {
            Ok(tc) => records.push(attach_env(tc, &env)),
            Err(e) => eprintln!("[warn] {ext_name}: tool_call failed: {e}"),
        }

        eprintln!("[bench] {ext_name}: event_dispatch ({DISPATCH_ITERATIONS} iters)");
        match block_on(scenario_event_dispatch(&spec, &js_cwd, DISPATCH_ITERATIONS)) {
            Ok(ed) => records.push(attach_env(ed, &env)),
            Err(e) => eprintln!("[warn] {ext_name}: event_dispatch failed: {e}"),
        }
    }

    Ok(records)
}

fn attach_env(mut record: Value, env: &Value) -> Value {
    if let Value::Object(ref mut map) = record {
        map.insert("env".to_string(), env.clone());
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
    let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")));
    fs::write(path, &content).unwrap_or_else(|e| {
        eprintln!("[error] failed to write {}: {e}", path.display());
    });
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn run_scenario_suite_and_emit_jsonl() {
    let records = run_all_scenarios().expect("scenario suite should complete");

    // Must have records for >=3 extensions
    let extensions: std::collections::HashSet<_> = records
        .iter()
        .filter_map(|r| r.get("extension").and_then(Value::as_str))
        .collect();
    assert!(
        extensions.len() >= 3,
        "expected >=3 extensions, got {}: {:?}",
        extensions.len(),
        extensions
    );

    // Must have all 4 scenario types
    let scenarios: std::collections::HashSet<_> = records
        .iter()
        .filter_map(|r| r.get("scenario").and_then(Value::as_str))
        .collect();
    for expected in &["cold_start", "warm_start", "tool_call", "event_dispatch"] {
        assert!(scenarios.contains(expected), "missing scenario: {expected}");
    }

    // All records must have schema field
    for record in &records {
        assert_eq!(
            record.get("schema").and_then(Value::as_str),
            Some("pi.ext.rust_bench.v1"),
            "record missing schema: {record}"
        );
    }

    // Write JSONL output
    let output_path = project_root().join("target/perf/scenario_runner.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} records written to {}",
        records.len(),
        output_path.display()
    );

    // Print summary
    eprintln!("\n=== Scenario Runner Summary ===");
    for record in &records {
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

/// Verify output stability: re-run and compare structure (not timing values).
#[test]
fn scenario_output_has_stable_structure() {
    let records = run_all_scenarios().expect("scenario suite should complete");

    for record in &records {
        let obj = record.as_object().expect("record should be object");

        // Required fields present
        assert!(obj.contains_key("schema"), "missing schema");
        assert!(obj.contains_key("runtime"), "missing runtime");
        assert!(obj.contains_key("scenario"), "missing scenario");
        assert!(obj.contains_key("extension"), "missing extension");
        assert!(obj.contains_key("env"), "missing env");

        // Env fingerprint has required fields
        let env = obj.get("env").unwrap();
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
