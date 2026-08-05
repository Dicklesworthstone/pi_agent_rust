//! Extension workload harness for deterministic legacy-vs-rust perf baselines.
//!
//! Workloads run through the production extension manager and runtime handle so
//! they measure the same owner-isolated routing boundary used by the CLI.
#![forbid(unsafe_code)]
// The driver uses `block_on`; the runtime handle owns its isolated QuickJS
// worker, so driver futures do not need to be `Send`.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::future_not_send,
    clippy::needless_raw_string_hashes,
    clippy::redundant_closure_for_method_calls,
    clippy::too_many_lines,
    clippy::suboptimal_flops
)]

use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use futures::executor::block_on;
use pi::error::{Error, Result};
use pi::extension_scoring::{
    InterferenceMatrixCompletenessReport, evaluate_interference_matrix_completeness,
    format_interference_pair_key, parse_interference_pair_key,
};
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::perf_build;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BENCH_SCHEMA: &str = "pi.ext.rust_bench.v1";
const EVIDENCE_CLASS_MEASURED: &str = "measured";
const CONFIDENCE_HIGH: &str = "high";
const CONFIDENCE_MEDIUM: &str = "medium";
const MEASUREMENT_BOUNDARY: &str = "production_extension_manager";
const MEASUREMENT_CONTRACT_VERSION: &str = "production_extension_manager.v1";
const HOTSPOT_MATRIX_SCHEMA: &str = "pi.ext.hostcall_hotspot_matrix.v1";
const BUILD_PROVENANCE_FIELDS: &[&str] = &[
    "source_commit",
    "source_dirty",
    "build_profile",
    "executable_build_profile",
    "executable_profile_verified",
    "build_fingerprint_verified",
    "build_profile_verified",
    "build_fingerprint_contract",
    "compiled_profile_family",
    "compiled_opt_level",
    "compiled_debug",
    "compiled_features",
    "binary_path",
    "binary_sha256",
    "debug_assertions",
    "config_hash",
];
const VOI_SCHEDULER_SCHEMA: &str = "pi.ext.voi_scheduler.v1";
const TRACE_EVENT_SCHEMA: &str = "pi.ext.hostcall_trace.v1";
const DEFAULT_MATRIX_FILENAME: &str = "ext_hostcall_hotspot_matrix.json";
const DEFAULT_TRACE_FILENAME: &str = "ext_hostcall_bridge_trace.jsonl";

#[derive(Clone, Copy)]
enum PerfArtifact {
    HotspotMatrix,
    Trace,
}

impl PerfArtifact {
    const fn name(self) -> &'static str {
        match self {
            Self::HotspotMatrix => DEFAULT_MATRIX_FILENAME,
            Self::Trace => DEFAULT_TRACE_FILENAME,
        }
    }
}

const DEFAULT_DOWNSTREAM_BEADS: &[&str] = &[
    "bd-3ar8v.4.12",
    "bd-3ar8v.4.20",
    "bd-3ar8v.4.21",
    "bd-3ar8v.4.23",
    "bd-3ar8v.4.29",
];
const STAGE_DECOMPOSITION: [&str; 6] = ["marshal", "queue", "schedule", "policy", "execute", "io"];
const DEFAULT_PMU_LLC_MISS_BUDGET_PCT: f64 = 18.0;
const DEFAULT_PMU_BRANCH_MISS_BUDGET_PCT: f64 = 6.0;
const DEFAULT_PMU_STALL_TOTAL_BUDGET_PCT: f64 = 65.0;
const DEFAULT_PMU_REGRESSION_LLC_DELTA_PCT: f64 = 1.0;
const DEFAULT_PMU_REGRESSION_BRANCH_DELTA_PCT: f64 = 0.5;
const DEFAULT_PMU_REGRESSION_STALL_DELTA_PCT: f64 = 2.0;
const DEFAULT_VOI_BUDGET_MS: f64 = 120.0;
const DEFAULT_VOI_MAX_EXPERIMENTS: usize = 3;
const DEFAULT_VOI_STALE_AFTER_HOURS: f64 = 24.0;

#[derive(Parser, Debug)]
#[command(name = "ext_workloads")]
#[command(about = "Deterministic extension workload runner for perf baselines")]
struct Args {
    /// Number of cold load+init runs per extension (fresh runtime each run).
    #[arg(
        long,
        default_value_t = NonZeroUsize::new(3).expect("default load-run count is nonzero")
    )]
    load_runs: NonZeroUsize,

    /// Iterations for tool-call + event-hook scenarios.
    #[arg(
        long,
        default_value_t = NonZeroU32::new(2_000).expect("default iteration count is nonzero")
    )]
    iterations: NonZeroU32,

    /// Iterations for realistic long-session hostcall profiling.
    #[arg(
        long,
        default_value_t = NonZeroU32::new(8_000)
            .expect("default long-session iteration count is nonzero")
    )]
    long_session_iterations: NonZeroU32,

    /// Number of real-corpus extensions to load for long-session profiling.
    #[arg(long, default_value_t = 8)]
    real_corpus_extensions: usize,

    /// Optional JSONL output path (stdout if omitted).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Optional path for hotspot matrix artifact.
    #[arg(long)]
    matrix_out: Option<PathBuf>,

    /// Optional path for trace JSONL artifact.
    #[arg(long)]
    trace_out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct RegressionEvidence {
    confidence: &'static str,
    eligible: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MeasurementRegion {
    Controlled,
    Load,
}

#[derive(Debug, Clone, Copy)]
struct RegressionEligibility<'a> {
    build_profile: &'a str,
    build_profile_verified: bool,
    provenance_identity_verified: bool,
    optimized_binary: bool,
    measurement_region: MeasurementRegion,
}

fn classify_regression_evidence(eligibility: RegressionEligibility<'_>) -> RegressionEvidence {
    let eligible = eligibility.build_profile == "perf"
        && eligibility.build_profile_verified
        && eligibility.provenance_identity_verified
        && eligibility.optimized_binary
        && eligibility.measurement_region == MeasurementRegion::Controlled;
    RegressionEvidence {
        confidence: if eligible {
            CONFIDENCE_HIGH
        } else {
            CONFIDENCE_MEDIUM
        },
        eligible,
    }
}

#[derive(Debug, Clone, Serialize)]
struct SummaryMs {
    count: usize,
    min_ms: Option<f64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct StageWeights {
    marshal: f64,
    queue: f64,
    schedule: f64,
    policy: f64,
    execute: f64,
    io: f64,
}

impl StageWeights {
    fn sum(self) -> f64 {
        self.marshal + self.queue + self.schedule + self.policy + self.execute + self.io
    }

    fn scaled(self, total_us: f64) -> StageTotals {
        StageTotals {
            marshal: self.marshal * total_us,
            queue: self.queue * total_us,
            schedule: self.schedule * total_us,
            policy: self.policy * total_us,
            execute: self.execute * total_us,
            io: self.io * total_us,
        }
    }
}

fn stage_weight_component(weights: StageWeights, stage: &str) -> f64 {
    match stage {
        "marshal" => weights.marshal,
        "queue" => weights.queue,
        "schedule" => weights.schedule,
        "policy" => weights.policy,
        "execute" => weights.execute,
        "io" => weights.io,
        _ => 0.0,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StageTotals {
    marshal: f64,
    queue: f64,
    schedule: f64,
    policy: f64,
    execute: f64,
    io: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
struct PmuBudgetSpec {
    llc_miss_budget_pct: f64,
    branch_miss_budget_pct: f64,
    stall_total_budget_pct: f64,
}

impl Default for PmuBudgetSpec {
    fn default() -> Self {
        Self {
            llc_miss_budget_pct: DEFAULT_PMU_LLC_MISS_BUDGET_PCT,
            branch_miss_budget_pct: DEFAULT_PMU_BRANCH_MISS_BUDGET_PCT,
            stall_total_budget_pct: DEFAULT_PMU_STALL_TOTAL_BUDGET_PCT,
        }
    }
}

impl PmuBudgetSpec {
    fn from_env() -> Self {
        fn parse_env_f64(key: &str) -> Option<f64> {
            std::env::var(key)
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite())
        }

        let mut budget = Self::default();
        if let Some(value) = parse_env_f64("PI_EXT_PMU_LLC_MISS_BUDGET_PCT") {
            budget.llc_miss_budget_pct = value.clamp(0.1, 100.0);
        }
        if let Some(value) = parse_env_f64("PI_EXT_PMU_BRANCH_MISS_BUDGET_PCT") {
            budget.branch_miss_budget_pct = value.clamp(0.1, 100.0);
        }
        if let Some(value) = parse_env_f64("PI_EXT_PMU_STALL_TOTAL_BUDGET_PCT") {
            budget.stall_total_budget_pct = value.clamp(0.1, 200.0);
        }
        budget
    }

    fn as_json(self) -> Value {
        json!({
            "llc_miss_budget_pct": self.llc_miss_budget_pct,
            "branch_miss_budget_pct": self.branch_miss_budget_pct,
            "stall_total_budget_pct": self.stall_total_budget_pct,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PmuCountersNormalized {
    frontend_stall_pct: Option<f64>,
    backend_stall_pct: Option<f64>,
    llc_miss_pct: Option<f64>,
    branch_miss_pct: Option<f64>,
    cycles_per_call: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct PerfOutcomeSnapshot {
    p50_us: Option<f64>,
    p95_us: Option<f64>,
    p99_us: Option<f64>,
    throughput_eps: Option<f64>,
    rss_mb: Option<f64>,
    cpu_pct: Option<f64>,
    io_wait_pct: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct VoiBudgetConfig {
    max_overhead_ms: f64,
    max_experiments: usize,
    stale_after_hours: f64,
}

impl Default for VoiBudgetConfig {
    fn default() -> Self {
        Self {
            max_overhead_ms: DEFAULT_VOI_BUDGET_MS,
            max_experiments: DEFAULT_VOI_MAX_EXPERIMENTS,
            stale_after_hours: DEFAULT_VOI_STALE_AFTER_HOURS,
        }
    }
}

impl VoiBudgetConfig {
    fn from_env() -> Self {
        fn parse_env_f64(key: &str) -> Option<f64> {
            std::env::var(key)
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite())
        }

        fn parse_env_usize(key: &str) -> Option<usize> {
            std::env::var(key)
                .ok()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
        }

        let mut config = Self::default();
        if let Some(value) = parse_env_f64("PI_EXT_VOI_BUDGET_MS") {
            config.max_overhead_ms = value.clamp(5.0, 2_000.0);
        }
        if let Some(value) = parse_env_usize("PI_EXT_VOI_MAX_EXPERIMENTS") {
            config.max_experiments = value.clamp(1, 12);
        }
        if let Some(value) = parse_env_f64("PI_EXT_VOI_STALE_AFTER_HOURS") {
            config.stale_after_hours = value.clamp(1.0, 168.0);
        }
        config
    }
}

#[derive(Debug, Clone)]
struct VoiCandidate {
    stage: String,
    utility_total: f64,
    uncertainty_reduction: f64,
    user_impact_score: f64,
    pressure_bonus: f64,
    expected_information_gain: f64,
    cost_ms: f64,
    voi_score: f64,
    recommended_probe: &'static str,
}

#[derive(Debug, Clone)]
struct ScoredVoiCandidate {
    candidate: VoiCandidate,
    rank: usize,
    selected: bool,
    skip_reason: Option<String>,
}

#[derive(Clone, Copy)]
struct VoiPlannerInputs<'a> {
    hotspot_entries: &'a [Value],
    run_metadata: &'a Value,
    pmu_meta: &'a Value,
    pmu_comparison: &'a Value,
    pmu_outcome_correlation: &'a Value,
    outcome_comparison: &'a Value,
}

impl StageTotals {
    fn add(self, rhs: Self) -> Self {
        Self {
            marshal: self.marshal + rhs.marshal,
            queue: self.queue + rhs.queue,
            schedule: self.schedule + rhs.schedule,
            policy: self.policy + rhs.policy,
            execute: self.execute + rhs.execute,
            io: self.io + rhs.io,
        }
    }

    fn total_us(self) -> f64 {
        self.marshal + self.queue + self.schedule + self.policy + self.execute + self.io
    }
}

#[derive(Debug, Clone)]
// These booleans mirror independent evidence fields in the parsed JSON protocol.
#[allow(clippy::struct_excessive_bools)]
struct ParsedProfileRecord {
    scenario: String,
    extension: String,
    samples: u64,
    total_us: f64,
    per_call_us: f64,
    weights: StageWeights,
    evidence_class: String,
    source_confidence: String,
    declared_gate_eligible: bool,
    effective_gate_eligible: bool,
    measurement_method: String,
    measurement_boundary: String,
    measurement_contract_version: String,
    build_profile: String,
    debug_assertions: bool,
    executable_build_profile: String,
    executable_profile_verified: bool,
    build_fingerprint_verified: bool,
    build_profile_verified: bool,
    build_fingerprint_contract: String,
    compiled_profile_family: String,
    compiled_opt_level: String,
    compiled_debug: String,
    compiled_features: Vec<String>,
    binary_path: String,
    binary_sha256: String,
    source_commit: String,
    source_dirty: bool,
    config_hash: String,
    disk_cache_policy: String,
    host_page_cache_policy: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let started_at = Utc::now();
    let wall_start = Instant::now();
    let build_profile = perf_build::detect_build_profile();
    let current_exe = std::env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    let executable_build_profile = current_exe
        .as_deref()
        .and_then(perf_build::profile_from_target_path);
    let executable_profile_verified = executable_build_profile.as_deref() == Some("perf");
    let build_fingerprint_verified = perf_build::has_canonical_perf_build_fingerprint();
    let build_profile_verified = executable_profile_verified && build_fingerprint_verified;
    let binary_path = current_exe
        .as_deref()
        .and_then(Path::to_str)
        .unwrap_or("unknown")
        .to_string();
    let binary_sha256 = current_exe
        .as_deref()
        .and_then(|path| perf_build::sha256_file(path).ok())
        .unwrap_or_else(|| "unknown".to_string());
    let source_commit = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let source_dirty = option_env!("VERGEN_GIT_DIRTY") != Some("false");
    let compiled_features = perf_build::compiled_feature_set();
    let provenance_identity_verified = is_full_git_sha(source_commit)
        && !source_dirty
        && binary_path != "unknown"
        && is_lower_hex_sha256(&binary_sha256)
        && !compiled_features.is_empty()
        && compiled_features.windows(2).all(|pair| pair[0] < pair[1]);
    let config_hash =
        perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
            source_commit,
            source_dirty,
            build_profile: &build_profile,
            executable_build_profile: executable_build_profile.as_deref().unwrap_or("unknown"),
            verification: perf_build::BenchmarkBuildVerification {
                executable_profile: executable_profile_verified,
                build_fingerprint: build_fingerprint_verified,
                build_profile: build_profile_verified,
            },
            build_fingerprint_contract: perf_build::BUILD_FINGERPRINT_CONTRACT,
            compiled_profile_family: perf_build::COMPILED_PROFILE_FAMILY,
            compiled_opt_level: perf_build::COMPILED_OPT_LEVEL,
            compiled_debug: perf_build::COMPILED_DEBUG,
            compiled_features: &compiled_features,
            binary_path: &binary_path,
            binary_sha256: &binary_sha256,
            debug_assertions: cfg!(debug_assertions),
        });
    let measured_evidence = classify_regression_evidence(RegressionEligibility {
        build_profile: &build_profile,
        build_profile_verified,
        provenance_identity_verified,
        optimized_binary: !cfg!(debug_assertions),
        measurement_region: MeasurementRegion::Controlled,
    });
    let load_evidence = classify_regression_evidence(RegressionEligibility {
        build_profile: &build_profile,
        build_profile_verified,
        provenance_identity_verified,
        optimized_binary: !cfg!(debug_assertions),
        measurement_region: MeasurementRegion::Load,
    });

    let mut out: Box<dyn Write> = match args.out.as_ref() {
        Some(path) => Box::new(fs::File::create(path)?),
        None => Box::new(io::stdout()),
    };

    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_cwd = cwd.display().to_string();

    let hello_entry = artifact_single_file_entry("hello");
    let pirate_entry = artifact_single_file_entry("pirate");

    let hello_spec = JsExtensionLoadSpec::from_entry_path(&hello_entry)?;
    let pirate_spec = JsExtensionLoadSpec::from_entry_path(&pirate_entry)?;

    let env = json!({
        "pkg": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        "build_ts": option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "build_profile": build_profile,
        "executable_build_profile": executable_build_profile,
        "executable_profile_verified": executable_profile_verified,
        "build_fingerprint_verified": build_fingerprint_verified,
        "build_profile_verified": build_profile_verified,
        "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
        "compiled_profile_family": perf_build::COMPILED_PROFILE_FAMILY,
        "compiled_opt_level": perf_build::COMPILED_OPT_LEVEL,
        "compiled_debug": perf_build::COMPILED_DEBUG,
        "compiled_features": compiled_features,
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "debug_assertions": cfg!(debug_assertions),
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "config_hash": config_hash,
    });

    let mut records = Vec::new();
    let mut trace_events = Vec::new();
    trace_events.push(trace_event(
        "run.start",
        "ext_workloads",
        &json!({
            "load_runs": args.load_runs.get(),
            "iterations": args.iterations.get(),
            "long_session_iterations": args.long_session_iterations.get(),
            "real_corpus_extensions": args.real_corpus_extensions,
            "build_profile": build_profile,
            "executable_build_profile": executable_build_profile,
            "executable_profile_verified": executable_profile_verified,
            "build_fingerprint_verified": build_fingerprint_verified,
            "build_profile_verified": build_profile_verified,
            "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
            "compiled_profile_family": perf_build::COMPILED_PROFILE_FAMILY,
            "compiled_opt_level": perf_build::COMPILED_OPT_LEVEL,
            "compiled_debug": perf_build::COMPILED_DEBUG,
            "compiled_features": compiled_features,
            "binary_path": binary_path,
            "binary_sha256": binary_sha256,
            "debug_assertions": cfg!(debug_assertions),
            "source_commit": source_commit,
            "source_dirty": source_dirty,
            "config_hash": config_hash,
        }),
    ));

    // ext_load_init/load_init_cold (hello + pirate)
    let hello_load = block_on(scenario_load_init_cold(
        &hello_spec,
        &js_cwd,
        args.load_runs.get(),
        load_evidence,
    ))?;
    let hello_load = attach_env(hello_load, &env);
    trace_events.push(trace_event_for_record("scenario.complete", &hello_load));
    records.push(hello_load);

    let pirate_load = block_on(scenario_load_init_cold(
        &pirate_spec,
        &js_cwd,
        args.load_runs.get(),
        load_evidence,
    ))?;
    let pirate_load = attach_env(pirate_load, &env);
    trace_events.push(trace_event_for_record("scenario.complete", &pirate_load));
    records.push(pirate_load);

    // ext_tool_call/hello
    let tool_call = block_on(scenario_tool_call(
        &hello_spec,
        &js_cwd,
        args.iterations.get(),
        measured_evidence,
    ))?;
    let tool_call = attach_env(tool_call, &env);
    trace_events.push(trace_event_for_record("scenario.complete", &tool_call));
    records.push(tool_call);

    // ext_event_hook/before_agent_start
    let event_hook = block_on(scenario_event_hook(
        &pirate_spec,
        &js_cwd,
        args.iterations.get(),
        measured_evidence,
    ))?;
    let event_hook = attach_env(event_hook, &env);
    trace_events.push(trace_event_for_record("scenario.complete", &event_hook));
    records.push(event_hook);

    let real_specs = discover_real_corpus_specs(args.real_corpus_extensions)?;
    if real_specs.is_empty() {
        trace_events.push(trace_event(
            "scenario.skip",
            "ext_hostcall_bridge/long_session_real_corpus",
            &json!({
                "reason": "no_safe_official_single_file_extensions_found",
                "requested_extensions": args.real_corpus_extensions,
            }),
        ));
    } else {
        let long_session = block_on(scenario_long_session_real_corpus(
            &real_specs,
            &js_cwd,
            args.long_session_iterations.get(),
            measured_evidence,
        ))?;
        let long_session = attach_env(long_session, &env);
        trace_events.push(trace_event_for_record("scenario.complete", &long_session));
        records.push(long_session);
    }

    for record in &records {
        writeln!(out, "{}", to_json_line(record)?)?;
    }

    let trace_path = args
        .trace_out
        .clone()
        .unwrap_or_else(|| default_perf_artifact_path(PerfArtifact::Trace));
    write_jsonl(&trace_path, &trace_events)?;

    let finished_at = Utc::now();
    let run_metadata = json!({
        "schema": "pi.ext.run_metadata.v1",
        "run_id": format!("ext-hostcall-{}", started_at.timestamp_millis()),
        "started_at": started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "finished_at": finished_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "elapsed_ms": wall_start.elapsed().as_secs_f64() * 1000.0,
        "cwd": cwd.display().to_string(),
        "commandline": std::env::args().collect::<Vec<_>>(),
        "git_sha": option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        "build_ts": option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "build_profile": build_profile,
        "executable_build_profile": executable_build_profile,
        "executable_profile_verified": executable_profile_verified,
        "build_fingerprint_verified": build_fingerprint_verified,
        "build_profile_verified": build_profile_verified,
        "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
        "compiled_profile_family": perf_build::COMPILED_PROFILE_FAMILY,
        "compiled_opt_level": perf_build::COMPILED_OPT_LEVEL,
        "compiled_debug": perf_build::COMPILED_DEBUG,
        "compiled_features": compiled_features,
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "debug_assertions": cfg!(debug_assertions),
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "config_hash": config_hash,
    });
    let trace_meta = json!({
        "schema": TRACE_EVENT_SCHEMA,
        "path": trace_path.display().to_string(),
        "event_count": trace_events.len(),
    });
    let pmu_meta = collect_pmu_metadata();
    let pmu_budget = pmu_budget_from_meta(&pmu_meta);
    let pmu_baseline_meta = collect_pmu_baseline_metadata(pmu_budget);
    let flame_meta = collect_flame_metadata();

    let hotspot_matrix = build_hotspot_matrix(
        &records,
        &run_metadata,
        &trace_meta,
        &pmu_meta,
        &pmu_baseline_meta,
        &flame_meta,
    );
    validate_hotspot_matrix_schema(&hotspot_matrix)?;

    let matrix_path = args
        .matrix_out
        .unwrap_or_else(|| default_perf_artifact_path(PerfArtifact::HotspotMatrix));
    fs::write(&matrix_path, to_json_pretty(&hotspot_matrix)?)?;
    eprintln!(
        "[ext_workloads] wrote hotspot matrix: {}",
        matrix_path.display()
    );
    eprintln!("[ext_workloads] wrote trace log: {}", trace_path.display());

    Ok(())
}

fn attach_env(mut record: Value, env: &Value) -> Value {
    if let Value::Object(ref mut map) = record {
        map.insert("env".to_string(), env.clone());
        for field in [
            "build_profile",
            "executable_build_profile",
            "executable_profile_verified",
            "build_fingerprint_verified",
            "build_profile_verified",
            "build_fingerprint_contract",
            "compiled_profile_family",
            "compiled_opt_level",
            "compiled_debug",
            "compiled_features",
            "binary_path",
            "binary_sha256",
            "debug_assertions",
            "source_commit",
            "source_dirty",
            "config_hash",
        ] {
            if let Some(value) = env.get(field) {
                map.insert(field.to_string(), value.clone());
            }
        }
    }
    record
}

fn artifact_single_file_entry(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ext_conformance/artifacts")
        .join(name)
        .join(format!("{name}.ts"))
}

fn default_perf_artifact_path(artifact: PerfArtifact) -> PathBuf {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    if let Some(output_root) = std::env::var_os("PERF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return if output_root.is_absolute() {
            output_root.join(artifact.name())
        } else {
            project_root.join(output_root).join(artifact.name())
        };
    }

    if let Some(target_root) = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        let target_dir = if target_root.is_absolute() {
            target_root
        } else {
            project_root.join(target_root)
        };
        return target_dir.join("perf").join(artifact.name());
    }

    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
        .join("pi_agent_rust")
        .join("ext_workloads")
        .join(artifact.name())
}

fn to_json_line(value: &Value) -> Result<String> {
    serde_json::to_string(value).map_err(|err| Error::Json(Box::new(err)))
}

fn to_json_pretty(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|err| Error::Json(Box::new(err)))
}

fn trace_event(event_type: &str, scenario: &str, details: &Value) -> Value {
    json!({
        "schema": TRACE_EVENT_SCHEMA,
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "event_type": event_type,
        "scenario": scenario,
        "details": details,
    })
}

fn trace_event_for_record(event_type: &str, record: &Value) -> Value {
    trace_event(
        event_type,
        record
            .get("scenario")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        &json!({
            "extension": record.get("extension").cloned().unwrap_or(Value::Null),
            "iterations": record.get("iterations").cloned().unwrap_or(Value::Null),
            "runs": record.get("runs").cloned().unwrap_or(Value::Null),
            "summary": record.get("summary").cloned().unwrap_or(Value::Null),
            "elapsed_ms": record.get("elapsed_ms").cloned().unwrap_or(Value::Null),
            "per_call_us": record.get("per_call_us").cloned().unwrap_or(Value::Null),
            "calls_per_sec": record.get("calls_per_sec").cloned().unwrap_or(Value::Null),
            "unexpected_hostcalls": record.get("unexpected_hostcalls").cloned().unwrap_or(Value::Null),
            "unexpected_hostcalls_observable": record.get("unexpected_hostcalls_observable").cloned().unwrap_or(Value::Null),
            "measurement_boundary": record.get("measurement_boundary").cloned().unwrap_or(Value::Null),
            "measurement_contract_version": record.get("measurement_contract_version").cloned().unwrap_or(Value::Null),
            "measurement_method": record.get("measurement_method").cloned().unwrap_or(Value::Null),
            "evidence_class": record.get("evidence_class").cloned().unwrap_or(Value::Null),
            "confidence": record.get("confidence").cloned().unwrap_or(Value::Null),
            "eligible_for_regression_gate": record.get("eligible_for_regression_gate").cloned().unwrap_or(Value::Null),
            "disk_cache_policy": record.get("disk_cache_policy").cloned().unwrap_or(Value::Null),
            "host_page_cache_policy": record.get("host_page_cache_policy").cloned().unwrap_or(Value::Null),
            "build_profile": record.get("build_profile").cloned().unwrap_or(Value::Null),
            "executable_build_profile": record.get("executable_build_profile").cloned().unwrap_or(Value::Null),
            "executable_profile_verified": record.get("executable_profile_verified").cloned().unwrap_or(Value::Null),
            "build_fingerprint_verified": record.get("build_fingerprint_verified").cloned().unwrap_or(Value::Null),
            "build_profile_verified": record.get("build_profile_verified").cloned().unwrap_or(Value::Null),
            "build_fingerprint_contract": record.get("build_fingerprint_contract").cloned().unwrap_or(Value::Null),
            "compiled_profile_family": record.get("compiled_profile_family").cloned().unwrap_or(Value::Null),
            "compiled_opt_level": record.get("compiled_opt_level").cloned().unwrap_or(Value::Null),
            "compiled_debug": record.get("compiled_debug").cloned().unwrap_or(Value::Null),
            "compiled_features": record.get("compiled_features").cloned().unwrap_or(Value::Null),
            "binary_path": record.get("binary_path").cloned().unwrap_or(Value::Null),
            "binary_sha256": record.get("binary_sha256").cloned().unwrap_or(Value::Null),
            "debug_assertions": record.get("debug_assertions").cloned().unwrap_or(Value::Null),
            "source_commit": record.get("source_commit").cloned().unwrap_or(Value::Null),
            "source_dirty": record.get("source_dirty").cloned().unwrap_or(Value::Null),
            "config_hash": record.get("config_hash").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn write_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = fs::File::create(path)?;
    for record in records {
        writeln!(out, "{}", to_json_line(record)?)?;
    }
    Ok(())
}

fn normalize_pct(value: Option<f64>) -> Option<f64> {
    let raw = value?;
    Some(if raw <= 1.0 {
        (raw * 100.0).clamp(0.0, 100.0)
    } else {
        raw.clamp(0.0, 100.0)
    })
}

fn value_as_f64(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_u64).map(|v| v as f64))
        .or_else(|| value.and_then(Value::as_i64).map(|v| v as f64))
}

fn object_counter_value(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| value_as_f64(object.get(*key)))
}

fn parse_pmu_counters(raw: &Value) -> Option<PmuCountersNormalized> {
    let root = raw.as_object()?;
    let source_object = root
        .get("counters")
        .and_then(Value::as_object)
        .unwrap_or(root);

    let counters = PmuCountersNormalized {
        frontend_stall_pct: normalize_pct(object_counter_value(
            source_object,
            &[
                "frontend_stall_pct",
                "frontend_stall_ratio",
                "frontend_bound_pct",
                "frontend_bound",
            ],
        )),
        backend_stall_pct: normalize_pct(object_counter_value(
            source_object,
            &[
                "backend_stall_pct",
                "backend_stall_ratio",
                "backend_bound_pct",
                "backend_bound",
            ],
        )),
        llc_miss_pct: normalize_pct(object_counter_value(
            source_object,
            &[
                "llc_miss_pct",
                "llc_miss_rate_pct",
                "llc_miss_ratio",
                "llc_miss_rate",
            ],
        )),
        branch_miss_pct: normalize_pct(object_counter_value(
            source_object,
            &[
                "branch_miss_pct",
                "branch_miss_rate_pct",
                "branch_miss_ratio",
                "branch_miss_rate",
            ],
        )),
        cycles_per_call: object_counter_value(
            source_object,
            &["cycles_per_call", "cycles_per_hostcall", "cycles_per_op"],
        )
        .map(|value| value.max(0.0)),
    };

    if counters.frontend_stall_pct.is_none()
        && counters.backend_stall_pct.is_none()
        && counters.llc_miss_pct.is_none()
        && counters.branch_miss_pct.is_none()
        && counters.cycles_per_call.is_none()
    {
        None
    } else {
        Some(counters)
    }
}

fn pmu_pressure_score(counters: &PmuCountersNormalized, budget: PmuBudgetSpec) -> f64 {
    let frontend = counters.frontend_stall_pct.unwrap_or(0.0);
    let backend = counters.backend_stall_pct.unwrap_or(0.0);
    let stall_total = frontend + backend;
    let llc = counters.llc_miss_pct.unwrap_or(0.0);
    let branch = counters.branch_miss_pct.unwrap_or(0.0);

    let stall_pressure = (stall_total / budget.stall_total_budget_pct.max(0.1)).clamp(0.0, 2.0);
    let llc_pressure = (llc / budget.llc_miss_budget_pct.max(0.1)).clamp(0.0, 2.0);
    let branch_pressure = (branch / budget.branch_miss_budget_pct.max(0.1)).clamp(0.0, 2.0);
    (stall_pressure * 0.5) + (llc_pressure * 0.35) + (branch_pressure * 0.15)
}

fn evaluate_pmu_budget(counters: &PmuCountersNormalized, budget: PmuBudgetSpec) -> Value {
    let frontend = counters.frontend_stall_pct.unwrap_or(0.0);
    let backend = counters.backend_stall_pct.unwrap_or(0.0);
    let stall_total = frontend + backend;
    let llc = counters.llc_miss_pct.unwrap_or(0.0);
    let branch = counters.branch_miss_pct.unwrap_or(0.0);

    let llc_ok = llc <= budget.llc_miss_budget_pct;
    let branch_ok = branch <= budget.branch_miss_budget_pct;
    let stall_ok = stall_total <= budget.stall_total_budget_pct;
    let status = if llc_ok && branch_ok && stall_ok {
        "pass"
    } else {
        "fail"
    };

    json!({
        "status": status,
        "checks": {
            "llc_miss_pct": { "value": llc, "budget": budget.llc_miss_budget_pct, "ok": llc_ok },
            "branch_miss_pct": { "value": branch, "budget": budget.branch_miss_budget_pct, "ok": branch_ok },
            "stall_total_pct": { "value": stall_total, "budget": budget.stall_total_budget_pct, "ok": stall_ok },
        },
        "pmu_pressure_score": pmu_pressure_score(counters, budget),
    })
}

fn annotate_pmu_payload(
    raw: &Value,
    source: &str,
    path: Option<&str>,
    budget: PmuBudgetSpec,
) -> Value {
    let normalized = parse_pmu_counters(raw);
    let budget_eval = normalized.as_ref().map_or_else(
        || {
            json!({
                "status": "not_evaluated",
                "reason": "recognized PMU counter fields were not found",
            })
        },
        |counters| evaluate_pmu_budget(counters, budget),
    );

    let mut base = json!({
        "status": "collected",
        "source": source,
        "counters": raw,
        "budget": budget.as_json(),
        "budget_evaluation": budget_eval,
    });
    if let Some(path) = path {
        base["path"] = Value::String(path.to_string());
    }
    if let Some(counters) = normalized {
        base["normalized_counters"] = serde_json::to_value(counters).unwrap_or(Value::Null);
    } else {
        base["normalized_counters"] = Value::Null;
    }
    base
}

fn collect_pmu_metadata() -> Value {
    let budget = PmuBudgetSpec::from_env();

    if let Ok(raw) = std::env::var("PI_EXT_PMU_COUNTERS_JSON") {
        return serde_json::from_str::<Value>(&raw).map_or_else(
            |_| {
                json!({
                    "status": "invalid",
                    "source": "env:PI_EXT_PMU_COUNTERS_JSON",
                    "reason": "failed_to_parse_json",
                    "budget": budget.as_json(),
                })
            },
            |parsed| annotate_pmu_payload(&parsed, "env:PI_EXT_PMU_COUNTERS_JSON", None, budget),
        );
    }

    if let Ok(path) = std::env::var("PI_EXT_PMU_COUNTERS_PATH") {
        return fs::read_to_string(&path).map_or_else(
            |_| {
                json!({
                    "status": "missing",
                    "source": "env:PI_EXT_PMU_COUNTERS_PATH",
                    "path": path,
                    "budget": budget.as_json(),
                })
            },
            |raw| {
                serde_json::from_str::<Value>(&raw).map_or_else(
                    |_| {
                        json!({
                            "status": "invalid",
                            "source": "env:PI_EXT_PMU_COUNTERS_PATH",
                            "path": path,
                            "reason": "failed_to_parse_json",
                            "budget": budget.as_json(),
                        })
                    },
                    |parsed| {
                        annotate_pmu_payload(
                            &parsed,
                            "env:PI_EXT_PMU_COUNTERS_PATH",
                            Some(&path),
                            budget,
                        )
                    },
                )
            },
        );
    }

    json!({
        "status": "not_collected",
        "reason": "set PI_EXT_PMU_COUNTERS_JSON or PI_EXT_PMU_COUNTERS_PATH to attach PMU counters",
        "budget": budget.as_json(),
    })
}

fn collect_pmu_baseline_metadata(budget: PmuBudgetSpec) -> Value {
    if let Ok(raw) = std::env::var("PI_EXT_PMU_BASELINE_COUNTERS_JSON") {
        return serde_json::from_str::<Value>(&raw).map_or_else(
            |_| {
                json!({
                    "status": "invalid",
                    "source": "env:PI_EXT_PMU_BASELINE_COUNTERS_JSON",
                    "reason": "failed_to_parse_json",
                    "budget": budget.as_json(),
                })
            },
            |parsed| {
                annotate_pmu_payload(
                    &parsed,
                    "env:PI_EXT_PMU_BASELINE_COUNTERS_JSON",
                    None,
                    budget,
                )
            },
        );
    }

    if let Ok(path) = std::env::var("PI_EXT_PMU_BASELINE_COUNTERS_PATH") {
        return fs::read_to_string(&path).map_or_else(
            |_| {
                json!({
                    "status": "missing",
                    "source": "env:PI_EXT_PMU_BASELINE_COUNTERS_PATH",
                    "path": path,
                    "budget": budget.as_json(),
                })
            },
            |raw| {
                serde_json::from_str::<Value>(&raw).map_or_else(
                    |_| {
                        json!({
                            "status": "invalid",
                            "source": "env:PI_EXT_PMU_BASELINE_COUNTERS_PATH",
                            "path": path,
                            "reason": "failed_to_parse_json",
                            "budget": budget.as_json(),
                        })
                    },
                    |parsed| {
                        annotate_pmu_payload(
                            &parsed,
                            "env:PI_EXT_PMU_BASELINE_COUNTERS_PATH",
                            Some(&path),
                            budget,
                        )
                    },
                )
            },
        );
    }

    json!({
        "status": "not_collected",
        "reason": "set PI_EXT_PMU_BASELINE_COUNTERS_JSON or PI_EXT_PMU_BASELINE_COUNTERS_PATH for before/after PMU comparison",
        "budget": budget.as_json(),
    })
}

fn pmu_counters_from_meta(meta: &Value) -> Option<PmuCountersNormalized> {
    meta.get("normalized_counters")
        .and_then(parse_pmu_counters)
        .or_else(|| parse_pmu_counters(meta))
}

fn record_weight(record: &Value) -> u64 {
    json_number_as_u64(record.get("iterations"))
        .or_else(|| json_number_as_u64(record.get("runs")))
        .or_else(|| json_number_as_u64(record.get("summary").and_then(|v| v.get("count"))))
        .unwrap_or(1)
        .max(1)
}

fn weighted_percentile(values: &[(f64, u64)], numerator: u64, denominator: u64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|lhs, rhs| lhs.0.total_cmp(&rhs.0));
    let total_weight = sorted
        .iter()
        .fold(0_u64, |acc, (_, weight)| acc.saturating_add(*weight));
    if total_weight == 0 {
        return None;
    }
    let target = total_weight
        .saturating_mul(numerator)
        .saturating_add(denominator.saturating_sub(1))
        / denominator.max(1);
    let mut cumulative = 0_u64;
    for (value, weight) in &sorted {
        cumulative = cumulative.saturating_add(*weight);
        if cumulative >= target.max(1) {
            return Some(*value);
        }
    }
    Some(sorted[sorted.len() - 1].0)
}

fn weighted_summary(values: &[(f64, u64)]) -> Value {
    if values.is_empty() {
        return json!({
            "status": "not_available",
            "reason": "no_weighted_samples",
        });
    }
    let total_weight = values
        .iter()
        .fold(0_u64, |acc, (_, weight)| acc.saturating_add(*weight));
    if total_weight == 0 {
        return json!({
            "status": "not_available",
            "reason": "zero_total_weight",
        });
    }

    let weighted_sum = values
        .iter()
        .fold(0.0, |acc, (value, weight)| acc + (value * (*weight as f64)));
    let mean = weighted_sum / total_weight as f64;
    let min = values
        .iter()
        .map(|(value, _)| *value)
        .min_by(f64::total_cmp)
        .unwrap_or(0.0);
    let max = values
        .iter()
        .map(|(value, _)| *value)
        .max_by(f64::total_cmp)
        .unwrap_or(0.0);

    json!({
        "status": "observed",
        "sample_weight_total": total_weight,
        "min": min,
        "max": max,
        "mean": mean,
        "p50": weighted_percentile(values, 50, 100),
        "p95": weighted_percentile(values, 95, 100),
        "p99": weighted_percentile(values, 99, 100),
    })
}

fn latency_outcome_summary(records: &[Value]) -> Value {
    let weighted = records
        .iter()
        .filter_map(parse_profile_record)
        .map(|entry| (entry.per_call_us, entry.samples.max(1)))
        .collect::<Vec<_>>();
    weighted_summary(&weighted)
}

fn throughput_outcome_summary(records: &[Value]) -> Value {
    let weighted = records
        .iter()
        .filter_map(|record| {
            json_number_as_f64(record.get("calls_per_sec")).map(|throughput| {
                let weight = record_weight(record);
                (throughput, weight)
            })
        })
        .collect::<Vec<_>>();
    weighted_summary(&weighted)
}

fn parse_perf_outcome_snapshot(raw: &Value) -> Option<PerfOutcomeSnapshot> {
    let root = raw
        .get("outcomes")
        .and_then(Value::as_object)
        .or_else(|| raw.as_object())?;
    let p50_us = value_as_f64(root.get("p50_us").or_else(|| root.get("p50")));
    let p95_us = value_as_f64(root.get("p95_us").or_else(|| root.get("p95")));
    let p99_us = value_as_f64(root.get("p99_us").or_else(|| root.get("p99")));
    let throughput_eps = value_as_f64(
        root.get("throughput_eps")
            .or_else(|| root.get("calls_per_sec"))
            .or_else(|| root.get("throughput")),
    );
    let rss_mb = value_as_f64(root.get("rss_mb"))
        .or_else(|| value_as_f64(root.get("rss_kb")).map(|rss_kb| (rss_kb / 1024.0).max(0.0)));
    let cpu_pct = value_as_f64(root.get("cpu_pct"));
    let io_wait_pct = value_as_f64(root.get("io_wait_pct"));

    if p50_us.is_none()
        && p95_us.is_none()
        && p99_us.is_none()
        && throughput_eps.is_none()
        && rss_mb.is_none()
        && cpu_pct.is_none()
        && io_wait_pct.is_none()
    {
        None
    } else {
        Some(PerfOutcomeSnapshot {
            p50_us,
            p95_us,
            p99_us,
            throughput_eps,
            rss_mb,
            cpu_pct,
            io_wait_pct,
        })
    }
}

fn collect_perf_baseline_metadata() -> Value {
    if let Ok(raw) = std::env::var("PI_EXT_BASELINE_OUTCOMES_JSON") {
        return serde_json::from_str::<Value>(&raw).map_or_else(
            |_| {
                json!({
                    "status": "invalid",
                    "source": "env:PI_EXT_BASELINE_OUTCOMES_JSON",
                    "reason": "failed_to_parse_json",
                })
            },
            |parsed| {
                let parsed_snapshot = parse_perf_outcome_snapshot(&parsed);
                json!({
                    "status": if parsed_snapshot.is_some() { "collected" } else { "invalid" },
                    "source": "env:PI_EXT_BASELINE_OUTCOMES_JSON",
                    "outcomes": parsed,
                    "normalized": parsed_snapshot,
                })
            },
        );
    }

    if let Ok(path) = std::env::var("PI_EXT_BASELINE_OUTCOMES_PATH") {
        return fs::read_to_string(&path).map_or_else(
            |_| {
                json!({
                    "status": "missing",
                    "source": "env:PI_EXT_BASELINE_OUTCOMES_PATH",
                    "path": path,
                })
            },
            |raw| {
                serde_json::from_str::<Value>(&raw).map_or_else(
                    |_| {
                        json!({
                            "status": "invalid",
                            "source": "env:PI_EXT_BASELINE_OUTCOMES_PATH",
                            "path": path,
                            "reason": "failed_to_parse_json",
                        })
                    },
                    |parsed| {
                        let parsed_snapshot = parse_perf_outcome_snapshot(&parsed);
                        json!({
                            "status": if parsed_snapshot.is_some() { "collected" } else { "invalid" },
                            "source": "env:PI_EXT_BASELINE_OUTCOMES_PATH",
                            "path": path,
                            "outcomes": parsed,
                            "normalized": parsed_snapshot,
                        })
                    },
                )
            },
        );
    }

    json!({
        "status": "not_collected",
        "reason": "set PI_EXT_BASELINE_OUTCOMES_JSON or PI_EXT_BASELINE_OUTCOMES_PATH for before/after outcome deltas",
    })
}

fn candidate_outcome_snapshot(records: &[Value]) -> PerfOutcomeSnapshot {
    let latency = latency_outcome_summary(records);
    let throughput = throughput_outcome_summary(records);
    let latency_view = latency
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "observed")
        .then_some(&latency)
        .and_then(Value::as_object);
    let throughput_view = throughput
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "observed")
        .then_some(&throughput)
        .and_then(Value::as_object);

    PerfOutcomeSnapshot {
        p50_us: latency_view.and_then(|view| value_as_f64(view.get("p50"))),
        p95_us: latency_view.and_then(|view| value_as_f64(view.get("p95"))),
        p99_us: latency_view.and_then(|view| value_as_f64(view.get("p99"))),
        throughput_eps: throughput_view.and_then(|view| value_as_f64(view.get("mean"))),
        rss_mb: None,
        cpu_pct: None,
        io_wait_pct: None,
    }
}

fn delta(candidate: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    match (candidate, baseline) {
        (Some(candidate), Some(baseline)) => Some(candidate - baseline),
        _ => None,
    }
}

fn compare_perf_outcomes(
    baseline: Option<PerfOutcomeSnapshot>,
    candidate: PerfOutcomeSnapshot,
) -> Value {
    let Some(baseline) = baseline else {
        return json!({
            "status": "not_compared",
            "reason": "baseline_outcomes_unavailable",
            "candidate": candidate,
        });
    };

    let delta_p95 = delta(candidate.p95_us, baseline.p95_us);
    let delta_p99 = delta(candidate.p99_us, baseline.p99_us);
    let delta_throughput = delta(candidate.throughput_eps, baseline.throughput_eps);
    let delta_rss = delta(candidate.rss_mb, baseline.rss_mb);
    let delta_cpu = delta(candidate.cpu_pct, baseline.cpu_pct);
    let delta_iowait = delta(candidate.io_wait_pct, baseline.io_wait_pct);

    let latency_regressed =
        delta_p95.is_some_and(|delta| delta > 0.0) || delta_p99.is_some_and(|delta| delta > 0.0);
    let throughput_regressed = delta_throughput.is_some_and(|delta| delta < 0.0);
    let resource_regressed = delta_rss.is_some_and(|delta| delta > 0.0)
        || delta_cpu.is_some_and(|delta| delta > 0.0)
        || delta_iowait.is_some_and(|delta| delta > 0.0);

    json!({
        "status": "compared",
        "baseline": baseline,
        "candidate": candidate,
        "deltas": {
            "p95_us": delta_p95,
            "p99_us": delta_p99,
            "throughput_eps": delta_throughput,
            "rss_mb": delta_rss,
            "cpu_pct": delta_cpu,
            "io_wait_pct": delta_iowait,
        },
        "regressions": {
            "latency": latency_regressed,
            "throughput": throughput_regressed,
            "resource": resource_regressed,
            "overall": latency_regressed || throughput_regressed || resource_regressed,
        }
    })
}

fn collect_flame_metadata() -> Value {
    if let Ok(path) = std::env::var("PI_EXT_FLAMEGRAPH_PATH") {
        let exists = Path::new(&path).exists();
        return json!({
            "status": if exists { "collected" } else { "missing" },
            "path": path,
            "exists": exists,
        });
    }

    json!({
        "status": "not_collected",
        "reason": "set PI_EXT_FLAMEGRAPH_PATH to attach flamegraph artifact",
    })
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    if len == 0 {
        return 0;
    }
    // Ceil(rank) then convert to 0-index.
    let rank = len
        .saturating_mul(numerator)
        .saturating_add(denominator - 1)
        / denominator;
    rank.saturating_sub(1).min(len - 1)
}

fn percentile(sorted_ms: &[f64], pct: usize) -> Option<f64> {
    if sorted_ms.is_empty() {
        return None;
    }
    Some(sorted_ms[percentile_index(sorted_ms.len(), pct, 100)])
}

fn summarize_ms(durations: &[Duration]) -> SummaryMs {
    if durations.is_empty() {
        return SummaryMs {
            count: 0,
            min_ms: None,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            max_ms: None,
        };
    }

    let mut ms = durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .collect::<Vec<_>>();
    ms.sort_by(f64::total_cmp);

    SummaryMs {
        count: ms.len(),
        min_ms: Some(ms[0]),
        p50_ms: percentile(&ms, 50),
        p95_ms: percentile(&ms, 95),
        p99_ms: percentile(&ms, 99),
        max_ms: Some(ms[ms.len() - 1]),
    }
}

struct BenchRuntime {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
}

async fn new_runtime(js_cwd: &str) -> Result<BenchRuntime> {
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], Path::new(js_cwd), None));
    let config = PiJsRuntimeConfig {
        cwd: js_cwd.to_string(),
        disk_cache_dir: None,
        ..Default::default()
    };
    let runtime = JsExtensionRuntimeHandle::start(config, tools, manager.clone()).await?;
    manager.set_js_runtime(runtime.clone());
    Ok(BenchRuntime { manager, runtime })
}

async fn load_extension(runtime: &BenchRuntime, spec: &JsExtensionLoadSpec) -> Result<()> {
    runtime.manager.load_js_extensions(vec![spec.clone()]).await
}

fn discover_real_corpus_specs(limit: usize) -> Result<Vec<JsExtensionLoadSpec>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = root.join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let raw = fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&raw).map_err(|err| Error::Json(Box::new(err)))?;
    let Some(entries) = manifest.get("extensions").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let artifact_root = root.join("tests/ext_conformance/artifacts");
    let mut specs = Vec::new();
    for entry in entries {
        if specs.len() >= limit {
            break;
        }
        if entry.get("source_tier").and_then(Value::as_str) != Some("official-pi-mono") {
            continue;
        }
        let caps = entry.get("capabilities").cloned().unwrap_or(Value::Null);
        if caps.get("uses_exec").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if caps.get("is_multi_file").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(entry_path) = entry.get("entry_path").and_then(Value::as_str) else {
            continue;
        };
        let full_path = artifact_root.join(entry_path);
        if !full_path.exists() {
            continue;
        }
        if let Ok(spec) = JsExtensionLoadSpec::from_entry_path(&full_path) {
            specs.push(spec);
        }
    }
    Ok(specs)
}

async fn scenario_load_init_cold(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    runs: usize,
    evidence: RegressionEvidence,
) -> Result<Value> {
    let mut timings = Vec::new();
    for _ in 0..runs {
        let start = Instant::now();
        let runtime = new_runtime(js_cwd).await?;
        load_extension(&runtime, spec).await?;
        timings.push(start.elapsed());
        if !runtime.manager.shutdown(Duration::from_secs(5)).await {
            return Err(Error::extension(
                "extension workload runtime did not shut down after cold load",
            ));
        }
    }

    Ok(json!({
        "schema": BENCH_SCHEMA,
        "runtime": "pi_agent_rust",
        "scenario": "ext_load_init/load_init_cold",
        "extension": spec.extension_id,
        "runs": runs,
        "summary": summarize_ms(&timings),
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": evidence.confidence,
        "eligible_for_regression_gate": evidence.eligible,
        "measurement_method": "wall_clock_observation",
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "uncontrolled",
    }))
}

async fn scenario_tool_call(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
    evidence: RegressionEvidence,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    let ctx = Arc::new(json!({"hasUI": false, "cwd": js_cwd}));
    let budget = Duration::from_secs(30);
    let started_at = Instant::now();
    for _ in 0..iterations {
        if started_at.elapsed() >= budget {
            let _ = runtime.manager.shutdown(Duration::from_secs(5)).await;
            return Err(Error::extension(format!(
                "tool-call benchmark timed out after {}ms",
                budget.as_millis()
            )));
        }
        let remaining_ms = u64::try_from(
            budget
                .saturating_sub(started_at.elapsed())
                .as_millis()
                .max(1),
        )
        .unwrap_or(u64::MAX);
        let _ = runtime
            .runtime
            .execute_tool(
                "hello".to_string(),
                "bench-call-1".to_string(),
                json!({"name": "World"}),
                Arc::clone(&ctx),
                remaining_ms,
            )
            .await?;
    }
    let elapsed = started_at.elapsed();
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(Error::extension(
            "extension workload runtime did not shut down after tool calls",
        ));
    }
    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations);
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": BENCH_SCHEMA,
        "runtime": "pi_agent_rust",
        "scenario": "ext_tool_call/hello",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": evidence.confidence,
        "eligible_for_regression_gate": evidence.eligible,
        "measurement_method": "wall_clock_observation",
        "unexpected_hostcalls": null,
        "unexpected_hostcalls_observable": false,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
    }))
}

async fn scenario_event_hook(
    spec: &JsExtensionLoadSpec,
    js_cwd: &str,
    iterations: u32,
    evidence: RegressionEvidence,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    load_extension(&runtime, spec).await?;

    let event_payload = json!({"systemPrompt": "You are Pi."});
    let budget = Duration::from_secs(30);
    let started_at = Instant::now();
    for _ in 0..iterations {
        if started_at.elapsed() >= budget {
            let _ = runtime.manager.shutdown(Duration::from_secs(5)).await;
            return Err(Error::extension(format!(
                "event-hook benchmark timed out after {}ms",
                budget.as_millis()
            )));
        }
        let remaining_ms = u64::try_from(
            budget
                .saturating_sub(started_at.elapsed())
                .as_millis()
                .max(1),
        )
        .unwrap_or(u64::MAX);
        let _ = runtime
            .manager
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(event_payload.clone()),
                remaining_ms,
            )
            .await?;
    }
    let elapsed = started_at.elapsed();
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(Error::extension(
            "extension workload runtime did not shut down after event dispatch",
        ));
    }
    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations);
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": BENCH_SCHEMA,
        "runtime": "pi_agent_rust",
        "scenario": "ext_event_hook/before_agent_start",
        "extension": spec.extension_id,
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": evidence.confidence,
        "eligible_for_regression_gate": evidence.eligible,
        "measurement_method": "wall_clock_observation",
        "unexpected_hostcalls": null,
        "unexpected_hostcalls_observable": false,
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
    }))
}

async fn scenario_long_session_real_corpus(
    specs: &[JsExtensionLoadSpec],
    js_cwd: &str,
    iterations: u32,
    evidence: RegressionEvidence,
) -> Result<Value> {
    let runtime = new_runtime(js_cwd).await?;
    runtime.manager.load_js_extensions(specs.to_vec()).await?;
    let loaded_extension_ids = specs
        .iter()
        .map(|spec| spec.extension_id.clone())
        .collect::<Vec<_>>();

    let event_payload = json!({
        "systemPrompt": "You are Pi.",
        "mode": "long-session",
    });

    let budget_secs = u64::from(iterations).saturating_div(200).clamp(30, 600);
    let budget = Duration::from_secs(budget_secs);
    let started_at = Instant::now();
    for _ in 0..iterations {
        if started_at.elapsed() >= budget {
            let _ = runtime.manager.shutdown(Duration::from_secs(5)).await;
            return Err(Error::extension(format!(
                "long-session benchmark timed out after {}ms",
                budget.as_millis()
            )));
        }
        let remaining_ms = u64::try_from(
            budget
                .saturating_sub(started_at.elapsed())
                .as_millis()
                .max(1),
        )
        .unwrap_or(u64::MAX);
        let _ = runtime
            .manager
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(event_payload.clone()),
                remaining_ms,
            )
            .await?;
    }
    let elapsed = started_at.elapsed();
    if !runtime.manager.shutdown(Duration::from_secs(5)).await {
        return Err(Error::extension(
            "extension workload runtime did not shut down after long session",
        ));
    }
    let elapsed_us = elapsed.as_secs_f64() * 1_000_000.0;
    let iters_f = f64::from(iterations);
    let per_call_us = elapsed_us / iters_f;
    let calls_per_sec = iters_f / elapsed.as_secs_f64().max(1e-12);

    Ok(json!({
        "schema": BENCH_SCHEMA,
        "runtime": "pi_agent_rust",
        "scenario": "ext_hostcall_bridge/long_session_real_corpus",
        "extension": format!("real_corpus_{}ext", loaded_extension_ids.len()),
        "iterations": iterations,
        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
        "per_call_us": per_call_us,
        "calls_per_sec": calls_per_sec,
        "extensions_loaded": loaded_extension_ids,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": evidence.confidence,
        "eligible_for_regression_gate": evidence.eligible,
        "measurement_method": "wall_clock_observation",
        "unexpected_hostcalls": null,
        "unexpected_hostcalls_observable": false,
        "profile_class": "long_session",
        "measurement_boundary": MEASUREMENT_BOUNDARY,
        "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
    }))
}

fn stage_weights_for_scenario(scenario: &str) -> StageWeights {
    if scenario.contains("load_init") {
        StageWeights {
            marshal: 0.33,
            queue: 0.12,
            schedule: 0.15,
            policy: 0.12,
            execute: 0.20,
            io: 0.08,
        }
    } else if scenario.contains("tool_call") {
        StageWeights {
            marshal: 0.08,
            queue: 0.14,
            schedule: 0.10,
            policy: 0.20,
            execute: 0.34,
            io: 0.14,
        }
    } else if scenario.contains("event_hook") {
        StageWeights {
            marshal: 0.09,
            queue: 0.16,
            schedule: 0.17,
            policy: 0.18,
            execute: 0.28,
            io: 0.12,
        }
    } else if scenario.contains("long_session") {
        StageWeights {
            marshal: 0.06,
            queue: 0.22,
            schedule: 0.16,
            policy: 0.18,
            execute: 0.26,
            io: 0.12,
        }
    } else {
        StageWeights {
            marshal: 0.16,
            queue: 0.16,
            schedule: 0.16,
            policy: 0.16,
            execute: 0.20,
            io: 0.16,
        }
    }
}

fn json_number_as_f64(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_u64).map(|v| v as f64))
        .or_else(|| value.and_then(Value::as_i64).map(|v| v as f64))
}

fn json_number_as_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).or_else(|| {
        value
            .and_then(Value::as_i64)
            .and_then(|v| u64::try_from(v).ok())
    })
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !value.bytes().all(|byte| byte == b'0')
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && !value.bytes().all(|byte| byte == b'0')
}

fn record_metadata_value<'a>(record: &'a Value, field: &str) -> Option<&'a Value> {
    record
        .get(field)
        .or_else(|| record.get("env").and_then(|env| env.get(field)))
}

fn record_provenance_duplicates_match(record: &Value) -> bool {
    let Some(env) = record.get("env").and_then(Value::as_object) else {
        return false;
    };
    BUILD_PROVENANCE_FIELDS.iter().all(|field| {
        record
            .get(*field)
            .is_some_and(|value| env.get(*field) == Some(value))
    })
}

fn parse_profile_record(record: &Value) -> Option<ParsedProfileRecord> {
    if record.get("schema").and_then(Value::as_str) != Some(BENCH_SCHEMA) {
        return None;
    }
    let scenario = record
        .get("scenario")
        .and_then(Value::as_str)
        .map(ToString::to_string)?;
    let extension = record
        .get("extension")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_string(), ToString::to_string);
    let samples = json_number_as_u64(record.get("iterations"))
        .or_else(|| json_number_as_u64(record.get("runs")))
        .or_else(|| json_number_as_u64(record.get("summary").and_then(|v| v.get("count"))))
        .unwrap_or(0);
    let per_call_us = json_number_as_f64(record.get("per_call_us")).or_else(|| {
        json_number_as_f64(record.get("summary").and_then(|v| v.get("p95_ms")))
            .map(|ms| ms * 1000.0)
    });

    let total_us = match (per_call_us, samples) {
        (Some(per_call), s) if s > 0 => per_call * s as f64,
        _ => json_number_as_f64(record.get("elapsed_ms")).map_or(0.0, |ms| ms * 1000.0),
    };
    if total_us <= 0.0 {
        return None;
    }

    let weights = stage_weights_for_scenario(&scenario);
    let normalized_total = weights.sum();
    if (normalized_total - 1.0).abs() > 1e-9 {
        return None;
    }

    let evidence_class = record
        .get("evidence_class")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let source_confidence = record
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let declared_gate_eligible = record
        .get("eligible_for_regression_gate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let measurement_method = record
        .get("measurement_method")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let measurement_boundary = record
        .get("measurement_boundary")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let measurement_contract_version = record
        .get("measurement_contract_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let build_profile = record_metadata_value(record, "build_profile")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let debug_assertions = record_metadata_value(record, "debug_assertions")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let executable_build_profile = record_metadata_value(record, "executable_build_profile")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let reported_executable_profile_verified =
        record_metadata_value(record, "executable_profile_verified")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let reported_build_fingerprint_verified =
        record_metadata_value(record, "build_fingerprint_verified")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let reported_build_profile_verified = record_metadata_value(record, "build_profile_verified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let build_fingerprint_contract = record_metadata_value(record, "build_fingerprint_contract")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let compiled_profile_family = record_metadata_value(record, "compiled_profile_family")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let compiled_opt_level = record_metadata_value(record, "compiled_opt_level")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let compiled_debug = record_metadata_value(record, "compiled_debug")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let compiled_features = record_metadata_value(record, "compiled_features")
        .and_then(Value::as_array)
        .and_then(|features| {
            features
                .iter()
                .map(Value::as_str)
                .map(|feature| feature.map(ToString::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    let binary_path = record_metadata_value(record, "binary_path")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let binary_sha256 = record_metadata_value(record, "binary_sha256")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let source_commit = record_metadata_value(record, "source_commit")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let source_dirty = record_metadata_value(record, "source_dirty")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let config_hash = record_metadata_value(record, "config_hash")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let disk_cache_policy = record
        .get("disk_cache_policy")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let host_page_cache_policy = record
        .get("host_page_cache_policy")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let executable_profile_matches = executable_build_profile == "perf"
        && perf_build::profile_from_target_path(Path::new(&binary_path)).as_deref() == Some("perf");
    let build_fingerprint_matches = build_fingerprint_contract
        == perf_build::BUILD_FINGERPRINT_CONTRACT
        && perf_build::matches_canonical_perf_build_fingerprint(
            &compiled_profile_family,
            &compiled_opt_level,
            &compiled_debug,
        );
    let executable_profile_verified =
        reported_executable_profile_verified && executable_profile_matches;
    let build_fingerprint_verified =
        reported_build_fingerprint_verified && build_fingerprint_matches;
    let build_profile_verified = reported_build_profile_verified
        && executable_profile_verified
        && build_fingerprint_verified;
    let compiled_feature_refs = compiled_features
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let expected_config_hash =
        perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
            source_commit: &source_commit,
            source_dirty,
            build_profile: &build_profile,
            executable_build_profile: &executable_build_profile,
            verification: perf_build::BenchmarkBuildVerification {
                executable_profile: reported_executable_profile_verified,
                build_fingerprint: reported_build_fingerprint_verified,
                build_profile: reported_build_profile_verified,
            },
            build_fingerprint_contract: &build_fingerprint_contract,
            compiled_profile_family: &compiled_profile_family,
            compiled_opt_level: &compiled_opt_level,
            compiled_debug: &compiled_debug,
            compiled_features: &compiled_feature_refs,
            binary_path: &binary_path,
            binary_sha256: &binary_sha256,
            debug_assertions,
        });
    let config_hash_verified = config_hash == expected_config_hash;
    let source_identity_verified = is_full_git_sha(&source_commit) && !source_dirty;
    let compiled_features_canonical =
        !compiled_features.is_empty() && compiled_features.windows(2).all(|pair| pair[0] < pair[1]);
    let exact_measurement_contract = measurement_method == "wall_clock_observation"
        && measurement_boundary == MEASUREMENT_BOUNDARY
        && measurement_contract_version == MEASUREMENT_CONTRACT_VERSION;
    let controlled_cache_contract = disk_cache_policy == "disabled"
        && host_page_cache_policy == "not_applicable_measured_region";
    let effective_gate_eligible = declared_gate_eligible
        && samples > 0
        && evidence_class == EVIDENCE_CLASS_MEASURED
        && source_confidence == CONFIDENCE_HIGH
        && exact_measurement_contract
        && build_profile == "perf"
        && executable_profile_verified
        && build_fingerprint_verified
        && build_profile_verified
        && source_identity_verified
        && compiled_features_canonical
        && config_hash_verified
        && record_provenance_duplicates_match(record)
        && !debug_assertions
        && is_lower_hex_sha256(&binary_sha256)
        && controlled_cache_contract;

    Some(ParsedProfileRecord {
        scenario,
        extension,
        samples,
        total_us,
        per_call_us: per_call_us.unwrap_or_else(|| total_us / (samples.max(1) as f64)),
        weights,
        evidence_class,
        source_confidence,
        declared_gate_eligible,
        effective_gate_eligible,
        measurement_method,
        measurement_boundary,
        measurement_contract_version,
        build_profile,
        debug_assertions,
        executable_build_profile,
        executable_profile_verified,
        build_fingerprint_verified,
        build_profile_verified,
        build_fingerprint_contract,
        compiled_profile_family,
        compiled_opt_level,
        compiled_debug,
        compiled_features,
        binary_path,
        binary_sha256,
        source_commit,
        source_dirty,
        config_hash,
        disk_cache_policy,
        host_page_cache_policy,
    })
}

fn stage_optimization_potential(stage: &str) -> f64 {
    match stage {
        "queue" => 0.34,
        "schedule" => 0.24,
        "policy" => 0.22,
        "execute" => 0.29,
        "marshal" => 0.16,
        "io" => 0.18,
        _ => 0.10,
    }
}

fn stage_recommendation(stage: &str) -> &'static str {
    match stage {
        "queue" => "Batch hostcall dequeues and reduce lock contention in queue drains",
        "schedule" => {
            "Reduce scheduler turn count and amortize microtask drains per hostcall burst"
        }
        "policy" => "Cache policy/risk decisions on stable param-shape hashes",
        "execute" => "Specialize hot hostcall opcodes and fast-path common tool/session calls",
        "marshal" => "Reduce JSON canonicalization and hash cost on repeated shapes",
        "io" => "Coalesce transport I/O and introduce bounded async pipelining",
        _ => "Profile and optimize dominant path",
    }
}

fn stage_user_impact(stage_total_us: f64, total_samples: u64, potential: f64) -> Value {
    let samples = total_samples.max(1) as f64;
    let per_call_saving_us = (stage_total_us * potential) / samples;
    let interactive_resume_p95_ms = per_call_saving_us / 1000.0;
    let turn_latency_p95_ms = interactive_resume_p95_ms * 1.2;
    json!({
        "interactive_resume_p95_ms": interactive_resume_p95_ms,
        "turn_latency_p95_ms": turn_latency_p95_ms,
        "note": "Projected savings from stage-specific optimization potential on observed traces",
    })
}

fn stage_probe_cost_ms(stage: &str, pmu_multiplier: f64) -> f64 {
    let base = match stage {
        "queue" => 42.0,
        "schedule" => 36.0,
        "execute" => 34.0,
        "policy" => 24.0,
        "io" => 28.0,
        "marshal" => 18.0,
        _ => 22.0,
    };
    let pressure_factor = (1.0 + ((pmu_multiplier - 1.0).max(0.0) * 0.5)).clamp(0.8, 2.0);
    base * pressure_factor
}

fn stage_probe_recommendation(stage: &str) -> &'static str {
    match stage {
        "queue" => "sample queue depth histogram + lock contention counters",
        "schedule" => "sample scheduler turn duration and microtask batch size",
        "execute" => "sample opcode-level latency histogram + tail breakdown",
        "policy" => "sample policy cache hit/miss + risk decision latency",
        "marshal" => "sample shape-hash cardinality + serialization cost",
        "io" => "sample transport wait time + async pipeline occupancy",
        _ => "sample stage latency and variance",
    }
}

fn pair_probe_recommendation(left: &str, right: &str) -> String {
    format!(
        "{} + {}",
        stage_probe_recommendation(left),
        stage_probe_recommendation(right)
    )
}

fn build_interference_matrix(
    parsed: &[ParsedProfileRecord],
) -> (Vec<Value>, InterferenceMatrixCompletenessReport) {
    let mut pair_totals_us = BTreeMap::<String, f64>::new();
    let mut pair_samples = BTreeMap::<String, u64>::new();
    let mut pair_scenario_us = BTreeMap::<String, BTreeMap<String, f64>>::new();

    for entry in parsed {
        for (left_idx, left_stage) in STAGE_DECOMPOSITION.iter().enumerate() {
            for right_stage in STAGE_DECOMPOSITION.iter().skip(left_idx + 1) {
                let Some(pair_key) = format_interference_pair_key(left_stage, right_stage) else {
                    continue;
                };
                let interaction_us = entry.total_us
                    * stage_weight_component(entry.weights, left_stage)
                    * stage_weight_component(entry.weights, right_stage);
                *pair_totals_us.entry(pair_key.clone()).or_insert(0.0) += interaction_us;
                *pair_samples.entry(pair_key.clone()).or_insert(0) += entry.samples;
                let scenario_totals = pair_scenario_us.entry(pair_key).or_default();
                *scenario_totals.entry(entry.scenario.clone()).or_insert(0.0) += interaction_us;
            }
        }
    }

    let total_interaction_us = pair_totals_us.values().sum::<f64>().max(1.0);
    let mut interference_entries = pair_totals_us
        .into_iter()
        .map(|(pair, interaction_us)| {
            let sample_count = pair_samples.get(&pair).copied().unwrap_or(0);
            let dominant_scenario = pair_scenario_us
                .get(&pair)
                .and_then(|scenario_totals| {
                    scenario_totals
                        .iter()
                        .max_by(|lhs, rhs| lhs.1.total_cmp(rhs.1))
                        .map(|(scenario, _)| scenario.clone())
                })
                .unwrap_or_else(|| "unknown".to_string());
            let (left_stage, right_stage) = parse_interference_pair_key(&pair)
                .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
            json!({
                "pair": pair,
                "stages": [left_stage, right_stage],
                "interaction_us": interaction_us,
                "share_pct": (interaction_us / total_interaction_us) * 100.0,
                "avg_us_per_sample": interaction_us / (sample_count.max(1) as f64),
                "sample_count": sample_count,
                "dominant_scenario": dominant_scenario,
                "recommended_probe": pair_probe_recommendation(&left_stage, &right_stage),
            })
        })
        .collect::<Vec<_>>();
    interference_entries.sort_by(|lhs, rhs| {
        let lhs_score = json_number_as_f64(lhs.get("interaction_us")).unwrap_or(0.0);
        let rhs_score = json_number_as_f64(rhs.get("interaction_us")).unwrap_or(0.0);
        rhs_score.total_cmp(&lhs_score)
    });

    let levers = STAGE_DECOMPOSITION
        .iter()
        .map(|stage| (*stage).to_string())
        .collect::<Vec<_>>();
    let observed_pair_keys = interference_entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("pair")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    let completeness = evaluate_interference_matrix_completeness(&levers, &observed_pair_keys);

    (interference_entries, completeness)
}

fn parse_rfc3339_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let raw = value.and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

fn evidence_age_hours(run_metadata: &Value, now: DateTime<Utc>) -> Option<f64> {
    let ts = parse_rfc3339_timestamp(run_metadata.get("finished_at"))
        .or_else(|| parse_rfc3339_timestamp(run_metadata.get("started_at")))?;
    let age_secs = (now.timestamp() - ts.timestamp()).max(0) as f64;
    Some(age_secs / 3600.0)
}

fn rank_voi_candidates(candidates: &mut [VoiCandidate]) {
    candidates.sort_by(|lhs, rhs| {
        rhs.voi_score
            .total_cmp(&lhs.voi_score)
            .then_with(|| rhs.utility_total.total_cmp(&lhs.utility_total))
            .then_with(|| lhs.stage.cmp(&rhs.stage))
    });
}

fn realized_information_gain(
    selected: &[VoiCandidate],
    correlation_strength: &str,
    outcome_regressed: bool,
    pmu_regressed: bool,
) -> Value {
    let expected_total = selected
        .iter()
        .map(|candidate| candidate.expected_information_gain)
        .sum::<f64>();
    let correlation_multiplier = match correlation_strength {
        "high" => 1.0,
        "moderate" => 0.7,
        "low" => 0.45,
        _ => 0.35,
    };
    let regression_penalty = if outcome_regressed && pmu_regressed {
        0.75
    } else if outcome_regressed || pmu_regressed {
        0.88
    } else {
        1.0
    };
    let realized_total = expected_total * correlation_multiplier * regression_penalty;
    json!({
        "expected_total": expected_total,
        "realized_total": realized_total,
        "correlation_strength": correlation_strength,
        "correlation_multiplier": correlation_multiplier,
        "regression_penalty": regression_penalty,
    })
}

fn build_voi_scheduler_plan(inputs: VoiPlannerInputs<'_>) -> Value {
    build_voi_scheduler_plan_at(inputs, Utc::now(), VoiBudgetConfig::from_env())
}

fn build_voi_scheduler_plan_at(
    inputs: VoiPlannerInputs<'_>,
    now: DateTime<Utc>,
    budget_config: VoiBudgetConfig,
) -> Value {
    let missing_telemetry = pmu_counters_from_meta(inputs.pmu_meta).is_none()
        || inputs.pmu_comparison.get("status").and_then(Value::as_str) != Some("compared");
    let evidence_age = evidence_age_hours(inputs.run_metadata, now);
    let stale_evidence =
        evidence_age.is_some_and(|age_hours| age_hours > budget_config.stale_after_hours);

    let correlation_strength = inputs
        .pmu_outcome_correlation
        .get("correlation_strength")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let pmu_worse = inputs
        .pmu_outcome_correlation
        .get("signals")
        .and_then(|signals| signals.get("pmu_worse"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome_regressed = inputs
        .outcome_comparison
        .get("regressions")
        .and_then(|regressions| regressions.get("overall"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pmu_regressed = inputs
        .pmu_comparison
        .get("regressions")
        .and_then(|regressions| regressions.get("overall"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let signal_mismatch = pmu_worse ^ outcome_regressed;
    let drift_detected = correlation_strength == "low" && signal_mismatch;

    let mut safe_mode_reasons = Vec::new();
    if missing_telemetry {
        safe_mode_reasons.push("missing_telemetry");
    }
    if stale_evidence {
        safe_mode_reasons.push("stale_evidence");
    }
    if drift_detected {
        safe_mode_reasons.push("estimator_drift");
    }
    let safe_mode = !safe_mode_reasons.is_empty();

    let mut candidates = inputs
        .hotspot_entries
        .iter()
        .filter_map(|entry| {
            let stage = entry
                .get("stage")
                .and_then(Value::as_str)
                .map(ToString::to_string)?;
            let ev_score = json_number_as_f64(entry.get("ev_score"))
                .unwrap_or(0.0)
                .max(0.0);
            let confidence = json_number_as_f64(entry.get("confidence"))
                .unwrap_or(0.5)
                .clamp(0.0, 1.0);
            let pmu_multiplier = json_number_as_f64(entry.get("pmu_multiplier"))
                .unwrap_or(1.0)
                .clamp(0.5, 3.0);
            let turn_latency_ms = entry
                .get("projected_user_impact")
                .and_then(|impact| impact.get("turn_latency_p95_ms"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0);

            let uncertainty_reduction = (1.0 - confidence) * (ev_score + 1.0);
            let user_impact_score = turn_latency_ms * 4.0;
            let pressure_bonus = (pmu_multiplier - 1.0).max(0.0) * 12.0;
            let utility_total = uncertainty_reduction + user_impact_score + pressure_bonus;
            let cost_ms = stage_probe_cost_ms(&stage, pmu_multiplier);
            let expected_information_gain = (utility_total / (cost_ms + 1.0)).clamp(0.0, 5.0);
            let voi_score = utility_total / cost_ms.max(1.0);

            Some(VoiCandidate {
                stage: stage.clone(),
                utility_total,
                uncertainty_reduction,
                user_impact_score,
                pressure_bonus,
                expected_information_gain,
                cost_ms,
                voi_score,
                recommended_probe: stage_probe_recommendation(&stage),
            })
        })
        .collect::<Vec<_>>();

    rank_voi_candidates(&mut candidates);
    let diagnostic_stage = safe_mode.then(|| {
        candidates
            .iter()
            .min_by(|lhs, rhs| {
                lhs.cost_ms
                    .total_cmp(&rhs.cost_ms)
                    .then_with(|| lhs.stage.cmp(&rhs.stage))
            })
            .map(|candidate| candidate.stage.clone())
    });
    let diagnostic_stage = diagnostic_stage.flatten();

    let mut remaining_budget_ms = budget_config.max_overhead_ms;
    let mut used_budget_ms = 0.0;
    let mut selected_count = 0_usize;
    let mut selected_candidates = Vec::new();
    let mut scored = Vec::new();

    for (idx, candidate) in candidates.iter().enumerate() {
        let mut selected = false;
        let mut skip_reason = None;

        if safe_mode {
            if diagnostic_stage
                .as_ref()
                .is_some_and(|stage| stage == &candidate.stage)
            {
                if candidate.cost_ms <= remaining_budget_ms {
                    selected = true;
                } else {
                    skip_reason = Some("diagnostic_budget_exceeded".to_string());
                }
            } else {
                skip_reason = Some("safe_mode_guardrail".to_string());
            }
        } else if selected_count >= budget_config.max_experiments {
            skip_reason = Some("max_experiments_reached".to_string());
        } else if candidate.cost_ms > remaining_budget_ms {
            skip_reason = Some("budget_exceeded".to_string());
        } else if candidate.utility_total <= 0.01 {
            skip_reason = Some("low_expected_utility".to_string());
        } else {
            selected = true;
        }

        if selected {
            remaining_budget_ms = (remaining_budget_ms - candidate.cost_ms).max(0.0);
            used_budget_ms += candidate.cost_ms;
            selected_count += 1;
            selected_candidates.push(candidate.clone());
        }

        scored.push(ScoredVoiCandidate {
            candidate: candidate.clone(),
            rank: idx + 1,
            selected,
            skip_reason,
        });
    }

    let realized_gain = realized_information_gain(
        &selected_candidates,
        correlation_strength,
        outcome_regressed,
        pmu_regressed,
    );
    let candidate_rows = scored
        .iter()
        .map(|item| {
            json!({
                "stage": item.candidate.stage,
                "rank": item.rank,
                "selected": item.selected,
                "skip_reason": item.skip_reason,
                "cost_ms": item.candidate.cost_ms,
                "voi_score": item.candidate.voi_score,
                "expected_information_gain": item.candidate.expected_information_gain,
                "recommended_probe": item.candidate.recommended_probe,
                "utility": {
                    "uncertainty_reduction": item.candidate.uncertainty_reduction,
                    "user_impact_score": item.candidate.user_impact_score,
                    "pressure_bonus": item.candidate.pressure_bonus,
                    "total": item.candidate.utility_total,
                },
            })
        })
        .collect::<Vec<_>>();
    let selected_plan = scored
        .iter()
        .filter(|item| item.selected)
        .map(|item| {
            json!({
                "stage": item.candidate.stage,
                "rank": item.rank,
                "cost_ms": item.candidate.cost_ms,
                "expected_information_gain": item.candidate.expected_information_gain,
                "recommended_probe": item.candidate.recommended_probe,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "schema": VOI_SCHEDULER_SCHEMA,
        "status": if safe_mode { "safe_mode" } else { "active" },
        "selection_strategy": if safe_mode { "safe_mode_min_cost_probe" } else { "greedy_voi_under_budget" },
        "deterministic_tiebreak": "voi_score desc, utility.total desc, stage asc",
        "safe_mode_reasons": safe_mode_reasons,
        "estimator": {
            "missing_telemetry": missing_telemetry,
            "drift_detected": drift_detected,
            "stale_evidence": stale_evidence,
            "evidence_age_hours": evidence_age,
            "stale_after_hours": budget_config.stale_after_hours,
        },
        "budget": {
            "max_overhead_ms": budget_config.max_overhead_ms,
            "max_experiments": budget_config.max_experiments,
            "used_overhead_ms": used_budget_ms,
            "remaining_overhead_ms": remaining_budget_ms,
            "feasible": used_budget_ms <= budget_config.max_overhead_ms + f64::EPSILON,
        },
        "candidates": candidate_rows,
        "selected_plan": selected_plan,
        "realized_information_gain": realized_gain,
    })
}

fn pmu_budget_from_meta(pmu_meta: &Value) -> PmuBudgetSpec {
    let mut budget = PmuBudgetSpec::default();
    let Some(object) = pmu_meta.get("budget").and_then(Value::as_object) else {
        return budget;
    };
    if let Some(value) = value_as_f64(object.get("llc_miss_budget_pct")) {
        budget.llc_miss_budget_pct = value.clamp(0.1, 100.0);
    }
    if let Some(value) = value_as_f64(object.get("branch_miss_budget_pct")) {
        budget.branch_miss_budget_pct = value.clamp(0.1, 100.0);
    }
    if let Some(value) = value_as_f64(object.get("stall_total_budget_pct")) {
        budget.stall_total_budget_pct = value.clamp(0.1, 200.0);
    }
    budget
}

fn pmu_stage_multiplier(stage: &str, pmu_meta: &Value) -> f64 {
    let budget = pmu_budget_from_meta(pmu_meta);
    let counters = pmu_counters_from_meta(pmu_meta);

    let Some(counters) = counters else {
        return 1.0;
    };

    let pressure = pmu_pressure_score(&counters, budget);
    let stage_sensitivity = match stage {
        "queue" => 1.0,
        "schedule" => 0.95,
        "execute" => 0.8,
        "policy" => 0.65,
        "io" => 0.55,
        "marshal" => 0.35,
        _ => 0.5,
    };
    let adjustment = (pressure - 1.0) * 0.35 * stage_sensitivity;
    (1.0 + adjustment).clamp(0.7, 1.8)
}

fn stall_total_pct(counters: &PmuCountersNormalized) -> f64 {
    counters.frontend_stall_pct.unwrap_or(0.0) + counters.backend_stall_pct.unwrap_or(0.0)
}

fn compare_pmu_profiles(candidate_meta: &Value, baseline_meta: &Value) -> Value {
    let budget = pmu_budget_from_meta(candidate_meta);
    let Some(candidate) = pmu_counters_from_meta(candidate_meta) else {
        return json!({
            "status": "not_compared",
            "reason": "candidate_pmu_counters_unavailable",
        });
    };

    let Some(baseline) = pmu_counters_from_meta(baseline_meta) else {
        return json!({
            "status": "not_compared",
            "reason": "baseline_pmu_counters_unavailable",
            "candidate": candidate,
        });
    };

    let candidate_stall_total = stall_total_pct(&candidate);
    let baseline_stall_total = stall_total_pct(&baseline);
    let delta_llc = delta(candidate.llc_miss_pct, baseline.llc_miss_pct);
    let delta_branch = delta(candidate.branch_miss_pct, baseline.branch_miss_pct);
    let delta_stall = Some(candidate_stall_total - baseline_stall_total);
    let delta_cycles = delta(candidate.cycles_per_call, baseline.cycles_per_call);

    let regression_thresholds = json!({
        "llc_miss_delta_pct": DEFAULT_PMU_REGRESSION_LLC_DELTA_PCT,
        "branch_miss_delta_pct": DEFAULT_PMU_REGRESSION_BRANCH_DELTA_PCT,
        "stall_total_delta_pct": DEFAULT_PMU_REGRESSION_STALL_DELTA_PCT,
    });
    let llc_regressed = delta_llc.is_some_and(|delta| delta > DEFAULT_PMU_REGRESSION_LLC_DELTA_PCT);
    let branch_regressed =
        delta_branch.is_some_and(|delta| delta > DEFAULT_PMU_REGRESSION_BRANCH_DELTA_PCT);
    let stall_regressed =
        delta_stall.is_some_and(|delta| delta > DEFAULT_PMU_REGRESSION_STALL_DELTA_PCT);
    let budget_evaluation = evaluate_pmu_budget(&candidate, budget);
    let budget_failed = budget_evaluation
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "fail");
    let overall_regression = llc_regressed || branch_regressed || stall_regressed || budget_failed;

    json!({
        "status": "compared",
        "baseline": baseline,
        "candidate": candidate,
        "delta_pct_points": {
            "llc_miss_pct": delta_llc,
            "branch_miss_pct": delta_branch,
            "stall_total_pct": delta_stall,
            "cycles_per_call": delta_cycles,
        },
        "pressure": {
            "baseline": pmu_pressure_score(&baseline, budget),
            "candidate": pmu_pressure_score(&candidate, budget),
        },
        "regression_thresholds": regression_thresholds,
        "regressions": {
            "llc_miss": llc_regressed,
            "branch_miss": branch_regressed,
            "stall_total": stall_regressed,
            "budget": budget_failed,
            "overall": overall_regression,
        },
    })
}

fn build_pmu_outcome_correlation(
    pmu_comparison: &Value,
    candidate_outcomes: PerfOutcomeSnapshot,
    outcome_comparison: &Value,
) -> Value {
    let stall_delta = pmu_comparison
        .get("delta_pct_points")
        .and_then(|delta| delta.get("stall_total_pct"))
        .and_then(Value::as_f64);
    let llc_delta = pmu_comparison
        .get("delta_pct_points")
        .and_then(|delta| delta.get("llc_miss_pct"))
        .and_then(Value::as_f64);
    let branch_delta = pmu_comparison
        .get("delta_pct_points")
        .and_then(|delta| delta.get("branch_miss_pct"))
        .and_then(Value::as_f64);

    let outcome_regression = outcome_comparison
        .get("regressions")
        .and_then(|regressions| regressions.get("overall"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let p95_p50_ratio = match (candidate_outcomes.p95_us, candidate_outcomes.p50_us) {
        (Some(p95), Some(p50)) if p50 > 0.0 => Some(p95 / p50),
        _ => None,
    };
    let p99_p95_ratio = match (candidate_outcomes.p99_us, candidate_outcomes.p95_us) {
        (Some(p99), Some(p95)) if p95 > 0.0 => Some(p99 / p95),
        _ => None,
    };
    let tail_under_pressure = p95_p50_ratio.is_some_and(|ratio| ratio > 1.2)
        || p99_p95_ratio.is_some_and(|ratio| ratio > 1.1);

    let pmu_worse = stall_delta.is_some_and(|delta| delta > 0.0)
        || llc_delta.is_some_and(|delta| delta > 0.0)
        || branch_delta.is_some_and(|delta| delta > 0.0);

    let correlation_strength = if pmu_worse && outcome_regression {
        "high"
    } else if pmu_worse || outcome_regression || tail_under_pressure {
        "moderate"
    } else {
        "low"
    };

    json!({
        "status": "computed",
        "pmu_shift": {
            "stall_total_pct_delta": stall_delta,
            "llc_miss_pct_delta": llc_delta,
            "branch_miss_pct_delta": branch_delta,
        },
        "user_visible_latency": {
            "p50_us": candidate_outcomes.p50_us,
            "p95_us": candidate_outcomes.p95_us,
            "p99_us": candidate_outcomes.p99_us,
            "tail_ratios": {
                "p95_over_p50": p95_p50_ratio,
                "p99_over_p95": p99_p95_ratio,
            },
        },
        "resource_outcomes": {
            "throughput_eps": candidate_outcomes.throughput_eps,
            "rss_mb": candidate_outcomes.rss_mb,
            "cpu_pct": candidate_outcomes.cpu_pct,
            "io_wait_pct": candidate_outcomes.io_wait_pct,
        },
        "signals": {
            "pmu_worse": pmu_worse,
            "tail_under_pressure": tail_under_pressure,
            "outcome_regression": outcome_regression,
        },
        "correlation_strength": correlation_strength,
    })
}

fn build_hotspot_matrix(
    records: &[Value],
    run_metadata: &Value,
    trace_meta: &Value,
    pmu_meta: &Value,
    pmu_baseline_meta: &Value,
    flame_meta: &Value,
) -> Value {
    let parsed = records
        .iter()
        .filter_map(parse_profile_record)
        .collect::<Vec<_>>();
    let mut totals = StageTotals::default();
    let mut total_samples = 0_u64;
    let mut eligible_samples = 0_u64;
    let mut declared_eligible_records = 0_usize;
    let mut effective_eligible_records = 0_usize;
    let mut dirty_source_records = 0_usize;
    let mut debug_assertion_records = 0_usize;
    let mut unverified_executable_profile_records = 0_usize;
    let mut unverified_build_fingerprint_records = 0_usize;
    let mut unverified_profile_records = 0_usize;
    let mut uncontrolled_host_cache_records = 0_usize;
    let mut build_profiles = BTreeSet::new();
    let mut executable_build_profiles = BTreeSet::new();
    let mut build_fingerprint_contracts = BTreeSet::new();
    let mut compiled_profile_families = BTreeSet::new();
    let mut compiled_opt_levels = BTreeSet::new();
    let mut compiled_debug_values = BTreeSet::new();
    let mut compiled_feature_sets = BTreeSet::new();
    let mut binary_paths = BTreeSet::new();
    let mut binary_sha256s = BTreeSet::new();
    let mut source_commits = BTreeSet::new();
    let mut config_hashes = BTreeSet::new();
    let mut disk_cache_policies = BTreeSet::new();
    let mut host_page_cache_policies = BTreeSet::new();
    let mut scenario_breakdown = Vec::new();

    for entry in &parsed {
        let weighted = entry.weights.scaled(entry.total_us);
        totals = totals.add(weighted);
        total_samples = total_samples.saturating_add(entry.samples);
        if entry.declared_gate_eligible {
            declared_eligible_records += 1;
        }
        if entry.effective_gate_eligible {
            effective_eligible_records += 1;
            eligible_samples = eligible_samples.saturating_add(entry.samples);
        }
        if entry.debug_assertions {
            debug_assertion_records += 1;
        }
        if entry.source_dirty {
            dirty_source_records += 1;
        }
        if !entry.executable_profile_verified {
            unverified_executable_profile_records += 1;
        }
        if !entry.build_fingerprint_verified {
            unverified_build_fingerprint_records += 1;
        }
        if !entry.build_profile_verified {
            unverified_profile_records += 1;
        }
        if entry.host_page_cache_policy == "uncontrolled" {
            uncontrolled_host_cache_records += 1;
        }
        build_profiles.insert(entry.build_profile.clone());
        executable_build_profiles.insert(entry.executable_build_profile.clone());
        build_fingerprint_contracts.insert(entry.build_fingerprint_contract.clone());
        compiled_profile_families.insert(entry.compiled_profile_family.clone());
        compiled_opt_levels.insert(entry.compiled_opt_level.clone());
        compiled_debug_values.insert(entry.compiled_debug.clone());
        compiled_feature_sets.insert(entry.compiled_features.join(","));
        binary_paths.insert(entry.binary_path.clone());
        binary_sha256s.insert(entry.binary_sha256.clone());
        source_commits.insert(entry.source_commit.clone());
        config_hashes.insert(entry.config_hash.clone());
        disk_cache_policies.insert(entry.disk_cache_policy.clone());
        host_page_cache_policies.insert(entry.host_page_cache_policy.clone());
        scenario_breakdown.push(json!({
            "scenario": entry.scenario,
            "extension": entry.extension,
            "samples": entry.samples,
            "per_call_us": entry.per_call_us,
            "total_us": entry.total_us,
            "evidence_class": entry.evidence_class,
            "source_confidence": entry.source_confidence,
            "declared_eligible_for_regression_gate": entry.declared_gate_eligible,
            "effective_eligible_for_regression_gate": entry.effective_gate_eligible,
            "measurement_method": entry.measurement_method,
            "measurement_boundary": entry.measurement_boundary,
            "measurement_contract_version": entry.measurement_contract_version,
            "build_profile": entry.build_profile,
            "debug_assertions": entry.debug_assertions,
            "executable_build_profile": entry.executable_build_profile,
            "executable_profile_verified": entry.executable_profile_verified,
            "build_fingerprint_verified": entry.build_fingerprint_verified,
            "build_profile_verified": entry.build_profile_verified,
            "build_fingerprint_contract": entry.build_fingerprint_contract,
            "compiled_profile_family": entry.compiled_profile_family,
            "compiled_opt_level": entry.compiled_opt_level,
            "compiled_debug": entry.compiled_debug,
            "compiled_features": entry.compiled_features,
            "binary_path": entry.binary_path,
            "binary_sha256": entry.binary_sha256,
            "source_commit": entry.source_commit,
            "source_dirty": entry.source_dirty,
            "config_hash": entry.config_hash,
            "disk_cache_policy": entry.disk_cache_policy,
            "host_page_cache_policy": entry.host_page_cache_policy,
            "weights": {
                "marshal": entry.weights.marshal,
                "queue": entry.weights.queue,
                "schedule": entry.weights.schedule,
                "policy": entry.weights.policy,
                "execute": entry.weights.execute,
                "io": entry.weights.io,
            }
        }));
    }

    let grand_total = totals.total_us().max(1.0);
    let sample_confidence = ((total_samples as f64).ln_1p() / 8.0).clamp(0.35, 0.99);
    let all_source_records_eligible =
        !parsed.is_empty() && effective_eligible_records == parsed.len();
    let provenance_confidence_cap = if all_source_records_eligible {
        // Stage attribution remains inferred even when every source timing is
        // admissible measured evidence.
        0.80
    } else if effective_eligible_records > 0 {
        // A mixed corpus must not let a large eligible sample dominate the
        // uncontrolled, debug, or otherwise inadmissible source records.
        0.50
    } else {
        0.35
    };
    let confidence = sample_confidence.min(provenance_confidence_cap);
    let evidence_provenance = json!({
        "derived_evidence_class": "inferred",
        "derived_confidence": if all_source_records_eligible { "medium" } else { "low" },
        "eligible_for_regression_gate": false,
        "reason": "stage attribution uses scenario-specific inferred weights",
        "source_record_count": parsed.len(),
        "declared_eligible_record_count": declared_eligible_records,
        "effective_eligible_record_count": effective_eligible_records,
        "ineligible_record_count": parsed.len().saturating_sub(effective_eligible_records),
        "total_sample_count": total_samples,
        "eligible_sample_count": eligible_samples,
        "sample_confidence": sample_confidence,
        "provenance_confidence_cap": provenance_confidence_cap,
        "effective_confidence": confidence,
        "dirty_source_record_count": dirty_source_records,
        "debug_assertion_record_count": debug_assertion_records,
        "unverified_executable_profile_record_count": unverified_executable_profile_records,
        "unverified_build_fingerprint_record_count": unverified_build_fingerprint_records,
        "unverified_profile_record_count": unverified_profile_records,
        "uncontrolled_host_cache_record_count": uncontrolled_host_cache_records,
        "build_profiles": build_profiles.into_iter().collect::<Vec<_>>(),
        "executable_build_profiles": executable_build_profiles.into_iter().collect::<Vec<_>>(),
        "build_fingerprint_contracts": build_fingerprint_contracts.into_iter().collect::<Vec<_>>(),
        "compiled_profile_families": compiled_profile_families.into_iter().collect::<Vec<_>>(),
        "compiled_opt_levels": compiled_opt_levels.into_iter().collect::<Vec<_>>(),
        "compiled_debug_values": compiled_debug_values.into_iter().collect::<Vec<_>>(),
        "compiled_feature_sets": compiled_feature_sets.into_iter().collect::<Vec<_>>(),
        "binary_paths": binary_paths.into_iter().collect::<Vec<_>>(),
        "binary_sha256s": binary_sha256s.into_iter().collect::<Vec<_>>(),
        "source_commits": source_commits.into_iter().collect::<Vec<_>>(),
        "config_hashes": config_hashes.into_iter().collect::<Vec<_>>(),
        "disk_cache_policies": disk_cache_policies.into_iter().collect::<Vec<_>>(),
        "host_page_cache_policies": host_page_cache_policies.into_iter().collect::<Vec<_>>(),
    });
    let pmu_budget = pmu_budget_from_meta(pmu_meta);
    let pmu_counters = pmu_counters_from_meta(pmu_meta);
    let pmu_budget_eval = pmu_counters.as_ref().map_or_else(
        || {
            json!({
                "status": "not_evaluated",
                "reason": "PMU counters unavailable",
            })
        },
        |counters| evaluate_pmu_budget(counters, pmu_budget),
    );
    let latency_summary = latency_outcome_summary(records);
    let throughput_summary = throughput_outcome_summary(records);
    let baseline_outcomes_meta = collect_perf_baseline_metadata();
    let baseline_outcomes = baseline_outcomes_meta
        .get("normalized")
        .and_then(parse_perf_outcome_snapshot);
    let candidate_outcomes = candidate_outcome_snapshot(records);
    let outcome_comparison = compare_perf_outcomes(baseline_outcomes, candidate_outcomes);
    let pmu_comparison = compare_pmu_profiles(pmu_meta, pmu_baseline_meta);
    let pmu_outcome_correlation =
        build_pmu_outcome_correlation(&pmu_comparison, candidate_outcomes, &outcome_comparison);
    let stage_values = [
        ("marshal", totals.marshal),
        ("queue", totals.queue),
        ("schedule", totals.schedule),
        ("policy", totals.policy),
        ("execute", totals.execute),
        ("io", totals.io),
    ];

    let mut hotspot_entries = stage_values
        .iter()
        .map(|(stage, stage_total_us)| {
            let share_pct = (*stage_total_us / grand_total) * 100.0;
            let potential = stage_optimization_potential(stage);
            let pmu_multiplier = pmu_stage_multiplier(stage, pmu_meta);
            let ev_score = share_pct * potential * confidence * pmu_multiplier;
            json!({
                "stage": stage,
                "total_us": stage_total_us,
                "share_pct": share_pct,
                "avg_us_per_sample": stage_total_us / (total_samples.max(1) as f64),
                "optimization_potential_pct": potential * 100.0,
                "confidence": confidence,
                "pmu_multiplier": pmu_multiplier,
                "ev_score": ev_score,
                "projected_user_impact": stage_user_impact(*stage_total_us, total_samples, potential),
                "pmu_budget_evaluation": pmu_budget_eval.clone(),
                "recommended_action": stage_recommendation(stage),
                "downstream_beads": DEFAULT_DOWNSTREAM_BEADS,
            })
        })
        .collect::<Vec<_>>();

    hotspot_entries.sort_by(|a, b| {
        let lhs = json_number_as_f64(a.get("ev_score")).unwrap_or(0.0);
        let rhs = json_number_as_f64(b.get("ev_score")).unwrap_or(0.0);
        rhs.total_cmp(&lhs)
    });
    let (interference_matrix, interference_matrix_completeness) =
        build_interference_matrix(&parsed);
    let voi_scheduler = build_voi_scheduler_plan(VoiPlannerInputs {
        hotspot_entries: &hotspot_entries,
        run_metadata,
        pmu_meta,
        pmu_comparison: &pmu_comparison,
        pmu_outcome_correlation: &pmu_outcome_correlation,
        outcome_comparison: &outcome_comparison,
    });

    json!({
        "schema": HOTSPOT_MATRIX_SCHEMA,
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "source_schema": BENCH_SCHEMA,
        "records_analyzed": records.len(),
        "scenario_records": scenario_breakdown.len(),
        "sample_count": total_samples,
        "evidence_provenance": evidence_provenance,
        "run_metadata": run_metadata,
        "artifacts": {
            "trace_log": trace_meta,
            "pmu_counters": pmu_meta,
            "pmu_baseline_counters": pmu_baseline_meta,
            "pmu_budget": pmu_budget.as_json(),
            "pmu_budget_evaluation": pmu_budget_eval,
            "pmu_comparison": pmu_comparison,
            "latency_outcomes": latency_summary,
            "throughput_outcomes": throughput_summary,
            "baseline_outcomes": baseline_outcomes_meta,
            "outcome_comparison": outcome_comparison,
            "pmu_outcome_correlation": pmu_outcome_correlation,
            "flame_data": flame_meta,
        },
        "stage_totals_us": {
            "marshal": totals.marshal,
            "queue": totals.queue,
            "schedule": totals.schedule,
            "policy": totals.policy,
            "execute": totals.execute,
            "io": totals.io,
            "total": grand_total,
        },
        "hotspot_matrix": hotspot_entries,
        "interference_matrix": interference_matrix,
        "interference_matrix_completeness": interference_matrix_completeness,
        "voi_scheduler": voi_scheduler,
        "scenario_breakdown": scenario_breakdown,
        "downstream_consumers": DEFAULT_DOWNSTREAM_BEADS,
        "methodology": {
            "stage_decomposition": STAGE_DECOMPOSITION,
            "ev_formula": "share_pct * optimization_potential * confidence * pmu_multiplier",
            "confidence_formula": "min(clamp(log(sample_count+1)/8, 0.35, 0.99), provenance_confidence_cap)",
            "provenance_confidence_caps": {
                "all_sources_gate_eligible": 0.80,
                "mixed_eligibility": 0.50,
                "no_gate_eligible_sources": 0.35,
            },
            "interference_formula": "sum(total_us * stage_weight_a * stage_weight_b) over parsed scenarios",
            "notes": "Queue/schedule attribution is inferred from scenario-specific stage weights and is never itself regression-gate eligible. Source profile, cache, and eligibility provenance caps confidence before PMU counters shape ev_score. Before/after PMU deltas and outcome comparison remain diagnostic when source evidence is ineligible."
        },
    })
}

fn validate_hotspot_matrix_schema(matrix: &Value) -> Result<()> {
    let required_top = [
        "schema",
        "generated_at",
        "records_analyzed",
        "run_metadata",
        "evidence_provenance",
        "artifacts",
        "stage_totals_us",
        "hotspot_matrix",
        "interference_matrix",
        "interference_matrix_completeness",
        "voi_scheduler",
        "scenario_breakdown",
        "downstream_consumers",
    ];
    for field in required_top {
        if matrix.get(field).is_none() {
            return Err(Error::extension(format!(
                "hotspot matrix missing required field: {field}"
            )));
        }
    }

    if matrix.get("schema").and_then(Value::as_str) != Some(HOTSPOT_MATRIX_SCHEMA) {
        return Err(Error::extension(format!(
            "unexpected hotspot matrix schema: {:?}",
            matrix.get("schema")
        )));
    }
    let Some(stage_totals) = matrix.get("stage_totals_us").and_then(Value::as_object) else {
        return Err(Error::extension(
            "stage_totals_us must be an object".to_string(),
        ));
    };
    for field in STAGE_DECOMPOSITION
        .iter()
        .copied()
        .chain(std::iter::once("total"))
    {
        let Some(value) = stage_totals.get(field).and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "stage_totals_us missing numeric field: {field}"
            )));
        };
        if !value.is_finite() || value < 0.0 {
            return Err(Error::extension(format!(
                "stage_totals_us field {field} must be finite and non-negative"
            )));
        }
    }
    let Some(methodology) = matrix.get("methodology").and_then(Value::as_object) else {
        return Err(Error::extension(
            "methodology must be an object".to_string(),
        ));
    };
    let Some(stage_decomposition) = methodology
        .get("stage_decomposition")
        .and_then(Value::as_array)
    else {
        return Err(Error::extension(
            "methodology.stage_decomposition must be an array".to_string(),
        ));
    };
    if stage_decomposition.len() != STAGE_DECOMPOSITION.len() {
        return Err(Error::extension(format!(
            "methodology.stage_decomposition must contain {} entries",
            STAGE_DECOMPOSITION.len()
        )));
    }
    for (idx, expected) in STAGE_DECOMPOSITION.iter().enumerate() {
        let Some(actual) = stage_decomposition.get(idx).and_then(Value::as_str) else {
            return Err(Error::extension(format!(
                "methodology.stage_decomposition index {idx} must be a string"
            )));
        };
        if actual != *expected {
            return Err(Error::extension(format!(
                "methodology.stage_decomposition index {idx} expected {expected}, got {actual}"
            )));
        }
    }

    let Some(artifacts) = matrix.get("artifacts").and_then(Value::as_object) else {
        return Err(Error::extension(
            "hotspot matrix artifacts must be an object".to_string(),
        ));
    };
    for field in [
        "pmu_counters",
        "pmu_baseline_counters",
        "pmu_comparison",
        "latency_outcomes",
        "throughput_outcomes",
        "baseline_outcomes",
        "outcome_comparison",
        "pmu_outcome_correlation",
    ] {
        if artifacts.get(field).is_none() {
            return Err(Error::extension(format!(
                "hotspot matrix artifacts missing field: {field}"
            )));
        }
    }

    let Some(evidence_provenance) = matrix.get("evidence_provenance").and_then(Value::as_object)
    else {
        return Err(Error::extension(
            "evidence_provenance must be an object".to_string(),
        ));
    };
    if evidence_provenance
        .get("derived_evidence_class")
        .and_then(Value::as_str)
        != Some("inferred")
    {
        return Err(Error::extension(
            "evidence_provenance.derived_evidence_class must be inferred".to_string(),
        ));
    }
    if evidence_provenance
        .get("eligible_for_regression_gate")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Err(Error::extension(
            "inferred hotspot attribution must not be regression-gate eligible".to_string(),
        ));
    }
    for field in [
        "source_record_count",
        "declared_eligible_record_count",
        "effective_eligible_record_count",
        "ineligible_record_count",
        "total_sample_count",
        "eligible_sample_count",
        "dirty_source_record_count",
        "debug_assertion_record_count",
        "unverified_executable_profile_record_count",
        "unverified_build_fingerprint_record_count",
        "unverified_profile_record_count",
        "uncontrolled_host_cache_record_count",
    ] {
        if evidence_provenance
            .get(field)
            .and_then(Value::as_u64)
            .is_none()
        {
            return Err(Error::extension(format!(
                "evidence_provenance.{field} must be an integer"
            )));
        }
    }
    for field in [
        "build_profiles",
        "executable_build_profiles",
        "build_fingerprint_contracts",
        "compiled_profile_families",
        "compiled_opt_levels",
        "compiled_debug_values",
        "compiled_feature_sets",
        "binary_paths",
        "binary_sha256s",
        "source_commits",
        "config_hashes",
        "disk_cache_policies",
        "host_page_cache_policies",
    ] {
        if !evidence_provenance
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err(Error::extension(format!(
                "evidence_provenance.{field} must be an array of strings"
            )));
        }
    }
    let Some(confidence_cap) = evidence_provenance
        .get("provenance_confidence_cap")
        .and_then(Value::as_f64)
    else {
        return Err(Error::extension(
            "evidence_provenance.provenance_confidence_cap must be a number".to_string(),
        ));
    };
    let Some(effective_confidence) = evidence_provenance
        .get("effective_confidence")
        .and_then(Value::as_f64)
    else {
        return Err(Error::extension(
            "evidence_provenance.effective_confidence must be a number".to_string(),
        ));
    };
    let Some(sample_confidence) = evidence_provenance
        .get("sample_confidence")
        .and_then(Value::as_f64)
    else {
        return Err(Error::extension(
            "evidence_provenance.sample_confidence must be a number".to_string(),
        ));
    };
    if !(0.0..=1.0).contains(&sample_confidence)
        || !(0.0..=1.0).contains(&confidence_cap)
        || !(0.0..=confidence_cap).contains(&effective_confidence)
        || (effective_confidence - sample_confidence.min(confidence_cap)).abs() > f64::EPSILON
    {
        return Err(Error::extension(
            "hotspot confidence must be finite, within [0, 1], and no greater than its provenance cap"
                .to_string(),
        ));
    }
    let source_record_count = evidence_provenance
        .get("source_record_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let effective_eligible_record_count = evidence_provenance
        .get("effective_eligible_record_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let expected_confidence_cap =
        if source_record_count > 0 && effective_eligible_record_count == source_record_count {
            0.80
        } else if effective_eligible_record_count > 0 {
            0.50
        } else {
            0.35
        };
    if (confidence_cap - expected_confidence_cap).abs() > f64::EPSILON {
        return Err(Error::extension(format!(
            "evidence_provenance.provenance_confidence_cap must be {expected_confidence_cap:.2} for the declared source eligibility mix"
        )));
    }
    let unsafe_source_count = [
        "dirty_source_record_count",
        "debug_assertion_record_count",
        "unverified_profile_record_count",
        "uncontrolled_host_cache_record_count",
    ]
    .into_iter()
    .filter_map(|field| evidence_provenance.get(field).and_then(Value::as_u64))
    .fold(0_u64, u64::saturating_add);
    if unsafe_source_count > 0 && confidence_cap > 0.50 {
        return Err(Error::extension(
            "dirty-source, debug, unverified-profile, or uncontrolled-cache inputs must cap hotspot confidence at 0.50"
                .to_string(),
        ));
    }

    let Some(scenario_breakdown) = matrix.get("scenario_breakdown").and_then(Value::as_array)
    else {
        return Err(Error::extension(
            "scenario_breakdown must be an array".to_string(),
        ));
    };
    for (idx, row) in scenario_breakdown.iter().enumerate() {
        let Some(row_obj) = row.as_object() else {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} must be an object"
            )));
        };
        for field in [
            "scenario",
            "extension",
            "samples",
            "per_call_us",
            "total_us",
            "evidence_class",
            "source_confidence",
            "declared_eligible_for_regression_gate",
            "effective_eligible_for_regression_gate",
            "measurement_method",
            "measurement_boundary",
            "measurement_contract_version",
            "build_profile",
            "debug_assertions",
            "executable_build_profile",
            "executable_profile_verified",
            "build_fingerprint_verified",
            "build_profile_verified",
            "build_fingerprint_contract",
            "compiled_profile_family",
            "compiled_opt_level",
            "compiled_debug",
            "compiled_features",
            "binary_path",
            "binary_sha256",
            "source_commit",
            "source_dirty",
            "config_hash",
            "disk_cache_policy",
            "host_page_cache_policy",
            "weights",
        ] {
            if row_obj.get(field).is_none() {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} missing field: {field}"
                )));
            }
        }
        if row_obj.get("scenario").and_then(Value::as_str).is_none() {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field scenario must be a string"
            )));
        }
        if row_obj.get("extension").and_then(Value::as_str).is_none() {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field extension must be a string"
            )));
        }
        if row_obj.get("samples").and_then(Value::as_u64).is_none() {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field samples must be an integer"
            )));
        }
        if row_obj.get("per_call_us").and_then(Value::as_f64).is_none() {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field per_call_us must be a number"
            )));
        }
        if row_obj.get("total_us").and_then(Value::as_f64).is_none() {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field total_us must be a number"
            )));
        }
        for field in [
            "evidence_class",
            "source_confidence",
            "measurement_method",
            "measurement_boundary",
            "measurement_contract_version",
            "build_profile",
            "executable_build_profile",
            "build_fingerprint_contract",
            "compiled_profile_family",
            "compiled_opt_level",
            "compiled_debug",
            "binary_path",
            "binary_sha256",
            "source_commit",
            "config_hash",
            "disk_cache_policy",
            "host_page_cache_policy",
        ] {
            if row_obj.get(field).and_then(Value::as_str).is_none() {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} field {field} must be a string"
                )));
            }
        }
        for field in [
            "declared_eligible_for_regression_gate",
            "effective_eligible_for_regression_gate",
            "debug_assertions",
            "source_dirty",
            "executable_profile_verified",
            "build_fingerprint_verified",
            "build_profile_verified",
        ] {
            if row_obj.get(field).and_then(Value::as_bool).is_none() {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} field {field} must be a boolean"
                )));
            }
        }
        let compiled_features = row_obj
            .get("compiled_features")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Error::extension(format!(
                    "scenario_breakdown row {idx} field compiled_features must be an array"
                ))
            })?
            .iter()
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    Error::extension(format!(
                        "scenario_breakdown row {idx} compiled_features entries must be strings"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !compiled_features.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} compiled_features must be sorted and deduplicated"
            )));
        }
        if row_obj
            .get("effective_eligible_for_regression_gate")
            .and_then(Value::as_bool)
            == Some(true)
        {
            let binary_path = row_obj
                .get("binary_path")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let binary_sha256 = row_obj
                .get("binary_sha256")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let fingerprint_matches = perf_build::matches_canonical_perf_build_fingerprint(
                row_obj
                    .get("compiled_profile_family")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                row_obj
                    .get("compiled_opt_level")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                row_obj
                    .get("compiled_debug")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            );
            let valid_sha256 = is_lower_hex_sha256(binary_sha256);
            let source_commit = row_obj
                .get("source_commit")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let source_dirty = row_obj
                .get("source_dirty")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let expected_config_hash =
                perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
                    source_commit,
                    source_dirty,
                    build_profile: row_obj
                        .get("build_profile")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    executable_build_profile: row_obj
                        .get("executable_build_profile")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    verification: perf_build::BenchmarkBuildVerification {
                        executable_profile: row_obj
                            .get("executable_profile_verified")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        build_fingerprint: row_obj
                            .get("build_fingerprint_verified")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        build_profile: row_obj
                            .get("build_profile_verified")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                    build_fingerprint_contract: row_obj
                        .get("build_fingerprint_contract")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    compiled_profile_family: row_obj
                        .get("compiled_profile_family")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    compiled_opt_level: row_obj
                        .get("compiled_opt_level")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    compiled_debug: row_obj
                        .get("compiled_debug")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    compiled_features: &compiled_features,
                    binary_path,
                    binary_sha256,
                    debug_assertions: row_obj
                        .get("debug_assertions")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                });
            let exact_eligible_contract = row_obj
                .get("declared_eligible_for_regression_gate")
                .and_then(Value::as_bool)
                == Some(true)
                && row_obj.get("evidence_class").and_then(Value::as_str)
                    == Some(EVIDENCE_CLASS_MEASURED)
                && row_obj.get("source_confidence").and_then(Value::as_str)
                    == Some(CONFIDENCE_HIGH)
                && row_obj.get("measurement_method").and_then(Value::as_str)
                    == Some("wall_clock_observation")
                && row_obj.get("measurement_boundary").and_then(Value::as_str)
                    == Some(MEASUREMENT_BOUNDARY)
                && row_obj
                    .get("measurement_contract_version")
                    .and_then(Value::as_str)
                    == Some(MEASUREMENT_CONTRACT_VERSION)
                && row_obj.get("build_profile").and_then(Value::as_str) == Some("perf")
                && row_obj
                    .get("executable_build_profile")
                    .and_then(Value::as_str)
                    == Some("perf")
                && perf_build::profile_from_target_path(Path::new(binary_path)).as_deref()
                    == Some("perf")
                && row_obj
                    .get("executable_profile_verified")
                    .and_then(Value::as_bool)
                    == Some(true)
                && row_obj
                    .get("build_fingerprint_verified")
                    .and_then(Value::as_bool)
                    == Some(true)
                && row_obj
                    .get("build_profile_verified")
                    .and_then(Value::as_bool)
                    == Some(true)
                && row_obj
                    .get("build_fingerprint_contract")
                    .and_then(Value::as_str)
                    == Some(perf_build::BUILD_FINGERPRINT_CONTRACT)
                && fingerprint_matches
                && !compiled_features.is_empty()
                && compiled_features.windows(2).all(|pair| pair[0] < pair[1])
                && row_obj.get("debug_assertions").and_then(Value::as_bool) == Some(false)
                && is_full_git_sha(source_commit)
                && !source_dirty
                && row_obj.get("config_hash").and_then(Value::as_str)
                    == Some(expected_config_hash.as_str())
                && row_obj.get("disk_cache_policy").and_then(Value::as_str) == Some("disabled")
                && row_obj
                    .get("host_page_cache_policy")
                    .and_then(Value::as_str)
                    == Some("not_applicable_measured_region")
                && valid_sha256;
            if !exact_eligible_contract {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} claims effective eligibility without the exact measured build/cache contract"
                )));
            }
        }
        let Some(weights) = row_obj.get("weights").and_then(Value::as_object) else {
            return Err(Error::extension(format!(
                "scenario_breakdown row {idx} field weights must be an object"
            )));
        };
        for field in ["marshal", "queue", "schedule", "policy", "execute", "io"] {
            let Some(weight) = weights.get(field).and_then(Value::as_f64) else {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} weight {field} must be a number"
                )));
            };
            if !weight.is_finite() {
                return Err(Error::extension(format!(
                    "scenario_breakdown row {idx} weight {field} must be finite"
                )));
            }
        }
    }

    let observed_counts = [
        ("source_record_count", scenario_breakdown.len() as u64),
        (
            "declared_eligible_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("declared_eligible_for_regression_gate")
                        .and_then(Value::as_bool)
                        == Some(true)
                })
                .count() as u64,
        ),
        (
            "effective_eligible_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("effective_eligible_for_regression_gate")
                        .and_then(Value::as_bool)
                        == Some(true)
                })
                .count() as u64,
        ),
        (
            "ineligible_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("effective_eligible_for_regression_gate")
                        .and_then(Value::as_bool)
                        != Some(true)
                })
                .count() as u64,
        ),
        (
            "dirty_source_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| row.get("source_dirty").and_then(Value::as_bool) == Some(true))
                .count() as u64,
        ),
        (
            "debug_assertion_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| row.get("debug_assertions").and_then(Value::as_bool) == Some(true))
                .count() as u64,
        ),
        (
            "unverified_executable_profile_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("executable_profile_verified")
                        .and_then(Value::as_bool)
                        != Some(true)
                })
                .count() as u64,
        ),
        (
            "unverified_build_fingerprint_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("build_fingerprint_verified")
                        .and_then(Value::as_bool)
                        != Some(true)
                })
                .count() as u64,
        ),
        (
            "unverified_profile_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("build_profile_verified").and_then(Value::as_bool) != Some(true)
                })
                .count() as u64,
        ),
        (
            "uncontrolled_host_cache_record_count",
            scenario_breakdown
                .iter()
                .filter(|row| {
                    row.get("host_page_cache_policy").and_then(Value::as_str)
                        == Some("uncontrolled")
                })
                .count() as u64,
        ),
    ];
    for (field, observed) in observed_counts {
        if evidence_provenance.get(field).and_then(Value::as_u64) != Some(observed) {
            return Err(Error::extension(format!(
                "evidence_provenance.{field} does not match scenario_breakdown"
            )));
        }
    }
    let observed_total_samples = scenario_breakdown.iter().fold(0_u64, |total, row| {
        total.saturating_add(row.get("samples").and_then(Value::as_u64).unwrap_or(0))
    });
    let observed_eligible_samples = scenario_breakdown
        .iter()
        .filter(|row| {
            row.get("effective_eligible_for_regression_gate")
                .and_then(Value::as_bool)
                == Some(true)
        })
        .fold(0_u64, |total, row| {
            total.saturating_add(row.get("samples").and_then(Value::as_u64).unwrap_or(0))
        });
    for (field, observed) in [
        ("total_sample_count", observed_total_samples),
        ("eligible_sample_count", observed_eligible_samples),
    ] {
        if evidence_provenance.get(field).and_then(Value::as_u64) != Some(observed) {
            return Err(Error::extension(format!(
                "evidence_provenance.{field} does not match scenario_breakdown"
            )));
        }
    }

    let Some(hotspots) = matrix.get("hotspot_matrix").and_then(Value::as_array) else {
        return Err(Error::extension(
            "hotspot_matrix must be an array".to_string(),
        ));
    };
    if hotspots.len() != STAGE_DECOMPOSITION.len() {
        return Err(Error::extension(format!(
            "hotspot_matrix must contain {} entries for complete stage decomposition",
            STAGE_DECOMPOSITION.len()
        )));
    }
    let mut seen_stages = BTreeSet::new();
    for (idx, entry) in hotspots.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} must be an object"
            )));
        };
        for field in [
            "stage",
            "ev_score",
            "confidence",
            "pmu_multiplier",
            "pmu_budget_evaluation",
            "projected_user_impact",
            "recommended_action",
            "downstream_beads",
        ] {
            if entry_obj.get(field).is_none() {
                return Err(Error::extension(format!(
                    "hotspot entry {idx} missing field {field}"
                )));
            }
        }
        let Some(stage) = entry_obj.get("stage").and_then(Value::as_str) else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field stage must be a string"
            )));
        };
        if !STAGE_DECOMPOSITION.contains(&stage) {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field stage must be one of {STAGE_DECOMPOSITION:?}, got {stage}"
            )));
        }
        if !seen_stages.insert(stage.to_string()) {
            return Err(Error::extension(format!(
                "hotspot_matrix contains duplicate stage entry: {stage}"
            )));
        }
        let Some(ev_score) = entry_obj.get("ev_score").and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field ev_score must be a number"
            )));
        };
        if !ev_score.is_finite() || ev_score < 0.0 {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field ev_score must be finite and non-negative"
            )));
        }
        let Some(confidence) = entry_obj.get("confidence").and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field confidence must be a number"
            )));
        };
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field confidence must be finite and within [0, 1]"
            )));
        }
        if confidence > confidence_cap {
            return Err(Error::extension(format!(
                "hotspot entry {idx} confidence {confidence} exceeds provenance cap {confidence_cap}"
            )));
        }
        let Some(pmu_multiplier) = entry_obj.get("pmu_multiplier").and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field pmu_multiplier must be a number"
            )));
        };
        if !pmu_multiplier.is_finite() || pmu_multiplier <= 0.0 {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field pmu_multiplier must be finite and positive"
            )));
        }
        let Some(downstream_beads) = entry_obj.get("downstream_beads").and_then(Value::as_array)
        else {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field downstream_beads must be an array"
            )));
        };
        if downstream_beads.is_empty() {
            return Err(Error::extension(format!(
                "hotspot entry {idx} field downstream_beads must not be empty"
            )));
        }
        for (bead_idx, bead) in downstream_beads.iter().enumerate() {
            let Some(bead_id) = bead.as_str() else {
                return Err(Error::extension(format!(
                    "hotspot entry {idx} downstream_beads[{bead_idx}] must be a string"
                )));
            };
            if bead_id.trim().is_empty() {
                return Err(Error::extension(format!(
                    "hotspot entry {idx} downstream_beads[{bead_idx}] must be non-empty"
                )));
            }
        }
    }
    if seen_stages.len() != STAGE_DECOMPOSITION.len() {
        let missing = STAGE_DECOMPOSITION
            .iter()
            .filter(|stage| !seen_stages.contains(**stage))
            .copied()
            .collect::<Vec<_>>();
        return Err(Error::extension(format!(
            "hotspot_matrix missing stage entries for {missing:?}"
        )));
    }

    let Some(interference_matrix) = matrix.get("interference_matrix").and_then(Value::as_array)
    else {
        return Err(Error::extension(
            "interference_matrix must be an array".to_string(),
        ));
    };
    if interference_matrix.is_empty() {
        return Err(Error::extension(
            "interference_matrix must contain at least one pair entry".to_string(),
        ));
    }

    let mut observed_pair_keys = Vec::with_capacity(interference_matrix.len());
    let mut share_pct_total = 0.0;
    for (idx, entry) in interference_matrix.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            return Err(Error::extension(format!(
                "interference entry {idx} must be an object"
            )));
        };
        for field in [
            "pair",
            "stages",
            "interaction_us",
            "share_pct",
            "sample_count",
            "recommended_probe",
        ] {
            if entry_obj.get(field).is_none() {
                return Err(Error::extension(format!(
                    "interference entry {idx} missing field: {field}"
                )));
            }
        }

        let Some(pair_key) = entry_obj.get("pair").and_then(Value::as_str) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field pair must be a string"
            )));
        };
        let Some((left_stage, right_stage)) = parse_interference_pair_key(pair_key) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field pair must parse as '<stage>+<stage>'"
            )));
        };
        let Some(canonical_pair) = format_interference_pair_key(&left_stage, &right_stage) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field pair must canonicalize"
            )));
        };
        if canonical_pair != pair_key {
            return Err(Error::extension(format!(
                "interference entry {idx} pair must be canonicalized, expected {canonical_pair}, got {pair_key}"
            )));
        }

        let Some(stages) = entry_obj.get("stages").and_then(Value::as_array) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field stages must be an array"
            )));
        };
        if stages.len() != 2 {
            return Err(Error::extension(format!(
                "interference entry {idx} field stages must contain exactly 2 stage names"
            )));
        }
        let Some(stages_left) = stages.first().and_then(Value::as_str) else {
            return Err(Error::extension(format!(
                "interference entry {idx} stages[0] must be a string"
            )));
        };
        let Some(stages_right) = stages.get(1).and_then(Value::as_str) else {
            return Err(Error::extension(format!(
                "interference entry {idx} stages[1] must be a string"
            )));
        };
        if !(STAGE_DECOMPOSITION.contains(&stages_left)
            && STAGE_DECOMPOSITION.contains(&stages_right))
        {
            return Err(Error::extension(format!(
                "interference entry {idx} field stages must be members of {STAGE_DECOMPOSITION:?}"
            )));
        }
        if stages_left != left_stage || stages_right != right_stage {
            return Err(Error::extension(format!(
                "interference entry {idx} stages must match parsed pair ordering ({left_stage}, {right_stage})"
            )));
        }

        let Some(interaction_us) = entry_obj.get("interaction_us").and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field interaction_us must be a number"
            )));
        };
        if !interaction_us.is_finite() || interaction_us < 0.0 {
            return Err(Error::extension(format!(
                "interference entry {idx} field interaction_us must be finite and non-negative"
            )));
        }

        let Some(share_pct) = entry_obj.get("share_pct").and_then(Value::as_f64) else {
            return Err(Error::extension(format!(
                "interference entry {idx} field share_pct must be a number"
            )));
        };
        if !share_pct.is_finite() || share_pct < 0.0 {
            return Err(Error::extension(format!(
                "interference entry {idx} field share_pct must be finite and non-negative"
            )));
        }
        share_pct_total += share_pct;

        if entry_obj
            .get("sample_count")
            .and_then(Value::as_u64)
            .is_none()
        {
            return Err(Error::extension(format!(
                "interference entry {idx} field sample_count must be an integer"
            )));
        }
        let Some(recommended_probe) = entry_obj.get("recommended_probe").and_then(Value::as_str)
        else {
            return Err(Error::extension(format!(
                "interference entry {idx} field recommended_probe must be a string"
            )));
        };
        if recommended_probe.trim().is_empty() {
            return Err(Error::extension(format!(
                "interference entry {idx} field recommended_probe must be non-empty"
            )));
        }

        observed_pair_keys.push(pair_key.to_string());
    }
    if (share_pct_total - 100.0).abs() > 0.5 {
        return Err(Error::extension(format!(
            "interference_matrix share_pct values must sum to ~100, got {share_pct_total:.3}"
        )));
    }

    let levers = STAGE_DECOMPOSITION
        .iter()
        .map(|stage| (*stage).to_string())
        .collect::<Vec<_>>();
    let computed_completeness =
        evaluate_interference_matrix_completeness(&levers, &observed_pair_keys);
    if !computed_completeness.complete {
        return Err(Error::extension(format!(
            "interference_matrix completeness failed: missing={:?} duplicates={:?} unknown={:?}",
            computed_completeness.missing_pairs,
            computed_completeness.duplicate_pairs,
            computed_completeness.unknown_pairs
        )));
    }

    let Some(reported_completeness_raw) = matrix.get("interference_matrix_completeness") else {
        return Err(Error::extension(
            "interference_matrix_completeness must be present".to_string(),
        ));
    };
    let reported_completeness: InterferenceMatrixCompletenessReport =
        serde_json::from_value(reported_completeness_raw.clone()).map_err(|err| {
            Error::extension(format!(
                "interference_matrix_completeness must match expected schema: {err}"
            ))
        })?;
    if reported_completeness != computed_completeness {
        return Err(Error::extension(format!(
            "interference_matrix_completeness mismatch: expected {computed_completeness:?}, got {reported_completeness:?}"
        )));
    }

    let Some(voi) = matrix.get("voi_scheduler").and_then(Value::as_object) else {
        return Err(Error::extension(
            "voi_scheduler must be an object".to_string(),
        ));
    };
    if voi.get("schema").and_then(Value::as_str) != Some(VOI_SCHEDULER_SCHEMA) {
        return Err(Error::extension(format!(
            "unexpected VOI scheduler schema: {:?}",
            voi.get("schema")
        )));
    }
    for field in [
        "status",
        "budget",
        "candidates",
        "selected_plan",
        "realized_information_gain",
    ] {
        if voi.get(field).is_none() {
            return Err(Error::extension(format!(
                "voi_scheduler missing field: {field}"
            )));
        }
    }
    let Some(voi_budget) = voi.get("budget").and_then(Value::as_object) else {
        return Err(Error::extension(
            "voi_scheduler.budget must be an object".to_string(),
        ));
    };
    for field in [
        "max_overhead_ms",
        "max_experiments",
        "used_overhead_ms",
        "remaining_overhead_ms",
        "feasible",
    ] {
        if voi_budget.get(field).is_none() {
            return Err(Error::extension(format!(
                "voi_scheduler budget missing field: {field}"
            )));
        }
    }

    let Some(voi_candidates) = voi.get("candidates").and_then(Value::as_array) else {
        return Err(Error::extension(
            "voi_scheduler.candidates must be an array".to_string(),
        ));
    };
    for (idx, row) in voi_candidates.iter().enumerate() {
        let Some(row_obj) = row.as_object() else {
            return Err(Error::extension(format!(
                "voi_scheduler candidate {idx} must be an object"
            )));
        };
        for field in [
            "stage",
            "rank",
            "selected",
            "skip_reason",
            "cost_ms",
            "voi_score",
            "expected_information_gain",
            "recommended_probe",
            "utility",
        ] {
            if row_obj.get(field).is_none() {
                return Err(Error::extension(format!(
                    "voi_scheduler candidate {idx} missing field: {field}"
                )));
            }
        }
        let Some(utility) = row_obj.get("utility").and_then(Value::as_object) else {
            return Err(Error::extension(format!(
                "voi_scheduler candidate {idx} utility must be an object"
            )));
        };
        for field in [
            "uncertainty_reduction",
            "user_impact_score",
            "pressure_bonus",
            "total",
        ] {
            if utility.get(field).is_none() {
                return Err(Error::extension(format!(
                    "voi_scheduler candidate {idx} utility missing field: {field}"
                )));
            }
        }
    }

    let Some(selected_plan) = voi.get("selected_plan").and_then(Value::as_array) else {
        return Err(Error::extension(
            "voi_scheduler.selected_plan must be an array".to_string(),
        ));
    };
    for (idx, row) in selected_plan.iter().enumerate() {
        let Some(row_obj) = row.as_object() else {
            return Err(Error::extension(format!(
                "voi_scheduler selected_plan {idx} must be an object"
            )));
        };
        for field in [
            "stage",
            "rank",
            "cost_ms",
            "expected_information_gain",
            "recommended_probe",
        ] {
            if row_obj.get(field).is_none() {
                return Err(Error::extension(format!(
                    "voi_scheduler selected_plan {idx} missing field: {field}"
                )));
            }
        }
    }

    let Some(realized_gain) = voi
        .get("realized_information_gain")
        .and_then(Value::as_object)
    else {
        return Err(Error::extension(
            "voi_scheduler.realized_information_gain must be an object".to_string(),
        ));
    };
    for field in [
        "expected_total",
        "realized_total",
        "correlation_strength",
        "correlation_multiplier",
        "regression_penalty",
    ] {
        if realized_gain.get(field).is_none() {
            return Err(Error::extension(format!(
                "voi_scheduler realized_information_gain missing field: {field}"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SOURCE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const TEST_COMPILED_FEATURES: &[&str] = &["sqlite-sessions"];

    fn test_provenance_config_hash(binary_path: &str, binary_sha256: &str) -> String {
        perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
            source_commit: TEST_SOURCE_COMMIT,
            source_dirty: false,
            build_profile: "perf",
            executable_build_profile: "perf",
            verification: perf_build::BenchmarkBuildVerification {
                executable_profile: true,
                build_fingerprint: true,
                build_profile: true,
            },
            build_fingerprint_contract: perf_build::BUILD_FINGERPRINT_CONTRACT,
            compiled_profile_family: "release",
            compiled_opt_level: "3",
            compiled_debug: "true",
            compiled_features: TEST_COMPILED_FEATURES,
            binary_path,
            binary_sha256,
            debug_assertions: false,
        })
    }

    #[test]
    fn workload_args_reject_zero_measured_work() {
        assert!(
            Args::try_parse_from(["ext_workloads", "--load-runs", "0"]).is_err(),
            "zero cold-load runs must not produce benchmark evidence"
        );
        assert!(
            Args::try_parse_from(["ext_workloads", "--iterations", "0"]).is_err(),
            "zero dispatch iterations must not produce benchmark evidence"
        );
        assert!(
            Args::try_parse_from(["ext_workloads", "--long-session-iterations", "0"]).is_err(),
            "zero long-session iterations must not produce benchmark evidence"
        );
    }

    #[test]
    fn workload_args_preserve_canonical_nonzero_defaults() {
        let args = Args::try_parse_from(["ext_workloads"]).expect("parse default workload args");
        assert_eq!(args.load_runs.get(), 3);
        assert_eq!(args.iterations.get(), 2_000);
        assert_eq!(args.long_session_iterations.get(), 8_000);
    }

    #[test]
    fn regression_evidence_requires_perf_and_controlled_measurement_region() {
        assert_eq!(
            classify_regression_evidence(RegressionEligibility {
                build_profile: "perf",
                build_profile_verified: true,
                provenance_identity_verified: true,
                optimized_binary: true,
                measurement_region: MeasurementRegion::Controlled,
            }),
            RegressionEvidence {
                confidence: CONFIDENCE_HIGH,
                eligible: true,
            }
        );
        for evidence in [
            classify_regression_evidence(RegressionEligibility {
                build_profile: "release",
                build_profile_verified: true,
                provenance_identity_verified: true,
                optimized_binary: true,
                measurement_region: MeasurementRegion::Controlled,
            }),
            classify_regression_evidence(RegressionEligibility {
                build_profile: "perf",
                build_profile_verified: false,
                provenance_identity_verified: true,
                optimized_binary: true,
                measurement_region: MeasurementRegion::Controlled,
            }),
            classify_regression_evidence(RegressionEligibility {
                build_profile: "perf",
                build_profile_verified: true,
                provenance_identity_verified: false,
                optimized_binary: true,
                measurement_region: MeasurementRegion::Controlled,
            }),
            classify_regression_evidence(RegressionEligibility {
                build_profile: "perf",
                build_profile_verified: true,
                provenance_identity_verified: true,
                optimized_binary: false,
                measurement_region: MeasurementRegion::Controlled,
            }),
            classify_regression_evidence(RegressionEligibility {
                build_profile: "perf",
                build_profile_verified: true,
                provenance_identity_verified: true,
                optimized_binary: true,
                measurement_region: MeasurementRegion::Load,
            }),
        ] {
            assert_eq!(evidence.confidence, CONFIDENCE_MEDIUM);
            assert!(!evidence.eligible);
        }
    }

    #[test]
    fn trace_event_preserves_full_evidence_contract() {
        let binary_path = "/tmp/repo/target/perf/examples/ext_workloads";
        let binary_sha256 = "a".repeat(64);
        let config_hash = test_provenance_config_hash(binary_path, &binary_sha256);
        let event = trace_event_for_record(
            "scenario.complete",
            &json!({
                "scenario": "ext_load_init/load_init_cold",
                "extension": "hello",
                "runs": 3,
                "summary": {
                    "count": 3,
                    "p50_ms": 1.25,
                    "p95_ms": 1.75,
                },
                "evidence_class": EVIDENCE_CLASS_MEASURED,
                "confidence": CONFIDENCE_MEDIUM,
                "eligible_for_regression_gate": false,
                "measurement_method": "wall_clock_observation",
                "measurement_boundary": MEASUREMENT_BOUNDARY,
                "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
                "disk_cache_policy": "disabled",
                "host_page_cache_policy": "uncontrolled",
                "build_profile": "perf",
                "executable_build_profile": "perf",
                "executable_profile_verified": true,
                "build_fingerprint_verified": true,
                "build_profile_verified": true,
                "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
                "compiled_profile_family": "release",
                "compiled_opt_level": "3",
                "compiled_debug": "true",
                "compiled_features": TEST_COMPILED_FEATURES,
                "binary_path": binary_path,
                "binary_sha256": binary_sha256,
                "debug_assertions": false,
                "source_commit": TEST_SOURCE_COMMIT,
                "source_dirty": false,
                "config_hash": config_hash,
            }),
        );
        let details = event.get("details").expect("trace event details");
        assert_eq!(details["evidence_class"], EVIDENCE_CLASS_MEASURED);
        assert_eq!(details["confidence"], CONFIDENCE_MEDIUM);
        assert_eq!(details["eligible_for_regression_gate"], false);
        assert_eq!(details["measurement_method"], "wall_clock_observation");
        assert_eq!(details["measurement_boundary"], MEASUREMENT_BOUNDARY);
        assert_eq!(
            details["measurement_contract_version"],
            MEASUREMENT_CONTRACT_VERSION
        );
        assert_eq!(details["disk_cache_policy"], "disabled");
        assert_eq!(details["host_page_cache_policy"], "uncontrolled");
        assert_eq!(details["summary"]["count"], 3);
        assert_eq!(details["summary"]["p95_ms"], 1.75);
        assert_eq!(details["build_profile"], "perf");
        assert_eq!(details["executable_build_profile"], "perf");
        assert_eq!(details["executable_profile_verified"], true);
        assert_eq!(details["build_fingerprint_verified"], true);
        assert_eq!(details["build_profile_verified"], true);
        assert_eq!(
            details["build_fingerprint_contract"],
            perf_build::BUILD_FINGERPRINT_CONTRACT
        );
        assert_eq!(details["compiled_profile_family"], "release");
        assert_eq!(details["compiled_opt_level"], "3");
        assert_eq!(details["compiled_debug"], "true");
        assert_eq!(details["compiled_features"], json!(TEST_COMPILED_FEATURES));
        assert_eq!(
            details["binary_path"],
            "/tmp/repo/target/perf/examples/ext_workloads"
        );
        assert_eq!(details["binary_sha256"], "a".repeat(64));
        assert_eq!(details["debug_assertions"], false);
        assert_eq!(details["source_commit"], TEST_SOURCE_COMMIT);
        assert_eq!(details["source_dirty"], false);
        assert_eq!(
            details["config_hash"],
            test_provenance_config_hash(binary_path, &"a".repeat(64))
        );
    }

    #[test]
    fn parse_pmu_counters_normalizes_ratio_inputs() {
        let raw = json!({
            "counters": {
                "frontend_stall_ratio": 0.22,
                "backend_stall_ratio": 0.31,
                "llc_miss_rate": 0.18,
                "branch_miss_rate_pct": 4.5,
                "cycles_per_call": 12345.0
            }
        });
        let parsed = parse_pmu_counters(&raw).expect("pmu counters should parse");
        assert_eq!(parsed.frontend_stall_pct, Some(22.0));
        assert_eq!(parsed.backend_stall_pct, Some(31.0));
        assert_eq!(parsed.llc_miss_pct, Some(18.0));
        assert_eq!(parsed.branch_miss_pct, Some(4.5));
        assert_eq!(parsed.cycles_per_call, Some(12_345.0));
    }

    #[test]
    fn pmu_budget_evaluation_fails_when_thresholds_exceeded() {
        let counters = PmuCountersNormalized {
            frontend_stall_pct: Some(40.0),
            backend_stall_pct: Some(35.0),
            llc_miss_pct: Some(25.0),
            branch_miss_pct: Some(8.0),
            cycles_per_call: Some(20_000.0),
        };
        let budget_eval = evaluate_pmu_budget(&counters, PmuBudgetSpec::default());
        assert_eq!(
            budget_eval.get("status").and_then(Value::as_str),
            Some("fail")
        );
        assert_eq!(
            budget_eval
                .get("checks")
                .and_then(|v| v.get("llc_miss_pct"))
                .and_then(|v| v.get("ok"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn compare_pmu_profiles_flags_regressions() {
        let candidate = json!({
            "status": "collected",
            "normalized_counters": {
                "frontend_stall_pct": 40.0,
                "backend_stall_pct": 34.0,
                "llc_miss_pct": 22.0,
                "branch_miss_pct": 7.5,
                "cycles_per_call": 19500.0
            },
            "budget": {
                "llc_miss_budget_pct": 18.0,
                "branch_miss_budget_pct": 6.0,
                "stall_total_budget_pct": 65.0
            }
        });
        let baseline = json!({
            "status": "collected",
            "normalized_counters": {
                "frontend_stall_pct": 31.0,
                "backend_stall_pct": 27.0,
                "llc_miss_pct": 15.0,
                "branch_miss_pct": 4.0,
                "cycles_per_call": 15000.0
            },
            "budget": {
                "llc_miss_budget_pct": 18.0,
                "branch_miss_budget_pct": 6.0,
                "stall_total_budget_pct": 65.0
            }
        });

        let comparison = compare_pmu_profiles(&candidate, &baseline);
        assert_eq!(
            comparison.get("status").and_then(Value::as_str),
            Some("compared")
        );
        assert_eq!(
            comparison
                .get("regressions")
                .and_then(|value| value.get("overall"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn compare_perf_outcomes_detects_latency_regression() {
        let baseline = PerfOutcomeSnapshot {
            p50_us: Some(95.0),
            p95_us: Some(120.0),
            p99_us: Some(160.0),
            throughput_eps: Some(1800.0),
            rss_mb: Some(180.0),
            cpu_pct: Some(70.0),
            io_wait_pct: Some(2.0),
        };
        let candidate = PerfOutcomeSnapshot {
            p50_us: Some(100.0),
            p95_us: Some(145.0),
            p99_us: Some(190.0),
            throughput_eps: Some(1720.0),
            rss_mb: Some(188.0),
            cpu_pct: Some(74.0),
            io_wait_pct: Some(2.6),
        };

        let comparison = compare_perf_outcomes(Some(baseline), candidate);
        assert_eq!(
            comparison.get("status").and_then(Value::as_str),
            Some("compared")
        );
        assert_eq!(
            comparison
                .get("regressions")
                .and_then(|value| value.get("latency"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            comparison
                .get("regressions")
                .and_then(|value| value.get("overall"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn pmu_outcome_correlation_high_when_signals_align() {
        let pmu_comparison = json!({
            "status": "compared",
            "delta_pct_points": {
                "stall_total_pct": 8.0,
                "llc_miss_pct": 3.0,
                "branch_miss_pct": 1.2
            }
        });
        let candidate_outcomes = PerfOutcomeSnapshot {
            p50_us: Some(100.0),
            p95_us: Some(145.0),
            p99_us: Some(195.0),
            throughput_eps: Some(1700.0),
            rss_mb: None,
            cpu_pct: None,
            io_wait_pct: None,
        };
        let outcome_comparison = json!({
            "status": "compared",
            "regressions": {
                "overall": true
            }
        });

        let correlation =
            build_pmu_outcome_correlation(&pmu_comparison, candidate_outcomes, &outcome_comparison);
        assert_eq!(
            correlation
                .get("correlation_strength")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn stage_weights_are_normalized() {
        for scenario in [
            "ext_load_init/load_init_cold",
            "ext_tool_call/hello",
            "ext_event_hook/before_agent_start",
            "ext_hostcall_bridge/long_session_real_corpus",
            "unknown",
        ] {
            let sum = stage_weights_for_scenario(scenario).sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "weights must sum to 1.0 for {scenario}, got {sum}"
            );
        }
    }

    #[test]
    fn parse_profile_record_from_per_call_fields() {
        let record = json!({
            "schema": BENCH_SCHEMA,
            "scenario": "ext_tool_call/hello",
            "extension": "hello",
            "iterations": 200,
            "per_call_us": 140.0,
        });
        let parsed = parse_profile_record(&record).expect("parse profile record");
        assert_eq!(parsed.samples, 200);
        assert!((parsed.total_us - 28_000.0).abs() < f64::EPSILON);
        assert!((parsed.per_call_us - 140.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_profile_record_from_summary_fields() {
        let record = json!({
            "schema": BENCH_SCHEMA,
            "scenario": "ext_load_init/load_init_cold",
            "extension": "pirate",
            "runs": 4,
            "summary": {
                "count": 4,
                "p95_ms": 3.5
            }
        });
        let parsed = parse_profile_record(&record).expect("parse profile summary");
        assert_eq!(parsed.samples, 4);
        assert!((parsed.per_call_us - 3500.0).abs() < 0.001);
        assert!((parsed.total_us - 14_000.0).abs() < 0.001);
    }

    #[test]
    fn profile_record_eligibility_rejects_forged_contract_and_build_claims() {
        assert!(is_full_git_sha(TEST_SOURCE_COMMIT));
        assert!(!is_full_git_sha("0123456789ABCDEF0123456789ABCDEF01234567"));
        assert!(!is_full_git_sha("0000000000000000000000000000000000000000"));
        assert!(is_lower_hex_sha256(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_lower_hex_sha256(
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
        ));
        assert!(!is_lower_hex_sha256(&"0".repeat(64)));

        let binary_path = "/tmp/repo/target/perf/examples/ext_workloads";
        let binary_sha256 = "b".repeat(64);
        let env = json!({
            "build_profile": "perf",
            "debug_assertions": false,
            "executable_build_profile": "perf",
            "executable_profile_verified": true,
            "build_fingerprint_verified": true,
            "build_profile_verified": true,
            "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
            "compiled_profile_family": "release",
            "compiled_opt_level": "3",
            "compiled_debug": "true",
            "compiled_features": TEST_COMPILED_FEATURES,
            "binary_path": binary_path,
            "binary_sha256": binary_sha256,
            "source_commit": TEST_SOURCE_COMMIT,
            "source_dirty": false,
            "config_hash": test_provenance_config_hash(binary_path, &"b".repeat(64)),
        });
        let eligible_record = attach_env(
            json!({
                "schema": BENCH_SCHEMA,
                "scenario": "ext_tool_call/hello",
                "extension": "hello",
                "iterations": 200,
                "per_call_us": 140.0,
                "evidence_class": EVIDENCE_CLASS_MEASURED,
                "confidence": CONFIDENCE_HIGH,
                "eligible_for_regression_gate": true,
                "measurement_method": "wall_clock_observation",
                "measurement_boundary": MEASUREMENT_BOUNDARY,
                "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
                "disk_cache_policy": "disabled",
                "host_page_cache_policy": "not_applicable_measured_region",
            }),
            &env,
        );
        assert!(
            parse_profile_record(&eligible_record)
                .is_some_and(|record| record.effective_gate_eligible),
            "the exact canonical provenance contract should remain eligible"
        );

        let mut forged_records = Vec::new();
        for (field, value) in [
            ("measurement_boundary", json!("production_fake")),
            ("measurement_contract_version", json!("bogus")),
            ("disk_cache_policy", json!("bogus")),
            ("host_page_cache_policy", json!("bogus")),
        ] {
            let mut forged = eligible_record.clone();
            forged[field] = value;
            forged_records.push(forged);
        }
        for (pointer, value) in [
            ("/env/compiled_opt_level", json!("0")),
            (
                "/env/binary_path",
                json!("/tmp/repo/target/debug/examples/ext_workloads"),
            ),
            ("/env/executable_build_profile", json!("debug")),
            ("/env/binary_sha256", json!("unknown")),
            ("/env/source_commit", json!("deadbeef")),
            ("/env/source_dirty", json!(true)),
            ("/env/compiled_features", json!(["z", "a"])),
            ("/env/config_hash", json!("0".repeat(64))),
        ] {
            let mut forged = eligible_record.clone();
            *forged
                .pointer_mut(pointer)
                .expect("eligible provenance fixture field") = value;
            forged_records.push(forged);
        }

        for forged in forged_records {
            let parsed = parse_profile_record(&forged).expect("parse forged profile record");
            assert!(
                !parsed.effective_gate_eligible,
                "self-declared verification must not bypass exact provenance checks: {forged}"
            );
        }
    }

    fn hotspot_matrix_schema_fixture() -> Value {
        let records = vec![
            json!({
                "schema": BENCH_SCHEMA,
                "scenario": "ext_tool_call/hello",
                "extension": "hello",
                "iterations": 1000,
                "per_call_us": 120.0,
            }),
            json!({
                "schema": BENCH_SCHEMA,
                "scenario": "ext_hostcall_bridge/long_session_real_corpus",
                "extension": "real_corpus_4ext",
                "iterations": 5000,
                "per_call_us": 180.0,
            }),
        ];
        build_hotspot_matrix(
            &records,
            &json!({ "run_id": "test-run" }),
            &json!({ "schema": TRACE_EVENT_SCHEMA }),
            &json!({
                "status": "collected",
                "normalized_counters": {
                    "frontend_stall_pct": 38.0,
                    "backend_stall_pct": 34.0,
                    "llc_miss_pct": 21.0,
                    "branch_miss_pct": 7.0,
                    "cycles_per_call": 18000.0
                },
                "budget": {
                    "llc_miss_budget_pct": 18.0,
                    "branch_miss_budget_pct": 6.0,
                    "stall_total_budget_pct": 65.0
                }
            }),
            &json!({
                "status": "collected",
                "normalized_counters": {
                    "frontend_stall_pct": 30.0,
                    "backend_stall_pct": 27.0,
                    "llc_miss_pct": 14.0,
                    "branch_miss_pct": 4.5,
                    "cycles_per_call": 15500.0
                },
                "budget": {
                    "llc_miss_budget_pct": 18.0,
                    "branch_miss_budget_pct": 6.0,
                    "stall_total_budget_pct": 65.0
                }
            }),
            &json!({ "status": "not_collected" }),
        )
    }

    #[test]
    fn hotspot_matrix_includes_ev_confidence_and_user_impact() {
        let matrix = hotspot_matrix_schema_fixture();
        validate_hotspot_matrix_schema(&matrix).expect("schema should validate");
        let hotspots = matrix["hotspot_matrix"]
            .as_array()
            .expect("hotspot_matrix array");
        assert!(!hotspots.is_empty(), "hotspot matrix should not be empty");
        let top = &hotspots[0];
        assert!(top.get("ev_score").is_some(), "missing ev_score");
        assert!(top.get("confidence").is_some(), "missing confidence");
        assert!(
            top.get("pmu_multiplier")
                .and_then(Value::as_f64)
                .is_some_and(|mult| mult > 1.0),
            "expected PMU multiplier to increase under high pressure"
        );
        assert!(
            top.get("projected_user_impact").is_some(),
            "missing projected_user_impact"
        );
        assert_eq!(
            matrix
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("pmu_comparison"))
                .and_then(|comparison| comparison.get("status"))
                .and_then(Value::as_str),
            Some("compared")
        );
        assert!(
            matrix
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("pmu_outcome_correlation"))
                .and_then(|correlation| correlation.get("correlation_strength"))
                .and_then(Value::as_str)
                .is_some(),
            "missing PMU outcome correlation strength"
        );
        assert_eq!(
            matrix
                .get("voi_scheduler")
                .and_then(|scheduler| scheduler.get("schema"))
                .and_then(Value::as_str),
            Some(VOI_SCHEDULER_SCHEMA),
            "missing VOI scheduler artifact"
        );
        let interference = matrix
            .get("interference_matrix")
            .and_then(Value::as_array)
            .expect("interference_matrix array");
        assert_eq!(
            interference.len(),
            15,
            "expected complete 6C2 interference matrix coverage"
        );
        assert!(
            interference.iter().all(|entry| entry.get("pair").is_some()),
            "every interference entry must include pair key"
        );
        let completeness = matrix
            .get("interference_matrix_completeness")
            .and_then(Value::as_object)
            .expect("interference_matrix_completeness object");
        assert_eq!(
            completeness.get("complete").and_then(Value::as_bool),
            Some(true),
            "interference matrix completeness must be true"
        );
        assert_eq!(
            completeness.get("expectedPairs").and_then(Value::as_u64),
            Some(15),
            "interference matrix expectedPairs must match 6C2"
        );
    }

    #[test]
    fn hotspot_matrix_preserves_ineligible_cache_and_profile_provenance() {
        let records = vec![json!({
            "schema": BENCH_SCHEMA,
            "scenario": "ext_load_init/load_init_cold",
            "extension": "hello",
            "runs": 10_000,
            "summary": {
                "count": 10_000,
                "p95_ms": 1.5,
            },
            "evidence_class": EVIDENCE_CLASS_MEASURED,
            "confidence": CONFIDENCE_HIGH,
            "eligible_for_regression_gate": true,
            "measurement_method": "wall_clock_observation",
            "measurement_boundary": MEASUREMENT_BOUNDARY,
            "measurement_contract_version": MEASUREMENT_CONTRACT_VERSION,
            "disk_cache_policy": "disabled",
            "host_page_cache_policy": "uncontrolled",
            "env": {
                "build_profile": "perf",
                "debug_assertions": false,
                "executable_build_profile": "perf",
                "executable_profile_verified": true,
                "build_fingerprint_verified": true,
                "build_profile_verified": true,
                "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
                "compiled_profile_family": "release",
                "compiled_opt_level": "3",
                "compiled_debug": "true",
                "compiled_features": TEST_COMPILED_FEATURES,
                "binary_path": "/tmp/repo/target/perf/examples/ext_workloads",
                "binary_sha256": "c".repeat(64),
                "source_commit": TEST_SOURCE_COMMIT,
                "source_dirty": false,
                "config_hash": test_provenance_config_hash(
                    "/tmp/repo/target/perf/examples/ext_workloads",
                    &"c".repeat(64),
                ),
            },
        })];
        let matrix = build_hotspot_matrix(
            &records,
            &json!({ "run_id": "provenance-test" }),
            &json!({ "schema": TRACE_EVENT_SCHEMA }),
            &json!({ "status": "not_collected" }),
            &json!({ "status": "not_collected" }),
            &json!({ "status": "not_collected" }),
        );
        validate_hotspot_matrix_schema(&matrix).expect("provenance matrix should validate");

        let provenance = &matrix["evidence_provenance"];
        assert_eq!(provenance["eligible_for_regression_gate"], false);
        assert_eq!(provenance["declared_eligible_record_count"], 1);
        assert_eq!(provenance["effective_eligible_record_count"], 0);
        assert_eq!(provenance["uncontrolled_host_cache_record_count"], 1);
        assert_eq!(provenance["provenance_confidence_cap"], 0.35);

        let source = &matrix["scenario_breakdown"][0];
        assert_eq!(source["host_page_cache_policy"], "uncontrolled");
        assert_eq!(source["build_profile"], "perf");
        assert_eq!(source["executable_build_profile"], "perf");
        assert_eq!(source["executable_profile_verified"], true);
        assert_eq!(source["build_fingerprint_verified"], true);
        assert_eq!(source["build_profile_verified"], true);
        assert_eq!(
            source["build_fingerprint_contract"],
            perf_build::BUILD_FINGERPRINT_CONTRACT
        );
        assert_eq!(source["compiled_profile_family"], "release");
        assert_eq!(source["compiled_opt_level"], "3");
        assert_eq!(source["compiled_debug"], "true");
        assert_eq!(source["compiled_features"], json!(TEST_COMPILED_FEATURES));
        assert_eq!(source["binary_sha256"], "c".repeat(64));
        assert_eq!(source["source_commit"], TEST_SOURCE_COMMIT);
        assert_eq!(source["source_dirty"], false);
        assert_eq!(source["declared_eligible_for_regression_gate"], true);
        assert_eq!(source["effective_eligible_for_regression_gate"], false);

        for hotspot in matrix["hotspot_matrix"]
            .as_array()
            .expect("hotspot entries")
        {
            assert!(
                hotspot["confidence"]
                    .as_f64()
                    .is_some_and(|value| value <= 0.35),
                "uncontrolled-cache evidence must not yield high-confidence attribution: {hotspot}"
            );
        }
    }

    #[test]
    fn hotspot_matrix_schema_rejects_missing_voi_candidate_utility_total() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/voi_scheduler/candidates/0/utility")
            .and_then(Value::as_object_mut)
            .expect("utility object")
            .remove("total");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("voi_scheduler candidate 0 utility missing field: total"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_missing_voi_selected_plan_probe() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/voi_scheduler/selected_plan/0")
            .and_then(Value::as_object_mut)
            .expect("selected_plan[0] object")
            .remove("recommended_probe");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("voi_scheduler selected_plan 0 missing field: recommended_probe"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_missing_scenario_breakdown_extension() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/scenario_breakdown/0")
            .and_then(Value::as_object_mut)
            .expect("scenario_breakdown[0] object")
            .remove("extension");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("scenario_breakdown row 0 missing field: extension"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_non_integer_scenario_breakdown_samples() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/scenario_breakdown/0/samples")
            .expect("scenario_breakdown[0].samples") = json!("1000");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("scenario_breakdown row 0 field samples must be an integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_non_numeric_scenario_breakdown_weight() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/scenario_breakdown/0/weights/schedule")
            .expect("scenario_breakdown[0].weights.schedule") = json!("slow");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("scenario_breakdown row 0 weight schedule must be a number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_missing_stage_totals_field() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/stage_totals_us")
            .and_then(Value::as_object_mut)
            .expect("stage_totals_us object")
            .remove("execute");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("stage_totals_us missing numeric field: execute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_methodology_stage_decomposition_drift() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/methodology/stage_decomposition/2")
            .expect("stage_decomposition[2]") = json!("dispatch");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string().contains(
                "methodology.stage_decomposition index 2 expected schedule, got dispatch"
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_duplicate_hotspot_stage_entries() {
        let mut matrix = hotspot_matrix_schema_fixture();
        let duplicate_stage = matrix
            .pointer("/hotspot_matrix/0/stage")
            .and_then(Value::as_str)
            .expect("hotspot_matrix[0].stage")
            .to_string();
        *matrix
            .pointer_mut("/hotspot_matrix/1/stage")
            .expect("hotspot_matrix[1].stage") = json!(duplicate_stage);

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("hotspot_matrix contains duplicate stage entry"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_incomplete_interference_matrix() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/interference_matrix")
            .and_then(Value::as_array_mut)
            .expect("interference_matrix array")
            .pop();

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        let message = err.to_string();
        assert!(
            message.contains("interference_matrix completeness failed")
                || message.contains("interference_matrix share_pct values must sum to ~100"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_unparseable_interference_pair() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/interference_matrix/0/pair")
            .expect("interference_matrix[0].pair") = json!("queue");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("interference entry 0 field pair must parse"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_noncanonical_interference_pair() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/interference_matrix/0/pair")
            .expect("interference_matrix[0].pair") = json!("queue+marshal");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string().contains("pair must be canonicalized"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_interference_share_pct_drift() {
        let mut matrix = hotspot_matrix_schema_fixture();
        let original_share = matrix
            .pointer("/interference_matrix/0/share_pct")
            .and_then(Value::as_f64)
            .expect("interference_matrix[0].share_pct");
        *matrix
            .pointer_mut("/interference_matrix/0/share_pct")
            .expect("interference_matrix[0].share_pct") = json!(original_share + 5.0);

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("interference_matrix share_pct values must sum to ~100"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_completeness_report_mismatch() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/interference_matrix_completeness/complete")
            .expect("interference_matrix_completeness.complete") = json!(false);

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("interference_matrix_completeness mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_stage_count_drift() {
        let mut matrix = hotspot_matrix_schema_fixture();
        matrix
            .pointer_mut("/hotspot_matrix")
            .and_then(Value::as_array_mut)
            .expect("hotspot_matrix array")
            .pop();

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string()
                .contains("hotspot_matrix must contain 6 entries for complete stage decomposition"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hotspot_matrix_schema_rejects_unknown_hotspot_stage() {
        let mut matrix = hotspot_matrix_schema_fixture();
        *matrix
            .pointer_mut("/hotspot_matrix/0/stage")
            .expect("hotspot_matrix[0].stage") = json!("dispatch");

        let err = validate_hotspot_matrix_schema(&matrix).expect_err("expected schema failure");
        assert!(
            err.to_string().contains("field stage must be one of"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn voi_scheduler_prefers_higher_utility_per_cost() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 90.0,
                "confidence": 0.40,
                "pmu_multiplier": 1.5,
                "projected_user_impact": { "turn_latency_p95_ms": 6.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 25.0,
                "confidence": 0.85,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 0.4 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:50:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 32.0,
                        "backend_stall_pct": 28.0,
                        "llc_miss_pct": 20.0,
                        "branch_miss_pct": 5.5
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": true }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "high",
                    "signals": {
                        "pmu_worse": true,
                        "outcome_regression": true
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": true }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 200.0,
                max_experiments: 3,
                stale_after_hours: 24.0,
            },
        );
        let candidates = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidate rows");
        assert_eq!(
            candidates[0].get("stage").and_then(Value::as_str),
            Some("queue")
        );
        let top_voi = candidates[0]
            .get("voi_score")
            .and_then(Value::as_f64)
            .expect("top voi");
        let second_voi = candidates[1]
            .get("voi_score")
            .and_then(Value::as_f64)
            .expect("second voi");
        assert!(top_voi > second_voi);
    }

    #[test]
    fn voi_scheduler_budget_selection_is_feasible() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 90.0,
                "confidence": 0.30,
                "pmu_multiplier": 1.8,
                "projected_user_impact": { "turn_latency_p95_ms": 8.0 }
            }),
            json!({
                "stage": "schedule",
                "ev_score": 72.0,
                "confidence": 0.35,
                "pmu_multiplier": 1.6,
                "projected_user_impact": { "turn_latency_p95_ms": 5.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 18.0,
                "confidence": 0.80,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 0.8 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 30.0,
                        "backend_stall_pct": 25.0,
                        "llc_miss_pct": 19.0,
                        "branch_miss_pct": 5.0
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "moderate",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 40.0,
                max_experiments: 3,
                stale_after_hours: 24.0,
            },
        );
        let used = plan
            .get("budget")
            .and_then(|budget| budget.get("used_overhead_ms"))
            .and_then(Value::as_f64)
            .expect("used budget");
        let feasible = plan
            .get("budget")
            .and_then(|budget| budget.get("feasible"))
            .and_then(Value::as_bool)
            .expect("feasible");
        assert!(feasible);
        assert!(used <= 40.0 + f64::EPSILON);
    }

    #[test]
    fn voi_scheduler_marks_max_experiments_reached_and_preserves_plan_consistency() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 95.0,
                "confidence": 0.25,
                "pmu_multiplier": 1.2,
                "projected_user_impact": { "turn_latency_p95_ms": 8.0 }
            }),
            json!({
                "stage": "schedule",
                "ev_score": 84.0,
                "confidence": 0.30,
                "pmu_multiplier": 1.1,
                "projected_user_impact": { "turn_latency_p95_ms": 7.0 }
            }),
            json!({
                "stage": "execute",
                "ev_score": 72.0,
                "confidence": 0.35,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 5.5 }
            }),
            json!({
                "stage": "policy",
                "ev_score": 50.0,
                "confidence": 0.45,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 3.5 }
            }),
        ];
        let budget = VoiBudgetConfig {
            max_overhead_ms: 300.0,
            max_experiments: 2,
            stale_after_hours: 24.0,
        };
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 28.0,
                        "backend_stall_pct": 24.0,
                        "llc_miss_pct": 17.0,
                        "branch_miss_pct": 4.8
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "high",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            budget,
        );

        let candidate_rows = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidate rows");
        let selected_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(true))
            .collect::<Vec<_>>();
        let skipped_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(false))
            .collect::<Vec<_>>();
        assert_eq!(
            selected_rows.len(),
            2,
            "max_experiments should cap selection"
        );
        assert!(
            skipped_rows.iter().all(|row| {
                row.get("skip_reason").and_then(Value::as_str) == Some("max_experiments_reached")
            }),
            "all non-selected rows should report max_experiments_reached"
        );

        let selected_plan = plan
            .get("selected_plan")
            .and_then(Value::as_array)
            .expect("selected_plan");
        assert_eq!(
            selected_plan.len(),
            selected_rows.len(),
            "selected_plan length should match selected candidate rows"
        );
        for plan_row in selected_plan {
            let stage = plan_row
                .get("stage")
                .and_then(Value::as_str)
                .expect("selected plan stage");
            let matching_row = selected_rows
                .iter()
                .find(|row| row.get("stage").and_then(Value::as_str) == Some(stage))
                .expect("matching selected candidate");

            assert_eq!(plan_row.get("rank"), matching_row.get("rank"));
            assert_eq!(plan_row.get("cost_ms"), matching_row.get("cost_ms"));
            assert_eq!(
                plan_row.get("expected_information_gain"),
                matching_row.get("expected_information_gain")
            );
            assert_eq!(
                plan_row.get("recommended_probe"),
                matching_row.get("recommended_probe")
            );
        }

        let selected_cost = selected_rows
            .iter()
            .map(|row| {
                row.get("cost_ms")
                    .and_then(Value::as_f64)
                    .expect("selected cost")
            })
            .sum::<f64>();
        let used_overhead = plan
            .get("budget")
            .and_then(|budget_obj| budget_obj.get("used_overhead_ms"))
            .and_then(Value::as_f64)
            .expect("used_overhead_ms");
        let remaining_overhead = plan
            .get("budget")
            .and_then(|budget_obj| budget_obj.get("remaining_overhead_ms"))
            .and_then(Value::as_f64)
            .expect("remaining_overhead_ms");
        assert!((used_overhead - selected_cost).abs() < 1e-9);
        assert!(
            (remaining_overhead - (budget.max_overhead_ms - selected_cost)).abs() < 1e-9,
            "remaining overhead must reflect selected cost"
        );
    }

    #[test]
    fn voi_scheduler_marks_budget_exceeded_when_remaining_budget_is_insufficient() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 95.0,
                "confidence": 0.25,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 9.0 }
            }),
            json!({
                "stage": "schedule",
                "ev_score": 50.0,
                "confidence": 0.35,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 4.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 38.0,
                "confidence": 0.40,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 3.0 }
            }),
        ];
        let budget = VoiBudgetConfig {
            max_overhead_ms: 50.0,
            max_experiments: 5,
            stale_after_hours: 24.0,
        };
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 27.0,
                        "backend_stall_pct": 23.0,
                        "llc_miss_pct": 16.0,
                        "branch_miss_pct": 4.5
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "high",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            budget,
        );

        let candidate_rows = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidate rows");
        let selected_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(true))
            .collect::<Vec<_>>();
        assert_eq!(selected_rows.len(), 1, "one stage should fit budget");

        let skipped_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(false))
            .collect::<Vec<_>>();
        assert!(
            skipped_rows.iter().any(
                |row| row.get("skip_reason").and_then(Value::as_str) == Some("budget_exceeded")
            ),
            "at least one skipped row should report budget_exceeded"
        );
        assert!(
            skipped_rows.iter().all(|row| {
                row.get("skip_reason").and_then(Value::as_str) != Some("max_experiments_reached")
            }),
            "budget-constrained fixture should not use max_experiments_reached"
        );

        let selected_plan = plan
            .get("selected_plan")
            .and_then(Value::as_array)
            .expect("selected_plan");
        assert_eq!(selected_plan.len(), 1);
        assert_eq!(
            selected_plan[0].get("stage"),
            selected_rows[0].get("stage"),
            "selected_plan stage should match selected candidate"
        );
    }

    #[test]
    fn realized_information_gain_applies_correlation_and_regression_penalty() {
        let selected = vec![
            VoiCandidate {
                stage: "queue".to_string(),
                utility_total: 18.0,
                uncertainty_reduction: 6.0,
                user_impact_score: 9.0,
                pressure_bonus: 3.0,
                expected_information_gain: 1.8,
                cost_ms: 42.0,
                voi_score: 0.42,
                recommended_probe: stage_probe_recommendation("queue"),
            },
            VoiCandidate {
                stage: "marshal".to_string(),
                utility_total: 7.0,
                uncertainty_reduction: 2.5,
                user_impact_score: 3.0,
                pressure_bonus: 1.5,
                expected_information_gain: 0.7,
                cost_ms: 18.0,
                voi_score: 0.38,
                recommended_probe: stage_probe_recommendation("marshal"),
            },
        ];

        let expected_total = 2.5;

        let high = realized_information_gain(&selected, "high", false, false);
        let high_realized = high
            .get("realized_total")
            .and_then(Value::as_f64)
            .expect("high realized_total");
        assert!((high_realized - expected_total).abs() < 1e-12);
        assert_eq!(
            high.get("correlation_multiplier").and_then(Value::as_f64),
            Some(1.0)
        );
        assert_eq!(
            high.get("regression_penalty").and_then(Value::as_f64),
            Some(1.0)
        );

        let low_penalized = realized_information_gain(&selected, "low", true, true);
        let low_penalized_realized = low_penalized
            .get("realized_total")
            .and_then(Value::as_f64)
            .expect("low realized_total");
        let expected_low_penalized = expected_total * 0.45 * 0.75;
        assert!((low_penalized_realized - expected_low_penalized).abs() < 1e-12);
        assert_eq!(
            low_penalized
                .get("correlation_multiplier")
                .and_then(Value::as_f64),
            Some(0.45)
        );
        assert_eq!(
            low_penalized
                .get("regression_penalty")
                .and_then(Value::as_f64),
            Some(0.75)
        );

        for (correlation_strength, outcome_regressed, pmu_regressed) in [
            ("high", false, false),
            ("moderate", false, true),
            ("low", true, false),
            ("unknown", true, true),
        ] {
            let gain = realized_information_gain(
                &selected,
                correlation_strength,
                outcome_regressed,
                pmu_regressed,
            );
            let realized_total = gain
                .get("realized_total")
                .and_then(Value::as_f64)
                .expect("realized_total");
            assert!(
                realized_total <= expected_total + 1e-12,
                "realized gain must not exceed expected gain for {correlation_strength}"
            );
        }
    }

    #[test]
    fn voi_scheduler_selection_efficiency_beats_naive_always_profile_baseline() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 95.0,
                "confidence": 0.25,
                "pmu_multiplier": 1.8,
                "projected_user_impact": { "turn_latency_p95_ms": 9.0 }
            }),
            json!({
                "stage": "schedule",
                "ev_score": 60.0,
                "confidence": 0.40,
                "pmu_multiplier": 1.5,
                "projected_user_impact": { "turn_latency_p95_ms": 6.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 15.0,
                "confidence": 0.92,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 0.3 }
            }),
            json!({
                "stage": "policy",
                "ev_score": 12.0,
                "confidence": 0.95,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 0.2 }
            }),
        ];
        let budget = VoiBudgetConfig {
            max_overhead_ms: 70.0,
            max_experiments: 3,
            stale_after_hours: 24.0,
        };
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 32.0,
                        "backend_stall_pct": 27.0,
                        "llc_miss_pct": 18.0,
                        "branch_miss_pct": 5.0
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "high",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            budget,
        );

        let candidate_rows = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidate rows");
        let naive_cost = candidate_rows
            .iter()
            .map(|row| {
                row.get("cost_ms")
                    .and_then(Value::as_f64)
                    .expect("candidate cost")
            })
            .sum::<f64>();
        let naive_gain = candidate_rows
            .iter()
            .map(|row| {
                row.get("expected_information_gain")
                    .and_then(Value::as_f64)
                    .expect("candidate expected gain")
            })
            .sum::<f64>();

        let selected_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(true))
            .collect::<Vec<_>>();
        assert!(
            !selected_rows.is_empty(),
            "at least one stage should be selected"
        );

        let selected_cost = selected_rows
            .iter()
            .map(|row| {
                row.get("cost_ms")
                    .and_then(Value::as_f64)
                    .expect("selected cost")
            })
            .sum::<f64>();
        let selected_gain = selected_rows
            .iter()
            .map(|row| {
                row.get("expected_information_gain")
                    .and_then(Value::as_f64)
                    .expect("selected gain")
            })
            .sum::<f64>();

        let used_overhead = plan
            .get("budget")
            .and_then(|budget_obj| budget_obj.get("used_overhead_ms"))
            .and_then(Value::as_f64)
            .expect("used_overhead_ms");
        assert!((used_overhead - selected_cost).abs() < 1e-9);
        assert!(
            used_overhead <= budget.max_overhead_ms + f64::EPSILON,
            "selected plan must satisfy overhead budget"
        );
        assert!(
            naive_cost > budget.max_overhead_ms,
            "naive always-profile baseline should exceed overhead budget in this fixture"
        );

        let selected_efficiency = selected_gain / selected_cost.max(1e-9);
        let naive_efficiency = naive_gain / naive_cost.max(1e-9);
        assert!(
            selected_efficiency >= naive_efficiency - 1e-9,
            "VOI-selected efficiency ({selected_efficiency}) should not regress vs naive baseline ({naive_efficiency})"
        );
    }

    #[test]
    fn voi_scheduler_tie_break_is_stage_lexicographic() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "beta",
                "ev_score": 20.0,
                "confidence": 0.50,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 1.0 }
            }),
            json!({
                "stage": "alpha",
                "ev_score": 20.0,
                "confidence": 0.50,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 1.0 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:59:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 20.0,
                        "backend_stall_pct": 20.0,
                        "llc_miss_pct": 12.0,
                        "branch_miss_pct": 3.0
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "high",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 200.0,
                max_experiments: 2,
                stale_after_hours: 24.0,
            },
        );
        let candidates = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidates");
        assert_eq!(
            candidates[0].get("stage").and_then(Value::as_str),
            Some("alpha"),
            "ties should resolve by stage name ascending"
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_on_stale_evidence() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![json!({
            "stage": "queue",
            "ev_score": 50.0,
            "confidence": 0.5,
            "pmu_multiplier": 1.2,
            "projected_user_impact": { "turn_latency_p95_ms": 2.0 }
        })];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-14T01:00:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 25.0,
                        "backend_stall_pct": 22.0,
                        "llc_miss_pct": 16.0,
                        "branch_miss_pct": 4.0
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "moderate",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 50.0,
                max_experiments: 2,
                stale_after_hours: 12.0,
            },
        );
        assert_eq!(
            plan.get("status").and_then(Value::as_str),
            Some("safe_mode")
        );
        assert!(
            plan.get("safe_mode_reasons")
                .and_then(Value::as_array)
                .is_some_and(|reasons| reasons
                    .iter()
                    .any(|reason| reason.as_str() == Some("stale_evidence")))
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_on_estimator_drift() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![json!({
            "stage": "queue",
            "ev_score": 55.0,
            "confidence": 0.45,
            "pmu_multiplier": 1.2,
            "projected_user_impact": { "turn_latency_p95_ms": 2.4 }
        })];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "normalized_counters": {
                        "frontend_stall_pct": 24.0,
                        "backend_stall_pct": 21.0,
                        "llc_miss_pct": 15.0,
                        "branch_miss_pct": 4.1
                    }
                }),
                pmu_comparison: &json!({
                    "status": "compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "low",
                    "signals": {
                        "pmu_worse": true,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 50.0,
                max_experiments: 2,
                stale_after_hours: 24.0,
            },
        );
        assert_eq!(
            plan.get("status").and_then(Value::as_str),
            Some("safe_mode")
        );
        let reasons = plan
            .get("safe_mode_reasons")
            .and_then(Value::as_array)
            .expect("safe_mode_reasons");
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].as_str(), Some("estimator_drift"));
        assert_eq!(
            plan.get("estimator")
                .and_then(|estimator| estimator.get("missing_telemetry"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            plan.get("estimator")
                .and_then(|estimator| estimator.get("stale_evidence"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            plan.get("estimator")
                .and_then(|estimator| estimator.get("drift_detected"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_reason_order_is_deterministic() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![json!({
            "stage": "queue",
            "ev_score": 45.0,
            "confidence": 0.5,
            "pmu_multiplier": 1.0,
            "projected_user_impact": { "turn_latency_p95_ms": 2.0 }
        })];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-14T01:00:00Z" }),
                pmu_meta: &json!({
                    "status": "not_collected"
                }),
                pmu_comparison: &json!({
                    "status": "not_compared",
                    "regressions": { "overall": false }
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "low",
                    "signals": {
                        "pmu_worse": true,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 50.0,
                max_experiments: 2,
                stale_after_hours: 12.0,
            },
        );
        assert_eq!(
            plan.get("status").and_then(Value::as_str),
            Some("safe_mode")
        );
        let reasons = plan
            .get("safe_mode_reasons")
            .and_then(Value::as_array)
            .expect("safe_mode_reasons")
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                "missing_telemetry".to_string(),
                "stale_evidence".to_string(),
                "estimator_drift".to_string()
            ]
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_selects_min_cost_diagnostic_stage() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 100.0,
                "confidence": 0.2,
                "pmu_multiplier": 1.6,
                "projected_user_impact": { "turn_latency_p95_ms": 9.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 10.0,
                "confidence": 0.8,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 1.0 }
            }),
            json!({
                "stage": "policy",
                "ev_score": 45.0,
                "confidence": 0.4,
                "pmu_multiplier": 1.1,
                "projected_user_impact": { "turn_latency_p95_ms": 3.5 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "status": "not_collected"
                }),
                pmu_comparison: &json!({
                    "status": "not_compared"
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "moderate",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 20.0,
                max_experiments: 3,
                stale_after_hours: 24.0,
            },
        );
        assert_eq!(
            plan.get("selection_strategy").and_then(Value::as_str),
            Some("safe_mode_min_cost_probe")
        );
        let selected_plan = plan
            .get("selected_plan")
            .and_then(Value::as_array)
            .expect("selected_plan");
        assert_eq!(selected_plan.len(), 1);
        assert_eq!(
            selected_plan[0].get("stage").and_then(Value::as_str),
            Some("marshal")
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_marks_diagnostic_budget_exceeded_when_too_small() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 100.0,
                "confidence": 0.2,
                "pmu_multiplier": 1.6,
                "projected_user_impact": { "turn_latency_p95_ms": 9.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 10.0,
                "confidence": 0.8,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 1.0 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "status": "not_collected"
                }),
                pmu_comparison: &json!({
                    "status": "not_compared"
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "moderate",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 5.0,
                max_experiments: 3,
                stale_after_hours: 24.0,
            },
        );
        assert_eq!(
            plan.get("selection_strategy").and_then(Value::as_str),
            Some("safe_mode_min_cost_probe")
        );
        assert_eq!(
            plan.get("selected_plan")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len),
            Some(0)
        );

        let marshal_row = plan
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("stage").and_then(Value::as_str) == Some("marshal"))
            })
            .expect("marshal candidate row");
        assert_eq!(
            marshal_row.get("skip_reason").and_then(Value::as_str),
            Some("diagnostic_budget_exceeded")
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_on_missing_telemetry() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![json!({
            "stage": "queue",
            "ev_score": 45.0,
            "confidence": 0.45,
            "pmu_multiplier": 1.1,
            "projected_user_impact": { "turn_latency_p95_ms": 2.2 }
        })];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "status": "not_collected"
                }),
                pmu_comparison: &json!({
                    "status": "not_compared",
                    "reason": "candidate_pmu_counters_unavailable"
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "low",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 50.0,
                max_experiments: 2,
                stale_after_hours: 24.0,
            },
        );
        assert_eq!(
            plan.get("status").and_then(Value::as_str),
            Some("safe_mode")
        );
        assert!(
            plan.get("safe_mode_reasons")
                .and_then(Value::as_array)
                .is_some_and(|reasons| reasons
                    .iter()
                    .any(|reason| reason.as_str() == Some("missing_telemetry")))
        );
    }

    #[test]
    fn voi_scheduler_safe_mode_marks_non_diagnostic_rows_with_guardrail_reason() {
        let now = DateTime::parse_from_rfc3339("2026-02-16T05:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let hotspots = vec![
            json!({
                "stage": "queue",
                "ev_score": 80.0,
                "confidence": 0.30,
                "pmu_multiplier": 1.4,
                "projected_user_impact": { "turn_latency_p95_ms": 7.0 }
            }),
            json!({
                "stage": "marshal",
                "ev_score": 20.0,
                "confidence": 0.75,
                "pmu_multiplier": 1.0,
                "projected_user_impact": { "turn_latency_p95_ms": 1.0 }
            }),
            json!({
                "stage": "policy",
                "ev_score": 42.0,
                "confidence": 0.40,
                "pmu_multiplier": 1.1,
                "projected_user_impact": { "turn_latency_p95_ms": 3.5 }
            }),
        ];
        let plan = build_voi_scheduler_plan_at(
            VoiPlannerInputs {
                hotspot_entries: &hotspots,
                run_metadata: &json!({ "finished_at": "2026-02-16T04:55:00Z" }),
                pmu_meta: &json!({
                    "status": "not_collected"
                }),
                pmu_comparison: &json!({
                    "status": "not_compared",
                    "reason": "candidate_pmu_counters_unavailable"
                }),
                pmu_outcome_correlation: &json!({
                    "correlation_strength": "moderate",
                    "signals": {
                        "pmu_worse": false,
                        "outcome_regression": false
                    }
                }),
                outcome_comparison: &json!({
                    "regressions": { "overall": false }
                }),
            },
            now,
            VoiBudgetConfig {
                max_overhead_ms: 100.0,
                max_experiments: 3,
                stale_after_hours: 24.0,
            },
        );
        assert_eq!(
            plan.get("status").and_then(Value::as_str),
            Some("safe_mode")
        );

        let candidate_rows = plan
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidate rows");
        let selected_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(true))
            .collect::<Vec<_>>();
        assert_eq!(
            selected_rows.len(),
            1,
            "safe mode should select only one diagnostic candidate"
        );
        assert_eq!(
            selected_rows[0].get("stage").and_then(Value::as_str),
            Some("marshal"),
            "safe mode should pick min-cost diagnostic stage"
        );

        let guardrail_rows = candidate_rows
            .iter()
            .filter(|row| row.get("selected").and_then(Value::as_bool) == Some(false))
            .collect::<Vec<_>>();
        assert!(
            guardrail_rows.iter().all(|row| {
                row.get("skip_reason").and_then(Value::as_str) == Some("safe_mode_guardrail")
            }),
            "non-diagnostic candidates should be skipped by safe-mode guardrail"
        );
    }
}
