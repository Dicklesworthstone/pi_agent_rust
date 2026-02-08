//! Extension memory profiling stress test (bd-2zxd).
//!
//! Loads 10+ extensions simultaneously, dispatches thousands of events, and
//! measures both RSS (process-level) and `QuickJS` heap usage at regular
//! intervals.
//!
//! ## Modes
//!
//! - Default (CI-friendly): 60 seconds, ~17 events/sec (1000 events/min)
//! - Full 1-hour: `PI_MEM_STRESS_DURATION_SECS=3600`
//!
//! ## Environment Variables
//!
//! | Variable                       | Default | Description                           |
//! |--------------------------------|---------|---------------------------------------|
//! | `PI_MEM_STRESS_DURATION_SECS`  | 60      | Total run duration                    |
//! | `PI_MEM_STRESS_EVENTS_PER_SEC` | 17      | Event dispatch rate (~1000/min)       |
//! | `PI_MEM_STRESS_RSS_INTERVAL`   | 5       | Seconds between RSS samples           |
//! | `PI_MEM_STRESS_MAX_EXTENSIONS` | 20      | Max extensions to load simultaneously |
//!
//! ## Output
//!
//! - CSV: `target/perf/ext_memory_stress.csv`
//! - Report: `target/perf/ext_memory_stress_report.json`

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, get_current_pid};

// ─── Configuration ──────────────────────────────────────────────────────────

fn duration_secs() -> u64 {
    std::env::var("PI_MEM_STRESS_DURATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

fn events_per_sec() -> u64 {
    std::env::var("PI_MEM_STRESS_EVENTS_PER_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(17)
}

fn rss_interval_secs() -> u64 {
    std::env::var("PI_MEM_STRESS_RSS_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

fn max_extensions() -> usize {
    std::env::var("PI_MEM_STRESS_MAX_EXTENSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

// ─── Manifest ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    is_multi_file: bool,
    uses_exec: bool,
}

impl ManifestEntry {
    const fn is_safe(&self) -> bool {
        !self.is_multi_file && !self.uses_exec
    }
}

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn output_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/perf")
}

fn load_manifest() -> &'static Vec<ManifestEntry> {
    static MANIFEST: OnceLock<Vec<ManifestEntry>> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/ext_conformance/VALIDATED_MANIFEST.json");
        let data = std::fs::read_to_string(&path).expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value = serde_json::from_str(&data).expect("Failed to parse manifest");
        json["extensions"]
            .as_array()
            .expect("extensions array")
            .iter()
            .map(|e| {
                let caps = &e["capabilities"];
                ManifestEntry {
                    id: e["id"].as_str().unwrap_or("").to_string(),
                    entry_path: e["entry_path"].as_str().unwrap_or("").to_string(),
                    is_multi_file: caps["is_multi_file"].as_bool().unwrap_or(false),
                    uses_exec: caps["uses_exec"].as_bool().unwrap_or(false),
                }
            })
            .collect()
    })
}

// ─── RSS Measurement ────────────────────────────────────────────────────────

fn measure_rss_kb(system: &mut System, pid: sysinfo::Pid, refresh: ProcessRefreshKind) -> u64 {
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    // sysinfo::Process::memory() returns bytes; convert to KB
    system.process(pid).map_or(0, |p| p.memory() / 1024)
}

/// Read `QuickJS` heap estimate from `/proc/self/statm`.
///
/// Returns `(rss_pages, data_pages)` — `data_pages` is the "data + stack"
/// field which closely tracks heap allocations including the `QuickJS` arena.
/// Multiply by `PAGE_SIZE` (typically 4096) for bytes.
fn read_statm() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/self/statm").ok()?;
    let fields: Vec<u64> = content
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    // statm fields: size resident shared text lib data dt
    // Index 1 = resident, Index 5 = data+stack
    if fields.len() >= 6 {
        Some((fields[1], fields[5]))
    } else {
        None
    }
}

const fn page_size_bytes() -> u64 {
    // On Linux, page size is virtually always 4096. We could use libc::sysconf
    // but that would add a dependency. Read from /proc/self/auxv if needed,
    // or just use the standard 4KiB.
    4096
}

// ─── Data Structures ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct MemorySample {
    elapsed_secs: u64,
    rss_kb: u64,
    /// `QuickJS` heap estimate in KB (data segment from `/proc/self/statm`).
    quickjs_heap_kb: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StressReport {
    schema: String,
    generated_at: String,
    config: StressConfig,
    extensions_loaded: usize,
    extension_names: Vec<String>,
    total_samples: usize,
    events_dispatched: u64,
    event_errors: u64,
    rss_baseline_kb: u64,
    rss_peak_kb: u64,
    rss_growth_factor: f64,
    monotonic_rss_growth: bool,
    /// `QuickJS` heap baseline (KB, from data segment of `/proc/self/statm`).
    quickjs_baseline_kb: u64,
    /// `QuickJS` heap peak (KB).
    quickjs_peak_kb: u64,
    /// `QuickJS` heap growth factor (peak / baseline).
    quickjs_growth_factor: f64,
    monotonic_quickjs_growth: bool,
    verdict: StressVerdict,
}

#[derive(Debug, Clone, Serialize)]
struct StressConfig {
    duration_secs: u64,
    events_per_sec: u64,
    rss_interval_secs: u64,
    max_extensions: usize,
}

#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct StressVerdict {
    /// Peak RSS is within 2x of baseline.
    rss_within_2x: bool,
    /// No monotonic growth trend across sample quarters.
    no_monotonic_rss_growth: bool,
    /// `QuickJS` heap within 2x of baseline.
    quickjs_within_2x: bool,
    /// No monotonic `QuickJS` heap growth.
    no_monotonic_quickjs_growth: bool,
    /// Overall pass/fail.
    pass: bool,
}

// ─── Monotonic Growth Detection ─────────────────────────────────────────────

/// Detect monotonic growth by splitting samples into 4 quarters and
/// checking if each quarter's median is strictly higher than the previous.
/// Requires at least 8 samples for meaningful analysis.
fn is_monotonic_growth(values: &[u64]) -> bool {
    if values.len() < 8 {
        return false; // not enough data
    }

    let quarter = values.len() / 4;
    let medians: Vec<u64> = (0..4)
        .map(|q| {
            let start = q * quarter;
            let end = if q == 3 {
                values.len()
            } else {
                start + quarter
            };
            let mut slice = values[start..end].to_vec();
            slice.sort_unstable();
            slice[slice.len() / 2]
        })
        .collect();

    // Monotonic if each quarter median is strictly larger than the previous
    medians.windows(2).all(|w| w[1] > w[0])
}

// ─── Inline Extension Generator (for CI without conformance artifacts) ──────

/// Generate N inline JS extensions that register tools and event hooks,
/// allocate some heap memory, and do minimal work on each event.
fn generate_inline_extensions(
    harness: &common::TestHarness,
    count: usize,
) -> Vec<JsExtensionLoadSpec> {
    let mut specs = Vec::with_capacity(count);
    for i in 0..count {
        let source = format!(
            r#"
export default function init(pi) {{
    // Allocate some heap to simulate real extension state
    const state = {{
        buffer: new Array(100).fill("ext{i}_data"),
        counter: 0,
    }};

    pi.registerTool({{
        name: "mem_probe_{i}",
        description: "Memory probe tool {i}",
        execute: async (_callId, _input) => {{
            state.counter++;
            return {{ ok: true }};
        }},
    }});

    pi.on("agent:start", async () => {{
        state.counter++;
        // Allocate a small temporary to exercise GC
        const tmp = new Array(10).fill(state.counter);
        return tmp.length;
    }});
}}
"#
        );
        let path = harness.create_file(
            format!("extensions/mem_ext_{i}/index.mjs"),
            source.as_bytes(),
        );
        if let Ok(spec) = JsExtensionLoadSpec::from_entry_path(&path) {
            specs.push(spec);
        }
    }
    specs
}

// ─── Core Stress Loop ───────────────────────────────────────────────────────

/// Parameters for the stress loop.
struct StressParams {
    duration: Duration,
    events_per_sec: u64,
    sample_interval: Duration,
}

/// Outcome from running the stress loop.
struct StressOutcome {
    samples: Vec<MemorySample>,
    events_dispatched: u64,
    event_errors: u64,
    rss_baseline_kb: u64,
    rss_peak_kb: u64,
    quickjs_baseline_kb: u64,
    quickjs_peak_kb: u64,
}

#[allow(clippy::too_many_lines)]
fn run_stress_loop(manager: &ExtensionManager, params: &StressParams) -> StressOutcome {
    let pid = get_current_pid().expect("get pid");
    let refresh = ProcessRefreshKind::nothing().with_memory();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));

    let rss_baseline_kb = measure_rss_kb(&mut system, pid, refresh);
    let ps = page_size_bytes();
    let quickjs_baseline_kb = read_statm().map_or(0, |(_, data)| data * ps / 1024);

    eprintln!(
        "[mem-stress] baseline: rss={rss_baseline_kb}KB quickjs_heap={quickjs_baseline_kb}KB"
    );

    let mut samples: Vec<MemorySample> = Vec::new();
    let mut events_dispatched: u64 = 0;
    let mut event_errors: u64 = 0;
    let mut rss_peak_kb = rss_baseline_kb;
    let mut quickjs_peak_kb = quickjs_baseline_kb;

    let start = Instant::now();
    let mut next_sample = start + params.sample_interval;

    #[allow(clippy::cast_precision_loss)]
    let event_interval = Duration::from_secs_f64(1.0 / params.events_per_sec as f64);
    let mut next_event = start;

    while start.elapsed() < params.duration {
        // Dispatch events
        let now = Instant::now();
        if now >= next_event {
            let result = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event(ExtensionEventName::AgentStart, None)
                        .await
                }
            });
            if result.is_err() {
                event_errors += 1;
            }
            events_dispatched += 1;

            next_event += event_interval;
            // Catch up if behind
            let after = Instant::now();
            if next_event < after {
                next_event = after + event_interval;
            }
        }

        // Sample memory at intervals
        if Instant::now() >= next_sample {
            let rss_kb = measure_rss_kb(&mut system, pid, refresh);
            let quickjs_kb = read_statm().map_or(0, |(_, data)| data * ps / 1024);

            if rss_kb > rss_peak_kb {
                rss_peak_kb = rss_kb;
            }
            if quickjs_kb > quickjs_peak_kb {
                quickjs_peak_kb = quickjs_kb;
            }

            let elapsed_secs = start.elapsed().as_secs();
            samples.push(MemorySample {
                elapsed_secs,
                rss_kb,
                quickjs_heap_kb: quickjs_kb,
            });

            if samples.len() % 6 == 0 {
                eprintln!(
                    "[mem-stress] t={elapsed_secs}s events={events_dispatched} \
                     rss={rss_kb}KB heap={quickjs_kb}KB"
                );
            }

            next_sample += params.sample_interval;
        }

        // Small sleep to avoid busy-waiting
        std::thread::sleep(Duration::from_millis(1));
    }

    eprintln!(
        "[mem-stress] done: {events_dispatched} events ({event_errors} errors), {} samples",
        samples.len()
    );

    StressOutcome {
        samples,
        events_dispatched,
        event_errors,
        rss_baseline_kb,
        rss_peak_kb,
        quickjs_baseline_kb,
        quickjs_peak_kb,
    }
}

// ─── Verdict + Report ──────────────────────────────────────────────────────

#[allow(clippy::cast_precision_loss)]
fn compute_verdict(outcome: &StressOutcome) -> VerdictData {
    let rss_growth_factor = if outcome.rss_baseline_kb > 0 {
        outcome.rss_peak_kb as f64 / outcome.rss_baseline_kb as f64
    } else {
        1.0
    };

    let quickjs_growth_factor = if outcome.quickjs_baseline_kb > 0 {
        outcome.quickjs_peak_kb as f64 / outcome.quickjs_baseline_kb as f64
    } else {
        1.0
    };

    let rss_values: Vec<u64> = outcome.samples.iter().map(|s| s.rss_kb).collect();
    let monotonic_rss = is_monotonic_growth(&rss_values);

    let quickjs_values: Vec<u64> = outcome.samples.iter().map(|s| s.quickjs_heap_kb).collect();
    let monotonic_quickjs = is_monotonic_growth(&quickjs_values);

    let verdict = StressVerdict {
        rss_within_2x: rss_growth_factor <= 2.0,
        no_monotonic_rss_growth: !monotonic_rss,
        quickjs_within_2x: quickjs_growth_factor <= 2.0,
        no_monotonic_quickjs_growth: !monotonic_quickjs,
        pass: rss_growth_factor <= 2.0
            && !monotonic_rss
            && quickjs_growth_factor <= 2.0
            && !monotonic_quickjs,
    };

    VerdictData {
        verdict,
        rss_growth_factor,
        monotonic_rss,
        quickjs_growth_factor,
        monotonic_quickjs,
    }
}

/// All computed verdict data for report generation.
struct VerdictData {
    verdict: StressVerdict,
    rss_growth_factor: f64,
    monotonic_rss: bool,
    quickjs_growth_factor: f64,
    monotonic_quickjs: bool,
}

fn write_report(outcome: &StressOutcome, vd: &VerdictData, ext_names: &[String], max_ext: usize) {
    let out_dir = output_dir();
    let _ = std::fs::create_dir_all(&out_dir);

    // CSV timeline
    let csv_path = out_dir.join("ext_memory_stress.csv");
    let mut csv = String::from("elapsed_secs,rss_kb,quickjs_heap_kb\n");
    for s in &outcome.samples {
        writeln!(csv, "{},{},{}", s.elapsed_secs, s.rss_kb, s.quickjs_heap_kb).unwrap();
    }
    std::fs::write(&csv_path, &csv).expect("write CSV");
    eprintln!("[mem-stress] CSV: {}", csv_path.display());

    // JSON report
    let report = StressReport {
        schema: "pi.ext.memory_stress.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        config: StressConfig {
            duration_secs: duration_secs(),
            events_per_sec: events_per_sec(),
            rss_interval_secs: rss_interval_secs(),
            max_extensions: max_ext,
        },
        extensions_loaded: ext_names.len(),
        extension_names: ext_names.to_vec(),
        total_samples: outcome.samples.len(),
        events_dispatched: outcome.events_dispatched,
        event_errors: outcome.event_errors,
        rss_baseline_kb: outcome.rss_baseline_kb,
        rss_peak_kb: outcome.rss_peak_kb,
        rss_growth_factor: vd.rss_growth_factor,
        monotonic_rss_growth: vd.monotonic_rss,
        quickjs_baseline_kb: outcome.quickjs_baseline_kb,
        quickjs_peak_kb: outcome.quickjs_peak_kb,
        quickjs_growth_factor: vd.quickjs_growth_factor,
        monotonic_quickjs_growth: vd.monotonic_quickjs,
        verdict: vd.verdict.clone(),
    };
    let report_path = out_dir.join("ext_memory_stress_report.json");
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .expect("write report");
    eprintln!("[mem-stress] Report: {}", report_path.display());
}

// ─── Tests ─────────────────────────────────────────────────────────────────

/// Full stress test using real conformance extensions (requires artifacts).
/// Gate behind env var to avoid running on every `cargo test`.
#[test]
#[allow(clippy::too_many_lines)]
fn ext_memory_stress_real_extensions() {
    if std::env::var("PI_MEM_STRESS_REAL").is_err() {
        eprintln!(
            "[mem-stress] skipping real extension stress test \
             (set PI_MEM_STRESS_REAL=1 to enable)"
        );
        return;
    }

    let duration = Duration::from_secs(duration_secs());
    let eps = events_per_sec();
    let sample_interval = Duration::from_secs(rss_interval_secs());
    let max_ext = max_extensions();

    eprintln!(
        "\n[mem-stress] duration={}s events/sec={eps} sample_interval={}s max_extensions={max_ext}",
        duration.as_secs(),
        sample_interval.as_secs()
    );

    // Load extensions from conformance manifest
    let manifest = load_manifest();
    let safe: Vec<&ManifestEntry> = manifest
        .iter()
        .filter(|e| e.is_safe())
        .take(max_ext)
        .collect();

    assert!(
        safe.len() >= 10,
        "Need at least 10 safe extensions, found {}",
        safe.len()
    );

    let arts = artifacts_dir();
    let specs: Vec<JsExtensionLoadSpec> = safe
        .iter()
        .filter_map(|e| {
            let path = arts.join(&e.entry_path);
            JsExtensionLoadSpec::from_entry_path(&path).ok()
        })
        .collect();
    let ext_names: Vec<String> = safe.iter().map(|e| e.id.clone()).collect();

    eprintln!("[mem-stress] loading {} extensions...", specs.len());

    let cwd = std::env::temp_dir().join("pi-mem-stress");
    let _ = std::fs::create_dir_all(&cwd);
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let manager = ExtensionManager::new();
    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    })
    .expect("start JS runtime");
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await }
    })
    .expect("load extensions");

    eprintln!(
        "[mem-stress] {} extensions loaded, starting stress loop",
        ext_names.len()
    );

    let params = StressParams {
        duration,
        events_per_sec: eps,
        sample_interval,
    };
    let outcome = run_stress_loop(&manager, &params);

    // Shutdown
    common::run_async({
        async move {
            let _ = manager.shutdown(Duration::from_secs(5)).await;
        }
    });

    let vd = compute_verdict(&outcome);

    eprintln!("[mem-stress] === VERDICT ===");
    eprintln!(
        "  RSS:    baseline={}KB peak={}KB growth={:.2}x monotonic={}",
        outcome.rss_baseline_kb, outcome.rss_peak_kb, vd.rss_growth_factor, vd.monotonic_rss,
    );
    eprintln!(
        "  QuickJS: baseline={}KB peak={}KB growth={:.2}x monotonic={}",
        outcome.quickjs_baseline_kb,
        outcome.quickjs_peak_kb,
        vd.quickjs_growth_factor,
        vd.monotonic_quickjs,
    );
    eprintln!(
        "  Pass: rss_2x={} rss_mono={} qjs_2x={} qjs_mono={} -> {}",
        vd.verdict.rss_within_2x,
        vd.verdict.no_monotonic_rss_growth,
        vd.verdict.quickjs_within_2x,
        vd.verdict.no_monotonic_quickjs_growth,
        if vd.verdict.pass { "PASS" } else { "FAIL" }
    );

    write_report(&outcome, &vd, &ext_names, max_ext);

    // Assertion (release only, debug builds use more memory)
    if !cfg!(debug_assertions) {
        assert!(
            vd.verdict.pass,
            "Memory stress test failed: rss_2x={} rss_mono={} qjs_2x={} qjs_mono={}",
            vd.verdict.rss_within_2x,
            vd.verdict.no_monotonic_rss_growth,
            vd.verdict.quickjs_within_2x,
            vd.verdict.no_monotonic_quickjs_growth,
        );
    }
}

/// CI-friendly stress test using inline JS extensions (no conformance artifacts
/// needed). Runs for 15 seconds with 12 inline extensions.
#[test]
#[allow(clippy::too_many_lines)]
fn ext_memory_stress_inline() {
    let harness = common::TestHarness::new("ext_memory_stress_inline");

    let ext_count = 12;
    let specs = generate_inline_extensions(&harness, ext_count);
    assert!(
        specs.len() >= 10,
        "Failed to generate enough extensions: {}",
        specs.len()
    );

    eprintln!(
        "[mem-stress-inline] loading {} inline extensions...",
        specs.len()
    );

    let cwd = harness.temp_dir().to_path_buf();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let manager = ExtensionManager::new();
    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    })
    .expect("start JS runtime");
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await }
    })
    .expect("load extensions");

    eprintln!("[mem-stress-inline] {ext_count} extensions loaded, running 15s stress");

    let params = StressParams {
        duration: Duration::from_secs(15),
        events_per_sec: 20,
        sample_interval: Duration::from_secs(2),
    };
    let outcome = run_stress_loop(&manager, &params);

    // Shutdown
    common::run_async({
        async move {
            let _ = manager.shutdown(Duration::from_secs(5)).await;
        }
    });

    let vd = compute_verdict(&outcome);
    let inline_extension_names: Vec<String> = (0..ext_count)
        .map(|index| format!("inline/mem_ext_{index}"))
        .collect();
    write_report(&outcome, &vd, &inline_extension_names, ext_count);

    eprintln!("[mem-stress-inline] === VERDICT ===");
    eprintln!(
        "  RSS:    baseline={}KB peak={}KB growth={:.2}x monotonic={}",
        outcome.rss_baseline_kb, outcome.rss_peak_kb, vd.rss_growth_factor, vd.monotonic_rss,
    );
    eprintln!(
        "  QuickJS: baseline={}KB peak={}KB growth={:.2}x monotonic={}",
        outcome.quickjs_baseline_kb,
        outcome.quickjs_peak_kb,
        vd.quickjs_growth_factor,
        vd.monotonic_quickjs,
    );
    eprintln!("  Pass: {}", if vd.verdict.pass { "PASS" } else { "FAIL" });

    // For CI: verify no gross leak (events dispatched, RSS didn't explode)
    assert!(
        outcome.events_dispatched > 0,
        "No events dispatched during stress test"
    );
    assert!(
        outcome.rss_peak_kb > 0,
        "RSS measurement returned 0 — sysinfo may not work"
    );
    // Soft check: RSS shouldn't more than 3x baseline even in debug
    #[allow(clippy::cast_precision_loss)]
    let rss_ratio = if outcome.rss_baseline_kb > 0 {
        outcome.rss_peak_kb as f64 / outcome.rss_baseline_kb as f64
    } else {
        1.0
    };
    assert!(
        rss_ratio < 3.0,
        "RSS grew {rss_ratio:.2}x (baseline={}KB peak={}KB) — potential memory leak",
        outcome.rss_baseline_kb,
        outcome.rss_peak_kb,
    );

    // In release builds, apply the strict verdict
    if !cfg!(debug_assertions) {
        assert!(
            vd.verdict.pass,
            "Memory stress test failed: rss_2x={} rss_mono={} qjs_2x={} qjs_mono={}",
            vd.verdict.rss_within_2x,
            vd.verdict.no_monotonic_rss_growth,
            vd.verdict.quickjs_within_2x,
            vd.verdict.no_monotonic_quickjs_growth,
        );
    }
}

/// Verify that `/proc/self/statm` is readable and produces sane data.
#[test]
fn statm_is_readable() {
    let result = read_statm();
    assert!(result.is_some(), "/proc/self/statm should be readable");
    let (rss_pages, data_pages) = result.unwrap();
    assert!(rss_pages > 0, "RSS should be > 0 pages");
    assert!(data_pages > 0, "Data segment should be > 0 pages");

    let ps = page_size_bytes();
    assert!(ps >= 4096, "Page size should be at least 4096 bytes");

    let rss_kb = rss_pages * ps / 1024;
    let data_kb = data_pages * ps / 1024;
    eprintln!("[statm] rss={rss_kb}KB data={data_kb}KB page_size={ps}");
    // Sanity: a running Rust test process should use at least 1MB
    assert!(rss_kb > 1024, "RSS suspiciously low: {rss_kb}KB");
}

/// Verify CSV output format is correct.
#[test]
fn csv_format_correctness() {
    let samples = vec![
        MemorySample {
            elapsed_secs: 0,
            rss_kb: 100_000,
            quickjs_heap_kb: 50_000,
        },
        MemorySample {
            elapsed_secs: 5,
            rss_kb: 100_500,
            quickjs_heap_kb: 50_100,
        },
        MemorySample {
            elapsed_secs: 10,
            rss_kb: 101_000,
            quickjs_heap_kb: 50_200,
        },
    ];

    let mut csv = String::from("elapsed_secs,rss_kb,quickjs_heap_kb\n");
    for s in &samples {
        writeln!(csv, "{},{},{}", s.elapsed_secs, s.rss_kb, s.quickjs_heap_kb).unwrap();
    }

    let lines: Vec<&str> = csv.trim().lines().collect();
    assert_eq!(lines[0], "elapsed_secs,rss_kb,quickjs_heap_kb");
    assert_eq!(lines.len(), 4); // header + 3 data rows
    assert_eq!(lines[1], "0,100000,50000");
    assert_eq!(lines[3], "10,101000,50200");
}

/// Verify JSON report schema is well-formed.
#[test]
fn report_schema_validity() {
    let verdict = StressVerdict {
        rss_within_2x: true,
        no_monotonic_rss_growth: true,
        quickjs_within_2x: true,
        no_monotonic_quickjs_growth: true,
        pass: true,
    };

    let report = StressReport {
        schema: "pi.ext.memory_stress.v1".to_string(),
        generated_at: "2026-01-01T00:00:00Z".to_string(),
        config: StressConfig {
            duration_secs: 60,
            events_per_sec: 17,
            rss_interval_secs: 5,
            max_extensions: 20,
        },
        extensions_loaded: 12,
        extension_names: vec!["test_ext".to_string()],
        total_samples: 12,
        events_dispatched: 1020,
        event_errors: 0,
        rss_baseline_kb: 100_000,
        rss_peak_kb: 110_000,
        rss_growth_factor: 1.1,
        monotonic_rss_growth: false,
        quickjs_baseline_kb: 50_000,
        quickjs_peak_kb: 55_000,
        quickjs_growth_factor: 1.1,
        monotonic_quickjs_growth: false,
        verdict,
    };

    let json = serde_json::to_string_pretty(&report).expect("serialize report");
    let parsed: Value = serde_json::from_str(&json).expect("parse report");

    assert_eq!(parsed["schema"], "pi.ext.memory_stress.v1");
    assert_eq!(parsed["extensions_loaded"], 12);
    assert_eq!(parsed["events_dispatched"], 1020);
    assert!(parsed["verdict"]["pass"].as_bool().unwrap());
    assert!(parsed["quickjs_baseline_kb"].is_number());
    assert!(parsed["quickjs_peak_kb"].is_number());
    assert!(parsed["quickjs_growth_factor"].is_number());
}

// ─── Unit Tests for Monotonic Detection ─────────────────────────────────────

#[cfg(test)]
mod monotonic_tests {
    use super::is_monotonic_growth;

    #[test]
    fn detects_monotonic_growth() {
        // Clearly increasing: each quarter median is higher than previous
        let values = vec![
            10, 11, 12, 13, 20, 21, 22, 23, 30, 31, 32, 33, 40, 41, 42, 43,
        ];
        assert!(is_monotonic_growth(&values));
    }

    #[test]
    fn stable_values_not_monotonic() {
        let values = vec![100, 101, 99, 100, 100, 99, 101, 100, 99, 100, 101, 100];
        assert!(!is_monotonic_growth(&values));
    }

    #[test]
    fn too_few_samples_not_monotonic() {
        assert!(!is_monotonic_growth(&[1, 2, 3, 4]));
        assert!(!is_monotonic_growth(&[]));
    }

    #[test]
    fn decreasing_values_not_monotonic() {
        let values = vec![40, 39, 38, 37, 30, 29, 28, 27, 20, 19, 18, 17, 10, 9, 8, 7];
        assert!(!is_monotonic_growth(&values));
    }

    #[test]
    fn saw_tooth_not_monotonic() {
        // Goes up then down — not monotonically growing
        let values = vec![
            10, 20, 30, 40, 50, 40, 30, 20, 10, 20, 30, 40, 50, 40, 30, 20,
        ];
        assert!(!is_monotonic_growth(&values));
    }

    #[test]
    fn flat_then_spike_not_monotonic() {
        // Stable for 3 quarters, spike in 4th — only 1 increasing pair, not all 3
        let values = vec![100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 200];
        assert!(!is_monotonic_growth(&values));
    }

    #[test]
    fn gradual_leak_detected() {
        // Simulate a slow leak: each quarter ~5% higher
        let values = vec![
            100, 101, 102, 103, 105, 106, 107, 108, 110, 111, 112, 113, 115, 116, 117, 118,
        ];
        assert!(is_monotonic_growth(&values));
    }

    #[test]
    fn noisy_but_stable_not_monotonic() {
        // Random noise around 100, no trend
        let values = vec![
            98, 103, 97, 101, 99, 104, 96, 102, 100, 97, 103, 98, 101, 99, 100, 98,
        ];
        assert!(!is_monotonic_growth(&values));
    }
}
