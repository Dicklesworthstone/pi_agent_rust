//! Extension workload harness for deterministic legacy-vs-rust perf baselines.
//!
//! This intentionally avoids `JsExtensionRuntimeHandle::start()` (which spawns an OS
//! thread) so it can run in constrained CI / sandbox environments.
#![forbid(unsafe_code)]
// QuickJS runtime types are intentionally single-threaded (Rc-based); this binary
// uses `block_on` and never requires `Send` futures.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::future_not_send,
    clippy::needless_raw_string_hashes,
    clippy::redundant_closure_for_method_calls,
    clippy::suboptimal_flops
)]

use clap::Parser;
use futures::executor::block_on;
use pi::error::{Error, Result};
use pi::extensions::JsExtensionLoadSpec;
use pi::extensions_js::{HostcallKind, PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::{HostcallOutcome, WallClock};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BENCH_REPORT_TOOL: &str = "__bench_report";

#[derive(Parser, Debug)]
#[command(name = "ext_workloads")]
#[command(about = "Deterministic extension workload runner for perf baselines")]
struct Args {
    /// Number of cold load+init runs per extension (fresh runtime each run).
    #[arg(long, default_value_t = 3)]
    load_runs: usize,

    /// Iterations for tool-call + event-hook scenarios.
    #[arg(long, default_value_t = 2000)]
    iterations: u32,

    /// Optional JSONL output path (stdout if omitted).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct SummaryMs {
    count: usize,
    min_ms: Option<f64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();

    let mut out: Box<dyn Write> = match args.out.as_ref() {
        Some(path) => Box::new(fs::File::create(path)?),
        None => Box::new(io::stdout()),
    };

    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_cwd = cwd.display().to_string();

    let hello_entry = artifact_single_file_entry("hello");
    let pirate_entry = artifact_single_file_entry("pirate");

    let hello_spec = JsExtensionLoadSpec::from_entry_path(&hello_entry)?;
    let pirate_spec = JsExtensionLoadSpec::from_entry_path(&pirate_entry)?;

    let env = json!({
        "pkg": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        "build_ts": option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });

    // ext_load_init/load_init_cold (hello + pirate)
    let hello_load = block_on(scenario_load_init_cold(
        &hello_spec,
        &js_cwd,
        args.load_runs,
    ))?;
    writeln!(out, "{}", attach_env(hello_load, &env))?;

    let pirate_load = block_on(scenario_load_init_cold(
        &pirate_spec,
        &js_cwd,
        args.load_runs,
    ))?;
    writeln!(out, "{}", attach_env(pirate_load, &env))?;

    // ext_tool_call/hello
    let tool_call = block_on(scenario_tool_call(&hello_spec, &js_cwd, args.iterations))?;
    writeln!(out, "{}", attach_env(tool_call, &env))?;

    // ext_event_hook/before_agent_start
    let event_hook = block_on(scenario_event_hook(&pirate_spec, &js_cwd, args.iterations))?;
    writeln!(out, "{}", attach_env(event_hook, &env))?;

    Ok(())
}

fn attach_env(mut record: Value, env: &Value) -> Value {
    if let Value::Object(ref mut map) = record {
        map.insert("env".to_string(), env.clone());
    }
    record
}

fn artifact_single_file_entry(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ext_conformance/artifacts")
        .join(name)
        .join(format!("{name}.ts"))
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    if len == 0 {
        return 0;
    }
    // Ceil(rank) then convert to 0-index.
    let rank = len
        .saturating_mul(numerator)
        .saturating_add(denominator - 1)
        / denominator;
    rank.saturating_sub(1).min(len - 1)
}

fn percentile(sorted_ms: &[f64], pct: usize) -> Option<f64> {
    if sorted_ms.is_empty() {
        return None;
    }
    Some(sorted_ms[percentile_index(sorted_ms.len(), pct, 100)])
}

fn summarize_ms(durations: &[Duration]) -> SummaryMs {
    if durations.is_empty() {
        return SummaryMs {
            count: 0,
            min_ms: None,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            max_ms: None,
        };
    }

    let mut ms = durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .collect::<Vec<_>>();
    ms.sort_by(f64::total_cmp);

    SummaryMs {
        count: ms.len(),
        min_ms: Some(ms[0]),
        p50_ms: percentile(&ms, 50),
        p95_ms: percentile(&ms, 95),
        p99_ms: percentile(&ms, 99),
        max_ms: Some(ms[ms.len() - 1]),
    }
}

async fn new_runtime(js_cwd: &str) -> Result<PiJsRuntime> {
    let config = PiJsRuntimeConfig {
        cwd: js_cwd.to_string(),
        ..Default::default()
    };
    PiJsRuntime::with_clock_and_config(WallClock, config).await
}

fn js_literal(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|err| Error::Json(Box::new(err)))
}

struct BenchPumpOutcome {
    report: Value,
    unexpected_hostcalls: BTreeMap<String, u64>,
    elapsed: Duration,
}

async fn run_bench_js(
    runtime: &PiJsRuntime,
    js: &str,
    budget: Duration,
) -> Result<BenchPumpOutcome> {
    let started_at = Instant::now();
    runtime.eval(js).await?;

    let mut report: Option<Value> = None;
    let mut unexpected_hostcalls: BTreeMap<String, u64> = BTreeMap::new();

    while started_at.elapsed() < budget {
        let mut requests = runtime.drain_hostcall_requests();
        while let Some(req) = requests.pop_front() {
            let (kind_key, outcome) = match &req.kind {
                HostcallKind::Tool { name } => {
                    if name == BENCH_REPORT_TOOL {
                        report = Some(req.payload.clone());
                        (
                            "tool.__bench_report".to_string(),
                            HostcallOutcome::Success(json!({})),
                        )
                    } else {
                        (
                            format!("tool.{name}"),
                            HostcallOutcome::Error {
                                code: "UNSUPPORTED_TOOL".to_string(),
                                message: format!(
                                    "benchmark harness does not implement tool {name}"
                                ),
                            },
                        )
                    }
                }
                HostcallKind::Ui { op } => (
                    format!("ui.{op}"),
                    HostcallOutcome::Success(json!({ "ok": true })),
                ),
                HostcallKind::Events { op } => (
                    format!("events.{op}"),
                    HostcallOutcome::Success(json!({ "ok": true })),
                ),
                HostcallKind::Session { op } => (
                    format!("session.{op}"),
                    HostcallOutcome::Success(json!({ "ok": true })),
                ),
                HostcallKind::Exec { cmd } => (
                    format!("exec.{cmd}"),
                    HostcallOutcome::Error {
                        code: "EXEC_DISABLED".to_string(),
                        message: "benchmark harness forbids pi.exec".to_string(),
                    },
                ),
                HostcallKind::Http => (
                    "http".to_string(),
                    HostcallOutcome::Error {
                        code: "HTTP_DISABLED".to_string(),
                        message: "benchmark harness forbids pi.http".to_string(),
                    },
                ),
            };

            if kind_key != "tool.__bench_report" {
                *unexpected_hostcalls.entry(kind_key).or_insert(0) += 1;
            }

            runtime.complete_hostcall(req.call_id, outcome);
            // Deliver the completion (one macrotask) and any microtasks it triggers.
            let _ = runtime.tick().await?;
        }

        // Drain any promise microtasks (even if no macrotasks ran).
        let _ = runtime.drain_microtasks().await?;

        if let Some(report) = report.take() {
            let elapsed = started_at.elapsed();
            return Ok(BenchPumpOutcome {
                report,
                unexpected_hostcalls,
                elapsed,
            });
        }

        if runtime.has_pending() {
            let _ = runtime.tick().await?;
        }
    }

    Err(Error::extension(format!(
        "benchmark timed out after {}ms",
        budget.as_millis()
    )))
}

fn report_ok_or_err(report: &Value) -> Result<()> {
    let ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        return Ok(());
    }
    let err = report
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown js error");
    Err(Error::extension(format!("js bench failed: {err}")))
}

async fn load_extension(runtime: &PiJsRuntime, spec: &JsExtensionLoadSpec) -> Result<()> {
    let ext_id = js_literal(&spec.extension_id)?;
    let entry = js_literal(&spec.entry_path.display().to_string())?;
    let meta = js_literal(&json!({
        "name": spec.name,
        "version": spec.version,
        "apiVersion": spec.api_version,
    }))?;

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
",
        bench_tool = js_literal(&BENCH_REPORT_TOOL)?,
    );

    let outcome = run_bench_js(runtime, &js, Duration::from_secs(10)).await?;
    report_ok_or_err(&outcome.report)?;
    if !outcome.unexpected_hostcalls.is_empty() {
        return Err(Error::extension(format!(
            "unexpected hostcalls during extension load: {:?}",
            outcome.unexpected_hostcalls
        )));
    }
    Ok(())
}

async fn scenario_load_init_cold(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    runs: usize,
) -> Result<Value> {
    let mut timings = Vec::new();
    for _ in 0..runs {
        let start = Instant::now();
        let runtime = new_runtime(js_cwd).await?;
        load_extension(&runtime, spec).await?;
        timings.push(start.elapsed());
    }

    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "ext_load_init/load_init_cold",
        "extension": spec.extension_id,
        "runs": runs,
        "summary": summarize_ms(&timings),
    }))
}

async fn scenario_tool_call(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    let tool_name = js_literal(&"hello")?;
    let call_id = js_literal(&"bench-call-1")?;
    let input = js_literal(&json!({"name": "World"}))?;
    let ctx = js_literal(&json!({"hasUI": false, "cwd": js_cwd}))?;
    let iterations_js = js_literal(&iterations)?;

    let js = format!(
        r"
(async () => {{
  try {{
    const N = {iterations};
    for (let i = 0; i < N; i++) {{
      await __pi_execute_tool({tool_name}, {call_id}, {input}, {ctx});
    }}
    await pi.tool({bench_tool}, {{ ok: true }});
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    await pi.tool({bench_tool}, {{ ok: false, error: msg }});
  }}
}})();
",
        iterations = iterations_js,
        bench_tool = js_literal(&BENCH_REPORT_TOOL)?,
    );

    let outcome = run_bench_js(&runtime, &js, Duration::from_secs(30)).await?;
    report_ok_or_err(&outcome.report)?;

    let elapsed = outcome.elapsed;
    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations.max(1));
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "ext_tool_call/hello",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "unexpected_hostcalls": outcome.unexpected_hostcalls,
    }))
}

async fn scenario_event_hook(
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

    let js = format!(
        r"
(async () => {{
  try {{
    const N = {iterations};
    for (let i = 0; i < N; i++) {{
      await __pi_dispatch_extension_event({event_name}, {event_payload}, {ctx});
    }}
    await pi.tool({bench_tool}, {{ ok: true }});
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    await pi.tool({bench_tool}, {{ ok: false, error: msg }});
  }}
}})();
",
        iterations = iterations_js,
        bench_tool = js_literal(&BENCH_REPORT_TOOL)?,
    );

    let outcome = run_bench_js(&runtime, &js, Duration::from_secs(30)).await?;
    report_ok_or_err(&outcome.report)?;

    let elapsed = outcome.elapsed;
    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations.max(1));
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "ext_event_hook/before_agent_start",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "unexpected_hostcalls": outcome.unexpected_hostcalls,
    }))
}
