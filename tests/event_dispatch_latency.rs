//! Extension event dispatch latency benchmarks (bd-1m27).
//!
//! Measures round-trip time for event dispatch through the extension pipeline:
//! Rust → JS handler → Rust result. Targets <5ms P99 for all event types.
//!
//! Scenarios:
//! - Per-event-type latency with a single extension
//! - Scaling: 1, 5, 10, 20 extensions with event hooks
//! - Hostcall round-trip latency (tool call from event handler)
//! - Context complexity impact (empty vs full session context)
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default iterations per event type.
const ITERATIONS: u64 = 200;
/// P99 budget in microseconds — release target is 5ms, debug is relaxed to 200ms
/// because debug builds have ~10-50x overhead from bounds checks and no inlining.
const P99_BUDGET_US: u64 = if cfg!(debug_assertions) {
    200_000
} else {
    5_000
};
/// Warmup iterations before measurement.
const WARMUP: u64 = 10;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn report_dir() -> PathBuf {
    project_root().join("target/perf")
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let rank = (len * numerator).saturating_add(denominator - 1) / denominator;
    rank.saturating_sub(1).min(len - 1)
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[percentile_index(sorted.len(), pct, 100)]
}

fn summarize_us(values: &[u64]) -> Value {
    if values.is_empty() {
        return json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let sum: u128 = sorted.iter().map(|v| u128::from(*v)).sum();
    let mean = u64::try_from(sum / (sorted.len() as u128)).unwrap_or(u64::MAX);
    json!({
        "count": sorted.len(),
        "min_us": sorted[0],
        "max_us": sorted[sorted.len() - 1],
        "mean_us": mean,
        "p50_us": percentile(&sorted, 50),
        "p95_us": percentile(&sorted, 95),
        "p99_us": percentile(&sorted, 99),
    })
}

// ─── Extension Synthesis ────────────────────────────────────────────────────

/// Generate JS source for a synthetic extension that hooks the specified events.
/// Each hook simply returns undefined (no-op) so we measure pure dispatch overhead.
fn synth_noop_extension(ext_index: usize, events: &[&str]) -> String {
    let mut hooks = String::new();
    for event in events {
        let _ = writeln!(
            hooks,
            r#"  pi.on("{event}", async (ev) => {{ return undefined; }});"#
        );
    }
    format!(
        r"export default function synthExt{ext_index}(pi) {{
{hooks}}}
"
    )
}

/// Generate JS source for an extension whose event hook makes a hostcall (tool call).
fn synth_hostcall_extension(ext_index: usize) -> String {
    format!(
        r#"export default function synthHostcall{ext_index}(pi) {{
  pi.on("agent_start", async (ev) => {{
    // Make a hostcall that round-trips through Rust
    const result = await pi.session("getSessionName", {{}});
    return undefined;
  }});
}}
"#
    )
}

// ─── Extension Loading ──────────────────────────────────────────────────────

struct LoadedExtensions {
    manager: ExtensionManager,
    count: usize,
}

fn load_synthetic_extensions(sources: &[(String, String)]) -> LoadedExtensions {
    let harness = common::TestHarness::new("event_dispatch_latency");
    let cwd = harness.temp_dir().to_path_buf();

    // Write each extension to a file
    let mut specs = Vec::new();
    for (name, source) in sources {
        let ext_dir = cwd.join("extensions").join(name);
        let _ = std::fs::create_dir_all(&ext_dir);
        let entry_path = ext_dir.join(format!("{name}.mjs"));
        std::fs::write(&entry_path, source).expect("write extension source");

        match JsExtensionLoadSpec::from_entry_path(&entry_path) {
            Ok(spec) => specs.push(spec),
            Err(e) => panic!("failed to create spec for {name}: {e}"),
        }
    }

    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let manager = ExtensionManager::new();
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
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let count = specs.len();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load synthetic extensions");
        }
    });

    // Keep harness alive to preserve temp dir
    std::mem::forget(harness);

    LoadedExtensions { manager, count }
}

// ─── Benchmark Runner ───────────────────────────────────────────────────────

struct LatencyResult {
    event: String,
    extensions: usize,
    iterations: u64,
    latencies_us: Vec<u64>,
    p99_within_budget: bool,
}

fn measure_event_latency(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: &Option<Value>,
    iterations: u64,
    warmup: u64,
) -> LatencyResult {
    // Warmup
    for _ in 0..warmup {
        let _ = common::run_async({
            let manager = manager.clone();
            let payload = payload.clone();
            async move { manager.dispatch_event(event, payload).await }
        });
    }

    // Measure
    let mut latencies_us = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let start = Instant::now();
        let _ = common::run_async({
            let manager = manager.clone();
            let payload = payload.clone();
            async move { manager.dispatch_event(event, payload).await }
        });
        let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        latencies_us.push(elapsed_us);
    }

    let mut sorted = latencies_us.clone();
    sorted.sort_unstable();
    let p99 = percentile(&sorted, 99);

    LatencyResult {
        event: format!("{event:?}"),
        extensions: 0, // caller sets
        iterations,
        latencies_us,
        p99_within_budget: p99 <= P99_BUDGET_US,
    }
}

fn result_to_jsonl(result: &LatencyResult) -> Value {
    json!({
        "schema": "pi.ext.event_dispatch_latency.v1",
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "event": result.event,
        "extensions": result.extensions,
        "iterations": result.iterations,
        "summary": summarize_us(&result.latencies_us),
        "budget_us": P99_BUDGET_US,
        "within_budget": result.p99_within_budget,
    })
}

// ============================================================================
// Tests
// ============================================================================

/// Measure per-event-type latency with a single no-op extension.
#[test]
fn event_type_latency_single_extension() {
    let events_to_test = [
        "before_agent_start",
        "agent_start",
        "agent_end",
        "turn_start",
        "turn_end",
        "input",
    ];

    let source = synth_noop_extension(0, &events_to_test);
    let loaded = load_synthetic_extensions(&[("synth-noop-0".to_string(), source)]);
    assert_eq!(loaded.count, 1);

    let event_enums = [
        ExtensionEventName::BeforeAgentStart,
        ExtensionEventName::AgentStart,
        ExtensionEventName::AgentEnd,
        ExtensionEventName::TurnStart,
        ExtensionEventName::TurnEnd,
        ExtensionEventName::Input,
    ];

    let mut results = Vec::new();
    let mut all_within_budget = true;

    for (i, event) in event_enums.iter().enumerate() {
        let payload = json!({ "systemPrompt": "test", "index": i });
        let mut result =
            measure_event_latency(&loaded.manager, *event, &Some(payload), ITERATIONS, WARMUP);
        result.extensions = 1;

        let summary = summarize_us(&result.latencies_us);
        eprintln!(
            "  {:<25} p50={:>6}us  p95={:>6}us  p99={:>6}us  {}",
            result.event,
            summary["p50_us"],
            summary["p95_us"],
            summary["p99_us"],
            if result.p99_within_budget {
                "PASS"
            } else {
                "FAIL"
            }
        );

        if !result.p99_within_budget {
            all_within_budget = false;
        }
        results.push(result);
    }

    // Write JSONL report
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);
    let report_path = report_dir.join("event_dispatch_latency.jsonl");
    let lines: Vec<String> = results
        .iter()
        .map(|r| serde_json::to_string(&result_to_jsonl(r)).unwrap_or_default())
        .collect();
    let _ = std::fs::write(&report_path, lines.join("\n") + "\n");
    eprintln!("  Report: {}", report_path.display());

    assert!(
        all_within_budget,
        "All event types should have p99 < {P99_BUDGET_US}us"
    );

    // Cleanup
    common::run_async({
        let manager = loaded.manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

/// Measure scaling: how latency changes with 1, 5, 10, 20 extensions.
#[test]
fn event_dispatch_scaling() {
    let scale_levels = [1, 5, 10, 20];
    let event = ExtensionEventName::AgentStart;
    let hook_events = ["agent_start"];

    let mut all_results = Vec::new();
    eprintln!("\n  Extension count scaling (event: AgentStart):");
    eprintln!(
        "  {:<12} {:<10} {:<10} {:<10} {:<10} Status",
        "Extensions", "p50 (us)", "p95 (us)", "p99 (us)", "max (us)"
    );

    for &count in &scale_levels {
        let sources: Vec<(String, String)> = (0..count)
            .map(|i| {
                (
                    format!("synth-scale-{i}"),
                    synth_noop_extension(i, &hook_events),
                )
            })
            .collect();

        let loaded = load_synthetic_extensions(&sources);
        assert_eq!(loaded.count, count);

        let payload = json!({ "systemPrompt": "test" });
        let mut result =
            measure_event_latency(&loaded.manager, event, &Some(payload), ITERATIONS, WARMUP);
        result.extensions = count;

        let summary = summarize_us(&result.latencies_us);
        eprintln!(
            "  {:<12} {:<10} {:<10} {:<10} {:<10} {}",
            count,
            summary["p50_us"],
            summary["p95_us"],
            summary["p99_us"],
            summary["max_us"],
            if result.p99_within_budget {
                "PASS"
            } else {
                "FAIL"
            }
        );

        all_results.push(result);

        // Cleanup
        common::run_async({
            let manager = loaded.manager;
            async move {
                let _ = manager.shutdown(Duration::from_millis(500)).await;
            }
        });
    }

    // Write scaling report
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);
    let report_path = report_dir.join("event_dispatch_scaling.jsonl");
    let lines: Vec<String> = all_results
        .iter()
        .map(|r| serde_json::to_string(&result_to_jsonl(r)).unwrap_or_default())
        .collect();
    let _ = std::fs::write(&report_path, lines.join("\n") + "\n");
    eprintln!("  Report: {}", report_path.display());

    // All scale levels should meet the budget
    for result in &all_results {
        assert!(
            result.p99_within_budget,
            "{} extensions: p99 should be < {P99_BUDGET_US}us",
            result.extensions
        );
    }
}

/// Measure hostcall round-trip latency (event handler that calls back into Rust).
#[test]
fn hostcall_roundtrip_latency() {
    let source = synth_hostcall_extension(0);
    let loaded = load_synthetic_extensions(&[("synth-hostcall-0".to_string(), source)]);
    assert_eq!(loaded.count, 1);

    let event = ExtensionEventName::AgentStart;
    let payload = json!({ "systemPrompt": "test" });

    let mut result =
        measure_event_latency(&loaded.manager, event, &Some(payload), ITERATIONS, WARMUP);
    result.extensions = 1;

    let summary = summarize_us(&result.latencies_us);
    eprintln!(
        "  Hostcall roundtrip: p50={}us  p95={}us  p99={}us  {}",
        summary["p50_us"],
        summary["p95_us"],
        summary["p99_us"],
        if result.p99_within_budget {
            "PASS"
        } else {
            "FAIL"
        }
    );

    // Write report
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);
    let report_path = report_dir.join("event_dispatch_hostcall.jsonl");
    let entry = result_to_jsonl(&result);
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string(&entry).unwrap_or_default() + "\n",
    );
    eprintln!("  Report: {}", report_path.display());

    assert!(
        result.p99_within_budget,
        "Hostcall roundtrip p99 should be < {P99_BUDGET_US}us"
    );

    // Cleanup
    common::run_async({
        let manager = loaded.manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

/// Measure overhead of dispatching events with no matching handlers.
#[test]
fn no_handler_dispatch_overhead() {
    // Extension hooks agent_start, but we dispatch turn_start → should be fast (no-op)
    let source = synth_noop_extension(0, &["agent_start"]);
    let loaded = load_synthetic_extensions(&[("synth-no-match-0".to_string(), source)]);
    assert_eq!(loaded.count, 1);

    let event = ExtensionEventName::TurnStart;
    let payload = json!({ "turnIndex": 1 });

    let mut result =
        measure_event_latency(&loaded.manager, event, &Some(payload), ITERATIONS, WARMUP);
    result.extensions = 1;

    let summary = summarize_us(&result.latencies_us);
    eprintln!(
        "  No-handler dispatch: p50={}us  p95={}us  p99={}us  {}",
        summary["p50_us"],
        summary["p95_us"],
        summary["p99_us"],
        if result.p99_within_budget {
            "PASS"
        } else {
            "FAIL"
        }
    );

    assert!(
        result.p99_within_budget,
        "No-handler dispatch p99 should be < {P99_BUDGET_US}us"
    );

    // Cleanup
    common::run_async({
        let manager = loaded.manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

/// Measure latency with real extensions from the conformance corpus.
#[test]
#[allow(clippy::too_many_lines)]
fn real_extension_dispatch_latency() {
    let manifest_path = project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let Ok(data) = std::fs::read_to_string(&manifest_path) else {
        eprintln!("  Skipping: VALIDATED_MANIFEST.json not found");
        return;
    };
    let manifest: Value = serde_json::from_str(&data).expect("parse manifest");
    let extensions = manifest["extensions"].as_array().expect("extensions array");

    let artifacts = project_root().join("tests/ext_conformance/artifacts");
    let mut paths = Vec::new();

    for ext in extensions {
        if paths.len() >= 10 {
            break;
        }
        if ext["source_tier"].as_str() != Some("official-pi-mono") {
            continue;
        }
        let caps = &ext["capabilities"];
        if caps["uses_exec"].as_bool() == Some(true) {
            continue;
        }
        if caps["is_multi_file"].as_bool() == Some(true) {
            continue;
        }
        if let Some(entry_path) = ext["entry_path"].as_str() {
            let full_path = artifacts.join(entry_path);
            if full_path.exists() {
                paths.push(full_path);
            }
        }
    }

    if paths.is_empty() {
        eprintln!("  Skipping: no suitable real extensions found");
        return;
    }

    let cwd = project_root();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let manager = ExtensionManager::new();
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
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let mut specs = Vec::new();
    for path in &paths {
        match JsExtensionLoadSpec::from_entry_path(path) {
            Ok(spec) => specs.push(spec),
            Err(e) => eprintln!("  skip {}: {e}", path.display()),
        }
    }

    let ext_count = specs.len();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load real extensions");
        }
    });

    eprintln!("\n  Real extension dispatch ({ext_count} extensions loaded):");

    let events_to_test = [
        (ExtensionEventName::BeforeAgentStart, "BeforeAgentStart"),
        (ExtensionEventName::AgentStart, "AgentStart"),
        (ExtensionEventName::TurnStart, "TurnStart"),
    ];

    let mut all_results = Vec::new();

    for (event, label) in &events_to_test {
        let payload = json!({ "systemPrompt": "test" });
        let mut result = measure_event_latency(&manager, *event, &Some(payload), ITERATIONS, WARMUP);
        result.extensions = ext_count;

        let summary = summarize_us(&result.latencies_us);
        eprintln!(
            "  {:<25} p50={:>6}us  p95={:>6}us  p99={:>6}us  {}",
            label,
            summary["p50_us"],
            summary["p95_us"],
            summary["p99_us"],
            if result.p99_within_budget {
                "PASS"
            } else {
                "FAIL"
            }
        );

        all_results.push(result);
    }

    // Write report
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);
    let report_path = report_dir.join("event_dispatch_real.jsonl");
    let lines: Vec<String> = all_results
        .iter()
        .map(|r| serde_json::to_string(&result_to_jsonl(r)).unwrap_or_default())
        .collect();
    let _ = std::fs::write(&report_path, lines.join("\n") + "\n");
    eprintln!("  Report: {}", report_path.display());

    // Cleanup
    common::run_async({
        let manager = manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

/// Generate consolidated JSON report with all latency data.
#[test]
#[ignore = "report generator: run manually after other tests"]
fn generate_latency_report() {
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);

    // Collect all event dispatch JSONL files
    let jsonl_files = [
        "event_dispatch_latency.jsonl",
        "event_dispatch_scaling.jsonl",
        "event_dispatch_hostcall.jsonl",
        "event_dispatch_real.jsonl",
    ];

    let mut all_records: Vec<Value> = Vec::new();
    for filename in &jsonl_files {
        let path = report_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                if let Ok(record) = serde_json::from_str::<Value>(line) {
                    all_records.push(record);
                }
            }
        }
    }

    if all_records.is_empty() {
        eprintln!("  No latency data found. Run the other tests first.");
        return;
    }

    // Build summary report
    let report = json!({
        "schema": "pi.ext.event_dispatch_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "budget_us": P99_BUDGET_US,
        "records": all_records.len(),
        "results": all_records,
    });

    let report_path = report_dir.join("event_dispatch_report.json");
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );
    eprintln!("  Full report: {}", report_path.display());

    // Generate markdown summary
    let mut md = String::new();
    let _ = writeln!(md, "# Event Dispatch Latency Report\n");
    let _ = writeln!(
        md,
        "Generated: {}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = writeln!(
        md,
        "Budget: P99 < {}us ({}ms)\n",
        P99_BUDGET_US,
        P99_BUDGET_US / 1000
    );
    let _ = writeln!(
        md,
        "| Event | Extensions | P50 (us) | P95 (us) | P99 (us) | Status |"
    );
    let _ = writeln!(
        md,
        "|-------|-----------|----------|----------|----------|--------|"
    );

    for record in &all_records {
        let event = record["event"].as_str().unwrap_or("?");
        let exts = record["extensions"].as_u64().unwrap_or(0);
        let summary = &record["summary"];
        let p50 = summary["p50_us"].as_u64().unwrap_or(0);
        let p95 = summary["p95_us"].as_u64().unwrap_or(0);
        let p99 = summary["p99_us"].as_u64().unwrap_or(0);
        let within = record["within_budget"].as_bool().unwrap_or(false);
        let status = if within { "PASS" } else { "FAIL" };
        let _ = writeln!(
            md,
            "| {event:<25} | {exts:<9} | {p50:<8} | {p95:<8} | {p99:<8} | {status} |"
        );
    }

    let md_path = report_dir.join("reports/EVENT_DISPATCH_LATENCY.md");
    let _ = std::fs::create_dir_all(report_dir.join("reports"));
    let _ = std::fs::write(&md_path, md);
    eprintln!("  Markdown: {}", md_path.display());
}
