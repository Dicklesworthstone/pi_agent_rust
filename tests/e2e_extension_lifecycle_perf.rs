//! E2E extension lifecycle and performance diagnostics (bd-3ar8v.4.9).
//!
//! Covers the full extension lifecycle: load → init → tool-call → event-hook →
//! budget-fallback → shutdown, with per-extension latency and resource diagnostics.
//! Uses real extensions from the conformance corpus (hello, pirate, diff).
//!
//! Outputs structured JSONL to `target/perf/e2e_lifecycle.jsonl` for CI integration.

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::future_not_send,
    clippy::doc_markdown
)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Extensions to test from the conformance corpus.
const LIFECYCLE_EXTENSIONS: &[&str] = &["hello", "pirate", "diff"];

/// Event dispatch iterations for latency measurement.
const EVENT_ITERATIONS: usize = 50;

/// Release event-dispatch budget is 100ms; debug/RCH full-suite runs get
/// extra headroom for unoptimized JS/runtime dispatch and scheduler noise.
const EVENT_DISPATCH_BUDGET_US: f64 = if cfg!(debug_assertions) {
    200_000.0
} else {
    100_000.0
};

/// Tool call iterations for latency measurement.
const TOOL_ITERATIONS: usize = 50;

/// Budget timeout for normal operations (ms).
const NORMAL_TIMEOUT_MS: u64 = 5_000;

/// Short timeout for budget-fallback testing (ms).
const SHORT_TIMEOUT_MS: u64 = 50;

// ─── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct LifecycleRecord {
    schema: String,
    extension: String,
    phase: String,
    iterations: usize,
    elapsed_ms: f64,
    per_call_us: f64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnosticsRecord {
    schema: String,
    extension: String,
    risk_ledger_entries: usize,
    hostcall_telemetry_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_state: Option<QuotaSnapshot>,
    snapshot_version: u64,
}

#[derive(Debug, Clone, Serialize)]
struct QuotaSnapshot {
    budget_remaining_ns: u64,
    call_count: u32,
    last_call_ns: u64,
    total_elapsed_ns: u64,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

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

fn write_jsonl(records: &[Value], path: &Path) {
    use std::fmt::Write as _;
    let mut content = String::new();
    for record in records {
        let _ = writeln!(
            content,
            "{}",
            serde_json::to_string(record).unwrap_or_default()
        );
    }
    fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .expect("create lifecycle perf output dir");
    fs::write(path, &content).expect("write lifecycle perf JSONL");
}

/// Create an `ExtensionManager` with a JS runtime, load the given spec, return manager.
fn setup_manager_with_extension(
    harness: &common::TestHarness,
    spec: &JsExtensionLoadSpec,
) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
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
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

fn shutdown(manager: &ExtensionManager) {
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_millis(500)).await }
    });
}

fn make_record(
    ext: &str,
    phase: &str,
    iterations: usize,
    elapsed: Duration,
    success: bool,
    error: Option<String>,
) -> LifecycleRecord {
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    let per_call_us = if iterations > 0 {
        (elapsed.as_secs_f64() * 1_000_000.0) / iterations as f64
    } else {
        0.0
    };
    LifecycleRecord {
        schema: "pi.ext.lifecycle_perf.v1".to_string(),
        extension: ext.to_string(),
        phase: phase.to_string(),
        iterations,
        elapsed_ms,
        per_call_us,
        success,
        error,
    }
}

// ─── Phase Runners ──────────────────────────────────────────────────────────

/// Phase 1: Cold load — create manager + runtime + load from scratch.
fn phase_cold_load(
    ext_name: &str,
    spec: &JsExtensionLoadSpec,
    harness: &common::TestHarness,
) -> LifecycleRecord {
    let start = Instant::now();
    let cwd = harness.temp_dir().to_path_buf();

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let result: Result<(), String> = (|| {
        let runtime = common::run_async({
            let manager = manager.clone();
            let tools = Arc::clone(&tools);
            async move {
                JsExtensionRuntimeHandle::start(js_config, tools, manager)
                    .await
                    .map_err(|e| e.to_string())
            }
        })?;
        manager.set_js_runtime(runtime);

        common::run_async({
            let manager = manager.clone();
            let spec = spec.clone();
            async move {
                manager
                    .load_js_extensions(vec![spec])
                    .await
                    .map_err(|e| e.to_string())
            }
        })?;

        Ok(())
    })();

    let elapsed = start.elapsed();
    shutdown(&manager);

    match result {
        Ok(()) => make_record(ext_name, "cold_load", 1, elapsed, true, None),
        Err(e) => make_record(ext_name, "cold_load", 1, elapsed, false, Some(e)),
    }
}

/// Phase 2: Event hook dispatch — fire `before_agent_start` N times.
fn phase_event_dispatch(
    ext_name: &str,
    manager: &ExtensionManager,
    iterations: usize,
) -> LifecycleRecord {
    let start = Instant::now();
    let mut successes = 0usize;
    let mut last_err = None;

    for _ in 0..iterations {
        let result = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(json!({"systemPrompt": "lifecycle test"})),
                        NORMAL_TIMEOUT_MS,
                    )
                    .await
            }
        });
        match result {
            Ok(_) => successes += 1,
            Err(e) => last_err = Some(e.to_string()),
        }
    }

    let elapsed = start.elapsed();
    let success = successes == iterations;
    make_record(
        ext_name,
        "event_dispatch",
        iterations,
        elapsed,
        success,
        last_err,
    )
}

/// Phase 3: Tool call — execute the extension's tool N times (if it has one).
fn phase_tool_call(
    ext_name: &str,
    manager: &ExtensionManager,
    iterations: usize,
) -> LifecycleRecord {
    let Some(runtime) = manager.js_runtime() else {
        return make_record(
            ext_name,
            "tool_call",
            0,
            Duration::ZERO,
            false,
            Some("no JS runtime available".to_string()),
        );
    };

    // Tool name matches extension name for hello/pirate; diff has "diff" command not tool.
    // Try the extension name as tool name first.
    let tool_name = ext_name.to_string();
    let ctx = json!({ "hasUI": false, "cwd": project_root().display().to_string() });

    let start = Instant::now();
    let mut successes = 0usize;
    let mut last_err = None;

    for i in 0..iterations {
        let result = futures::executor::block_on(runtime.execute_tool(
            tool_name.clone(),
            format!("lifecycle-{i}"),
            json!({"name": "test"}),
            std::sync::Arc::new(ctx.clone()),
            NORMAL_TIMEOUT_MS,
        ));
        match result {
            Ok(_) => successes += 1,
            Err(e) => last_err = Some(e.to_string()),
        }
    }

    let elapsed = start.elapsed();
    // Some extensions (pirate, diff) may not have a tool named after them — that's OK.
    make_record(
        ext_name,
        "tool_call",
        iterations,
        elapsed,
        successes > 0,
        last_err,
    )
}

/// Phase 4: Budget fallback — dispatch with a very short timeout.
fn phase_budget_fallback(ext_name: &str, manager: &ExtensionManager) -> LifecycleRecord {
    let start = Instant::now();

    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "budget fallback test"})),
                    SHORT_TIMEOUT_MS,
                )
                .await
        }
    });

    let elapsed = start.elapsed();
    // Budget fallback should not hang — either succeed quickly or return error/None.
    let success = elapsed < Duration::from_secs(3);
    let error = if success {
        result.err().map(|e| e.to_string())
    } else {
        Some(format!("budget fallback took {elapsed:?}, expected <3s"))
    };
    make_record(ext_name, "budget_fallback", 1, elapsed, success, error)
}

/// Phase 5: Shutdown — measure time to shut down the manager.
fn phase_shutdown(ext_name: &str, manager: &ExtensionManager) -> LifecycleRecord {
    let start = Instant::now();
    let ok = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });
    let elapsed = start.elapsed();
    let success = ok && elapsed < Duration::from_secs(2);
    let error = if !ok {
        Some("shutdown returned false".to_string())
    } else if elapsed >= Duration::from_secs(2) {
        Some(format!("shutdown took {elapsed:?}"))
    } else {
        None
    };
    make_record(ext_name, "shutdown", 1, elapsed, success, error)
}

/// Collect per-extension diagnostics.
fn collect_diagnostics(ext_name: &str, manager: &ExtensionManager) -> DiagnosticsRecord {
    let risk_ledger = manager.runtime_risk_ledger_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let quota = manager
        .quota_state(ext_name)
        .map(|(budget, calls, last, total)| QuotaSnapshot {
            budget_remaining_ns: budget,
            call_count: calls,
            last_call_ns: last,
            total_elapsed_ns: total,
        });

    DiagnosticsRecord {
        schema: "pi.ext.lifecycle_diagnostics.v1".to_string(),
        extension: ext_name.to_string(),
        risk_ledger_entries: risk_ledger.entries.len(),
        hostcall_telemetry_entries: telemetry.entries.len(),
        quota_state: quota,
        snapshot_version: manager.snapshot_version(),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Full lifecycle test: load → event → tool → budget-fallback → diagnostics → shutdown
/// for each extension in the corpus.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_full_lifecycle_all_extensions() {
    let mut all_records: Vec<Value> = Vec::new();
    let mut per_ext_summary: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            eprintln!(
                "[skip] {ext_name}: artifact not found at {}",
                entry.display()
            );
            continue;
        }

        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
        let harness = common::TestHarness::new(format!("lifecycle_{ext_name}"));

        // Phase 1: Cold load
        eprintln!("[lifecycle] {ext_name}: cold_load");
        let cold = phase_cold_load(ext_name, &spec, &harness);
        all_records.push(serde_json::to_value(&cold).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "cold_load: {:.2}ms ok={}",
                cold.elapsed_ms, cold.success
            ));

        // Create a persistent manager for remaining phases
        let manager = setup_manager_with_extension(&harness, &spec);

        // Phase 2: Event dispatch
        eprintln!("[lifecycle] {ext_name}: event_dispatch ({EVENT_ITERATIONS} iters)");
        let events = phase_event_dispatch(ext_name, &manager, EVENT_ITERATIONS);
        all_records.push(serde_json::to_value(&events).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "event_dispatch: {:.1}us/call ok={}",
                events.per_call_us, events.success
            ));

        // Phase 3: Tool call
        eprintln!("[lifecycle] {ext_name}: tool_call ({TOOL_ITERATIONS} iters)");
        let tools = phase_tool_call(ext_name, &manager, TOOL_ITERATIONS);
        all_records.push(serde_json::to_value(&tools).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "tool_call: {:.1}us/call ok={}",
                tools.per_call_us, tools.success
            ));

        // Phase 4: Budget fallback
        eprintln!("[lifecycle] {ext_name}: budget_fallback");
        let fallback = phase_budget_fallback(ext_name, &manager);
        all_records.push(serde_json::to_value(&fallback).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "budget_fallback: {:.2}ms ok={}",
                fallback.elapsed_ms, fallback.success
            ));

        // Collect diagnostics before shutdown
        eprintln!("[lifecycle] {ext_name}: diagnostics");
        let diag = collect_diagnostics(ext_name, &manager);
        all_records.push(serde_json::to_value(&diag).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "diag: risk={} telemetry={} snap={}",
                diag.risk_ledger_entries, diag.hostcall_telemetry_entries, diag.snapshot_version
            ));

        // Phase 5: Shutdown
        eprintln!("[lifecycle] {ext_name}: shutdown");
        let shut = phase_shutdown(ext_name, &manager);
        all_records.push(serde_json::to_value(&shut).unwrap());
        per_ext_summary
            .entry(ext_name.to_string())
            .or_default()
            .push(format!(
                "shutdown: {:.2}ms ok={}",
                shut.elapsed_ms, shut.success
            ));
    }

    // Write JSONL output
    let output_path = perf_output_path("e2e_lifecycle.jsonl");
    write_jsonl(&all_records, &output_path);
    eprintln!(
        "\n[output] {} records written to {}",
        all_records.len(),
        output_path.display()
    );

    // Print summary
    eprintln!("\n=== E2E Extension Lifecycle Summary ===");
    for (ext, phases) in &per_ext_summary {
        eprintln!("  {ext}:");
        for phase in phases {
            eprintln!("    {phase}");
        }
    }

    // Assertions
    assert!(
        per_ext_summary.len() >= 3,
        "expected >=3 extensions, got {}",
        per_ext_summary.len()
    );

    // All cold loads must succeed
    let cold_loads: Vec<&Value> = all_records
        .iter()
        .filter(|r| r.get("phase").and_then(Value::as_str) == Some("cold_load"))
        .collect();
    for rec in &cold_loads {
        assert_eq!(
            rec.get("success").and_then(Value::as_bool),
            Some(true),
            "cold_load failed for {}",
            rec.get("extension").and_then(Value::as_str).unwrap_or("?")
        );
    }

    // All shutdowns must succeed
    let shutdowns: Vec<&Value> = all_records
        .iter()
        .filter(|r| r.get("phase").and_then(Value::as_str) == Some("shutdown"))
        .collect();
    for rec in &shutdowns {
        assert_eq!(
            rec.get("success").and_then(Value::as_bool),
            Some(true),
            "shutdown failed for {}",
            rec.get("extension").and_then(Value::as_str).unwrap_or("?")
        );
    }

    // Budget fallback must not hang (success = returned within 3s)
    let fallbacks: Vec<&Value> = all_records
        .iter()
        .filter(|r| r.get("phase").and_then(Value::as_str) == Some("budget_fallback"))
        .collect();
    for rec in &fallbacks {
        assert_eq!(
            rec.get("success").and_then(Value::as_bool),
            Some(true),
            "budget_fallback hung for {}",
            rec.get("extension").and_then(Value::as_str).unwrap_or("?")
        );
    }

    // All records must have schema field
    for record in &all_records {
        assert!(
            record.get("schema").and_then(Value::as_str).is_some(),
            "record missing schema: {record}"
        );
    }
}

/// Verify that event dispatch latency is reasonable on average.
#[test]
fn event_dispatch_latency_within_budget() {
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
        let harness = common::TestHarness::new(format!("latency_event_{ext_name}"));
        let manager = setup_manager_with_extension(&harness, &spec);

        let record = phase_event_dispatch(ext_name, &manager, 20);
        eprintln!(
            "[latency] {ext_name} event_dispatch: {:.1}us/call",
            record.per_call_us
        );

        // Average should stay within the configured per-call budget.
        assert!(
            record.per_call_us < EVENT_DISPATCH_BUDGET_US,
            "{ext_name}: event dispatch avg {:.1}us exceeds {:.0}us budget",
            record.per_call_us,
            EVENT_DISPATCH_BUDGET_US
        );

        shutdown(&manager);
    }
}

/// Verify cold load completes within 5 seconds for each extension.
#[test]
fn cold_load_within_budget() {
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
        let harness = common::TestHarness::new(format!("cold_load_{ext_name}"));

        let record = phase_cold_load(ext_name, &spec, &harness);
        eprintln!(
            "[cold_load] {ext_name}: {:.2}ms ok={}",
            record.elapsed_ms, record.success
        );

        assert!(record.success, "{ext_name}: cold load failed");
        assert!(
            record.elapsed_ms < 5000.0,
            "{ext_name}: cold load took {:.2}ms, exceeds 5s budget",
            record.elapsed_ms
        );
    }
}

/// Verify per-extension diagnostics are collected and contain expected fields.
#[test]
fn per_extension_diagnostics_collected() {
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
        let harness = common::TestHarness::new(format!("diag_{ext_name}"));
        let manager = setup_manager_with_extension(&harness, &spec);

        // Fire some events to generate telemetry
        for _ in 0..5 {
            let _ = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event(
                            ExtensionEventName::AgentStart,
                            Some(json!({"systemPrompt": "diag test", "model": "test"})),
                        )
                        .await
                }
            });
        }

        let diag = collect_diagnostics(ext_name, &manager);
        eprintln!(
            "[diag] {ext_name}: risk={} telemetry={} snap={}",
            diag.risk_ledger_entries, diag.hostcall_telemetry_entries, diag.snapshot_version
        );

        // Snapshot version is recorded for diagnostics (may be 0 if no RCU snapshot yet).
        eprintln!(
            "[diag] {ext_name}: snapshot_version={}",
            diag.snapshot_version
        );

        // Serialization should produce valid JSON
        let json_val = serde_json::to_value(&diag).expect("serialize diagnostics");
        assert_eq!(
            json_val.get("schema").and_then(Value::as_str),
            Some("pi.ext.lifecycle_diagnostics.v1")
        );
        assert_eq!(
            json_val.get("extension").and_then(Value::as_str),
            Some(*ext_name)
        );

        shutdown(&manager);
    }
}

/// Rapid lifecycle cycling: create → load → dispatch → shutdown, repeated.
#[test]
fn rapid_lifecycle_cycling() {
    let cycles = 3;
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");

        let start = Instant::now();
        for cycle in 0..cycles {
            let harness = common::TestHarness::new(format!("rapid_{ext_name}_{cycle}"));
            let manager = setup_manager_with_extension(&harness, &spec);

            // Quick dispatch
            let _ = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event_with_response(
                            ExtensionEventName::BeforeAgentStart,
                            Some(json!({"systemPrompt": "rapid cycle"})),
                            NORMAL_TIMEOUT_MS,
                        )
                        .await
                }
            });

            shutdown(&manager);
        }
        let total = start.elapsed();
        eprintln!(
            "[rapid] {ext_name}: {cycles} cycles in {total:?} ({:.2}ms/cycle)",
            total.as_secs_f64() * 1000.0 / f64::from(cycles)
        );

        // Should not take more than 30s for 3 cycles
        assert!(
            total < Duration::from_secs(30),
            "{ext_name}: rapid cycling took {total:?}"
        );
    }
}

/// Verify that all lifecycle phases are represented in JSONL output.
#[test]
fn jsonl_output_schema_completeness() {
    let mut records: Vec<Value> = Vec::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
        let harness = common::TestHarness::new(format!("schema_{ext_name}"));

        let cold = phase_cold_load(ext_name, &spec, &harness);
        records.push(serde_json::to_value(&cold).unwrap());

        let manager = setup_manager_with_extension(&harness, &spec);
        let events = phase_event_dispatch(ext_name, &manager, 5);
        records.push(serde_json::to_value(&events).unwrap());

        let tools = phase_tool_call(ext_name, &manager, 5);
        records.push(serde_json::to_value(&tools).unwrap());

        let fallback = phase_budget_fallback(ext_name, &manager);
        records.push(serde_json::to_value(&fallback).unwrap());

        let diag = collect_diagnostics(ext_name, &manager);
        records.push(serde_json::to_value(&diag).unwrap());

        let shut = phase_shutdown(ext_name, &manager);
        records.push(serde_json::to_value(&shut).unwrap());
    }

    // Verify all phases are present
    let phases: std::collections::HashSet<&str> = records
        .iter()
        .filter_map(|r| r.get("phase").and_then(Value::as_str))
        .collect();
    for expected in &[
        "cold_load",
        "event_dispatch",
        "tool_call",
        "budget_fallback",
        "shutdown",
    ] {
        assert!(phases.contains(expected), "missing phase: {expected}");
    }

    // Verify diagnostics records are present (different schema)
    let diag_count = records
        .iter()
        .filter(|r| {
            r.get("schema").and_then(Value::as_str) == Some("pi.ext.lifecycle_diagnostics.v1")
        })
        .count();
    assert!(
        diag_count >= 3,
        "expected >=3 diagnostics records, got {diag_count}"
    );

    // Verify all extensions are represented
    let extensions: std::collections::HashSet<&str> = records
        .iter()
        .filter_map(|r| r.get("extension").and_then(Value::as_str))
        .collect();
    assert!(
        extensions.len() >= 3,
        "expected >=3 extensions in output, got {}: {extensions:?}",
        extensions.len()
    );

    // All lifecycle records must have required fields
    for record in &records {
        assert!(
            record.get("schema").and_then(Value::as_str).is_some(),
            "record missing schema"
        );
        assert!(
            record.get("extension").and_then(Value::as_str).is_some(),
            "record missing extension"
        );
    }
}

// ─── Interference / Composed Extension Measurement ──────────────────────────

/// Latency samples from dispatching events on a single extension.
#[derive(Debug, Clone, Serialize)]
struct LatencySamples {
    extension: String,
    count: usize,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    mean_us: f64,
}

fn percentile_us(sorted: &[f64], pct: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (sorted.len() * pct).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Measure per-event latency for N dispatches on a loaded manager, return sorted samples in us.
fn measure_event_latencies(manager: &ExtensionManager, iterations: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(json!({"systemPrompt": "interference test"})),
                        NORMAL_TIMEOUT_MS,
                    )
                    .await
                    .expect("dispatch_event_with_response failed");
            }
        });
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }

    samples.sort_by(f64::total_cmp);
    samples
}

fn samples_to_record(ext_label: &str, samples: &[f64]) -> LatencySamples {
    let mean = if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<f64>() / samples.len() as f64
    };
    LatencySamples {
        extension: ext_label.to_string(),
        count: samples.len(),
        p50_us: percentile_us(samples, 50),
        p95_us: percentile_us(samples, 95),
        p99_us: percentile_us(samples, 99),
        mean_us: mean,
    }
}

/// Load multiple extensions into a single manager.
fn setup_composed_manager(
    harness: &common::TestHarness,
    specs: &[JsExtensionLoadSpec],
) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
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
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        let specs = specs.to_vec();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load extensions");
        }
    });

    manager
}

/// Interference measurement: compare single-extension baselines vs composed (all 3 together).
/// Emits structured interference delta to JSONL.
#[test]
#[allow(clippy::too_many_lines)]
fn interference_single_vs_composed() {
    let iterations = 30;
    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    let mut loaded_extensions: Vec<String> = Vec::new();
    let mut baselines: BTreeMap<String, LatencySamples> = BTreeMap::new();
    let mut records: Vec<Value> = Vec::new();

    // Phase A: Measure single-extension baselines.
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("spec");
        specs.push(spec.clone());
        loaded_extensions.push(ext_name.to_string());

        let harness = common::TestHarness::new(format!("interference_single_{ext_name}"));
        let manager = setup_manager_with_extension(&harness, &spec);

        let samples = measure_event_latencies(&manager, iterations);
        let rec = samples_to_record(ext_name, &samples);
        eprintln!(
            "[interference] {ext_name} single: p50={:.1}us p95={:.1}us p99={:.1}us mean={:.1}us",
            rec.p50_us, rec.p95_us, rec.p99_us, rec.mean_us
        );
        baselines.insert(ext_name.to_string(), rec.clone());

        records.push(json!({
            "schema": "pi.ext.interference.v1",
            "phase": "baseline_single",
            "extension": ext_name,
            "extension_count": 1,
            "iterations": iterations,
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
        }));

        shutdown(&manager);
    }

    assert!(
        specs.len() >= 2,
        "need at least 2 extensions for interference measurement"
    );
    assert_eq!(
        specs.len(),
        loaded_extensions.len(),
        "loaded extension metadata should align with measured specs"
    );

    // Phase B: Measure composed (all extensions loaded together).
    let harness = common::TestHarness::new("interference_composed");
    let composed_manager = setup_composed_manager(&harness, &specs);

    let composed_samples = measure_event_latencies(&composed_manager, iterations);
    let composed_rec = samples_to_record("composed", &composed_samples);
    eprintln!(
        "[interference] composed ({}ext): p50={:.1}us p95={:.1}us p99={:.1}us mean={:.1}us",
        specs.len(),
        composed_rec.p50_us,
        composed_rec.p95_us,
        composed_rec.p99_us,
        composed_rec.mean_us,
    );

    records.push(json!({
        "schema": "pi.ext.interference.v1",
        "phase": "composed",
        "extension": "composed",
        "extension_count": specs.len(),
        "extensions_loaded": &loaded_extensions,
        "iterations": iterations,
        "p50_us": composed_rec.p50_us,
        "p95_us": composed_rec.p95_us,
        "p99_us": composed_rec.p99_us,
        "mean_us": composed_rec.mean_us,
    }));

    // Phase C: Compute interference deltas.
    // Use aggregate baseline: average of single-extension p50/p95/p99.
    let baseline_count = baselines.len() as f64;
    let avg_baseline_p50 = baselines.values().map(|b| b.p50_us).sum::<f64>() / baseline_count;
    let avg_baseline_p95 = baselines.values().map(|b| b.p95_us).sum::<f64>() / baseline_count;
    let avg_baseline_p99 = baselines.values().map(|b| b.p99_us).sum::<f64>() / baseline_count;

    let p50_ratio = if avg_baseline_p50 > 0.0 {
        composed_rec.p50_us / avg_baseline_p50
    } else {
        1.0
    };
    let p95_ratio = if avg_baseline_p95 > 0.0 {
        composed_rec.p95_us / avg_baseline_p95
    } else {
        1.0
    };
    let p99_ratio = if avg_baseline_p99 > 0.0 {
        composed_rec.p99_us / avg_baseline_p99
    } else {
        1.0
    };

    // Tail amplification: how much more the tail widens under composition.
    let single_tail_ratio = if avg_baseline_p50 > 0.0 {
        avg_baseline_p95 / avg_baseline_p50
    } else {
        1.0
    };
    let composed_tail_ratio = if composed_rec.p50_us > 0.0 {
        composed_rec.p95_us / composed_rec.p50_us
    } else {
        1.0
    };
    let tail_amplification = if single_tail_ratio > 0.0 {
        composed_tail_ratio / single_tail_ratio
    } else {
        1.0
    };

    eprintln!(
        "[interference] deltas: p50_ratio={p50_ratio:.2}x p95_ratio={p95_ratio:.2}x p99_ratio={p99_ratio:.2}x tail_amp={tail_amplification:.2}x"
    );

    records.push(json!({
        "schema": "pi.ext.interference.v1",
        "phase": "interference_delta",
        "extension": "composed",
        "extension_count": specs.len(),
        "baseline_extension_count": baselines.len(),
        "baseline_avg_p50_us": avg_baseline_p50,
        "baseline_avg_p95_us": avg_baseline_p95,
        "baseline_avg_p99_us": avg_baseline_p99,
        "composed_p50_us": composed_rec.p50_us,
        "composed_p95_us": composed_rec.p95_us,
        "composed_p99_us": composed_rec.p99_us,
        "p50_ratio": p50_ratio,
        "p95_ratio": p95_ratio,
        "p99_ratio": p99_ratio,
        "single_tail_ratio_p95_over_p50": single_tail_ratio,
        "composed_tail_ratio_p95_over_p50": composed_tail_ratio,
        "tail_amplification": tail_amplification,
    }));

    // Write interference JSONL
    let output_path = perf_output_path("e2e_interference.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} interference records written to {}",
        records.len(),
        output_path.display()
    );

    shutdown(&composed_manager);

    // Assertions: composed should not be catastrophically worse (< 10x slowdown).
    assert!(
        p50_ratio < 10.0,
        "composed p50 ratio {p50_ratio:.2}x exceeds 10x (severe interference)"
    );
    assert!(
        p95_ratio < 15.0,
        "composed p95 ratio {p95_ratio:.2}x exceeds 15x (severe tail interference)"
    );
    assert!(
        p99_ratio < 20.0,
        "composed p99 ratio {p99_ratio:.2}x exceeds 20x (severe extreme-tail interference)"
    );

    let mut phase_counts = BTreeMap::<String, usize>::new();
    for record in &records {
        assert_eq!(
            record.get("schema").and_then(Value::as_str),
            Some("pi.ext.interference.v1"),
            "interference record must keep schema contract"
        );
        let Some(phase) = record.get("phase").and_then(Value::as_str) else {
            panic!("interference record missing phase: {record}");
        };
        *phase_counts.entry(phase.to_string()).or_insert(0) += 1;
    }
    assert_eq!(
        phase_counts.get("baseline_single").copied().unwrap_or(0),
        loaded_extensions.len(),
        "expected one baseline_single row per loaded extension"
    );
    assert_eq!(
        phase_counts.get("composed").copied().unwrap_or(0),
        1,
        "expected exactly one composed row"
    );
    assert_eq!(
        phase_counts.get("interference_delta").copied().unwrap_or(0),
        1,
        "expected exactly one interference_delta row"
    );
}

/// Verify composed extension loading works and doesn't crash.
#[test]
fn composed_extension_load_succeeds() {
    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        specs.push(JsExtensionLoadSpec::from_entry_path(&entry).expect("spec"));
    }

    assert!(
        specs.len() >= 2,
        "need at least 2 extensions for composed test"
    );

    let harness = common::TestHarness::new("composed_load");
    let manager = setup_composed_manager(&harness, &specs);

    // Verify all extensions are loaded — dispatch event should work.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "composed test"})),
                    NORMAL_TIMEOUT_MS,
                )
                .await
        }
    });
    assert!(
        result.is_ok(),
        "composed dispatch should succeed: {result:?}"
    );

    // Collect diagnostics on the composed manager.
    let diag = collect_diagnostics("composed", &manager);
    eprintln!(
        "[composed] diagnostics: risk={} telemetry={} snap={}",
        diag.risk_ledger_entries, diag.hostcall_telemetry_entries, diag.snapshot_version
    );

    shutdown(&manager);
}

/// Per-extension tool-call isolation: measure each extension's tool latency
/// both in isolation and under composed load (all extensions loaded).
/// Identifies which extensions suffer most from interference.
#[test]
#[allow(clippy::too_many_lines)]
fn per_extension_tool_isolation() {
    let iterations = 20;
    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    let mut ext_names: Vec<&str> = Vec::new();
    let mut records: Vec<Value> = Vec::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("spec");
        specs.push(spec);
        ext_names.push(ext_name);
    }

    if specs.len() < 2 {
        eprintln!("[skip] need >=2 extensions for isolation test");
        return;
    }

    // Phase A: Isolated tool-call latency per extension.
    let mut isolated_latencies: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (i, ext_name) in ext_names.iter().enumerate() {
        let harness = common::TestHarness::new(format!("isolation_single_{ext_name}"));
        let manager = setup_manager_with_extension(&harness, &specs[i]);

        let Some(runtime) = manager.js_runtime() else {
            continue;
        };
        let ctx = json!({ "hasUI": false, "cwd": project_root().display().to_string() });

        let mut samples = Vec::with_capacity(iterations);
        for j in 0..iterations {
            let start = Instant::now();
            let _ = futures::executor::block_on(runtime.execute_tool(
                ext_name.to_string(),
                format!("iso-{j}"),
                json!({"name": "test"}),
                std::sync::Arc::new(ctx.clone()),
                NORMAL_TIMEOUT_MS,
            ));
            samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
        samples.sort_by(f64::total_cmp);

        let rec = samples_to_record(ext_name, &samples);
        eprintln!(
            "[isolation] {ext_name} single: p50={:.1}us p95={:.1}us",
            rec.p50_us, rec.p95_us
        );
        records.push(json!({
            "schema": "pi.ext.isolation.v1",
            "phase": "isolated_tool_call",
            "extension": ext_name,
            "extension_count": 1,
            "iterations": iterations,
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
        }));
        isolated_latencies.insert(ext_name.to_string(), samples);

        shutdown(&manager);
    }

    // Phase B: Composed tool-call latency per extension.
    let harness = common::TestHarness::new("isolation_composed");
    let composed_manager = setup_composed_manager(&harness, &specs);
    let Some(composed_runtime) = composed_manager.js_runtime() else {
        eprintln!("[skip] no JS runtime for composed manager");
        shutdown(&composed_manager);
        return;
    };
    let ctx = json!({ "hasUI": false, "cwd": project_root().display().to_string() });

    for ext_name in &ext_names {
        let mut samples = Vec::with_capacity(iterations);
        for j in 0..iterations {
            let start = Instant::now();
            let _ = futures::executor::block_on(composed_runtime.execute_tool(
                ext_name.to_string(),
                format!("comp-{j}"),
                json!({"name": "test"}),
                std::sync::Arc::new(ctx.clone()),
                NORMAL_TIMEOUT_MS,
            ));
            samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
        samples.sort_by(f64::total_cmp);

        let rec = samples_to_record(ext_name, &samples);
        eprintln!(
            "[isolation] {ext_name} composed: p50={:.1}us p95={:.1}us",
            rec.p50_us, rec.p95_us
        );

        // Compute interference burden for this extension.
        let isolated = isolated_latencies.get(*ext_name);
        let isolated_p50 = isolated.map_or(rec.p50_us, |s| percentile_us(s, 50));
        let burden_pct = if isolated_p50 > 0.0 {
            ((rec.p50_us - isolated_p50) / isolated_p50) * 100.0
        } else {
            0.0
        };

        records.push(json!({
            "schema": "pi.ext.isolation.v1",
            "phase": "composed_tool_call",
            "extension": ext_name,
            "extension_count": specs.len(),
            "iterations": iterations,
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
            "interference_burden_pct": burden_pct,
        }));
    }

    // Write isolation JSONL
    let output_path = perf_output_path("e2e_isolation.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} isolation records written to {}",
        records.len(),
        output_path.display()
    );

    shutdown(&composed_manager);
}

/// Incremental interference scaling: measure latency with 1, 2, then 3 extensions.
/// Shows how interference grows with extension count.
#[test]
fn interference_scaling_by_count() {
    let iterations = 20;
    let mut all_specs: Vec<JsExtensionLoadSpec> = Vec::new();
    let mut all_names: Vec<&str> = Vec::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        all_specs.push(JsExtensionLoadSpec::from_entry_path(&entry).expect("spec"));
        all_names.push(ext_name);
    }

    if all_specs.len() < 3 {
        eprintln!("[skip] need >=3 extensions for scaling test");
        return;
    }

    let mut records: Vec<Value> = Vec::new();
    let mut prev_p50 = 0.0_f64;

    for count in 1..=all_specs.len() {
        let subset = &all_specs[..count];
        let label = all_names[..count].join("+");
        let harness = common::TestHarness::new(format!("scaling_{count}ext"));
        let manager = setup_composed_manager(&harness, subset);

        let samples = measure_event_latencies(&manager, iterations);
        let rec = samples_to_record(&label, &samples);

        let scaling_ratio = if prev_p50 > 0.0 {
            rec.p50_us / prev_p50
        } else {
            1.0
        };

        eprintln!(
            "[scaling] {count}ext ({label}): p50={:.1}us p95={:.1}us scale={scaling_ratio:.2}x",
            rec.p50_us, rec.p95_us
        );

        records.push(json!({
            "schema": "pi.ext.scaling.v1",
            "extension_count": count,
            "extensions": &all_names[..count],
            "iterations": iterations,
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
            "scaling_ratio_vs_previous": scaling_ratio,
        }));

        prev_p50 = rec.p50_us;
        shutdown(&manager);
    }

    // Write scaling JSONL
    let output_path = perf_output_path("e2e_scaling.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} scaling records written to {}",
        records.len(),
        output_path.display()
    );

    // Verify scaling is not catastrophically super-linear. The relative ratio
    // can spike when the previous p50 is a sub-millisecond sample, so require
    // a budget breach as well before failing the diagnostic gate.
    for record in &records {
        let ratio = record
            .get("scaling_ratio_vs_previous")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        let p95_us = record.get("p95_us").and_then(Value::as_f64).unwrap_or(0.0);
        let count = record
            .get("extension_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        assert!(
            ratio < 100.0 || p95_us < EVENT_DISPATCH_BUDGET_US,
            "scaling ratio {ratio:.2}x at {count} extensions exceeds 100x and p95 {p95_us:.1}us exceeds {EVENT_DISPATCH_BUDGET_US:.0}us budget (super-linear interference)"
        );
    }
}

// ─── Per-Stage Decomposition & Contention Attribution ────────────────────────

/// Measure per-phase latency (cold_load, event_dispatch, tool_call) for a
/// single extension, returning sorted sample vectors keyed by phase name.
fn measure_phase_latencies(
    ext_name: &str,
    spec: &JsExtensionLoadSpec,
    harness: &common::TestHarness,
    iterations: usize,
) -> BTreeMap<String, Vec<f64>> {
    let mut phase_samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();

    // Cold load samples
    let mut cold_samples = Vec::with_capacity(3);
    for _ in 0..3 {
        let start = Instant::now();
        let cwd = harness.temp_dir().to_path_buf();
        let mgr = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
        let js_cfg = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            ..Default::default()
        };
        let ok = (|| -> Result<(), String> {
            let rt = common::run_async({
                let mgr = mgr.clone();
                let tools = Arc::clone(&tools);
                async move {
                    JsExtensionRuntimeHandle::start(js_cfg, tools, mgr)
                        .await
                        .map_err(|e| e.to_string())
                }
            })?;
            mgr.set_js_runtime(rt);
            common::run_async({
                let mgr = mgr.clone();
                let s = spec.clone();
                async move {
                    mgr.load_js_extensions(vec![s])
                        .await
                        .map_err(|e| e.to_string())
                }
            })?;
            Ok(())
        })();
        cold_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        shutdown(&mgr);
        if ok.is_err() {
            break;
        }
    }
    cold_samples.sort_by(f64::total_cmp);
    phase_samples.insert("cold_load".to_string(), cold_samples);

    // Event dispatch + tool call on a persistent manager
    let manager = setup_manager_with_extension(harness, spec);

    let event_samples = measure_event_latencies(&manager, iterations);
    phase_samples.insert("event_dispatch".to_string(), event_samples);

    // Tool call samples
    if let Some(runtime) = manager.js_runtime() {
        let ctx = json!({ "hasUI": false, "cwd": project_root().display().to_string() });
        let tool_name = ext_name.to_string();
        let mut tool_samples = Vec::with_capacity(iterations);
        for i in 0..iterations {
            let start = Instant::now();
            let _ = futures::executor::block_on(runtime.execute_tool(
                tool_name.clone(),
                format!("decomp-{i}"),
                json!({"name": "test"}),
                std::sync::Arc::new(ctx.clone()),
                NORMAL_TIMEOUT_MS,
            ));
            tool_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
        tool_samples.sort_by(f64::total_cmp);
        phase_samples.insert("tool_call".to_string(), tool_samples);
    }

    shutdown(&manager);
    phase_samples
}

/// Measure per-phase latency on a composed manager (all extensions loaded).
fn measure_composed_phase_latencies(
    harness: &common::TestHarness,
    specs: &[JsExtensionLoadSpec],
    iterations: usize,
) -> BTreeMap<String, Vec<f64>> {
    let mut phase_samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();

    // Cold load for composed
    let mut cold_samples = Vec::with_capacity(3);
    for _ in 0..3 {
        let start = Instant::now();
        let cwd = harness.temp_dir().to_path_buf();
        let mgr = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
        let js_cfg = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            ..Default::default()
        };
        let ok = (|| -> Result<(), String> {
            let rt = common::run_async({
                let mgr = mgr.clone();
                let tools = Arc::clone(&tools);
                async move {
                    JsExtensionRuntimeHandle::start(js_cfg, tools, mgr)
                        .await
                        .map_err(|e| e.to_string())
                }
            })?;
            mgr.set_js_runtime(rt);
            common::run_async({
                let mgr = mgr.clone();
                let s = specs.to_vec();
                async move { mgr.load_js_extensions(s).await.map_err(|e| e.to_string()) }
            })?;
            Ok(())
        })();
        cold_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        shutdown(&mgr);
        if ok.is_err() {
            break;
        }
    }
    cold_samples.sort_by(f64::total_cmp);
    phase_samples.insert("cold_load".to_string(), cold_samples);

    // Event dispatch and tool calls on persistent composed manager
    let composed = setup_composed_manager(harness, specs);
    let event_samples = measure_event_latencies(&composed, iterations);
    phase_samples.insert("event_dispatch".to_string(), event_samples);

    // Tool call on first extension that has a tool
    if let Some(runtime) = composed.js_runtime() {
        let ctx = json!({ "hasUI": false, "cwd": project_root().display().to_string() });
        let tool_name = "hello".to_string();
        let mut tool_samples = Vec::with_capacity(iterations);
        for i in 0..iterations {
            let start = Instant::now();
            let _ = futures::executor::block_on(runtime.execute_tool(
                tool_name.clone(),
                format!("composed-decomp-{i}"),
                json!({"name": "test"}),
                std::sync::Arc::new(ctx.clone()),
                NORMAL_TIMEOUT_MS,
            ));
            tool_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
        tool_samples.sort_by(f64::total_cmp);
        phase_samples.insert("tool_call".to_string(), tool_samples);
    }

    shutdown(&composed);
    phase_samples
}

/// Per-stage decomposition: compare single-extension vs composed latency broken
/// down by phase (cold_load, event_dispatch, tool_call). Identifies which phase
/// suffers the most interference under composition.
#[test]
#[allow(clippy::too_many_lines)]
fn stage_decomposition_composed_vs_isolated() {
    let iterations = 20;
    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    let mut records: Vec<Value> = Vec::new();
    let mut single_phase_data: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("spec");
        specs.push(spec.clone());

        let harness = common::TestHarness::new(format!("decomp_single_{ext_name}"));
        let phase_data = measure_phase_latencies(ext_name, &spec, &harness, iterations);

        for (phase, samples) in &phase_data {
            let rec = samples_to_record(ext_name, samples);
            eprintln!(
                "[decomp] {ext_name}/{phase} single: p50={:.1}us p95={:.1}us",
                rec.p50_us, rec.p95_us
            );
            records.push(json!({
                "schema": "pi.ext.decomposition.v1",
                "mode": "single",
                "extension": ext_name,
                "phase": phase,
                "p50_us": rec.p50_us,
                "p95_us": rec.p95_us,
                "p99_us": rec.p99_us,
                "mean_us": rec.mean_us,
                "sample_count": rec.count,
            }));
        }
        single_phase_data.insert(ext_name.to_string(), phase_data);
    }

    if specs.len() < 2 {
        eprintln!("[skip] need >=2 extensions for decomposition test");
        return;
    }

    let harness = common::TestHarness::new("decomp_composed");
    let composed_phases = measure_composed_phase_latencies(&harness, &specs, iterations);

    for (phase, samples) in &composed_phases {
        let rec = samples_to_record("composed", samples);
        eprintln!(
            "[decomp] composed/{phase}: p50={:.1}us p95={:.1}us",
            rec.p50_us, rec.p95_us
        );
        records.push(json!({
            "schema": "pi.ext.decomposition.v1",
            "mode": "composed",
            "extension": "composed",
            "extension_count": specs.len(),
            "phase": phase,
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
            "sample_count": rec.count,
        }));
    }

    let phases = ["cold_load", "event_dispatch", "tool_call"];
    let mut phase_ratios: Vec<Value> = Vec::new();
    let mut worst_phase = String::new();
    let mut worst_ratio = 0.0_f64;

    for phase in &phases {
        let singles: Vec<f64> = single_phase_data
            .values()
            .filter_map(|ext_phases| {
                ext_phases
                    .get(*phase)
                    .filter(|s| !s.is_empty())
                    .map(|s| percentile_us(s, 50))
            })
            .collect();

        let avg_single_p50 = if singles.is_empty() {
            continue;
        } else {
            singles.iter().sum::<f64>() / singles.len() as f64
        };

        let composed_p50 = composed_phases
            .get(*phase)
            .filter(|s| !s.is_empty())
            .map_or(0.0, |s| percentile_us(s, 50));

        let ratio = if avg_single_p50 > 0.0 {
            composed_p50 / avg_single_p50
        } else {
            1.0
        };

        let avg_single_p95: f64 = single_phase_data
            .values()
            .filter_map(|ext_phases| {
                ext_phases
                    .get(*phase)
                    .filter(|s| !s.is_empty())
                    .map(|s| percentile_us(s, 95))
            })
            .sum::<f64>()
            / singles.len().max(1) as f64;

        let composed_p95 = composed_phases
            .get(*phase)
            .filter(|s| !s.is_empty())
            .map_or(0.0, |s| percentile_us(s, 95));

        let tail_ratio = if avg_single_p95 > 0.0 {
            composed_p95 / avg_single_p95
        } else {
            1.0
        };

        eprintln!("[decomp] {phase}: p50_ratio={ratio:.2}x tail_ratio={tail_ratio:.2}x");

        if ratio > worst_ratio {
            worst_ratio = ratio;
            worst_phase = phase.to_string();
        }

        phase_ratios.push(json!({
            "schema": "pi.ext.decomposition.v1",
            "mode": "ratio",
            "phase": phase,
            "avg_single_p50_us": avg_single_p50,
            "composed_p50_us": composed_p50,
            "p50_interference_ratio": ratio,
            "avg_single_p95_us": avg_single_p95,
            "composed_p95_us": composed_p95,
            "p95_interference_ratio": tail_ratio,
        }));
    }
    records.extend(phase_ratios);

    if !worst_phase.is_empty() {
        records.push(json!({
            "schema": "pi.ext.decomposition.v1",
            "mode": "summary",
            "worst_interference_phase": worst_phase,
            "worst_p50_ratio": worst_ratio,
        }));
        eprintln!("[decomp] worst phase: {worst_phase} at {worst_ratio:.2}x");
    }

    let output_path = perf_output_path("e2e_decomposition.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} decomposition records written to {}",
        records.len(),
        output_path.display()
    );

    for record in &records {
        if record.get("mode").and_then(Value::as_str) == Some("ratio") {
            let ratio = record
                .get("p50_interference_ratio")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let phase = record.get("phase").and_then(Value::as_str).unwrap_or("?");
            assert!(
                ratio < 15.0,
                "{phase} p50 interference ratio {ratio:.2}x exceeds 15x"
            );
        }
    }
}

/// Pairwise extension contention matrix: measure interference for each pair
/// of extensions (hello+pirate, hello+diff, pirate+diff) to identify which
/// pair causes the most contention.
#[test]
#[allow(clippy::too_many_lines)]
fn pairwise_extension_contention_matrix() {
    let iterations = 20;
    let mut specs: Vec<(String, JsExtensionLoadSpec)> = Vec::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("spec");
        specs.push((ext_name.to_string(), spec));
    }

    if specs.len() < 2 {
        eprintln!("[skip] need >=2 extensions for contention matrix");
        return;
    }

    let mut records: Vec<Value> = Vec::new();

    // Phase A: Single-extension baselines.
    let mut baselines: BTreeMap<String, LatencySamples> = BTreeMap::new();
    for (name, spec) in &specs {
        let harness = common::TestHarness::new(format!("contention_single_{name}"));
        let manager = setup_manager_with_extension(&harness, spec);
        let samples = measure_event_latencies(&manager, iterations);
        let rec = samples_to_record(name, &samples);
        eprintln!(
            "[contention] {name} single: p50={:.1}us p95={:.1}us",
            rec.p50_us, rec.p95_us
        );
        baselines.insert(name.clone(), rec.clone());
        records.push(json!({
            "schema": "pi.ext.contention.v1",
            "phase": "baseline",
            "extensions": [name],
            "p50_us": rec.p50_us,
            "p95_us": rec.p95_us,
            "p99_us": rec.p99_us,
            "mean_us": rec.mean_us,
        }));
        shutdown(&manager);
    }

    // Phase B: Pairwise measurements.
    let mut pair_ratios: Vec<(String, String, f64, f64)> = Vec::new();

    for i in 0..specs.len() {
        for j in (i + 1)..specs.len() {
            let (name_a, spec_a) = &specs[i];
            let (name_b, spec_b) = &specs[j];
            let pair_label = format!("{name_a}+{name_b}");

            let harness = common::TestHarness::new(format!("contention_pair_{name_a}_{name_b}"));
            let pair_specs = vec![spec_a.clone(), spec_b.clone()];
            let manager = setup_composed_manager(&harness, &pair_specs);
            let samples = measure_event_latencies(&manager, iterations);
            let rec = samples_to_record(&pair_label, &samples);

            let bl_first_p50 = baselines.get(name_a).map_or(1.0, |b| b.p50_us);
            let bl_second_p50 = baselines.get(name_b).map_or(1.0, |b| b.p50_us);
            let avg_baseline = f64::midpoint(bl_first_p50, bl_second_p50);
            let p50_ratio = if avg_baseline > 0.0 {
                rec.p50_us / avg_baseline
            } else {
                1.0
            };

            let bl_first_p95 = baselines.get(name_a).map_or(1.0, |b| b.p95_us);
            let bl_second_p95 = baselines.get(name_b).map_or(1.0, |b| b.p95_us);
            let avg_baseline_p95 = f64::midpoint(bl_first_p95, bl_second_p95);
            let p95_ratio = if avg_baseline_p95 > 0.0 {
                rec.p95_us / avg_baseline_p95
            } else {
                1.0
            };

            eprintln!(
                "[contention] {pair_label}: p50={:.1}us p95={:.1}us ratio={p50_ratio:.2}x tail={p95_ratio:.2}x",
                rec.p50_us, rec.p95_us
            );

            pair_ratios.push((name_a.clone(), name_b.clone(), p50_ratio, p95_ratio));
            records.push(json!({
                "schema": "pi.ext.contention.v1",
                "phase": "pair",
                "extensions": [name_a, name_b],
                "pair": pair_label,
                "p50_us": rec.p50_us,
                "p95_us": rec.p95_us,
                "p99_us": rec.p99_us,
                "mean_us": rec.mean_us,
                "avg_baseline_p50_us": avg_baseline,
                "avg_baseline_p95_us": avg_baseline_p95,
                "p50_interference_ratio": p50_ratio,
                "p95_interference_ratio": p95_ratio,
            }));

            shutdown(&manager);
        }
    }

    let hottest = pair_ratios
        .iter()
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    if let Some((ext_a, ext_b, ratio, tail)) = hottest {
        records.push(json!({
            "schema": "pi.ext.contention.v1",
            "phase": "summary",
            "hottest_pair": format!("{ext_a}+{ext_b}"),
            "hottest_p50_ratio": ratio,
            "hottest_p95_ratio": tail,
            "total_pairs_measured": pair_ratios.len(),
        }));
        eprintln!("[contention] hottest pair: {ext_a}+{ext_b} at {ratio:.2}x p50, {tail:.2}x p95");
    }

    let output_path = perf_output_path("e2e_contention.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[output] {} contention records written to {}",
        records.len(),
        output_path.display()
    );

    for (ext_a, ext_b, ratio, _) in &pair_ratios {
        assert!(
            *ratio < 12.0,
            "{ext_a}+{ext_b} p50 ratio {ratio:.2}x exceeds 12x (severe pairwise contention)"
        );
    }
}

/// Structured regression gate: runs composed measurement, compares against
/// per-phase and per-pair thresholds, and emits a structured pass/fail report
/// with specific regression indicators for each dimension.
#[test]
#[allow(clippy::too_many_lines)]
fn regression_gate_structured_report() {
    let iterations = 20;
    let phase_p50_threshold = 12.0_f64;
    let phase_p95_threshold = 18.0_f64;
    let tail_amplification_threshold = 3.0_f64;
    let overall_p50_threshold = 10.0_f64;

    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    let mut records: Vec<Value> = Vec::new();
    let mut gate_failures: Vec<String> = Vec::new();

    for ext_name in LIFECYCLE_EXTENSIONS {
        let entry = artifact_entry(ext_name);
        if !entry.exists() {
            continue;
        }
        specs.push(JsExtensionLoadSpec::from_entry_path(&entry).expect("spec"));
    }

    if specs.len() < 2 {
        eprintln!("[skip] need >=2 extensions for regression gate");
        return;
    }

    let mut baselines: BTreeMap<String, LatencySamples> = BTreeMap::new();
    for (i, ext_name) in LIFECYCLE_EXTENSIONS.iter().enumerate() {
        if i >= specs.len() {
            break;
        }
        let harness = common::TestHarness::new(format!("reggate_single_{ext_name}"));
        let manager = setup_manager_with_extension(&harness, &specs[i]);
        let samples = measure_event_latencies(&manager, iterations);
        baselines.insert(ext_name.to_string(), samples_to_record(ext_name, &samples));
        shutdown(&manager);
    }

    let harness = common::TestHarness::new("reggate_composed");
    let composed = setup_composed_manager(&harness, &specs);
    let composed_samples = measure_event_latencies(&composed, iterations);
    let composed_rec = samples_to_record("composed", &composed_samples);

    let single_phase_data: BTreeMap<String, BTreeMap<String, Vec<f64>>> = LIFECYCLE_EXTENSIONS
        .iter()
        .enumerate()
        .filter_map(|(i, ext_name)| {
            if i >= specs.len() {
                return None;
            }
            let h = common::TestHarness::new(format!("reggate_phase_{ext_name}"));
            let data = measure_phase_latencies(ext_name, &specs[i], &h, iterations);
            Some((ext_name.to_string(), data))
        })
        .collect();

    let composed_harness = common::TestHarness::new("reggate_composed_phases");
    let composed_phases = measure_composed_phase_latencies(&composed_harness, &specs, iterations);

    // Gate 1: Overall composed vs single p50 ratio.
    let baseline_count = baselines.len() as f64;
    let avg_baseline_p50 = baselines.values().map(|b| b.p50_us).sum::<f64>() / baseline_count;
    let avg_baseline_p95 = baselines.values().map(|b| b.p95_us).sum::<f64>() / baseline_count;

    let overall_p50_ratio = if avg_baseline_p50 > 0.0 {
        composed_rec.p50_us / avg_baseline_p50
    } else {
        1.0
    };

    let gate1_pass = overall_p50_ratio < overall_p50_threshold;
    if !gate1_pass {
        gate_failures.push(format!(
            "overall_p50: {overall_p50_ratio:.2}x >= {overall_p50_threshold}x"
        ));
    }
    eprintln!(
        "[reggate] overall p50 ratio: {overall_p50_ratio:.2}x (threshold: {overall_p50_threshold}x) {}",
        if gate1_pass { "PASS" } else { "FAIL" }
    );

    // Gate 2: Tail amplification.
    let single_tail = if avg_baseline_p50 > 0.0 {
        avg_baseline_p95 / avg_baseline_p50
    } else {
        1.0
    };
    let composed_tail = if composed_rec.p50_us > 0.0 {
        composed_rec.p95_us / composed_rec.p50_us
    } else {
        1.0
    };
    let tail_amp = if single_tail > 0.0 {
        composed_tail / single_tail
    } else {
        1.0
    };

    let debug_tail_absolute_pass =
        cfg!(debug_assertions) && composed_rec.p95_us < EVENT_DISPATCH_BUDGET_US;
    let gate2_pass = tail_amp < tail_amplification_threshold || debug_tail_absolute_pass;
    if !gate2_pass {
        gate_failures.push(format!(
            "tail_amplification: {tail_amp:.2}x >= {tail_amplification_threshold}x"
        ));
    }
    eprintln!(
        "[reggate] tail amplification: {tail_amp:.2}x (threshold: {tail_amplification_threshold}x, composed_p95={:.1}us, debug_budget={:.0}us) {}",
        composed_rec.p95_us,
        EVENT_DISPATCH_BUDGET_US,
        if gate2_pass { "PASS" } else { "FAIL" }
    );

    // Gate 3: Per-phase regression check.
    let mut phase_gates: Vec<Value> = Vec::new();
    for phase in &["cold_load", "event_dispatch", "tool_call"] {
        let singles: Vec<f64> = single_phase_data
            .values()
            .filter_map(|ext| {
                ext.get(*phase)
                    .filter(|s| !s.is_empty())
                    .map(|s| percentile_us(s, 50))
            })
            .collect();
        let avg_single_p50 = if singles.is_empty() {
            continue;
        } else {
            singles.iter().sum::<f64>() / singles.len() as f64
        };

        let composed_p50 = composed_phases
            .get(*phase)
            .filter(|s| !s.is_empty())
            .map_or(0.0, |s| percentile_us(s, 50));
        let ratio = if avg_single_p50 > 0.0 {
            composed_p50 / avg_single_p50
        } else {
            1.0
        };

        let singles_p95: Vec<f64> = single_phase_data
            .values()
            .filter_map(|ext| {
                ext.get(*phase)
                    .filter(|s| !s.is_empty())
                    .map(|s| percentile_us(s, 95))
            })
            .collect();
        let avg_single_p95 = if singles_p95.is_empty() {
            0.0
        } else {
            singles_p95.iter().sum::<f64>() / singles_p95.len() as f64
        };

        let composed_p95 = composed_phases
            .get(*phase)
            .filter(|s| !s.is_empty())
            .map_or(0.0, |s| percentile_us(s, 95));
        let p95_ratio = if avg_single_p95 > 0.0 {
            composed_p95 / avg_single_p95
        } else {
            1.0
        };

        let p50_pass = ratio < phase_p50_threshold;
        let p95_pass = p95_ratio < phase_p95_threshold;
        if !p50_pass {
            gate_failures.push(format!(
                "phase_{phase}_p50: {ratio:.2}x >= {phase_p50_threshold}x"
            ));
        }
        if !p95_pass {
            gate_failures.push(format!(
                "phase_{phase}_p95: {p95_ratio:.2}x >= {phase_p95_threshold}x"
            ));
        }
        eprintln!(
            "[reggate] {phase}: p50={ratio:.2}x p95={p95_ratio:.2}x {}",
            if p50_pass && p95_pass { "PASS" } else { "FAIL" }
        );

        phase_gates.push(json!({
            "phase": phase,
            "avg_single_p50_us": avg_single_p50,
            "composed_p50_us": composed_p50,
            "p50_ratio": ratio,
            "p50_pass": p50_pass,
            "avg_single_p95_us": avg_single_p95,
            "composed_p95_us": composed_p95,
            "p95_ratio": p95_ratio,
            "p95_pass": p95_pass,
        }));
    }

    shutdown(&composed);

    let all_pass = gate_failures.is_empty();
    let report = json!({
        "schema": "pi.ext.regression_gate.v1",
        "verdict": if all_pass { "pass" } else { "fail" },
        "extension_count": specs.len(),
        "iterations": iterations,
        "gates": {
            "overall_p50": {
                "ratio": overall_p50_ratio,
                "threshold": overall_p50_threshold,
                "pass": gate1_pass,
            },
            "tail_amplification": {
                "value": tail_amp,
                "threshold": tail_amplification_threshold,
                "composed_p95_us": composed_rec.p95_us,
                "debug_absolute_p95_budget_us": EVENT_DISPATCH_BUDGET_US,
                "debug_absolute_pass": debug_tail_absolute_pass,
                "pass": gate2_pass,
            },
            "per_phase": phase_gates,
        },
        "failures": gate_failures,
        "baselines": baselines.iter().map(|(k, v)| {
            json!({
                "extension": k,
                "p50_us": v.p50_us,
                "p95_us": v.p95_us,
            })
        }).collect::<Vec<_>>(),
        "composed": {
            "p50_us": composed_rec.p50_us,
            "p95_us": composed_rec.p95_us,
            "p99_us": composed_rec.p99_us,
        },
    });

    records.push(report);

    let output_path = perf_output_path("e2e_regression_gate.jsonl");
    write_jsonl(&records, &output_path);
    eprintln!(
        "\n[reggate] verdict: {} ({} gate failures) -> {}",
        if all_pass { "PASS" } else { "FAIL" },
        gate_failures.len(),
        output_path.display()
    );

    assert!(all_pass, "regression gate FAILED: {gate_failures:?}");
}
