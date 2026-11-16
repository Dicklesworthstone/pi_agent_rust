//! Heterogeneous workload E2E evidence for OCO budget tuning (`bd-2p9jj`).
//!
//! Compares static budget control vs OCO-adaptive control across multiple
//! workload regimes and emits a machine-readable artifact.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{
    ExtensionBudgetControllerConfig, ExtensionEventName, ExtensionManager, ExtensionPolicy,
    HostcallReactorConfig, JsExtensionLoadSpec,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIN_EXTENSIONS: usize = 10;
const OCO_E2E_DURATION_SECS: u64 = 3;
const REACTOR_SHARD_COUNT: usize = 4;
const REACTOR_LANE_CAPACITY: usize = 512;
const REACTOR_DRAIN_BUDGET: usize = 128;
const MAX_P99_REGRESSION_RATIO: f64 = 2.0;
const MAX_ERROR_RATE_DELTA_PCT: f64 = 15.0;
const MIN_THROUGHPUT_RATIO: f64 = 0.75;

#[derive(Debug, Clone, Copy)]
struct WorkloadSpec {
    name: &'static str,
    events_per_sec: u64,
}

#[derive(Debug, Serialize)]
struct WorkloadMetrics {
    event_count: u64,
    p95_us: u64,
    p99_us: u64,
    throughput_eps: f64,
    error_rate_pct: f64,
    rejected_enqueues: u64,
    oco_rounds_max: u64,
    oco_guardrail_rollbacks_max: u64,
}

#[derive(Debug, Serialize)]
struct OcoComparisonSlice {
    workload: String,
    events_per_sec: u64,
    baseline: WorkloadMetrics,
    adaptive: WorkloadMetrics,
    p99_ratio_adaptive_vs_static: f64,
    throughput_ratio_adaptive_vs_static: f64,
    error_rate_delta_pct: f64,
    thresholds: OcoThresholds,
    pass: bool,
}

#[derive(Debug, Serialize, Clone, Copy)]
struct OcoThresholds {
    max_p99_regression_ratio: f64,
    max_error_rate_delta_pct: f64,
    min_throughput_ratio: f64,
}

#[derive(Debug, Serialize)]
struct OcoEvidenceReport {
    schema: String,
    generated_at: String,
    duration_secs_per_workload: u64,
    extensions_loaded: usize,
    slices: Vec<OcoComparisonSlice>,
    overall_pass: bool,
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn report_dir() -> PathBuf {
    let repo_root = project_root();
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map_or_else(
            || repo_root.join("target"),
            |path| {
                if path.is_absolute() {
                    path
                } else {
                    repo_root.join(path)
                }
            },
        );
    target_dir.join("perf")
}

fn collect_safe_extensions(max: usize) -> Vec<PathBuf> {
    let manifest_path = project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let data = std::fs::read_to_string(&manifest_path).expect("read VALIDATED_MANIFEST.json");
    let manifest: Value = serde_json::from_str(&data).expect("parse VALIDATED_MANIFEST.json");
    let extensions = manifest["extensions"].as_array().expect("extensions array");

    let artifacts = artifacts_dir();
    let mut paths = Vec::new();

    for ext in extensions {
        if paths.len() >= max {
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

    paths
}

fn enable_reactor_if_needed(manager: &ExtensionManager) {
    if manager.hostcall_reactor_enabled() {
        return;
    }
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: REACTOR_SHARD_COUNT,
        lane_capacity: REACTOR_LANE_CAPACITY,
        core_ids: None,
    });
}

fn load_extensions_with_oco_mode(
    paths: &[PathBuf],
    oco_enabled: bool,
) -> (ExtensionManager, usize) {
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
            pi::extensions::JsExtensionRuntimeHandle::start_with_policy(
                js_config,
                tools,
                manager,
                ExtensionPolicy::default(),
            )
            .await
            .expect("start JS runtime for OCO E2E")
        }
    });
    manager.set_js_runtime(runtime);
    enable_reactor_if_needed(&manager);

    let mut budget_config = ExtensionBudgetControllerConfig {
        enabled: true,
        ..Default::default()
    };
    budget_config.oco_tuner.enabled = oco_enabled;
    manager.set_budget_controller_config(budget_config);

    let mut specs = Vec::new();
    for path in paths {
        if let Ok(spec) = JsExtensionLoadSpec::from_entry_path(path) {
            specs.push(spec);
        }
    }
    let count = specs.len();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load JS extensions for OCO E2E");
        }
    });

    (manager, count)
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * pct).saturating_add(99) / 100;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn error_rate_pct(error_count: u64, event_count: u64) -> f64 {
    if event_count == 0 {
        return 0.0;
    }
    (error_count as f64 / event_count as f64) * 100.0
}

fn workload_payload(workload: &str) -> Value {
    match workload {
        "interactive_light" => json!({
            "systemPrompt": "You are Pi.",
            "model": "claude-sonnet-4-5",
            "context": "short-interactive"
        }),
        "bursty_tools" => json!({
            "systemPrompt": "You are Pi.",
            "model": "claude-sonnet-4-5",
            "toolHint": "simulate-bursty-tool-calls",
            "context": "medium-burst"
        }),
        "queue_pressure" => json!({
            "systemPrompt": "You are Pi.",
            "model": "claude-sonnet-4-5",
            "toolHint": "heavy-queue-pressure",
            "context": "long-session-heavy"
        }),
        _ => json!({
            "systemPrompt": "You are Pi.",
            "model": "claude-sonnet-4-5"
        }),
    }
}

fn run_workload(
    manager: &ExtensionManager,
    workload: &WorkloadSpec,
    duration: Duration,
) -> WorkloadMetrics {
    let payload = workload_payload(workload.name);
    let interval = Duration::from_secs_f64(1.0 / workload.events_per_sec as f64);
    let start = Instant::now();
    let mut next_event = start;
    let mut latencies_us = Vec::new();
    let mut error_count = 0_u64;
    let mut event_count = 0_u64;

    while start.elapsed() < duration {
        let now = Instant::now();
        if now < next_event {
            std::thread::sleep(next_event.duration_since(now).min(Duration::from_millis(1)));
            continue;
        }

        let dispatch_start = Instant::now();
        let result = common::run_async({
            let manager = manager.clone();
            let payload = payload.clone();
            async move {
                manager
                    .dispatch_event(ExtensionEventName::AgentStart, Some(payload))
                    .await
            }
        });
        let elapsed_us = u64::try_from(dispatch_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        latencies_us.push(elapsed_us);
        if result.is_err() {
            error_count = error_count.saturating_add(1);
        }
        event_count = event_count.saturating_add(1);
        let _ = manager.reactor_drain_global(REACTOR_DRAIN_BUDGET);

        next_event += interval;
        let catch_up = Instant::now();
        if next_event < catch_up {
            next_event = catch_up + interval;
        }
    }

    latencies_us.sort_unstable();
    let p95_us = percentile(&latencies_us, 95);
    let p99_us = percentile(&latencies_us, 99);
    let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
    let throughput_eps = event_count as f64 / elapsed_s;
    let error_rate_pct = error_rate_pct(error_count, event_count);
    let rejected_enqueues = manager
        .reactor_telemetry()
        .map_or(0, |telemetry| telemetry.rejected_enqueues);

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let mut extension_ids = BTreeSet::new();
    for entry in telemetry.entries {
        if !entry.extension_id.is_empty() {
            extension_ids.insert(entry.extension_id);
        }
    }
    let mut oco_rounds_max = 0_u64;
    let mut oco_guardrail_rollbacks_max = 0_u64;
    for extension_id in extension_ids {
        if let Some(snapshot) = manager.oco_tuner_snapshot(&extension_id) {
            oco_rounds_max = oco_rounds_max.max(snapshot.rounds);
            oco_guardrail_rollbacks_max =
                oco_guardrail_rollbacks_max.max(snapshot.guardrail_rollbacks);
        }
    }

    WorkloadMetrics {
        event_count,
        p95_us,
        p99_us,
        throughput_eps,
        error_rate_pct,
        rejected_enqueues,
        oco_rounds_max,
        oco_guardrail_rollbacks_max,
    }
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator <= f64::EPSILON {
        1.0
    } else {
        numerator / denominator
    }
}

fn shutdown(manager: ExtensionManager) {
    common::run_async(async move {
        let _ = manager.shutdown(Duration::from_millis(750)).await;
    });
}

fn strict_oco_gate_enabled() -> bool {
    std::env::var("PI_OCO_HETEROGENEOUS_STRICT").is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

#[test]
fn oco_tuner_heterogeneous_workload_e2e_evidence() {
    let ext_paths = collect_safe_extensions(15);
    assert!(
        ext_paths.len() >= MIN_EXTENSIONS,
        "Need at least {MIN_EXTENSIONS} safe extensions for OCO E2E, found {}",
        ext_paths.len()
    );

    let workloads = [
        WorkloadSpec {
            name: "interactive_light",
            events_per_sec: 24,
        },
        WorkloadSpec {
            name: "bursty_tools",
            events_per_sec: 60,
        },
        WorkloadSpec {
            name: "queue_pressure",
            events_per_sec: 110,
        },
    ];
    let duration = Duration::from_secs(OCO_E2E_DURATION_SECS);
    let thresholds = OcoThresholds {
        max_p99_regression_ratio: MAX_P99_REGRESSION_RATIO,
        max_error_rate_delta_pct: MAX_ERROR_RATE_DELTA_PCT,
        min_throughput_ratio: MIN_THROUGHPUT_RATIO,
    };

    let mut slices = Vec::new();
    let mut extensions_loaded = 0_usize;

    for workload in workloads {
        let (baseline_manager, baseline_loaded) = load_extensions_with_oco_mode(&ext_paths, false);
        extensions_loaded = extensions_loaded.max(baseline_loaded);
        let baseline = run_workload(&baseline_manager, &workload, duration);
        shutdown(baseline_manager);

        let (adaptive_manager, adaptive_loaded) = load_extensions_with_oco_mode(&ext_paths, true);
        extensions_loaded = extensions_loaded.max(adaptive_loaded);
        let adaptive = run_workload(&adaptive_manager, &workload, duration);
        shutdown(adaptive_manager);

        let p99_ratio = ratio(adaptive.p99_us as f64, baseline.p99_us as f64);
        let throughput_ratio = ratio(adaptive.throughput_eps, baseline.throughput_eps);
        let error_rate_delta_pct = adaptive.error_rate_pct - baseline.error_rate_pct;
        let adaptive_signal_present = adaptive.oco_rounds_max > 0;

        let pass = p99_ratio <= thresholds.max_p99_regression_ratio
            && error_rate_delta_pct <= thresholds.max_error_rate_delta_pct
            && throughput_ratio >= thresholds.min_throughput_ratio
            && adaptive_signal_present;

        slices.push(OcoComparisonSlice {
            workload: workload.name.to_string(),
            events_per_sec: workload.events_per_sec,
            baseline,
            adaptive,
            p99_ratio_adaptive_vs_static: p99_ratio,
            throughput_ratio_adaptive_vs_static: throughput_ratio,
            error_rate_delta_pct,
            thresholds,
            pass,
        });
    }

    let report = OcoEvidenceReport {
        schema: "pi.ext.oco_heterogeneous_e2e.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        duration_secs_per_workload: OCO_E2E_DURATION_SECS,
        extensions_loaded,
        overall_pass: slices.iter().all(|slice| slice.pass),
        slices,
    };

    let output_dir = report_dir();
    std::fs::create_dir_all(&output_dir).expect("create OCO heterogeneous report directory");
    let output_path = output_dir.join("oco_heterogeneous_e2e.json");
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(&report).expect("serialize OCO heterogeneous report"),
    )
    .expect("write OCO heterogeneous report");

    let strict_gate = strict_oco_gate_enabled();
    if !report.overall_pass {
        eprintln!(
            "OCO heterogeneous evidence exceeded one or more thresholds (strict_gate={}): {}",
            strict_gate,
            output_path.to_string_lossy()
        );
    }
    if strict_gate {
        assert!(
            report.overall_pass,
            "OCO heterogeneous workload evidence failed; see {}",
            output_path.to_string_lossy()
        );
    }
}
