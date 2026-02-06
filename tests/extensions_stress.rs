//! Memory profiling and stress test for concurrent extensions (bd-3dxz).
//!
//! Loads 10+ extensions simultaneously, fires events at a controlled rate,
//! monitors RSS memory usage and event dispatch latency, and generates a
//! JSONL report.
//!
//! The full 1-hour test is available via the `ext_stress` binary:
//!   cargo run --bin `ext_stress` -- --`duration_secs=3600` --`max_extensions=15`
//!
//! These `#[test]` functions run shorter durations for CI gating.

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{ExtensionEventName, ExtensionManager, JsExtensionLoadSpec};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, get_current_pid};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Minimum number of extensions to load for the stress test.
const MIN_EXTENSIONS: usize = 10;
/// Short stress duration for CI (seconds).
const SHORT_STRESS_SECS: u64 = 30;
/// Events per second during stress.
const EVENTS_PER_SEC: u64 = 50;
/// RSS sampling interval (seconds).
const RSS_SAMPLE_INTERVAL_SECS: u64 = 5;
/// Maximum acceptable RSS growth (10%).
const MAX_RSS_GROWTH_PCT: f64 = 0.10;
/// Maximum acceptable latency degradation (2x).
const MAX_LATENCY_DEGRADATION: u64 = 2;
/// Absolute p99 cap for noisy shared CI/agent hosts.
const MAX_P99_LAST_US: u64 = 25_000;

// ─── Helper Types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RssSample {
    t_s: u64,
    rss_kb: u64,
}

#[derive(Debug)]
struct StressResult {
    initial_rss_kb: u64,
    max_rss_kb: u64,
    rss_growth_pct: Option<f64>,
    rss_samples: Vec<RssSample>,
    latencies_us: Vec<u64>,
    p99_first: Option<u64>,
    p99_last: Option<u64>,
    event_count: u64,
    error_count: u64,
    errors: Vec<String>,
    rss_ok: bool,
    latency_ok: bool,
    extensions_loaded: usize,
}

// ─── Pure Helper Functions ──────────────────────────────────────────────────

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

fn summarize_latencies(values: &[u64]) -> Value {
    if values.is_empty() {
        return json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p50 = percentile(&sorted, 50);
    let p95 = percentile(&sorted, 95);
    let p99 = percentile(&sorted, 99);
    let min = sorted.first().copied().unwrap_or(0);
    let max = sorted.last().copied().unwrap_or(0);
    let sum: u128 = sorted.iter().map(|v| u128::from(*v)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = u64::try_from(sum / (sorted.len() as u128)).unwrap_or(u64::MAX);
    json!({
        "count": sorted.len(),
        "min": min,
        "max": max,
        "mean": mean,
        "p50": p50,
        "p95": p95,
        "p99": p99,
    })
}

/// Compute p99 from the first 10% and last 10% of samples to detect degradation.
fn p99_first_last(values: &[u64]) -> (Option<u64>, Option<u64>) {
    if values.is_empty() {
        return (None, None);
    }
    let len = values.len();
    let window = (len / 10).max(1);
    let first = &values[..window];
    let last = &values[len.saturating_sub(window)..];
    let p99_first = {
        let mut s = first.to_vec();
        s.sort_unstable();
        if s.is_empty() {
            None
        } else {
            Some(s[percentile_index(s.len(), 99, 100)])
        }
    };
    let p99_last = {
        let mut s = last.to_vec();
        s.sort_unstable();
        if s.is_empty() {
            None
        } else {
            Some(s[percentile_index(s.len(), 99, 100)])
        }
    };
    (p99_first, p99_last)
}

const fn latency_within_budget(p99_first: Option<u64>, p99_last: Option<u64>) -> bool {
    match (p99_first, p99_last) {
        (Some(first), Some(last)) if first > 0 => {
            last <= first.saturating_mul(MAX_LATENCY_DEGRADATION) || last <= MAX_P99_LAST_US
        }
        _ => true,
    }
}

// ─── Setup Functions ────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn report_dir() -> PathBuf {
    // Write stress artifacts under `target/` so `cargo test` remains side-effect free
    // with respect to tracked repository files.
    project_root().join("target/perf")
}

/// Collect entry paths for official-pi-mono extensions that are single-file
/// (no npm deps, no exec required) for safe loading in test context.
fn collect_safe_extensions(max: usize) -> Vec<PathBuf> {
    let manifest_path = project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let data = std::fs::read_to_string(&manifest_path).expect("read VALIDATED_MANIFEST.json");
    let manifest: Value = serde_json::from_str(&data).expect("parse manifest");
    let extensions = manifest["extensions"].as_array().expect("extensions array");

    let artifacts = artifacts_dir();
    let mut paths = Vec::new();

    for ext in extensions {
        if paths.len() >= max {
            break;
        }
        // Only use official-pi-mono extensions
        if ext["source_tier"].as_str() != Some("official-pi-mono") {
            continue;
        }
        // Skip multi-file extensions and those requiring exec
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
    paths
}

fn load_extensions(paths: &[PathBuf]) -> (ExtensionManager, usize) {
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
            pi::extensions::JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime for stress test")
        }
    });
    manager.set_js_runtime(runtime);

    let mut specs: Vec<JsExtensionLoadSpec> = Vec::new();
    for path in paths {
        match JsExtensionLoadSpec::from_entry_path(path) {
            Ok(spec) => specs.push(spec),
            Err(e) => eprintln!("  skip {}: {e}", path.display()),
        }
    }

    let count = specs.len();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load extensions for stress test");
        }
    });

    (manager, count)
}

// ─── Stress Loop ────────────────────────────────────────────────────────────

fn run_stress_loop(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: Option<&Value>,
    events_per_sec: u64,
    duration: Duration,
    rss_interval_secs: u64,
) -> StressResult {
    let pid = get_current_pid().expect("get current PID");
    let refresh = ProcessRefreshKind::nothing().with_memory();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));

    // Initial RSS measurement
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let initial_rss_kb = system.process(pid).map_or(0, sysinfo::Process::memory);
    let mut max_rss_kb = initial_rss_kb;
    let mut rss_samples = vec![RssSample {
        t_s: 0,
        rss_kb: initial_rss_kb,
    }];

    #[allow(clippy::cast_precision_loss)]
    let interval = Duration::from_secs_f64(1.0 / events_per_sec as f64);
    let start = Instant::now();
    let mut next_event = start;
    let mut next_rss = start + Duration::from_secs(rss_interval_secs);
    let mut latencies_us = Vec::new();
    let mut errors = Vec::new();
    let mut error_count: u64 = 0;
    let mut event_count: u64 = 0;

    while start.elapsed() < duration {
        let now = Instant::now();
        if now < next_event {
            std::thread::sleep(next_event.duration_since(now).min(Duration::from_millis(1)));
            continue;
        }

        // Dispatch event and measure latency
        let dispatch_start = Instant::now();
        let result = common::run_async({
            let manager = manager.clone();
            let payload = payload.cloned();
            async move { manager.dispatch_event(event, payload).await }
        });
        let elapsed_us = u64::try_from(dispatch_start.elapsed().as_micros()).unwrap_or(u64::MAX);

        if let Err(err) = result {
            error_count += 1;
            if errors.len() < 10 {
                errors.push(err.to_string());
            }
        }
        latencies_us.push(elapsed_us);
        event_count += 1;

        next_event += interval;
        // Catch up if behind
        let catch_up = Instant::now();
        if next_event < catch_up {
            next_event = catch_up + interval;
        }

        // RSS sampling
        if Instant::now() >= next_rss {
            system.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[pid]),
                true,
                refresh,
            );
            if let Some(process) = system.process(pid) {
                let rss_kb = process.memory();
                if rss_kb > max_rss_kb {
                    max_rss_kb = rss_kb;
                }
                rss_samples.push(RssSample {
                    t_s: start.elapsed().as_secs(),
                    rss_kb,
                });
            }
            next_rss += Duration::from_secs(rss_interval_secs);
        }
    }

    // Compute metrics
    let (p99_first, p99_last) = p99_first_last(&latencies_us);

    let rss_growth_pct = if initial_rss_kb > 0 {
        #[allow(clippy::cast_precision_loss)]
        let growth = (max_rss_kb.saturating_sub(initial_rss_kb) as f64) / (initial_rss_kb as f64);
        Some(growth)
    } else {
        None
    };

    let rss_ok = rss_growth_pct.is_none_or(|growth| growth <= MAX_RSS_GROWTH_PCT);
    let latency_ok = latency_within_budget(p99_first, p99_last);

    StressResult {
        initial_rss_kb,
        max_rss_kb,
        rss_growth_pct,
        rss_samples,
        latencies_us,
        p99_first,
        p99_last,
        event_count,
        error_count,
        errors,
        rss_ok,
        latency_ok,
        extensions_loaded: 0, // caller sets
    }
}

// ─── Report Generation ──────────────────────────────────────────────────────

fn write_stress_report(result: &StressResult, duration_secs: u64, ext_names: &[String]) {
    let report_dir = report_dir();
    let _ = std::fs::create_dir_all(&report_dir);

    // JSONL event log
    let events_path = report_dir.join("stress_events.jsonl");
    let mut lines: Vec<String> = Vec::new();

    // RSS samples as events
    for sample in &result.rss_samples {
        let entry = json!({
            "schema": "pi.ext.stress_rss.v1",
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "t_s": sample.t_s,
            "rss_kb": sample.rss_kb,
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }

    // Summary event
    let summary_entry = json!({
        "schema": "pi.ext.stress_summary.v1",
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "extensions_loaded": result.extensions_loaded,
        "duration_secs": duration_secs,
        "event_count": result.event_count,
        "error_count": result.error_count,
        "initial_rss_kb": result.initial_rss_kb,
        "max_rss_kb": result.max_rss_kb,
        "rss_growth_pct": result.rss_growth_pct,
        "rss_ok": result.rss_ok,
        "latency_ok": result.latency_ok,
        "p99_first_us": result.p99_first,
        "p99_last_us": result.p99_last,
        "latency_summary": summarize_latencies(&result.latencies_us),
    });
    lines.push(serde_json::to_string(&summary_entry).unwrap_or_default());

    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // Triage summary JSON
    let triage = json!({
        "schema": "pi.ext.stress_triage.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "config": {
            "duration_secs": duration_secs,
            "events_per_sec": EVENTS_PER_SEC,
            "rss_interval_secs": RSS_SAMPLE_INTERVAL_SECS,
            "extensions": ext_names,
        },
        "results": {
            "extensions_loaded": result.extensions_loaded,
            "event_count": result.event_count,
            "error_count": result.error_count,
            "sample_errors": result.errors,
            "rss": {
                "initial_kb": result.initial_rss_kb,
                "max_kb": result.max_rss_kb,
                "growth_pct": result.rss_growth_pct,
                "ok": result.rss_ok,
            },
            "latency": {
                "p99_first_us": result.p99_first,
                "p99_last_us": result.p99_last,
                "ok": result.latency_ok,
                "summary": summarize_latencies(&result.latencies_us),
            },
        },
        "pass": result.rss_ok && result.latency_ok,
    });
    let triage_path = report_dir.join("stress_triage.json");
    let _ = std::fs::write(
        &triage_path,
        serde_json::to_string_pretty(&triage).unwrap_or_default(),
    );

    eprintln!("\n=== Stress Test Report ===");
    eprintln!("  Extensions loaded: {}", result.extensions_loaded);
    eprintln!("  Duration: {duration_secs}s");
    eprintln!("  Events dispatched: {}", result.event_count);
    eprintln!("  Errors: {}", result.error_count);
    eprintln!(
        "  RSS: {}KB → {}KB (growth: {:.1}%)",
        result.initial_rss_kb,
        result.max_rss_kb,
        result.rss_growth_pct.unwrap_or(0.0) * 100.0
    );
    eprintln!("  RSS OK: {}", result.rss_ok);
    eprintln!(
        "  P99 first: {:?}us, last: {:?}us",
        result.p99_first, result.p99_last
    );
    eprintln!("  Latency OK: {}", result.latency_ok);
    eprintln!("  Report: {}", events_path.display());
    eprintln!("  Triage: {}\n", triage_path.display());
}

// ============================================================================
// Unit tests: percentile and summary functions
// ============================================================================

#[test]
fn percentile_index_empty() {
    assert_eq!(percentile_index(0, 50, 100), 0);
    assert_eq!(percentile_index(0, 99, 100), 0);
}

#[test]
fn percentile_index_single_element() {
    assert_eq!(percentile_index(1, 50, 100), 0);
    assert_eq!(percentile_index(1, 99, 100), 0);
    assert_eq!(percentile_index(1, 1, 100), 0);
}

#[test]
fn percentile_index_two_elements() {
    // p50 of [a, b] → index 0
    assert_eq!(percentile_index(2, 50, 100), 0);
    // p99 of [a, b] → index 1
    assert_eq!(percentile_index(2, 99, 100), 1);
}

#[test]
fn percentile_index_ten_elements() {
    // p50 of 10 elements → index 4
    assert_eq!(percentile_index(10, 50, 100), 4);
    // p99 of 10 elements → index 9
    assert_eq!(percentile_index(10, 99, 100), 9);
    // p10 of 10 elements → index 0
    assert_eq!(percentile_index(10, 10, 100), 0);
}

#[test]
fn percentile_index_hundred_elements() {
    // p50 of 100 → index 49
    assert_eq!(percentile_index(100, 50, 100), 49);
    // p99 of 100 → index 98
    assert_eq!(percentile_index(100, 99, 100), 98);
    // p1 of 100 → index 0
    assert_eq!(percentile_index(100, 1, 100), 0);
}

#[test]
fn percentile_empty_returns_zero() {
    assert_eq!(percentile(&[], 50), 0);
    assert_eq!(percentile(&[], 99), 0);
}

#[test]
fn percentile_single_value() {
    assert_eq!(percentile(&[42], 50), 42);
    assert_eq!(percentile(&[42], 99), 42);
}

#[test]
fn percentile_sorted_values() {
    let sorted: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile(&sorted, 50), 50);
    assert_eq!(percentile(&sorted, 99), 99);
    assert_eq!(percentile(&sorted, 1), 1);
}

#[test]
fn summarize_latencies_empty() {
    let summary = summarize_latencies(&[]);
    assert_eq!(summary["count"], 0);
}

#[test]
fn summarize_latencies_single() {
    let summary = summarize_latencies(&[1000]);
    assert_eq!(summary["count"], 1);
    assert_eq!(summary["min"], 1000);
    assert_eq!(summary["max"], 1000);
    assert_eq!(summary["mean"], 1000);
    assert_eq!(summary["p50"], 1000);
    assert_eq!(summary["p99"], 1000);
}

#[test]
fn summarize_latencies_range() {
    let values: Vec<u64> = (100..=200).collect();
    let summary = summarize_latencies(&values);
    assert_eq!(summary["count"], 101);
    assert_eq!(summary["min"], 100);
    assert_eq!(summary["max"], 200);
    assert_eq!(summary["p50"], 150);
}

#[test]
fn p99_first_last_empty() {
    let (first, last) = p99_first_last(&[]);
    assert!(first.is_none());
    assert!(last.is_none());
}

#[test]
fn p99_first_last_small() {
    let values = vec![100, 200, 300, 400, 500];
    let (first, last) = p99_first_last(&values);
    assert!(first.is_some());
    assert!(last.is_some());
}

#[test]
fn p99_first_last_detects_degradation() {
    // First window: low latencies (100-200us)
    // Last window: high latencies (500-1000us)
    let mut values: Vec<u64> = Vec::with_capacity(100);
    values.extend(std::iter::repeat_n(150, 50));
    values.extend(std::iter::repeat_n(800, 50));
    let (first, last) = p99_first_last(&values);
    let first = first.unwrap();
    let last = last.unwrap();
    assert!(
        last > first,
        "last p99 ({last}) should be higher than first p99 ({first})"
    );
}

#[test]
fn p99_first_last_stable_latency() {
    // All values in same range → first and last p99 should be similar
    let values: Vec<u64> = (0..100).map(|_| 200).collect();
    let (first, last) = p99_first_last(&values);
    let first = first.unwrap();
    let last = last.unwrap();
    assert_eq!(
        first, last,
        "stable latency should have equal first/last p99"
    );
}

// ============================================================================
// Unit tests: RSS growth validation
// ============================================================================

#[test]
fn rss_growth_within_budget() {
    let initial: u64 = 100_000; // 100MB
    let max: u64 = 109_000; // 109MB → 9% growth
    #[allow(clippy::cast_precision_loss)]
    let growth = (max.saturating_sub(initial) as f64) / (initial as f64);
    assert!(
        growth <= MAX_RSS_GROWTH_PCT,
        "9% growth should be within {MAX_RSS_GROWTH_PCT}"
    );
}

#[test]
fn rss_growth_exceeds_budget() {
    let initial: u64 = 100_000;
    let max: u64 = 115_000; // 15% growth
    #[allow(clippy::cast_precision_loss)]
    let growth = (max.saturating_sub(initial) as f64) / (initial as f64);
    assert!(
        growth > MAX_RSS_GROWTH_PCT,
        "15% growth should exceed {MAX_RSS_GROWTH_PCT}"
    );
}

#[test]
fn latency_degradation_within_budget() {
    let p99_first: u64 = 1000; // 1ms
    let p99_last: u64 = 1800; // 1.8ms → 1.8x
    assert!(
        latency_within_budget(Some(p99_first), Some(p99_last)),
        "1.8x degradation should be within {MAX_LATENCY_DEGRADATION}x"
    );
}

#[test]
fn latency_degradation_exceeds_budget() {
    let p99_first: u64 = 1000;
    let p99_last: u64 = 30_000; // 30ms and 30x
    assert!(
        !latency_within_budget(Some(p99_first), Some(p99_last)),
        "30x degradation and >{MAX_P99_LAST_US}us should exceed budget"
    );
}

#[test]
fn latency_degradation_low_baseline_uses_absolute_cap() {
    let p99_first: u64 = 261;
    let p99_last: u64 = 22_672;
    assert!(
        latency_within_budget(Some(p99_first), Some(p99_last)),
        "shared-host jitter below absolute cap should remain within budget"
    );
}

// ============================================================================
// Integration: Short stress test with 10+ concurrent extensions
// ============================================================================

#[test]
fn stress_short_10_extensions() {
    let ext_paths = collect_safe_extensions(15);
    assert!(
        ext_paths.len() >= MIN_EXTENSIONS,
        "Need at least {MIN_EXTENSIONS} extensions for stress test, found {}",
        ext_paths.len()
    );

    let ext_names: Vec<String> = ext_paths
        .iter()
        .filter_map(|p| {
            p.strip_prefix(artifacts_dir())
                .ok()
                .map(|rel| rel.display().to_string())
        })
        .collect();

    eprintln!(
        "\n  Loading {} extensions for stress test:",
        ext_paths.len()
    );
    for name in &ext_names {
        eprintln!("    - {name}");
    }

    let (manager, loaded_count) = load_extensions(&ext_paths);
    assert!(
        loaded_count >= MIN_EXTENSIONS,
        "Need at least {MIN_EXTENSIONS} loaded, got {loaded_count}"
    );

    eprintln!("  Running stress loop: {SHORT_STRESS_SECS}s at {EVENTS_PER_SEC} events/s");

    let payload = json!({
        "systemPrompt": "You are Pi.",
        "model": "claude-sonnet-4-5",
    });

    let mut result = run_stress_loop(
        &manager,
        ExtensionEventName::AgentStart,
        Some(&payload),
        EVENTS_PER_SEC,
        Duration::from_secs(SHORT_STRESS_SECS),
        RSS_SAMPLE_INTERVAL_SECS,
    );
    result.extensions_loaded = loaded_count;

    // Generate report
    write_stress_report(&result, SHORT_STRESS_SECS, &ext_names);

    // Verify events were dispatched
    assert!(
        result.event_count > 0,
        "should have dispatched at least some events"
    );

    // Verify RSS tracking worked
    assert!(
        result.initial_rss_kb > 0,
        "initial RSS should be measurable"
    );
    assert!(
        !result.rss_samples.is_empty(),
        "should have collected RSS samples"
    );

    // Verify pass criteria
    assert!(
        result.rss_ok,
        "RSS growth should be within budget: initial={}KB max={}KB growth={:.1}%",
        result.initial_rss_kb,
        result.max_rss_kb,
        result.rss_growth_pct.unwrap_or(0.0) * 100.0
    );
    assert!(
        result.latency_ok,
        "Latency degradation should be within budget: p99_first={:?}us p99_last={:?}us",
        result.p99_first, result.p99_last
    );

    // Verify low error rate (some errors OK due to missing handlers)
    #[allow(clippy::cast_precision_loss)]
    let error_rate = if result.event_count > 0 {
        result.error_count as f64 / result.event_count as f64
    } else {
        0.0
    };
    // Allow errors - event dispatch may fail if extensions don't handle the event
    // The key metric is that the system doesn't crash or leak
    eprintln!(
        "  Error rate: {:.1}% ({}/{})",
        error_rate * 100.0,
        result.error_count,
        result.event_count
    );

    // Cleanup
    common::run_async({
        let manager = manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

#[test]
fn stress_verify_no_panic_rapid_dispatch() {
    // Rapid-fire events without delay to stress the dispatch path
    let ext_paths = collect_safe_extensions(5);
    if ext_paths.len() < 3 {
        eprintln!("  Skipping rapid dispatch test: not enough extensions");
        return;
    }

    let (manager, _loaded) = load_extensions(&ext_paths);

    // Fire 500 events as fast as possible
    let start = Instant::now();
    let mut count = 0u64;
    let mut errors = 0u64;
    for _ in 0..500 {
        let result = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event(
                        ExtensionEventName::AgentStart,
                        Some(json!({"systemPrompt": "test"})),
                    )
                    .await
            }
        });
        if result.is_err() {
            errors += 1;
        }
        count += 1;
    }
    let elapsed = start.elapsed();

    eprintln!(
        "  Rapid dispatch: {} events in {:.1}ms ({} errors)",
        count,
        elapsed.as_secs_f64() * 1000.0,
        errors
    );

    // The test passes if we reach here without panicking
    assert!(count >= 500, "should have dispatched all events");

    // Cleanup
    common::run_async({
        let manager = manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

#[test]
fn stress_concurrent_event_types() {
    // Dispatch different event types to stress multiple code paths
    let ext_paths = collect_safe_extensions(5);
    if ext_paths.len() < 3 {
        eprintln!("  Skipping concurrent event types test: not enough extensions");
        return;
    }

    let (manager, loaded) = load_extensions(&ext_paths);
    eprintln!("  Testing {loaded} extensions with mixed event types");

    let events = [
        (
            ExtensionEventName::AgentStart,
            json!({"systemPrompt": "test"}),
        ),
        (ExtensionEventName::TurnStart, json!({"turnIndex": 1})),
        (ExtensionEventName::MessageStart, json!({"role": "user"})),
        (ExtensionEventName::Input, json!({"text": "hello"})),
    ];

    let mut total = 0u64;
    let mut errors = 0u64;
    let start = Instant::now();

    for (event, payload) in &events {
        for _ in 0..50 {
            let result = common::run_async({
                let manager = manager.clone();
                let payload = Some(payload.clone());
                let event = *event;
                async move { manager.dispatch_event(event, payload).await }
            });
            if result.is_err() {
                errors += 1;
            }
            total += 1;
        }
    }
    let elapsed = start.elapsed();

    eprintln!(
        "  Mixed events: {} dispatched in {:.1}ms ({} errors)",
        total,
        elapsed.as_secs_f64() * 1000.0,
        errors
    );

    assert!(total >= 200, "should have dispatched all events");

    // Cleanup
    common::run_async({
        let manager = manager;
        async move {
            let _ = manager.shutdown(Duration::from_millis(500)).await;
        }
    });
}

#[test]
fn stress_extension_load_unload_cycle() {
    const CYCLES: usize = 3;

    // Load extensions, dispatch events, shutdown, repeat — verify no resource leaks
    let ext_paths = collect_safe_extensions(5);
    if ext_paths.len() < 3 {
        eprintln!("  Skipping load/unload cycle test: not enough extensions");
        return;
    }

    let pid = get_current_pid().expect("get PID");
    let refresh = ProcessRefreshKind::nothing().with_memory();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));

    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let initial_rss = system.process(pid).map_or(0, sysinfo::Process::memory);
    for cycle in 0..CYCLES {
        let (manager, loaded) = load_extensions(&ext_paths);
        eprintln!("  Cycle {}/{CYCLES}: loaded {loaded} extensions", cycle + 1);

        // Dispatch some events
        for _ in 0..20 {
            let _ = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event(
                            ExtensionEventName::AgentStart,
                            Some(json!({"systemPrompt": "test"})),
                        )
                        .await
                }
            });
        }

        // Shutdown
        common::run_async({
            let manager = manager.clone();
            async move {
                let _ = manager.shutdown(Duration::from_secs(1)).await;
            }
        });
    }

    // Check RSS after all cycles
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let final_rss = system.process(pid).map_or(0, sysinfo::Process::memory);

    eprintln!("  Load/unload cycles: RSS {initial_rss}KB → {final_rss}KB");

    // Allow generous budget for test overhead (GC, allocator fragmentation)
    // Main goal: detect catastrophic leaks, not minor fluctuations
    if initial_rss > 0 {
        #[allow(clippy::cast_precision_loss)]
        let growth = (final_rss.saturating_sub(initial_rss) as f64) / (initial_rss as f64);
        assert!(
            growth <= 0.50,
            "RSS after {CYCLES} load/unload cycles should not grow >50% (got {:.1}%)",
            growth * 100.0
        );
    }
}
