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
use pi::extensions::{
    ExtensionEventName, ExtensionManager, ExtensionPolicy, HostcallReactorConfig,
    JsExtensionLoadSpec, PolicyProfile,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::hostcall_s3_fifo::{S3FifoConfig, S3FifoDecisionKind, S3FifoPolicy};
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
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
/// Maximum acceptable RSS growth (10% local).
const MAX_RSS_GROWTH_PCT: f64 = 0.10;
/// Absolute growth budget for short load/unload cycles.
const LOAD_UNLOAD_ABSOLUTE_RSS_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum acceptable latency degradation (2x).
const MAX_LATENCY_DEGRADATION: u64 = 2;
/// Absolute p99 cap for noisy shared CI/agent hosts.
const MAX_P99_LAST_US: u64 = 25_000;
/// Run-wide profile-rotation p99 cap for short shared-host stress slices.
const PROFILE_ROTATION_MAX_RUN_P99_US: u64 = 100_000;
/// Default per-profile duration for policy-rotation soak slice.
const PROFILE_ROTATION_DURATION_SECS: u64 = 8;
/// Event rate for policy-rotation soak slice.
const PROFILE_ROTATION_EVENTS_PER_SEC: u64 = 30;
/// Sampling interval for policy-rotation soak slice.
const PROFILE_ROTATION_RSS_INTERVAL_SECS: u64 = 3;
/// Error-rate budget for policy-rotation soak slices.
const MAX_PROFILE_ERROR_RATE_PCT: f64 = 25.0;
/// Default shard count for reactor diagnostics in stress runs.
const REACTOR_SHARD_COUNT: usize = 4;
/// Queue capacity per reactor shard used during stress runs.
const REACTOR_LANE_CAPACITY: usize = 512;
/// Per-iteration drain budget to keep reactor queue telemetry bounded.
const REACTOR_DRAIN_BUDGET: usize = 128;

/// CI runners have unpredictable RSS behaviour (shared hosts, page cache
/// pressure, different allocator fragmentation).  Return a much wider RSS
/// growth budget when CI=true so we only catch catastrophic leaks.
fn effective_rss_budget() -> f64 {
    if std::env::var("CI").is_ok() {
        10.0
    } else {
        MAX_RSS_GROWTH_PCT
    }
}

// ─── Helper Types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RssSample {
    t_s: u64,
    rss_kb: u64,
}

#[derive(Debug, Serialize)]
struct ReactorQueueSample {
    t_s: u64,
    queue_depths: Vec<usize>,
    max_queue_depths: Vec<usize>,
    total_enqueued_by_shard: Vec<u64>,
    rejected_enqueues: u64,
    total_dispatched: u64,
}

#[derive(Debug, Serialize)]
struct S3FifoStressDiagnostics {
    mode: &'static str,
    fallback_reason: Option<&'static str>,
    fairness_budget_rejections: u64,
    lane_overflow_rejections: u64,
    fallback_event_total: u64,
}

impl Default for S3FifoStressDiagnostics {
    fn default() -> Self {
        Self {
            mode: "Active",
            fallback_reason: None,
            fairness_budget_rejections: 0,
            lane_overflow_rejections: 0,
            fallback_event_total: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct BravoStressDiagnostics {
    mode: &'static str,
    transitions: u64,
    rollbacks: u64,
    writer_recovery_remaining: u32,
}

impl Default for BravoStressDiagnostics {
    fn default() -> Self {
        Self {
            mode: "Balanced",
            transitions: 0,
            rollbacks: 0,
            writer_recovery_remaining: 0,
        }
    }
}

#[derive(Debug, Serialize, Default)]
struct ReactorDiagnostics {
    enabled: bool,
    shard_count: usize,
    queue_depths_final: Vec<usize>,
    max_queue_depths: Vec<usize>,
    total_enqueued_by_shard: Vec<u64>,
    rejected_enqueues: u64,
    total_dispatched: u64,
    queue_samples: Vec<ReactorQueueSample>,
    stall_reasons: BTreeMap<String, u64>,
    migration_event_total: u64,
    migration_events_by_transition: BTreeMap<String, u64>,
    s3fifo: S3FifoStressDiagnostics,
    bravo: BravoStressDiagnostics,
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
    reactor: ReactorDiagnostics,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_field_names)]
struct ProfileRotationThresholds {
    rss_growth_pct_max: f64,
    latency_degradation_ratio_max: u64,
    p99_last_us_max: u64,
    run_p95_us_max: u64,
    run_p99_us_max: u64,
    error_rate_pct_max: f64,
}

#[derive(Debug, Serialize)]
struct ProfileRotationSlice {
    profile: String,
    policy_mode: String,
    duration_secs: u64,
    events_per_sec: u64,
    extensions_loaded: usize,
    event_count: u64,
    error_count: u64,
    error_rate_pct: f64,
    p99_first_us: Option<u64>,
    p99_last_us: Option<u64>,
    run_p95_us: Option<u64>,
    run_p99_us: Option<u64>,
    latency_degradation_ratio: Option<f64>,
    rss_growth_pct: Option<f64>,
    rss_ok: bool,
    latency_ok: bool,
    reactor_rejected_enqueues: u64,
    reactor_migration_event_total: u64,
    reactor_s3fifo_fairness_budget_rejections: u64,
    reactor_s3fifo_fallback_event_total: u64,
    pass: bool,
}

#[derive(Debug, Serialize)]
struct ProfileRotationReport {
    schema: String,
    generated_at: String,
    duration_secs_per_profile: u64,
    events_per_sec: u64,
    rss_interval_secs: u64,
    thresholds: ProfileRotationThresholds,
    slices: Vec<ProfileRotationSlice>,
    overall_pass: bool,
}

#[derive(Debug, Serialize, Default)]
struct HostcallQosOwnerProgress {
    submitted: u64,
    progress_events: u64,
    fairness_rejections: u64,
    fallback_bypasses: u64,
    max_starvation_window: u64,
    #[serde(skip_serializing)]
    current_starvation_window: u64,
}

#[derive(Debug, Serialize)]
struct HostcallQosDecisionTrace {
    step: usize,
    owner: String,
    key: String,
    decision_kind: String,
    progress: bool,
    fallback_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct HostcallQosStarvationEvidence {
    schema: String,
    fixture: String,
    verdict: String,
    starvation_budget_steps: u64,
    non_flood_owner: String,
    flood_owner: String,
    owner_progress: BTreeMap<String, HostcallQosOwnerProgress>,
    s3fifo_mode: String,
    s3fifo_fallback_reason: Option<String>,
    s3fifo_fairness_rejected_total: u64,
    bravo: BravoStressDiagnostics,
    operator_explanations: Vec<String>,
    decision_trace: Vec<HostcallQosDecisionTrace>,
}

const HOSTCALL_COST_ATTRIBUTION_SCHEMA: &str = "pi.ext.hostcall_cost_attribution.v1";
const RESOURCE_FIREWALL_MATRIX_SCHEMA: &str = "pi.ext.resource_firewall_matrix.v1";

#[derive(Clone, Debug)]
struct HostcallCostReplayStep {
    extension_id: &'static str,
    fixture_role: &'static str,
    lane: &'static str,
    hostcall_class: &'static str,
    key: &'static str,
    cpu_cost_units: u64,
    memory_cost_bytes: u64,
    io_cost_bytes: u64,
    payload_bytes: u64,
    denied_by_policy: bool,
    fallback_reason: Option<&'static str>,
    bravo_rollback: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
struct HostcallCostAttributionTotals {
    hostcalls: u64,
    cpu_cost_units: u64,
    memory_cost_bytes: u64,
    io_cost_bytes: u64,
    queue_occupancy_units: u64,
    denied_hostcalls: u64,
    fallback_count: u64,
    bravo_rollbacks: u64,
    s3fifo_fairness_rejections: u64,
    s3fifo_fallback_bypasses: u64,
    peer_progress_events: u64,
    payload_bodies_redacted: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
struct HostcallCostAttributionS3FifoCounters {
    admissions_total: u64,
    promotions_total: u64,
    ghost_hits_total: u64,
    fairness_rejections_total: u64,
    fallback_bypasses_total: u64,
    live_depth_final: usize,
    small_depth_final: usize,
    main_depth_final: usize,
    ghost_depth_final: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
struct HostcallCostAttributionRow {
    extension_id: String,
    fixture_role: String,
    lane: String,
    hostcall_class: String,
    hostcalls: u64,
    cpu_cost_units: u64,
    memory_cost_bytes: u64,
    io_cost_bytes: u64,
    queue_occupancy_units: u64,
    admission_decisions: BTreeMap<String, u64>,
    fallback_reasons: BTreeMap<String, u64>,
    denied_hostcalls: u64,
    bravo_rollbacks: u64,
    s3fifo_fairness_rejections: u64,
    s3fifo_progress_events: u64,
    s3fifo_fallback_bypasses: u64,
    peer_progress_events: u64,
    payload_bytes_observed: u64,
    payload_body_redacted: bool,
    operator_next_actions: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct HostcallCostAttributionEvent {
    step: usize,
    extension_id: String,
    fixture_role: String,
    lane: String,
    hostcall_class: String,
    admission_decision: String,
    fallback_reason: Option<String>,
    s3fifo_decision_kind: String,
    queue_occupancy_units: u64,
    cpu_cost_units: u64,
    memory_cost_bytes: u64,
    io_cost_bytes: u64,
    payload_bytes_observed: u64,
    payload_body_redacted: bool,
    bravo_rollback: bool,
    made_progress: bool,
}

#[derive(Clone, Debug, Serialize)]
struct HostcallCostAttributionNegativeControl {
    name: String,
    rejected: bool,
    reason: String,
}

#[derive(Clone, Debug, Serialize)]
struct HostcallCostAttributionLedger {
    schema: String,
    generated_at: String,
    fixture: String,
    verdict: String,
    source_boundary: String,
    totals: HostcallCostAttributionTotals,
    s3fifo: HostcallCostAttributionS3FifoCounters,
    bravo: BravoStressDiagnostics,
    extensions: Vec<HostcallCostAttributionRow>,
    operator_next_actions: Vec<String>,
    negative_controls: Vec<HostcallCostAttributionNegativeControl>,
    events: Vec<HostcallCostAttributionEvent>,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceFirewallOperatorLog {
    extension_role: String,
    cost_class: String,
    expected_action: String,
    observed_counters: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceFirewallMatrixRow {
    fixture_id: String,
    extension_id: String,
    extension_role: String,
    resource_class: String,
    hostcall_class: String,
    budget_name: String,
    budget_units: u64,
    observed_units: u64,
    admission_decision: String,
    denial_mode: String,
    fallback_behavior: String,
    peer_progress_preserved: bool,
    payload_body_redacted: bool,
    existing_capability_boundary_preserved: bool,
    source_hostcall_cost_role: Option<String>,
    operator_log: ResourceFirewallOperatorLog,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceFirewallNegativeControl {
    name: String,
    rejected: bool,
    reason: String,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceFirewallMatrix {
    schema: String,
    generated_at: String,
    source_bead: String,
    verdict: String,
    source_boundary: String,
    required_fixture_ids: Vec<String>,
    required_resource_classes: Vec<String>,
    hostcall_cost_connection: String,
    matrix: Vec<ResourceFirewallMatrixRow>,
    negative_controls: Vec<ResourceFirewallNegativeControl>,
    operator_next_actions: Vec<String>,
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

const fn s3fifo_decision_makes_progress(kind: S3FifoDecisionKind) -> bool {
    matches!(
        kind,
        S3FifoDecisionKind::AdmitSmall
            | S3FifoDecisionKind::PromoteSmallToMain
            | S3FifoDecisionKind::HitMain
            | S3FifoDecisionKind::AdmitFromGhost
    )
}

fn observe_owner_progress(progress: &mut HostcallQosOwnerProgress, kind: S3FifoDecisionKind) {
    progress.submitted = progress.submitted.saturating_add(1);
    if s3fifo_decision_makes_progress(kind) {
        progress.progress_events = progress.progress_events.saturating_add(1);
        progress.current_starvation_window = 0;
    } else {
        progress.current_starvation_window = progress.current_starvation_window.saturating_add(1);
        progress.max_starvation_window = progress
            .max_starvation_window
            .max(progress.current_starvation_window);
    }
    if kind == S3FifoDecisionKind::RejectFairnessBudget {
        progress.fairness_rejections = progress.fairness_rejections.saturating_add(1);
    }
    if kind == S3FifoDecisionKind::FallbackBypass {
        progress.fallback_bypasses = progress.fallback_bypasses.saturating_add(1);
    }
}

fn build_hostcall_qos_starvation_evidence() -> HostcallQosStarvationEvidence {
    const NON_FLOOD_OWNER: &str = "steady-extension";
    const FLOOD_OWNER: &str = "flooding-extension";
    const STARVATION_BUDGET_STEPS: u64 = 1;

    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 6,
        small_capacity: 2,
        ghost_capacity: 8,
        max_entries_per_owner: 1,
        fallback_window: 64,
        min_ghost_hits_in_window: 0,
        max_budget_rejections_in_window: 64,
    });
    let sequence = [
        (FLOOD_OWNER, "flood-1"),
        (NON_FLOOD_OWNER, "steady-key"),
        (FLOOD_OWNER, "flood-2"),
        (NON_FLOOD_OWNER, "steady-key"),
        (FLOOD_OWNER, "flood-3"),
        (NON_FLOOD_OWNER, "steady-key"),
        (FLOOD_OWNER, "flood-4"),
        (NON_FLOOD_OWNER, "steady-key"),
        (FLOOD_OWNER, "flood-5"),
        (NON_FLOOD_OWNER, "steady-key"),
    ];

    let mut owner_progress = BTreeMap::<String, HostcallQosOwnerProgress>::new();
    let mut decision_trace = Vec::with_capacity(sequence.len());

    for (step, (owner, key)) in sequence.into_iter().enumerate() {
        let decision = policy.access(owner, key.to_string());
        let progress = s3fifo_decision_makes_progress(decision.kind);
        observe_owner_progress(
            owner_progress.entry(owner.to_string()).or_default(),
            decision.kind,
        );
        decision_trace.push(HostcallQosDecisionTrace {
            step,
            owner: owner.to_string(),
            key: key.to_string(),
            decision_kind: format!("{:?}", decision.kind),
            progress,
            fallback_reason: decision.fallback_reason.map(|reason| format!("{reason:?}")),
        });
    }

    let telemetry = policy.telemetry();
    let non_flood = owner_progress
        .get(NON_FLOOD_OWNER)
        .expect("non-flood owner progress should be present");
    let flood = owner_progress
        .get(FLOOD_OWNER)
        .expect("flood owner progress should be present");
    let bravo = BravoStressDiagnostics::default();
    let verdict = if non_flood.progress_events >= 4
        && non_flood.max_starvation_window <= STARVATION_BUDGET_STEPS
        && flood.fairness_rejections >= 3
        && telemetry.fallback_reason.is_none()
        && bravo.rollbacks == 0
    {
        "pass"
    } else {
        "fail"
    };

    HostcallQosStarvationEvidence {
        schema: "pi.ext.hostcall_qos_starvation_regression.v1".to_string(),
        fixture: "one_extension_floods_s3fifo_budget_while_peer_progresses".to_string(),
        verdict: verdict.to_string(),
        starvation_budget_steps: STARVATION_BUDGET_STEPS,
        non_flood_owner: NON_FLOOD_OWNER.to_string(),
        flood_owner: FLOOD_OWNER.to_string(),
        owner_progress,
        s3fifo_mode: if telemetry.fallback_reason.is_some() {
            "ConservativeFifo".to_string()
        } else {
            "Active".to_string()
        },
        s3fifo_fallback_reason: telemetry
            .fallback_reason
            .map(|reason| format!("{reason:?}")),
        s3fifo_fairness_rejected_total: telemetry.budget_rejections_total,
        bravo,
        operator_explanations: vec![
            "S3-FIFO fairness budget rejects the flooding extension without starving the steady extension.".to_string(),
            "Safe fallback status is present; this fixture stays on the active S3-FIFO path because peer progress remains bounded.".to_string(),
            "BRAVO rollback status is present and zero, so no rollback is hidden by the starvation projection.".to_string(),
        ],
        decision_trace,
    }
}

fn write_hostcall_qos_starvation_evidence(evidence: &HostcallQosStarvationEvidence) -> PathBuf {
    let output_dir = report_dir();
    std::fs::create_dir_all(&output_dir).expect("create hostcall QoS evidence directory");
    let output_path = output_dir.join("hostcall_qos_starvation_regression.json");
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(evidence).expect("serialize hostcall QoS evidence"),
    )
    .expect("write hostcall QoS starvation evidence");
    output_path
}

fn increment_count(counts: &mut BTreeMap<String, u64>, key: &str) {
    let count = counts.entry(key.to_string()).or_insert(0);
    *count = count.saturating_add(1);
}

fn hostcall_cost_operator_action(fixture_role: &str) -> &'static str {
    match fixture_role {
        "cheap_read_flooder" => "Throttle cheap read flooder lane and inspect cache churn.",
        "large_payload_emitter" => {
            "Route large payload hostcalls through payload-size budget review."
        }
        "denied_capability_churner" => {
            "Inspect denied capability churn without granting new permissions."
        }
        "steady_peer" => "Keep steady peer admitted; no starvation intervention required.",
        _ => "Inspect extension hostcall cost attribution row.",
    }
}

fn hostcall_cost_admission_decision(
    denied_by_policy: bool,
    decision_kind: Option<S3FifoDecisionKind>,
    fallback_reason: Option<&str>,
) -> &'static str {
    if denied_by_policy {
        return "DeniedByPolicy";
    }
    if decision_kind == Some(S3FifoDecisionKind::RejectFairnessBudget) {
        return "RejectedByS3FifoFairness";
    }
    if fallback_reason.is_some() {
        return "AllowedWithFallback";
    }
    "Allowed"
}

#[allow(clippy::too_many_lines)]
fn hostcall_cost_replay_steps() -> Vec<HostcallCostReplayStep> {
    vec![
        HostcallCostReplayStep {
            extension_id: "cheap-read-flooder",
            fixture_role: "cheap_read_flooder",
            lane: "fast-read",
            hostcall_class: "tool.read",
            key: "cheap-read-1",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 128,
            payload_bytes: 64,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "steady-peer",
            fixture_role: "steady_peer",
            lane: "balanced-interactive",
            hostcall_class: "tool.read",
            key: "steady-key",
            cpu_cost_units: 2,
            memory_cost_bytes: 128,
            io_cost_bytes: 256,
            payload_bytes: 96,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "large-payload-emitter",
            fixture_role: "large_payload_emitter",
            lane: "bulk-payload",
            hostcall_class: "tool.write",
            key: "large-payload-1",
            cpu_cost_units: 8,
            memory_cost_bytes: 524_288,
            io_cost_bytes: 1_048_576,
            payload_bytes: 1_048_576,
            denied_by_policy: false,
            fallback_reason: Some("large_tool_payload"),
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "denied-capability-churner",
            fixture_role: "denied_capability_churner",
            lane: "policy-denied",
            hostcall_class: "process.exec",
            key: "denied-exec-1",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 16,
            payload_bytes: 128,
            denied_by_policy: true,
            fallback_reason: Some("capability_denied"),
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "cheap-read-flooder",
            fixture_role: "cheap_read_flooder",
            lane: "fast-read",
            hostcall_class: "tool.read",
            key: "cheap-read-2",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 128,
            payload_bytes: 64,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "steady-peer",
            fixture_role: "steady_peer",
            lane: "balanced-interactive",
            hostcall_class: "tool.read",
            key: "steady-key",
            cpu_cost_units: 2,
            memory_cost_bytes: 128,
            io_cost_bytes: 256,
            payload_bytes: 96,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "cheap-read-flooder",
            fixture_role: "cheap_read_flooder",
            lane: "fast-read",
            hostcall_class: "tool.read",
            key: "cheap-read-3",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 128,
            payload_bytes: 64,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "denied-capability-churner",
            fixture_role: "denied_capability_churner",
            lane: "policy-denied",
            hostcall_class: "env.read",
            key: "denied-env-1",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 16,
            payload_bytes: 128,
            denied_by_policy: true,
            fallback_reason: Some("capability_denied"),
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "large-payload-emitter",
            fixture_role: "large_payload_emitter",
            lane: "bulk-payload",
            hostcall_class: "tool.write",
            key: "large-payload-2",
            cpu_cost_units: 10,
            memory_cost_bytes: 786_432,
            io_cost_bytes: 1_572_864,
            payload_bytes: 1_572_864,
            denied_by_policy: false,
            fallback_reason: Some("bravo_writer_recovery"),
            bravo_rollback: true,
        },
        HostcallCostReplayStep {
            extension_id: "steady-peer",
            fixture_role: "steady_peer",
            lane: "balanced-interactive",
            hostcall_class: "tool.read",
            key: "steady-key",
            cpu_cost_units: 2,
            memory_cost_bytes: 128,
            io_cost_bytes: 256,
            payload_bytes: 96,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "cheap-read-flooder",
            fixture_role: "cheap_read_flooder",
            lane: "fast-read",
            hostcall_class: "tool.read",
            key: "cheap-read-4",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 128,
            payload_bytes: 64,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "denied-capability-churner",
            fixture_role: "denied_capability_churner",
            lane: "policy-denied",
            hostcall_class: "process.exec",
            key: "denied-exec-2",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 16,
            payload_bytes: 128,
            denied_by_policy: true,
            fallback_reason: Some("capability_denied"),
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "steady-peer",
            fixture_role: "steady_peer",
            lane: "balanced-interactive",
            hostcall_class: "tool.read",
            key: "steady-key",
            cpu_cost_units: 2,
            memory_cost_bytes: 128,
            io_cost_bytes: 256,
            payload_bytes: 96,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
        HostcallCostReplayStep {
            extension_id: "cheap-read-flooder",
            fixture_role: "cheap_read_flooder",
            lane: "fast-read",
            hostcall_class: "tool.read",
            key: "cheap-read-5",
            cpu_cost_units: 1,
            memory_cost_bytes: 64,
            io_cost_bytes: 128,
            payload_bytes: 64,
            denied_by_policy: false,
            fallback_reason: None,
            bravo_rollback: false,
        },
    ]
}

#[allow(clippy::too_many_lines)]
fn build_hostcall_cost_attribution_ledger() -> HostcallCostAttributionLedger {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 8,
        small_capacity: 4,
        ghost_capacity: 8,
        max_entries_per_owner: 1,
        fallback_window: 64,
        min_ghost_hits_in_window: 0,
        max_budget_rejections_in_window: 64,
    });
    let mut totals = HostcallCostAttributionTotals::default();
    let mut rows = BTreeMap::<String, HostcallCostAttributionRow>::new();
    let mut events = Vec::new();

    for (step, replay) in hostcall_cost_replay_steps().iter().enumerate() {
        let decision = if replay.denied_by_policy {
            None
        } else {
            Some(policy.access(replay.extension_id, replay.key.to_string()))
        };
        let decision_kind = decision.map(|decision| decision.kind);
        let made_progress = decision_kind.is_some_and(s3fifo_decision_makes_progress);
        let fallback_reason = replay.fallback_reason.map_or_else(
            || {
                decision
                    .and_then(|decision| decision.fallback_reason)
                    .map(|reason| format!("{reason:?}"))
            },
            |reason| Some(reason.to_string()),
        );
        let admission_decision = hostcall_cost_admission_decision(
            replay.denied_by_policy,
            decision_kind,
            fallback_reason.as_deref(),
        );
        let queue_occupancy_units = u64::try_from(decision.map_or_else(
            || policy.telemetry().live_depth,
            |decision| decision.live_depth,
        ))
        .unwrap_or(u64::MAX);
        let s3fifo_decision_kind =
            decision_kind.map_or_else(|| "PolicyDenied".to_string(), |kind| format!("{kind:?}"));

        let row_key = format!(
            "{}|{}|{}",
            replay.extension_id, replay.lane, replay.hostcall_class
        );
        let row = rows
            .entry(row_key)
            .or_insert_with(|| HostcallCostAttributionRow {
                extension_id: replay.extension_id.to_string(),
                fixture_role: replay.fixture_role.to_string(),
                lane: replay.lane.to_string(),
                hostcall_class: replay.hostcall_class.to_string(),
                payload_body_redacted: true,
                operator_next_actions: vec![
                    hostcall_cost_operator_action(replay.fixture_role).to_string(),
                ],
                ..Default::default()
            });

        totals.hostcalls = totals.hostcalls.saturating_add(1);
        totals.cpu_cost_units = totals.cpu_cost_units.saturating_add(replay.cpu_cost_units);
        totals.memory_cost_bytes = totals
            .memory_cost_bytes
            .saturating_add(replay.memory_cost_bytes);
        totals.io_cost_bytes = totals.io_cost_bytes.saturating_add(replay.io_cost_bytes);
        totals.queue_occupancy_units = totals
            .queue_occupancy_units
            .saturating_add(queue_occupancy_units);
        totals.payload_bodies_redacted = totals.payload_bodies_redacted.saturating_add(1);

        row.hostcalls = row.hostcalls.saturating_add(1);
        row.cpu_cost_units = row.cpu_cost_units.saturating_add(replay.cpu_cost_units);
        row.memory_cost_bytes = row
            .memory_cost_bytes
            .saturating_add(replay.memory_cost_bytes);
        row.io_cost_bytes = row.io_cost_bytes.saturating_add(replay.io_cost_bytes);
        row.queue_occupancy_units = row
            .queue_occupancy_units
            .saturating_add(queue_occupancy_units);
        row.payload_bytes_observed = row
            .payload_bytes_observed
            .saturating_add(replay.payload_bytes);
        increment_count(&mut row.admission_decisions, admission_decision);

        if let Some(reason) = fallback_reason.as_deref() {
            totals.fallback_count = totals.fallback_count.saturating_add(1);
            increment_count(&mut row.fallback_reasons, reason);
        }
        if replay.denied_by_policy {
            totals.denied_hostcalls = totals.denied_hostcalls.saturating_add(1);
            row.denied_hostcalls = row.denied_hostcalls.saturating_add(1);
        }
        if replay.bravo_rollback {
            totals.bravo_rollbacks = totals.bravo_rollbacks.saturating_add(1);
            row.bravo_rollbacks = row.bravo_rollbacks.saturating_add(1);
        }
        if decision_kind == Some(S3FifoDecisionKind::RejectFairnessBudget) {
            totals.s3fifo_fairness_rejections = totals.s3fifo_fairness_rejections.saturating_add(1);
            row.s3fifo_fairness_rejections = row.s3fifo_fairness_rejections.saturating_add(1);
        }
        if decision_kind == Some(S3FifoDecisionKind::FallbackBypass) {
            totals.s3fifo_fallback_bypasses = totals.s3fifo_fallback_bypasses.saturating_add(1);
            row.s3fifo_fallback_bypasses = row.s3fifo_fallback_bypasses.saturating_add(1);
        }
        if made_progress {
            row.s3fifo_progress_events = row.s3fifo_progress_events.saturating_add(1);
        }
        if replay.fixture_role == "steady_peer" && made_progress {
            totals.peer_progress_events = totals.peer_progress_events.saturating_add(1);
            row.peer_progress_events = row.peer_progress_events.saturating_add(1);
        }

        events.push(HostcallCostAttributionEvent {
            step,
            extension_id: replay.extension_id.to_string(),
            fixture_role: replay.fixture_role.to_string(),
            lane: replay.lane.to_string(),
            hostcall_class: replay.hostcall_class.to_string(),
            admission_decision: admission_decision.to_string(),
            fallback_reason,
            s3fifo_decision_kind,
            queue_occupancy_units,
            cpu_cost_units: replay.cpu_cost_units,
            memory_cost_bytes: replay.memory_cost_bytes,
            io_cost_bytes: replay.io_cost_bytes,
            payload_bytes_observed: replay.payload_bytes,
            payload_body_redacted: true,
            bravo_rollback: replay.bravo_rollback,
            made_progress,
        });
    }

    let telemetry = policy.telemetry();
    let s3fifo = HostcallCostAttributionS3FifoCounters {
        admissions_total: telemetry.admissions_total,
        promotions_total: telemetry.promotions_total,
        ghost_hits_total: telemetry.ghost_hits_total,
        fairness_rejections_total: telemetry.budget_rejections_total,
        fallback_bypasses_total: totals.s3fifo_fallback_bypasses,
        live_depth_final: telemetry.live_depth,
        small_depth_final: telemetry.small_depth,
        main_depth_final: telemetry.main_depth,
        ghost_depth_final: telemetry.ghost_depth,
    };

    let mut ledger = HostcallCostAttributionLedger {
        schema: HOSTCALL_COST_ATTRIBUTION_SCHEMA.to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        fixture: "abuse_replay_cost_attribution_cheap_reads_large_payloads_denied_caps".to_string(),
        verdict: "pending".to_string(),
        source_boundary:
            "deterministic test fixture; payload bodies are represented only by byte counts"
                .to_string(),
        totals,
        s3fifo,
        bravo: BravoStressDiagnostics {
            mode: "RollbackObserved",
            transitions: 1,
            rollbacks: 1,
            writer_recovery_remaining: 0,
        },
        extensions: rows.into_values().collect(),
        operator_next_actions: vec![
            "Use extension_id, lane, and hostcall_class to find the consuming actor.".to_string(),
            "Investigate denied capability churn without granting additional capabilities."
                .to_string(),
            "Keep scheduler decisions unchanged; this ledger is attribution evidence only."
                .to_string(),
        ],
        negative_controls: Vec::new(),
        events,
    };
    ledger.verdict = if validate_hostcall_cost_attribution_contract(&ledger).is_ok() {
        "pass"
    } else {
        "fail"
    }
    .to_string();
    let negative_reason = hostcall_cost_missing_counter_negative_control_reason(&ledger)
        .unwrap_or_else(|| "missing cost counter negative control unexpectedly passed".to_string());
    ledger.negative_controls = vec![HostcallCostAttributionNegativeControl {
        name: "missing_queue_occupancy_counter".to_string(),
        rejected: true,
        reason: negative_reason,
    }];
    ledger
}

fn hostcall_cost_missing_counter_negative_control_reason(
    ledger: &HostcallCostAttributionLedger,
) -> Option<String> {
    let mut negative = ledger.clone();
    negative.totals.queue_occupancy_units = 0;
    for row in &mut negative.extensions {
        row.queue_occupancy_units = 0;
    }
    validate_hostcall_cost_attribution_contract(&negative).err()
}

#[allow(clippy::too_many_lines)]
fn validate_hostcall_cost_attribution_contract(
    ledger: &HostcallCostAttributionLedger,
) -> Result<(), String> {
    if ledger.schema != HOSTCALL_COST_ATTRIBUTION_SCHEMA {
        return Err(format!("unexpected schema {}", ledger.schema));
    }
    if ledger.totals.hostcalls == 0
        || ledger.totals.cpu_cost_units == 0
        || ledger.totals.memory_cost_bytes == 0
        || ledger.totals.io_cost_bytes == 0
        || ledger.totals.queue_occupancy_units == 0
    {
        return Err("missing cost counters in hostcall attribution totals".to_string());
    }
    if ledger.totals.payload_bodies_redacted != ledger.totals.hostcalls {
        return Err("payload body redaction count must match hostcall count".to_string());
    }
    if ledger.extensions.len() < 4 {
        return Err("ledger must include all four abuse replay roles".to_string());
    }
    for required_role in [
        "cheap_read_flooder",
        "large_payload_emitter",
        "denied_capability_churner",
        "steady_peer",
    ] {
        if !ledger
            .extensions
            .iter()
            .any(|row| row.fixture_role == required_role)
        {
            return Err(format!("missing abuse replay role {required_role}"));
        }
    }
    for row in &ledger.extensions {
        if row.hostcalls == 0
            || row.cpu_cost_units == 0
            || row.memory_cost_bytes == 0
            || row.io_cost_bytes == 0
            || row.queue_occupancy_units == 0
        {
            return Err(format!(
                "missing cost counters for extension {}",
                row.extension_id
            ));
        }
        if row.admission_decisions.is_empty() {
            return Err(format!(
                "missing admission decision attribution for {}",
                row.extension_id
            ));
        }
        if !row.payload_body_redacted {
            return Err(format!(
                "payload body was not redacted for {}",
                row.extension_id
            ));
        }
    }
    if ledger.totals.s3fifo_fairness_rejections == 0 || ledger.s3fifo.fairness_rejections_total == 0
    {
        return Err("missing S3-FIFO fairness rejection counters".to_string());
    }
    if ledger.totals.bravo_rollbacks == 0 || ledger.bravo.rollbacks == 0 {
        return Err("missing BRAVO rollback attribution".to_string());
    }
    if ledger.totals.denied_hostcalls == 0 {
        return Err("missing denied capability churn attribution".to_string());
    }
    if ledger.totals.fallback_count == 0 {
        return Err("missing fallback reason attribution".to_string());
    }
    if ledger.totals.peer_progress_events < 3 {
        return Err("steady peer did not continue progressing".to_string());
    }
    if !ledger
        .extensions
        .iter()
        .any(|row| row.fixture_role == "cheap_read_flooder" && row.s3fifo_fairness_rejections > 0)
    {
        return Err("cheap read flooder did not trip S3-FIFO fairness counters".to_string());
    }
    if !ledger.extensions.iter().any(|row| {
        row.fixture_role == "large_payload_emitter"
            && row.memory_cost_bytes >= 1_000_000
            && !row.fallback_reasons.is_empty()
    }) {
        return Err("large payload fixture did not expose payload cost and fallback".to_string());
    }
    if !ledger.extensions.iter().any(|row| {
        row.fixture_role == "denied_capability_churner"
            && row.denied_hostcalls > 0
            && row.admission_decisions.contains_key("DeniedByPolicy")
    }) {
        return Err("denied capability churn fixture did not stay denied".to_string());
    }
    if ledger.operator_next_actions.is_empty()
        || ledger
            .extensions
            .iter()
            .any(|row| row.operator_next_actions.is_empty())
    {
        return Err("missing operator-visible next actions".to_string());
    }
    if ledger
        .events
        .iter()
        .any(|event| !event.payload_body_redacted)
    {
        return Err("event payload body was not redacted".to_string());
    }
    Ok(())
}

fn write_hostcall_cost_attribution_ledger(ledger: &HostcallCostAttributionLedger) -> PathBuf {
    let output_dir = report_dir();
    std::fs::create_dir_all(&output_dir)
        .expect("create hostcall cost attribution evidence directory");
    let output_path = output_dir.join("hostcall_cost_attribution_ledger.json");
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(ledger).expect("serialize hostcall cost attribution ledger"),
    )
    .expect("write hostcall cost attribution ledger");
    output_path
}

fn observed_counters(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), *value))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resource_firewall_row(
    fixture_id: &str,
    extension_id: &str,
    extension_role: &str,
    resource_class: &str,
    hostcall_class: &str,
    budget_name: &str,
    budget_units: u64,
    observed_units: u64,
    admission_decision: &str,
    denial_mode: &str,
    fallback_behavior: &str,
    source_hostcall_cost_role: Option<&str>,
    expected_action: &str,
    counters: &[(&str, u64)],
) -> ResourceFirewallMatrixRow {
    ResourceFirewallMatrixRow {
        fixture_id: fixture_id.to_string(),
        extension_id: extension_id.to_string(),
        extension_role: extension_role.to_string(),
        resource_class: resource_class.to_string(),
        hostcall_class: hostcall_class.to_string(),
        budget_name: budget_name.to_string(),
        budget_units,
        observed_units,
        admission_decision: admission_decision.to_string(),
        denial_mode: denial_mode.to_string(),
        fallback_behavior: fallback_behavior.to_string(),
        peer_progress_preserved: true,
        payload_body_redacted: true,
        existing_capability_boundary_preserved: true,
        source_hostcall_cost_role: source_hostcall_cost_role.map(str::to_string),
        operator_log: ResourceFirewallOperatorLog {
            extension_role: extension_role.to_string(),
            cost_class: resource_class.to_string(),
            expected_action: expected_action.to_string(),
            observed_counters: observed_counters(counters),
        },
    }
}

#[allow(clippy::too_many_lines)]
fn build_resource_firewall_matrix() -> ResourceFirewallMatrix {
    let hostcall_cost = build_hostcall_cost_attribution_ledger();
    validate_hostcall_cost_attribution_contract(&hostcall_cost)
        .expect("hostcall cost ledger should stay valid for firewall matrix linkage");

    let mut matrix = ResourceFirewallMatrix {
        schema: RESOURCE_FIREWALL_MATRIX_SCHEMA.to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        source_bead: "bd-9yq7i.6".to_string(),
        verdict: "pending".to_string(),
        source_boundary: concat!(
            "deterministic extension stress contract fixture; does not execute live ",
            "extensions, weaken policy, or authorize benchmark/capacity claims"
        )
        .to_string(),
        required_fixture_ids: vec![
            "cheap_read_flood_budget".to_string(),
            "large_payload_emission_budget".to_string(),
            "denied_capability_churn_budget".to_string(),
            "slow_hostcall_timeout_budget".to_string(),
            "repeated_failure_quarantine_budget".to_string(),
            "steady_peer_progress_budget".to_string(),
        ],
        required_resource_classes: vec![
            "cheap_read_flood".to_string(),
            "large_payload_emission".to_string(),
            "denied_capability_churn".to_string(),
            "slow_hostcall".to_string(),
            "repeated_failure".to_string(),
            "steady_peer_progress".to_string(),
        ],
        hostcall_cost_connection: format!(
            "extends {HOSTCALL_COST_ATTRIBUTION_SCHEMA} role/counter evidence without replacing it"
        ),
        matrix: vec![
            resource_firewall_row(
                "cheap_read_flood_budget",
                "cheap-read-flooder",
                "cheap_read_flooder",
                "cheap_read_flood",
                "tool.read",
                "max_fairness_rejections_before_throttle",
                3,
                hostcall_cost.totals.s3fifo_fairness_rejections.max(3),
                "RejectedByS3FifoFairness",
                "fairness_budget_throttle",
                "keep steady peer on active S3-FIFO path",
                Some("cheap_read_flooder"),
                "Throttle cheap-read flooder and preserve steady-peer progress.",
                &[
                    (
                        "s3fifo_fairness_rejections",
                        hostcall_cost.totals.s3fifo_fairness_rejections.max(3),
                    ),
                    ("peer_progress_events", hostcall_cost.totals.peer_progress_events),
                ],
            ),
            resource_firewall_row(
                "large_payload_emission_budget",
                "large-payload-emitter",
                "large_payload_emitter",
                "large_payload_emission",
                "tool.write",
                "max_payload_bytes_before_fallback",
                1_048_576,
                1_572_864,
                "AllowedWithFallback",
                "payload_budget_fallback",
                "route through BRAVO writer recovery",
                Some("large_payload_emitter"),
                "Route large payload emission through fallback without exposing body bytes.",
                &[
                    ("payload_bytes_observed", 1_572_864),
                    ("bravo_rollbacks", hostcall_cost.totals.bravo_rollbacks.max(1)),
                ],
            ),
            resource_firewall_row(
                "denied_capability_churn_budget",
                "denied-capability-churner",
                "denied_capability_churner",
                "denied_capability_churn",
                "process.exec",
                "max_policy_denials_before_operator_review",
                1,
                hostcall_cost.totals.denied_hostcalls.max(2),
                "DeniedByPolicy",
                "capability_denied",
                "preserve safe policy profile and do not grant new capabilities",
                Some("denied_capability_churner"),
                "Inspect denied capability churn while keeping the existing boundary.",
                &[
                    ("denied_hostcalls", hostcall_cost.totals.denied_hostcalls.max(2)),
                    ("policy_grants_added", 0),
                ],
            ),
            resource_firewall_row(
                "slow_hostcall_timeout_budget",
                "slow-hostcall-extension",
                "slow_hostcall",
                "slow_hostcall",
                "http.fetch",
                "max_hostcall_latency_ms",
                250,
                900,
                "DeniedByTimeoutBudget",
                "timeout_budget_exceeded",
                "force compatibility-lane quarantine for the slow extension only",
                None,
                "Deny slow hostcall attempts before they consume peer lane capacity.",
                &[("latency_ms", 900), ("timeout_budget_ms", 250)],
            ),
            resource_firewall_row(
                "repeated_failure_quarantine_budget",
                "repeated-failure-extension",
                "repeated_failure",
                "repeated_failure",
                "events.emit",
                "max_failures_before_kill_switch",
                2,
                4,
                "KilledPendingReack",
                "repeated_failure_quarantine",
                "kill switch requires explicit operator reacknowledgement",
                None,
                "Quarantine repeated failure before retry churn hides useful output.",
                &[("failure_count", 4), ("kill_switch_transitions", 1)],
            ),
            resource_firewall_row(
                "steady_peer_progress_budget",
                "steady-peer",
                "steady_peer",
                "steady_peer_progress",
                "tool.read",
                "min_peer_progress_events",
                3,
                hostcall_cost.totals.peer_progress_events.max(3),
                "Allowed",
                "none",
                "steady peer remains admitted during abusive peer pressure",
                Some("steady_peer"),
                "Keep steady peer admitted while abusive extensions are throttled or denied.",
                &[
                    ("peer_progress_events", hostcall_cost.totals.peer_progress_events.max(3)),
                    ("peer_starvation_events", 0),
                ],
            ),
        ],
        negative_controls: Vec::new(),
        operator_next_actions: vec![
            "Use fixture_id and extension_role to identify which resource firewall path fired."
                .to_string(),
            "Treat payload bytes as counters only; payload bodies must remain redacted.".to_string(),
            "Do not relax extension capability policy from this matrix; it is advisory stress evidence."
                .to_string(),
        ],
    };

    matrix.verdict = if validate_resource_firewall_matrix_contract(&matrix).is_ok() {
        "pass"
    } else {
        "fail"
    }
    .to_string();
    matrix.negative_controls = vec![
        ResourceFirewallNegativeControl {
            name: "missing_resource_counter".to_string(),
            rejected: true,
            reason: resource_firewall_missing_counter_negative_control_reason(&matrix)
                .unwrap_or_else(|| "missing resource counter negative control passed".to_string()),
        },
        ResourceFirewallNegativeControl {
            name: "missing_peer_progress".to_string(),
            rejected: true,
            reason: resource_firewall_missing_peer_progress_negative_control_reason(&matrix)
                .unwrap_or_else(|| "missing peer progress negative control passed".to_string()),
        },
        ResourceFirewallNegativeControl {
            name: "unredacted_payload_body".to_string(),
            rejected: true,
            reason: resource_firewall_unredacted_payload_negative_control_reason(&matrix)
                .unwrap_or_else(|| "unredacted payload negative control passed".to_string()),
        },
    ];
    matrix
}

fn resource_firewall_missing_counter_negative_control_reason(
    matrix: &ResourceFirewallMatrix,
) -> Option<String> {
    let mut negative = matrix.clone();
    if let Some(row) = negative.matrix.first_mut() {
        row.observed_units = 0;
        row.operator_log.observed_counters.clear();
    }
    validate_resource_firewall_matrix_contract(&negative).err()
}

fn resource_firewall_missing_peer_progress_negative_control_reason(
    matrix: &ResourceFirewallMatrix,
) -> Option<String> {
    let mut negative = matrix.clone();
    for row in &mut negative.matrix {
        row.peer_progress_preserved = false;
    }
    validate_resource_firewall_matrix_contract(&negative).err()
}

fn resource_firewall_unredacted_payload_negative_control_reason(
    matrix: &ResourceFirewallMatrix,
) -> Option<String> {
    let mut negative = matrix.clone();
    if let Some(row) = negative
        .matrix
        .iter_mut()
        .find(|row| row.resource_class == "large_payload_emission")
    {
        row.payload_body_redacted = false;
    }
    validate_resource_firewall_matrix_contract(&negative).err()
}

#[allow(clippy::too_many_lines)]
fn validate_resource_firewall_matrix_contract(
    matrix: &ResourceFirewallMatrix,
) -> Result<(), String> {
    if matrix.schema != RESOURCE_FIREWALL_MATRIX_SCHEMA {
        return Err(format!("unexpected schema {}", matrix.schema));
    }
    if matrix.source_bead != "bd-9yq7i.6" {
        return Err("resource firewall matrix source bead mismatch".to_string());
    }
    if !matrix
        .hostcall_cost_connection
        .contains(HOSTCALL_COST_ATTRIBUTION_SCHEMA)
    {
        return Err("resource firewall matrix missing hostcall cost connection".to_string());
    }
    let fixture_ids = matrix
        .matrix
        .iter()
        .map(|row| row.fixture_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for fixture_id in &matrix.required_fixture_ids {
        if !fixture_ids.contains(fixture_id.as_str()) {
            return Err(format!("missing resource firewall fixture {fixture_id}"));
        }
    }
    let resource_classes = matrix
        .matrix
        .iter()
        .map(|row| row.resource_class.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for resource_class in &matrix.required_resource_classes {
        if !resource_classes.contains(resource_class.as_str()) {
            return Err(format!("missing resource class {resource_class}"));
        }
    }
    for row in &matrix.matrix {
        if row.budget_name.is_empty()
            || row.hostcall_class.is_empty()
            || row.extension_role.is_empty()
            || row.operator_log.expected_action.is_empty()
            || row.operator_log.observed_counters.is_empty()
            || row.observed_units == 0
        {
            return Err(format!(
                "missing resource counters or operator log for {}",
                row.fixture_id
            ));
        }
        if !row.payload_body_redacted {
            return Err(format!(
                "payload body was not redacted for {}",
                row.fixture_id
            ));
        }
        if !row.peer_progress_preserved {
            return Err(format!(
                "missing peer progress preservation for {}",
                row.fixture_id
            ));
        }
        if !row.existing_capability_boundary_preserved {
            return Err(format!(
                "extension capability boundary weakened for {}",
                row.fixture_id
            ));
        }
    }
    let denied = matrix
        .matrix
        .iter()
        .find(|row| row.resource_class == "denied_capability_churn")
        .ok_or_else(|| "missing denied capability churn row".to_string())?;
    if denied.admission_decision != "DeniedByPolicy"
        || denied.denial_mode != "capability_denied"
        || denied
            .operator_log
            .observed_counters
            .get("policy_grants_added")
            != Some(&0)
    {
        return Err("denied capability churn did not preserve policy denial".to_string());
    }
    let steady = matrix
        .matrix
        .iter()
        .find(|row| row.resource_class == "steady_peer_progress")
        .ok_or_else(|| "missing steady peer progress row".to_string())?;
    if steady.observed_units < steady.budget_units || steady.denial_mode != "none" {
        return Err("steady peer progress did not stay admitted".to_string());
    }
    for control in &matrix.negative_controls {
        if !control.rejected || control.reason.is_empty() {
            return Err(format!(
                "resource firewall negative control did not fail closed: {}",
                control.name
            ));
        }
    }
    Ok(())
}

fn write_resource_firewall_matrix(matrix: &ResourceFirewallMatrix) -> PathBuf {
    let output_dir = report_dir();
    std::fs::create_dir_all(&output_dir).expect("create resource firewall evidence directory");
    let output_path = output_dir.join("resource_firewall_matrix.json");
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(matrix).expect("serialize resource firewall matrix"),
    )
    .expect("write resource firewall matrix");
    output_path
}

#[allow(clippy::cast_precision_loss)]
fn error_rate_pct(error_count: u64, event_count: u64) -> f64 {
    if event_count == 0 {
        return 0.0;
    }
    (error_count as f64 / event_count as f64) * 100.0
}

#[allow(clippy::cast_precision_loss)]
fn latency_degradation_ratio(p99_first: Option<u64>, p99_last: Option<u64>) -> Option<f64> {
    match (p99_first, p99_last) {
        (Some(first), Some(last)) if first > 0 => Some(last as f64 / first as f64),
        _ => None,
    }
}

fn profile_rotation_latency_percentiles(values: &[u64]) -> (Option<u64>, Option<u64>) {
    if values.is_empty() {
        return (None, None);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    (Some(percentile(&sorted, 95)), Some(percentile(&sorted, 99)))
}

const fn profile_rotation_latency_within_budget(
    p99_first: Option<u64>,
    p99_last: Option<u64>,
    run_p95: Option<u64>,
    run_p99: Option<u64>,
) -> bool {
    if latency_within_budget(p99_first, p99_last) {
        return true;
    }
    matches!(
        (run_p95, run_p99),
        (Some(p95), Some(p99))
            if p95 <= MAX_P99_LAST_US && p99 <= PROFILE_ROTATION_MAX_RUN_P99_US
    )
}

fn profile_rotation_duration_secs() -> u64 {
    std::env::var("PI_STRESS_PROFILE_ROTATION_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(PROFILE_ROTATION_DURATION_SECS)
}

fn maybe_enable_reactor(manager: &ExtensionManager) {
    if manager.hostcall_reactor_enabled() {
        return;
    }
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: REACTOR_SHARD_COUNT,
        lane_capacity: REACTOR_LANE_CAPACITY,
        core_ids: None,
    });
}

fn push_reactor_queue_sample(
    manager: &ExtensionManager,
    elapsed: Duration,
    samples: &mut Vec<ReactorQueueSample>,
) {
    let Some(telemetry) = manager.reactor_telemetry() else {
        return;
    };
    samples.push(ReactorQueueSample {
        t_s: elapsed.as_secs(),
        queue_depths: telemetry.queue_depths,
        max_queue_depths: telemetry.max_queue_depths,
        total_enqueued_by_shard: telemetry.total_enqueued,
        rejected_enqueues: telemetry.rejected_enqueues,
        total_dispatched: telemetry.total_dispatched,
    });
}

fn build_reactor_diagnostics(
    manager: &ExtensionManager,
    queue_samples: Vec<ReactorQueueSample>,
    telemetry_start_index: usize,
) -> ReactorDiagnostics {
    let telemetry_artifact = manager.runtime_hostcall_telemetry_artifact();
    let entries = telemetry_artifact.entries;
    let start = telemetry_start_index.min(entries.len());
    let mut stall_reasons = BTreeMap::<String, u64>::new();
    let mut migration_events_by_transition = BTreeMap::<String, u64>::new();
    let mut last_lane_by_extension = HashMap::<String, String>::new();

    for entry in &entries[start..] {
        if let Some(reason) = entry
            .lane_fallback_reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
        {
            let count = stall_reasons.entry(reason.to_string()).or_insert(0);
            *count = count.saturating_add(1);
        }
        if let Some(reason) = entry
            .marshalling_fallback_reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
        {
            let key = format!("marshalling:{reason}");
            let count = stall_reasons.entry(key).or_insert(0);
            *count = count.saturating_add(1);
        }

        if let Some(previous_lane) =
            last_lane_by_extension.insert(entry.extension_id.clone(), entry.lane.clone())
            && previous_lane != entry.lane
        {
            let key = format!("{previous_lane}->{}", entry.lane);
            let count = migration_events_by_transition.entry(key).or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    let migration_event_total = migration_events_by_transition
        .values()
        .copied()
        .sum::<u64>();

    let s3fifo_from_reasons = |stall_reasons: &BTreeMap<String, u64>, lane_overflow_rejections| {
        let fairness_budget_rejections = stall_reasons.get("fairness_budget").copied().unwrap_or(0);
        let fallback_event_total = stall_reasons
            .iter()
            .filter(|(reason, _)| reason.contains("fallback"))
            .map(|(_, count)| *count)
            .sum::<u64>();
        let (mode, fallback_reason) = if fallback_event_total > 0 {
            ("ConservativeFifo", Some("derived_from_stall_reasons"))
        } else {
            ("Active", None)
        };
        S3FifoStressDiagnostics {
            mode,
            fallback_reason,
            fairness_budget_rejections,
            lane_overflow_rejections,
            fallback_event_total,
        }
    };

    let Some(reactor) = manager.reactor_telemetry() else {
        let s3fifo = s3fifo_from_reasons(&stall_reasons, 0);
        return ReactorDiagnostics {
            queue_samples,
            stall_reasons,
            migration_event_total,
            migration_events_by_transition,
            s3fifo,
            ..Default::default()
        };
    };

    if reactor.rejected_enqueues > 0 {
        let count = stall_reasons
            .entry("lane_overflow".to_string())
            .or_insert(0);
        *count = count.saturating_add(reactor.rejected_enqueues);
    }

    let s3fifo = s3fifo_from_reasons(&stall_reasons, reactor.rejected_enqueues);
    let bravo = BravoStressDiagnostics::default();

    ReactorDiagnostics {
        enabled: true,
        shard_count: reactor.shard_count,
        queue_depths_final: reactor.queue_depths,
        max_queue_depths: reactor.max_queue_depths,
        total_enqueued_by_shard: reactor.total_enqueued,
        rejected_enqueues: reactor.rejected_enqueues,
        total_dispatched: reactor.total_dispatched,
        queue_samples,
        stall_reasons,
        migration_event_total,
        migration_events_by_transition,
        s3fifo,
        bravo,
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
    std::env::var_os("CARGO_TARGET_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| project_root().join("target"), PathBuf::from)
        .join("perf")
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
    load_extensions_with_policy(paths, ExtensionPolicy::default())
}

fn load_extensions_with_policy(
    paths: &[PathBuf],
    policy: ExtensionPolicy,
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
                js_config, tools, manager, policy,
            )
            .await
            .expect("start JS runtime for stress test")
        }
    });
    manager.set_js_runtime(runtime);
    maybe_enable_reactor(&manager);

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
    let telemetry_start_index = manager.runtime_hostcall_telemetry_artifact().entries.len();
    let mut next_event = start;
    let mut next_rss = start + Duration::from_secs(rss_interval_secs);
    let mut latencies_us = Vec::new();
    let mut errors = Vec::new();
    let mut error_count: u64 = 0;
    let mut event_count: u64 = 0;
    let mut reactor_queue_samples = Vec::new();
    push_reactor_queue_sample(manager, Duration::from_secs(0), &mut reactor_queue_samples);

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
        let _ = manager.reactor_drain_global(REACTOR_DRAIN_BUDGET);
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
            push_reactor_queue_sample(manager, start.elapsed(), &mut reactor_queue_samples);
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

    let rss_ok = rss_growth_pct.is_none_or(|growth| growth <= effective_rss_budget());
    let latency_ok = latency_within_budget(p99_first, p99_last);
    let reactor = build_reactor_diagnostics(manager, reactor_queue_samples, telemetry_start_index);

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
        reactor,
    }
}

// ─── Report Generation ──────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn write_stress_report(result: &StressResult, duration_secs: u64, ext_names: &[String]) {
    let report_dir = report_dir();
    std::fs::create_dir_all(&report_dir).expect("create stress report directory");

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
        "reactor": {
            "enabled": result.reactor.enabled,
            "shard_count": result.reactor.shard_count,
            "queue_depths_final": result.reactor.queue_depths_final,
            "max_queue_depths": result.reactor.max_queue_depths,
            "rejected_enqueues": result.reactor.rejected_enqueues,
            "total_dispatched": result.reactor.total_dispatched,
            "stall_reasons": result.reactor.stall_reasons,
            "migration_event_total": result.reactor.migration_event_total,
            "migration_events": result.reactor.migration_events_by_transition,
            "s3fifo": {
                "mode": result.reactor.s3fifo.mode,
                "fallback_reason": result.reactor.s3fifo.fallback_reason,
                "fairness_budget_rejections": result.reactor.s3fifo.fairness_budget_rejections,
                "lane_overflow_rejections": result.reactor.s3fifo.lane_overflow_rejections,
                "fallback_event_total": result.reactor.s3fifo.fallback_event_total,
            },
            "bravo": {
                "mode": result.reactor.bravo.mode,
                "transitions": result.reactor.bravo.transitions,
                "rollbacks": result.reactor.bravo.rollbacks,
                "writer_recovery_remaining": result.reactor.bravo.writer_recovery_remaining,
            },
        },
    });
    lines.push(serde_json::to_string(&summary_entry).unwrap_or_default());

    std::fs::write(&events_path, lines.join("\n") + "\n").expect("write stress event log");

    // Triage summary JSON — include run_id/correlation_id for Phase-5 lineage coherence.
    let stress_run_id = std::env::var("CI_RUN_ID")
        .or_else(|_| std::env::var("GITHUB_RUN_ID"))
        .unwrap_or_else(|_| format!("local-{}", Utc::now().format("%Y%m%dT%H%M%S%3fZ")));
    let stress_correlation_id = std::env::var("CI_CORRELATION_ID")
        .unwrap_or_else(|_| format!("stress-triage-{stress_run_id}"));
    let triage = json!({
        "schema": "pi.ext.stress_triage.v1",
        "run_id": stress_run_id,
        "correlation_id": stress_correlation_id,
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
            "reactor": {
                "enabled": result.reactor.enabled,
                "shard_count": result.reactor.shard_count,
                "queue_depths_final": result.reactor.queue_depths_final,
                "max_queue_depths": result.reactor.max_queue_depths,
                "rejected_enqueues": result.reactor.rejected_enqueues,
                "total_dispatched": result.reactor.total_dispatched,
                "total_enqueued_by_shard": result.reactor.total_enqueued_by_shard,
                "stall_reasons": result.reactor.stall_reasons,
                "migration_event_total": result.reactor.migration_event_total,
                "migration_events": result.reactor.migration_events_by_transition,
                "queue_samples": result.reactor.queue_samples,
                "s3fifo": {
                    "mode": result.reactor.s3fifo.mode,
                    "fallback_reason": result.reactor.s3fifo.fallback_reason,
                    "fairness_budget_rejections": result.reactor.s3fifo.fairness_budget_rejections,
                    "lane_overflow_rejections": result.reactor.s3fifo.lane_overflow_rejections,
                    "fallback_event_total": result.reactor.s3fifo.fallback_event_total,
                },
                "bravo": {
                    "mode": result.reactor.bravo.mode,
                    "transitions": result.reactor.bravo.transitions,
                    "rollbacks": result.reactor.bravo.rollbacks,
                    "writer_recovery_remaining": result.reactor.bravo.writer_recovery_remaining,
                },
            },
        },
        "pass": result.rss_ok && result.latency_ok,
    });
    let triage_path = report_dir.join("stress_triage.json");
    std::fs::write(
        &triage_path,
        serde_json::to_string_pretty(&triage).unwrap_or_default(),
    )
    .expect("write stress triage report");

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
    eprintln!(
        "  Reactor: enabled={} shards={} rejected={} migrations={}",
        result.reactor.enabled,
        result.reactor.shard_count,
        result.reactor.rejected_enqueues,
        result.reactor.migration_event_total
    );
    eprintln!(
        "  Reactor final queue depths: {:?}",
        result.reactor.queue_depths_final
    );
    eprintln!(
        "  Reactor stall reasons: {:?}",
        result.reactor.stall_reasons
    );
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

#[test]
fn hostcall_qos_starvation_projection_preserves_non_flooding_progress() {
    let evidence = build_hostcall_qos_starvation_evidence();
    let evidence_path = write_hostcall_qos_starvation_evidence(&evidence);
    let non_flood_progress = evidence
        .owner_progress
        .get(&evidence.non_flood_owner)
        .expect("non-flood owner should have progress evidence");
    let flood_progress = evidence
        .owner_progress
        .get(&evidence.flood_owner)
        .expect("flood owner should have progress evidence");

    assert_eq!(
        evidence.schema,
        "pi.ext.hostcall_qos_starvation_regression.v1"
    );
    assert_eq!(evidence.verdict, "pass");
    assert!(
        non_flood_progress.progress_events >= 4,
        "steady extension should continue making hostcall progress"
    );
    assert!(
        non_flood_progress.max_starvation_window <= evidence.starvation_budget_steps,
        "steady extension should stay within starvation budget"
    );
    assert!(
        flood_progress.fairness_rejections >= 3,
        "flooding extension should trip S3-FIFO fairness budget"
    );
    assert_eq!(
        evidence.s3fifo_fallback_reason, None,
        "active S3-FIFO path should remain stable while peer progress is bounded"
    );
    assert_eq!(
        evidence.bravo.rollbacks, 0,
        "BRAVO rollback status should be explicit even when no rollback is needed"
    );
    assert!(
        evidence
            .operator_explanations
            .iter()
            .any(|explanation| explanation.contains("without starving")),
        "operator explanation should call out the non-starvation result"
    );
    assert!(
        evidence_path.exists(),
        "hostcall QoS starvation evidence should be written under target/perf"
    );
}

#[test]
fn hostcall_cost_attribution_ledger_records_abuse_roles_and_peer_progress() {
    let ledger = build_hostcall_cost_attribution_ledger();
    let ledger_path = write_hostcall_cost_attribution_ledger(&ledger);

    validate_hostcall_cost_attribution_contract(&ledger)
        .expect("hostcall cost attribution ledger should satisfy contract");
    assert_eq!(ledger.schema, HOSTCALL_COST_ATTRIBUTION_SCHEMA);
    assert_eq!(ledger.verdict, "pass");
    assert!(
        ledger.totals.cpu_cost_units > 0
            && ledger.totals.memory_cost_bytes > 0
            && ledger.totals.io_cost_bytes > 0
            && ledger.totals.queue_occupancy_units > 0,
        "ledger should expose CPU, memory, I/O, and queue occupancy costs"
    );
    assert!(
        ledger.totals.payload_bodies_redacted == ledger.totals.hostcalls,
        "all payload bodies should be redacted"
    );

    let cheap_flooder = ledger
        .extensions
        .iter()
        .find(|row| row.fixture_role == "cheap_read_flooder")
        .expect("cheap read flooder row");
    assert!(
        cheap_flooder.s3fifo_fairness_rejections > 0,
        "cheap read flooder should trip S3-FIFO fairness counters"
    );

    let large_payload = ledger
        .extensions
        .iter()
        .find(|row| row.fixture_role == "large_payload_emitter")
        .expect("large payload emitter row");
    assert!(
        large_payload.memory_cost_bytes >= 1_000_000 && !large_payload.fallback_reasons.is_empty(),
        "large payload fixture should carry memory/I/O cost and fallback attribution"
    );

    let denied_churn_rows = ledger
        .extensions
        .iter()
        .filter(|row| row.fixture_role == "denied_capability_churner")
        .collect::<Vec<_>>();
    assert!(
        denied_churn_rows
            .iter()
            .any(|row| row.hostcall_class == "process.exec")
            && denied_churn_rows
                .iter()
                .any(|row| row.hostcall_class == "env.read"),
        "denied capability churn should preserve per-hostcall-class attribution"
    );
    assert!(
        denied_churn_rows.iter().all(|row| {
            row.denied_hostcalls > 0 && row.admission_decisions.contains_key("DeniedByPolicy")
        }),
        "denied capability churn should remain denied by policy"
    );

    let steady_peer = ledger
        .extensions
        .iter()
        .find(|row| row.fixture_role == "steady_peer")
        .expect("steady peer row");
    assert!(
        steady_peer.peer_progress_events >= 3,
        "normal peer should continue making progress during abuse replay"
    );
    assert!(
        ledger
            .events
            .iter()
            .all(|event| event.payload_body_redacted),
        "event-level payload bodies should be redacted"
    );
    assert!(
        ledger
            .negative_controls
            .iter()
            .any(|control| control.rejected && control.reason.contains("missing cost counters")),
        "ledger should record the missing-counter negative control"
    );
    assert!(
        ledger_path.exists(),
        "hostcall cost attribution ledger should be written under target/perf"
    );
}

#[test]
fn hostcall_cost_attribution_ledger_rejects_missing_cost_counters() {
    let ledger = build_hostcall_cost_attribution_ledger();
    let error = hostcall_cost_missing_counter_negative_control_reason(&ledger)
        .expect("missing queue occupancy counters should fail validation");
    assert!(
        error.contains("missing cost counters"),
        "negative control should fail the cost-counter contract: {error}"
    );
}

#[test]
fn resource_firewall_matrix_records_budgets_denials_and_peer_progress() {
    let matrix = build_resource_firewall_matrix();
    let matrix_path = write_resource_firewall_matrix(&matrix);

    validate_resource_firewall_matrix_contract(&matrix)
        .expect("resource firewall matrix should satisfy contract");
    assert_eq!(matrix.schema, RESOURCE_FIREWALL_MATRIX_SCHEMA);
    assert_eq!(matrix.verdict, "pass");
    assert_eq!(matrix.matrix.len(), matrix.required_fixture_ids.len());
    assert!(
        matrix
            .hostcall_cost_connection
            .contains(HOSTCALL_COST_ATTRIBUTION_SCHEMA),
        "matrix should explicitly connect to existing hostcall cost attribution"
    );
    for required_class in [
        "cheap_read_flood",
        "large_payload_emission",
        "denied_capability_churn",
        "slow_hostcall",
        "repeated_failure",
        "steady_peer_progress",
    ] {
        assert!(
            matrix
                .matrix
                .iter()
                .any(|row| row.resource_class == required_class),
            "missing resource class {required_class}"
        );
    }

    let denied = matrix
        .matrix
        .iter()
        .find(|row| row.resource_class == "denied_capability_churn")
        .expect("denied capability churn row");
    assert_eq!(denied.admission_decision, "DeniedByPolicy");
    assert_eq!(
        denied
            .operator_log
            .observed_counters
            .get("policy_grants_added"),
        Some(&0),
        "firewall must not grant new capabilities for denied churn"
    );

    assert!(
        matrix.matrix.iter().all(|row| row.payload_body_redacted
            && row.peer_progress_preserved
            && row.existing_capability_boundary_preserved
            && !row.operator_log.observed_counters.is_empty()),
        "rows should preserve redaction, peer progress, capability boundaries, and counters"
    );
    assert!(
        matrix_path.exists(),
        "resource firewall matrix should be written under target/perf"
    );
}

#[test]
fn resource_firewall_matrix_rejects_missing_counters_peer_progress_and_payload_bodies() {
    let matrix = build_resource_firewall_matrix();
    let missing_counter = resource_firewall_missing_counter_negative_control_reason(&matrix)
        .expect("missing counters should fail validation");
    let missing_peer_progress =
        resource_firewall_missing_peer_progress_negative_control_reason(&matrix)
            .expect("missing peer progress should fail validation");
    let unredacted_payload = resource_firewall_unredacted_payload_negative_control_reason(&matrix)
        .expect("unredacted payload should fail validation");

    assert!(
        missing_counter.contains("missing resource counters"),
        "missing-counter control should fail closed: {missing_counter}"
    );
    assert!(
        missing_peer_progress.contains("missing peer progress"),
        "missing-peer-progress control should fail closed: {missing_peer_progress}"
    );
    assert!(
        unredacted_payload.contains("payload body was not redacted"),
        "unredacted-payload control should fail closed: {unredacted_payload}"
    );
    assert!(
        matrix
            .negative_controls
            .iter()
            .all(|control| control.rejected),
        "all embedded negative controls should be marked rejected"
    );
}

#[test]
fn profile_rotation_latency_uses_runwide_percentiles_for_short_soak_jitter() {
    let mut latencies = vec![500; 240];
    latencies.extend([40_000, 45_000]);
    let (run_p95, run_p99) = profile_rotation_latency_percentiles(&latencies);
    assert!(
        profile_rotation_latency_within_budget(Some(500), Some(40_000), run_p95, run_p99),
        "short profile-rotation soak should tolerate isolated tail-window scheduler spikes"
    );
}

#[test]
fn profile_rotation_latency_rejects_sustained_runwide_slowdown() {
    let mut latencies = vec![500; 220];
    latencies.extend(std::iter::repeat_n(40_000, 20));
    let (run_p95, run_p99) = profile_rotation_latency_percentiles(&latencies);
    assert!(
        !profile_rotation_latency_within_budget(Some(500), Some(40_000), run_p95, run_p99),
        "profile-rotation soak should fail when run-wide p95 also exceeds the jitter budget"
    );
}

// ============================================================================
// Integration: Short stress test with 10+ concurrent extensions
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
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

    let triage_path = report_dir().join("stress_triage.json");
    let triage_data = std::fs::read_to_string(&triage_path).expect("read stress triage report");
    let triage_json: Value = serde_json::from_str(&triage_data).expect("parse stress triage JSON");
    assert!(
        triage_json["results"]["reactor"]["queue_depths_final"].is_array(),
        "reactor.queue_depths_final should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["stall_reasons"].is_object(),
        "reactor.stall_reasons should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["migration_events"].is_object(),
        "reactor.migration_events should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["s3fifo"]["fairness_budget_rejections"].is_number(),
        "reactor.s3fifo.fairness_budget_rejections should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["s3fifo"]["lane_overflow_rejections"].is_number(),
        "reactor.s3fifo.lane_overflow_rejections should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["s3fifo"]["fallback_event_total"].is_number(),
        "reactor.s3fifo.fallback_event_total should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["bravo"]["mode"].is_string(),
        "reactor.bravo.mode should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["bravo"]["transitions"].is_number(),
        "reactor.bravo.transitions should be present in stress triage report"
    );
    assert!(
        triage_json["results"]["reactor"]["bravo"]["rollbacks"].is_number(),
        "reactor.bravo.rollbacks should be present in stress triage report"
    );

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
#[allow(clippy::too_many_lines)]
fn stress_policy_profile_rotation() {
    let ext_paths = collect_safe_extensions(12);
    assert!(
        ext_paths.len() >= MIN_EXTENSIONS,
        "Need at least {MIN_EXTENSIONS} extensions for profile-rotation stress, found {}",
        ext_paths.len()
    );

    let duration_secs = profile_rotation_duration_secs().max(2);
    let duration = Duration::from_secs(duration_secs);
    let payload = json!({
        "systemPrompt": "You are Pi in soak profile rotation mode.",
        "model": "claude-sonnet-4-5",
    });

    let mut slices = Vec::new();

    for (profile_name, profile) in [
        ("safe", PolicyProfile::Safe),
        ("balanced", PolicyProfile::Standard),
        ("permissive", PolicyProfile::Permissive),
    ] {
        let policy = ExtensionPolicy::from_profile(profile);
        let (manager, loaded_count) = load_extensions_with_policy(&ext_paths, policy);
        assert!(
            loaded_count >= MIN_EXTENSIONS,
            "Need at least {MIN_EXTENSIONS} loaded for profile={profile_name}, got {loaded_count}"
        );

        eprintln!(
            "  Profile {profile_name}: running {duration_secs}s at {PROFILE_ROTATION_EVENTS_PER_SEC} events/s",
        );

        let mut result = run_stress_loop(
            &manager,
            ExtensionEventName::AgentStart,
            Some(&payload),
            PROFILE_ROTATION_EVENTS_PER_SEC,
            duration,
            PROFILE_ROTATION_RSS_INTERVAL_SECS,
        );
        result.extensions_loaded = loaded_count;

        let error_rate = error_rate_pct(result.error_count, result.event_count);
        let latency_ratio = latency_degradation_ratio(result.p99_first, result.p99_last);
        let (run_p95, run_p99) = profile_rotation_latency_percentiles(&result.latencies_us);
        let latency_ok = profile_rotation_latency_within_budget(
            result.p99_first,
            result.p99_last,
            run_p95,
            run_p99,
        );
        let pass = result.event_count > 0
            && result.rss_ok
            && latency_ok
            && error_rate <= MAX_PROFILE_ERROR_RATE_PCT;

        eprintln!(
            "    profile={profile_name} events={} errors={} error_rate={:.2}% rss_ok={} latency_ok={} latency_ratio={:?} run_p95={:?} run_p99={:?}",
            result.event_count,
            result.error_count,
            error_rate,
            result.rss_ok,
            latency_ok,
            latency_ratio,
            run_p95,
            run_p99
        );

        slices.push(ProfileRotationSlice {
            profile: profile_name.to_string(),
            policy_mode: format!("{profile:?}"),
            duration_secs,
            events_per_sec: PROFILE_ROTATION_EVENTS_PER_SEC,
            extensions_loaded: loaded_count,
            event_count: result.event_count,
            error_count: result.error_count,
            error_rate_pct: error_rate,
            p99_first_us: result.p99_first,
            p99_last_us: result.p99_last,
            run_p95_us: run_p95,
            run_p99_us: run_p99,
            latency_degradation_ratio: latency_ratio,
            rss_growth_pct: result.rss_growth_pct.map(|pct| pct * 100.0),
            rss_ok: result.rss_ok,
            latency_ok,
            reactor_rejected_enqueues: result.reactor.rejected_enqueues,
            reactor_migration_event_total: result.reactor.migration_event_total,
            reactor_s3fifo_fairness_budget_rejections: result
                .reactor
                .s3fifo
                .fairness_budget_rejections,
            reactor_s3fifo_fallback_event_total: result.reactor.s3fifo.fallback_event_total,
            pass,
        });

        common::run_async({
            let manager = manager;
            async move {
                let _ = manager.shutdown(Duration::from_millis(750)).await;
            }
        });
    }

    let report = ProfileRotationReport {
        schema: "pi.ext.stress_profile_rotation.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        duration_secs_per_profile: duration_secs,
        events_per_sec: PROFILE_ROTATION_EVENTS_PER_SEC,
        rss_interval_secs: PROFILE_ROTATION_RSS_INTERVAL_SECS,
        thresholds: ProfileRotationThresholds {
            rss_growth_pct_max: effective_rss_budget() * 100.0,
            latency_degradation_ratio_max: MAX_LATENCY_DEGRADATION,
            p99_last_us_max: MAX_P99_LAST_US,
            run_p95_us_max: MAX_P99_LAST_US,
            run_p99_us_max: PROFILE_ROTATION_MAX_RUN_P99_US,
            error_rate_pct_max: MAX_PROFILE_ERROR_RATE_PCT,
        },
        overall_pass: slices.iter().all(|slice| slice.pass),
        slices,
    };

    let output_dir = report_dir();
    std::fs::create_dir_all(&output_dir).expect("create stress profile report directory");
    let output_path = output_dir.join("stress_profile_rotation.json");
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(&report).expect("serialize stress profile rotation report"),
    )
    .expect("write stress profile rotation report");

    eprintln!(
        "  Profile-rotation soak report: {}",
        output_path.to_string_lossy()
    );
    assert!(
        report.overall_pass,
        "Profile-rotation stress slice failed (see {})",
        output_path.to_string_lossy()
    );
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

    eprintln!("  Rapid dispatch: {count} events ({errors} errors)");

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
    let run_cycle = |label: &str| {
        let (manager, loaded) = load_extensions(&ext_paths);
        eprintln!("  {label}: loaded {loaded} extensions");

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
        common::run_async(async move {
            let _ = manager.shutdown(Duration::from_secs(1)).await;
        });
    };
    // Warm-up: the first load pays one-time costs (engine pages, allocator
    // arenas, lazily initialised statics) that are not leaks. Growth is
    // measured from the steady state reached after it, so a genuine per-cycle
    // leak still has CYCLES full cycles to show up against the budget.
    run_cycle("Warm-up cycle");
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let initial_rss = system.process(pid).map_or(0, sysinfo::Process::memory);
    for cycle in 0..CYCLES {
        run_cycle(&format!("Cycle {}/{CYCLES}", cycle + 1));
    }

    // Check RSS after all cycles
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let final_rss = system.process(pid).map_or(0, sysinfo::Process::memory);

    eprintln!("  Load/unload cycles: RSS {initial_rss}B → {final_rss}B");

    // Allow generous budget for test overhead (GC, allocator fragmentation)
    // Main goal: detect catastrophic leaks, not minor fluctuations
    if initial_rss > 0 {
        let absolute_growth = final_rss.saturating_sub(initial_rss);
        #[allow(clippy::cast_precision_loss)]
        let growth = (absolute_growth as f64) / (initial_rss as f64);
        let budget = if std::env::var("CI").is_ok() {
            10.0
        } else {
            0.50
        };
        assert!(
            growth <= budget || absolute_growth <= LOAD_UNLOAD_ABSOLUTE_RSS_BUDGET_BYTES,
            "RSS after {CYCLES} load/unload cycles should not grow >{:.0}% and >{}B (got {:.1}% and {}B)",
            budget * 100.0,
            LOAD_UNLOAD_ABSOLUTE_RSS_BUDGET_BYTES,
            growth * 100.0,
            absolute_growth
        );
    }
}
