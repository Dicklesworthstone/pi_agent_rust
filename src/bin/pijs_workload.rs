//! `PiJS` workload harness for deterministic perf baselines.
#![forbid(unsafe_code)]

use clap::Parser;
use futures::executor::block_on;
use pi::error::{Error, Result};
use pi::extensions_js::PiJsRuntime;
use pi::scheduler::HostcallOutcome;
use serde_json::json;
use std::time::Instant;

const BENCH_TOOL_SETUP: &str = r#"
__pi_begin_extension("ext.bench", { name: "Bench" });
pi.registerTool({
  name: "bench_tool",
  description: "Benchmark tool",
  parameters: { type: "object", properties: { value: { type: "number" } } },
  execute: async (_callId, input) => {
    return { ok: true, value: input.value };
  },
});
__pi_end_extension();
"#;

const BENCH_TOOL_CALL: &str = r#"
globalThis.__bench_done = false;
pi.tool("bench_tool", { value: 1 }).then(() => { globalThis.__bench_done = true; });
"#;

const BENCH_TOOL_ASSERT: &str = r#"
if (!globalThis.__bench_done) {
  throw new Error("bench tool call did not resolve");
}
"#;

#[derive(Parser, Debug)]
#[command(name = "pijs_workload")]
#[command(about = "Deterministic PiJS workload runner for perf baselines")]
struct Args {
    /// Outer loop iterations.
    #[arg(long, default_value_t = 200)]
    iterations: usize,
    /// Tool calls per iteration.
    #[arg(long, default_value_t = 1)]
    tool_calls: usize,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let runtime = block_on(PiJsRuntime::new())?;
    block_on(runtime.eval(BENCH_TOOL_SETUP))?;

    let start = Instant::now();
    for _ in 0..args.iterations {
        for _ in 0..args.tool_calls {
            run_tool_roundtrip(&runtime)?;
        }
    }
    let elapsed = start.elapsed();

    let total_calls = args.iterations.saturating_mul(args.tool_calls);
    let elapsed_ms = elapsed.as_millis();
    let elapsed_us = elapsed.as_micros();
    let total_calls_u128 = total_calls as u128;

    let per_call_us = if total_calls == 0 {
        0
    } else {
        elapsed_us / total_calls_u128
    };
    let calls_per_sec = if elapsed_us == 0 {
        0
    } else {
        total_calls_u128.saturating_mul(1_000_000) / elapsed_us
    };

    println!(
        "{}",
        json!({
            "scenario": "tool_call_roundtrip",
            "iterations": args.iterations,
            "tool_calls_per_iteration": args.tool_calls,
            "total_calls": total_calls,
            "elapsed_ms": elapsed_ms,
            "per_call_us": per_call_us,
            "calls_per_sec": calls_per_sec,
        })
    );

    Ok(())
}

fn run_tool_roundtrip(runtime: &PiJsRuntime) -> Result<()> {
    block_on(async {
        runtime.eval(BENCH_TOOL_CALL).await?;
        let mut requests = runtime.drain_hostcall_requests();
        let request = requests
            .pop_front()
            .ok_or_else(|| Error::extension("bench workload: missing hostcall request"))?;
        if !requests.is_empty() {
            return Err(Error::extension(
                "bench workload: unexpected extra hostcall requests",
            ));
        }

        runtime.complete_hostcall(
            request.call_id,
            HostcallOutcome::Success(json!({"ok": true})),
        );
        runtime.tick().await?;
        runtime.eval(BENCH_TOOL_ASSERT).await?;
        Ok(())
    })
}
