//! Provider-registry startup and memory guardrails.
//!
//! Tracks provider-metadata growth impact on startup-facing lookup paths and
//! memory footprint, with machine-readable artifacts for triage.
//!
//! Bead: bd-3uqg.8.6

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use chrono::{SecondsFormat, Utc};
use common::harness::TestHarness;
use pi::provider_metadata::{
    PROVIDER_METADATA, canonical_provider_id, provider_auth_env_keys, provider_metadata,
    provider_routing_defaults,
};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

static GUARDRAIL_LOCK: Mutex<()> = Mutex::new(());

const STARTUP_P95_MS_DEFAULT: f64 = 1.0;
const LOOKUP_P99_US_DEFAULT: f64 = 40.0;
const FOOTPRINT_MB_DEFAULT: f64 = 1.0;
const RSS_DELTA_MB_DEFAULT: f64 = 8.0;

const STARTUP_P95_MS_ENV: &str = "PI_PROVIDER_REGISTRY_STARTUP_P95_MS";
const LOOKUP_P99_US_ENV: &str = "PI_PROVIDER_REGISTRY_LOOKUP_P99_US";
const FOOTPRINT_MB_ENV: &str = "PI_PROVIDER_REGISTRY_FOOTPRINT_MB";
const RSS_DELTA_MB_ENV: &str = "PI_PROVIDER_REGISTRY_RSS_DELTA_MB";

#[derive(Debug, Clone, Serialize)]
struct GuardrailEvent {
    schema: &'static str,
    test: &'static str,
    metric: &'static str,
    actual: f64,
    threshold: f64,
    unit: &'static str,
    status: &'static str,
    providers: usize,
    aliases: usize,
    timestamp: String,
    details: serde_json::Value,
}

#[derive(Debug, Clone)]
struct Thresholds {
    startup_p95_ms: f64,
    lookup_p99_us: f64,
    footprint_mb: f64,
    rss_delta_mb: f64,
}

#[derive(Debug, Clone)]
struct LookupDataset {
    ids: Vec<String>,
    provider_count: usize,
    alias_count: usize,
}

type MaterializedProviderRow = (String, Vec<String>, Vec<String>, Option<String>);

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn output_dir() -> PathBuf {
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
    let out = target_dir.join("perf");
    std::fs::create_dir_all(&out).expect("create provider registry guardrail output dir");
    out
}

fn artifact_jsonl() -> PathBuf {
    output_dir().join("provider_registry_guardrails.jsonl")
}

fn artifact_latest() -> PathBuf {
    output_dir().join("provider_registry_guardrails.latest.json")
}

fn append_jsonl(path: &Path, line: &str) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .expect("create provider registry guardrail artifact dir");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open provider registry guardrail artifact");
    let _ = writeln!(file, "{line}");
}

fn parse_budget(raw: Option<&str>, default: f64) -> f64 {
    raw.and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(default)
}

fn read_thresholds() -> Thresholds {
    let startup_default = if cfg!(debug_assertions) {
        STARTUP_P95_MS_DEFAULT * 20.0
    } else {
        STARTUP_P95_MS_DEFAULT
    };
    let lookup_default = if cfg!(debug_assertions) {
        LOOKUP_P99_US_DEFAULT * 15.0
    } else {
        LOOKUP_P99_US_DEFAULT
    };

    Thresholds {
        startup_p95_ms: parse_budget(
            std::env::var(STARTUP_P95_MS_ENV).ok().as_deref(),
            startup_default,
        ),
        lookup_p99_us: parse_budget(
            std::env::var(LOOKUP_P99_US_ENV).ok().as_deref(),
            lookup_default,
        ),
        footprint_mb: parse_budget(
            std::env::var(FOOTPRINT_MB_ENV).ok().as_deref(),
            FOOTPRINT_MB_DEFAULT,
        ),
        rss_delta_mb: parse_budget(
            std::env::var(RSS_DELTA_MB_ENV).ok().as_deref(),
            RSS_DELTA_MB_DEFAULT,
        ),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * ((sorted.len() - 1) as f64)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn collect_lookup_dataset() -> LookupDataset {
    let mut ids = Vec::new();
    let mut alias_count = 0usize;

    for meta in PROVIDER_METADATA {
        ids.push(meta.canonical_id.to_string());
        ids.push(meta.canonical_id.to_uppercase());
        for alias in meta.aliases {
            ids.push((*alias).to_string());
            ids.push(alias.to_uppercase());
            alias_count += 1;
        }
    }
    ids.push("nonexistent-provider".to_string());
    ids.push("NONEXISTENT-PROVIDER".to_string());

    LookupDataset {
        ids,
        provider_count: PROVIDER_METADATA.len(),
        alias_count,
    }
}

fn benchmark_startup_index_build(dataset: &LookupDataset) -> (f64, f64, Vec<f64>) {
    let warmup = 25usize;
    let runs = 200usize;

    for _ in 0..warmup {
        let mut index: HashMap<String, &'static str> = HashMap::new();
        for meta in PROVIDER_METADATA {
            index.insert(meta.canonical_id.to_string(), meta.canonical_id);
            for alias in meta.aliases {
                index.insert(alias.to_string(), meta.canonical_id);
            }
        }
        let _ = index.get("openai");
    }

    let mut samples_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let mut index: HashMap<String, &'static str> = HashMap::new();
        for meta in PROVIDER_METADATA {
            index.insert(meta.canonical_id.to_string(), meta.canonical_id);
            for alias in meta.aliases {
                index.insert(alias.to_string(), meta.canonical_id);
            }
        }
        for id in &dataset.ids {
            let _ = index.get(id.as_str());
        }
        samples_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = percentile(&samples_ms, 95.0);
    let p99 = percentile(&samples_ms, 99.0);
    (p95, p99, samples_ms)
}

fn benchmark_lookup_latency(dataset: &LookupDataset) -> (f64, f64, Vec<f64>) {
    let warmup_rounds = 25usize;
    let rounds = 250usize;

    for _ in 0..warmup_rounds {
        for id in &dataset.ids {
            let _ = provider_metadata(id);
            let _ = canonical_provider_id(id);
            let _ = provider_auth_env_keys(id);
            let _ = provider_routing_defaults(id);
        }
    }

    let mut samples_us = Vec::with_capacity(rounds * dataset.ids.len());
    for _ in 0..rounds {
        for id in &dataset.ids {
            let start = Instant::now();
            let _ = provider_metadata(id);
            let _ = canonical_provider_id(id);
            let _ = provider_auth_env_keys(id);
            let _ = provider_routing_defaults(id);
            samples_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
    }

    samples_us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = percentile(&samples_us, 95.0);
    let p99 = percentile(&samples_us, 99.0);
    (p95, p99, samples_us)
}

fn provider_footprint_bytes() -> Vec<(&'static str, usize)> {
    let mut rows = Vec::with_capacity(PROVIDER_METADATA.len());
    for meta in PROVIDER_METADATA {
        let mut bytes = meta.canonical_id.len();
        bytes += meta.aliases.iter().map(|alias| alias.len()).sum::<usize>();
        bytes += meta
            .auth_env_keys
            .iter()
            .map(|key| key.len())
            .sum::<usize>();
        if let Some(defaults) = meta.routing_defaults {
            bytes += defaults.api.len();
            bytes += defaults.base_url.len();
        }
        rows.push((meta.canonical_id, bytes));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    rows
}

fn estimate_registry_footprint_bytes() -> usize {
    let static_table = std::mem::size_of_val(PROVIDER_METADATA);
    let string_bytes: usize = provider_footprint_bytes().iter().map(|(_, b)| *b).sum();
    static_table + string_bytes
}

fn process_rss_mb() -> f64 {
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new().expect("initialize sysinfo");
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let rss_bytes = system.process(pid).map_or(0, sysinfo::Process::memory);
    rss_bytes as f64 / (1024.0 * 1024.0)
}

fn emit_guardrail_event(event: &GuardrailEvent) {
    let json = serde_json::to_string(event).expect("serialize guardrail event");
    append_jsonl(&artifact_jsonl(), &json);
    let pretty = serde_json::to_string_pretty(event).expect("serialize pretty guardrail event");
    std::fs::write(artifact_latest(), pretty).expect("write latest guardrail event");
}

fn status(actual: f64, threshold: f64) -> &'static str {
    if actual <= threshold { "PASS" } else { "FAIL" }
}

#[test]
fn provider_registry_startup_and_lookup_guardrails() {
    let _guard = GUARDRAIL_LOCK.lock().expect("provider guardrail lock");
    let harness = TestHarness::new("provider_registry_startup_and_lookup_guardrails");
    let thresholds = read_thresholds();
    let dataset = collect_lookup_dataset();

    let (startup_p95_ms, startup_p99_ms, startup_samples_ms) =
        benchmark_startup_index_build(&dataset);
    let (lookup_p95_us, lookup_p99_us, lookup_samples_us) = benchmark_lookup_latency(&dataset);

    harness.log().info(
        "measure",
        "measured provider registry startup+lookup latency",
    );

    let startup_event = GuardrailEvent {
        schema: "pi.provider_registry.guardrail.v1",
        test: "provider_registry_startup_and_lookup_guardrails",
        metric: "startup_index_build_p95_ms",
        actual: startup_p95_ms,
        threshold: thresholds.startup_p95_ms,
        unit: "ms",
        status: status(startup_p95_ms, thresholds.startup_p95_ms),
        providers: dataset.provider_count,
        aliases: dataset.alias_count,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        details: json!({
            "startup_index_build_p99_ms": startup_p99_ms,
            "lookup_p95_us": lookup_p95_us,
            "lookup_p99_us": lookup_p99_us,
            "lookup_threshold_us": thresholds.lookup_p99_us,
            "samples": {
                "startup_count": startup_samples_ms.len(),
                "lookup_count": lookup_samples_us.len(),
            },
        }),
    };
    emit_guardrail_event(&startup_event);

    let lookup_event = GuardrailEvent {
        schema: "pi.provider_registry.guardrail.v1",
        test: "provider_registry_startup_and_lookup_guardrails",
        metric: "provider_lookup_p99_us",
        actual: lookup_p99_us,
        threshold: thresholds.lookup_p99_us,
        unit: "us",
        status: status(lookup_p99_us, thresholds.lookup_p99_us),
        providers: dataset.provider_count,
        aliases: dataset.alias_count,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        details: json!({
            "lookup_p95_us": lookup_p95_us,
            "startup_index_build_p95_ms": startup_p95_ms,
            "startup_threshold_ms": thresholds.startup_p95_ms,
            "samples": {
                "startup_count": startup_samples_ms.len(),
                "lookup_count": lookup_samples_us.len(),
            },
        }),
    };
    emit_guardrail_event(&lookup_event);

    eprintln!("\n=== Provider Registry Startup + Lookup Guardrails ===");
    eprintln!(
        "  Providers: {} (aliases: {})",
        dataset.provider_count, dataset.alias_count
    );
    eprintln!(
        "  Startup index p95: {:.3}ms (budget: {:.3}ms)",
        startup_p95_ms, thresholds.startup_p95_ms
    );
    eprintln!(
        "  Lookup p99:        {:.3}us (budget: {:.3}us)",
        lookup_p99_us, thresholds.lookup_p99_us
    );

    assert!(
        startup_p95_ms <= thresholds.startup_p95_ms,
        "startup index build p95 {:.3}ms exceeds budget {:.3}ms",
        startup_p95_ms,
        thresholds.startup_p95_ms
    );
    assert!(
        lookup_p99_us <= thresholds.lookup_p99_us,
        "provider lookup p99 {:.3}us exceeds budget {:.3}us",
        lookup_p99_us,
        thresholds.lookup_p99_us
    );
}

#[test]
fn provider_registry_memory_guardrails() {
    let _guard = GUARDRAIL_LOCK.lock().expect("provider guardrail lock");
    let harness = TestHarness::new("provider_registry_memory_guardrails");
    let thresholds = read_thresholds();
    let dataset = collect_lookup_dataset();

    let before_rss_mb = process_rss_mb();
    let materialized: Vec<MaterializedProviderRow> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            let aliases = meta
                .aliases
                .iter()
                .map(|v| (*v).to_string())
                .collect::<Vec<_>>();
            let keys = meta
                .auth_env_keys
                .iter()
                .map(|v| (*v).to_string())
                .collect::<Vec<_>>();
            let base = meta.routing_defaults.map(|d| d.base_url.to_string());
            (meta.canonical_id.to_string(), aliases, keys, base)
        })
        .collect();
    let after_rss_mb = process_rss_mb();

    std::hint::black_box(&materialized);

    let estimated_mb = estimate_registry_footprint_bytes() as f64 / (1024.0 * 1024.0);
    let rss_delta_mb = (after_rss_mb - before_rss_mb).max(0.0);

    let top = provider_footprint_bytes();
    let top_entries = top
        .iter()
        .take(5)
        .map(|(id, bytes)| json!({"provider": id, "bytes": bytes}))
        .collect::<Vec<_>>();

    harness
        .log()
        .info("measure", "measured provider registry memory footprint");

    let event = GuardrailEvent {
        schema: "pi.provider_registry.guardrail.v1",
        test: "provider_registry_memory_guardrails",
        metric: "provider_registry_footprint_mb",
        actual: estimated_mb,
        threshold: thresholds.footprint_mb,
        unit: "MB",
        status: status(estimated_mb, thresholds.footprint_mb),
        providers: dataset.provider_count,
        aliases: dataset.alias_count,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        details: json!({
            "rss_before_mb": before_rss_mb,
            "rss_after_mb": after_rss_mb,
            "rss_delta_mb": rss_delta_mb,
            "rss_delta_threshold_mb": thresholds.rss_delta_mb,
            "materialized_entries": materialized.len(),
            "top_provider_footprints": top_entries,
        }),
    };
    emit_guardrail_event(&event);

    let mut diag = String::new();
    for (id, bytes) in top.iter().take(5) {
        let _ = write!(&mut diag, "{id}:{bytes}B ");
    }

    eprintln!("\n=== Provider Registry Memory Guardrails ===");
    eprintln!(
        "  Estimated footprint: {:.3}MB (budget: {:.3}MB)",
        estimated_mb, thresholds.footprint_mb
    );
    eprintln!(
        "  RSS delta:           {:.3}MB (budget: {:.3}MB)",
        rss_delta_mb, thresholds.rss_delta_mb
    );
    eprintln!("  Top contributors:    {}", diag.trim());

    assert!(
        estimated_mb <= thresholds.footprint_mb,
        "provider registry estimated footprint {:.3}MB exceeds {:.3}MB; top contributors: {}",
        estimated_mb,
        thresholds.footprint_mb,
        diag.trim()
    );
    assert!(
        rss_delta_mb <= thresholds.rss_delta_mb,
        "provider registry RSS delta {:.3}MB exceeds {:.3}MB; top contributors: {}",
        rss_delta_mb,
        thresholds.rss_delta_mb,
        diag.trim()
    );
}

#[test]
fn threshold_parser_accepts_valid_positive_numbers() {
    assert_f64_eq(parse_budget(Some("1"), 9.0), 1.0);
    assert_f64_eq(parse_budget(Some("3.25"), 9.0), 3.25);
    assert_f64_eq(parse_budget(Some(" 42 "), 9.0), 42.0);
}

#[test]
fn threshold_parser_rejects_invalid_or_non_positive_inputs() {
    assert_f64_eq(parse_budget(Some(""), 7.0), 7.0);
    assert_f64_eq(parse_budget(Some("abc"), 7.0), 7.0);
    assert_f64_eq(parse_budget(Some("-1.5"), 7.0), 7.0);
    assert_f64_eq(parse_budget(Some("0"), 7.0), 7.0);
    assert_f64_eq(parse_budget(None, 7.0), 7.0);
}

fn assert_f64_eq(actual: f64, expected: f64) {
    let delta = (actual - expected).abs();
    assert!(
        delta <= 1e-12,
        "expected {expected}, got {actual}, delta={delta}"
    );
}
