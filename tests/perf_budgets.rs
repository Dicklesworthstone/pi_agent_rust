//! Performance budget definitions and enforcement tests (bd-1fc4).
//!
//! Centralizes all performance budgets for the Pi Agent Rust runtime. Each budget
//! has an explicit threshold, measurement methodology, and CI enforcement path.
//!
//! Budgets are validated against actual benchmark data when available.
//! Run with: `cargo test --test perf_budgets -- --nocapture`

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::unreadable_literal
)]

use pi::perf_build::{
    BINARY_SIZE_RELEASE_BUDGET_MB, BUILD_FINGERPRINT_CONTRACT, BenchmarkBuildVerification,
    BenchmarkProvenance, CANONICAL_PIJS_PERF_FEATURES, benchmark_provenance_config_hash,
    matches_canonical_perf_build_fingerprint, matches_canonical_pijs_perf_features,
    profile_from_target_path, sha256_file,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BudgetComparison {
    Maximum,
    Minimum,
}

impl BudgetComparison {
    fn passes(self, actual: f64, threshold: f64) -> bool {
        match self {
            Self::Maximum => actual <= threshold,
            Self::Minimum => actual >= threshold,
        }
    }

    const fn symbol(self) -> &'static str {
        match self {
            Self::Maximum => "<=",
            Self::Minimum => ">=",
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Maximum => "maximum",
            Self::Minimum => "minimum",
        }
    }
}

// ─── Budget Definitions ──────────────────────────────────────────────────────

/// A single performance budget with threshold and measurement context.
#[derive(Debug, Clone, Serialize)]
struct Budget {
    /// Human-readable name.
    name: &'static str,
    /// Category (startup, extension, tool, memory, binary).
    category: &'static str,
    /// The metric being measured (e.g., "p95 latency", "RSS").
    metric: &'static str,
    /// Unit of measurement (ms, us, MB, count).
    unit: &'static str,
    /// Comparison boundary interpreted according to `comparison`.
    threshold: f64,
    /// Whether passing requires the measured value to stay at or below the
    /// threshold, or at or above it.
    comparison: BudgetComparison,
    /// Measurement methodology.
    methodology: &'static str,
    /// Whether this budget is enforced in CI.
    ci_enforced: bool,
}

/// All performance budgets for the Pi Agent Rust runtime.
const BUDGETS: &[Budget] = &[
    // ── Startup ──────────────────────────────────────────────────────────
    Budget {
        name: "startup_version_p95",
        category: "startup",
        metric: "p95 latency",
        unit: "ms",
        threshold: 100.0,
        comparison: BudgetComparison::Maximum,
        methodology: "hyperfine: `pi --version` (10 runs, 3 warmup)",
        ci_enforced: true,
    },
    Budget {
        name: "startup_full_agent_p95",
        category: "startup",
        metric: "p95 latency",
        unit: "ms",
        threshold: 200.0,
        comparison: BudgetComparison::Maximum,
        methodology: "hyperfine: `pi --print '.'` with full init (10 runs, 3 warmup)",
        ci_enforced: false, // Requires API key or VCR
    },
    // ── Extension Loading ────────────────────────────────────────────────
    Budget {
        name: "ext_cold_load_simple_p95",
        category: "extension",
        metric: "p95 cold load time",
        unit: "ms",
        threshold: 5.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: load_init_cold for simple single-file extensions (10 samples)",
        ci_enforced: true,
    },
    Budget {
        name: "ext_cold_load_complex_p95",
        category: "extension",
        metric: "p95 cold load time",
        unit: "ms",
        threshold: 50.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: load_init_cold for multi-registration extensions (10 samples)",
        ci_enforced: false,
    },
    Budget {
        name: "ext_load_60_total",
        category: "extension",
        metric: "total load time (60 official extensions)",
        unit: "ms",
        threshold: 10000.0, // 10 seconds total for all 60
        comparison: BudgetComparison::Maximum,
        methodology: "conformance runner: sequential load of all 60 official extensions",
        ci_enforced: false,
    },
    // ── Tool Call ─────────────────────────────────────────────────────────
    Budget {
        name: "tool_call_latency_mean",
        category: "tool_call",
        metric: "mean per-call latency",
        unit: "us",
        threshold: 200.0,
        comparison: BudgetComparison::Maximum,
        methodology: "pijs_workload: arithmetic mean across exactly 2000 iterations x 1 tool call, executable-path-verified perf profile",
        ci_enforced: true,
    },
    Budget {
        name: "tool_call_throughput_min",
        category: "tool_call",
        metric: "minimum calls/sec",
        unit: "calls/sec",
        threshold: 5000.0, // Must meet or exceed 5k calls/sec
        comparison: BudgetComparison::Minimum,
        methodology: "pijs_workload: aggregate throughput across exactly 2000 iterations x 10 tool calls, executable-path-verified perf profile",
        ci_enforced: true,
    },
    // ── Event Dispatch ───────────────────────────────────────────────────
    Budget {
        name: "event_dispatch_p99",
        category: "event_dispatch",
        metric: "p99 dispatch latency",
        unit: "us",
        threshold: 5000.0, // 5ms
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: event_hook dispatch for before_agent_start (100 samples)",
        ci_enforced: false,
    },
    // ── Context Intelligence ─────────────────────────────────────────────
    Budget {
        name: "context_graph_build_cold_p95",
        category: "context_intelligence",
        metric: "p95 cold graph build latency",
        unit: "ms",
        threshold: 500.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: semantic_context/graph_build_cold on large filesystem fixture",
        ci_enforced: true,
    },
    Budget {
        name: "context_graph_build_warm_p95",
        category: "context_intelligence",
        metric: "p95 warm graph build latency",
        unit: "ms",
        threshold: 250.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: semantic_context/graph_build_warm on large filesystem fixture",
        ci_enforced: true,
    },
    Budget {
        name: "context_incremental_update_p95",
        category: "context_intelligence",
        metric: "p95 single-change rebuild latency",
        unit: "ms",
        threshold: 250.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: semantic_context/incremental_update rebuild after one changed file",
        ci_enforced: true,
    },
    Budget {
        name: "context_planning_p95",
        category: "context_intelligence",
        metric: "p95 planner latency",
        unit: "ms",
        threshold: 50.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: semantic_context/planning on large graph fixture",
        ci_enforced: true,
    },
    Budget {
        name: "context_bundle_serialization_p95",
        category: "context_intelligence",
        metric: "p95 bundle serialization latency",
        unit: "ms",
        threshold: 25.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: semantic_context/bundle_serialization on large bundle fixture",
        ci_enforced: true,
    },
    Budget {
        name: "context_bundle_estimated_bytes_max",
        category: "context_intelligence",
        metric: "bundle estimated size",
        unit: "bytes",
        threshold: 262_144.0,
        comparison: BudgetComparison::Maximum,
        methodology: "semantic_context budget artifact: estimated selected bundle bytes",
        ci_enforced: true,
    },
    // ── Policy Evaluation ────────────────────────────────────────────────
    Budget {
        name: "policy_eval_p99",
        category: "policy",
        metric: "p99 evaluation time",
        unit: "ns",
        threshold: 500.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: ext_policy/evaluate with various modes and capabilities",
        ci_enforced: true,
    },
    // ── Memory ───────────────────────────────────────────────────────────
    Budget {
        name: "idle_memory_rss",
        category: "memory",
        metric: "RSS at idle",
        unit: "MB",
        threshold: 50.0,
        comparison: BudgetComparison::Maximum,
        methodology: "sysinfo: measure RSS after startup, before any user input",
        ci_enforced: true,
    },
    Budget {
        name: "sustained_load_rss_growth",
        category: "memory",
        metric: "RSS growth under 30s sustained load",
        unit: "percent",
        threshold: 5.0,
        comparison: BudgetComparison::Maximum,
        methodology: "stress test: 15 extensions, 50 events/sec for 30 seconds",
        ci_enforced: false,
    },
    // ── Binary Size ──────────────────────────────────────────────────────
    Budget {
        name: "binary_size_release",
        category: "binary",
        metric: "release binary size",
        unit: "MB",
        threshold: BINARY_SIZE_RELEASE_BUDGET_MB,
        comparison: BudgetComparison::Maximum,
        methodology: "ls -la target/release/pi (stripped)",
        ci_enforced: true,
    },
    // ── Protocol Parsing ─────────────────────────────────────────────────
    Budget {
        name: "protocol_parse_p99",
        category: "protocol",
        metric: "p99 parse+validate time",
        unit: "us",
        threshold: 50.0,
        comparison: BudgetComparison::Maximum,
        methodology: "criterion: ext_protocol/parse_and_validate for host_call and log messages",
        ci_enforced: true,
    },
];

/// Canonical cross-language inventory serialization used by release consumers.
///
/// Array and field order are fixed. Thresholds use exactly six decimal places,
/// avoiding parser-dependent `100` versus `100.0` spellings while retaining all
/// precision used by the v0.2.0 budget inventory.
fn budget_inventory_canonical_json() -> String {
    let mut canonical = String::from("[");
    for (index, budget) in BUDGETS.iter().enumerate() {
        if index != 0 {
            canonical.push(',');
        }
        let name = serde_json::to_string(budget.name).expect("serialize budget name");
        let category = serde_json::to_string(budget.category).expect("serialize budget category");
        let metric = serde_json::to_string(budget.metric).expect("serialize budget metric");
        let unit = serde_json::to_string(budget.unit).expect("serialize budget unit");
        let comparison =
            serde_json::to_string(budget.comparison.as_str()).expect("serialize comparison");
        let methodology = serde_json::to_string(budget.methodology).expect("serialize methodology");
        let _ = write!(
            canonical,
            "{{\"name\":{name},\"category\":{category},\"metric\":{metric},\"unit\":{unit},\"threshold\":{:.6},\"comparison\":{comparison},\"ci_enforced\":{},\"methodology\":{methodology}}}",
            budget.threshold, budget.ci_enforced
        );
    }
    canonical.push(']');
    canonical
}

fn budget_inventory_sha256() -> String {
    let digest = Sha256::digest(budget_inventory_canonical_json().as_bytes());
    format!("{digest:x}")
}

const DEFAULT_MAX_ARTIFACT_AGE_HOURS: f64 = 24.0;
const BUN_KILLER_MAX_RUST_VS_BUN_RATIO: f64 = 0.33;
const CONTEXT_BENCH_CASE: &str = "large_workspace";
const CONTEXT_INTELLIGENCE_PERF_SCHEMA: &str = "pi.semantic_context.performance_budget.v1";
const CONTEXT_INTELLIGENCE_BUDGET_METRICS: &[(&str, &str)] = &[
    (
        "context_graph_build_cold_p95",
        "context_graph_build_cold_ms",
    ),
    (
        "context_graph_build_warm_p95",
        "context_graph_build_warm_ms",
    ),
    (
        "context_incremental_update_p95",
        "context_incremental_update_ms",
    ),
    ("context_planning_p95", "context_planning_ms"),
    (
        "context_bundle_serialization_p95",
        "context_bundle_serialization_ms",
    ),
    (
        "context_bundle_estimated_bytes_max",
        "context_bundle_estimated_bytes",
    ),
];
const CONTEXT_INTELLIGENCE_CACHE_FIELDS: &[&str] =
    &["cold_graph_build", "warm_graph_build", "incremental_update"];
const PIJS_REGRESSION_GATE_ITERATIONS: u64 = 2_000;

// ─── Data Readers ────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn resolve_target_dir(root: &Path, raw_target_dir: Option<&std::ffi::OsStr>) -> PathBuf {
    raw_target_dir.map_or_else(
        || root.join("target"),
        |raw| {
            let target_dir = PathBuf::from(raw);
            if target_dir.is_absolute() {
                target_dir
            } else {
                root.join(target_dir)
            }
        },
    )
}

fn target_dir_candidates_for(
    root: &Path,
    canonical_project_root: &Path,
    raw_target_dir: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    if root == canonical_project_root {
        vec![resolve_target_dir(root, raw_target_dir)]
    } else {
        // Callers evaluating a fixture root must remain hermetic and must not
        // inherit artifacts from the real project's Cargo target directory.
        vec![root.join("target")]
    }
}

fn target_dir_candidates(root: &Path) -> Vec<PathBuf> {
    target_dir_candidates_for(
        root,
        &project_root(),
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
    )
}

fn resolve_env_path(root: &Path, path: PathBuf) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn dedup_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn perf_evidence_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(raw) = std::env::var_os("PERF_EVIDENCE_DIR")
        && let Some(path) = resolve_env_path(root, PathBuf::from(raw))
    {
        dirs.push(path);
    }
    if let Some(raw) = std::env::var_os("PERF_EVIDENCE_DIRS") {
        for path in std::env::split_paths(&raw) {
            if let Some(path) = resolve_env_path(root, path) {
                dirs.push(path);
            }
        }
    }
    dedup_paths(dirs)
}

fn evidence_dir_paths(root: &Path, relative_paths: &[&str]) -> Vec<PathBuf> {
    perf_evidence_dirs(root)
        .into_iter()
        .flat_map(|dir| {
            relative_paths
                .iter()
                .map(move |relative| dir.join(relative))
        })
        .collect()
}

fn evidence_then_target_paths(
    root: &Path,
    evidence_relative_paths: &[&str],
    target_relative_paths: &[&str],
) -> Vec<PathBuf> {
    let mut paths = evidence_dir_paths(root, evidence_relative_paths);
    for cargo_target_dir in target_dir_candidates(root) {
        paths.extend(
            target_relative_paths
                .iter()
                .map(|relative| cargo_target_dir.join(relative)),
        );
    }
    dedup_paths(paths)
}

fn portable_relative_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn display_source_path(root: &Path, path: &Path) -> String {
    for (index, evidence_dir) in perf_evidence_dirs(root).iter().enumerate() {
        if let Ok(relative) = path.strip_prefix(evidence_dir) {
            return format!("evidence[{index}]://{}", portable_relative_path(relative));
        }
    }
    for (index, target_dir) in target_dir_candidates(root).iter().enumerate() {
        if let Ok(relative) = path.strip_prefix(target_dir) {
            return format!(
                "cargo-target[{index}]://{}",
                portable_relative_path(relative)
            );
        }
    }
    if let Ok(relative) = path.strip_prefix(root) {
        return format!("repo://{}", portable_relative_path(relative));
    }
    format!(
        "external://{}",
        path.file_name()
            .map_or_else(|| "artifact".into(), |name| name.to_string_lossy())
    )
}

fn canonicalize_diagnostic_text(root: &Path, text: &str) -> String {
    let mut replacements = Vec::new();
    for (index, evidence_dir) in perf_evidence_dirs(root).iter().enumerate() {
        replacements.push((
            format!("{}/", evidence_dir.to_string_lossy().trim_end_matches('/')),
            format!("evidence[{index}]://"),
        ));
    }
    for (index, target_dir) in target_dir_candidates(root).iter().enumerate() {
        replacements.push((
            format!("{}/", target_dir.to_string_lossy().trim_end_matches('/')),
            format!("cargo-target[{index}]://"),
        ));
    }
    replacements.push((
        format!("{}/", root.to_string_lossy().trim_end_matches('/')),
        "repo://".to_string(),
    ));
    replacements.sort_by(|(left, _), (right, _)| right.len().cmp(&left.len()));

    replacements
        .into_iter()
        .fold(text.to_string(), |canonical, (prefix, replacement)| {
            canonical.replace(&prefix, &replacement)
        })
}

fn read_json_file(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_jsonl_file(path: &Path) -> Vec<Value> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn load_perf_sli_matrix() -> Value {
    let path = project_root().join("docs/perf_sli_matrix.json");
    read_json_file(&path).unwrap_or_else(|| {
        eprintln!("failed to parse {}", path.display());
        Value::Null
    })
}

/// Measurement result for a budget check.
#[derive(Debug, Clone, Serialize)]
struct BudgetResult {
    budget_name: String,
    category: String,
    threshold: f64,
    comparison: BudgetComparison,
    unit: String,
    actual: Option<f64>,
    status: String, // "PASS", "FAIL", "NO_DATA"
    source: String,
    ci_enforced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DataContractFailure {
    contract_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_name: Option<String>,
    detail: String,
    remediation: String,
}

fn perf_strict_mode() -> bool {
    std::env::var("PI_PERF_STRICT").is_ok_and(|v| v == "1")
}

fn budget_report_generation_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim() == "1")
}

fn budget_report_generation_requested() -> bool {
    budget_report_generation_enabled(
        std::env::var("PI_GENERATE_PERF_BUDGET_REPORT")
            .ok()
            .as_deref(),
    )
}

fn max_artifact_age_hours() -> f64 {
    std::env::var("PI_PERF_MAX_ARTIFACT_AGE_HOURS")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|hours| *hours > 0.0)
        .unwrap_or(DEFAULT_MAX_ARTIFACT_AGE_HOURS)
}

fn perf_run_id() -> Option<String> {
    [
        "PERF_CLAIM_CORRELATION_ID",
        "CI_CORRELATION_ID",
        "PI_PERF_CORRELATION_ID",
    ]
    .into_iter()
    .find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn tracked_index_flags_are_default(output: &[u8]) -> bool {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .all(|record| record.starts_with(b"H "))
}

fn git_command_succeeds(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn clean_source_commit(root: &Path) -> Option<String> {
    let index_flags = Command::new("git")
        .args(["ls-files", "-v", "-z", "--"])
        .current_dir(root)
        .output()
        .ok()?;
    if !index_flags.status.success() || !tracked_index_flags_are_default(&index_flags.stdout) {
        return None;
    }

    let status = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .ok()?;
    if !status.status.success() || !status.stdout.is_empty() {
        return None;
    }
    if !git_command_succeeds(root, &["diff", "--quiet", "--no-ext-diff", "HEAD", "--"])
        || !git_command_succeeds(
            root,
            &["diff", "--cached", "--quiet", "--no-ext-diff", "HEAD", "--"],
        )
    {
        return None;
    }
    let revision = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .current_dir(root)
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let commit = String::from_utf8(revision.stdout).ok()?;
    let commit = commit.trim();
    (commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| commit.to_ascii_lowercase())
}

#[allow(clippy::too_many_arguments)]
fn claim_readiness_blockers(
    strict_mode: bool,
    source_commit: Option<&str>,
    run_id: Option<&str>,
    correlation_id: Option<&str>,
    ci_enforced: usize,
    ci_with_data: usize,
    ci_fail: usize,
    ci_no_data: usize,
    fail: usize,
    no_data: usize,
    data_contract_failures: usize,
) -> Vec<&'static str> {
    let mut blockers = BTreeSet::new();
    if !strict_mode {
        blockers.insert("strict_mode_disabled");
    }
    if source_commit.is_none() {
        blockers.insert("source_commit_unbound");
    }
    if run_id.is_none() {
        blockers.insert("run_id_missing");
    }
    if correlation_id.is_none() || run_id != correlation_id {
        blockers.insert("correlation_id_missing");
    }
    if ci_with_data != ci_enforced || ci_no_data != 0 {
        blockers.insert("ci_budget_data_missing");
    }
    if ci_fail != 0 {
        blockers.insert("ci_budget_failed");
    }
    if fail != 0 {
        blockers.insert("budget_failed");
    }
    if no_data != 0 {
        blockers.insert("budget_data_missing");
    }
    if data_contract_failures != 0 {
        blockers.insert("data_contract_failure");
    }
    blockers.into_iter().collect()
}

fn budget_definitions_value() -> Vec<Value> {
    BUDGETS
        .iter()
        .map(|budget| {
            json!({
                "name": budget.name,
                "category": budget.category,
                "metric": budget.metric,
                "unit": budget.unit,
                "threshold": budget.threshold,
                "comparison": budget.comparison,
                "ci_enforced": budget.ci_enforced,
                "methodology": budget.methodology,
            })
        })
        .collect()
}

struct BudgetSummaryLineage<'a> {
    generated_at: &'a str,
    source_commit: Option<&'a str>,
    run_id: Option<&'a str>,
    correlation_id: Option<&'a str>,
    strict_mode: bool,
}

fn benchmark_lineage_is_authoritative(lineage: &BudgetSummaryLineage<'_>) -> bool {
    lineage.strict_mode
        && lineage.source_commit.is_some()
        && lineage.run_id.is_some()
        && lineage.run_id == lineage.correlation_id
}

fn blocked_sentinel_result(budget: &Budget) -> BudgetResult {
    BudgetResult {
        budget_name: budget.name.to_string(),
        category: budget.category.to_string(),
        threshold: budget.threshold,
        comparison: budget.comparison,
        unit: budget.unit.to_string(),
        actual: None,
        status: "NO_DATA".to_string(),
        source: "not evaluated: authoritative benchmark lineage is incomplete".to_string(),
        ci_enforced: budget.ci_enforced,
        failure_reason: None,
    }
}

fn evaluate_budget_report(
    root: &Path,
    lineage: &BudgetSummaryLineage<'_>,
) -> (Vec<BudgetResult>, Vec<DataContractFailure>) {
    if !benchmark_lineage_is_authoritative(lineage) {
        return (
            BUDGETS.iter().map(blocked_sentinel_result).collect(),
            Vec::new(),
        );
    }

    (
        BUDGETS
            .iter()
            .map(|budget| check_budget_with_strict_at_root(budget, true, root))
            .collect(),
        collect_data_contract_failures(root),
    )
}

fn budget_summary_value(
    lineage: &BudgetSummaryLineage<'_>,
    results: &[BudgetResult],
    data_contract_failures: &[DataContractFailure],
) -> Value {
    let pass_count = results
        .iter()
        .filter(|result| result.status == "PASS")
        .count();
    let fail_count = results
        .iter()
        .filter(|result| result.status == "FAIL")
        .count();
    let no_data_count = results
        .iter()
        .filter(|result| result.status == "NO_DATA")
        .count();
    let ci_enforced_count = BUDGETS.iter().filter(|budget| budget.ci_enforced).count();
    let ci_results = results
        .iter()
        .filter(|result| result.ci_enforced)
        .collect::<Vec<_>>();
    let ci_with_data_count = ci_results
        .iter()
        .filter(|result| result.actual.is_some())
        .count();
    let ci_fail_count = ci_results
        .iter()
        .filter(|result| result.status == "FAIL")
        .count();
    let ci_no_data_count = ci_results
        .iter()
        .filter(|result| result.status == "NO_DATA")
        .count();
    let readiness_blockers = claim_readiness_blockers(
        lineage.strict_mode,
        lineage.source_commit,
        lineage.run_id,
        lineage.correlation_id,
        ci_enforced_count,
        ci_with_data_count,
        ci_fail_count,
        ci_no_data_count,
        fail_count,
        no_data_count,
        data_contract_failures.len(),
    );
    let claims_authorized = readiness_blockers.is_empty();

    json!({
        "schema": "pi.perf.budget_summary.v2",
        "generated_at": lineage.generated_at,
        "source_commit": lineage.source_commit,
        "run_id": lineage.run_id,
        "correlation_id": lineage.correlation_id,
        "strict_mode": lineage.strict_mode,
        "total_budgets": BUDGETS.len(),
        "ci_enforced": ci_enforced_count,
        "ci_with_data": ci_with_data_count,
        "ci_fail": ci_fail_count,
        "ci_no_data": ci_no_data_count,
        "pass": pass_count,
        "fail": fail_count,
        "no_data": no_data_count,
        "data_contract_failures_count": data_contract_failures.len(),
        "failing_data_contracts": data_contract_failures,
        "budgets": budget_definitions_value(),
        "budget_results": results,
        "claim_readiness": {
            "status": if claims_authorized { "claim_ready" } else { "blocked" },
            "performance_claims_authorized": claims_authorized,
            "blocking_reason_codes": readiness_blockers,
        },
    })
}

fn classify_budget_status(budget: &Budget, actual: Option<f64>, strict: bool) -> &'static str {
    match actual {
        Some(val) => {
            if budget.comparison.passes(val, budget.threshold) {
                "PASS"
            } else {
                "FAIL"
            }
        }
        None if budget.ci_enforced && strict => "FAIL",
        None => "NO_DATA",
    }
}

fn artifact_age_hours(path: &Path) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = SystemTime::now().duration_since(modified).ok()?;
    Some(elapsed.as_secs_f64() / 3600.0)
}

fn format_path_list(root: &Path, paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| display_source_path(root, path))
        .collect::<Vec<_>>()
        .join(", ")
}

fn evaluate_artifact_contract(
    root: &Path,
    paths: &[PathBuf],
    max_age_hours: f64,
) -> Option<String> {
    if paths.is_empty() {
        return Some("no artifact paths configured".to_string());
    }

    let existing: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        return Some(format!(
            "missing artifacts; expected one of [{}]",
            format_path_list(root, paths)
        ));
    }

    let mut fresh_found = false;
    let mut stale_details = Vec::new();
    for path in existing {
        match artifact_age_hours(path) {
            Some(age_hours) if age_hours <= max_age_hours => {
                fresh_found = true;
            }
            Some(_) => {
                stale_details.push(format!("{} (stale)", display_source_path(root, path)));
            }
            None => {
                stale_details.push(format!(
                    "{} (mtime unavailable)",
                    display_source_path(root, path)
                ));
            }
        }
    }

    if fresh_found {
        None
    } else {
        Some(format!(
            "all candidate artifacts are stale/invalid (>{max_age_hours:.2}h): {}",
            stale_details.join(", ")
        ))
    }
}

fn budget_artifact_candidates(root: &Path, budget_name: &str) -> Vec<PathBuf> {
    match budget_name {
        "tool_call_latency_mean" | "tool_call_throughput_min" => {
            pijs_workload_candidate_paths(root)
        }
        "ext_cold_load_simple_p95" => criterion_estimate_candidate_paths(
            root,
            "criterion/ext_load_init/load_init_cold/hello/new/estimates.json",
        ),
        "startup_version_p95" => criterion_estimate_candidate_paths(
            root,
            "criterion/startup/version/warm/new/estimates.json",
        ),
        "context_graph_build_cold_p95" => {
            context_criterion_estimate_candidate_paths(root, "graph_build_cold")
        }
        "context_graph_build_warm_p95" => {
            context_criterion_estimate_candidate_paths(root, "graph_build_warm")
        }
        "context_incremental_update_p95" => {
            context_criterion_estimate_candidate_paths(root, "incremental_update")
        }
        "context_planning_p95" => context_criterion_estimate_candidate_paths(root, "planning"),
        "context_bundle_serialization_p95" => {
            context_criterion_estimate_candidate_paths(root, "bundle_serialization")
        }
        "context_bundle_estimated_bytes_max" => context_intelligence_budget_candidate_paths(root),
        "policy_eval_p99" => collect_estimate_json_files_from_bases(&criterion_base_candidates(
            root,
            "criterion/ext_policy/evaluate",
        )),
        "binary_size_release" => binary_size_candidate_paths(root),
        "protocol_parse_p99" => collect_estimate_json_files_from_bases(&criterion_base_candidates(
            root,
            "criterion/ext_protocol/parse_and_validate",
        )),
        _ => Vec::new(),
    }
}

fn binary_size_release_override() -> Option<PathBuf> {
    std::env::var("PERF_RELEASE_BINARY_PATH")
        .ok()
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn build_binary_size_candidate_paths(
    target_dir: &Path,
    release_binary_override: Option<PathBuf>,
    detected_profile: &str,
) -> Vec<PathBuf> {
    let normalized_profile = detected_profile.trim();
    let mut paths = Vec::with_capacity(4);
    if let Some(path) = release_binary_override {
        paths.push(path);
    }
    paths.push(target_dir.join("release/pi"));
    if !normalized_profile.is_empty() && !normalized_profile.eq_ignore_ascii_case("debug") {
        paths.push(target_dir.join(normalized_profile).join("pi"));
    }
    paths.push(target_dir.join("perf/pi"));

    let mut dedup = std::collections::HashSet::new();
    paths.retain(|path| dedup.insert(path.clone()));
    paths
}

fn binary_size_candidate_paths(root: &Path) -> Vec<PathBuf> {
    let detected_profile = pi::perf_build::detect_build_profile();
    let release_binary_override = binary_size_release_override();
    let mut paths = Vec::new();
    for dir in perf_evidence_dirs(root) {
        paths.extend(build_binary_size_candidate_paths(
            &dir,
            release_binary_override.clone(),
            &detected_profile,
        ));
    }
    for dir in target_dir_candidates(root) {
        paths.extend(build_binary_size_candidate_paths(
            &dir,
            release_binary_override.clone(),
            &detected_profile,
        ));
    }
    dedup_paths(paths)
}

fn collect_estimate_json_files(base: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return vec![base.to_path_buf()];
    };
    for entry in entries.flatten() {
        files.push(entry.path().join("new/estimates.json"));
    }
    files.sort();
    if files.is_empty() {
        files.push(base.to_path_buf());
    }
    files
}

fn collect_estimate_json_files_from_bases(bases: &[PathBuf]) -> Vec<PathBuf> {
    dedup_paths(
        bases
            .iter()
            .flat_map(|base| collect_estimate_json_files(base))
            .collect(),
    )
}

fn criterion_base_candidates(root: &Path, relative: &str) -> Vec<PathBuf> {
    let mut bases = evidence_dir_paths(root, &[relative]);
    for dir in target_dir_candidates(root) {
        bases.push(dir.join(relative));
    }
    dedup_paths(bases)
}

fn criterion_estimate_candidate_paths(root: &Path, relative: &str) -> Vec<PathBuf> {
    evidence_then_target_paths(root, &[relative], &[relative])
}

fn context_criterion_relative(bench_name: &str) -> String {
    format!("criterion/semantic_context/{bench_name}/{CONTEXT_BENCH_CASE}/new/estimates.json")
}

fn context_criterion_estimate_candidate_paths(root: &Path, bench_name: &str) -> Vec<PathBuf> {
    let relative = context_criterion_relative(bench_name);
    criterion_estimate_candidate_paths(root, &relative)
}

fn context_intelligence_budget_metric_key(budget_name: &str) -> Option<&'static str> {
    CONTEXT_INTELLIGENCE_BUDGET_METRICS
        .iter()
        .find_map(|(name, metric)| (*name).eq(budget_name).then_some(*metric))
}

fn context_intelligence_budget_candidate_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("PERF_CONTEXT_INTELLIGENCE_BUDGET_JSON") {
        let trimmed = path.trim();
        if !trimmed.is_empty()
            && let Some(path) = resolve_env_path(root, PathBuf::from(trimmed))
        {
            paths.push(path);
        }
    }
    for dir in perf_evidence_dirs(root) {
        paths.extend(context_intelligence_budget_candidate_paths_in_evidence_dir(
            &dir,
        ));
    }
    for dir in target_dir_candidates(root) {
        paths.extend(context_intelligence_budget_candidate_paths_in_target_dir(
            &dir,
        ));
    }
    paths.push(root.join("tests/perf/reports/context_intelligence_planner_budget.json"));
    dedup_paths(paths)
}

fn context_intelligence_budget_candidate_paths_in_target_dir(target_dir: &Path) -> Vec<PathBuf> {
    [
        "perf/context_intelligence_planner_budget.json",
        "perf/results/context_intelligence_planner_budget.json",
        "perf/context_intelligence/perf_budget.json",
    ]
    .into_iter()
    .map(|relative| target_dir.join(relative))
    .collect()
}

fn context_intelligence_budget_candidate_paths_in_evidence_dir(
    evidence_dir: &Path,
) -> Vec<PathBuf> {
    dedup_paths(
        [
            "context_intelligence_planner_budget.json",
            "results/context_intelligence_planner_budget.json",
            "perf/context_intelligence_planner_budget.json",
            "perf/results/context_intelligence_planner_budget.json",
            "context_intelligence/perf_budget.json",
            "perf/context_intelligence/perf_budget.json",
        ]
        .into_iter()
        .map(|relative| evidence_dir.join(relative))
        .collect(),
    )
}

fn extension_stratification_candidates(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("PERF_EXTENSION_STRATIFICATION_JSON") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            paths.push(PathBuf::from(trimmed));
        }
    }
    paths.extend(evidence_then_target_paths(
        root,
        &[
            "extension_benchmark_stratification.json",
            "perf/extension_benchmark_stratification.json",
            "results/extension_benchmark_stratification.json",
            "perf/results/extension_benchmark_stratification.json",
        ],
        &[
            "perf/extension_benchmark_stratification.json",
            "perf/results/extension_benchmark_stratification.json",
        ],
    ));
    paths.push(root.join("tests/perf/reports/extension_benchmark_stratification.json"));
    dedup_paths(paths)
}

fn phase1_matrix_validation_candidates(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("PERF_PHASE1_MATRIX_VALIDATION_JSON") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            paths.push(PathBuf::from(trimmed));
        }
    }
    paths.extend(evidence_then_target_paths(
        root,
        &[
            "phase1_matrix_validation.json",
            "results/phase1_matrix_validation.json",
            "perf/results/phase1_matrix_validation.json",
        ],
        &["perf/results/phase1_matrix_validation.json"],
    ));
    paths.push(root.join("tests/perf/reports/phase1_matrix_validation.json"));
    dedup_paths(paths)
}

fn first_existing_path(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

fn first_fresh_existing_path(paths: &[PathBuf], max_age_hours: f64) -> Option<PathBuf> {
    paths
        .iter()
        .find(|path| {
            path.exists()
                && artifact_age_hours(path).is_some_and(|age_hours| age_hours <= max_age_hours)
        })
        .cloned()
        .or_else(|| first_existing_path(paths))
}

fn is_positive_finite_metric(value: Option<f64>) -> bool {
    value.is_some_and(|v| v.is_finite() && v > 0.0)
}

fn metric_state(value: Option<f64>) -> &'static str {
    match value {
        Some(v) if v.is_finite() && v > 0.0 => "valid",
        Some(v) if !v.is_finite() => "non_finite",
        Some(_) => "non_positive",
        None => "missing_or_non_numeric",
    }
}

const fn required_bool_state(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "missing_or_non_boolean",
    }
}

fn collect_full_e2e_rows(payload: &Value) -> Vec<&Value> {
    payload
        .get("layers")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |rows| {
            rows.iter()
                .filter(|row| {
                    matches!(
                        row.get("layer_id").and_then(Value::as_str),
                        Some("full_e2e_long_session")
                    )
                })
                .collect::<Vec<_>>()
        })
}

fn duplicate_full_e2e_failure(path: &Path, full_e2e_count: usize) -> Option<DataContractFailure> {
    (full_e2e_count > 1).then(|| DataContractFailure {
        contract_id: "missing_required_e2e_or_ratio_outputs".to_string(),
        budget_name: None,
        detail: format!(
            "duplicate full_e2e_long_session layers found (count={full_e2e_count}) in {}",
            path.display()
        ),
        remediation:
            "Emit exactly one full_e2e_long_session layer in extension_benchmark_stratification."
                .to_string(),
    })
}

fn required_e2e_metric_failure(
    path: &Path,
    full_e2e: Option<&Value>,
) -> Option<DataContractFailure> {
    let absolute_value = full_e2e
        .and_then(|row| row.pointer("/absolute_metrics/value"))
        .and_then(Value::as_f64);
    let node_ratio_value = full_e2e
        .and_then(|row| row.pointer("/relative_metrics/rust_vs_node_ratio"))
        .and_then(Value::as_f64);
    let bun_ratio_value = full_e2e
        .and_then(|row| row.pointer("/relative_metrics/rust_vs_bun_ratio"))
        .and_then(Value::as_f64);

    let absolute_valid = is_positive_finite_metric(absolute_value);
    let node_ratio_valid = is_positive_finite_metric(node_ratio_value);
    let bun_ratio_valid = is_positive_finite_metric(bun_ratio_value);

    (!absolute_valid || !node_ratio_valid || !bun_ratio_valid).then(|| DataContractFailure {
        contract_id: "missing_required_e2e_or_ratio_outputs".to_string(),
        budget_name: None,
        detail: format!(
            "full_e2e_long_session evidence has invalid required values (absolute_metrics.value={}, rust_vs_node_ratio={}, rust_vs_bun_ratio={}) in {}",
            metric_state(absolute_value),
            metric_state(node_ratio_value),
            metric_state(bun_ratio_value),
            path.display()
        ),
        remediation:
            "Emit full_e2e_long_session absolute latency and Rust-vs-Node/Bun ratios as finite positive numbers."
                .to_string(),
    })
}

fn bun_killer_ratio_release_gate_failure(
    path: &Path,
    full_e2e: Option<&Value>,
) -> Option<DataContractFailure> {
    let bun_ratio_value = full_e2e
        .and_then(|row| row.pointer("/relative_metrics/rust_vs_bun_ratio"))
        .and_then(Value::as_f64);
    let bun_ratio_value = bun_ratio_value?;
    if !is_positive_finite_metric(Some(bun_ratio_value)) {
        // Non-positive/non-finite values are handled by required_e2e_metric_failure.
        return None;
    }
    (bun_ratio_value > BUN_KILLER_MAX_RUST_VS_BUN_RATIO).then(|| DataContractFailure {
        contract_id: "bun_killer_ratio_release_gate".to_string(),
        budget_name: None,
        detail: format!(
            "full_e2e_long_session rust_vs_bun_ratio={bun_ratio_value:.6} exceeds Bun-killer release gate <= {:.2} in {}",
            BUN_KILLER_MAX_RUST_VS_BUN_RATIO,
            path.display()
        ),
        remediation: format!(
            "Reduce full_e2e_long_session rust_vs_bun_ratio to <= {BUN_KILLER_MAX_RUST_VS_BUN_RATIO:.2} before release promotion."
        ),
    })
}

fn claim_integrity_guard_failure(path: &Path, payload: &Value) -> Option<DataContractFailure> {
    let global_claim_valid = payload
        .pointer("/claim_integrity/cherry_pick_guard/global_claim_valid")
        .and_then(Value::as_bool);
    let full_e2e_layer_coverage = payload
        .pointer("/claim_integrity/cherry_pick_guard/layer_coverage/full_e2e_long_session")
        .and_then(Value::as_bool);

    (global_claim_valid != Some(true) || full_e2e_layer_coverage != Some(true)).then(|| {
        DataContractFailure {
            contract_id: "invalid_claim_integrity_guard".to_string(),
            budget_name: None,
            detail: format!(
                "claim_integrity.cherry_pick_guard requires global_claim_valid=true and layer_coverage.full_e2e_long_session=true (global_claim_valid={}, full_e2e_layer_coverage={}) in {}",
                required_bool_state(global_claim_valid),
                required_bool_state(full_e2e_layer_coverage),
                path.display()
            ),
            remediation:
                "Emit claim_integrity.cherry_pick_guard.global_claim_valid=true and layer_coverage.full_e2e_long_session=true for valid global claims."
                    .to_string(),
        }
    })
}

fn microbench_only_claim_failure(path: &Path, payload: &Value) -> Option<DataContractFailure> {
    let invalidity_reasons = payload
        .pointer("/claim_integrity/cherry_pick_guard/invalidity_reasons")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        });

    invalidity_reasons
        .iter()
        .any(|reason| reason == "microbench_only_claim")
        .then(|| DataContractFailure {
            contract_id: "microbench_only_claim".to_string(),
            budget_name: None,
            detail: format!(
                "claim_integrity.cherry_pick_guard.invalidity_reasons contains microbench_only_claim in {}",
                path.display()
            ),
            remediation: "Provide full E2E matrix evidence before making global performance claims."
                .to_string(),
        })
}

#[allow(clippy::too_many_lines)]
fn evaluate_phase1_weighted_attribution_contract(
    root: &Path,
    max_age_hours: f64,
) -> Vec<DataContractFailure> {
    let mut failures = Vec::new();
    let candidates = phase1_matrix_validation_candidates(root);
    if let Some(detail) = evaluate_artifact_contract(root, &candidates, max_age_hours) {
        failures.push(DataContractFailure {
            contract_id: "missing_or_stale_phase1_matrix_validation_evidence".to_string(),
            budget_name: None,
            detail,
            remediation: "Generate fresh phase1_matrix_validation.json in the current perf run."
                .to_string(),
        });
        return failures;
    }

    let Some(path) = first_existing_path(&candidates) else {
        failures.push(DataContractFailure {
            contract_id: "invalid_phase1_matrix_validation_contract".to_string(),
            budget_name: None,
            detail: "phase1 matrix validation artifact not found".to_string(),
            remediation: "Emit phase1_matrix_validation.json before evaluating perf budgets."
                .to_string(),
        });
        return failures;
    };

    let Some(payload) = read_json_file(&path) else {
        failures.push(DataContractFailure {
            contract_id: "invalid_phase1_matrix_validation_contract".to_string(),
            budget_name: None,
            detail: format!("failed to parse JSON at {}", path.display()),
            remediation: "Write valid JSON for phase1_matrix_validation artifact.".to_string(),
        });
        return failures;
    };

    let matrix_schema = payload.get("schema").and_then(Value::as_str);
    if matrix_schema != Some("pi.perf.phase1_matrix_validation.v1") {
        failures.push(DataContractFailure {
            contract_id: "invalid_phase1_matrix_validation_contract".to_string(),
            budget_name: None,
            detail: format!(
                "phase1 matrix schema must be pi.perf.phase1_matrix_validation.v1 (observed={}) in {}",
                matrix_schema.unwrap_or("missing_or_non_string"),
                path.display()
            ),
            remediation:
                "Set phase1_matrix_validation.schema to pi.perf.phase1_matrix_validation.v1."
                    .to_string(),
        });
    }

    let Some(weighted) = payload
        .get("weighted_bottleneck_attribution")
        .and_then(Value::as_object)
    else {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "phase1_matrix_validation.weighted_bottleneck_attribution must be an object in {}",
                path.display()
            ),
            remediation:
                "Emit weighted_bottleneck_attribution object with schema/status/lineage and outputs."
                    .to_string(),
        });
        return failures;
    };

    let weighted_schema = weighted.get("schema").and_then(Value::as_str);
    if weighted_schema != Some("pi.perf.phase1_weighted_bottleneck_attribution.v1") {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.schema must be pi.perf.phase1_weighted_bottleneck_attribution.v1 (observed={}) in {}",
                weighted_schema.unwrap_or("missing_or_non_string"),
                path.display()
            ),
            remediation:
                "Set weighted_bottleneck_attribution.schema to pi.perf.phase1_weighted_bottleneck_attribution.v1."
                    .to_string(),
        });
    }

    let weighted_status = weighted.get("status").and_then(Value::as_str);
    if !matches!(weighted_status, Some("computed" | "missing")) {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.status must be one of computed/missing (observed={}) in {}",
                weighted_status.unwrap_or("missing_or_non_string"),
                path.display()
            ),
            remediation:
                "Set weighted_bottleneck_attribution.status to computed or missing.".to_string(),
        });
    }

    let per_scale = weighted.get("per_scale").and_then(Value::as_array);
    if per_scale.is_none() {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.per_scale must be an array in {}",
                path.display()
            ),
            remediation:
                "Emit weighted_bottleneck_attribution.per_scale as an array (empty only when status=missing)."
                    .to_string(),
        });
    }

    let global_ranking = weighted.get("global_ranking").and_then(Value::as_array);
    if global_ranking.is_none() {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.global_ranking must be an array in {}",
                path.display()
            ),
            remediation:
                "Emit weighted_bottleneck_attribution.global_ranking as an array (empty only when status=missing)."
                    .to_string(),
        });
    }

    let Some(lineage) = weighted.get("lineage").and_then(Value::as_object) else {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.lineage must be an object in {}",
                path.display()
            ),
            remediation:
                "Emit weighted_bottleneck_attribution.lineage with source_cell_count and valid_cell_count."
                    .to_string(),
        });
        return failures;
    };

    let source_cell_count = lineage.get("source_cell_count").and_then(Value::as_u64);
    let valid_cell_count = lineage.get("valid_cell_count").and_then(Value::as_u64);

    if source_cell_count.is_none() || valid_cell_count.is_none() {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.lineage requires integer source_cell_count and valid_cell_count in {}",
                path.display()
            ),
            remediation:
                "Emit integer lineage.source_cell_count and lineage.valid_cell_count.".to_string(),
        });
        return failures;
    }

    let source_cell_count = source_cell_count.unwrap_or_default();
    let valid_cell_count = valid_cell_count.unwrap_or_default();
    if valid_cell_count > source_cell_count {
        failures.push(DataContractFailure {
            contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
            budget_name: None,
            detail: format!(
                "weighted_bottleneck_attribution.lineage.valid_cell_count ({valid_cell_count}) must be <= source_cell_count ({source_cell_count}) in {}",
                path.display()
            ),
            remediation:
                "Correct weighted_bottleneck_attribution.lineage counts to preserve valid<=source."
                    .to_string(),
        });
    }

    if let Some(matrix_cells) = payload.get("matrix_cells").and_then(Value::as_array) {
        let observed_source = matrix_cells.len() as u64;
        if source_cell_count != observed_source {
            failures.push(DataContractFailure {
                contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
                budget_name: None,
                detail: format!(
                    "weighted_bottleneck_attribution.lineage.source_cell_count ({source_cell_count}) must equal phase1_matrix_validation.matrix_cells length ({observed_source}) in {}",
                    path.display()
                ),
                remediation:
                    "Align weighted_bottleneck_attribution.lineage.source_cell_count with matrix_cells length."
                        .to_string(),
            });
        }
    }

    let per_scale_len = per_scale.map_or(0, Vec::len);
    let global_ranking_len = global_ranking.map_or(0, Vec::len);
    match weighted_status {
        Some("missing")
            if valid_cell_count != 0 || per_scale_len != 0 || global_ranking_len != 0 =>
        {
            failures.push(DataContractFailure {
                contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
                budget_name: None,
                detail: format!(
                    "weighted_bottleneck_attribution.status=missing requires lineage.valid_cell_count=0 and empty per_scale/global_ranking (observed valid_cell_count={valid_cell_count}, per_scale={per_scale_len}, global_ranking={global_ranking_len}) in {}",
                    path.display()
                ),
                remediation:
                    "When status=missing, set lineage.valid_cell_count=0 and emit empty per_scale/global_ranking arrays."
                        .to_string(),
            });
        }
        Some("computed")
            if valid_cell_count == 0 || per_scale_len == 0 || global_ranking_len == 0 =>
        {
            failures.push(DataContractFailure {
                contract_id: "invalid_weighted_bottleneck_attribution_contract".to_string(),
                budget_name: None,
                detail: format!(
                    "weighted_bottleneck_attribution.status=computed requires lineage.valid_cell_count>0 and non-empty per_scale/global_ranking (observed valid_cell_count={valid_cell_count}, per_scale={per_scale_len}, global_ranking={global_ranking_len}) in {}",
                    path.display()
                ),
                remediation:
                    "When status=computed, ensure lineage.valid_cell_count>0 with populated per_scale/global_ranking outputs."
                        .to_string(),
            });
        }
        _ => {}
    }

    failures
}

fn evaluate_required_e2e_ratio_contract(
    root: &Path,
    max_age_hours: f64,
) -> Vec<DataContractFailure> {
    let mut failures = Vec::new();
    let candidates = extension_stratification_candidates(root);
    if let Some(detail) = evaluate_artifact_contract(root, &candidates, max_age_hours) {
        failures.push(DataContractFailure {
            contract_id: "missing_or_stale_e2e_matrix_evidence".to_string(),
            budget_name: None,
            detail,
            remediation:
                "Generate fresh extension_benchmark_stratification.json in the current perf run."
                    .to_string(),
        });
        return failures;
    }

    let Some(path) = first_existing_path(&candidates) else {
        failures.push(DataContractFailure {
            contract_id: "missing_required_e2e_or_ratio_outputs".to_string(),
            budget_name: None,
            detail: "extension benchmark stratification artifact not found".to_string(),
            remediation:
                "Emit extension_benchmark_stratification.json before evaluating perf budgets."
                    .to_string(),
        });
        return failures;
    };

    let Some(payload) = read_json_file(&path) else {
        failures.push(DataContractFailure {
            contract_id: "invalid_e2e_matrix_evidence".to_string(),
            budget_name: None,
            detail: format!("failed to parse JSON at {}", path.display()),
            remediation: "Write valid JSON for extension_benchmark_stratification artifact."
                .to_string(),
        });
        return failures;
    };

    let full_e2e_rows = collect_full_e2e_rows(&payload);
    if let Some(failure) = duplicate_full_e2e_failure(&path, full_e2e_rows.len()) {
        failures.push(failure);
    }
    if let Some(failure) = required_e2e_metric_failure(&path, full_e2e_rows.first().copied()) {
        failures.push(failure);
    }
    if let Some(failure) =
        bun_killer_ratio_release_gate_failure(&path, full_e2e_rows.first().copied())
    {
        failures.push(failure);
    }
    if let Some(failure) = claim_integrity_guard_failure(&path, &payload) {
        failures.push(failure);
    }
    if let Some(failure) = microbench_only_claim_failure(&path, &payload) {
        failures.push(failure);
    }

    failures
}

fn context_intelligence_metric_value(payload: &Value, metric_key: &str) -> Option<f64> {
    let metric = payload
        .get("metrics")
        .and_then(Value::as_object)?
        .get(metric_key)?;
    ["p95_ms", "value_ms", "bytes", "value"]
        .into_iter()
        .find_map(|field| metric.get(field).and_then(Value::as_f64))
}

fn required_non_empty_string(payload: &Value, pointer: &str) -> bool {
    payload
        .pointer(pointer)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn context_intelligence_failure(
    contract_id: &str,
    budget_name: Option<&str>,
    detail: impl Into<String>,
    remediation: &str,
) -> DataContractFailure {
    DataContractFailure {
        contract_id: contract_id.to_string(),
        budget_name: budget_name.map(str::to_string),
        detail: detail.into(),
        remediation: remediation.to_string(),
    }
}

fn load_context_intelligence_budget_payload(
    root: &Path,
    max_age_hours: f64,
) -> Result<(PathBuf, Value), DataContractFailure> {
    let candidates = context_intelligence_budget_candidate_paths(root);
    if let Some(detail) = evaluate_artifact_contract(root, &candidates, max_age_hours) {
        return Err(context_intelligence_failure(
            "missing_or_stale_context_intelligence_budget_evidence",
            None,
            detail,
            "Generate fresh context_intelligence_planner_budget.json in the current perf run.",
        ));
    }

    let Some(path) = first_fresh_existing_path(&candidates, max_age_hours) else {
        return Err(context_intelligence_failure(
            "invalid_context_intelligence_budget_contract",
            None,
            "context intelligence budget artifact not found",
            "Emit context_intelligence_planner_budget.json before evaluating perf budgets.",
        ));
    };

    let Some(payload) = read_json_file(&path) else {
        return Err(context_intelligence_failure(
            "invalid_context_intelligence_budget_contract",
            None,
            format!("failed to parse JSON at {}", path.display()),
            "Write valid JSON for context intelligence perf evidence.",
        ));
    };

    Ok((path, payload))
}

fn validate_context_intelligence_schema(
    failures: &mut Vec<DataContractFailure>,
    path: &Path,
    payload: &Value,
) {
    let schema = payload.get("schema").and_then(Value::as_str);
    if schema != Some(CONTEXT_INTELLIGENCE_PERF_SCHEMA) {
        failures.push(context_intelligence_failure(
            "invalid_context_intelligence_budget_contract",
            None,
            format!(
                "context intelligence budget schema must be {CONTEXT_INTELLIGENCE_PERF_SCHEMA} (observed={}) in {}",
                schema.unwrap_or("missing_or_non_string"),
                path.display()
            ),
            "Set context_intelligence_planner_budget.schema to the versioned perf contract.",
        ));
    }
}

fn validate_context_intelligence_environment(
    failures: &mut Vec<DataContractFailure>,
    path: &Path,
    payload: &Value,
) {
    for pointer in [
        "/environment/cargo_target_dir",
        "/environment/tmpdir",
        "/host/os",
        "/host/arch",
    ] {
        if !required_non_empty_string(payload, pointer) {
            failures.push(context_intelligence_failure(
                "invalid_context_intelligence_budget_contract",
                None,
                format!(
                    "context intelligence budget artifact missing non-empty {pointer} in {}",
                    path.display()
                ),
                "Emit CARGO_TARGET_DIR/TMPDIR and host fingerprint fields in the budget artifact.",
            ));
        }
    }
}

fn validate_context_intelligence_determinism(
    failures: &mut Vec<DataContractFailure>,
    path: &Path,
    payload: &Value,
) {
    let randomized_checked = payload
        .pointer("/determinism/randomized_file_order_checked")
        .and_then(Value::as_bool);
    let deterministic_match = payload
        .pointer("/determinism/matched")
        .and_then(Value::as_bool);
    if randomized_checked != Some(true) || deterministic_match != Some(true) {
        failures.push(context_intelligence_failure(
            "invalid_context_intelligence_determinism_contract",
            None,
            format!(
                "determinism requires randomized_file_order_checked=true and matched=true (randomized_file_order_checked={}, matched={}) in {}",
                required_bool_state(randomized_checked),
                required_bool_state(deterministic_match),
                path.display()
            ),
            "Replay the synthetic large workspace with randomized file order and record a matching bundle summary.",
        ));
    }
}

fn validate_context_intelligence_cache(
    failures: &mut Vec<DataContractFailure>,
    path: &Path,
    payload: &Value,
) {
    for field in CONTEXT_INTELLIGENCE_CACHE_FIELDS {
        let pointer = format!("/cache_hit_miss/{field}");
        if !required_non_empty_string(payload, &pointer) {
            failures.push(context_intelligence_failure(
                "invalid_context_intelligence_cache_contract",
                None,
                format!(
                    "context intelligence budget artifact missing non-empty cache_hit_miss.{field} in {}",
                    path.display()
                ),
                "Record cold, warm, and incremental cache hit/miss reasons in the budget artifact.",
            ));
        }
    }
}

fn validate_context_intelligence_metrics(
    failures: &mut Vec<DataContractFailure>,
    path: &Path,
    payload: &Value,
) {
    for &(budget_name, metric_key) in CONTEXT_INTELLIGENCE_BUDGET_METRICS {
        let metric_value = context_intelligence_metric_value(payload, metric_key);
        if !is_positive_finite_metric(metric_value) {
            failures.push(context_intelligence_failure(
                "invalid_context_intelligence_budget_metric",
                Some(budget_name),
                format!(
                    "context intelligence metric {metric_key} is {} in {}",
                    metric_state(metric_value),
                    path.display()
                ),
                "Emit every context-intelligence budget metric as a finite positive number.",
            ));
        }
    }
}

fn evaluate_context_intelligence_budget_contract(
    root: &Path,
    max_age_hours: f64,
) -> Vec<DataContractFailure> {
    let mut failures = Vec::new();
    let (path, payload) = match load_context_intelligence_budget_payload(root, max_age_hours) {
        Ok(payload) => payload,
        Err(failure) => return vec![failure],
    };

    validate_context_intelligence_schema(&mut failures, &path, &payload);
    validate_context_intelligence_environment(&mut failures, &path, &payload);
    validate_context_intelligence_determinism(&mut failures, &path, &payload);
    validate_context_intelligence_cache(&mut failures, &path, &payload);
    validate_context_intelligence_metrics(&mut failures, &path, &payload);
    failures
}

fn collect_data_contract_failures(root: &Path) -> Vec<DataContractFailure> {
    let max_age_hours = max_artifact_age_hours();
    let mut failures = Vec::new();

    for budget in BUDGETS.iter().filter(|budget| budget.ci_enforced) {
        if matches!(
            budget.name,
            "tool_call_latency_mean" | "tool_call_throughput_min"
        ) {
            // PiJS selects one canonical artifact by precedence. Its dedicated
            // contract below binds freshness and parsing to that exact source.
            continue;
        }
        let candidates = budget_artifact_candidates(root, budget.name);
        if candidates.is_empty() {
            continue;
        }
        if let Some(detail) = evaluate_artifact_contract(root, &candidates, max_age_hours) {
            failures.push(DataContractFailure {
                contract_id: "missing_or_stale_budget_artifact".to_string(),
                budget_name: Some(budget.name.to_string()),
                detail,
                remediation: "Regenerate benchmark artifacts in the same CI/perf run before evaluating budgets."
                    .to_string(),
            });
        }
    }

    failures.extend(evaluate_required_e2e_ratio_contract(root, max_age_hours));
    failures.extend(evaluate_phase1_weighted_attribution_contract(
        root,
        max_age_hours,
    ));
    failures.extend(evaluate_context_intelligence_budget_contract(
        root,
        max_age_hours,
    ));
    failures.extend(evaluate_pijs_workload_gate_contract(root, max_age_hours));
    for failure in &mut failures {
        failure.detail = canonicalize_diagnostic_text(root, &failure.detail);
    }
    failures
}

fn check_budget(budget: &Budget) -> BudgetResult {
    check_budget_with_strict(budget, perf_strict_mode())
}

fn check_budget_with_strict(budget: &Budget, strict: bool) -> BudgetResult {
    let root = project_root();
    check_budget_with_strict_at_root(budget, strict, &root)
}

fn check_budget_with_strict_at_root(budget: &Budget, strict: bool, root: &Path) -> BudgetResult {
    // Try to find actual measurement for this budget
    let (actual, source) = match budget.name {
        "tool_call_latency_mean" => read_pijs_workload_mean_latency(root),
        "tool_call_throughput_min" => read_pijs_workload_throughput(root),
        "ext_cold_load_simple_p95" => read_criterion_load_time(root, "hello"),
        "ext_cold_load_complex_p95" => read_criterion_load_time(root, "pirate"),
        "ext_load_60_total" => read_total_load_time(root),
        "sustained_load_rss_growth" => read_stress_rss_growth(root),
        "startup_version_p95" => read_criterion_startup(root, "version"),
        "startup_full_agent_p95" => read_criterion_startup(root, "help"),
        "event_dispatch_p99" => read_scenario_runner_per_call(root, "event_dispatch"),
        "context_graph_build_cold_p95" => read_context_intelligence_budget_metric(
            root,
            "context_graph_build_cold_p95",
            Some("graph_build_cold"),
        ),
        "context_graph_build_warm_p95" => read_context_intelligence_budget_metric(
            root,
            "context_graph_build_warm_p95",
            Some("graph_build_warm"),
        ),
        "context_incremental_update_p95" => read_context_intelligence_budget_metric(
            root,
            "context_incremental_update_p95",
            Some("incremental_update"),
        ),
        "context_planning_p95" => {
            read_context_intelligence_budget_metric(root, "context_planning_p95", Some("planning"))
        }
        "context_bundle_serialization_p95" => read_context_intelligence_budget_metric(
            root,
            "context_bundle_serialization_p95",
            Some("bundle_serialization"),
        ),
        "context_bundle_estimated_bytes_max" => read_context_intelligence_budget_metric(
            root,
            "context_bundle_estimated_bytes_max",
            None,
        ),
        "policy_eval_p99" => read_criterion_policy_eval(root),
        "idle_memory_rss" => read_idle_memory_rss(),
        "binary_size_release" => read_binary_size(root),
        "protocol_parse_p99" => read_criterion_protocol_parse(root),
        _ => (None, "no data source configured".to_string()),
    };

    let status = classify_budget_status(budget, actual, strict);
    let failure_reason = if status == "FAIL" && actual.is_none() && budget.ci_enforced && strict {
        Some("missing_measurement_data".to_string())
    } else {
        None
    };

    BudgetResult {
        budget_name: budget.name.to_string(),
        category: budget.category.to_string(),
        threshold: budget.threshold,
        comparison: budget.comparison,
        unit: budget.unit.to_string(),
        actual,
        status: status.to_string(),
        source,
        ci_enforced: budget.ci_enforced,
        failure_reason,
    }
}

fn require_pijs_string(record: &Value, field: &str, expected: &str) -> Result<(), String> {
    let observed = record.get(field).and_then(Value::as_str);
    if observed == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "{field} must equal {expected:?} (observed={observed:?})"
        ))
    }
}

fn require_pijs_perf_binary_path(record: &Value) -> Result<(), String> {
    let binary_path = record
        .get("binary_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "binary_path must be a non-empty string".to_string())?;
    let path = Path::new(binary_path);
    if !path.is_absolute() {
        return Err("binary_path must be absolute".to_string());
    }
    if path.file_stem().and_then(|name| name.to_str()) != Some("pijs_workload") {
        return Err(format!(
            "binary_path must identify the pijs_workload executable (observed={binary_path:?})"
        ));
    }
    let derived_profile = profile_from_target_path(path);
    if derived_profile.as_deref() != Some("perf") {
        return Err(format!(
            "binary_path must resolve to Cargo profile \"perf\" (observed={binary_path:?}, derived_profile={derived_profile:?})"
        ));
    }
    require_pijs_string(record, "executable_build_profile", "perf")?;
    let canonical_path = std::fs::canonicalize(path)
        .map_err(|err| format!("binary_path must resolve to an existing executable: {err}"))?;
    if canonical_path != path {
        return Err(format!(
            "binary_path must be canonical (observed={binary_path:?}, canonical={:?})",
            canonical_path.display().to_string()
        ));
    }
    let claimed_sha256 = record
        .get("binary_sha256")
        .and_then(Value::as_str)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "binary_sha256 must be a 64-character hexadecimal string".to_string())?;
    let observed_sha256 = sha256_file(path)
        .map_err(|err| format!("failed to hash binary_path {binary_path:?}: {err}"))?;
    if claimed_sha256 != observed_sha256 {
        return Err(format!(
            "binary_sha256 does not match binary_path (claimed={claimed_sha256}, observed={observed_sha256})"
        ));
    }
    Ok(())
}

fn validate_pijs_gate_record(record: &Value, expected_tool_calls: u64) -> Result<(), String> {
    for (field, expected) in [
        ("schema", "pi.perf.workload.v1"),
        ("tool", "pijs_workload"),
        ("scenario", "tool_call_roundtrip"),
        ("runtime_engine", "quickjs"),
        ("build_profile", "perf"),
        ("build_fingerprint_contract", BUILD_FINGERPRINT_CONTRACT),
        ("compiled_profile_family", "release"),
        ("compiled_opt_level", "3"),
        ("compiled_debug", "true"),
        ("evidence_class", "measured"),
        ("confidence", "high"),
        ("measurement_method", "wall_clock_observation"),
        ("measurement_boundary", "production_extension_manager"),
        (
            "measurement_contract_version",
            "production_extension_manager.v1",
        ),
        ("disk_cache_policy", "disabled"),
        ("host_page_cache_policy", "not_applicable_measured_region"),
        ("allocator_requested", "system"),
        ("allocator_request_source", "env"),
        ("allocator_effective", "system"),
    ] {
        require_pijs_string(record, field, expected)?;
    }

    if record
        .get("eligible_for_regression_gate")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("eligible_for_regression_gate must equal true".to_string());
    }
    if record
        .get("build_profile_verified")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("build_profile_verified must equal true".to_string());
    }
    for field in ["build_fingerprint_verified", "executable_profile_verified"] {
        if record.get(field).and_then(Value::as_bool) != Some(true) {
            return Err(format!("{field} must equal true"));
        }
    }
    if record.get("debug_assertions").and_then(Value::as_bool) != Some(false) {
        return Err("debug_assertions must equal false".to_string());
    }
    require_pijs_perf_binary_path(record)?;

    if !matches_canonical_perf_build_fingerprint(
        record
            .get("compiled_profile_family")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        record
            .get("compiled_opt_level")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        record
            .get("compiled_debug")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ) {
        return Err(
            "compiled Cargo settings do not match the canonical perf fingerprint".to_string(),
        );
    }
    let compiled_features = record
        .get("compiled_features")
        .and_then(Value::as_array)
        .ok_or_else(|| "compiled_features must be an array".to_string())?
        .iter()
        .map(|feature| {
            feature
                .as_str()
                .ok_or_else(|| "compiled_features entries must be strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !matches_canonical_pijs_perf_features(&compiled_features) {
        return Err(format!(
            "compiled_features must equal canonical shipping feature set {CANONICAL_PIJS_PERF_FEATURES:?} (observed={compiled_features:?})"
        ));
    }
    if record
        .get("allocator_fallback_reason")
        .is_some_and(|value| !value.is_null())
    {
        return Err(
            "allocator_fallback_reason must be null for the canonical system lane".to_string(),
        );
    }
    if record.get("source_dirty").and_then(Value::as_bool) != Some(false) {
        return Err("source_dirty must equal false".to_string());
    }
    let source_commit = record
        .get("source_commit")
        .and_then(Value::as_str)
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "source_commit must be a full 40-character Git SHA".to_string())?;
    if source_commit.bytes().all(|byte| byte == b'0') {
        return Err("source_commit must not be the all-zero Git SHA".to_string());
    }
    let run_id = record
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "run_id must be a non-empty string".to_string())?;
    let correlation_id = record
        .get("correlation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "correlation_id must be a non-empty string".to_string())?;
    if run_id != correlation_id {
        return Err("run_id and correlation_id must be identical".to_string());
    }
    let binary_path = record["binary_path"]
        .as_str()
        .expect("validated binary_path");
    let binary_sha256 = record["binary_sha256"]
        .as_str()
        .expect("validated binary_sha256");
    let expected_config_hash = benchmark_provenance_config_hash(&BenchmarkProvenance {
        source_commit,
        source_dirty: false,
        build_profile: "perf",
        executable_build_profile: "perf",
        verification: BenchmarkBuildVerification {
            executable_profile: true,
            build_fingerprint: true,
            build_profile: true,
        },
        build_fingerprint_contract: BUILD_FINGERPRINT_CONTRACT,
        compiled_profile_family: "release",
        compiled_opt_level: "3",
        compiled_debug: "true",
        compiled_features: &compiled_features,
        binary_path,
        binary_sha256,
        debug_assertions: false,
    });
    let claimed_config_hash = record
        .get("config_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| "config_hash must be a string".to_string())?;
    if claimed_config_hash != expected_config_hash {
        return Err(format!(
            "config_hash does not match asserted provenance (claimed={claimed_config_hash}, expected={expected_config_hash})"
        ));
    }

    let iterations = record
        .get("iterations")
        .and_then(Value::as_u64)
        .ok_or_else(|| "iterations must be an integer".to_string())?;
    if iterations != PIJS_REGRESSION_GATE_ITERATIONS {
        return Err(format!(
            "iterations must equal {PIJS_REGRESSION_GATE_ITERATIONS} (observed={iterations})"
        ));
    }
    let tool_calls = record
        .get("tool_calls_per_iteration")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "tool_calls_per_iteration must be a positive integer".to_string())?;
    if tool_calls != expected_tool_calls {
        return Err(format!(
            "tool_calls_per_iteration must equal {expected_tool_calls} (observed={tool_calls})"
        ));
    }
    let total_calls = record
        .get("total_calls")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "total_calls must be a positive integer".to_string())?;
    let expected_total = iterations
        .checked_mul(tool_calls)
        .ok_or_else(|| "iterations * tool_calls_per_iteration overflows u64".to_string())?;
    if total_calls != expected_total {
        return Err(format!(
            "total_calls must equal iterations * tool_calls_per_iteration ({expected_total}); observed={total_calls}"
        ));
    }
    Ok(())
}

fn require_positive_pijs_float(record: &Value, field: &str) -> Result<f64, String> {
    record
        .get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| format!("{field} must contain a finite positive metric"))
}

fn pijs_float_matches(claimed: f64, derived: f64) -> bool {
    let serialization_tolerance = derived.abs().max(1.0) * f64::EPSILON * 16.0;
    (claimed - derived).abs() <= serialization_tolerance
}

fn derive_and_validate_pijs_metrics(record: &Value) -> Result<(f64, f64), String> {
    let total_calls = record
        .get("total_calls")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "total_calls must be a positive integer".to_string())?;
    let elapsed_us = record
        .get("elapsed_us")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "elapsed_us must be a positive integer".to_string())?;
    let elapsed_us_f64 = require_positive_pijs_float(record, "elapsed_us_f64")?;
    if elapsed_us_f64.floor() != elapsed_us as f64 {
        return Err(format!(
            "elapsed_us must equal floor(elapsed_us_f64) (elapsed_us={elapsed_us}, elapsed_us_f64={elapsed_us_f64})"
        ));
    }

    let total_calls_f64 = total_calls as f64;
    let derived_mean_latency = elapsed_us_f64 / total_calls_f64;
    let claimed_mean_latency = require_positive_pijs_float(record, "per_call_us_f64")?;
    if !pijs_float_matches(claimed_mean_latency, derived_mean_latency) {
        return Err(format!(
            "per_call_us_f64 is inconsistent with elapsed_us_f64 / total_calls (claimed={claimed_mean_latency}, derived={derived_mean_latency})"
        ));
    }

    let claimed_integer_latency = record
        .get("per_call_us")
        .and_then(Value::as_u64)
        .ok_or_else(|| "per_call_us must be an integer".to_string())?;
    let expected_integer_latency = elapsed_us / total_calls;
    if claimed_integer_latency != expected_integer_latency {
        return Err(format!(
            "per_call_us must equal elapsed_us / total_calls with integer truncation ({expected_integer_latency}); observed={claimed_integer_latency}"
        ));
    }

    let claimed_throughput = record
        .get("calls_per_sec")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "calls_per_sec must be a positive integer".to_string())?;
    let expected_throughput = u128::from(total_calls)
        .checked_mul(1_000_000)
        .ok_or_else(|| "total_calls * 1_000_000 overflows u128".to_string())?
        / u128::from(elapsed_us);
    if u128::from(claimed_throughput) != expected_throughput {
        return Err(format!(
            "calls_per_sec must equal total_calls * 1_000_000 / elapsed_us with integer truncation ({expected_throughput}); observed={claimed_throughput}"
        ));
    }

    let derived_throughput = total_calls_f64 * 1_000_000.0 / elapsed_us_f64;
    Ok((derived_mean_latency, derived_throughput))
}

fn validate_pijs_timestamp(
    record: &Value,
    max_age_hours: f64,
) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let raw = record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "timestamp must be a non-empty RFC3339 string".to_string())?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|err| format!("timestamp must be valid RFC3339: {err}"))?
        .with_timezone(&chrono::Utc);
    let age = chrono::Utc::now().signed_duration_since(timestamp);
    if age < chrono::TimeDelta::minutes(-5) {
        return Err("timestamp is more than five minutes in the future".to_string());
    }
    let max_age_ms = max_age_hours * 60.0 * 60.0 * 1_000.0;
    if age.num_milliseconds() as f64 > max_age_ms {
        return Err(format!("timestamp is stale (maximum {max_age_hours:.2}h)"));
    }
    Ok(timestamp)
}

#[derive(Debug, Clone)]
struct ValidatedPijsGatePair {
    mean_latency_us: f64,
    throughput_calls_per_sec: f64,
}

fn validate_pijs_gate_pair(
    events: &[Value],
    max_age_hours: f64,
) -> Result<ValidatedPijsGatePair, String> {
    let mut admitted = Vec::new();
    for event in events.iter().filter(|event| {
        event
            .get("eligible_for_regression_gate")
            .and_then(Value::as_bool)
            == Some(true)
    }) {
        let tool_calls = event
            .get("tool_calls_per_iteration")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "eligible PiJS record tool_calls_per_iteration must be an integer".to_string()
            })?;
        if !matches!(tool_calls, 1 | 10) {
            return Err(format!(
                "eligible PiJS record uses unsupported tool_calls_per_iteration={tool_calls}"
            ));
        }
        validate_pijs_gate_record(event, tool_calls)?;
        let metrics = derive_and_validate_pijs_metrics(event)?;
        let timestamp = validate_pijs_timestamp(event, max_age_hours)?;
        admitted.push((tool_calls, event, metrics, timestamp));
    }

    if admitted.len() != 2 {
        return Err(format!(
            "PiJS regression gate requires exactly two eligible records (one 1-call lane and one 10-call lane); observed {}",
            admitted.len()
        ));
    }
    admitted.sort_by_key(|(tool_calls, ..)| *tool_calls);
    if admitted[0].0 != 1 || admitted[1].0 != 10 {
        return Err(
            "PiJS regression gate requires exactly one 1-call lane and one 10-call lane"
                .to_string(),
        );
    }

    let latency_record = admitted[0].1;
    let throughput_record = admitted[1].1;
    for field in [
        "run_id",
        "correlation_id",
        "source_commit",
        "binary_path",
        "binary_sha256",
        "build_fingerprint_contract",
        "config_hash",
        "compiled_profile_family",
        "compiled_opt_level",
        "compiled_debug",
        "allocator_requested",
        "allocator_effective",
    ] {
        if latency_record.get(field) != throughput_record.get(field) {
            return Err(format!("PiJS 1-call and 10-call lanes must share {field}"));
        }
    }
    if latency_record.get("compiled_features") != throughput_record.get("compiled_features") {
        return Err("PiJS 1-call and 10-call lanes must share compiled_features".to_string());
    }
    let timestamp_span = admitted[1].3.signed_duration_since(admitted[0].3).abs();
    if timestamp_span > chrono::TimeDelta::minutes(15) {
        return Err("PiJS lane timestamps must be within 15 minutes of one another".to_string());
    }

    Ok(ValidatedPijsGatePair {
        mean_latency_us: admitted[0].2.0,
        throughput_calls_per_sec: admitted[1].2.1,
    })
}

fn read_pijs_gate_pair(root: &Path, max_age_hours: f64) -> (Option<ValidatedPijsGatePair>, String) {
    let (events, source) = match load_pijs_workload_artifact(root) {
        PijsWorkloadArtifact::Missing => {
            return (None, "no pijs_workload data".to_string());
        }
        PijsWorkloadArtifact::Invalid { source, detail, .. } => {
            return (
                None,
                format!("invalid pijs_workload artifact {source}: {detail}"),
            );
        }
        PijsWorkloadArtifact::Loaded {
            path,
            source,
            events,
        } => {
            if let Err(detail) = validate_selected_pijs_freshness(&path, &source, max_age_hours) {
                return (None, detail);
            }
            (events, source)
        }
    };
    match validate_pijs_gate_pair(&events, max_age_hours) {
        Ok(pair) => (Some(pair), source),
        Err(detail) => (
            None,
            format!("no admissible pijs_workload pair in {source}: {detail}"),
        ),
    }
}

fn read_pijs_workload_mean_latency(root: &Path) -> (Option<f64>, String) {
    let (pair, source) = read_pijs_gate_pair(root, max_artifact_age_hours());
    (pair.map(|pair| pair.mean_latency_us), source)
}

fn read_pijs_workload_throughput(root: &Path) -> (Option<f64>, String) {
    let (pair, source) = read_pijs_gate_pair(root, max_artifact_age_hours());
    (pair.map(|pair| pair.throughput_calls_per_sec), source)
}

fn evaluate_pijs_workload_gate_contract(
    root: &Path,
    max_age_hours: f64,
) -> Vec<DataContractFailure> {
    let (contract_id, detail, remediation) = match load_pijs_workload_artifact(root) {
        PijsWorkloadArtifact::Missing => (
            "missing_or_stale_budget_artifact",
            format!(
                "missing artifacts; expected one of [{}]",
                format_path_list(root, &pijs_workload_candidate_paths(root))
            ),
            "Generate the canonical PiJS workload artifact in the current perf run.".to_string(),
        ),
        PijsWorkloadArtifact::Invalid { source, detail } => (
            "invalid_pijs_workload_artifact",
            format!("invalid selected artifact {source}: {detail}"),
            "Regenerate the selected PiJS JSONL artifact; every nonblank line must be valid JSON."
                .to_string(),
        ),
        PijsWorkloadArtifact::Loaded {
            path,
            source,
            events,
        } => {
            if let Err(detail) = validate_selected_pijs_freshness(&path, &source, max_age_hours) {
                (
                    "missing_or_stale_budget_artifact",
                    detail,
                    "Regenerate the selected PiJS workload artifact in the current perf run."
                        .to_string(),
                )
            } else if let Err(detail) = validate_pijs_gate_pair(&events, max_age_hours) {
                (
                    "ineligible_pijs_workload_artifact",
                    format!("no admissible PiJS pair in {source}: {detail}"),
                    format!(
                        "Generate one same-run pair of exactly {PIJS_REGRESSION_GATE_ITERATIONS}-iteration canonical perf-profile QuickJS measurements through the production extension manager."
                    ),
                )
            } else {
                return Vec::new();
            }
        }
    };

    ["tool_call_latency_mean", "tool_call_throughput_min"]
        .into_iter()
        .map(|budget_name| DataContractFailure {
            contract_id: contract_id.to_string(),
            budget_name: Some(budget_name.to_string()),
            detail: detail.clone(),
            remediation: remediation.clone(),
        })
        .collect()
}

#[derive(Debug)]
enum PijsWorkloadArtifact {
    Missing,
    Invalid {
        source: String,
        detail: String,
    },
    Loaded {
        path: PathBuf,
        source: String,
        events: Vec<Value>,
    },
}

fn load_pijs_workload_artifact(root: &Path) -> PijsWorkloadArtifact {
    for path in pijs_workload_candidate_paths(root) {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                let source = display_source_path(root, &path);
                return PijsWorkloadArtifact::Invalid {
                    source,
                    detail: format!("could not read selected artifact: {err}"),
                };
            }
        };
        let source = display_source_path(root, &path);
        let mut events = Vec::new();
        for (line_index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(event) => events.push(event),
                Err(err) => {
                    return PijsWorkloadArtifact::Invalid {
                        source,
                        detail: format!("line {} is not valid JSON: {err}", line_index + 1),
                    };
                }
            }
        }
        if events.is_empty() {
            return PijsWorkloadArtifact::Invalid {
                source,
                detail: "artifact contains no nonblank JSON records".to_string(),
            };
        }
        return PijsWorkloadArtifact::Loaded {
            path,
            source,
            events,
        };
    }
    PijsWorkloadArtifact::Missing
}

fn validate_selected_pijs_freshness(
    path: &Path,
    source: &str,
    max_age_hours: f64,
) -> Result<(), String> {
    match artifact_age_hours(path) {
        Some(age_hours) if age_hours <= max_age_hours => Ok(()),
        Some(_) => Err(format!(
            "selected artifact {source} is stale (maximum {max_age_hours:.2}h)"
        )),
        None => Err(format!(
            "selected artifact {source} has unavailable or invalid modification time"
        )),
    }
}

fn pijs_workload_candidate_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for dir in perf_evidence_dirs(root) {
        paths.extend(pijs_workload_candidate_paths_in_evidence_dir(&dir));
    }
    for dir in target_dir_candidates(root) {
        paths.extend(pijs_workload_candidate_paths_in_target_dir(&dir));
    }
    dedup_paths(paths)
}

fn pijs_workload_candidate_paths_in_target_dir(target_dir: &Path) -> Vec<PathBuf> {
    let perf_dir = target_dir.join("perf");
    [
        "perf/pijs_workload_perf.jsonl",
        "release/pijs_workload_release.jsonl",
        "debug/pijs_workload_debug.jsonl",
        "pijs_workload.jsonl",
        "results/pijs_workload.jsonl",
    ]
    .into_iter()
    .map(|relative| perf_dir.join(relative))
    .collect()
}

fn pijs_workload_candidate_paths_in_evidence_dir(evidence_dir: &Path) -> Vec<PathBuf> {
    dedup_paths(
        [
            "pijs_workload_perf.jsonl",
            "pijs_workload_release.jsonl",
            "pijs_workload_debug.jsonl",
            "pijs_workload.jsonl",
            "results/pijs_workload.jsonl",
            "perf/pijs_workload_perf.jsonl",
            "perf/pijs_workload_release.jsonl",
            "perf/pijs_workload_debug.jsonl",
            "perf/pijs_workload.jsonl",
            "perf/results/pijs_workload.jsonl",
        ]
        .into_iter()
        .map(|relative| evidence_dir.join(relative))
        .collect(),
    )
}

fn read_criterion_load_time(root: &Path, ext: &str) -> (Option<f64>, String) {
    // Criterion stores results in target/criterion/<group>/<bench>/new/estimates.json
    let relative = format!("criterion/ext_load_init/load_init_cold/{ext}/new/estimates.json");
    for path in criterion_estimate_candidate_paths(root, &relative) {
        if let Some(estimates) = read_json_file(&path)
            && let Some(mean_ns) = estimates
                .get("mean")
                .and_then(|m| m.get("point_estimate"))
                .and_then(Value::as_f64)
        {
            let ms = mean_ns / 1_000_000.0;
            return (Some(ms), display_source_path(root, &path));
        }
    }
    (None, format!("no criterion data for {ext}"))
}

fn read_total_load_time(root: &Path) -> (Option<f64>, String) {
    let path = root.join("tests/ext_conformance/reports/load_time_benchmark.json");
    if let Some(report) = read_json_file(&path)
        && let Some(results) = report.get("results").and_then(Value::as_array)
    {
        let total_ms: f64 = results
            .iter()
            .filter_map(|r| {
                r.get("rust")
                    .and_then(|rust| rust.get("load_time_ms"))
                    .and_then(Value::as_f64)
            })
            .sum();
        return (
            Some(total_ms),
            "load_time_benchmark.json (sum of Rust load times)".to_string(),
        );
    }
    (None, "no load time benchmark data".to_string())
}

fn read_stress_rss_growth(root: &Path) -> (Option<f64>, String) {
    let mut candidate_paths = evidence_then_target_paths(
        root,
        &[
            "stress_triage.json",
            "results/stress_triage.json",
            "perf/stress_triage.json",
            "perf/results/stress_triage.json",
        ],
        &["perf/stress_triage.json", "perf/results/stress_triage.json"],
    );
    candidate_paths.push(root.join("tests/perf/reports/stress_triage.json"));
    let candidate_paths = dedup_paths(candidate_paths);

    for path in candidate_paths {
        if let Some(triage) = read_json_file(&path) {
            let pct = triage
                .get("rss_growth_pct")
                .and_then(Value::as_f64)
                .or_else(|| {
                    triage
                        .get("results")
                        .and_then(|results| results.get("rss"))
                        .and_then(|rss| rss.get("growth_pct"))
                        .and_then(Value::as_f64)
                });

            if let Some(value) = pct {
                let normalized_percent = if value <= 1.0 { value * 100.0 } else { value };
                return (Some(normalized_percent), display_source_path(root, &path));
            }
        }
    }
    (None, "no stress test data".to_string())
}

// ─── New Data Readers (bd-20s9) ──────────────────────────────────────────────

fn read_criterion_startup(root: &Path, subcommand: &str) -> (Option<f64>, String) {
    // Criterion stores startup benchmarks at target/criterion/startup/<subcommand>/warm/new/estimates.json
    let relative = format!("criterion/startup/{subcommand}/warm/new/estimates.json");
    for path in criterion_estimate_candidate_paths(root, &relative) {
        if let Some(estimates) = read_json_file(&path)
            && let Some(mean_ns) = estimates
                .get("mean")
                .and_then(|m| m.get("point_estimate"))
                .and_then(Value::as_f64)
        {
            let ms = mean_ns / 1_000_000.0;
            return (Some(ms), display_source_path(root, &path));
        }
    }
    (None, format!("no criterion data for startup/{subcommand}"))
}

fn read_criterion_context_intelligence(root: &Path, bench_name: &str) -> (Option<f64>, String) {
    for path in context_criterion_estimate_candidate_paths(root, bench_name) {
        if let Some(estimates) = read_json_file(&path)
            && let Some(mean_ns) = estimates
                .get("mean")
                .and_then(|m| m.get("point_estimate"))
                .and_then(Value::as_f64)
        {
            return (
                Some(mean_ns / 1_000_000.0),
                display_source_path(root, &path),
            );
        }
    }
    (
        None,
        format!("no criterion data for semantic_context/{bench_name}/{CONTEXT_BENCH_CASE}"),
    )
}

fn read_context_intelligence_budget_metric(
    root: &Path,
    budget_name: &str,
    criterion_bench_name: Option<&str>,
) -> (Option<f64>, String) {
    let Some(metric_key) = context_intelligence_budget_metric_key(budget_name) else {
        return (
            None,
            format!("no context intelligence metric key for {budget_name}"),
        );
    };
    for path in context_intelligence_budget_candidate_paths(root) {
        let Some(payload) = read_json_file(&path) else {
            continue;
        };
        if payload.get("schema").and_then(Value::as_str) != Some(CONTEXT_INTELLIGENCE_PERF_SCHEMA) {
            continue;
        }
        if let Some(value) = context_intelligence_metric_value(&payload, metric_key) {
            return (Some(value), display_source_path(root, &path));
        }
    }

    criterion_bench_name.map_or_else(
        || {
            (
                None,
                format!("no context intelligence budget artifact metric {metric_key}"),
            )
        },
        |bench_name| read_criterion_context_intelligence(root, bench_name),
    )
}

fn read_scenario_runner_per_call(root: &Path, scenario: &str) -> (Option<f64>, String) {
    let candidates = evidence_then_target_paths(
        root,
        &[
            "scenario_runner.jsonl",
            "results/scenario_runner.jsonl",
            "perf/scenario_runner.jsonl",
            "perf/results/scenario_runner.jsonl",
        ],
        &[
            "perf/scenario_runner.jsonl",
            "perf/results/scenario_runner.jsonl",
        ],
    );
    // Find the worst (max) per_call_us across all extensions for this scenario.
    let mut max_us: Option<f64> = None;
    let mut source: Option<String> = None;
    for path in candidates {
        for event in read_jsonl_file(&path) {
            if event.get("scenario").and_then(Value::as_str) != Some(scenario) {
                continue;
            }
            if let Some(us) = event.get("per_call_us").and_then(Value::as_f64) {
                max_us = Some(max_us.map_or(us, |prev: f64| prev.max(us)));
                source.get_or_insert_with(|| display_source_path(root, &path));
            }
        }
    }
    let source = source.unwrap_or_else(|| format!("no scenario_runner data for {scenario}"));
    if let Some(us) = max_us {
        (Some(us), source)
    } else {
        (None, source)
    }
}

fn read_criterion_policy_eval(root: &Path) -> (Option<f64>, String) {
    // Policy eval benchmarks: target/criterion/ext_policy/evaluate/*/new/estimates.json
    // Take the worst (max) across all policy variants, convert ns → ns.
    let mut max_ns: Option<f64> = None;
    for path in collect_estimate_json_files_from_bases(&criterion_base_candidates(
        root,
        "criterion/ext_policy/evaluate",
    )) {
        if let Some(estimates) = read_json_file(&path)
            && let Some(mean_ns) = estimates
                .get("mean")
                .and_then(|m| m.get("point_estimate"))
                .and_then(Value::as_f64)
        {
            max_ns = Some(max_ns.map_or(mean_ns, |prev: f64| prev.max(mean_ns)));
        }
    }
    max_ns.map_or_else(
        || (None, "no criterion data for policy eval".to_string()),
        |ns| (Some(ns), "criterion: ext_policy/evaluate (max)".to_string()),
    )
}

fn read_idle_memory_rss() -> (Option<f64>, String) {
    (
        None,
        "no canonical idle Pi RSS artifact; test-harness process RSS is inadmissible".to_string(),
    )
}

fn read_binary_size(root: &Path) -> (Option<f64>, String) {
    for path in binary_size_candidate_paths(root) {
        if let Ok(meta) = std::fs::metadata(&path) {
            let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
            let source = display_source_path(root, &path);
            return (Some(size_mb), source);
        }
    }
    (None, "no candidate pi binary found".to_string())
}

fn read_criterion_protocol_parse(root: &Path) -> (Option<f64>, String) {
    // Protocol parse: target/criterion/ext_protocol/parse_and_validate/*/new/estimates.json
    // Take the worst (max) across variants, convert ns → us.
    let mut max_us: Option<f64> = None;
    for path in collect_estimate_json_files_from_bases(&criterion_base_candidates(
        root,
        "criterion/ext_protocol/parse_and_validate",
    )) {
        if let Some(estimates) = read_json_file(&path)
            && let Some(mean_ns) = estimates
                .get("mean")
                .and_then(|m| m.get("point_estimate"))
                .and_then(Value::as_f64)
        {
            let us = mean_ns / 1000.0;
            max_us = Some(max_us.map_or(us, |prev: f64| prev.max(us)));
        }
    }
    max_us.map_or_else(
        || (None, "no criterion data for protocol parse".to_string()),
        |us| {
            (
                Some(us),
                "criterion: ext_protocol/parse_and_validate (max)".to_string(),
            )
        },
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn target_dir_resolution_honors_cargo_target_dir_shape() {
    let root = Path::new("/workspace/pi_agent_rust");

    assert_eq!(resolve_target_dir(root, None), root.join("target"));
    assert_eq!(
        resolve_target_dir(root, Some(std::ffi::OsStr::new("target/sunnybeacon"))),
        root.join("target/sunnybeacon")
    );
    assert_eq!(
        resolve_target_dir(
            root,
            Some(std::ffi::OsStr::new(
                "/data/tmp/pi_agent_rust_cargo/sunnybeacon/target"
            ))
        ),
        PathBuf::from("/data/tmp/pi_agent_rust_cargo/sunnybeacon/target")
    );
}

#[test]
fn explicit_target_dir_is_authoritative_and_fixture_roots_are_hermetic() {
    let project = Path::new("/workspace/pi_agent_rust");
    let explicit = std::ffi::OsStr::new("/data/tmp/pi-release-target");
    assert_eq!(
        target_dir_candidates_for(project, project, Some(explicit)),
        vec![PathBuf::from("/data/tmp/pi-release-target")],
        "an explicit Cargo target must not fall through to ignored repo-local artifacts"
    );

    let fixture = Path::new("/tmp/pi-budget-fixture");
    assert_eq!(
        target_dir_candidates_for(fixture, project, Some(explicit)),
        vec![fixture.join("target")],
        "fixture evaluations must not inherit the real project's target directory"
    );
}

#[test]
fn pijs_workload_candidates_follow_resolved_target_dir() {
    let root = Path::new("/workspace/pi_agent_rust");
    let candidates = pijs_workload_candidate_paths_in_target_dir(&resolve_target_dir(root, None));

    assert_eq!(
        candidates[0],
        root.join("target/perf/perf/pijs_workload_perf.jsonl")
    );
    assert_eq!(candidates[3], root.join("target/perf/pijs_workload.jsonl"));
    assert_eq!(
        candidates[4],
        root.join("target/perf/results/pijs_workload.jsonl")
    );
}

#[test]
fn pijs_workload_candidates_accept_staged_evidence_dir_layout() {
    let evidence_dir = Path::new("/workspace/pi_agent_rust/tests/perf/reports/staged");
    let candidates = pijs_workload_candidate_paths_in_evidence_dir(evidence_dir);

    assert_eq!(candidates[0], evidence_dir.join("pijs_workload_perf.jsonl"));
    assert_eq!(candidates[3], evidence_dir.join("pijs_workload.jsonl"));
    assert_eq!(
        candidates[9],
        evidence_dir.join("perf/results/pijs_workload.jsonl")
    );
}

#[test]
fn context_intelligence_budget_artifacts_follow_resolved_target_dir() {
    let root = Path::new("/workspace/pi_agent_rust");
    let candidates = budget_artifact_candidates(root, "context_graph_build_cold_p95");
    let machine_candidates = context_intelligence_budget_candidate_paths(root);

    assert!(
        candidates.contains(&root.join(
            "target/criterion/semantic_context/graph_build_cold/large_workspace/new/estimates.json"
        )),
        "context graph build budget must inspect the resolved cargo target dir: {candidates:?}"
    );
    assert!(
        machine_candidates
            .contains(&root.join("target/perf/context_intelligence_planner_budget.json")),
        "context intelligence budget artifact must inspect the resolved cargo target dir: {machine_candidates:?}"
    );
    assert!(
        context_intelligence_budget_candidate_paths_in_evidence_dir(Path::new(
            "/workspace/pi_agent_rust/docs/evidence/perf"
        ))
        .contains(&PathBuf::from(
            "/workspace/pi_agent_rust/docs/evidence/perf/perf/results/context_intelligence_planner_budget.json"
        )),
        "staged perf evidence dirs must support nested perf/results artifacts"
    );
}

#[test]
fn budget_definitions_are_valid() {
    for budget in BUDGETS {
        assert!(!budget.name.is_empty(), "budget name must not be empty");
        assert!(
            !budget.category.is_empty(),
            "budget category must not be empty"
        );
        assert!(budget.threshold > 0.0, "budget threshold must be positive");
        assert!(!budget.unit.is_empty(), "budget unit must not be empty");
        assert!(
            !budget.methodology.is_empty(),
            "budget methodology must not be empty"
        );
    }
    eprintln!("[budgets] {} budgets defined", BUDGETS.len());
}

#[test]
fn budget_names_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for budget in BUDGETS {
        assert!(
            seen.insert(budget.name),
            "duplicate budget name: {}",
            budget.name
        );
    }
}

#[test]
fn budget_comparison_directions_are_explicit_and_not_name_derived() {
    let minimum_budgets = BUDGETS
        .iter()
        .filter(|budget| budget.comparison == BudgetComparison::Minimum)
        .map(|budget| budget.name)
        .collect::<Vec<_>>();
    assert_eq!(minimum_budgets, vec!["tool_call_throughput_min"]);

    let maximum = BUDGETS
        .iter()
        .find(|budget| budget.name == "tool_call_latency_mean")
        .expect("maximum budget");
    assert_eq!(
        classify_budget_status(maximum, Some(maximum.threshold), true),
        "PASS"
    );
    assert_eq!(
        classify_budget_status(maximum, Some(maximum.threshold + 1.0), true),
        "FAIL"
    );

    let minimum = BUDGETS
        .iter()
        .find(|budget| budget.name == "tool_call_throughput_min")
        .expect("minimum budget");
    assert_eq!(
        classify_budget_status(minimum, Some(minimum.threshold), true),
        "PASS"
    );
    assert_eq!(
        classify_budget_status(minimum, Some(minimum.threshold - 1.0), true),
        "FAIL"
    );
}

#[test]
fn budget_inventory_has_stable_cross_language_serialization() {
    let canonical = budget_inventory_canonical_json();
    let parsed: Value = serde_json::from_str(&canonical).expect("canonical inventory is JSON");
    assert_eq!(
        parsed.as_array().map(Vec::len),
        Some(BUDGETS.len()),
        "canonical inventory must serialize every budget in declaration order"
    );
    assert!(canonical.starts_with(
        "[{\"name\":\"startup_version_p95\",\"category\":\"startup\",\"metric\":\"p95 latency\",\"unit\":\"ms\",\"threshold\":100.000000,\"comparison\":\"maximum\",\"ci_enforced\":true,\"methodology\":"
    ));
    assert!(canonical.contains(
        "\"name\":\"tool_call_throughput_min\",\"category\":\"tool_call\",\"metric\":\"minimum calls/sec\",\"unit\":\"calls/sec\",\"threshold\":5000.000000,\"comparison\":\"minimum\""
    ));
    assert_eq!(
        budget_inventory_sha256(),
        "96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad",
        "canonical v0.2.0 budget inventory drifted"
    );
}

#[test]
fn ci_enforced_budgets_have_data_sources() {
    // CI-enforced budgets should have measurement data available
    let ci_budgets: Vec<_> = BUDGETS.iter().filter(|b| b.ci_enforced).collect();
    eprintln!(
        "[budgets] {} CI-enforced budgets out of {} total",
        ci_budgets.len(),
        BUDGETS.len()
    );
    for budget in &ci_budgets {
        eprintln!(
            "  {} ({}): {} {} {}",
            budget.name, budget.category, budget.threshold, budget.unit, budget.methodology
        );
    }
    assert!(
        ci_budgets.len() >= 5,
        "should have at least 5 CI-enforced budgets"
    );
}

#[test]
fn ci_enforced_budgets_fail_on_regression_or_missing_data() {
    let strict = perf_strict_mode();
    let root = project_root();

    let mut checked_with_data = 0usize;
    let mut checked_without_data = 0usize;
    let mut regressions = Vec::new();
    let mut no_data_budgets = Vec::new();
    let mut missing_data_failures = Vec::new();

    for budget in BUDGETS.iter().filter(|budget| budget.ci_enforced) {
        let result = check_budget(budget);
        match result.status.as_str() {
            "PASS" => {
                if result.actual.is_some() {
                    checked_with_data += 1;
                }
            }
            "FAIL" => {
                if let Some(actual) = result.actual {
                    checked_with_data += 1;
                    regressions.push(format!(
                        "{}: actual={actual:.3}{} threshold={:.3}{} source={}",
                        budget.name, budget.unit, budget.threshold, budget.unit, result.source
                    ));
                } else {
                    checked_without_data += 1;
                    missing_data_failures.push(format!(
                        "{}: FAIL (missing measurement data; source={})",
                        budget.name, result.source
                    ));
                }
            }
            _ => {
                checked_without_data += 1;
                no_data_budgets.push(format!(
                    "{}: NO_DATA (source={})",
                    budget.name, result.source
                ));
            }
        }
    }

    let data_contract_failures = collect_data_contract_failures(&root);

    eprintln!(
        "[budget] CI-enforced: with_data={checked_with_data}, without_data={checked_without_data}, strict={strict}"
    );
    if !no_data_budgets.is_empty() {
        eprintln!(
            "[budget] CI-enforced budgets with NO_DATA:\n  {}",
            no_data_budgets.join("\n  ")
        );
    }
    if !missing_data_failures.is_empty() {
        eprintln!(
            "[budget] CI-enforced budgets failing due to missing data:\n  {}",
            missing_data_failures.join("\n  ")
        );
    }
    if !data_contract_failures.is_empty() {
        let formatted = data_contract_failures
            .iter()
            .map(|failure| {
                let budget_name = failure
                    .budget_name
                    .as_deref()
                    .map_or_else(|| "<global>".to_string(), ToString::to_string);
                format!(
                    "{} [{}]: {}",
                    failure.contract_id, budget_name, failure.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        eprintln!("[budget] Data contract failures:\n  {formatted}");
    }

    assert!(
        regressions.is_empty(),
        "CI budget regressions detected:\n{}",
        regressions.join("\n")
    );

    if strict {
        assert!(
            missing_data_failures.is_empty(),
            "CI-enforced budgets missing measurement data must fail closed:\n{}",
            missing_data_failures.join("\n")
        );
        assert!(
            data_contract_failures.is_empty(),
            "CI-enforced data-contract violations detected:\n{}",
            data_contract_failures
                .iter()
                .map(|failure| format!(
                    "{} [{}]: {}",
                    failure.contract_id,
                    failure.budget_name.as_deref().unwrap_or("<global>"),
                    failure.detail
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

#[test]
fn check_tool_call_mean_latency_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "tool_call_latency_mean")
        .expect("tool_call_latency_mean budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual <= budget.threshold,
            "mean tool call latency {actual}us exceeds budget {}us",
            budget.threshold
        );
    }
}

#[test]
fn check_tool_call_throughput_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "tool_call_throughput_min")
        .expect("tool_call_throughput_min budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual >= budget.threshold,
            "tool call throughput {actual} calls/sec below budget {} calls/sec",
            budget.threshold
        );
    }
}

#[test]
fn pijs_workload_profile_field_is_present_when_data_exists() {
    let root = project_root();
    let (events, source) = match load_pijs_workload_artifact(&root) {
        PijsWorkloadArtifact::Missing => {
            eprintln!("[budget] No pijs_workload data — skipping profile field check");
            return;
        }
        PijsWorkloadArtifact::Invalid { source, detail } => {
            panic!("invalid pijs_workload artifact {source}: {detail}");
        }
        PijsWorkloadArtifact::Loaded { events, source, .. } => (events, source),
    };

    for event in &events {
        let profile = event
            .get("build_profile")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !profile.trim().is_empty(),
            "pijs_workload event missing non-empty build_profile in {source}: {event}"
        );
        assert!(
            event
                .get("build_profile_verified")
                .and_then(Value::as_bool)
                .is_some(),
            "pijs_workload event missing boolean build_profile_verified in {source}: {event}"
        );
    }
}

fn valid_pijs_gate_record(root: &Path, tool_calls_per_iteration: u64) -> Value {
    let iterations = PIJS_REGRESSION_GATE_ITERATIONS;
    let total_calls = iterations * tool_calls_per_iteration;
    let elapsed_us_f64 = total_calls as f64 * 49.5;
    let elapsed_us = elapsed_us_f64 as u64;
    let binary_path = root.join("target/perf/examples/pijs_workload");
    std::fs::create_dir_all(binary_path.parent().expect("PiJS binary parent"))
        .expect("create PiJS binary parent");
    if !binary_path.exists() {
        std::fs::write(&binary_path, b"canonical-pijs-test-binary")
            .expect("write PiJS test binary");
    }
    let binary_path = std::fs::canonicalize(binary_path).expect("canonicalize PiJS test binary");
    let binary_sha256 = sha256_file(&binary_path).expect("hash PiJS test binary");
    let binary_path = binary_path.display().to_string();
    let source_commit = "0123456789abcdef0123456789abcdef01234567";
    let config_hash = benchmark_provenance_config_hash(&BenchmarkProvenance {
        source_commit,
        source_dirty: false,
        build_profile: "perf",
        executable_build_profile: "perf",
        verification: BenchmarkBuildVerification {
            executable_profile: true,
            build_fingerprint: true,
            build_profile: true,
        },
        build_fingerprint_contract: BUILD_FINGERPRINT_CONTRACT,
        compiled_profile_family: "release",
        compiled_opt_level: "3",
        compiled_debug: "true",
        compiled_features: CANONICAL_PIJS_PERF_FEATURES,
        binary_path: &binary_path,
        binary_sha256: &binary_sha256,
        debug_assertions: false,
    });
    let mut record = json!({
        "schema": "pi.perf.workload.v1",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "run_id": "pijs-test-run",
        "correlation_id": "pijs-test-run",
        "source_commit": source_commit,
        "source_dirty": false,
        "tool": "pijs_workload",
        "scenario": "tool_call_roundtrip",
        "iterations": iterations,
        "tool_calls_per_iteration": tool_calls_per_iteration,
        "total_calls": total_calls,
        "elapsed_ms": elapsed_us / 1_000,
        "elapsed_us": elapsed_us,
        "elapsed_us_f64": elapsed_us_f64,
        "per_call_us": elapsed_us / total_calls,
        "per_call_us_f64": 49.5,
        "calls_per_sec": total_calls * 1_000_000 / elapsed_us,
    });
    let provenance = json!({
        "build_profile": "perf",
        "build_profile_verified": true,
        "build_fingerprint_contract": BUILD_FINGERPRINT_CONTRACT,
        "build_fingerprint_verified": true,
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "compiled_debug": "true",
        "compiled_features": CANONICAL_PIJS_PERF_FEATURES,
        "executable_build_profile": "perf",
        "executable_profile_verified": true,
        "debug_assertions": false,
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "config_hash": config_hash,
        "runtime_engine": "quickjs",
        "evidence_class": "measured",
        "confidence": "high",
        "eligible_for_regression_gate": true,
        "measurement_method": "wall_clock_observation",
        "measurement_boundary": "production_extension_manager",
        "measurement_contract_version": "production_extension_manager.v1",
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
        "allocator_requested": "system",
        "allocator_request_source": "env",
        "allocator_effective": "system",
        "allocator_fallback_reason": null
    });
    record.as_object_mut().expect("PiJS fixture record").extend(
        provenance
            .as_object()
            .expect("PiJS fixture provenance")
            .clone(),
    );
    record
}

fn write_pijs_workload_records(path: &Path, records: &[Value]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create pijs workload artifact directory");
    }
    let payload = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{payload}\n")).expect("write pijs workload artifact");
}

fn retarget_pijs_record(record: &mut Value, binary_path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(binary_path.parent().expect("PiJS binary parent"))
        .expect("create PiJS binary parent");
    std::fs::write(binary_path, contents).expect("write retargeted PiJS binary");
    let binary_path = std::fs::canonicalize(binary_path).expect("canonicalize PiJS binary");
    record["binary_path"] = json!(binary_path.display().to_string());
    record["executable_build_profile"] =
        json!(profile_from_target_path(&binary_path).expect("derive retargeted binary profile"));
    record["binary_sha256"] =
        json!(sha256_file(&binary_path).expect("hash retargeted PiJS binary"));
    refresh_pijs_test_config_hash(record);
}

fn refresh_pijs_test_config_hash(record: &mut Value) {
    let features = record["compiled_features"]
        .as_array()
        .expect("compiled features")
        .iter()
        .map(|value| value.as_str().expect("compiled feature string"))
        .collect::<Vec<_>>();
    let hash = benchmark_provenance_config_hash(&BenchmarkProvenance {
        source_commit: record["source_commit"].as_str().expect("source commit"),
        source_dirty: record["source_dirty"].as_bool().expect("source dirty"),
        build_profile: record["build_profile"].as_str().expect("build profile"),
        executable_build_profile: record["executable_build_profile"]
            .as_str()
            .expect("executable build profile"),
        verification: BenchmarkBuildVerification {
            executable_profile: record["executable_profile_verified"]
                .as_bool()
                .expect("executable profile verified"),
            build_fingerprint: record["build_fingerprint_verified"]
                .as_bool()
                .expect("build fingerprint verified"),
            build_profile: record["build_profile_verified"]
                .as_bool()
                .expect("build profile verified"),
        },
        build_fingerprint_contract: record["build_fingerprint_contract"]
            .as_str()
            .expect("build fingerprint contract"),
        compiled_profile_family: record["compiled_profile_family"]
            .as_str()
            .expect("compiled profile family"),
        compiled_opt_level: record["compiled_opt_level"]
            .as_str()
            .expect("compiled opt level"),
        compiled_debug: record["compiled_debug"].as_str().expect("compiled debug"),
        compiled_features: &features,
        binary_path: record["binary_path"].as_str().expect("binary path"),
        binary_sha256: record["binary_sha256"].as_str().expect("binary sha256"),
        debug_assertions: record["debug_assertions"]
            .as_bool()
            .expect("debug assertions"),
    });
    record["config_hash"] = json!(hash);
}

#[test]
fn pijs_workload_reader_prefers_profile_labeled_artifact_path() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let profile_dir = tmp.path().join("target/perf/perf");
    std::fs::create_dir_all(&profile_dir).expect("create profile perf dir");
    let path = profile_dir.join("pijs_workload_perf.jsonl");
    write_pijs_workload_records(
        &path,
        &[
            valid_pijs_gate_record(tmp.path(), 1),
            valid_pijs_gate_record(tmp.path(), 10),
        ],
    );

    let (latency, source) = read_pijs_workload_mean_latency(tmp.path());
    assert_eq!(latency, Some(49.5));
    assert_eq!(
        source,
        "cargo-target[0]://perf/perf/pijs_workload_perf.jsonl"
    );
}

#[test]
fn pijs_gate_reader_accepts_perf_quickjs_production_record() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    write_pijs_workload_records(
        &path,
        &[
            valid_pijs_gate_record(tmp.path(), 1),
            valid_pijs_gate_record(tmp.path(), 10),
        ],
    );

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, Some(49.5));
    let throughput = read_pijs_workload_throughput(tmp.path())
        .0
        .expect("canonical throughput");
    assert!((throughput - (1_000_000.0 / 49.5)).abs() < 1e-9);
    assert!(
        evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS).is_empty()
    );
}

#[test]
fn pijs_gate_reader_accepts_custom_cargo_target_dir_layout() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let artifact = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let binary = tmp.path().join("pi-build/perf/examples/pijs_workload");
    let mut latency = valid_pijs_gate_record(tmp.path(), 1);
    let mut throughput = valid_pijs_gate_record(tmp.path(), 10);
    retarget_pijs_record(&mut latency, &binary, b"custom-target-pijs-binary");
    retarget_pijs_record(&mut throughput, &binary, b"custom-target-pijs-binary");
    write_pijs_workload_records(&artifact, &[latency, throughput]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, Some(49.5));
}

#[test]
fn pijs_gate_reader_rejects_forged_metrics() {
    let cases = [
        (
            1_u64,
            "per_call_us_f64",
            json!(0.01),
            "per_call_us_f64 is inconsistent",
        ),
        (
            10_u64,
            "calls_per_sec",
            json!(9_999_999),
            "calls_per_sec must equal",
        ),
    ];
    for (lane, field, forged_value, expected_error) in cases {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
        let mut latency = valid_pijs_gate_record(tmp.path(), 1);
        let mut throughput = valid_pijs_gate_record(tmp.path(), 10);
        if lane == 1 {
            latency[field] = forged_value;
        } else {
            throughput[field] = forged_value;
        }
        write_pijs_workload_records(&path, &[latency, throughput]);

        let failures =
            evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
        assert_eq!(failures.len(), 2);
        assert!(
            failures
                .iter()
                .all(|failure| failure.detail.contains(expected_error))
        );
    }
}

#[test]
fn pijs_gate_reader_rejects_stale_timestamp_even_when_artifact_mtime_is_fresh() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let stale = (chrono::Utc::now() - chrono::TimeDelta::hours(48)).to_rfc3339();
    let mut latency = valid_pijs_gate_record(tmp.path(), 1);
    let mut throughput = valid_pijs_gate_record(tmp.path(), 10);
    latency["timestamp"] = json!(stale);
    throughput["timestamp"] = latency["timestamp"].clone();
    write_pijs_workload_records(&path, &[latency, throughput]);

    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), 24.0);
    assert_eq!(failures.len(), 2);
    assert!(
        failures
            .iter()
            .all(|failure| failure.detail.contains("timestamp is stale"))
    );
}

#[test]
fn pijs_gate_reader_rejects_mixed_run_identity_and_duplicate_lanes() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let latency = valid_pijs_gate_record(tmp.path(), 1);
    let mut throughput = valid_pijs_gate_record(tmp.path(), 10);
    throughput["run_id"] = json!("other-run");
    throughput["correlation_id"] = json!("other-run");
    write_pijs_workload_records(&path, &[latency.clone(), throughput]);
    assert!(
        read_pijs_workload_mean_latency(tmp.path())
            .1
            .contains("must share run_id")
    );

    write_pijs_workload_records(
        &path,
        &[
            latency.clone(),
            latency,
            valid_pijs_gate_record(tmp.path(), 10),
        ],
    );
    assert!(
        read_pijs_workload_mean_latency(tmp.path())
            .1
            .contains("exactly two eligible records")
    );
}

#[test]
fn pijs_gate_reader_rejects_binary_hash_allocator_and_feature_conflicts() {
    for (field, value, expected_error) in [
        (
            "binary_sha256",
            json!("0".repeat(64)),
            "binary_sha256 does not match",
        ),
        (
            "allocator_effective",
            json!("jemalloc"),
            "allocator_effective must equal \"system\"",
        ),
        (
            "compiled_features",
            json!(["sqlite-sessions"]),
            "compiled_features must equal canonical shipping feature set",
        ),
        (
            "compiled_opt_level",
            json!("z"),
            "compiled_opt_level must equal \"3\"",
        ),
        ("source_dirty", json!(true), "source_dirty must equal false"),
    ] {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
        let mut latency = valid_pijs_gate_record(tmp.path(), 1);
        latency[field] = value;
        write_pijs_workload_records(&path, &[latency, valid_pijs_gate_record(tmp.path(), 10)]);
        let failures =
            evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
        assert!(
            failures
                .iter()
                .all(|failure| failure.detail.contains(expected_error)),
            "unexpected failures: {failures:?}"
        );
    }
}

#[test]
fn pijs_gate_reader_rejects_zero_work() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut record = valid_pijs_gate_record(tmp.path(), 1);
    record["iterations"] = json!(0);
    record["total_calls"] = json!(0);
    write_pijs_workload_records(&path, &[record]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert!(failures.iter().any(|failure| {
        failure.contract_id == "ineligible_pijs_workload_artifact"
            && failure.budget_name.as_deref() == Some("tool_call_latency_mean")
            && failure
                .detail
                .contains("iterations must equal 2000 (observed=0)")
    }));
}

#[test]
fn pijs_gate_reader_requires_exact_canonical_iteration_count() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut record = valid_pijs_gate_record(tmp.path(), 1);
    record["iterations"] = json!(PIJS_REGRESSION_GATE_ITERATIONS - 1);
    record["total_calls"] = json!(PIJS_REGRESSION_GATE_ITERATIONS - 1);
    write_pijs_workload_records(&path, &[record]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert!(failures.iter().any(|failure| {
        failure.budget_name.as_deref() == Some("tool_call_latency_mean")
            && failure
                .detail
                .contains("iterations must equal 2000 (observed=1999)")
    }));
}

#[test]
fn pijs_gate_reader_rejects_unverified_perf_profile_claim() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut record = valid_pijs_gate_record(tmp.path(), 1);
    record["build_profile_verified"] = json!(false);
    write_pijs_workload_records(&path, &[record]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert!(failures.iter().any(|failure| {
        failure.budget_name.as_deref() == Some("tool_call_latency_mean")
            && failure
                .detail
                .contains("build_profile_verified must equal true")
    }));
}

#[test]
fn pijs_gate_reader_requires_nonempty_binary_path() {
    for binary_path in [None, Some("")] {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
        let mut record = valid_pijs_gate_record(tmp.path(), 1);
        match binary_path {
            Some(value) => record["binary_path"] = json!(value),
            None => {
                record
                    .as_object_mut()
                    .expect("PiJS fixture object")
                    .remove("binary_path");
            }
        }
        write_pijs_workload_records(&path, &[record]);

        assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
        let failures =
            evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
        assert!(failures.iter().any(|failure| {
            failure.budget_name.as_deref() == Some("tool_call_latency_mean")
                && failure
                    .detail
                    .contains("binary_path must be a non-empty string")
        }));
    }
}

#[test]
fn pijs_gate_reader_derives_perf_profile_from_binary_path() {
    let cases = [
        (
            "/tmp/pi_agent_rust/target/release/examples/pijs_workload",
            "derived_profile=Some(\"release\")",
        ),
        (
            "/tmp/pi_agent_rust/bin/pijs_workload",
            "derived_profile=Some(\"bin\")",
        ),
        (
            "/tmp/pi_agent_rust/target/perf/examples",
            "must identify the pijs_workload executable",
        ),
    ];

    for (binary_path, expected_error) in cases {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
        let mut record = valid_pijs_gate_record(tmp.path(), 1);
        record["binary_path"] = json!(binary_path);
        write_pijs_workload_records(&path, &[record]);

        assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
        let failures =
            evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
        assert!(failures.iter().any(|failure| {
            failure.budget_name.as_deref() == Some("tool_call_latency_mean")
                && failure.detail.contains(expected_error)
        }));
    }
}

#[test]
fn pijs_gate_reader_requires_precise_mean_latency_metric() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut record = valid_pijs_gate_record(tmp.path(), 1);
    record
        .as_object_mut()
        .expect("PiJS fixture object")
        .remove("per_call_us_f64");
    write_pijs_workload_records(&path, &[record]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert!(failures.iter().any(|failure| {
        failure.budget_name.as_deref() == Some("tool_call_latency_mean")
            && failure
                .detail
                .contains("per_call_us_f64 must contain a finite positive metric")
    }));
}

#[test]
fn pijs_gate_reader_rejects_debug_preview_native_and_explicitly_ineligible_rows() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut debug = valid_pijs_gate_record(tmp.path(), 1);
    debug["build_profile"] = json!("debug");
    let mut preview = valid_pijs_gate_record(tmp.path(), 1);
    preview["runtime_engine"] = json!("native_rust_preview");
    let mut native = valid_pijs_gate_record(tmp.path(), 1);
    native["runtime_engine"] = json!("native_rust_runtime");
    let mut ineligible = valid_pijs_gate_record(tmp.path(), 1);
    ineligible["eligible_for_regression_gate"] = json!(false);
    write_pijs_workload_records(&path, &[debug, preview, native, ineligible]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
}

#[test]
fn pijs_gate_reader_rejects_invalid_eligible_row_even_with_valid_quickjs_row() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let mut preview = valid_pijs_gate_record(tmp.path(), 1);
    preview["runtime_engine"] = json!("native_rust_preview");
    preview["per_call_us_f64"] = json!(0.01);
    let valid = valid_pijs_gate_record(tmp.path(), 1);
    write_pijs_workload_records(&path, &[preview, valid]);

    assert_eq!(read_pijs_workload_mean_latency(tmp.path()).0, None);
}

#[test]
fn pijs_gate_reader_fails_closed_on_invalid_canonical_artifact() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let canonical = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let fallback = tmp
        .path()
        .join("target/perf/release/pijs_workload_release.jsonl");
    let mut invalid = valid_pijs_gate_record(tmp.path(), 1);
    invalid["confidence"] = json!("medium");
    write_pijs_workload_records(&canonical, &[invalid]);
    write_pijs_workload_records(&fallback, &[valid_pijs_gate_record(tmp.path(), 1)]);

    let (latency, source) = read_pijs_workload_mean_latency(tmp.path());
    assert_eq!(latency, None);
    assert_eq!(
        source,
        "no admissible pijs_workload pair in cargo-target[0]://perf/perf/pijs_workload_perf.jsonl: confidence must equal \"high\" (observed=Some(\"medium\"))"
    );
}

#[test]
fn pijs_gate_reader_rejects_mixed_valid_and_corrupt_jsonl() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let latency = valid_pijs_gate_record(tmp.path(), 1);
    let throughput = valid_pijs_gate_record(tmp.path(), 10);
    std::fs::create_dir_all(path.parent().expect("artifact parent"))
        .expect("create artifact directory");
    std::fs::write(&path, format!("{latency}\n{{not-json\n{throughput}\n"))
        .expect("write mixed-validity artifact");

    let (actual, source) = read_pijs_workload_mean_latency(tmp.path());
    assert_eq!(actual, None);
    assert!(source.contains("line 2 is not valid JSON"), "{source}");
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert_eq!(failures.len(), 2);
    assert!(failures.iter().all(|failure| {
        failure.contract_id == "invalid_pijs_workload_artifact"
            && failure.detail.contains("line 2 is not valid JSON")
    }));
}

#[test]
fn pijs_gate_freshness_is_bound_to_selected_canonical_artifact() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let canonical = tmp.path().join("target/perf/perf/pijs_workload_perf.jsonl");
    let fallback = tmp
        .path()
        .join("target/perf/release/pijs_workload_release.jsonl");
    write_pijs_workload_records(
        &canonical,
        &[
            valid_pijs_gate_record(tmp.path(), 1),
            valid_pijs_gate_record(tmp.path(), 10),
        ],
    );
    write_pijs_workload_records(
        &fallback,
        &[
            valid_pijs_gate_record(tmp.path(), 1),
            valid_pijs_gate_record(tmp.path(), 10),
        ],
    );
    filetime::set_file_mtime(&canonical, filetime::FileTime::from_unix_time(1, 0))
        .expect("make canonical artifact stale");

    let (actual, source) = read_pijs_workload_mean_latency(tmp.path());
    assert_eq!(actual, None);
    assert!(
        source.contains(
            "selected artifact cargo-target[0]://perf/perf/pijs_workload_perf.jsonl is stale"
        ),
        "{source}"
    );
    let failures = evaluate_pijs_workload_gate_contract(tmp.path(), DEFAULT_MAX_ARTIFACT_AGE_HOURS);
    assert_eq!(failures.len(), 2);
    assert!(failures.iter().all(|failure| {
        failure.contract_id == "missing_or_stale_budget_artifact"
            && failure
                .detail
                .contains("cargo-target[0]://perf/perf/pijs_workload_perf.jsonl is stale")
    }));
}

#[test]
fn check_extension_load_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "ext_cold_load_simple_p95")
        .expect("ext_cold_load_simple_p95 budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual <= budget.threshold,
            "extension cold load {actual}ms exceeds budget {}ms",
            budget.threshold
        );
    }
}

#[test]
fn budget_report_generation_is_explicitly_opt_in() {
    assert!(!budget_report_generation_enabled(None));
    assert!(!budget_report_generation_enabled(Some("")));
    assert!(!budget_report_generation_enabled(Some("0")));
    assert!(budget_report_generation_enabled(Some("1")));
}

#[test]
fn blocked_sentinel_is_independent_of_artifact_roots_and_contents() {
    let first = tempfile::tempdir().expect("first fixture root");
    let second = tempfile::tempdir().expect("second fixture root");
    let first_artifact = first
        .path()
        .join("target/criterion/startup/version/warm/new/estimates.json");
    std::fs::create_dir_all(first_artifact.parent().expect("first artifact parent"))
        .expect("create first artifact parent");
    std::fs::write(&first_artifact, r#"{"mean":{"point_estimate":1.0}}"#)
        .expect("write first ambient artifact");
    let second_artifact = second.path().join("target/perf/pijs_workload.jsonl");
    std::fs::create_dir_all(second_artifact.parent().expect("second artifact parent"))
        .expect("create second artifact parent");
    std::fs::write(&second_artifact, "not-json\n").expect("write second ambient artifact");
    assert_eq!(
        display_source_path(first.path(), &first_artifact),
        "cargo-target[0]://criterion/startup/version/warm/new/estimates.json"
    );
    assert_eq!(
        display_source_path(second.path(), &second_artifact),
        "cargo-target[0]://perf/pijs_workload.jsonl"
    );

    let lineage = BudgetSummaryLineage {
        generated_at: "2026-08-05T17:00:00.000Z",
        source_commit: None,
        run_id: None,
        correlation_id: None,
        strict_mode: false,
    };
    let (first_results, first_failures) = evaluate_budget_report(first.path(), &lineage);
    let (second_results, second_failures) = evaluate_budget_report(second.path(), &lineage);
    let first_summary = budget_summary_value(&lineage, &first_results, &first_failures);
    let second_summary = budget_summary_value(&lineage, &second_results, &second_failures);

    assert_eq!(first_summary, second_summary);
    assert!(first_failures.is_empty());
    assert_eq!(first_summary["pass"].as_u64(), Some(0));
    assert_eq!(first_summary["fail"].as_u64(), Some(0));
    assert_eq!(
        first_summary["no_data"].as_u64(),
        Some(BUDGETS.len() as u64)
    );
    assert!(first_results.iter().all(|result| {
        result.actual.is_none()
            && result.status == "NO_DATA"
            && result.source == "not evaluated: authoritative benchmark lineage is incomplete"
    }));
}

#[test]
fn clean_source_commit_rejects_hidden_index_flags_and_untracked_files() {
    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let repo = tempfile::tempdir().expect("temporary git repository");
    git(repo.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.path().join("tracked.txt"), "tracked\n").expect("write tracked file");
    git(repo.path(), &["add", "tracked.txt"]);
    git(
        repo.path(),
        &[
            "-c",
            "user.name=Pi Test",
            "-c",
            "user.email=pi-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    assert!(clean_source_commit(repo.path()).is_some());

    git(
        repo.path(),
        &["update-index", "--skip-worktree", "tracked.txt"],
    );
    assert_eq!(clean_source_commit(repo.path()), None);
    git(
        repo.path(),
        &["update-index", "--no-skip-worktree", "tracked.txt"],
    );
    git(
        repo.path(),
        &["update-index", "--assume-unchanged", "tracked.txt"],
    );
    assert_eq!(clean_source_commit(repo.path()), None);
    git(
        repo.path(),
        &["update-index", "--no-assume-unchanged", "tracked.txt"],
    );
    assert!(clean_source_commit(repo.path()).is_some());

    let nested = repo.path().join("untracked/nested.txt");
    std::fs::create_dir_all(nested.parent().expect("nested parent"))
        .expect("create untracked directory");
    std::fs::write(nested, "untracked\n").expect("write untracked file");
    assert_eq!(clean_source_commit(repo.path()), None);
}

#[test]
fn claim_readiness_requires_complete_strict_same_run_evidence() {
    assert!(
        claim_readiness_blockers(
            true,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("release-run"),
            Some("release-run"),
            4,
            4,
            0,
            0,
            0,
            0,
            0,
        )
        .is_empty()
    );

    assert_eq!(
        claim_readiness_blockers(
            false,
            None,
            None,
            Some("different-run"),
            4,
            3,
            1,
            1,
            2,
            3,
            2,
        ),
        vec![
            "budget_data_missing",
            "budget_failed",
            "ci_budget_data_missing",
            "ci_budget_failed",
            "correlation_id_missing",
            "data_contract_failure",
            "run_id_missing",
            "source_commit_unbound",
            "strict_mode_disabled",
        ]
    );

    assert_eq!(
        claim_readiness_blockers(
            true,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("release-run"),
            Some("release-run"),
            4,
            4,
            0,
            0,
            1,
            0,
            0,
        ),
        vec!["budget_failed"],
        "a non-CI budget failure must block blanket performance claims",
    );
    assert_eq!(
        claim_readiness_blockers(
            true,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("release-run"),
            Some("release-run"),
            4,
            4,
            0,
            0,
            0,
            1,
            0,
        ),
        vec!["budget_data_missing"],
        "missing data for a non-CI budget must block blanket performance claims",
    );
}

#[test]
fn checked_in_budget_summary_matches_fresh_canonical_evaluation_exactly() {
    let root = project_root();
    let summary_path = root.join("tests/perf/reports/budget_summary.json");
    let summary_text = std::fs::read_to_string(&summary_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", summary_path.display()));
    assert!(
        summary_text.ends_with('\n') && !summary_text.ends_with("\n\n"),
        "checked-in budget summary must end with exactly one newline"
    );
    let checked_in: Value =
        serde_json::from_str(&summary_text).expect("checked-in budget summary must be valid JSON");
    assert_eq!(
        checked_in.get("schema").and_then(Value::as_str),
        Some("pi.perf.budget_summary.v2")
    );

    let generated_at = checked_in
        .get("generated_at")
        .and_then(Value::as_str)
        .expect("budget summary generated_at");
    let parsed_generated_at = chrono::DateTime::parse_from_rfc3339(generated_at)
        .expect("budget summary generated_at must be RFC3339")
        .with_timezone(&chrono::Utc);
    assert_eq!(
        parsed_generated_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        generated_at,
        "budget summary generated_at must use canonical millisecond UTC form"
    );

    let optional_string = |field: &str| match checked_in.get(field) {
        Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.as_str()),
        _ => panic!("budget summary {field} must be null or a non-empty string"),
    };
    let source_commit = optional_string("source_commit");
    if let Some(source_commit) = source_commit {
        assert!(
            source_commit.len() == 40
                && source_commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && !source_commit.bytes().all(|byte| byte == b'0'),
            "budget summary source_commit must be a full lowercase nonzero Git SHA"
        );
    }
    let run_id = optional_string("run_id");
    let correlation_id = optional_string("correlation_id");
    assert_eq!(
        run_id, correlation_id,
        "budget summary run and correlation identity must be identical"
    );
    let strict_mode = checked_in
        .get("strict_mode")
        .and_then(Value::as_bool)
        .expect("budget summary strict_mode");

    let lineage = BudgetSummaryLineage {
        generated_at,
        source_commit,
        run_id,
        correlation_id,
        strict_mode,
    };
    let (fresh_results, fresh_failures) = evaluate_budget_report(&root, &lineage);
    let expected = budget_summary_value(&lineage, &fresh_results, &fresh_failures);
    assert_eq!(
        checked_in, expected,
        "checked-in budget summary must exactly match fresh definitions, results, failures, counts, lineage, and readiness"
    );

    let events_path = root.join("tests/perf/reports/budget_events.jsonl");
    let events_text = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", events_path.display()));
    let checked_events = events_text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<Value>(line).unwrap_or_else(|err| {
                panic!(
                    "{} line {} is not valid JSON: {err}",
                    events_path.display(),
                    index + 1
                )
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        Value::Array(checked_events),
        expected["budget_results"],
        "checked-in budget events must exactly match the canonical summary results"
    );

    if !benchmark_lineage_is_authoritative(&lineage) {
        let markdown_path = root.join("tests/perf/reports/PERF_BUDGETS.md");
        let markdown = std::fs::read_to_string(&markdown_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", markdown_path.display()));
        assert!(
            markdown.contains(
                "## Failing Data Contracts\n\n- Not evaluated: authoritative benchmark lineage is incomplete."
            ),
            "blocked sentinel Markdown must not imply that data contracts were evaluated cleanly"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn generate_budget_report() {
    if !budget_report_generation_requested() {
        eprintln!(
            "[budget] Report generation skipped; set PI_GENERATE_PERF_BUDGET_REPORT=1 to write tracked reports"
        );
        return;
    }
    let root = project_root();
    // Capture source/run identity before mutating any tracked report. Otherwise
    // a clean, claim-ready generation would make itself appear dirty.
    let strict_mode = perf_strict_mode();
    let source_commit = clean_source_commit(&root);
    let run_id = perf_run_id();
    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let lineage = BudgetSummaryLineage {
        generated_at: &generated_at,
        source_commit: source_commit.as_deref(),
        run_id: run_id.as_deref(),
        correlation_id: run_id.as_deref(),
        strict_mode,
    };
    let (results, data_contract_failures) = evaluate_budget_report(&root, &lineage);
    let reports_dir = root.join("tests/perf/reports");
    let _ = std::fs::create_dir_all(&reports_dir);

    // ── Write JSONL ──
    let jsonl_path = reports_dir.join("budget_events.jsonl");
    let jsonl: String = results
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&jsonl_path, format!("{jsonl}\n")).expect("write budget_events.jsonl");

    // ── Write summary JSON ──
    let pass_count = results.iter().filter(|r| r.status == "PASS").count();
    let fail_count = results.iter().filter(|r| r.status == "FAIL").count();
    let no_data_count = results.iter().filter(|r| r.status == "NO_DATA").count();
    let ci_enforced_count = BUDGETS.iter().filter(|b| b.ci_enforced).count();
    let ci_results: Vec<_> = results.iter().filter(|result| result.ci_enforced).collect();
    let ci_with_data_count = ci_results
        .iter()
        .filter(|result| result.actual.is_some())
        .count();
    let ci_fail_count = ci_results
        .iter()
        .filter(|result| result.status == "FAIL")
        .count();
    let ci_no_data_count = ci_results
        .iter()
        .filter(|result| result.status == "NO_DATA")
        .count();
    let data_contract_failures_count = data_contract_failures.len();
    let run_id_json = run_id.as_deref();
    let run_id_label = run_id.as_deref().unwrap_or("not set").to_string();
    let correlation_id = run_id.as_deref();
    let readiness_blockers = claim_readiness_blockers(
        strict_mode,
        source_commit.as_deref(),
        run_id_json,
        correlation_id,
        ci_enforced_count,
        ci_with_data_count,
        ci_fail_count,
        ci_no_data_count,
        fail_count,
        no_data_count,
        data_contract_failures_count,
    );
    let claims_authorized = readiness_blockers.is_empty();
    let claim_readiness_status = if claims_authorized {
        "claim_ready"
    } else {
        "blocked"
    };
    let summary = budget_summary_value(&lineage, &results, &data_contract_failures);

    let summary_path = reports_dir.join("budget_summary.json");
    std::fs::write(
        &summary_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        ),
    )
    .expect("write budget_summary.json");

    // ── Write Markdown ──
    let mut md = String::with_capacity(8 * 1024);

    md.push_str("# Performance Budgets\n\n");
    let _ = writeln!(md, "> Generated: {}\n", generated_at);
    let _ = writeln!(md, "> Run ID: {run_id_label}\n");
    let _ = writeln!(
        md,
        "> Source commit: {}\n",
        source_commit.as_deref().unwrap_or("not bound (dirty tree)")
    );
    let _ = writeln!(md, "> Strict mode: {strict_mode}\n");
    let _ = writeln!(md, "> Claim readiness: {claim_readiness_status}\n");

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|---|---|\n");
    let _ = writeln!(md, "| Total budgets | {} |", BUDGETS.len());
    let _ = writeln!(md, "| CI-enforced | {ci_enforced_count} |");
    let _ = writeln!(md, "| CI-enforced with data | {ci_with_data_count} |");
    let _ = writeln!(md, "| CI-enforced FAIL | {ci_fail_count} |");
    let _ = writeln!(md, "| CI-enforced NO_DATA | {ci_no_data_count} |");
    let _ = writeln!(md, "| PASS | {pass_count} |");
    let _ = writeln!(md, "| FAIL | {fail_count} |");
    let _ = writeln!(md, "| No data | {no_data_count} |\n");
    let _ = writeln!(
        md,
        "| Failing data contracts | {data_contract_failures_count} |\n"
    );

    md.push_str("## Claim Readiness\n\n");
    if claims_authorized {
        md.push_str("Performance claims are authorized by this evidence set.\n\n");
    } else {
        md.push_str("Performance claims are blocked. Blocking reason codes:\n\n");
        for blocker in &readiness_blockers {
            let _ = writeln!(md, "- `{blocker}`");
        }
        md.push('\n');
    }

    // Group by category
    let categories = [
        "startup",
        "extension",
        "tool_call",
        "event_dispatch",
        "context_intelligence",
        "policy",
        "memory",
        "binary",
        "protocol",
    ];

    for cat in &categories {
        let cat_budgets: Vec<_> = BUDGETS.iter().filter(|b| b.category.eq(*cat)).collect();
        if cat_budgets.is_empty() {
            continue;
        }

        let _ = writeln!(md, "## {}\n", capitalize(cat));
        md.push_str("| Budget | Metric | Comparison | Threshold | Actual | Status | CI |\n");
        md.push_str("|---|---|---|---|---|---|---|\n");

        for budget in &cat_budgets {
            let Some(result) = results.iter().find(|r| r.budget_name.eq(budget.name)) else {
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} {} | - | NO_DATA | {} |",
                    budget.name,
                    budget.metric,
                    budget.comparison.symbol(),
                    format_value(budget.threshold, budget.unit),
                    budget.unit,
                    if budget.ci_enforced { "yes" } else { "no" }
                );
                continue;
            };
            let actual_str = result
                .actual
                .map_or_else(|| "-".to_string(), |v| format_value(v, budget.unit));
            let ci_str = if budget.ci_enforced { "Yes" } else { "No" };

            let _ = writeln!(
                md,
                "| `{}` | {} | {} | {} {} | {} | {} | {} |",
                budget.name,
                budget.metric,
                budget.comparison.symbol(),
                budget.threshold,
                budget.unit,
                actual_str,
                result.status,
                ci_str,
            );
        }
        md.push('\n');
    }

    md.push_str("## Failing Data Contracts\n\n");
    if !benchmark_lineage_is_authoritative(&lineage) {
        md.push_str("- Not evaluated: authoritative benchmark lineage is incomplete.\n\n");
    } else if data_contract_failures.is_empty() {
        md.push_str("- None\n\n");
    } else {
        for failure in &data_contract_failures {
            let budget_label = failure.budget_name.as_deref().unwrap_or("global");
            let _ = writeln!(
                md,
                "- `{}` (`{}`): {}",
                failure.contract_id, budget_label, failure.detail
            );
            let _ = writeln!(md, "  - Remediation: {}", failure.remediation);
        }
        md.push('\n');
    }

    // Methodology
    md.push_str("## Measurement Methodology\n\n");
    for budget in BUDGETS {
        let _ = writeln!(md, "- **`{}`**: {}", budget.name, budget.methodology);
    }
    md.push('\n');

    md.push_str("## CI Enforcement\n\n");
    md.push_str("CI-enforced budgets are checked on every PR. A budget violation ");
    md.push_str("blocks the PR from merging. Non-CI budgets are informational and ");
    md.push_str("checked in nightly runs.\n\n");
    md.push_str("```bash\n");
    md.push_str("# Run budget checks\n");
    md.push_str("cargo test --test perf_budgets -- --nocapture\n\n");
    md.push_str("# Generate full budget report\n");
    md.push_str("PI_GENERATE_PERF_BUDGET_REPORT=1 cargo test --test perf_budgets generate_budget_report -- --nocapture\n");
    md.push_str("```\n");

    let md_path = reports_dir.join("PERF_BUDGETS.md");
    std::fs::write(&md_path, &md).expect("write PERF_BUDGETS.md");

    // Print summary
    eprintln!("\n=== Performance Budget Report ===");
    eprintln!("  Total: {}", BUDGETS.len());
    eprintln!("  PASS:  {pass_count}");
    eprintln!("  FAIL:  {fail_count}");
    eprintln!("  N/A:   {no_data_count}");
    eprintln!("  Data contract failures: {data_contract_failures_count}");
    eprintln!("  Reports:");
    eprintln!("    {}", md_path.display());
    eprintln!("    {}", summary_path.display());
    eprintln!("    {}", jsonl_path.display());
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| {
        let upper: String = c.to_uppercase().collect();
        let rest: String = chars.collect();
        format!("{upper}{rest}")
    })
}

fn format_value(val: f64, unit: &str) -> String {
    match unit {
        "ms" | "MB" | "percent" => format!("{val:.1}"),
        "us" | "ns" | "calls/sec" => format!("{val:.0}"),
        _ => format!("{val:.2}"),
    }
}

#[test]
fn classify_budget_status_promotes_ci_no_data_to_fail_under_strict() {
    let budget = BUDGETS
        .iter()
        .find(|budget| budget.name == "tool_call_latency_mean")
        .expect("tool_call_latency_mean budget exists");
    assert_eq!(classify_budget_status(budget, None, false), "NO_DATA");
    assert_eq!(classify_budget_status(budget, None, true), "FAIL");
}

#[test]
fn idle_memory_budget_rejects_test_harness_rss_as_release_evidence() {
    let (actual, source) = read_idle_memory_rss();
    assert_eq!(actual, None);
    assert!(source.contains("test-harness process RSS is inadmissible"));
    let budget = BUDGETS
        .iter()
        .find(|budget| budget.name == "idle_memory_rss")
        .expect("idle-memory budget");
    assert_eq!(classify_budget_status(budget, actual, true), "FAIL");
}

#[test]
fn artifact_contract_flags_stale_evidence() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let artifact_path = tmp.path().join("artifact.json");
    std::fs::write(&artifact_path, "{}\n").expect("write artifact");
    std::thread::sleep(std::time::Duration::from_millis(25));

    let violation = evaluate_artifact_contract(tmp.path(), &[artifact_path], 0.000001)
        .expect("stale artifact violation expected");
    assert!(
        violation.contains("stale/invalid"),
        "expected stale violation text, got: {violation}"
    );
}

#[test]
fn binary_size_candidate_builder_defaults_to_release_then_perf() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_prefers_release_override_then_release_then_perf() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let override_path = target_dir.join("custom-release/pi");
    let candidates = build_binary_size_candidate_paths(target_dir, Some(override_path.clone()), "");
    assert_eq!(
        candidates,
        vec![
            override_path,
            target_dir.join("release/pi"),
            target_dir.join("perf/pi"),
        ]
    );
}

#[test]
fn binary_size_candidate_builder_includes_non_debug_profile_before_perf() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "bench-profile");
    assert_eq!(
        candidates,
        vec![
            target_dir.join("release/pi"),
            target_dir.join("bench-profile/pi"),
            target_dir.join("perf/pi"),
        ]
    );
}

#[test]
fn binary_size_candidate_builder_ignores_debug_profile() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "debug");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_ignores_debug_profile_case_insensitive() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "DeBuG");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_ignores_padded_debug_profile_case_insensitive() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "  DeBuG\t");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_dedups_perf_profile() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "perf");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_dedups_release_profile() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, "release");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_dedups_override_matching_release() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let release = target_dir.join("release/pi");
    let candidates =
        build_binary_size_candidate_paths(target_dir, Some(release.clone()), "release");
    assert_eq!(candidates, vec![release, target_dir.join("perf/pi")]);
}

#[test]
fn binary_size_candidate_builder_ignores_whitespace_only_profile() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, " \t ");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

#[test]
fn binary_size_candidate_builder_trims_profile_before_dedup() {
    let target_dir = Path::new("/tmp/pi-agent-target");
    let candidates = build_binary_size_candidate_paths(target_dir, None, " release ");
    assert_eq!(
        candidates,
        vec![target_dir.join("release/pi"), target_dir.join("perf/pi")]
    );
}

fn valid_context_intelligence_budget_artifact_fixture() -> Value {
    json!({
        "schema": CONTEXT_INTELLIGENCE_PERF_SCHEMA,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "run_id": "context-budget-test",
        "correlation_id": "context-budget-test",
        "environment": {
            "cargo_target_dir": "/data/tmp/pi_agent_rust_cargo/test/target",
            "tmpdir": "/data/tmp/pi_agent_rust_cargo/test/tmp"
        },
        "host": {
            "os": "linux",
            "arch": "x86_64"
        },
        "workspace": {
            "fixture": "synthetic_large_workspace",
            "files": 128,
            "graph_nodes": 512,
            "graph_edges": 768
        },
        "cache_hit_miss": {
            "cold_graph_build": "miss:no_prior_graph",
            "warm_graph_build": "hit:fingerprint_stable",
            "incremental_update": "miss:input_fingerprint_changed"
        },
        "determinism": {
            "randomized_file_order_checked": true,
            "matched": true,
            "first_summary_sha256": "abc123",
            "second_summary_sha256": "abc123"
        },
        "metrics": {
            "context_graph_build_cold_ms": {"p95_ms": 42.0},
            "context_graph_build_warm_ms": {"p95_ms": 12.0},
            "context_incremental_update_ms": {"p95_ms": 18.0},
            "context_planning_ms": {"p95_ms": 3.0},
            "context_bundle_serialization_ms": {"p95_ms": 1.5},
            "context_bundle_estimated_bytes": {"bytes": 8192.0}
        }
    })
}

fn write_context_intelligence_budget_artifact(path: &Path, payload: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create context budget artifact dir");
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(payload).unwrap_or_default(),
    )
    .expect("write context intelligence budget artifact");
}

#[test]
fn context_intelligence_budget_reader_prefers_machine_artifact() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let artifact = tmp
        .path()
        .join("target/perf/context_intelligence_planner_budget.json");
    write_context_intelligence_budget_artifact(
        &artifact,
        &valid_context_intelligence_budget_artifact_fixture(),
    );

    let (actual, source) = read_context_intelligence_budget_metric(
        tmp.path(),
        "context_graph_build_cold_p95",
        Some("graph_build_cold"),
    );

    assert_eq!(actual, Some(42.0));
    assert_eq!(
        source,
        "cargo-target[0]://perf/context_intelligence_planner_budget.json"
    );
}

#[test]
fn context_intelligence_budget_contract_accepts_valid_artifact() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let artifact = tmp
        .path()
        .join("target/perf/context_intelligence_planner_budget.json");
    write_context_intelligence_budget_artifact(
        &artifact,
        &valid_context_intelligence_budget_artifact_fixture(),
    );

    let failures = evaluate_context_intelligence_budget_contract(tmp.path(), 24.0);
    assert!(
        failures.is_empty(),
        "did not expect context intelligence budget failures, got: {failures:?}",
    );
}

#[test]
fn context_intelligence_budget_contract_fails_closed_when_missing() {
    let tmp = tempfile::tempdir().expect("create tempdir");

    let failures = evaluate_context_intelligence_budget_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "missing_or_stale_context_intelligence_budget_evidence"
        }),
        "expected missing context budget evidence failure, got: {failures:?}",
    );
}

#[test]
fn context_intelligence_budget_contract_requires_randomized_order_replay() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let artifact = tmp
        .path()
        .join("target/perf/context_intelligence_planner_budget.json");
    let mut payload = valid_context_intelligence_budget_artifact_fixture();
    payload["determinism"]["matched"] = json!(false);
    write_context_intelligence_budget_artifact(&artifact, &payload);

    let failures = evaluate_context_intelligence_budget_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_context_intelligence_determinism_contract"
        }),
        "expected determinism contract failure, got: {failures:?}",
    );
}

fn write_stratification_artifact(path: &Path, invalidity_reasons: &[&str], include_full_e2e: bool) {
    let full_e2e_layer = include_full_e2e.then(|| {
        json!({
            "layer_id": "full_e2e_long_session",
            "absolute_metrics": {"value": 120.0},
            "relative_metrics": {"rust_vs_node_ratio": 1.8, "rust_vs_bun_ratio": 1.5}
        })
    });
    write_stratification_artifact_with_full_e2e_layer(path, invalidity_reasons, full_e2e_layer);
}

fn write_stratification_artifact_with_full_e2e_layer(
    path: &Path,
    invalidity_reasons: &[&str],
    full_e2e_layer: Option<Value>,
) {
    let full_e2e_layers = full_e2e_layer.into_iter().collect::<Vec<_>>();
    write_stratification_artifact_with_claim_guard(
        path,
        invalidity_reasons,
        &full_e2e_layers,
        Some(true),
        Some(!full_e2e_layers.is_empty()),
    );
}

fn write_stratification_artifact_with_full_e2e_layers(
    path: &Path,
    invalidity_reasons: &[&str],
    full_e2e_layers: &[Value],
) {
    write_stratification_artifact_with_claim_guard(
        path,
        invalidity_reasons,
        full_e2e_layers,
        Some(true),
        Some(!full_e2e_layers.is_empty()),
    );
}

fn write_stratification_artifact_with_claim_guard(
    path: &Path,
    invalidity_reasons: &[&str],
    full_e2e_layers: &[Value],
    global_claim_valid: Option<bool>,
    full_e2e_layer_coverage: Option<bool>,
) {
    let mut layers = vec![
        json!({
            "layer_id": "cold_load_init",
            "absolute_metrics": {"value": 10.0},
            "relative_metrics": {"rust_vs_node_ratio": 2.1, "rust_vs_bun_ratio": 1.7}
        }),
        json!({
            "layer_id": "per_call_dispatch_micro",
            "absolute_metrics": {"value": 40.0},
            "relative_metrics": {"rust_vs_node_ratio": 2.0, "rust_vs_bun_ratio": 1.6}
        }),
    ];
    if !full_e2e_layers.is_empty() {
        layers.extend(full_e2e_layers.iter().cloned());
    }

    let mut cherry_pick_guard = serde_json::Map::new();
    cherry_pick_guard.insert(
        "invalidity_reasons".to_string(),
        Value::Array(
            invalidity_reasons
                .iter()
                .map(|reason| Value::String((*reason).to_string()))
                .collect(),
        ),
    );
    if let Some(valid) = global_claim_valid {
        cherry_pick_guard.insert("global_claim_valid".to_string(), Value::Bool(valid));
    }
    if let Some(covered) = full_e2e_layer_coverage {
        let mut layer_coverage = serde_json::Map::new();
        layer_coverage.insert("full_e2e_long_session".to_string(), Value::Bool(covered));
        cherry_pick_guard.insert("layer_coverage".to_string(), Value::Object(layer_coverage));
    }

    let payload = json!({
        "schema": "pi.perf.extension_benchmark_stratification.v1",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "layers": layers,
        "claim_integrity": {
            "cherry_pick_guard": Value::Object(cherry_pick_guard)
        }
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )
    .expect("write stratification artifact");
}

fn valid_weighted_bottleneck_attribution_fixture() -> Value {
    json!({
        "schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
        "status": "computed",
        "weighting_policy": "session_messages",
        "confidence_method": "weighted_normal_approx_95",
        "per_scale": [
            {
                "session_messages": 100_000,
                "partitions": [
                    {
                        "workload_partition": "matched-state",
                        "present": true,
                        "scenario_id": "matched-state/session_100000",
                        "total_stage_ms": 117.0,
                        "stage_pct": {
                            "open_ms": 41.0,
                            "append_ms": 31.0,
                            "save_ms": 19.0,
                            "index_ms": 9.0
                        }
                    },
                    {
                        "workload_partition": "realistic",
                        "present": true,
                        "scenario_id": "realistic/session_100000",
                        "total_stage_ms": 105.0,
                        "stage_pct": {
                            "open_ms": 42.0,
                            "append_ms": 30.0,
                            "save_ms": 18.0,
                            "index_ms": 10.0
                        }
                    }
                ]
            }
        ],
        "global_ranking": [
            {
                "stage": "open_ms",
                "weighted_stage_ms": 9_200_000.0,
                "weighted_contribution_pct": 41.4,
                "mean_share_pct": 41.4,
                "ci95_lower_pct": 40.8,
                "ci95_upper_pct": 42.0,
                "sample_size": 2
            }
        ],
        "lineage": {
            "source_stream": "phase1_matrix_validation.matrix_cells",
            "source_cell_count": 2,
            "valid_cell_count": 2
        }
    })
}

fn write_phase1_matrix_validation_artifact(path: &Path, weighted_bottleneck_attribution: &Value) {
    let payload = json!({
        "schema": "pi.perf.phase1_matrix_validation.v1",
        "run_id": "20260217T000000Z",
        "correlation_id": "abc123def456",
        "matrix_cells": [
            {
                "workload_partition": "matched-state",
                "session_messages": 100_000,
                "scenario_id": "matched-state/session_100000",
                "status": "pass",
                "stage_attribution": {
                    "open_ms": 48.0,
                    "append_ms": 36.0,
                    "save_ms": 22.0,
                    "index_ms": 11.0,
                    "total_stage_ms": 117.0
                }
            },
            {
                "workload_partition": "realistic",
                "session_messages": 100_000,
                "scenario_id": "realistic/session_100000",
                "status": "pass",
                "stage_attribution": {
                    "open_ms": 44.0,
                    "append_ms": 32.0,
                    "save_ms": 19.0,
                    "index_ms": 10.0,
                    "total_stage_ms": 105.0
                }
            }
        ],
        "weighted_bottleneck_attribution": weighted_bottleneck_attribution
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )
    .expect("write phase1 matrix artifact");
}

#[test]
fn required_e2e_ratio_contract_fails_when_full_e2e_evidence_missing() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    write_stratification_artifact(&artifact, &[], false);

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contract_id == "missing_required_e2e_or_ratio_outputs"),
        "expected missing_required_e2e_or_ratio_outputs failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_flags_microbench_only_claim() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    write_stratification_artifact(&artifact, &["microbench_only_claim"], true);

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contract_id == "microbench_only_claim"),
        "expected microbench_only_claim failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_full_e2e_values_non_positive() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let invalid_full_e2e = json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": 0.0},
        "relative_metrics": {"rust_vs_node_ratio": -1.0, "rust_vs_bun_ratio": 1.5}
    });
    write_stratification_artifact_with_full_e2e_layer(&artifact, &[], Some(invalid_full_e2e));

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contract_id == "missing_required_e2e_or_ratio_outputs"),
        "expected missing_required_e2e_or_ratio_outputs failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_full_e2e_values_non_numeric() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let invalid_full_e2e = json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": "n/a"},
        "relative_metrics": {"rust_vs_node_ratio": 1.8, "rust_vs_bun_ratio": null}
    });
    write_stratification_artifact_with_full_e2e_layer(&artifact, &[], Some(invalid_full_e2e));

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contract_id == "missing_required_e2e_or_ratio_outputs"),
        "expected missing_required_e2e_or_ratio_outputs failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_duplicate_full_e2e_layers_present() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let duplicate_layers = vec![
        json!({
            "layer_id": "full_e2e_long_session",
            "absolute_metrics": {"value": 120.0},
            "relative_metrics": {"rust_vs_node_ratio": 1.8, "rust_vs_bun_ratio": 1.5}
        }),
        json!({
            "layer_id": "full_e2e_long_session",
            "absolute_metrics": {"value": 130.0},
            "relative_metrics": {"rust_vs_node_ratio": 1.7, "rust_vs_bun_ratio": 1.4}
        }),
    ];
    write_stratification_artifact_with_full_e2e_layers(&artifact, &[], &duplicate_layers);

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "missing_required_e2e_or_ratio_outputs"
                && failure
                    .detail
                    .contains("duplicate full_e2e_long_session layers")
        }),
        "expected duplicate full_e2e_long_session failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_global_claim_valid_is_false() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let full_e2e_layers = vec![json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": 120.0},
        "relative_metrics": {"rust_vs_node_ratio": 1.8, "rust_vs_bun_ratio": 1.5}
    })];
    write_stratification_artifact_with_claim_guard(
        &artifact,
        &[],
        &full_e2e_layers,
        Some(false),
        Some(true),
    );

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_claim_integrity_guard"
                && failure.detail.contains("global_claim_valid=false")
        }),
        "expected invalid_claim_integrity_guard failure for false global_claim_valid, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_layer_coverage_missing() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let full_e2e_layers = vec![json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": 120.0},
        "relative_metrics": {"rust_vs_node_ratio": 1.8, "rust_vs_bun_ratio": 1.5}
    })];
    write_stratification_artifact_with_claim_guard(
        &artifact,
        &[],
        &full_e2e_layers,
        Some(true),
        None,
    );

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_claim_integrity_guard"
                && failure
                    .detail
                    .contains("full_e2e_layer_coverage=missing_or_non_boolean")
        }),
        "expected invalid_claim_integrity_guard failure for missing layer coverage, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_fails_when_bun_killer_ratio_exceeds_threshold() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let full_e2e_layer = json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": 120.0},
        "relative_metrics": {"rust_vs_node_ratio": 0.40, "rust_vs_bun_ratio": 0.34}
    });
    write_stratification_artifact_with_full_e2e_layer(&artifact, &[], Some(full_e2e_layer));

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contract_id == "bun_killer_ratio_release_gate"),
        "expected bun_killer_ratio_release_gate failure, got: {failures:?}",
    );
}

#[test]
fn required_e2e_ratio_contract_accepts_bun_killer_ratio_at_threshold() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf");
    std::fs::create_dir_all(&perf_dir).expect("create perf dir");
    let artifact = perf_dir.join("extension_benchmark_stratification.json");
    let full_e2e_layer = json!({
        "layer_id": "full_e2e_long_session",
        "absolute_metrics": {"value": 120.0},
        "relative_metrics": {"rust_vs_node_ratio": 0.30, "rust_vs_bun_ratio": 0.33}
    });
    write_stratification_artifact_with_full_e2e_layer(&artifact, &[], Some(full_e2e_layer));

    let failures = evaluate_required_e2e_ratio_contract(tmp.path(), 24.0);
    assert!(
        !failures
            .iter()
            .any(|failure| failure.contract_id == "bun_killer_ratio_release_gate"),
        "did not expect bun_killer_ratio_release_gate failure, got: {failures:?}",
    );
}

#[test]
fn phase1_weighted_contract_accepts_valid_artifact() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf/results");
    std::fs::create_dir_all(&perf_dir).expect("create perf results dir");
    let artifact = perf_dir.join("phase1_matrix_validation.json");
    write_phase1_matrix_validation_artifact(
        &artifact,
        &valid_weighted_bottleneck_attribution_fixture(),
    );

    let failures = evaluate_phase1_weighted_attribution_contract(tmp.path(), 24.0);
    assert!(
        failures.is_empty(),
        "did not expect weighted-attribution contract failures, got: {failures:?}",
    );
}

#[test]
fn phase1_weighted_contract_fails_when_object_missing() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf/results");
    std::fs::create_dir_all(&perf_dir).expect("create perf results dir");
    let artifact = perf_dir.join("phase1_matrix_validation.json");
    write_phase1_matrix_validation_artifact(&artifact, &Value::Null);

    let failures = evaluate_phase1_weighted_attribution_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_weighted_bottleneck_attribution_contract"
                && failure.detail.contains("must be an object")
        }),
        "expected missing weighted-attribution object failure, got: {failures:?}",
    );
}

#[test]
fn phase1_weighted_contract_fails_when_schema_invalid() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf/results");
    std::fs::create_dir_all(&perf_dir).expect("create perf results dir");
    let artifact = perf_dir.join("phase1_matrix_validation.json");
    let mut weighted = valid_weighted_bottleneck_attribution_fixture();
    weighted["schema"] = json!("pi.perf.phase1_weighted_bottleneck_attribution.v0");
    write_phase1_matrix_validation_artifact(&artifact, &weighted);

    let failures = evaluate_phase1_weighted_attribution_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_weighted_bottleneck_attribution_contract"
                && failure
                    .detail
                    .contains("schema must be pi.perf.phase1_weighted_bottleneck_attribution.v1")
        }),
        "expected invalid weighted schema failure, got: {failures:?}",
    );
}

#[test]
fn phase1_weighted_contract_fails_when_status_invalid() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf/results");
    std::fs::create_dir_all(&perf_dir).expect("create perf results dir");
    let artifact = perf_dir.join("phase1_matrix_validation.json");
    let mut weighted = valid_weighted_bottleneck_attribution_fixture();
    weighted["status"] = json!("partial");
    write_phase1_matrix_validation_artifact(&artifact, &weighted);

    let failures = evaluate_phase1_weighted_attribution_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_weighted_bottleneck_attribution_contract"
                && failure
                    .detail
                    .contains("status must be one of computed/missing")
        }),
        "expected invalid weighted status failure, got: {failures:?}",
    );
}

#[test]
fn phase1_weighted_contract_fails_when_missing_status_coherence_breaks() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let perf_dir = tmp.path().join("target/perf/results");
    std::fs::create_dir_all(&perf_dir).expect("create perf results dir");
    let artifact = perf_dir.join("phase1_matrix_validation.json");
    let mut weighted = valid_weighted_bottleneck_attribution_fixture();
    weighted["status"] = json!("missing");
    write_phase1_matrix_validation_artifact(&artifact, &weighted);

    let failures = evaluate_phase1_weighted_attribution_contract(tmp.path(), 24.0);
    assert!(
        failures.iter().any(|failure| {
            failure.contract_id == "invalid_weighted_bottleneck_attribution_contract"
                && failure.detail.contains("status=missing requires")
        }),
        "expected missing-status coherence failure, got: {failures:?}",
    );
}

#[test]
fn perf_sli_matrix_defines_evidence_adjudication_contract() {
    let perf = load_perf_sli_matrix();
    let contract = perf["evidence_adjudication_contract"]
        .as_object()
        .expect("evidence_adjudication_contract must be object");

    assert_eq!(
        contract.get("schema").and_then(Value::as_str),
        Some("pi.perf.evidence_adjudication_contract.v1"),
        "evidence_adjudication_contract.schema must be versioned"
    );

    let required_inputs: Vec<&str> = contract["required_input_artifacts"]
        .as_array()
        .expect("required_input_artifacts must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in [
        "summary_json",
        "baseline_variance_confidence",
        "extension_benchmark_stratification",
        "phase1_matrix_validation",
        "claim_integrity_scenario_cells",
    ] {
        assert!(
            required_inputs.contains(&required),
            "required_input_artifacts must include {required}"
        );
    }

    let statuses: Vec<&str> = contract["allowed_verdict_statuses"]
        .as_array()
        .expect("allowed_verdict_statuses must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for status in ["resolved", "conflict", "stale", "non_canonical"] {
        assert!(
            statuses.contains(&status),
            "allowed_verdict_statuses must include {status}"
        );
    }
}

#[test]
fn perf_sli_matrix_adjudication_contract_is_fail_closed() {
    let perf = load_perf_sli_matrix();
    let contract = &perf["evidence_adjudication_contract"];

    let reason_codes: Vec<&str> = contract["fail_closed_reason_codes"]
        .as_array()
        .expect("fail_closed_reason_codes must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for reason in [
        "missing_input_artifact",
        "stale_input_artifact",
        "lineage_mismatch",
        "confidence_conflict_unresolved",
        "non_canonical_claim_source",
    ] {
        assert!(
            reason_codes.contains(&reason),
            "fail_closed_reason_codes must include {reason}"
        );
    }

    assert!(
        perf["ci_enforcement"]["fail_closed_conditions"]
            .as_array()
            .expect("ci_enforcement.fail_closed_conditions must be an array")
            .iter()
            .filter_map(Value::as_str)
            .any(|condition| condition == "unresolved_conflicting_claims"),
        "ci_enforcement.fail_closed_conditions must include unresolved_conflicting_claims"
    );
}
