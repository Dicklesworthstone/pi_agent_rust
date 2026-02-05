//! Extension stress test harness: memory/RSS + event dispatch latency.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use asupersync::time::{sleep, wall_now};
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use pi::extensions::{ExtensionEventName, ExtensionManager, JsExtensionLoadSpec};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, RefreshKind, System, get_current_pid};

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
    let report_path = args.report_path.unwrap_or_else(default_report_path);

    let mut entries = extensions_by_tier(&manifest_path, &args.tier)?;
    if let Some(max) = args.max_extensions {
        entries.truncate(max);
    }
    let limited_entries = entries;

    if limited_entries.is_empty() {
        bail!("No extensions found for tier {}", args.tier);
    }

    let (specs, names) = build_specs(&artifacts_dir, &limited_entries)?;
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

    if args.warmup_secs > 0 {
        run_loop(
            &manager,
            event,
            payload.clone(),
            args.events_per_sec,
            Duration::from_secs(args.warmup_secs),
            args.rss_interval_secs,
            false,
        )
        .await?;
    }

    let run_result = run_loop(
        &manager,
        event,
        payload,
        args.events_per_sec,
        Duration::from_secs(args.duration_secs),
        args.rss_interval_secs,
        true,
    )
    .await?;

    let report = serde_json::json!({
        "schema": "pi.ext.stress_profile.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "config": {
            "tier": args.tier,
            "event": args.event,
            "payload_index": args.payload_index,
            "duration_secs": args.duration_secs,
            "warmup_secs": args.warmup_secs,
            "rss_interval_secs": args.rss_interval_secs,
            "events_per_sec": args.events_per_sec,
            "max_extensions": args.max_extensions,
        },
        "extensions": {
            "count": names.len(),
            "names": names,
        },
        "rss": {
            "initial_kb": run_result.initial_rss_kb,
            "max_kb": run_result.max_rss_kb,
            "growth_pct": run_result.rss_growth_pct,
            "samples": run_result.rss_samples,
        },
        "latency_us": {
            "summary": summarize_us(&run_result.latencies_us),
            "p99_first": run_result.p99_first,
            "p99_last": run_result.p99_last,
        },
        "events": {
            "count": run_result.event_count,
            "errors": run_result.error_count,
            "sample_errors": run_result.errors,
        },
        "pass": {
            "rss_ok": run_result.rss_ok,
            "latency_ok": run_result.latency_ok,
            "overall": run_result.rss_ok && run_result.latency_ok,
        }
    });

    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("write report {}", report_path.display()))?;

    println!(
        "Report written to {} (events={}, rss_ok={}, latency_ok={})",
        report_path.display(),
        run_result.event_count,
        run_result.rss_ok,
        run_result.latency_ok
    );

    Ok(())
}

struct RunResult {
    initial_rss_kb: u64,
    max_rss_kb: u64,
    rss_growth_pct: Option<f64>,
    rss_samples: Vec<Value>,
    latencies_us: Vec<u64>,
    p99_first: Option<u64>,
    p99_last: Option<u64>,
    event_count: u64,
    error_count: u64,
    errors: Vec<String>,
    rss_ok: bool,
    latency_ok: bool,
}

async fn run_loop(
    manager: &ExtensionManager,
    event: ExtensionEventName,
    payload: Option<Value>,
    events_per_sec: u64,
    duration: Duration,
    rss_interval_secs: u64,
    collect: bool,
) -> Result<RunResult> {
    #[allow(clippy::cast_precision_loss)]
    let interval = Duration::from_secs_f64(1.0 / events_per_sec as f64);
    let start = Instant::now();
    let mut next_event = start;

    let pid = get_current_pid().map_err(|err| anyhow::anyhow!(err))?;
    let refresh = ProcessRefreshKind::nothing().with_memory();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
    let initial_rss_kb = system.process(pid).map_or(0, sysinfo::Process::memory);
    let mut max_rss_kb = initial_rss_kb;
    let mut rss_samples: Vec<Value> = Vec::new();
    let mut next_rss = if rss_interval_secs == 0 {
        None
    } else {
        Some(start + Duration::from_secs(rss_interval_secs))
    };

    let mut latencies_us = Vec::new();
    let mut errors = Vec::new();
    let mut error_count: u64 = 0;
    let mut event_count: u64 = 0;

    while start.elapsed() < duration {
        let now = Instant::now();
        if now < next_event {
            sleep(wall_now(), next_event - now).await;
            continue;
        }

        let dispatch_start = Instant::now();
        if let Err(err) = manager.dispatch_event(event, payload.clone()).await {
            error_count += 1;
            if errors.len() < 5 {
                errors.push(err.to_string());
            }
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

        if let Some(next_rss_due) = next_rss {
            if Instant::now() >= next_rss_due {
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
                    if collect {
                        rss_samples.push(serde_json::json!({
                            "t_s": start.elapsed().as_secs(),
                            "rss_kb": rss_kb,
                        }));
                    }
                }
                next_rss = Some(next_rss_due + Duration::from_secs(rss_interval_secs));
            }
        }
    }

    let (p99_first, p99_last) = if collect {
        p99_first_last(&latencies_us)
    } else {
        (None, None)
    };

    let rss_growth_pct = if initial_rss_kb > 0 {
        #[allow(clippy::cast_precision_loss)]
        let growth = (max_rss_kb.saturating_sub(initial_rss_kb) as f64) / (initial_rss_kb as f64);
        Some(growth)
    } else {
        None
    };

    let rss_ok = rss_growth_pct.is_none_or(|growth| growth <= 0.10);
    let latency_ok = match (p99_first, p99_last) {
        (Some(first), Some(last)) if first > 0 => last <= first.saturating_mul(2),
        _ => true,
    };

    Ok(RunResult {
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
    })
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
        "session_before_switch" => Ok(ExtensionEventName::SessionBeforeSwitch),
        "session_switch" => Ok(ExtensionEventName::SessionSwitch),
        "session_before_fork" => Ok(ExtensionEventName::SessionBeforeFork),
        "session_fork" => Ok(ExtensionEventName::SessionFork),
        "session_before_compact" => Ok(ExtensionEventName::SessionBeforeCompact),
        "session_compact" => Ok(ExtensionEventName::SessionCompact),
        other => bail!("Unsupported event name: {other}"),
    }
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
    let p99 = sorted[percentile_index(sorted.len(), 99, 100)];
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
        "p99": p99,
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
