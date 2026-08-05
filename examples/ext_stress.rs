//! Extension stress test harness: memory/RSS + event dispatch latency.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use asupersync::time::{timeout, wall_now};
use chrono::{SecondsFormat, Utc};
use clap::{ArgAction, Parser};
use pi::extensions::{
    ExtensionEventName, ExtensionManager, HostcallReactorConfig, JsExtensionLoadSpec,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "ext_stress")]
#[command(about = "Extension stress test: RSS + event dispatch latency")]
struct Args {
    /// Source tier to load (default: official-pi-mono).
    #[arg(long, default_value = "official-pi-mono")]
    tier: String,
    /// Total duration in seconds.
    #[arg(long, default_value_t = 3600)]
    duration_secs: u64,
    /// Warmup duration in seconds (excluded from report).
    #[arg(long, default_value_t = 0)]
    warmup_secs: u64,
    /// RSS sampling interval in seconds (0 disables sampling).
    #[arg(long, default_value_t = 10)]
    rss_interval_secs: u64,
    /// Event dispatch rate (events per second).
    #[arg(long, default_value_t = 100)]
    events_per_sec: u64,
    /// Extension event name to dispatch (e.g. `agent_start`, `input`).
    #[arg(long, default_value = "agent_start")]
    event: String,
    /// Index into the event payload list (if defined in `event_payloads` file).
    #[arg(long, default_value_t = 0)]
    payload_index: usize,
    /// Maximum number of extensions to load.
    #[arg(long)]
    max_extensions: Option<usize>,
    /// Generate this many synthetic `agent_start` handlers that call back into the Rust host.
    #[arg(long, default_value_t = 0)]
    synthetic_hostcall_extensions: usize,
    /// Override directory for generated synthetic hostcall extension sources.
    #[arg(long)]
    synthetic_hostcall_root: Option<PathBuf>,
    /// Override path to `VALIDATED_MANIFEST.json`.
    #[arg(long)]
    manifest_path: Option<PathBuf>,
    /// Override artifacts root dir.
    #[arg(long)]
    artifacts_dir: Option<PathBuf>,
    /// Override path to event payloads JSON.
    #[arg(long)]
    event_payloads_path: Option<PathBuf>,
    /// Output report path (JSON).
    #[arg(long)]
    report_path: Option<PathBuf>,
    /// Output event stream path (JSONL).
    #[arg(long)]
    events_path: Option<PathBuf>,
    /// Stable run ID for evidence lineage.
    #[arg(long)]
    run_id: Option<String>,
    /// Shared correlation ID for multi-artifact evidence lineage.
    #[arg(long)]
    correlation_id: Option<String>,
    /// Enable reactor diagnostics (queue depth, stall reasons, migration events).
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    reactor_enabled: bool,
    /// Number of reactor shards to configure when enabled.
    #[arg(long, default_value_t = 4)]
    reactor_shards: usize,
    /// Per-shard queue capacity for reactor diagnostics.
    #[arg(long, default_value_t = 256)]
    reactor_lane_capacity: usize,
    /// Optional comma-separated core IDs, one per reactor shard, for NUMA/advisory placement probes.
    #[arg(long)]
    reactor_core_ids: Option<String>,
    /// Drain budget per dispatch iteration to keep queue depth bounded.
    #[arg(long, default_value_t = 128)]
    reactor_drain_budget: usize,
    /// Extra per-event harness timeout in milliseconds (0 uses manager defaults only).
    #[arg(long, default_value_t = 5_000)]
    dispatch_timeout_ms: u64,
    /// Run a built-in comparison: baseline shard count vs configured reactor mesh.
    #[arg(long, default_value_t = false, action = ArgAction::Set)]
    compare_shard_baseline: bool,
    /// Baseline shard count used by `--compare-shard-baseline`.
    #[arg(long, default_value_t = 1)]
    compare_baseline_shards: usize,
    /// Fail after writing the report unless reactor enqueue and dispatch counters advanced.
    #[arg(long, default_value_t = false, action = ArgAction::Set)]
    require_reactor_activity: bool,
}

fn main() {
    if let Err(err) = main_impl() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn main_impl() -> Result<()> {
    let args = Args::parse();

    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let join = handle.spawn(Box::pin(run(args)));
    runtime.block_on(join)
}

#[allow(clippy::too_many_lines)]
async fn run(args: Args) -> Result<()> {
    if args.events_per_sec == 0 {
        bail!("--events-per-sec must be > 0");
    }

    let manifest_path = args.manifest_path.unwrap_or_else(default_manifest_path);
    let artifacts_dir = args.artifacts_dir.unwrap_or_else(default_artifacts_dir);
    let payloads_path = args
        .event_payloads_path
        .unwrap_or_else(default_event_payloads_path);
    let report_path = args.report_path.clone().unwrap_or_else(default_report_path);
    let events_path = args.events_path.clone().unwrap_or_else(default_events_path);
    let synthetic_hostcall_root = args.synthetic_hostcall_root.clone();
    let run_id = args
        .run_id
        .as_deref()
        .map_or_else(default_run_id, str::to_owned);
    let correlation_id = args
        .correlation_id
        .as_deref()
        .map(str::to_owned)
        .or_else(|| std::env::var("CI_CORRELATION_ID").ok())
        .unwrap_or_else(|| format!("ext-stress-{run_id}"));

    let (specs, names) = if args.synthetic_hostcall_extensions > 0 {
        let synthetic_root = synthetic_hostcall_root.clone().unwrap_or_else(|| {
            report_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("synthetic_hostcall_extensions")
        });
        build_synthetic_hostcall_specs(&synthetic_root, args.synthetic_hostcall_extensions)?
    } else {
        let mut entries = extensions_by_tier(&manifest_path, &args.tier)?;
        if let Some(max) = args.max_extensions {
            entries.truncate(max);
        }
        let limited_entries = entries;

        if limited_entries.is_empty() {
            bail!("No extensions found for tier {}", args.tier);
        }

        build_specs(&artifacts_dir, &limited_entries)?
    };
    let payload = event_payload_for(&payloads_path, &args.event, args.payload_index)?;
    let event = parse_event_name(&args.event)?;

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string();
    let tools = Arc::new(ToolRegistry::new(&[], Path::new(&cwd), None));
    let manager = ExtensionManager::new();
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.clone(),
        ..Default::default()
    };
    let runtime = pi::extensions::JsExtensionRuntimeHandle::start(
        js_config,
        Arc::clone(&tools),
        manager.clone(),
    )
    .await
    .context("start JS extension runtime")?;
    manager.set_js_runtime(runtime);
    manager.set_cwd(cwd);

    manager
        .load_js_extensions(specs)
        .await
        .context("load JS extensions")?;

    let target_shards = args.reactor_shards.max(1);
    let baseline_shards = args.compare_baseline_shards.max(1);
    let lane_capacity = args.reactor_lane_capacity.max(1);
    let reactor_core_ids = parse_reactor_core_ids(args.reactor_core_ids.as_deref())?;

    if args.compare_shard_baseline && !args.reactor_enabled {
        bail!("--compare-shard-baseline requires --reactor-enabled=true");
    }
    if args.compare_shard_baseline && baseline_shards == target_shards {
        bail!(
            "--compare-shard-baseline requires --compare-baseline-shards to differ from --reactor-shards"
        );
    }
    if let Some(core_ids) = &reactor_core_ids
        && core_ids.len() != target_shards
    {
        bail!("--reactor-core-ids must contain exactly one core id per --reactor-shards entry");
    }

    let (run_result, comparison, comparison_ok) = if args.compare_shard_baseline {
        configure_reactor(&manager, true, baseline_shards, lane_capacity, None);
        let baseline_result = run_profile(
            &manager,
            event,
            payload.clone(),
            args.events_per_sec,
            args.warmup_secs,
            args.duration_secs,
            args.rss_interval_secs,
            args.reactor_drain_budget,
            args.dispatch_timeout_ms,
        )
        .await?;

        configure_reactor(
            &manager,
            true,
            target_shards,
            lane_capacity,
            reactor_core_ids.clone(),
        );
        let candidate_result = run_profile(
            &manager,
            event,
            payload,
            args.events_per_sec,
            args.warmup_secs,
            args.duration_secs,
            args.rss_interval_secs,
            args.reactor_drain_budget,
            args.dispatch_timeout_ms,
        )
        .await?;

        let comparison = Some(build_shard_comparison_report(
            &baseline_result,
            &candidate_result,
            args.duration_secs,
            baseline_shards,
            target_shards,
        ));
        let comparison_ok = baseline_result.events_ok && candidate_result.events_ok;
        (candidate_result, comparison, comparison_ok)
    } else {
        configure_reactor(
            &manager,
            args.reactor_enabled,
            target_shards,
            lane_capacity,
            reactor_core_ids.clone(),
        );
        let result = run_profile(
            &manager,
            event,
            payload,
            args.events_per_sec,
            args.warmup_secs,
            args.duration_secs,
            args.rss_interval_secs,
            args.reactor_drain_budget,
            args.dispatch_timeout_ms,
        )
        .await?;
        (result, None, true)
    };
    let latency_summary = summarize_us(&run_result.latencies_us);
    let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let reactor_activity = ReactorActivity::from_report(&run_result.reactor);
    let reactor_activity_ok = !args.require_reactor_activity || reactor_activity.ok;

    let report = serde_json::json!({
        "schema": "pi.ext.stress_profile.v1",
        "run_id": run_id,
        "correlation_id": correlation_id,
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "events_path": events_path.display().to_string(),
        "host": {
            "logical_cpus": logical_cpus,
            "target_class": if logical_cpus >= 64 { "64plus_cpu" } else { "below_64_cpu" },
        },
        "config": {
            "tier": args.tier,
            "event": args.event,
            "payload_index": args.payload_index,
            "duration_secs": args.duration_secs,
            "warmup_secs": args.warmup_secs,
            "rss_interval_secs": args.rss_interval_secs,
            "events_per_sec": args.events_per_sec,
            "max_extensions": args.max_extensions,
            "synthetic_hostcall_extensions": args.synthetic_hostcall_extensions,
            "synthetic_hostcall_root": synthetic_hostcall_root
                .as_ref()
                .map(|path| path.display().to_string()),
            "reactor_enabled": args.reactor_enabled,
            "reactor_shards": target_shards,
            "reactor_lane_capacity": lane_capacity,
            "reactor_core_ids": reactor_core_ids,
            "reactor_drain_budget": args.reactor_drain_budget,
            "dispatch_timeout_ms": args.dispatch_timeout_ms,
            "compare_shard_baseline": args.compare_shard_baseline,
            "compare_baseline_shards": baseline_shards,
            "require_reactor_activity": args.require_reactor_activity,
        },
        "extensions": {
            "count": names.len(),
            "names": names,
        },
        "rss": {
            "initial_bytes": run_result.initial_rss_bytes,
            "max_bytes": run_result.max_rss_bytes,
            "initial_kib": bytes_to_kib(run_result.initial_rss_bytes),
            "max_kib": bytes_to_kib(run_result.max_rss_bytes),
            "initial_kb": bytes_to_kib(run_result.initial_rss_bytes),
            "max_kb": bytes_to_kib(run_result.max_rss_bytes),
            "growth_pct": run_result.rss_growth_pct,
            "samples": run_result.resource_samples_json(),
        },
        "cpu": {
            "process_max_pct": run_result.max_cpu_usage_pct,
            "process_mean_pct": run_result.mean_cpu_usage_pct(),
            "samples": run_result.resource_samples_json(),
        },
        "latency_us": {
            "summary": latency_summary,
            "p99_first": run_result.p99_first,
            "p99_last": run_result.p99_last,
        },
        "events": {
            "count": run_result.event_count,
            "target_count": run_result.target_event_count,
            "min_count": run_result.min_event_count,
            "errors": run_result.error_count,
            "sample_errors": run_result.errors,
        },
        "reactor": run_result.reactor,
        "reactor_activity": reactor_activity.to_json(),
        "comparison": comparison,
        "pass": {
            "rss_ok": run_result.rss_ok,
            "latency_ok": run_result.latency_ok,
            "events_ok": run_result.events_ok,
            "comparison_ok": comparison_ok,
            "reactor_activity_ok": reactor_activity_ok,
            "overall": run_result.rss_ok
                && run_result.latency_ok
                && run_result.events_ok
                && comparison_ok
                && reactor_activity_ok,
        }
    });

    write_events_jsonl(&events_path, &report, &run_result).await?;

    if let Some(parent) = report_path.parent() {
        asupersync::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    asupersync::fs::write(&report_path, serde_json::to_string_pretty(&report)?)
        .await
        .with_context(|| format!("write report {}", report_path.display()))?;

    println!(
        "Report written to {} (events={}, rss_ok={}, latency_ok={})",
        report_path.display(),
        run_result.event_count,
        run_result.rss_ok,
        run_result.latency_ok
    );
    println!(
        "Reactor activity: enabled={}, enqueued_total={}, dispatched_total={}, active_shards={}, ok={}",
        reactor_activity.enabled,
        reactor_activity.enqueued_total,
        reactor_activity.dispatched_total,
        reactor_activity.active_shards,
        reactor_activity.ok
    );

    ensure_reactor_activity(args.require_reactor_activity, reactor_activity)?;

    Ok(())
}

struct RunResult {
    initial_rss_bytes: u64,
    max_rss_bytes: u64,
    rss_growth_pct: Option<f64>,
    resource_samples: Vec<ResourceSample>,
    max_cpu_usage_pct: f32,
    latencies_us: Vec<u64>,
    p99_first: Option<u64>,
    p99_last: Option<u64>,
    event_count: u64,
    target_event_count: u64,
    min_event_count: u64,
    error_count: u64,
    errors: Vec<String>,
    rss_ok: bool,
    latency_ok: bool,
    events_ok: bool,
    reactor: Value,
}

impl RunResult {
    #[allow(clippy::cast_precision_loss)]
    fn mean_cpu_usage_pct(&self) -> Option<f64> {
        if self.resource_samples.is_empty() {
            return None;
        }
        let total = self
            .resource_samples
            .iter()
            .map(|sample| f64::from(sample.process_cpu_pct))
            .sum::<f64>();
        Some(total / self.resource_samples.len() as f64)
    }

    fn resource_samples_json(&self) -> Vec<Value> {
        self.resource_samples
            .iter()
            .map(ResourceSample::to_json)
            .collect()
    }
}

#[derive(Clone)]
struct ResourceSample {
    t_s: u64,
    rss_bytes: u64,
    process_cpu_pct: f32,
}

impl ResourceSample {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "t_s": self.t_s,
            "rss_bytes": self.rss_bytes,
            "rss_kib": bytes_to_kib(self.rss_bytes),
            "rss_kb": bytes_to_kib(self.rss_bytes),
            "process_cpu_pct": self.process_cpu_pct,
        })
    }
}

fn configure_reactor(
    manager: &ExtensionManager,
    enabled: bool,
    shard_count: usize,
    lane_capacity: usize,
    core_ids: Option<Vec<usize>>,
) {
    manager.disable_hostcall_reactor();
    if enabled {
        manager.enable_hostcall_reactor(HostcallReactorConfig {
            shard_count: shard_count.max(1),
            lane_capacity: lane_capacity.max(1),
            core_ids,
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_profile(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: Option<Value>,
    events_per_sec: u64,
    warmup_secs: u64,
    duration_secs: u64,
    rss_interval_secs: u64,
    reactor_drain_budget: usize,
    dispatch_timeout_ms: u64,
) -> Result<RunResult> {
    if warmup_secs > 0 {
        run_loop(
            manager,
            event,
            payload.clone(),
            events_per_sec,
            Duration::from_secs(warmup_secs),
            rss_interval_secs,
            reactor_drain_budget,
            dispatch_timeout_ms,
            false,
        )
        .await?;
    }

    run_loop(
        manager,
        event,
        payload,
        events_per_sec,
        Duration::from_secs(duration_secs),
        rss_interval_secs,
        reactor_drain_budget,
        dispatch_timeout_ms,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_loop(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: Option<Value>,
    events_per_sec: u64,
    duration: Duration,
    rss_interval_secs: u64,
    reactor_drain_budget: usize,
    dispatch_timeout_ms: u64,
    collect: bool,
) -> Result<RunResult> {
    #[allow(clippy::cast_precision_loss)]
    let interval = Duration::from_secs_f64(1.0 / events_per_sec as f64);
    let start = Instant::now();
    let deadline = start + duration;
    let telemetry_start_index = manager.runtime_hostcall_telemetry_artifact().entries.len();
    let mut next_event = start;

    let mut resource_probe = ResourceProbe::new();
    let initial_sample = resource_probe.sample(0);
    let initial_rss_bytes = initial_sample.rss_bytes;
    let initial_cpu_usage_pct = initial_sample.process_cpu_pct;
    let mut max_rss_bytes = initial_rss_bytes;
    let mut max_cpu_usage_pct = initial_cpu_usage_pct;
    let mut resource_samples = if collect {
        vec![initial_sample]
    } else {
        Vec::new()
    };
    let mut next_rss = if rss_interval_secs == 0 {
        None
    } else {
        Some(start + Duration::from_secs(rss_interval_secs))
    };

    let mut latencies_us = Vec::new();
    let mut errors = Vec::new();
    let mut error_count: u64 = 0;
    let mut event_count: u64 = 0;
    let mut reactor_queue_samples = Vec::new();
    push_reactor_queue_sample(manager, Duration::from_secs(0), &mut reactor_queue_samples);

    while Instant::now() < deadline {
        let now = Instant::now();
        if now < next_event {
            let Some(delay) = next_event_sleep_duration(now, next_event, deadline) else {
                break;
            };
            std::thread::park_timeout(delay.min(Duration::from_millis(1)));
            continue;
        }

        let dispatch_start = Instant::now();
        if let Err(err) = dispatch_event_with_harness_timeout(
            manager,
            event,
            payload.clone(),
            dispatch_timeout_ms,
        )
        .await
        {
            error_count += 1;
            if errors.len() < 5 {
                errors.push(err.to_string());
            }
        }
        if reactor_drain_budget > 0 {
            let _ = manager.reactor_drain_global(reactor_drain_budget);
        }
        let elapsed_us = u64::try_from(dispatch_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        if collect {
            latencies_us.push(elapsed_us);
        }
        event_count += 1;

        next_event += interval;
        let catch_up = Instant::now();
        if next_event < catch_up {
            next_event = catch_up + interval;
        }

        if let Some(next_rss_due) = next_rss
            && Instant::now() >= next_rss_due
        {
            let sample = resource_probe.sample(start.elapsed().as_secs());
            if sample.rss_bytes > max_rss_bytes {
                max_rss_bytes = sample.rss_bytes;
            }
            if sample.process_cpu_pct > max_cpu_usage_pct {
                max_cpu_usage_pct = sample.process_cpu_pct;
            }
            if collect {
                resource_samples.push(sample);
            }
            if collect {
                push_reactor_queue_sample(manager, start.elapsed(), &mut reactor_queue_samples);
            }
            next_rss = Some(next_rss_due + Duration::from_secs(rss_interval_secs));
        }
    }

    let (p99_first, p99_last) = if collect {
        p99_first_last(&latencies_us)
    } else {
        (None, None)
    };

    let rss_growth_pct = if initial_rss_bytes > 0 {
        #[allow(clippy::cast_precision_loss)]
        let growth =
            (max_rss_bytes.saturating_sub(initial_rss_bytes) as f64) / (initial_rss_bytes as f64);
        Some(growth)
    } else {
        None
    };

    let rss_ok = rss_growth_pct.is_none_or(|growth| growth <= 0.10);
    let latency_ok = match (p99_first, p99_last) {
        (Some(first), Some(last)) if first > 0 => last <= first.saturating_mul(2),
        _ => true,
    };
    let target_event_count = events_per_sec.saturating_mul(duration.as_secs());
    let min_event_count = if target_event_count == 0 {
        1
    } else {
        target_event_count.saturating_mul(9).div_ceil(10)
    };
    let events_ok = event_count >= min_event_count && error_count == 0;
    if collect {
        push_reactor_queue_sample(manager, start.elapsed(), &mut reactor_queue_samples);
    }
    let reactor = if collect {
        build_reactor_report(manager, &reactor_queue_samples, telemetry_start_index)
    } else {
        serde_json::json!({
            "enabled": manager.hostcall_reactor_enabled(),
            "queue_samples": [],
            "stall_reasons": {},
            "migration_events": {
                "total": 0,
                "by_transition": {},
            },
        })
    };

    Ok(RunResult {
        initial_rss_bytes,
        max_rss_bytes,
        rss_growth_pct,
        resource_samples,
        max_cpu_usage_pct,
        latencies_us,
        p99_first,
        p99_last,
        event_count,
        target_event_count,
        min_event_count,
        error_count,
        errors,
        rss_ok,
        latency_ok,
        events_ok,
        reactor,
    })
}

fn next_event_sleep_duration(
    now: Instant,
    next_event: Instant,
    deadline: Instant,
) -> Option<Duration> {
    if now >= deadline || next_event >= deadline {
        return None;
    }
    next_event.checked_duration_since(now)
}

async fn dispatch_event_with_harness_timeout(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: Option<Value>,
    dispatch_timeout_ms: u64,
) -> Result<()> {
    if dispatch_timeout_ms == 0 {
        return manager
            .dispatch_event(event, payload)
            .await
            .map_err(Into::into);
    }

    timeout(
        wall_now(),
        Duration::from_millis(dispatch_timeout_ms),
        Box::pin(manager.dispatch_event(event, payload)),
    )
    .await
    .map_or_else(
        |_| {
            Err(anyhow::anyhow!(
                "event dispatch timed out after {dispatch_timeout_ms}ms"
            ))
        },
        |result| result.map_err(Into::into),
    )
}

struct ResourceProbe {
    last_process_jiffies: Option<u64>,
    last_total_jiffies: Option<u64>,
    logical_cpus: usize,
}

impl ResourceProbe {
    fn new() -> Self {
        Self {
            last_process_jiffies: None,
            last_total_jiffies: None,
            logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn sample(&mut self, t_s: u64) -> ResourceSample {
        let rss_bytes = read_self_rss_bytes().unwrap_or(0);
        let process_jiffies = read_self_process_jiffies().unwrap_or(0);
        let total_jiffies = read_total_cpu_jiffies().unwrap_or(0);
        let process_cpu_pct = match (self.last_process_jiffies, self.last_total_jiffies) {
            (Some(last_process), Some(last_total)) => {
                let process_delta = process_jiffies.saturating_sub(last_process) as f64;
                let total_delta = total_jiffies.saturating_sub(last_total) as f64;
                if total_delta > 0.0 {
                    ((process_delta / total_delta) * self.logical_cpus as f64 * 100.0) as f32
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        self.last_process_jiffies = Some(process_jiffies);
        self.last_total_jiffies = Some(total_jiffies);
        ResourceSample {
            t_s,
            rss_bytes,
            process_cpu_pct,
        }
    }
}

fn read_self_rss_bytes() -> Option<u64> {
    parse_vmrss_bytes(&std::fs::read_to_string("/proc/self/status").ok()?)
}

fn read_self_process_jiffies() -> Option<u64> {
    parse_process_jiffies(&std::fs::read_to_string("/proc/self/stat").ok()?)
}

fn read_total_cpu_jiffies() -> Option<u64> {
    parse_total_cpu_jiffies(&std::fs::read_to_string("/proc/stat").ok()?)
}

fn parse_vmrss_bytes(status: &str) -> Option<u64> {
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let kib = fields.next()?.parse::<u64>().ok()?;
        return Some(kib.saturating_mul(1024));
    }
    None
}

fn parse_process_jiffies(stat: &str) -> Option<u64> {
    let close_paren = stat.rfind(')')?;
    let rest = stat.get(close_paren + 1..)?;
    let mut fields = rest.split_whitespace();
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

fn parse_total_cpu_jiffies(stat: &str) -> Option<u64> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let mut total = 0_u64;
    for field in line.split_whitespace().skip(1) {
        total = total.saturating_add(field.parse::<u64>().ok()?);
    }
    Some(total)
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn default_manifest_path() -> PathBuf {
    project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

fn default_artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn default_event_payloads_path() -> PathBuf {
    project_root().join("tests/ext_conformance/event_payloads/event_payloads.json")
}

fn default_report_path() -> PathBuf {
    project_root().join("tests/perf/reports/ext_stress_report.json")
}

fn default_events_path() -> PathBuf {
    project_root().join("tests/perf/reports/ext_stress_events.jsonl")
}

fn default_run_id() -> String {
    std::env::var("CI_RUN_ID")
        .or_else(|_| std::env::var("GITHUB_RUN_ID"))
        .unwrap_or_else(|_| format!("local-{}", Utc::now().format("%Y%m%dT%H%M%SZ")))
}

const fn bytes_to_kib(bytes: u64) -> u64 {
    bytes / 1024
}

async fn write_events_jsonl(
    events_path: &Path,
    report: &Value,
    run_result: &RunResult,
) -> Result<()> {
    if let Some(parent) = events_path.parent() {
        asupersync::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create events directory {}", parent.display()))?;
    }

    let run_id = report
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let correlation_id = report
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines = Vec::with_capacity(run_result.resource_samples.len() + 1);
    for sample in &run_result.resource_samples {
        lines.push(serde_json::to_string(&serde_json::json!({
            "schema": "pi.ext.stress_resource_sample.v1",
            "run_id": run_id,
            "correlation_id": correlation_id,
            "ts": generated_at,
            "t_s": sample.t_s,
            "rss_bytes": sample.rss_bytes,
            "rss_kib": bytes_to_kib(sample.rss_bytes),
            "rss_kb": bytes_to_kib(sample.rss_bytes),
            "process_cpu_pct": sample.process_cpu_pct,
        }))?);
    }
    lines.push(serde_json::to_string(&serde_json::json!({
        "schema": "pi.ext.stress_summary.v1",
        "run_id": run_id,
        "correlation_id": correlation_id,
        "ts": generated_at,
        "event_count": run_result.event_count,
        "target_event_count": run_result.target_event_count,
        "min_event_count": run_result.min_event_count,
        "error_count": run_result.error_count,
        "events_ok": run_result.events_ok,
        "rss": report.get("rss"),
        "cpu": report.get("cpu"),
        "latency_us": report.get("latency_us"),
        "reactor": report.get("reactor"),
        "pass": report.get("pass"),
    }))?);

    asupersync::fs::write(events_path, lines.join("\n") + "\n")
        .await
        .with_context(|| format!("write events {}", events_path.display()))?;
    Ok(())
}

fn push_reactor_queue_sample(
    manager: &ExtensionManager,
    elapsed: Duration,
    samples: &mut Vec<Value>,
) {
    let Some(telemetry) = manager.reactor_telemetry() else {
        return;
    };
    samples.push(serde_json::json!({
        "t_s": elapsed.as_secs(),
        "queue_depths": telemetry.queue_depths,
        "max_queue_depths": telemetry.max_queue_depths,
        "total_enqueued_by_shard": telemetry.total_enqueued,
        "rejected_enqueues": telemetry.rejected_enqueues,
        "total_dispatched": telemetry.total_dispatched,
    }));
}

fn build_s3fifo_report(
    stall_reasons: &BTreeMap<String, u64>,
    lane_overflow_rejections: u64,
) -> Value {
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
    serde_json::json!({
        "mode": mode,
        "fallback_reason": fallback_reason,
        "fairness_budget_rejections": fairness_budget_rejections,
        "lane_overflow_rejections": lane_overflow_rejections,
        "fallback_event_total": fallback_event_total,
    })
}

fn build_bravo_report() -> Value {
    serde_json::json!({
        "mode": "Balanced",
        "transitions": 0,
        "rollbacks": 0,
        "writer_recovery_remaining": 0,
    })
}

fn build_reactor_report(
    manager: &ExtensionManager,
    queue_samples: &[Value],
    telemetry_start_index: usize,
) -> Value {
    let telemetry_artifact = manager.runtime_hostcall_telemetry_artifact();
    let entries = telemetry_artifact.entries;
    let start = telemetry_start_index.min(entries.len());
    let mut stall_reasons = BTreeMap::<String, u64>::new();
    let mut migration_events = BTreeMap::<String, u64>::new();
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
            let count = migration_events.entry(key).or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    let migration_total = migration_events.values().copied().sum::<u64>();

    let Some(reactor) = manager.reactor_telemetry() else {
        let s3fifo = build_s3fifo_report(&stall_reasons, 0);
        let bravo = build_bravo_report();
        return serde_json::json!({
            "enabled": false,
            "queue_samples": queue_samples,
            "stall_reasons": stall_reasons,
            "migration_events": {
                "total": migration_total,
                "by_transition": migration_events,
            },
            "s3fifo": s3fifo,
            "bravo": bravo,
        });
    };

    if reactor.rejected_enqueues > 0 {
        let count = stall_reasons
            .entry("lane_overflow".to_string())
            .or_insert(0);
        *count = count.saturating_add(reactor.rejected_enqueues);
    }

    let s3fifo = build_s3fifo_report(&stall_reasons, reactor.rejected_enqueues);
    let bravo = build_bravo_report();
    serde_json::json!({
        "enabled": true,
        "shard_count": reactor.shard_count,
        "numa_pool_active": reactor.numa_pool_active,
        "affinity_advisory_count": reactor.affinity_advisory_count,
        "queue_depths_final": reactor.queue_depths,
        "max_queue_depths": reactor.max_queue_depths,
        "total_enqueued_by_shard": reactor.total_enqueued,
        "rejected_enqueues": reactor.rejected_enqueues,
        "total_dispatched": reactor.total_dispatched,
        "queue_samples": queue_samples,
        "stall_reasons": stall_reasons,
        "migration_events": {
            "total": migration_total,
            "by_transition": migration_events,
        },
        "s3fifo": s3fifo,
        "bravo": bravo,
    })
}

#[allow(clippy::cast_precision_loss)]
fn throughput_eps(event_count: u64, duration_secs: u64) -> f64 {
    if duration_secs == 0 {
        return 0.0;
    }
    event_count as f64 / duration_secs as f64
}

#[allow(clippy::cast_precision_loss)]
fn pct_delta(baseline: f64, candidate: f64) -> Option<f64> {
    if baseline <= 0.0 {
        return None;
    }
    Some(((candidate - baseline) / baseline) * 100.0)
}

fn signed_delta_u64(candidate: u64, baseline: u64) -> i64 {
    if candidate >= baseline {
        i64::try_from(candidate - baseline).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(baseline - candidate).unwrap_or(i64::MAX)
    }
}

fn value_u64_at_path(value: &Value, path: &[&str]) -> u64 {
    let mut cursor = value;
    for segment in path {
        let Some(next) = cursor.get(*segment) else {
            return 0;
        };
        cursor = next;
    }
    cursor.as_u64().unwrap_or(0)
}

fn sum_u64_array_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(0, |items| items.iter().filter_map(Value::as_u64).sum())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReactorActivity {
    enabled: bool,
    enqueued_total: u64,
    dispatched_total: u64,
    active_shards: usize,
    ok: bool,
    failure_reason: Option<&'static str>,
}

impl ReactorActivity {
    fn from_report(reactor: &Value) -> Self {
        let enabled = reactor
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let enqueued_total = sum_u64_array_field(reactor, "total_enqueued_by_shard");
        let dispatched_total = value_u64_at_path(reactor, &["total_dispatched"]);
        let active_shards = reactor
            .get("total_enqueued_by_shard")
            .and_then(Value::as_array)
            .map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| item.as_u64().unwrap_or(0) > 0)
                    .count()
            });
        let failure_reason = if !enabled {
            Some("reactor_disabled")
        } else if enqueued_total == 0 {
            Some("no_reactor_enqueues")
        } else if dispatched_total == 0 {
            Some("no_reactor_dispatches")
        } else {
            None
        };

        Self {
            enabled,
            enqueued_total,
            dispatched_total,
            active_shards,
            ok: failure_reason.is_none(),
            failure_reason,
        }
    }

    fn to_json(self) -> Value {
        serde_json::json!({
            "enabled": self.enabled,
            "enqueued_total": self.enqueued_total,
            "dispatched_total": self.dispatched_total,
            "active_shards": self.active_shards,
            "ok": self.ok,
            "failure_reason": self.failure_reason,
        })
    }
}

fn ensure_reactor_activity(required: bool, activity: ReactorActivity) -> Result<()> {
    if !required || activity.ok {
        return Ok(());
    }
    bail!(
        "--require-reactor-activity requested but reactor activity check failed: {} \
         (enqueued_total={}, dispatched_total={})",
        activity.failure_reason.unwrap_or("unknown"),
        activity.enqueued_total,
        activity.dispatched_total
    );
}

fn build_shard_comparison_report(
    baseline: &RunResult,
    candidate: &RunResult,
    duration_secs: u64,
    baseline_shards: usize,
    candidate_shards: usize,
) -> Value {
    let baseline_latency = summarize_us(&baseline.latencies_us);
    let candidate_latency = summarize_us(&candidate.latencies_us);
    let baseline_throughput = throughput_eps(baseline.event_count, duration_secs);
    let candidate_throughput = throughput_eps(candidate.event_count, duration_secs);

    let baseline_p95 = baseline_latency.get("p95").and_then(Value::as_u64);
    let candidate_p95 = candidate_latency.get("p95").and_then(Value::as_u64);
    let baseline_p99 = baseline_latency.get("p99").and_then(Value::as_u64);
    let candidate_p99 = candidate_latency.get("p99").and_then(Value::as_u64);
    let baseline_p999 = baseline_latency.get("p999").and_then(Value::as_u64);
    let candidate_p999 = candidate_latency.get("p999").and_then(Value::as_u64);

    let baseline_rejected = value_u64_at_path(&baseline.reactor, &["rejected_enqueues"]);
    let candidate_rejected = value_u64_at_path(&candidate.reactor, &["rejected_enqueues"]);
    let baseline_max_depth = sum_u64_array_field(&baseline.reactor, "max_queue_depths");
    let candidate_max_depth = sum_u64_array_field(&candidate.reactor, "max_queue_depths");
    let baseline_lane_overflow =
        value_u64_at_path(&baseline.reactor, &["stall_reasons", "lane_overflow"]);
    let candidate_lane_overflow =
        value_u64_at_path(&candidate.reactor, &["stall_reasons", "lane_overflow"]);

    let throughput_gain_pct = pct_delta(baseline_throughput, candidate_throughput);
    let p95_improved = match (baseline_p95, candidate_p95) {
        (Some(base), Some(curr)) => curr <= base,
        _ => false,
    };
    let p99_improved = match (baseline_p99, candidate_p99) {
        (Some(base), Some(curr)) => curr <= base,
        _ => false,
    };
    let extreme_tail_improved = match (baseline_p999, candidate_p999) {
        (Some(base), Some(curr)) => curr <= base,
        _ => false,
    };
    let contention_improved =
        candidate_rejected <= baseline_rejected && candidate_max_depth <= baseline_max_depth;

    serde_json::json!({
        "mode": "shard_baseline_vs_reactor_mesh",
        "baseline": {
            "shards": baseline_shards,
            "events": {
                "count": baseline.event_count,
                "target_count": baseline.target_event_count,
                "min_count": baseline.min_event_count,
                "errors": baseline.error_count,
                "events_ok": baseline.events_ok,
            },
            "throughput_eps": baseline_throughput,
            "latency_us": baseline_latency,
            "reactor": baseline.reactor.clone(),
        },
        "candidate": {
            "shards": candidate_shards,
            "events": {
                "count": candidate.event_count,
                "target_count": candidate.target_event_count,
                "min_count": candidate.min_event_count,
                "errors": candidate.error_count,
                "events_ok": candidate.events_ok,
            },
            "throughput_eps": candidate_throughput,
            "latency_us": candidate_latency,
            "reactor": candidate.reactor.clone(),
        },
        "delta": {
            "throughput_eps": candidate_throughput - baseline_throughput,
            "throughput_gain_pct": throughput_gain_pct,
            "p95_us": match (baseline_p95, candidate_p95) {
                (Some(base), Some(curr)) => Some(signed_delta_u64(curr, base)),
                _ => None,
            },
            "p99_us": match (baseline_p99, candidate_p99) {
                (Some(base), Some(curr)) => Some(signed_delta_u64(curr, base)),
                _ => None,
            },
            "p999_us": match (baseline_p999, candidate_p999) {
                (Some(base), Some(curr)) => Some(signed_delta_u64(curr, base)),
                _ => None,
            },
            "rejected_enqueues": signed_delta_u64(candidate_rejected, baseline_rejected),
            "max_queue_depth_total": signed_delta_u64(candidate_max_depth, baseline_max_depth),
            "lane_overflow_stalls": signed_delta_u64(candidate_lane_overflow, baseline_lane_overflow),
        },
        "improved": {
            "throughput": candidate_throughput >= baseline_throughput,
            "p95": p95_improved,
            "p99": p99_improved,
            "p999": extreme_tail_improved,
            "contention_proxy": contention_improved,
        }
    })
}

fn extensions_by_tier(manifest_path: &Path, tier: &str) -> Result<Vec<(String, String)>> {
    let data = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let json: Value =
        serde_json::from_str(&data).with_context(|| "parse manifest JSON".to_string())?;
    let extensions = json["extensions"]
        .as_array()
        .context("manifest.extensions should be an array")?;

    let mut out = Vec::new();
    for entry in extensions {
        if entry["source_tier"].as_str() != Some(tier) {
            continue;
        }
        let entry_path = entry["entry_path"]
            .as_str()
            .context("missing entry_path in manifest entry")?;
        let path = Path::new(entry_path);
        let mut components = path.components();
        let Some(root) = components.next() else {
            continue;
        };
        let extension_dir = root.as_os_str().to_string_lossy().to_string();
        let remaining = components.as_path().to_string_lossy().to_string();
        let entry_file = if remaining.is_empty() {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(entry_path)
                .to_string()
        } else {
            remaining
        };
        out.push((extension_dir, entry_file));
    }
    Ok(out)
}

fn build_specs(
    artifacts_dir: &Path,
    entries: &[(String, String)],
) -> Result<(Vec<JsExtensionLoadSpec>, Vec<String>)> {
    let mut specs = Vec::new();
    let mut names = Vec::new();
    for (extension_dir, entry_file) in entries {
        let ext_path = artifacts_dir.join(extension_dir).join(entry_file);
        let spec = JsExtensionLoadSpec::from_entry_path(&ext_path)
            .with_context(|| format!("build load spec for {}", ext_path.display()))?;
        specs.push(spec);
        names.push(format!("{extension_dir}/{entry_file}"));
    }
    Ok((specs, names))
}

fn build_synthetic_hostcall_specs(
    root: &Path,
    count: usize,
) -> Result<(Vec<JsExtensionLoadSpec>, Vec<String>)> {
    if count == 0 {
        bail!("synthetic hostcall extension count must be > 0");
    }
    std::fs::create_dir_all(root)
        .with_context(|| format!("create synthetic extension dir {}", root.display()))?;
    let mut specs = Vec::with_capacity(count);
    let mut names = Vec::with_capacity(count);
    for idx in 0..count {
        let path = root.join(format!("synthetic-hostcall-{idx}.js"));
        std::fs::write(&path, synthetic_hostcall_source(idx))
            .with_context(|| format!("write synthetic extension {}", path.display()))?;
        specs.push(
            JsExtensionLoadSpec::from_entry_path(&path)
                .with_context(|| format!("build synthetic load spec for {}", path.display()))?,
        );
        names.push(format!("synthetic-hostcall-{idx}.js"));
    }
    Ok((specs, names))
}

fn synthetic_hostcall_source(idx: usize) -> String {
    format!(
        r#"export default function syntheticHostcall{idx}(pi) {{
  pi.on("agent_start", async () => {{
    await pi.events("list_flags", {{}});
    return undefined;
  }});
}}
"#
    )
}

fn event_payload_for(
    payloads_path: &Path,
    event_name: &str,
    index: usize,
) -> Result<Option<Value>> {
    let data = std::fs::read_to_string(payloads_path)
        .with_context(|| format!("read payloads {}", payloads_path.display()))?;
    let json: Value =
        serde_json::from_str(&data).with_context(|| "parse payloads JSON".to_string())?;
    let payloads = json["event_payloads"]
        .as_object()
        .context("event_payloads should be an object")?;
    let Some(list) = payloads.get(event_name).and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(entry) = list.get(index) else {
        bail!("payload index {index} out of range for event {event_name}");
    };
    Ok(entry.get("payload").cloned())
}

fn parse_event_name(name: &str) -> Result<ExtensionEventName> {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "startup" => Ok(ExtensionEventName::Startup),
        "input" => Ok(ExtensionEventName::Input),
        "before_agent_start" => Ok(ExtensionEventName::BeforeAgentStart),
        "context" => Ok(ExtensionEventName::Context),
        "agent_start" => Ok(ExtensionEventName::AgentStart),
        "agent_end" => Ok(ExtensionEventName::AgentEnd),
        "turn_start" => Ok(ExtensionEventName::TurnStart),
        "turn_end" => Ok(ExtensionEventName::TurnEnd),
        "message_start" => Ok(ExtensionEventName::MessageStart),
        "message_update" => Ok(ExtensionEventName::MessageUpdate),
        "message_end" => Ok(ExtensionEventName::MessageEnd),
        "tool_execution_start" => Ok(ExtensionEventName::ToolExecutionStart),
        "tool_execution_update" => Ok(ExtensionEventName::ToolExecutionUpdate),
        "tool_execution_end" => Ok(ExtensionEventName::ToolExecutionEnd),
        "tool_call" => Ok(ExtensionEventName::ToolCall),
        "tool_result" => Ok(ExtensionEventName::ToolResult),
        "session_start" => Ok(ExtensionEventName::SessionStart),
        "session_before_switch" => Ok(ExtensionEventName::SessionBeforeSwitch),
        "session_switch" => Ok(ExtensionEventName::SessionSwitch),
        "session_before_fork" => Ok(ExtensionEventName::SessionBeforeFork),
        "session_fork" => Ok(ExtensionEventName::SessionFork),
        "session_before_compact" => Ok(ExtensionEventName::SessionBeforeCompact),
        "session_compact" => Ok(ExtensionEventName::SessionCompact),
        "resources_discover" => Ok(ExtensionEventName::ResourcesDiscover),
        "model_select" => Ok(ExtensionEventName::ModelSelect),
        "user_bash" => Ok(ExtensionEventName::UserBash),
        "session_before_tree" => Ok(ExtensionEventName::SessionBeforeTree),
        "session_tree" => Ok(ExtensionEventName::SessionTree),
        "session_shutdown" => Ok(ExtensionEventName::SessionShutdown),
        other => bail!("Unsupported event name: {other}"),
    }
}

fn parse_reactor_core_ids(value: Option<&str>) -> Result<Option<Vec<usize>>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mut ids = Vec::new();
    for raw in value.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            bail!("--reactor-core-ids contains an empty core id");
        }
        let parsed = item
            .parse::<usize>()
            .with_context(|| format!("parse --reactor-core-ids entry {item:?}"))?;
        ids.push(parsed);
    }
    Ok(Some(ids))
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let rank = (len * numerator).saturating_add(denominator - 1) / denominator;
    rank.saturating_sub(1).min(len - 1)
}

fn summarize_us(values: &[u64]) -> Value {
    if values.is_empty() {
        return serde_json::json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[percentile_index(sorted.len(), 50, 100)];
    let p95 = sorted[percentile_index(sorted.len(), 95, 100)];
    let p99 = sorted[percentile_index(sorted.len(), 99, 100)];
    let p999 = sorted[percentile_index(sorted.len(), 999, 1000)];
    let min = sorted.first().copied().unwrap_or(0);
    let max = sorted.last().copied().unwrap_or(0);
    let sum: u128 = sorted.iter().map(|v| u128::from(*v)).sum();
    let mean = u64::try_from(sum / (sorted.len() as u128)).unwrap_or(u64::MAX);
    serde_json::json!({
        "count": sorted.len(),
        "min": min,
        "max": max,
        "mean": mean,
        "p50": p50,
        "p95": p95,
        "p99": p99,
        "p999": p999,
    })
}

fn p99_first_last(values: &[u64]) -> (Option<u64>, Option<u64>) {
    if values.is_empty() {
        return (None, None);
    }
    let len = values.len();
    let window = (len / 10).max(1);
    let first = &values[..window];
    let last = &values[len.saturating_sub(window)..];
    let p99_first = summarize_us(first).get("p99").and_then(Value::as_u64);
    let p99_last = summarize_us(last).get("p99").and_then(Value::as_u64);
    (p99_first, p99_last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_result(event_count: u64, latencies_us: Vec<u64>, reactor: Value) -> RunResult {
        RunResult {
            initial_rss_bytes: 100,
            max_rss_bytes: 110,
            rss_growth_pct: Some(0.10),
            resource_samples: Vec::new(),
            max_cpu_usage_pct: 0.0,
            latencies_us,
            p99_first: None,
            p99_last: None,
            event_count,
            target_event_count: event_count,
            min_event_count: event_count,
            error_count: 0,
            errors: Vec::new(),
            rss_ok: true,
            latency_ok: true,
            events_ok: event_count > 0,
            reactor,
        }
    }

    #[test]
    fn summarize_us_includes_p95() {
        let values = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let summary = summarize_us(&values);
        assert_eq!(summary["p95"].as_u64(), Some(100));
        assert_eq!(summary["p99"].as_u64(), Some(100));
        assert_eq!(summary["p999"].as_u64(), Some(100));
    }

    #[test]
    fn shard_comparison_reports_improvement_deltas() {
        let baseline = synthetic_result(
            100,
            vec![300, 300, 300, 300, 300],
            serde_json::json!({
                "rejected_enqueues": 8,
                "max_queue_depths": [20, 20],
                "stall_reasons": { "lane_overflow": 5 }
            }),
        );
        let candidate = synthetic_result(
            140,
            vec![180, 180, 180, 180, 180],
            serde_json::json!({
                "rejected_enqueues": 2,
                "max_queue_depths": [8, 8, 8, 8],
                "stall_reasons": { "lane_overflow": 1 }
            }),
        );

        let report = build_shard_comparison_report(&baseline, &candidate, 10, 1, 4);
        assert_eq!(
            report["improved"]["throughput"].as_bool(),
            Some(true),
            "throughput should improve"
        );
        assert_eq!(
            report["improved"]["p99"].as_bool(),
            Some(true),
            "p99 should improve"
        );
        assert_eq!(
            report["improved"]["p999"].as_bool(),
            Some(true),
            "p999 should improve"
        );
        assert_eq!(
            report["delta"]["rejected_enqueues"].as_i64(),
            Some(-6),
            "rejected enqueues should drop"
        );
        let gain = report["delta"]["throughput_gain_pct"]
            .as_f64()
            .expect("gain pct");
        assert!(gain > 0.0, "throughput gain should be positive");
    }

    #[test]
    fn reactor_report_includes_stable_hostcall_queue_fields() {
        let manager = ExtensionManager::new();
        let report = build_reactor_report(&manager, &[], 0);

        assert_eq!(report["s3fifo"]["mode"].as_str(), Some("Active"));
        assert_eq!(
            report["s3fifo"]["fairness_budget_rejections"].as_u64(),
            Some(0)
        );
        assert_eq!(
            report["s3fifo"]["lane_overflow_rejections"].as_u64(),
            Some(0)
        );
        assert_eq!(report["s3fifo"]["fallback_event_total"].as_u64(), Some(0));
        assert_eq!(report["bravo"]["mode"].as_str(), Some("Balanced"));
        assert_eq!(report["bravo"]["transitions"].as_u64(), Some(0));
        assert_eq!(report["bravo"]["rollbacks"].as_u64(), Some(0));
    }

    #[test]
    fn resource_sample_reports_bytes_and_kib() {
        let sample = ResourceSample {
            t_s: 7,
            rss_bytes: 65_536,
            process_cpu_pct: 12.5,
        };
        let json = sample.to_json();
        assert_eq!(json["rss_bytes"].as_u64(), Some(65_536));
        assert_eq!(json["rss_kib"].as_u64(), Some(64));
        assert_eq!(json["rss_kb"].as_u64(), Some(64));
        assert_eq!(json["process_cpu_pct"].as_f64(), Some(12.5));
    }

    #[test]
    fn parse_reactor_core_ids_accepts_comma_list() {
        assert_eq!(
            parse_reactor_core_ids(Some("0, 16,32, 48")).unwrap(),
            Some(vec![0, 16, 32, 48])
        );
    }

    #[test]
    fn parse_reactor_core_ids_empty_is_none() {
        assert_eq!(parse_reactor_core_ids(None).unwrap(), None);
        assert_eq!(parse_reactor_core_ids(Some("  ")).unwrap(), None);
    }

    #[test]
    fn parse_reactor_core_ids_rejects_empty_entry() {
        let err = parse_reactor_core_ids(Some("0,,2")).unwrap_err();
        assert!(
            err.to_string().contains("empty core id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn synthetic_hostcall_source_uses_agent_start_events_hostcall() {
        let source = synthetic_hostcall_source(3);
        assert!(source.contains("syntheticHostcall3"));
        assert!(source.contains("agent_start"));
        assert!(source.contains(r#"pi.events("list_flags", {})"#));
    }

    #[test]
    fn event_pacer_does_not_sleep_at_run_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(4);
        let before_deadline = start + Duration::from_secs(3);

        assert_eq!(
            next_event_sleep_duration(start, before_deadline, deadline),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            next_event_sleep_duration(before_deadline, deadline, deadline),
            None
        );
        assert_eq!(
            next_event_sleep_duration(deadline, deadline, deadline),
            None
        );
    }

    #[test]
    fn require_reactor_activity_fails_when_counters_stay_zero() {
        let activity = ReactorActivity::from_report(&serde_json::json!({
            "enabled": true,
            "total_enqueued_by_shard": [0, 0, 0, 0],
            "total_dispatched": 0,
        }));
        let err = ensure_reactor_activity(true, activity).unwrap_err();
        assert!(
            err.to_string().contains("no_reactor_enqueues"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn require_reactor_activity_fails_when_dispatches_stay_zero() {
        let activity = ReactorActivity::from_report(&serde_json::json!({
            "enabled": true,
            "total_enqueued_by_shard": [2, 0, 3, 0],
            "total_dispatched": 0,
        }));
        let err = ensure_reactor_activity(true, activity).unwrap_err();
        assert!(
            err.to_string().contains("no_reactor_dispatches"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn require_reactor_activity_accepts_enqueues_and_dispatches() {
        let activity = ReactorActivity::from_report(&serde_json::json!({
            "enabled": true,
            "total_enqueued_by_shard": [2, 0, 3, 0],
            "total_dispatched": 5,
        }));
        assert!(activity.ok);
        assert_eq!(activity.enqueued_total, 5);
        assert_eq!(activity.dispatched_total, 5);
        assert_eq!(activity.active_shards, 2);
        assert!(ensure_reactor_activity(true, activity).is_ok());
    }

    #[test]
    fn proc_status_parser_extracts_vmrss_bytes() {
        let status = "Name:\text_stress\nVmRSS:\t   1234 kB\nVmSize:\t9999 kB\n";
        assert_eq!(parse_vmrss_bytes(status), Some(1_263_616));
    }

    #[test]
    fn proc_stat_parser_handles_spaces_in_comm() {
        let stat = "12345 (name with spaces) S 1 2 3 4 5 6 7 8 9 10 21 34 0 0 0";
        assert_eq!(parse_process_jiffies(stat), Some(55));
    }

    #[test]
    fn proc_cpu_parser_sums_total_jiffies() {
        let stat = "cpu  1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4\n";
        assert_eq!(parse_total_cpu_jiffies(stat), Some(55));
    }
}
