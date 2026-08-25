//! Benchmark JSONL schema definitions and validation tests (bd-167l).
//!
//! Defines the canonical machine-readable output format for extension benchmark
//! runs. All benchmark JSONL records share a common envelope with environment
//! fingerprint, and schema-specific payload fields.
//!
//! Run with: `cargo test --test bench_schema -- --nocapture`

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::literal_string_with_formatting_args,
    dead_code
)]

use pi::perf_build::{
    BUILD_FINGERPRINT_CONTRACT, BenchmarkBuildVerification, BenchmarkProvenance,
    CANONICAL_PIJS_PERF_FEATURES, benchmark_provenance_config_hash,
    matches_canonical_perf_build_fingerprint, profile_from_target_path, sha256_file,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Schema Definitions ──────────────────────────────────────────────────────

/// Common environment fingerprint included in every benchmark record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvFingerprint {
    /// Operating system (e.g., "Linux (Ubuntu 25.10)")
    pub os: String,
    /// CPU architecture (e.g., "x86_64")
    pub arch: String,
    /// CPU model string
    pub cpu_model: String,
    /// Number of logical CPU cores
    pub cpu_cores: u32,
    /// Total system memory in MB
    pub mem_total_mb: u64,
    /// Build profile: "debug" or "release"
    pub build_profile: String,
    /// Git commit hash (short)
    pub git_commit: String,
    /// Cargo feature flags active during build
    #[serde(default)]
    pub features: Vec<String>,
    /// SHA-256 of the concatenated env fields (for dedup/comparison)
    pub config_hash: String,
}

/// Schema: `pi.ext.rust_bench.v1` — Rust extension benchmark event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustBenchEvent {
    pub schema: String,
    pub runtime: String,
    pub scenario: String,
    pub extension: String,
    #[serde(flatten)]
    pub payload: Value,
    #[serde(default)]
    pub env: Option<EnvFingerprint>,
}

/// Schema: `pi.ext.legacy_bench.v1` — Legacy (TS/Node) benchmark event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyBenchEvent {
    pub schema: String,
    pub runtime: String,
    pub scenario: String,
    pub extension: String,
    #[serde(flatten)]
    pub payload: Value,
    #[serde(default)]
    pub node: Option<Value>,
}

/// Schema: `pi.perf.workload.v1` — PiJS workload benchmark event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadEvent {
    pub scenario: String,
    pub iterations: u64,
    pub tool_calls_per_iteration: u64,
    pub total_calls: u64,
    pub elapsed_ms: u64,
    pub per_call_us: u64,
    pub calls_per_sec: u64,
}

/// Schema: `pi.perf.budget.v1` — Performance budget check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetEvent {
    pub budget_name: String,
    pub category: String,
    pub threshold: f64,
    pub unit: String,
    pub actual: Option<f64>,
    pub status: String,
    pub source: String,
}

// ─── Schema Registry ─────────────────────────────────────────────────────────

const PERF_BUDGET_SUMMARY_SCHEMA: &str = "pi.perf.budget_summary.v2";
const PERF_BUDGET_DEFINITION_FIELDS: &[&str] = &[
    "name",
    "category",
    "metric",
    "unit",
    "threshold",
    "comparison",
    "ci_enforced",
    "methodology",
];
const PERF_BUDGET_COMPARISON_VALUES: &[&str] = &["maximum", "minimum"];
const PERF_CLAIM_READINESS_BLOCKER_CODES: &[&str] = &[
    "budget_data_missing",
    "budget_failed",
    "ci_budget_data_missing",
    "ci_budget_failed",
    "correlation_id_missing",
    "data_contract_failure",
    "run_id_missing",
    "source_commit_unbound",
    "strict_mode_disabled",
];
const PERF_BUDGET_V0_2_0_INVENTORY_SHA256: &str =
    "4e24380af0ca4fe8fd94850d63e607868d15d704a42d434bdb1c762e7e327663";

/// Known JSONL schemas with version and description.
const SCHEMAS: &[(&str, &str)] = &[
    (
        "pi.bench.protocol.v1",
        "Canonical benchmark protocol contract (partitions, datasets, metadata, replay inputs)",
    ),
    (
        "pi.ext.rust_bench.v1",
        "Rust QuickJS extension benchmark event (load, tool call, event hook)",
    ),
    (
        "pi.ext.legacy_bench.v1",
        "Legacy pi-mono (Node.js) extension benchmark event",
    ),
    (
        "pi.perf.workload.v1",
        "PiJS workload harness output (tool call throughput)",
    ),
    ("pi.perf.budget.v1", "Performance budget check result"),
    (
        PERF_BUDGET_SUMMARY_SCHEMA,
        "Strict provenance-bound budget summary with per-budget results and claim readiness",
    ),
    (
        "pi.ext.conformance_report.v2",
        "Per-extension conformance report event",
    ),
    (
        "pi.ext.conformance_summary.v2",
        "Aggregate conformance summary with per-tier breakdowns",
    ),
    (
        "pi.perf.extension_benchmark_stratification.v1",
        "Layered extension benchmark artifact linking cold-load, per-call, and full E2E evidence with claim-integrity guards",
    ),
    (
        "pi.perf.phase1_matrix_validation.v1",
        "Phase-1 realistic/matched-state matrix validation with stage attribution and release-gate readiness",
    ),
    (
        "pi.resource_governor.admission.v1",
        "Host-scale resource-governor admission decision telemetry for swarm pressure control",
    ),
];

/// Required fields for each schema (field name, description).
const RUST_BENCH_REQUIRED: &[&str] = &["schema", "runtime", "scenario", "extension"];
const LEGACY_BENCH_REQUIRED: &[&str] = &["schema", "runtime", "scenario", "extension"];
const WORKLOAD_REQUIRED: &[&str] = &[
    "scenario",
    "iterations",
    "tool_calls_per_iteration",
    "total_calls",
    "elapsed_ms",
    "per_call_us",
    "calls_per_sec",
];

/// Environment fingerprint fields.
const ENV_FINGERPRINT_FIELDS: &[(&str, &str)] = &[
    ("os", "Operating system name and version"),
    ("arch", "CPU architecture (x86_64, aarch64)"),
    (
        "cpu_model",
        "CPU model string from /proc/cpuinfo or sysinfo",
    ),
    ("cpu_cores", "Logical CPU core count"),
    ("mem_total_mb", "Total system memory in megabytes"),
    ("build_profile", "Cargo build profile: debug or release"),
    ("git_commit", "Short git commit hash of the build"),
    ("features", "Active Cargo feature flags"),
    ("config_hash", "SHA-256 of env fields for dedup"),
];

const BENCH_PROTOCOL_SCHEMA: &str = "pi.bench.protocol.v1";
const BENCH_PROTOCOL_VERSION: &str = "1.0.0";
const PARTITION_MATCHED_STATE: &str = "matched-state";
const PARTITION_REALISTIC: &str = "realistic";
const PARTITION_WEIGHT_MATCHED_STATE: f64 = 0.3;
const PARTITION_WEIGHT_REALISTIC: f64 = 0.7;
const EVIDENCE_CLASS_MEASURED: &str = "measured";
const EVIDENCE_CLASS_INFERRED: &str = "inferred";
const CONFIDENCE_HIGH: &str = "high";
const CONFIDENCE_MEDIUM: &str = "medium";
const CONFIDENCE_LOW: &str = "low";
const MEASUREMENT_METHOD_WALL_CLOCK: &str = "wall_clock_observation";
const MEASUREMENT_METHOD_SYNTHETIC: &str = "synthetic_seed_projection";
const HOST_PAGE_CACHE_UNCONTROLLED: &str = "uncontrolled";
const REGRESSION_GATE_ALLOWED_BOUNDARIES: &[&str] = &[
    "in_process_preview",
    "production_extension_manager",
    "production_extension_runtime",
    "synthetic_seed_generation",
    "synthetic_seed_projection",
];
const REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES: &[&str] = &[
    "disabled",
    "not_applicable",
    "not_applicable_synthetic",
    "unique_per_scenario_shared_across_warmup_and_runs",
];
const REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES: &[&str] = &[
    "not_applicable_measured_region",
    "not_applicable_synthetic",
    HOST_PAGE_CACHE_UNCONTROLLED,
];
const REGRESSION_GATE_ELIGIBLE_BOUNDARIES: &[&str] = &[
    "production_extension_manager",
    "production_extension_runtime",
];
const REGRESSION_GATE_REQUIRED_RECORD_FIELDS: &[&str] = &[
    "evidence_class",
    "confidence",
    "eligible_for_regression_gate",
    "measurement_method",
    "measurement_boundary",
    "measurement_contract_version",
    "disk_cache_policy",
];
const REGRESSION_GATE_REQUIRED_ELIGIBLE_PROVENANCE_FIELDS: &[&str] = &[
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
const REGRESSION_GATE_LOAD_REQUIRED_RECORD_FIELDS: &[&str] =
    &["disk_cache_policy", "host_page_cache_policy"];
const REGRESSION_GATE_POSITIVE_SAMPLE_FIELDS: &[&str] = &["runs", "iterations", "total_calls"];
const REGRESSION_GATE_GENERIC_SCOPE: &str = "generic_benchmark_record";
const PIJS_GATE_SCOPE: &str = "pi.perf.workload.v1/pijs_workload";
const PIJS_GATE_SCHEMA: &str = "pi.perf.workload.v1";
const PIJS_GATE_TOOL: &str = "pijs_workload";
const PIJS_GATE_SCENARIO: &str = "tool_call_roundtrip";
const PIJS_GATE_RUNTIME_ENGINE: &str = "quickjs";
const PIJS_GATE_BUILD_PROFILE: &str = "perf";
const PIJS_GATE_ITERATIONS: u64 = 2_000;
const PIJS_GATE_TOOL_CALL_COUNTS: &[u64] = &[1, 10];
const PIJS_GATE_MEASUREMENT_BOUNDARY: &str = "production_extension_manager";
const PIJS_GATE_MEASUREMENT_CONTRACT_VERSION: &str = "production_extension_manager.v1";
const PIJS_GATE_REQUIRED_RECORD_FIELDS: &[&str] = &[
    "timestamp",
    "run_id",
    "correlation_id",
    "source_commit",
    "source_dirty",
    "binary_path",
    "executable_build_profile",
    "executable_profile_verified",
    "binary_sha256",
    "build_profile_verified",
    "build_fingerprint_contract",
    "build_fingerprint_verified",
    "debug_assertions",
    "compiled_profile_family",
    "compiled_opt_level",
    "compiled_debug",
    "compiled_features",
    "config_hash",
    "allocator_requested",
    "allocator_request_source",
    "allocator_effective",
    "allocator_fallback_reason",
    "elapsed_us",
    "elapsed_us_f64",
    "per_call_us_f64",
];
const EXT_STRATIFICATION_SCHEMA: &str = "pi.perf.extension_benchmark_stratification.v1";
const PHASE1_MATRIX_SCHEMA: &str = "pi.perf.phase1_matrix_validation.v1";
const RESOURCE_GOVERNOR_ADMISSION_SCHEMA: &str = "pi.resource_governor.admission.v1";
const REALISTIC_SESSION_SIZES: &[u64] = &[100_000, 200_000, 500_000, 1_000_000, 5_000_000];
const USER_PERCEIVED_SLI_IDS: &[&str] = &[
    "interactive_turn_p95_ms",
    "resume_session_p95_ms",
    "extension_dispatch_p95_ms",
    "tool_roundtrip_p95_ms",
    "tail_stability_p99_over_p50_ratio",
];
const SWARM_LATENCY_QUANTILES: &[&str] = &["p50", "p95", "p99", "p999"];
const SWARM_QUEUE_DEPTH_QUANTILES: &[&str] = &["p50", "p95", "p99", "p999", "max"];
const SWARM_RESOURCE_USAGE_KEYS: &[&str] = &["rss_mb", "cpu_pct"];
const SWARM_COMPONENT_BREAKDOWN_KEYS: &[&str] = &["tool", "provider", "extension", "session"];
const SWARM_STAGE_BREAKDOWN_KEYS: &[&str] = &["open", "append", "save", "index"];
const SWARM_HOST_CAPACITY_KEYS: &[&str] =
    &["target_cpu_cores", "observed_cpu_cores", "mem_total_mb"];
const SWARM_FAIL_CLOSED_REASON_PREFIX: &str = "missing_swarm_metrics";

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn schema_doc_generation_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim() == "1")
}

fn schema_doc_generation_requested() -> bool {
    schema_doc_generation_enabled(
        std::env::var("PI_GENERATE_BENCH_SCHEMA_DOCS")
            .ok()
            .as_deref(),
    )
}

fn read_jsonl_file(path: &Path) -> Result<Vec<Value>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut records = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(line).map_err(|err| {
            format!(
                "{} line {} is not valid JSON: {err}",
                path.display(),
                line_index + 1
            )
        })?;
        records.push(record);
    }
    Ok(records)
}

fn read_jsonl_file_or_panic(path: &Path) -> Vec<Value> {
    read_jsonl_file(path).unwrap_or_else(|err| panic!("{err}"))
}

fn resolve_bench_target_dir(root: &Path, raw_target_dir: Option<&std::ffi::OsStr>) -> PathBuf {
    raw_target_dir.map_or_else(
        || root.join("target"),
        |raw| {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        },
    )
}

fn dedup_bench_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn pijs_schema_candidate_paths_in_target_dir(target_dir: &Path) -> Vec<PathBuf> {
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

fn bench_target_dirs_for(
    root: &Path,
    canonical_project_root: &Path,
    raw_target_dir: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    if root == canonical_project_root {
        vec![resolve_bench_target_dir(root, raw_target_dir)]
    } else {
        vec![root.join("target")]
    }
}

fn pijs_schema_candidate_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut evidence_dirs = std::env::var_os("PERF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(raw) = std::env::var_os("PERF_EVIDENCE_DIRS") {
        evidence_dirs.extend(std::env::split_paths(&raw));
    }
    for raw in evidence_dirs {
        let dir = if raw.is_absolute() {
            raw
        } else {
            root.join(raw)
        };
        for relative in [
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
        ] {
            paths.push(dir.join(relative));
        }
    }
    for target_dir in bench_target_dirs_for(
        root,
        &project_root(),
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
    ) {
        paths.extend(pijs_schema_candidate_paths_in_target_dir(&target_dir));
    }
    dedup_bench_paths(paths)
}

fn load_selected_pijs_schema_artifact(
    root: &Path,
) -> Result<Option<(PathBuf, Vec<Value>)>, String> {
    load_selected_pijs_schema_artifact_from_paths(&pijs_schema_candidate_paths(root))
}

fn load_selected_pijs_schema_artifact_from_paths(
    paths: &[PathBuf],
) -> Result<Option<(PathBuf, Vec<Value>)>, String> {
    for path in paths {
        if !path.exists() {
            continue;
        }
        let events = read_jsonl_file(path)?;
        if events.is_empty() {
            return Err(format!(
                "selected PiJS artifact {} contains no nonblank JSON records",
                path.display()
            ));
        }
        return Ok(Some((path.clone(), events)));
    }
    Ok(None)
}

fn has_required_fields(record: &Value, fields: &[&str]) -> Vec<String> {
    let mut missing = Vec::new();
    for field in fields {
        if record.get(*field).is_none() {
            missing.push((*field).to_string());
        }
    }
    missing
}

#[test]
fn strict_jsonl_reader_rejects_every_nonblank_malformed_line() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().join("artifact.jsonl");
    fs::write(&path, "{\"ok\":true}\n\n{not-json\n{\"later\":true}\n")
        .expect("write malformed JSONL fixture");

    let err = read_jsonl_file(&path).expect_err("malformed nonblank line must fail closed");
    assert!(err.contains("line 3 is not valid JSON"), "{err}");
}

#[test]
fn schema_doc_generation_is_explicitly_opt_in() {
    assert!(!schema_doc_generation_enabled(None));
    assert!(!schema_doc_generation_enabled(Some("")));
    assert!(!schema_doc_generation_enabled(Some("0")));
    assert!(schema_doc_generation_enabled(Some("1")));
}

#[test]
fn pijs_schema_selection_rejects_empty_or_malformed_canonical_before_fallback() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let canonical = tmp.path().join("pijs_workload_perf.jsonl");
    let fallback = tmp.path().join("pijs_workload.jsonl");
    fs::write(&fallback, "{\"schema\":\"fallback\"}\n").expect("write fallback");

    fs::write(&canonical, " \n\n").expect("write empty canonical");
    let err = load_selected_pijs_schema_artifact_from_paths(&[canonical.clone(), fallback.clone()])
        .expect_err("empty selected canonical must not fall back");
    assert!(err.contains("contains no nonblank JSON records"), "{err}");

    fs::write(&canonical, "{not-json\n").expect("write malformed canonical");
    let err = load_selected_pijs_schema_artifact_from_paths(&[canonical, fallback])
        .expect_err("malformed selected canonical must not fall back");
    assert!(err.contains("line 1 is not valid JSON"), "{err}");
}

#[test]
fn pijs_schema_candidates_prioritize_canonical_perf_artifact() {
    let target_dir = Path::new("/tmp/pi-target");
    let paths = pijs_schema_candidate_paths_in_target_dir(target_dir);
    assert_eq!(
        paths.first(),
        Some(&target_dir.join("perf/perf/pijs_workload_perf.jsonl"))
    );
    assert_eq!(
        paths.get(3),
        Some(&target_dir.join("perf/pijs_workload.jsonl"))
    );
}

#[test]
fn pijs_schema_target_selection_is_explicit_and_hermetic() {
    let project = Path::new("/workspace/pi_agent_rust");
    let explicit = std::ffi::OsStr::new("/data/tmp/pi-schema-target");
    assert_eq!(
        bench_target_dirs_for(project, project, Some(explicit)),
        vec![PathBuf::from("/data/tmp/pi-schema-target")]
    );

    let fixture = Path::new("/tmp/pi-schema-fixture");
    assert_eq!(
        bench_target_dirs_for(fixture, project, Some(explicit)),
        vec![fixture.join("target")],
        "fixture schema selection must not inherit the real project's artifacts"
    );
}

#[cfg(unix)]
fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("pi-{prefix}-{nanos}"))
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, content).expect("write executable stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("set executable permission");
}

#[cfg(unix)]
fn install_fake_persistence_fault_toolchain(bin_dir: &Path) {
    let rch_stub = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" != "exec" || "${2:-}" != "--" ]]; then
  echo "unexpected fake rch invocation: $*" >&2
  exit 64
fi
if [[ "${RCH_REQUIRE_REMOTE:-0}" != "true" && "${RCH_REQUIRE_REMOTE:-0}" != "1" ]]; then
  echo "persistence runner did not require remote execution" >&2
  exit 65
fi
for key in CI_CORRELATION_ID RUST_LOG TEST_LOG_JSONL_PATH TEST_ARTIFACT_INDEX_PATH; do
  case ",${RCH_ENV_ALLOWLIST:-}," in
    *",$key,"*) ;;
    *)
      echo "RCH_ENV_ALLOWLIST omitted $key" >&2
      exit 66
      ;;
  esac
done
shift 2
PI_FAKE_RCH_EXECUTED=1 exec "$@"
"#;
    write_executable(&bin_dir.join("rch"), rch_stub);

    let cargo_stub = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${PI_FAKE_RCH_EXECUTED:-0}" != "1" ]]; then
  echo "cargo bypassed fake rch" >&2
  exit 67
fi
if [[ "${TEST_LOG_JSONL_PATH:-}" != "junit.xml" ]]; then
  echo "unexpected remote test log path: ${TEST_LOG_JSONL_PATH:-unset}" >&2
  exit 68
fi
if [[ "${TEST_ARTIFACT_INDEX_PATH:-}" != "test-results.xml" ]]; then
  echo "unexpected remote artifact index path: ${TEST_ARTIFACT_INDEX_PATH:-unset}" >&2
  exit 69
fi
case " $* " in
  *" --exact "*) ;;
  *)
    echo "persistence runner omitted libtest --exact: $*" >&2
    exit 70
    ;;
esac

case " $* " in
  *" jsonl_fault_injection_flush_windows_preserve_integrity "*)
    case_id="jsonl"
    test_name="e2e_jsonl_fault_injection_flush_windows"
    fault_message="jsonl mid-flush failure"
    summary_name="jsonl-fault-window-summary.json"
    summary_windows='{"pre_flush":["jsonl-base"],"mid_flush":["jsonl-base","jsonl-midflush-pending"],"post_flush":["jsonl-base","jsonl-midflush-pending","jsonl-postflush-persisted"]}'
    ;;
  *" sqlite_fault_injection_flush_windows_preserve_integrity "*)
    case_id="sqlite"
    test_name="e2e_sqlite_fault_injection_flush_windows"
    fault_message="sqlite mid-flush failure"
    summary_name="sqlite-fault-window-summary.json"
    summary_windows='{"pre_flush":["sqlite-base"],"mid_flush":["sqlite-base"],"post_flush":["sqlite-base","sqlite-postflush-persisted"]}'
    if [[ "${PI_FAKE_OMIT_SQLITE_REPORTS:-0}" == "1" ]]; then
      printf '%s\n' "$case_id" >>"${PI_FAKE_INVOCATION_LOG:?}"
      exit 0
    fi
    ;;
  *)
    echo "unexpected fake cargo test: $*" >&2
    exit 71
    ;;
esac

printf '%s\n' "$case_id" >>"${PI_FAKE_INVOCATION_LOG:?}"
summary_correlation="${CI_CORRELATION_ID:?}"
if [[ "${PI_FAKE_WRONG_SUMMARY_IDENTITY:-0}" == "1" ]]; then
  summary_correlation="stale-summary-correlation"
fi
summary_payload="{\"schema\":\"pi.e2e.persistence_fault_case_summary.v1\",\"case_id\":\"$case_id\",\"test_name\":\"$test_name\",\"correlation_id\":\"$summary_correlation\",\"scenario\":\"${case_id}_fault_windows\",\"windows\":$summary_windows}"
summary_size="$(python3 -c 'import sys; print(len(sys.argv[1].encode()))' "$summary_payload")"
summary_sha="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$summary_payload")"
summary_base64="$(python3 -c 'import base64,sys; print(base64.b64encode(sys.argv[1].encode()).decode())' "$summary_payload")"
if [[ "${PI_FAKE_TAMPERED_SUMMARY_PAYLOAD:-0}" == "1" ]]; then
  summary_base64="$(python3 -c 'import base64; print(base64.b64encode(b"{}").decode())')"
fi
artifact_record="{\"schema\":\"pi.test.artifact.v1\",\"type\":\"artifact\",\"test\":\"$test_name\",\"seq\":3,\"ts\":\"2026-08-25T00:00:00Z\",\"t_ms\":0,\"name\":\"$summary_name\",\"path\":\"/tmp/$summary_name\",\"size_bytes\":$summary_size,\"sha256\":\"$summary_sha\"}"
if [[ "${PI_FAKE_MALFORMED_ARTIFACT_INDEX:-0}" == "1" ]]; then
  artifact_record="{\"schema\":\"pi.test.artifact.v1\",\"test\":\"$test_name\",\"name\":\"$summary_name\"}"
fi
diagnostic_test_name="$test_name"
if [[ "${PI_FAKE_WRONG_TEST_LOG_IDENTITY:-0}" == "1" ]]; then
  diagnostic_test_name="wrong-$test_name"
fi
log_record="{\"schema\":\"pi.test.log.v2\",\"type\":\"log\",\"test\":\"$diagnostic_test_name\",\"trace_id\":\"trace-$case_id\",\"ci_correlation_id\":\"${CI_CORRELATION_ID:?}\",\"seq\":1,\"ts\":\"2026-08-25T00:00:00Z\",\"t_ms\":0,\"level\":\"info\",\"category\":\"fault\",\"message\":\"$fault_message\"}"
if [[ "${PI_FAKE_MALFORMED_TEST_LOG:-0}" == "1" ]]; then
  log_record="{\"schema\":\"pi.test.log.v2\",\"type\":\"log\",\"test\":\"$test_name\",\"ci_correlation_id\":\"${CI_CORRELATION_ID:?}\",\"category\":\"fault\",\"message\":\"$fault_message\"}"
fi
payload_record="{\"schema\":\"pi.test.log.v2\",\"type\":\"log\",\"test\":\"$diagnostic_test_name\",\"trace_id\":\"trace-$case_id\",\"ci_correlation_id\":\"${CI_CORRELATION_ID:?}\",\"seq\":2,\"ts\":\"2026-08-25T00:00:00Z\",\"t_ms\":0,\"level\":\"info\",\"category\":\"artifact_payload\",\"message\":\"inline JSON artifact bytes\",\"context\":{\"artifact_name\":\"$summary_name\",\"content_encoding\":\"base64\",\"content_sha256\":\"$summary_sha\",\"content_base64\":\"$summary_base64\"}}"
cat >"$TEST_LOG_JSONL_PATH" <<JSON
$log_record
$payload_record
$artifact_record
JSON
printf '%s\n' "$artifact_record" >"$TEST_ARTIFACT_INDEX_PATH"
"#;
    write_executable(&bin_dir.join("cargo"), cargo_stub);
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum FakePersistenceFault {
    None,
    OmitSqliteReports,
    MalformedArtifactIndex,
    MalformedTestLog,
    TamperedSummaryPayload,
    WrongTestLogIdentity,
    WrongSummaryIdentity,
}

#[cfg(unix)]
fn run_persistence_fault_runner_with_fake_rch(
    temp_root: &Path,
    correlation_id: &str,
    fault: FakePersistenceFault,
) -> std::process::Output {
    let bin_dir = temp_root.join("bin");
    let target_dir = temp_root.join("target");
    let tmp_dir = temp_root.join("tmp");
    let artifact_dir = temp_root.join("artifacts");
    let invocation_log = temp_root.join("invocations.log");
    fs::create_dir_all(&bin_dir).expect("create fake persistence bin dir");
    fs::create_dir_all(&target_dir).expect("create fake persistence target dir");
    fs::create_dir_all(&tmp_dir).expect("create fake persistence tmp dir");
    install_fake_persistence_fault_toolchain(&bin_dir);

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg("scripts/e2e/run_persistence_fault_injection.sh")
        .current_dir(project_root())
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("TMPDIR", &tmp_dir)
        .env("E2E_ARTIFACT_DIR", &artifact_dir)
        .env("CI_CORRELATION_ID", correlation_id)
        .env("PERSISTENCE_CARGO_RUNNER", "rch")
        .env("PERSISTENCE_MIN_REPO_FREE_MB", "1")
        .env("PERSISTENCE_MIN_TMP_FREE_MB", "1")
        .env("PI_FAKE_INVOCATION_LOG", &invocation_log);
    match fault {
        FakePersistenceFault::None => {}
        FakePersistenceFault::OmitSqliteReports => {
            command.env("PI_FAKE_OMIT_SQLITE_REPORTS", "1");
        }
        FakePersistenceFault::MalformedArtifactIndex => {
            command.env("PI_FAKE_MALFORMED_ARTIFACT_INDEX", "1");
        }
        FakePersistenceFault::MalformedTestLog => {
            command.env("PI_FAKE_MALFORMED_TEST_LOG", "1");
        }
        FakePersistenceFault::TamperedSummaryPayload => {
            command.env("PI_FAKE_TAMPERED_SUMMARY_PAYLOAD", "1");
        }
        FakePersistenceFault::WrongTestLogIdentity => {
            command.env("PI_FAKE_WRONG_TEST_LOG_IDENTITY", "1");
        }
        FakePersistenceFault::WrongSummaryIdentity => {
            command.env("PI_FAKE_WRONG_SUMMARY_IDENTITY", "1");
        }
    }
    command.output().expect("run persistence fault runner")
}

#[cfg(unix)]
#[test]
fn persistence_fault_runner_retrieves_current_rch_diagnostics_and_fails_closed() {
    let runner =
        fs::read_to_string(project_root().join("scripts/e2e/run_persistence_fault_injection.sh"))
            .expect("read persistence fault runner");
    assert!(
        runner.contains("time.monotonic_ns() // 1_000_000"),
        "duration measurement must use a portable monotonic clock"
    );
    assert!(
        !runner.contains("date +%s%N"),
        "the runner must not depend on GNU date nanoseconds"
    );
    for token in ["fcntl.LOCK_EX", "RUN_NONCE", "PERSISTENCE_REPORT_LOCK_HELD"] {
        assert!(
            runner.contains(token),
            "concurrent RCH report retrieval guard must include {token}"
        );
    }

    let success_root = unique_temp_dir("persistence-rch-success");
    let success_correlation = "persistence-rch-success-correlation";
    let success = run_persistence_fault_runner_with_fake_rch(
        &success_root,
        success_correlation,
        FakePersistenceFault::None,
    );
    assert!(
        success.status.success(),
        "fake RCH evidence run should pass. stdout={}\nstderr={}",
        String::from_utf8_lossy(&success.stdout),
        String::from_utf8_lossy(&success.stderr)
    );
    let success_summary: Value = serde_json::from_slice(
        &fs::read(success_root.join("artifacts/integrity-summary.json"))
            .expect("read successful integrity summary"),
    )
    .expect("parse successful integrity summary");
    assert_eq!(success_summary["run_id"], success_correlation);
    assert_eq!(success_summary["correlation_id"], success_correlation);
    assert!(
        success_summary["source_commit"]
            .as_str()
            .is_some_and(|value| value.len() == 40),
        "persistence summary must bind the full source commit"
    );
    assert!(
        success_summary["source_dirty"].is_boolean(),
        "persistence summary must bind source dirty state"
    );
    assert!(
        success_summary["source_tree_sha256"]
            .as_str()
            .is_some_and(|value| value.len() == 64),
        "persistence summary must bind the exact tracked and untracked source bytes"
    );
    assert_eq!(success_summary["source_tree_stable"], true);
    assert_eq!(success_summary["overall_passed"], true);
    for case in success_summary["cases"]
        .as_array()
        .expect("successful summary cases")
    {
        assert_eq!(case["checks"]["correlation_id_current"], true);
        assert_eq!(case["checks"]["test_identity_current"], true);
        assert_eq!(case["checks"]["result_identity_current"], true);
        assert_eq!(case["checks"]["diagnostic_log_schema_valid"], true);
        assert_eq!(case["checks"]["artifact_index_schema_valid"], true);
        assert_eq!(case["test_log_records"], 2);
        assert_eq!(case["artifact_records"], 1);
        assert_eq!(case["checks"]["summary_artifact_bytes_verified"], true);
        assert_eq!(case["checks"]["summary_artifact_path_confined"], true);
    }
    for (case_id, summary_name) in [
        ("jsonl", "jsonl-fault-window-summary.json"),
        ("sqlite", "sqlite-fault-window-summary.json"),
    ] {
        let case_dir = success_root.join("artifacts").join(case_id);
        let artifact_index = fs::read_to_string(case_dir.join("artifact-index.jsonl"))
            .expect("read canonical artifact index");
        let summary_record: Value = serde_json::from_str(
            artifact_index
                .lines()
                .find(|line| line.contains(summary_name))
                .expect("summary artifact row"),
        )
        .expect("parse summary artifact row");
        let indexed_path = Path::new(
            summary_record["path"]
                .as_str()
                .expect("canonical summary path"),
        );
        assert_eq!(indexed_path.parent(), Some(case_dir.as_path()));
        assert!(
            indexed_path.is_file(),
            "canonical summary artifact must exist"
        );
        assert_eq!(
            indexed_path.file_name().and_then(|name| name.to_str()),
            Some(summary_name)
        );
        assert_eq!(
            summary_record["remote_path"],
            format!("/tmp/{summary_name}"),
            "remote-only path must be preserved as provenance, not retained as the canonical path"
        );
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(success_root.join("artifacts/run-manifest.json"))
            .expect("read successful run manifest"),
    )
    .expect("parse successful run manifest");
    assert_eq!(manifest["rch_require_remote"], true);
    assert_eq!(
        fs::read_to_string(success_root.join("invocations.log"))
            .expect("read successful invocation log"),
        "jsonl\nsqlite\n"
    );

    let missing_root = unique_temp_dir("persistence-rch-missing");
    let missing = run_persistence_fault_runner_with_fake_rch(
        &missing_root,
        "persistence-rch-missing-correlation",
        FakePersistenceFault::OmitSqliteReports,
    );
    assert!(
        !missing.status.success(),
        "missing RCH diagnostics must fail the aggregate runner"
    );
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("RCH did not retrieve junit.xml for case 'sqlite'"),
        "missing-report failure should identify the absent RCH artifact. stderr={}",
        String::from_utf8_lossy(&missing.stderr)
    );
    let missing_summary: Value = serde_json::from_slice(
        &fs::read(missing_root.join("artifacts/integrity-summary.json"))
            .expect("read missing-report integrity summary"),
    )
    .expect("parse missing-report integrity summary");
    assert_eq!(missing_summary["overall_passed"], false);
    assert_eq!(missing_summary["cases"][0]["passed"], true);
    assert_eq!(missing_summary["cases"][1]["passed"], false);

    let malformed_root = unique_temp_dir("persistence-rch-malformed-artifact");
    let malformed = run_persistence_fault_runner_with_fake_rch(
        &malformed_root,
        "persistence-rch-malformed-correlation",
        FakePersistenceFault::MalformedArtifactIndex,
    );
    assert!(
        !malformed.status.success(),
        "malformed artifact evidence must fail closed"
    );
    let malformed_summary: Value = serde_json::from_slice(
        &fs::read(malformed_root.join("artifacts/integrity-summary.json"))
            .expect("read malformed-evidence integrity summary"),
    )
    .expect("parse malformed-evidence integrity summary");
    assert_eq!(malformed_summary["overall_passed"], false);
    for case in malformed_summary["cases"]
        .as_array()
        .expect("malformed-evidence summary cases")
    {
        assert_eq!(
            case["checks"]["summary_artifact_schema_valid"], false,
            "schema-incomplete artifact rows must be rejected"
        );
    }

    let malformed_log_root = unique_temp_dir("persistence-rch-malformed-log");
    let malformed_log = run_persistence_fault_runner_with_fake_rch(
        &malformed_log_root,
        "persistence-rch-malformed-log-correlation",
        FakePersistenceFault::MalformedTestLog,
    );
    assert!(
        !malformed_log.status.success(),
        "schema-incomplete diagnostic logs must fail closed"
    );
    let malformed_log_summary: Value = serde_json::from_slice(
        &fs::read(malformed_log_root.join("artifacts/integrity-summary.json"))
            .expect("read malformed-log integrity summary"),
    )
    .expect("parse malformed-log integrity summary");
    for case in malformed_log_summary["cases"]
        .as_array()
        .expect("malformed-log summary cases")
    {
        assert_eq!(
            case["checks"]["diagnostic_log_schema_valid"], false,
            "missing trace/sequence/time/level fields must be rejected"
        );
    }

    let tampered_payload_root = unique_temp_dir("persistence-rch-tampered-payload");
    let tampered_payload = run_persistence_fault_runner_with_fake_rch(
        &tampered_payload_root,
        "persistence-rch-tampered-payload-correlation",
        FakePersistenceFault::TamperedSummaryPayload,
    );
    assert!(
        !tampered_payload.status.success(),
        "summary bytes that do not match the indexed hash must fail closed"
    );
    let tampered_payload_summary: Value = serde_json::from_slice(
        &fs::read(tampered_payload_root.join("artifacts/integrity-summary.json"))
            .expect("read tampered-payload integrity summary"),
    )
    .expect("parse tampered-payload integrity summary");
    for case in tampered_payload_summary["cases"]
        .as_array()
        .expect("tampered-payload summary cases")
    {
        assert_eq!(
            case["checks"]["summary_artifact_bytes_verified"], false,
            "retrieved inline bytes must match the remote artifact digest"
        );
    }

    let wrong_test_root = unique_temp_dir("persistence-rch-wrong-test");
    let wrong_test = run_persistence_fault_runner_with_fake_rch(
        &wrong_test_root,
        "persistence-rch-wrong-test-correlation",
        FakePersistenceFault::WrongTestLogIdentity,
    );
    assert!(
        !wrong_test.status.success(),
        "current-correlation diagnostics from the wrong test must fail closed"
    );
    let wrong_test_summary: Value = serde_json::from_slice(
        &fs::read(wrong_test_root.join("artifacts/integrity-summary.json"))
            .expect("read wrong-test integrity summary"),
    )
    .expect("parse wrong-test integrity summary");
    for case in wrong_test_summary["cases"]
        .as_array()
        .expect("wrong-test summary cases")
    {
        assert_eq!(case["checks"]["test_identity_current"], false);
        assert_eq!(case["checks"]["diagnostic_log_schema_valid"], false);
        assert_eq!(case["checks"]["summary_artifact_bytes_verified"], false);
    }

    let wrong_summary_root = unique_temp_dir("persistence-rch-wrong-summary");
    let wrong_summary = run_persistence_fault_runner_with_fake_rch(
        &wrong_summary_root,
        "persistence-rch-wrong-summary-correlation",
        FakePersistenceFault::WrongSummaryIdentity,
    );
    assert!(
        !wrong_summary.status.success(),
        "self-consistent summary bytes with stale embedded identity must fail closed"
    );
    let wrong_summary_report: Value = serde_json::from_slice(
        &fs::read(wrong_summary_root.join("artifacts/integrity-summary.json"))
            .expect("read wrong-summary integrity summary"),
    )
    .expect("parse wrong-summary integrity summary");
    for case in wrong_summary_report["cases"]
        .as_array()
        .expect("wrong-summary cases")
    {
        assert_eq!(case["checks"]["test_identity_current"], true);
        assert_eq!(case["checks"]["diagnostic_log_schema_valid"], true);
        assert_eq!(case["checks"]["summary_artifact_schema_valid"], true);
        assert_eq!(case["checks"]["summary_artifact_bytes_verified"], false);
    }
}

#[cfg(unix)]
const FAKE_ORCHESTRATE_SOURCE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
#[cfg(unix)]
const FAKE_ORCHESTRATE_CORRELATION_ID: &str = "bench-schema-orchestrate-correlation";

#[cfg(unix)]
#[allow(clippy::literal_string_with_formatting_args)] // bash ${VAR} syntax, not Rust fmt
fn install_fake_orchestrate_toolchain(bin_dir: &Path) {
    let cargo_stub = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${PI_FAKE_RCH_EXECUTED:-0}" != "1" ]]; then
  echo "cargo bypassed the required rch exec path" >&2
  exit 66
fi
target_dir="${CARGO_TARGET_DIR:-target}"
test_name=""
bench_name=""
for ((i=1; i<=$#; i++)); do
  if [[ "${!i}" == "--test" ]]; then
    j=$((i+1))
    if [[ $j -le $# ]]; then
      test_name="${!j}"
    fi
  fi
  if [[ "${!i}" == "--bench" ]]; then
    j=$((i+1))
    if [[ $j -le $# ]]; then
      bench_name="${!j}"
    fi
  fi
done

mkdir -p "$target_dir/perf"

if [[ -n "$bench_name" ]]; then
  criterion_root="$target_dir/criterion/${PI_CRITERION_OUTPUT_SUBDIR:?}"
  mkdir -p "$criterion_root/report"
  printf '%s\n' '<html>current isolated fixture report</html>' >"$criterion_root/report/index.html"
  printf '%s\n' '[bench-env] os=linux arch=x86_64 cpu="fixture" cores=8 mem_mb=1024 governor=performance turbo=disabled aslr=disabled thp=never noise_score=0 config_hash=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' >&2
  write_estimate() {
    local path="$criterion_root/$1"
    mkdir -p "$(dirname "$path")"
    printf '%s\n' '{"mean":{"point_estimate":1.0},"median":{"point_estimate":1.0},"median_abs_dev":{"point_estimate":0.0}}' >"$path"
  }
  case "$bench_name" in
    tools)
      write_estimate "truncation/head/1000/new/estimates.json"
      ;;
    extensions)
      for relative in \
        ext_load_init/load_init_cold/hello/new/estimates.json \
        ext_load_init/load_init_cold/pirate/new/estimates.json \
        ext_policy/evaluate/prompt_allow/new/estimates.json \
        ext_policy/evaluate/prompt_prompt/new/estimates.json \
        ext_policy/evaluate/prompt_deny/new/estimates.json \
        ext_policy/evaluate/strict_allow/new/estimates.json \
        ext_policy/evaluate/strict_deny/new/estimates.json \
        ext_policy/evaluate/permissive_allow/new/estimates.json \
        ext_protocol/parse_and_validate/host_call_small/new/estimates.json \
        ext_protocol/parse_and_validate/log_big/new/estimates.json; do
        write_estimate "$relative"
      done
      ;;
    system)
      for relative in \
        startup/version/warm/new/estimates.json \
        startup/help/warm/new/estimates.json \
        startup/list_models/warm/new/estimates.json; do
        write_estimate "$relative"
      done
      ;;
    semantic_context)
      for relative in \
        semantic_context/graph_build_cold/large_workspace/new/estimates.json \
        semantic_context/graph_build_warm/large_workspace/new/estimates.json \
        semantic_context/incremental_update/large_workspace/new/estimates.json \
        semantic_context/planning/large_workspace/new/estimates.json \
        semantic_context/bundle_serialization/large_workspace/new/estimates.json; do
        write_estimate "$relative"
      done
      mkdir -p "$criterion_root/context_intelligence"
      cat >"$criterion_root/context_intelligence/perf_budget.json" <<JSON
{"schema":"pi.semantic_context.performance_budget.v1","generated_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","run_id":"${CI_CORRELATION_ID:?}","correlation_id":"${CI_CORRELATION_ID:?}","source_commit":"${VERGEN_GIT_SHA:?}","source_dirty":false,"environment":{"cargo_target_dir":"$target_dir","tmpdir":"/tmp"},"host":{"os":"linux","arch":"x86_64"},"determinism":{"randomized_file_order_checked":true,"matched":true},"cache_hit_miss":{"cold_graph_build":"miss","warm_graph_build":"hit","incremental_update":"miss"},"metrics":{"context_graph_build_cold_ms":{"value_ms":1.0},"context_graph_build_warm_ms":{"value_ms":1.0},"context_incremental_update_ms":{"value_ms":1.0},"context_planning_ms":{"value_ms":1.0},"context_bundle_serialization_ms":{"value_ms":1.0},"context_bundle_estimated_bytes":{"bytes":8192.0}}}
JSON
      ;;
  esac
fi

case "$test_name" in
  bench_scenario_runner)
    cat >"$target_dir/perf/scenario_runner.jsonl" <<'JSON'
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"cold_start","extension":"hello","stats":{"p95_ms":18.0},"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","partition":"matched-state","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/cold_start","replay_input":{"runs":5}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"tool_call","extension":"hello","per_call_us":33.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","partition":"matched-state","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/tool_call","replay_input":{"iterations":500}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"matched-state","open_ms":48.0,"append_ms":36.0,"save_ms":22.0,"index_ms":11.0,"total_ms":117.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/session_100000","replay_input":{"session_messages":100000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"matched-state","open_ms":62.0,"append_ms":45.0,"save_ms":29.0,"index_ms":13.0,"total_ms":149.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/session_200000","replay_input":{"session_messages":200000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"matched-state","open_ms":91.0,"append_ms":68.0,"save_ms":43.0,"index_ms":18.0,"total_ms":220.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/session_500000","replay_input":{"session_messages":500000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"matched-state","open_ms":136.0,"append_ms":101.0,"save_ms":64.0,"index_ms":24.0,"total_ms":325.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/session_1000000","replay_input":{"session_messages":1000000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"matched-state","open_ms":212.0,"append_ms":158.0,"save_ms":97.0,"index_ms":35.0,"total_ms":502.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"matched-state/session_5000000","replay_input":{"session_messages":5000000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"realistic","open_ms":44.0,"append_ms":32.0,"save_ms":19.0,"index_ms":10.0,"total_ms":105.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"realistic/session_100000","replay_input":{"session_messages":100000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"realistic","open_ms":57.0,"append_ms":41.0,"save_ms":25.0,"index_ms":12.0,"total_ms":135.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"realistic/session_200000","replay_input":{"session_messages":200000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"realistic","open_ms":84.0,"append_ms":61.0,"save_ms":37.0,"index_ms":16.0,"total_ms":198.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"realistic/session_500000","replay_input":{"session_messages":500000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"realistic","open_ms":124.0,"append_ms":90.0,"save_ms":54.0,"index_ms":21.0,"total_ms":289.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"realistic/session_1000000","replay_input":{"session_messages":1000000}}}
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","scenario":"session_workload_matrix","extension":"core","partition":"realistic","open_ms":198.0,"append_ms":146.0,"save_ms":88.0,"index_ms":33.0,"total_ms":465.0,"protocol_schema":"pi.bench.protocol.v1","protocol_version":"1.0.0","evidence_class":"measured","confidence":"high","correlation_id":"stub-correlation","scenario_metadata":{"runtime":"pi_agent_rust","build_profile":"perf","host":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8},"scenario_id":"realistic/session_5000000","replay_input":{"session_messages":5000000}}}
JSON
    if [[ "${PI_FAKE_DROP_INDEX_STAGE_SAMPLE:-0}" == "1" ]]; then
      python3 - "$target_dir/perf/scenario_runner.jsonl" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
rows = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
rewritten = []
dropped = False
for line in rows:
    record = json.loads(line)
    if (
        not dropped
        and record.get("scenario") == "session_workload_matrix"
        and record.get("partition") == "matched-state"
        and record.get("scenario_metadata", {}).get("scenario_id") == "matched-state/session_100000"
    ):
        record.pop("index_ms", None)
        dropped = True
    rewritten.append(json.dumps(record, separators=(",", ":")))
path.write_text("\n".join(rewritten) + ("\n" if rewritten else ""), encoding="utf-8")
PY
    fi
    if [[ "${PI_FAKE_DROP_ALL_STAGE_SAMPLES:-0}" == "1" ]]; then
      python3 - "$target_dir/perf/scenario_runner.jsonl" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
rows = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
rewritten = []
for line in rows:
    record = json.loads(line)
    if record.get("scenario") == "session_workload_matrix":
        for field in ("open_ms", "append_ms", "save_ms", "index_ms"):
            record.pop(field, None)
    rewritten.append(json.dumps(record, separators=(",", ":")))
path.write_text("\n".join(rewritten) + ("\n" if rewritten else ""), encoding="utf-8")
PY
    fi
    cat >"$target_dir/perf/legacy_extension_workloads.jsonl" <<'JSON'
{"schema":"pi.ext.legacy_bench.v1","scenario":"ext_load_init/load_init_cold","extension":"hello","runtime_kind":"node","summary":{"p50_ms":10.0}}
{"schema":"pi.ext.legacy_bench.v1","scenario":"ext_load_init/load_init_cold","extension":"hello","runtime_kind":"bun","summary":{"p50_ms":8.0}}
{"schema":"pi.ext.legacy_bench.v1","scenario":"ext_tool_call/hello","extension":"hello","runtime_kind":"node","per_call_us":20.0}
{"schema":"pi.ext.legacy_bench.v1","scenario":"ext_tool_call/hello","extension":"hello","runtime_kind":"bun","per_call_us":15.0}
{"schema":"pi.ext.legacy_bench.v1","scenario":"full_e2e_long_session","runtime_kind":"node","elapsed_ms":2400.0}
{"schema":"pi.ext.legacy_bench.v1","scenario":"full_e2e_long_session","runtime_kind":"bun","elapsed_ms":1800.0}
JSON
    python3 - "$target_dir/perf/scenario_runner.jsonl" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
rows = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
rewritten = []
for line in rows:
    record = json.loads(line)
    if record.get("scenario") == "session_workload_matrix":
        session_messages = int(record.get("session_messages") or 0)
        total_ms = float(record.get("total_ms") or 0.0)
        queue_p50 = max(1, session_messages // 100000)
        record["swarm_metrics"] = {
            "latency_quantiles_ms": {
                "p50": total_ms,
                "p95": total_ms * 1.15,
                "p99": total_ms * 1.35,
                "p999": total_ms * 1.75,
            },
            "queue_depth": {
                "p50": queue_p50,
                "p95": queue_p50 * 2,
                "p99": queue_p50 * 3,
                "p999": queue_p50 * 4,
                "max": queue_p50 * 4,
            },
            "resource_usage": {
                "rss_mb": 64,
                "cpu_pct": 0.0,
            },
            "component_breakdown_ms": {
                "tool": 0.0,
                "provider": 0.0,
                "extension": 0.0,
                "session": total_ms,
            },
            "stage_breakdown_ms": {
                "open": float(record.get("open_ms") or 0.0),
                "append": float(record.get("append_ms") or 0.0),
                "save": float(record.get("save_ms") or 0.0),
                "index": float(record.get("index_ms") or 0.0),
            },
            "host_capacity": {
                "target_cpu_cores": 64,
                "observed_cpu_cores": 8,
                "mem_total_mb": 262144,
            },
        }
    rewritten.append(json.dumps(record, separators=(",", ":")))
path.write_text("\n".join(rewritten) + ("\n" if rewritten else ""), encoding="utf-8")
PY
    if [[ "${PI_FAKE_DROP_SWARM_METRICS:-0}" == "1" ]]; then
      python3 - "$target_dir/perf/scenario_runner.jsonl" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
rows = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
rewritten = []
for line in rows:
    record = json.loads(line)
    if record.get("scenario") == "session_workload_matrix":
        record.pop("swarm_metrics", None)
    rewritten.append(json.dumps(record, separators=(",", ":")))
path.write_text("\n".join(rewritten) + ("\n" if rewritten else ""), encoding="utf-8")
PY
    fi
    ;;
  ext_bench_harness)
    cat >"$target_dir/perf/ext_bench_harness.jsonl" <<'JSON'
{"schema":"pi.ext.rust_bench.v1","scenario":"cold_load","extension":"hello","success":true,"stats":{"p95_us":18000}}
JSON
    cat >"$target_dir/perf/ext_bench_harness_report.json" <<'JSON'
{"schema":"pi.bench.harness_report.v1","summary":{"total_scenarios":1}}
JSON
    ;;
  perf_bench_harness)
    if [[ -n "${PI_FAKE_PERF_BENCH_INVOCATION_MARKER:-}" ]]; then
      printf '%s\n' invoked >"$PI_FAKE_PERF_BENCH_INVOCATION_MARKER"
    fi
    if [[ "${PI_FAKE_RCH_STRICT_PINNED:-0}" != "1" ]]; then
      echo "perf_bench_harness did not use the clean committed-source pin" >&2
      exit 73
    fi
    args=("$@")
    expected_args=(
      nextest run
      --build-jobs "${CARGO_BUILD_JOBS:?}"
      --test perf_bench_harness
      --cargo-profile perf
      --test-threads 1
      --no-tests fail
      -- bench_extension_scenarios --exact
    )
    if (( ${#args[@]} != ${#expected_args[@]} )); then
      echo "perf_bench_harness used the wrong RCH nextest argv length: $*" >&2
      exit 68
    fi
    for ((i=0; i<${#expected_args[@]}; i++)); do
      if [[ "${args[i]}" != "${expected_args[i]}" ]]; then
        echo "perf_bench_harness used unexpected RCH nextest argv at $i: $*" >&2
        exit 72
      fi
    done
    if [[ "${PI_FAKE_REQUIRE_BENCH_CONTROLS:-0}" == "1" ]] \
      && { [[ "${BENCH_QUICK:-}" != "1" ]] || [[ "${BENCH_ITERATIONS:-}" != "1" ]]; }; then
      echo "perf_bench_harness did not receive benchmark quick/iteration controls" >&2
      exit 75
    fi
    if [[ -z "${BENCH_OUTPUT_TARGET_SUBDIR:-}" ]]; then
      echo "perf_bench_harness omitted BENCH_OUTPUT_TARGET_SUBDIR" >&2
      exit 69
    fi
    if [[ "${PI_FAKE_DROP_RCH_EXTENSION_ARTIFACT:-0}" != "1" ]]; then
      mkdir -p "$target_dir/$BENCH_OUTPUT_TARGET_SUBDIR"
      extension_commit="${VERGEN_GIT_SHA:?}"
      if [[ "${PI_FAKE_WRONG_RCH_EXTENSION_COMMIT:-0}" == "1" ]]; then
        extension_commit="ffffffffffffffffffffffffffffffffffffffff"
      fi
      benchmark_run_id="${PI_BENCH_RUN_ID:?}"
      if [[ "${PI_FAKE_STALE_RCH_EXTENSION_ARTIFACT:-0}" == "1" ]]; then
        benchmark_run_id="stale-benchmark-run"
      fi
      binary_sha256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      binary_path="/target/perf/deps/perf_bench_harness-stub"
      if [[ "${PI_FAKE_WRONG_RCH_EXTENSION_BINARY_PROFILE:-0}" == "1" ]]; then
        binary_path="/target/release/deps/perf_bench_harness-stub"
      fi
      config_hash="$(python3 - "$extension_commit" "$binary_sha256" "$binary_path" <<'PY'
import hashlib
import json
import sys

payload = {
    "binary_path": sys.argv[3],
    "binary_sha256": sys.argv[2],
    "build_fingerprint_contract": "cargo_build_fingerprint.v1",
    "build_fingerprint_verified": True,
    "build_profile": "perf",
    "build_profile_verified": True,
    "compiled_debug": "true",
    "compiled_features": [],
    "compiled_opt_level": "3",
    "compiled_profile_family": "release",
    "debug_assertions": False,
    "executable_build_profile": "perf",
    "executable_profile_verified": True,
    "source_commit": sys.argv[1],
    "source_dirty": False,
}
encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
print(hashlib.sha256(encoded).hexdigest())
PY
)"
      if [[ "${PI_FAKE_INVALID_RCH_EXTENSION_CONFIG_HASH:-0}" == "1" ]]; then
        config_hash="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      fi
      extension_artifact="$target_dir/$BENCH_OUTPUT_TARGET_SUBDIR/extension_bench.jsonl"
      : >"$extension_artifact"
      if [[ "${BENCH_QUICK:-0}" == "1" ]]; then
        extension_names=(hello pirate diff)
      else
        extension_names=(
          hello pirate diff bookmark custom-header custom-footer
          confirm-destructive dirty-repo-guard
        )
      fi
      for extension_name in "${extension_names[@]}"; do
        for scenario_name in cold_start warm_start; do
          cat >>"$extension_artifact" <<JSON
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","run_id":"${CI_CORRELATION_ID:?}","correlation_id":"${CI_CORRELATION_ID:?}","benchmark_run_id":"$benchmark_run_id","source_commit":"$extension_commit","source_dirty":false,"scenario":"$scenario_name","extension":"$extension_name","runs":1,"summary":{"count":1,"min_ms":1.0,"p50_ms":1.0,"p95_ms":1.0,"p99_ms":1.0,"p999_ms":1.0,"max_ms":1.0,"mean_ms":1.0},"elapsed_ms":1.0,"per_call_us":1000.0,"calls_per_sec":1000.0,"env":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8,"mem_total_mb":1024,"build_profile":"perf","executable_build_profile":"perf","executable_profile_verified":true,"build_fingerprint_verified":true,"build_profile_verified":true,"build_fingerprint_contract":"cargo_build_fingerprint.v1","compiled_profile_family":"release","compiled_opt_level":"3","compiled_debug":"true","debug_assertions":false,"git_commit":"$extension_commit","source_dirty":false,"features":[],"binary_path":"$binary_path","binary_sha256":"$binary_sha256","config_hash":"$config_hash"},"timestamp":"$(date -u +%Y-%m-%dT%H:%M:%SZ)"}
JSON
        done
      done
      cat >>"$extension_artifact" <<JSON
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","run_id":"${CI_CORRELATION_ID:?}","correlation_id":"${CI_CORRELATION_ID:?}","benchmark_run_id":"$benchmark_run_id","source_commit":"$extension_commit","source_dirty":false,"scenario":"tool_call","extension":"hello","runs":1,"summary":{"count":1,"min_ms":1.0,"p50_ms":1.0,"p95_ms":1.0,"p99_ms":1.0,"p999_ms":1.0,"max_ms":1.0,"mean_ms":1.0},"elapsed_ms":1.0,"per_call_us":1000.0,"calls_per_sec":1000.0,"env":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8,"mem_total_mb":1024,"build_profile":"perf","executable_build_profile":"perf","executable_profile_verified":true,"build_fingerprint_verified":true,"build_profile_verified":true,"build_fingerprint_contract":"cargo_build_fingerprint.v1","compiled_profile_family":"release","compiled_opt_level":"3","compiled_debug":"true","debug_assertions":false,"git_commit":"$extension_commit","source_dirty":false,"features":[],"binary_path":"$binary_path","binary_sha256":"$binary_sha256","config_hash":"$config_hash"},"timestamp":"$(date -u +%Y-%m-%dT%H:%M:%SZ)"}
JSON
      if [[ "${PI_FAKE_DROP_RCH_EXTENSION_COVERAGE:-0}" != "1" ]]; then
        cat >>"$extension_artifact" <<JSON
{"schema":"pi.ext.rust_bench.v1","runtime":"pi_agent_rust","run_id":"${CI_CORRELATION_ID:?}","correlation_id":"${CI_CORRELATION_ID:?}","benchmark_run_id":"$benchmark_run_id","source_commit":"$extension_commit","source_dirty":false,"scenario":"event_hook","extension":"pirate","runs":1,"summary":{"count":1,"min_ms":1.0,"p50_ms":1.0,"p95_ms":1.0,"p99_ms":1.0,"p999_ms":1.0,"max_ms":1.0,"mean_ms":1.0},"elapsed_ms":1.0,"per_call_us":1000.0,"calls_per_sec":1000.0,"env":{"os":"linux","arch":"x86_64","cpu_model":"stub","cpu_cores":8,"mem_total_mb":1024,"build_profile":"perf","executable_build_profile":"perf","executable_profile_verified":true,"build_fingerprint_verified":true,"build_profile_verified":true,"build_fingerprint_contract":"cargo_build_fingerprint.v1","compiled_profile_family":"release","compiled_opt_level":"3","compiled_debug":"true","debug_assertions":false,"git_commit":"$extension_commit","source_dirty":false,"features":[],"binary_path":"$binary_path","binary_sha256":"$binary_sha256","config_hash":"$config_hash"},"timestamp":"$(date -u +%Y-%m-%dT%H:%M:%SZ)"}
JSON
      fi
      printf '%s\n' '# fake extension benchmark summary' \
        >"$target_dir/$BENCH_OUTPUT_TARGET_SUBDIR/extension_bench_summary.md"
    fi
    cat >"$target_dir/perf/pijs_workload.jsonl" <<'JSON'
{"schema":"pi.perf.workload.v1","scenario":"200x10","iterations":200,"tool_calls_per_iteration":10,"total_calls":2000,"elapsed_ms":1200,"per_call_us":45,"calls_per_sec":1666}
JSON
    ;;
  perf_budgets)
    if [[ "${PI_PERF_POST_GENERATION:-0}" == "1" ]]; then
      if [[ "${PI_FAKE_RCH_STRICT_PINNED:-0}" != "1" \
        || "${PI_FAKE_RCH_HAS_OVERLAY:-0}" != "1" \
        || "${PERF_EVIDENCE_DIR:-}" != "${PI_FAKE_RCH_OVERLAY_PATH:-}" ]]; then
        echo "post-generation perf_budgets must use a clean current-evidence overlay" >&2
        exit 74
      fi
      case " $* " in
        *" ci_enforced_budgets_fail_on_regression_or_missing_data "*" --exact "*) ;;
        *)
          echo "post-generation perf_budgets invocation omitted the exact data-contract test" >&2
          exit 64
          ;;
      esac
      python3 - \
        "${PERF_EVIDENCE_DIR:?}" \
        "${CI_CORRELATION_ID:?}" \
        "${PI_PERF_EXPECTED_SOURCE_COMMIT:?}" <<'PY'
import json
import sys
from pathlib import Path

evidence_dir = Path(sys.argv[1])
expected_correlation_id = sys.argv[2]
expected_source_commit = sys.argv[3]
inventory_path = evidence_dir / "post_generation_evidence_inventory.json"
inventory = json.loads(inventory_path.read_text(encoding="utf-8"))
if inventory.get("schema") != "pi.perf.post_generation_evidence_inventory.v1":
    raise SystemExit("post-generation evidence inventory schema mismatch")
if inventory.get("source_commit") != expected_source_commit:
    raise SystemExit("post-generation evidence inventory source_commit mismatch")
if inventory.get("correlation_id") != expected_correlation_id:
    raise SystemExit("post-generation evidence inventory correlation_id mismatch")
if not inventory.get("entries"):
    raise SystemExit("post-generation evidence inventory is empty")
for name in (
    "extension_benchmark_stratification.json",
    "phase1_matrix_validation.json",
):
    path = evidence_dir / name
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("correlation_id") != expected_correlation_id:
        raise SystemExit(f"{name}: correlation_id mismatch")
marker = {
    "schema": "pi.perf.fake_post_generation_invocation.v1",
    "correlation_id": expected_correlation_id,
    "test_filter": "ci_enforced_budgets_fail_on_regression_or_missing_data",
    "exact": True,
}
print(json.dumps(marker, sort_keys=True))
PY
    fi
    ;;
esac

if [[ -n "${CI_CORRELATION_ID:-}" ]]; then
  python3 - "$target_dir" "$CI_CORRELATION_ID" "$(git rev-parse HEAD)" <<'PY'
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

target_dir = Path(sys.argv[1])
correlation_id = sys.argv[2]
source_commit = sys.argv[3]
generated_at = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
artifacts = (
    ("scenario_runner.jsonl", "orchestration_correlation_id"),
    ("pijs_workload.jsonl", "correlation_id"),
    ("ext_bench_harness.jsonl", "correlation_id"),
    ("legacy_extension_workloads.jsonl", "correlation_id"),
)
for relative_path, correlation_field in artifacts:
    path = target_dir / "perf" / relative_path
    if not path.is_file():
        continue
    records = [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    for record in records:
        record["timestamp"] = generated_at
        record["run_id"] = correlation_id
        record[correlation_field] = correlation_id
        record["source_commit"] = source_commit
        record["source_dirty"] = False
    if (
        relative_path == "scenario_runner.jsonl"
        and records
    ):
        if (
            os.environ.get("PI_FAKE_INJECT_FOREIGN_SCENARIO_ROW") == "1"
            and not any(
                record.get("orchestration_correlation_id") == "foreign-correlation"
                for record in records
            )
        ):
            foreign = dict(records[-1])
            foreign["orchestration_correlation_id"] = "foreign-correlation"
            foreign["total_ms"] = 0.001
            records.append(foreign)
        if os.environ.get("PI_FAKE_INJECT_STALE_SCENARIO_ROW") == "1":
            stale = dict(records[-1])
            stale["timestamp"] = "2000-01-01T00:00:00Z"
            stale["total_ms"] = 0.002
            records.append(stale)
    path.write_text(
        "\n".join(json.dumps(record, separators=(",", ":")) for record in records)
        + "\n",
        encoding="utf-8",
    )
PY
  if [[ "${PI_FAKE_INJECT_MALFORMED_SCENARIO_ROW:-0}" == "1" ]]; then
    printf '{not-json\n' >>"$target_dir/perf/scenario_runner.jsonl"
  fi
fi
exit 0
"#;
    write_executable(&bin_dir.join("cargo"), cargo_stub);
    let rch_stub = r#"#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  check)
    if [[ "${PI_FAKE_RCH_CHECK_OK:-0}" == "1" ]]; then
      exit 0
    fi
    exit 2
    ;;
  exec)
    if [[ "${RCH_REQUIRE_REMOTE:-0}" != "1" ]]; then
      echo "rch exec was not placed in fail-closed proof mode" >&2
      exit 65
    fi
    for key in BENCH_OUTPUT_TARGET_SUBDIR BENCH_QUICK BENCH_ITERATIONS PI_BENCH_RUN_ID CARGO_BUILD_JOBS PI_CRITERION_OUTPUT_SUBDIR; do
      case ",${RCH_ENV_ALLOWLIST:-}," in
        *",$key,"*) ;;
        *)
          echo "RCH_ENV_ALLOWLIST omitted $key" >&2
          exit 66
          ;;
      esac
    done
    shift
    if [[ "${1:-}" == "--no-color" ]]; then
      shift
    fi
    strict_pinned=0
    has_overlay=0
    overlay_path=""
    if [[ "${1:-}" == "--base" ]]; then
      if (( $# < 6 )) \
        || [[ "${2:-}" != "$(git rev-parse HEAD)" ]] \
        || [[ "${3:-}" != "--clean-overlay" ]]; then
        echo "strict RCH execution used an invalid clean committed-source pin" >&2
        exit 67
      fi
      strict_pinned=1
      if [[ "${4:-}" == "--no-overlay" \
        && "${5:-}" == "--" \
        && "${6:-}" == "cargo" ]]; then
        shift 5
      elif [[ "${4:-}" == "--overlay-path" \
        && -n "${5:-}" \
        && "${6:-}" == "--" \
        && "${7:-}" == "cargo" ]]; then
        has_overlay=1
        overlay_path="${5}"
        case "$overlay_path" in
          /*|../*|*/../*|*/..)
            echo "RCH overlay path must be normalized and repo-relative" >&2
            exit 76
            ;;
        esac
        if [[ ! -d "$overlay_path" ]]; then
          echo "RCH overlay path is missing: $overlay_path" >&2
          exit 76
        fi
        if find "$overlay_path" -type d -empty -print -quit | grep -q .; then
          echo "RCH clean-overlay fixture rejects empty directories" >&2
          exit 76
        fi
        shift 6
      else
        echo "strict RCH execution used an invalid overlay contract" >&2
        exit 67
      fi
    elif [[ "${1:-}" == "--" ]]; then
      shift
    else
      echo "unexpected fake RCH exec arguments: $*" >&2
      exit 68
    fi
    if [[ "$has_overlay" == "1" ]]; then
      for key in PERF_EVIDENCE_DIR PI_PERF_POST_GENERATION PI_PERF_EXPECTED_SOURCE_COMMIT CI_CORRELATION_ID PI_PERF_STRICT; do
        case ",${RCH_ENV_ALLOWLIST:-}," in
          *",$key,"*) ;;
          *)
            echo "RCH_ENV_ALLOWLIST omitted $key" >&2
            exit 66
            ;;
        esac
      done
    fi
    if PI_FAKE_RCH_EXECUTED=1 \
      PI_FAKE_RCH_STRICT_PINNED="$strict_pinned" \
      PI_FAKE_RCH_HAS_OVERLAY="$has_overlay" \
      PI_FAKE_RCH_OVERLAY_PATH="$overlay_path" \
      "$@"; then
      if [[ "${PI_FAKE_RCH_LOCAL_FALLBACK:-0}" == "1" ]]; then
        echo "[RCH] local (fixture fallback)" >&2
      else
        echo "[RCH] remote fixture-worker (1.00s)" >&2
        if [[ "$strict_pinned" == "1" ]]; then
          echo "[RCH] clean-overlay receipt: base=$(git rev-parse HEAD) overlay-fingerprint=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" >&2
        fi
      fi
      exit 0
    else
      exit_code=$?
      exit "$exit_code"
    fi
    ;;
  *)
    exit 64
    ;;
esac
"#;
    write_executable(&bin_dir.join("rch"), rch_stub);
    let git_stub = r#"#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  rev-parse)
    if [[ "${PI_FAKE_GIT_IDENTITY_UNAVAILABLE:-0}" == "1" ]]; then
      exit 64
    fi
    call_number=1
    if [[ -n "${PI_FAKE_GIT_REV_PARSE_STATE_FILE:-}" ]]; then
      if [[ -f "$PI_FAKE_GIT_REV_PARSE_STATE_FILE" ]]; then
        read -r call_number <"$PI_FAKE_GIT_REV_PARSE_STATE_FILE"
        call_number=$((call_number + 1))
      fi
      printf '%s\n' "$call_number" >"$PI_FAKE_GIT_REV_PARSE_STATE_FILE"
    fi
    full_commit='0123456789abcdef0123456789abcdef01234567'
    if [[ "${PI_FAKE_GIT_DRIFT_FROM_REV_PARSE_CALL:-0}" -gt 0 \
      && "$call_number" -ge "${PI_FAKE_GIT_DRIFT_FROM_REV_PARSE_CALL}" ]]; then
      full_commit='ffffffffffffffffffffffffffffffffffffffff'
    fi
    if [[ -n "${PI_FAKE_GIT_DRIFT_AFTER_OUTPUT_RELATIVE:-}" \
      && -e "${PERF_OUTPUT_DIR:?}/${PI_FAKE_GIT_DRIFT_AFTER_OUTPUT_RELATIVE}" ]]; then
      full_commit='ffffffffffffffffffffffffffffffffffffffff'
    fi
    if [[ "${2:-}" == "--short" && "${3:-}" == "HEAD" ]]; then
      printf '%s\n' "${full_commit:0:8}"
    elif [[ "${2:-}" == "HEAD" ]]; then
      printf '%s\n' "$full_commit"
    else
      exit 64
    fi
    ;;
  status)
    if [[ "${PI_FAKE_GIT_STATUS_UNAVAILABLE:-0}" == "1" ]]; then
      exit 64
    fi
    if [[ "${PI_FAKE_GIT_DIRTY:-0}" == "1" ]]; then
      printf '%s\n' ' M scripts/perf/orchestrate.sh'
    fi
    ;;
  *)
    exit 64
    ;;
esac
"#;
    write_executable(&bin_dir.join("git"), git_stub);
}

#[cfg(unix)]
fn install_fake_orchestrate_staging_artifacts(target_dir: &Path) {
    fn write_json(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("artifact path has parent"))
            .expect("create fake artifact parent");
        fs::write(path, content).expect("write fake staging artifact");
    }

    let criterion_estimate = r#"{"mean":{"point_estimate":1.0},"median":{"point_estimate":1.0},"median_abs_dev":{"point_estimate":0.0}}"#;
    for path in [
        target_dir.join("criterion/startup/version/warm/new/estimates.json"),
        target_dir.join("criterion/ext_load_init/load_init_cold/hello/new/estimates.json"),
        target_dir.join("criterion/ext_policy/evaluate/hello/new/estimates.json"),
        target_dir.join("criterion/ext_protocol/parse_and_validate/hello/new/estimates.json"),
        target_dir
            .join("criterion/semantic_context/graph_build_cold/large_workspace/new/estimates.json"),
        target_dir
            .join("criterion/semantic_context/graph_build_warm/large_workspace/new/estimates.json"),
        target_dir.join(
            "criterion/semantic_context/incremental_update/large_workspace/new/estimates.json",
        ),
        target_dir.join("criterion/semantic_context/planning/large_workspace/new/estimates.json"),
        target_dir.join(
            "criterion/semantic_context/bundle_serialization/large_workspace/new/estimates.json",
        ),
    ] {
        write_json(&path, criterion_estimate);
    }

    write_json(
        &target_dir.join("perf/context_intelligence/perf_budget.json"),
        r#"{"schema":"pi.semantic_context.performance_budget.v1"}"#,
    );

    let release_pi = target_dir.join("release/pi");
    fs::create_dir_all(release_pi.parent().expect("release path has parent"))
        .expect("create fake release parent");
    write_executable(&release_pi, "#!/usr/bin/env sh\nexit 0\n");

    write_json(
        &target_dir.join("perf/extension_benchmark_stratification.json"),
        r#"{"schema":"pi.perf.extension_benchmark_stratification.v1"}"#,
    );
    write_json(
        &target_dir.join("perf/results/phase1_matrix_validation.json"),
        r#"{"schema":"pi.perf.phase1_matrix_validation.v1"}"#,
    );
}

#[cfg(unix)]
fn install_fake_orchestrate_rch_attestation(target_dir: &Path, output_dir: &Path) {
    let binary_path = target_dir.join("perf/deps/perf_budgets-fake");
    fs::create_dir_all(binary_path.parent().expect("fake binary parent"))
        .expect("create fake perf_budgets binary directory");
    let binary = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" != "ci_enforced_budgets_fail_on_regression_or_missing_data" \
  || "${2:-}" != "--exact" ]]; then
  echo "post-generation perf_budgets invocation omitted the exact data-contract test" >&2
  exit 64
fi
python3 - "${PERF_EVIDENCE_DIR:?}" "${CI_CORRELATION_ID:?}" <<'PY'
import json
import sys
from pathlib import Path

evidence_dir = Path(sys.argv[1])
expected_correlation_id = sys.argv[2]
for name in (
    "extension_benchmark_stratification.json",
    "phase1_matrix_validation.json",
):
    path = evidence_dir / name
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("correlation_id") != expected_correlation_id:
        raise SystemExit(f"{name}: correlation_id mismatch")
print(
    json.dumps(
        {
            "schema": "pi.perf.fake_post_generation_invocation.v1",
            "correlation_id": expected_correlation_id,
            "test_filter": "ci_enforced_budgets_fail_on_regression_or_missing_data",
            "exact": True,
        },
        sort_keys=True,
    )
)
PY
"#;
    write_executable(&binary_path, binary);

    let attestation_path = output_dir.join("results/perf_budgets_test_binary.json");
    fs::create_dir_all(attestation_path.parent().expect("fake attestation parent"))
        .expect("create fake perf_budgets attestation directory");
    let binary_sha256 = sha256_file(&binary_path).expect("hash fake perf_budgets binary");
    fs::write(
        attestation_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.perf.test_binary_attestation.v1",
            "generated_at": "2026-08-24T00:00:00Z",
            "source_commit": FAKE_ORCHESTRATE_SOURCE_COMMIT,
            "source_dirty": false,
            "cargo_profile": "perf",
            "target_name": "perf_budgets",
            "binary_path": binary_path,
            "sha256": binary_sha256,
        }))
        .expect("serialize fake perf_budgets attestation"),
    )
    .expect("write fake perf_budgets attestation");
}

fn canonical_protocol_contract() -> Value {
    let realistic_replay_inputs = REALISTIC_SESSION_SIZES
        .iter()
        .map(|messages| {
            json!({
                "scenario_id": format!("realistic/session_{messages}"),
                "partition": PARTITION_REALISTIC,
                "session_messages": messages,
                "replay_input": {
                    "transcript_fixture": format!("tests/artifacts/perf/session_{messages}.jsonl"),
                    "seed": 7,
                    "mode": "replay",
                },
            })
        })
        .collect::<Vec<_>>();

    let user_perceived_sli_catalog = vec![
        json!({
            "sli_id": "interactive_turn_p95_ms",
            "label": "Interactive turn latency (P95)",
            "unit": "ms",
            "objective": { "comparator": "<=", "threshold": 1200 },
            "ux_interpretation": {
                "good": "Feels responsive for normal coding dialogue.",
                "degraded": "Noticeable lag in turn-to-turn iteration speed.",
                "critical": "Workflow feels blocked; conversation rhythm breaks."
            }
        }),
        json!({
            "sli_id": "resume_session_p95_ms",
            "label": "Session resume latency (P95)",
            "unit": "ms",
            "objective": { "comparator": "<=", "threshold": 1800 },
            "ux_interpretation": {
                "good": "Project/session restore feels immediate after launch.",
                "degraded": "Resume feels sluggish but still tolerable.",
                "critical": "Resume delays materially slow task pickup."
            }
        }),
        json!({
            "sli_id": "extension_dispatch_p95_ms",
            "label": "Extension hostcall dispatch latency (P95)",
            "unit": "ms",
            "objective": { "comparator": "<=", "threshold": 350 },
            "ux_interpretation": {
                "good": "Extension-backed actions feel near-instant.",
                "degraded": "Extension interactions feel sticky/intermittent.",
                "critical": "Extension UX appears stalled or unreliable."
            }
        }),
        json!({
            "sli_id": "tool_roundtrip_p95_ms",
            "label": "Tool-call roundtrip latency (P95)",
            "unit": "ms",
            "objective": { "comparator": "<=", "threshold": 900 },
            "ux_interpretation": {
                "good": "Tool invocation and result handoff feel tight.",
                "degraded": "Tool feedback loop slows coding momentum.",
                "critical": "Tool usage becomes a bottleneck."
            }
        }),
        json!({
            "sli_id": "tail_stability_p99_over_p50_ratio",
            "label": "Tail stability (P99/P50 ratio)",
            "unit": "ratio",
            "objective": { "comparator": "<=", "threshold": 4.0 },
            "ux_interpretation": {
                "good": "Latency is predictable with low surprise spikes.",
                "degraded": "Intermittent long-tail pauses are noticeable.",
                "critical": "Frequent latency spikes disrupt workflow trust."
            }
        }),
    ];

    let mut scenario_sli_matrix = vec![
        json!({
            "partition": PARTITION_MATCHED_STATE,
            "scenario_id": "cold_start",
            "sli_ids": ["interactive_turn_p95_ms", "resume_session_p95_ms", "tail_stability_p99_over_p50_ratio"],
            "phase_validation_beads": ["bd-3ar8v.1.5", "bd-3ar8v.2.11"],
            "ux_outcome": "First interaction after startup feels responsive."
        }),
        json!({
            "partition": PARTITION_MATCHED_STATE,
            "scenario_id": "warm_start",
            "sli_ids": ["interactive_turn_p95_ms", "tail_stability_p99_over_p50_ratio"],
            "phase_validation_beads": ["bd-3ar8v.1.5", "bd-3ar8v.2.11"],
            "ux_outcome": "Steady-state turn latency remains consistently snappy."
        }),
        json!({
            "partition": PARTITION_MATCHED_STATE,
            "scenario_id": "tool_call",
            "sli_ids": ["tool_roundtrip_p95_ms", "interactive_turn_p95_ms", "tail_stability_p99_over_p50_ratio"],
            "phase_validation_beads": ["bd-3ar8v.2.11", "bd-3ar8v.6.7"],
            "ux_outcome": "Tool-assisted coding loop stays fluid."
        }),
        json!({
            "partition": PARTITION_MATCHED_STATE,
            "scenario_id": "event_dispatch",
            "sli_ids": ["extension_dispatch_p95_ms", "tail_stability_p99_over_p50_ratio"],
            "phase_validation_beads": ["bd-3ar8v.3.11", "bd-3ar8v.6.7"],
            "ux_outcome": "Extension events execute without perceptible stalls."
        }),
    ];

    scenario_sli_matrix.extend(REALISTIC_SESSION_SIZES.iter().map(|messages| {
        json!({
            "partition": PARTITION_REALISTIC,
            "scenario_id": format!("realistic/session_{messages}"),
            "sli_ids": ["interactive_turn_p95_ms", "resume_session_p95_ms", "tail_stability_p99_over_p50_ratio"],
            "phase_validation_beads": ["bd-3ar8v.3.11", "bd-3ar8v.6.7"],
            "ux_outcome": "Large-session operations remain usable for humans under realistic transcript load."
        })
    }));

    json!({
        "schema": BENCH_PROTOCOL_SCHEMA,
        "version": BENCH_PROTOCOL_VERSION,
        "partition_tags": [PARTITION_MATCHED_STATE, PARTITION_REALISTIC],
        "realistic_session_sizes": REALISTIC_SESSION_SIZES,
        "matched_state_scenarios": [
            {
                "scenario": "cold_start",
                "replay_input": { "runs": 5, "extension_fixture_set": ["hello", "pirate", "diff"] },
            },
            {
                "scenario": "warm_start",
                "replay_input": { "runs": 5, "extension_fixture_set": ["hello", "pirate", "diff"] },
            },
            {
                "scenario": "tool_call",
                "replay_input": { "iterations": 500, "extension_fixture_set": ["hello", "pirate", "diff"] },
            },
            {
                "scenario": "event_dispatch",
                "replay_input": { "iterations": 500, "event_name": "before_agent_start" },
            },
        ],
        "realistic_replay_inputs": realistic_replay_inputs,
        "required_metadata_fields": [
            "runtime",
            "build_profile",
            "host",
            "scenario_id",
            "correlation_id",
        ],
        "evidence_labels": {
            "evidence_class": [EVIDENCE_CLASS_MEASURED, EVIDENCE_CLASS_INFERRED],
            "confidence": [CONFIDENCE_HIGH, CONFIDENCE_MEDIUM, CONFIDENCE_LOW],
        },
        "budget_input_negative_controls": {
            "scope": "release_budget_inputs",
            "control_schema_version": "v1",
            "unproven_input_status": "NO_DATA",
            "release_binary": {
                "schema": "pi.perf.binary_size_measurement.v1",
                "required_fields": [
                    "binary_path",
                    "binary_sha256",
                    "size_bytes",
                    "cargo_profile",
                    "compiled_profile_family",
                    "compiled_opt_level",
                    "strip",
                    "profile_source"
                ]
            },
            "idle_rss": {
                "schema": "pi.perf.idle_rss_measurement.v1",
                "required_fields": [
                    "generated_at",
                    "pid",
                    "process_name",
                    "allocator",
                    "binary_path",
                    "binary_sha256",
                    "rss_bytes",
                    "idle_state",
                    "cargo_profile",
                    "build_command",
                    "sample_count",
                    "samples",
                    "rss_spread_bytes",
                    "settle_ms",
                    "bench_env_source",
                    "bench_env",
                    "bench_env_sha256"
                ]
            },
            "criterion_cold_load": {
                "schema": "pi.perf.cold_load_measurement.v1",
                "required_fields": [
                    "bench_env_source",
                    "bench_env_sha256",
                    "governor",
                    "aslr",
                    "thp",
                    "noise_score",
                    "artifact_sha256"
                ],
                "max_noise_score": 0
            }
        },
        "regression_gate_admission": {
            "scope": REGRESSION_GATE_GENERIC_SCOPE,
            "required_record_fields": REGRESSION_GATE_REQUIRED_RECORD_FIELDS,
            "load_scenario_required_fields": REGRESSION_GATE_LOAD_REQUIRED_RECORD_FIELDS,
            "allowed_measurement_methods": [
                MEASUREMENT_METHOD_WALL_CLOCK,
                MEASUREMENT_METHOD_SYNTHETIC,
            ],
            "allowed_measurement_boundaries": REGRESSION_GATE_ALLOWED_BOUNDARIES,
            "eligible_measurement_boundaries": REGRESSION_GATE_ELIGIBLE_BOUNDARIES,
            "allowed_disk_cache_policies": REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES,
            "allowed_host_page_cache_policies": REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES,
            "required_eligible_provenance_fields": REGRESSION_GATE_REQUIRED_ELIGIBLE_PROVENANCE_FIELDS,
            "eligible_evidence_class": EVIDENCE_CLASS_MEASURED,
            "eligible_confidence": CONFIDENCE_HIGH,
            "eligible_measurement_method": MEASUREMENT_METHOD_WALL_CLOCK,
            "required_eligible_host_page_cache_policy": "not_applicable_measured_region",
            "positive_sample_count_fields": REGRESSION_GATE_POSITIVE_SAMPLE_FIELDS,
            "require_positive_sample_count": true,
            "uncontrolled_host_page_cache_eligible": false,
        },
        "pijs_regression_gate_admission": {
            "scope": PIJS_GATE_SCOPE,
            "inherits": "regression_gate_admission",
            "required_record_fields": PIJS_GATE_REQUIRED_RECORD_FIELDS,
            "required_schema": PIJS_GATE_SCHEMA,
            "required_tool": PIJS_GATE_TOOL,
            "required_scenario": PIJS_GATE_SCENARIO,
            "required_runtime_engine": PIJS_GATE_RUNTIME_ENGINE,
            "required_build_profile": PIJS_GATE_BUILD_PROFILE,
            "required_build_profile_verified": true,
            "required_iterations": PIJS_GATE_ITERATIONS,
            "required_tool_calls_per_iteration": PIJS_GATE_TOOL_CALL_COUNTS,
            "required_measurement_boundary": PIJS_GATE_MEASUREMENT_BOUNDARY,
            "required_measurement_contract_version": PIJS_GATE_MEASUREMENT_CONTRACT_VERSION,
        },
        "partition_weighting": {
            PARTITION_MATCHED_STATE: PARTITION_WEIGHT_MATCHED_STATE,
            PARTITION_REALISTIC: PARTITION_WEIGHT_REALISTIC,
            "weights_sum_to": 1.0,
        },
        "partition_interpretation": {
            "primary_partition": PARTITION_REALISTIC,
            "secondary_partition": PARTITION_MATCHED_STATE,
            "global_claim_requires_partitions": [PARTITION_MATCHED_STATE, PARTITION_REALISTIC],
            "forbid_single_partition_conclusion": true,
            "interpretation_notes": {
                PARTITION_MATCHED_STATE: "Use matched-state for controlled equivalence and attribution; do not generalize alone.",
                PARTITION_REALISTIC: "Use realistic workloads as primary user-impact evidence and release-facing performance narrative.",
            },
        },
        "swarm_scale_requirements": {
            "target_cpu_cores": 64,
            "required_latency_quantiles": SWARM_LATENCY_QUANTILES,
            "required_queue_depth_quantiles": SWARM_QUEUE_DEPTH_QUANTILES,
            "required_resource_usage_keys": SWARM_RESOURCE_USAGE_KEYS,
            "required_component_breakdown_keys": SWARM_COMPONENT_BREAKDOWN_KEYS,
            "required_stage_breakdown_keys": SWARM_STAGE_BREAKDOWN_KEYS,
            "fail_closed_on_missing_measurements": true,
            "documented_run_commands": {
                "local": "PERF_CARGO_RUNNER=local ./scripts/perf/orchestrate.sh --profile full",
                "rch_required": "./scripts/perf/orchestrate.sh --require-rch --profile full"
            }
        },
        "user_perceived_sli_catalog": user_perceived_sli_catalog,
        "scenario_sli_matrix": scenario_sli_matrix,
    })
}

fn canonical_budget_summary_contract() -> Value {
    json!({
        "schema": PERF_BUDGET_SUMMARY_SCHEMA,
        "budget_definition_required_fields": PERF_BUDGET_DEFINITION_FIELDS,
        "comparison_values": PERF_BUDGET_COMPARISON_VALUES,
        "comparison_semantics": {
            "maximum": "actual <= threshold",
            "minimum": "actual >= threshold",
        },
        "claim_readiness": {
            "scope": "all_declared_budgets",
            "blocked_lineage_evaluation": "canonical_all_no_data_sentinel_without_artifact_discovery",
            "authorization_requires": [
                "strict_mode",
                "bound_source_commit",
                "matching_nonempty_run_and_correlation_ids",
                "all_declared_budgets_have_data",
                "all_declared_budgets_pass",
                "zero_data_contract_failures",
            ],
            "blocking_reason_codes": PERF_CLAIM_READINESS_BLOCKER_CODES,
            "blocking_reason_order": "lexicographic_ascending",
        },
        "inventory_digest": {
            "algorithm": "sha256",
            "canonical_v0_2_0_sha256": PERF_BUDGET_V0_2_0_INVENTORY_SHA256,
            "container": "compact_json_array",
            "budget_order": "producer_declaration_order",
            "field_order": PERF_BUDGET_DEFINITION_FIELDS,
            "threshold_representation": "exactly_six_decimal_places",
        },
    })
}

fn validate_metric_group(
    metrics: &serde_json::Map<String, Value>,
    field: &str,
    required_keys: &[&str],
    context: &str,
    allow_null: bool,
    require_monotonic: bool,
) -> Result<bool, String> {
    let group = metrics
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{context}.{field} must be an object"))?;
    let mut complete = true;
    let mut previous = 0.0_f64;

    for key in required_keys {
        let raw_value = group
            .get(*key)
            .ok_or_else(|| format!("{context}.{field} missing {key}"))?;
        if raw_value.is_null() {
            if allow_null {
                complete = false;
                continue;
            }
            return Err(format!("{context}.{field}.{key} must not be null"));
        }
        let value = raw_value
            .as_f64()
            .ok_or_else(|| format!("{context}.{field}.{key} must be numeric"))?;
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "{context}.{field}.{key} must be finite and non-negative, got: {value}"
            ));
        }
        if require_monotonic && value < previous {
            return Err(format!(
                "{context}.{field}.{key} must be monotonic; {value} came after {previous}"
            ));
        }
        previous = value;
    }

    let unexpected = group
        .keys()
        .filter(|key| !required_keys.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(format!(
            "{context}.{field} has unexpected keys: {unexpected:?}"
        ));
    }

    Ok(complete)
}

fn validate_swarm_metrics_value(
    value: &Value,
    context: &str,
    allow_null: bool,
) -> Result<bool, String> {
    let metrics = value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))?;
    let mut complete = true;

    complete &= validate_metric_group(
        metrics,
        "latency_quantiles_ms",
        SWARM_LATENCY_QUANTILES,
        context,
        allow_null,
        true,
    )?;
    complete &= validate_metric_group(
        metrics,
        "queue_depth",
        SWARM_QUEUE_DEPTH_QUANTILES,
        context,
        allow_null,
        true,
    )?;
    complete &= validate_metric_group(
        metrics,
        "resource_usage",
        SWARM_RESOURCE_USAGE_KEYS,
        context,
        allow_null,
        false,
    )?;
    complete &= validate_metric_group(
        metrics,
        "component_breakdown_ms",
        SWARM_COMPONENT_BREAKDOWN_KEYS,
        context,
        allow_null,
        false,
    )?;
    complete &= validate_metric_group(
        metrics,
        "stage_breakdown_ms",
        SWARM_STAGE_BREAKDOWN_KEYS,
        context,
        allow_null,
        false,
    )?;
    complete &= validate_metric_group(
        metrics,
        "host_capacity",
        SWARM_HOST_CAPACITY_KEYS,
        context,
        allow_null,
        false,
    )?;

    Ok(complete)
}

fn collect_string_set(value: &Value, context: &str) -> Result<BTreeSet<String>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| format!("{context} must be an array"))?;
    let mut set = BTreeSet::new();
    for item in array {
        let item = item
            .as_str()
            .ok_or_else(|| format!("{context} entries must be strings"))?;
        if item.trim().is_empty() {
            return Err(format!("{context} entries must be non-empty strings"));
        }
        if !set.insert(item.to_string()) {
            return Err(format!(
                "{context} must not contain duplicate entries: {item}"
            ));
        }
    }
    Ok(set)
}

fn validate_resource_governor_admission_record(record: &Value) -> Result<(), String> {
    let schema = record
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| "resource governor record missing schema".to_string())?;
    if schema != RESOURCE_GOVERNOR_ADMISSION_SCHEMA {
        return Err(format!(
            "resource governor schema must be {RESOURCE_GOVERNOR_ADMISSION_SCHEMA}, got: {schema}"
        ));
    }

    let request = record
        .get("request")
        .and_then(Value::as_object)
        .ok_or_else(|| "resource governor request must be an object".to_string())?;
    for field in [
        "operation",
        "capability",
        "estimated_tool_output_bytes",
        "queue_depth",
    ] {
        if !request.contains_key(field) {
            return Err(format!("resource governor request missing {field}"));
        }
    }
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| "resource governor request.operation must be a string".to_string())?;
    if ![
        "tool", "exec", "http", "session", "ui", "events", "log", "unknown",
    ]
    .contains(&operation)
    {
        return Err(format!(
            "resource governor request.operation has unknown value: {operation}"
        ));
    }
    let capability = request
        .get("capability")
        .and_then(Value::as_str)
        .ok_or_else(|| "resource governor request.capability must be a string".to_string())?;
    if capability.trim().is_empty() {
        return Err("resource governor request.capability must not be empty".to_string());
    }
    for field in ["estimated_tool_output_bytes", "queue_depth"] {
        let Some(value) = request.get(field).and_then(Value::as_u64) else {
            return Err(format!(
                "resource governor request.{field} must be an integer"
            ));
        };
        if field == "queue_depth" && value == 0 {
            return Err("resource governor request.queue_depth must be positive".to_string());
        }
    }

    let decision = record
        .get("decision")
        .and_then(Value::as_object)
        .ok_or_else(|| "resource governor decision must be an object".to_string())?;
    for field in [
        "action",
        "dominant_dimension",
        "dominant_ratio",
        "reason",
        "retry_after_ms",
        "sample",
        "budgets",
    ] {
        if !decision.contains_key(field) {
            return Err(format!("resource governor decision missing {field}"));
        }
    }
    let action = decision
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "resource governor decision.action must be a string".to_string())?;
    if !["admit", "backpressure", "deny"].contains(&action) {
        return Err(format!(
            "resource governor decision.action has unknown value: {action}"
        ));
    }
    let dominant_ratio = decision
        .get("dominant_ratio")
        .and_then(Value::as_f64)
        .ok_or_else(|| "resource governor decision.dominant_ratio must be numeric".to_string())?;
    if !dominant_ratio.is_finite() || dominant_ratio < 0.0 {
        return Err(
            "resource governor decision.dominant_ratio must be finite and non-negative".to_string(),
        );
    }
    if !decision
        .get("sample")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err("resource governor decision.sample must be an object".to_string());
    }
    let budgets = decision
        .get("budgets")
        .and_then(Value::as_object)
        .ok_or_else(|| "resource governor decision.budgets must be an object".to_string())?;
    for field in [
        "cpu_cores",
        "max_load_avg_1m",
        "max_rss_bytes",
        "max_processes",
        "max_fds",
        "max_tool_output_bytes",
        "backpressure_ratio",
        "deny_ratio",
    ] {
        if !budgets.contains_key(field) {
            return Err(format!("resource governor budgets missing {field}"));
        }
    }

    Ok(())
}

fn require_string_array_eq(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    expected: &[&str],
    context: &str,
) -> Result<(), String> {
    let parsed = obj
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{context}.{field} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| format!("{context}.{field} entries must be strings"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected = expected
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    if parsed != expected {
        return Err(format!(
            "{context}.{field} must equal {expected:?}, got {parsed:?}"
        ));
    }
    Ok(())
}

fn regression_provenance_field<'a>(record: &'a Value, field: &str) -> Option<&'a Value> {
    record
        .get(field)
        .or_else(|| record.get("env").and_then(|env| env.get(field)))
}

fn validate_eligible_build_provenance(record: &Value) -> Result<(), String> {
    for field in [
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
    ] {
        if let (Some(top_level), Some(env_value)) = (
            record.get(field),
            record.get("env").and_then(|env| env.get(field)),
        ) && top_level != env_value
        {
            return Err(format!(
                "regression-gate top-level {field} must match env.{field}"
            ));
        }
    }
    for (field, expected) in [
        ("build_profile", "perf"),
        ("executable_build_profile", "perf"),
        ("build_fingerprint_contract", BUILD_FINGERPRINT_CONTRACT),
        ("compiled_profile_family", "release"),
        ("compiled_opt_level", "3"),
        ("compiled_debug", "true"),
    ] {
        let observed = regression_provenance_field(record, field).and_then(Value::as_str);
        if observed != Some(expected) {
            return Err(format!(
                "regression-gate eligible evidence {field} must equal {expected:?} (observed={observed:?})"
            ));
        }
    }
    if !matches_canonical_perf_build_fingerprint("release", "3", "true") {
        return Err("canonical perf build fingerprint contract is internally invalid".to_string());
    }
    for field in [
        "executable_profile_verified",
        "build_fingerprint_verified",
        "build_profile_verified",
    ] {
        if regression_provenance_field(record, field).and_then(Value::as_bool) != Some(true) {
            return Err(format!(
                "regression-gate eligible evidence {field} must equal true"
            ));
        }
    }
    if regression_provenance_field(record, "debug_assertions").and_then(Value::as_bool)
        != Some(false)
    {
        return Err(
            "regression-gate eligible evidence debug_assertions must equal false".to_string(),
        );
    }

    let binary_path = regression_provenance_field(record, "binary_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "regression-gate eligible evidence binary_path must be non-empty".to_string()
        })?;
    let binary_path = Path::new(binary_path);
    if !binary_path.is_absolute() {
        return Err("regression-gate eligible evidence binary_path must be absolute".to_string());
    }
    let derived_profile = profile_from_target_path(binary_path);
    if derived_profile.as_deref() != Some("perf") {
        return Err(format!(
            "regression-gate eligible evidence binary_path must derive perf profile (observed={derived_profile:?})"
        ));
    }
    let canonical_path = fs::canonicalize(binary_path).map_err(|err| {
        format!("regression-gate eligible evidence binary_path must exist: {err}")
    })?;
    if canonical_path != binary_path {
        return Err("regression-gate eligible evidence binary_path must be canonical".to_string());
    }
    let claimed_sha256 = regression_provenance_field(record, "binary_sha256")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| {
            "regression-gate eligible evidence binary_sha256 must be lowercase SHA-256".to_string()
        })?;
    let observed_sha256 = sha256_file(binary_path)
        .map_err(|err| format!("failed to hash regression-gate binary_path: {err}"))?;
    if claimed_sha256 != observed_sha256 {
        return Err(
            "regression-gate eligible evidence binary_sha256 does not match binary_path"
                .to_string(),
        );
    }
    let source_commit = regression_provenance_field(record, "source_commit")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 40
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && !value.bytes().all(|byte| byte == b'0')
        })
        .ok_or_else(|| {
            "regression-gate eligible evidence source_commit must be a full lowercase Git SHA"
                .to_string()
        })?;
    if regression_provenance_field(record, "source_dirty").and_then(Value::as_bool) != Some(false) {
        return Err("regression-gate eligible evidence source_dirty must equal false".to_string());
    }
    let compiled_features = regression_provenance_field(record, "compiled_features")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "regression-gate eligible evidence compiled_features must be an array".to_string()
        })?
        .iter()
        .map(|value| {
            value.as_str().ok_or_else(|| {
                "regression-gate eligible evidence compiled_features entries must be strings"
                    .to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if compiled_features.is_empty() || compiled_features.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(
            "regression-gate eligible evidence compiled_features must be non-empty, sorted, and unique"
                .to_string(),
        );
    }
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
        binary_path: binary_path.to_str().ok_or_else(|| {
            "regression-gate eligible evidence binary_path must be UTF-8".to_string()
        })?,
        binary_sha256: claimed_sha256,
        debug_assertions: false,
    });
    let claimed_config_hash = regression_provenance_field(record, "config_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "regression-gate eligible evidence config_hash must be a string".to_string()
        })?;
    if claimed_config_hash != expected_config_hash {
        return Err(
            "regression-gate eligible evidence config_hash does not match asserted provenance"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_regression_gate_admission_record(record: &Value) -> Result<(), String> {
    let missing = has_required_fields(record, REGRESSION_GATE_REQUIRED_RECORD_FIELDS);
    if !missing.is_empty() {
        return Err(format!(
            "regression-gate admission is missing required fields: {missing:?}"
        ));
    }

    let evidence_class = record
        .get("evidence_class")
        .and_then(Value::as_str)
        .ok_or_else(|| "regression-gate evidence_class must be a string".to_string())?;
    if !matches!(
        evidence_class,
        EVIDENCE_CLASS_MEASURED | EVIDENCE_CLASS_INFERRED
    ) {
        return Err(format!(
            "invalid regression-gate evidence_class: {evidence_class}"
        ));
    }
    let confidence = record
        .get("confidence")
        .and_then(Value::as_str)
        .ok_or_else(|| "regression-gate confidence must be a string".to_string())?;
    if !matches!(
        confidence,
        CONFIDENCE_HIGH | CONFIDENCE_MEDIUM | CONFIDENCE_LOW
    ) {
        return Err(format!("invalid regression-gate confidence: {confidence}"));
    }

    let eligible = record
        .get("eligible_for_regression_gate")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            "regression-gate eligible_for_regression_gate must be a boolean".to_string()
        })?;
    let measurement_method = record
        .get("measurement_method")
        .and_then(Value::as_str)
        .ok_or_else(|| "regression-gate measurement_method must be a string".to_string())?;
    if !matches!(
        measurement_method,
        MEASUREMENT_METHOD_WALL_CLOCK | MEASUREMENT_METHOD_SYNTHETIC
    ) {
        return Err(format!(
            "invalid regression-gate measurement_method: {measurement_method}"
        ));
    }

    let measurement_boundary = record
        .get("measurement_boundary")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "regression-gate measurement_boundary must be a non-empty string".to_string()
        })?;
    let measurement_contract_version = record
        .get("measurement_contract_version")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "regression-gate measurement_contract_version must be a non-empty string".to_string()
        })?;
    let disk_cache_policy = record
        .get("disk_cache_policy")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "regression-gate disk_cache_policy must be a non-empty string".to_string()
        })?;
    if !REGRESSION_GATE_ALLOWED_BOUNDARIES.contains(&measurement_boundary) {
        return Err(format!(
            "invalid regression-gate measurement_boundary token: {measurement_boundary}"
        ));
    }
    let expected_contract = format!("{measurement_boundary}.v1");
    if measurement_contract_version != expected_contract {
        return Err(format!(
            "regression-gate measurement_contract_version must equal {expected_contract:?} for boundary {measurement_boundary:?}"
        ));
    }
    if !REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES.contains(&disk_cache_policy) {
        return Err(format!(
            "invalid regression-gate disk_cache_policy token: {disk_cache_policy}"
        ));
    }
    if let Some(host_page_cache_policy) = record.get("host_page_cache_policy") {
        let host_page_cache_policy = host_page_cache_policy
            .as_str()
            .ok_or_else(|| "regression-gate host_page_cache_policy must be a string".to_string())?;
        if !REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES.contains(&host_page_cache_policy) {
            return Err(format!(
                "invalid regression-gate host_page_cache_policy token: {host_page_cache_policy}"
            ));
        }
    }

    let scenario = record
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_load_scenario = matches!(
        scenario,
        "cold_start" | "warm_start" | "ext_load_init/load_init_cold"
    );
    if is_load_scenario {
        let missing = has_required_fields(record, REGRESSION_GATE_LOAD_REQUIRED_RECORD_FIELDS);
        if !missing.is_empty() {
            return Err(format!(
                "regression-gate load scenario is missing cache-policy fields: {missing:?}"
            ));
        }
        for field in REGRESSION_GATE_LOAD_REQUIRED_RECORD_FIELDS {
            let policy = record
                .get(*field)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    format!("regression-gate load scenario {field} must be a non-empty string")
                })?;
            let allowed = if *field == "disk_cache_policy" {
                REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES
            } else {
                REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES
            };
            if !allowed.contains(&policy) {
                return Err(format!(
                    "invalid regression-gate load scenario {field} token: {policy}"
                ));
            }
        }
    }

    if !eligible {
        return Ok(());
    }
    validate_eligible_build_provenance(record)?;
    let host_page_cache_policy = record
        .get("host_page_cache_policy")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "regression-gate eligible evidence requires host_page_cache_policy".to_string()
        })?;
    if host_page_cache_policy != "not_applicable_measured_region" {
        return Err(format!(
            "regression-gate eligible evidence requires host_page_cache_policy=\"not_applicable_measured_region\", got: {host_page_cache_policy}"
        ));
    }
    if evidence_class != EVIDENCE_CLASS_MEASURED {
        return Err("regression-gate eligible evidence must be measured".to_string());
    }
    if confidence != CONFIDENCE_HIGH {
        return Err("regression-gate eligible evidence must have high confidence".to_string());
    }
    if measurement_method != MEASUREMENT_METHOD_WALL_CLOCK {
        return Err(
            "regression-gate eligible evidence must use wall_clock_observation".to_string(),
        );
    }
    if !matches!(
        measurement_boundary,
        "production_extension_manager" | "production_extension_runtime"
    ) {
        return Err(format!(
            "regression-gate eligible evidence requires a production boundary, got: {measurement_boundary}"
        ));
    }
    let mut has_sample_count = false;
    for field in REGRESSION_GATE_POSITIVE_SAMPLE_FIELDS {
        let Some(raw_value) = record.get(*field) else {
            continue;
        };
        has_sample_count = true;
        if raw_value.as_u64().is_none_or(|value| value == 0) {
            return Err(format!(
                "regression-gate eligible evidence {field} must be a positive integer"
            ));
        }
    }
    if !has_sample_count {
        return Err(format!(
            "regression-gate eligible evidence requires a positive sample count in one of {REGRESSION_GATE_POSITIVE_SAMPLE_FIELDS:?}"
        ));
    }
    Ok(())
}

fn require_pijs_gate_string(record: &Value, field: &str, expected: &str) -> Result<(), String> {
    let observed = record.get(field).and_then(Value::as_str);
    if observed == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "PiJS regression-gate {field} must equal {expected:?} (observed={observed:?})"
        ))
    }
}

fn is_pijs_workload_record(record: &Value) -> bool {
    record.get("tool").and_then(Value::as_str) == Some(PIJS_GATE_TOOL)
}

fn validate_pijs_regression_gate_admission_record(record: &Value) -> Result<(), String> {
    validate_regression_gate_admission_record(record)?;
    if record
        .get("eligible_for_regression_gate")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Ok(());
    }

    let missing = has_required_fields(record, PIJS_GATE_REQUIRED_RECORD_FIELDS);
    if !missing.is_empty() {
        return Err(format!(
            "PiJS regression-gate record is missing required fields: {missing:?}"
        ));
    }

    for (field, expected) in [
        ("schema", PIJS_GATE_SCHEMA),
        ("tool", PIJS_GATE_TOOL),
        ("scenario", PIJS_GATE_SCENARIO),
        ("runtime_engine", PIJS_GATE_RUNTIME_ENGINE),
        ("build_profile", PIJS_GATE_BUILD_PROFILE),
        ("build_fingerprint_contract", BUILD_FINGERPRINT_CONTRACT),
        ("compiled_profile_family", "release"),
        ("compiled_opt_level", "3"),
        ("compiled_debug", "true"),
        ("executable_build_profile", "perf"),
        ("allocator_requested", "system"),
        ("allocator_request_source", "env"),
        ("allocator_effective", "system"),
        ("measurement_boundary", PIJS_GATE_MEASUREMENT_BOUNDARY),
        (
            "measurement_contract_version",
            PIJS_GATE_MEASUREMENT_CONTRACT_VERSION,
        ),
    ] {
        require_pijs_gate_string(record, field, expected)?;
    }

    if record
        .get("build_profile_verified")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("PiJS regression-gate build_profile_verified must equal true".to_string());
    }
    for field in ["build_fingerprint_verified", "executable_profile_verified"] {
        if record.get(field).and_then(Value::as_bool) != Some(true) {
            return Err(format!("PiJS regression-gate {field} must equal true"));
        }
    }
    if record.get("debug_assertions").and_then(Value::as_bool) != Some(false) {
        return Err("PiJS regression-gate debug_assertions must equal false".to_string());
    }
    if record.get("source_dirty").and_then(Value::as_bool) != Some(false) {
        return Err("PiJS regression-gate source_dirty must equal false".to_string());
    }
    if !record
        .get("allocator_fallback_reason")
        .is_some_and(Value::is_null)
    {
        return Err("PiJS regression-gate allocator_fallback_reason must equal null".to_string());
    }
    let features = record
        .get("compiled_features")
        .and_then(Value::as_array)
        .ok_or_else(|| "PiJS regression-gate compiled_features must be an array".to_string())?
        .iter()
        .map(|feature| {
            feature.as_str().ok_or_else(|| {
                "PiJS regression-gate compiled_features entries must be strings".to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if features != CANONICAL_PIJS_PERF_FEATURES {
        return Err(format!(
            "PiJS regression-gate compiled_features must equal {CANONICAL_PIJS_PERF_FEATURES:?}"
        ));
    }
    let run_id = record
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "PiJS regression-gate run_id must be non-empty".to_string())?;
    let correlation_id = record
        .get("correlation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "PiJS regression-gate correlation_id must be non-empty".to_string())?;
    if run_id != correlation_id {
        return Err("PiJS regression-gate run_id and correlation_id must be identical".to_string());
    }
    let timestamp = record
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| "PiJS regression-gate timestamp must be a string".to_string())?;
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map_err(|err| format!("PiJS regression-gate timestamp must be RFC3339: {err}"))?;
    for (field, expected_len) in [("source_commit", 40), ("binary_sha256", 64)] {
        let value = record
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() == expected_len
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                format!(
                    "PiJS regression-gate {field} must be a {expected_len}-character hexadecimal string"
                )
            })?;
        if value.bytes().all(|byte| byte == b'0') {
            return Err(format!(
                "PiJS regression-gate {field} must not be all zeros"
            ));
        }
    }
    record
        .get("binary_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "PiJS regression-gate binary_path must be non-empty".to_string())?;

    let iterations = record
        .get("iterations")
        .and_then(Value::as_u64)
        .ok_or_else(|| "PiJS regression-gate iterations must be an integer".to_string())?;
    if iterations != PIJS_GATE_ITERATIONS {
        return Err(format!(
            "PiJS regression-gate iterations must equal {PIJS_GATE_ITERATIONS} (observed={iterations})"
        ));
    }

    let tool_calls = record
        .get("tool_calls_per_iteration")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "PiJS regression-gate tool_calls_per_iteration must be an integer".to_string()
        })?;
    if !PIJS_GATE_TOOL_CALL_COUNTS.contains(&tool_calls) {
        return Err(format!(
            "PiJS regression-gate tool_calls_per_iteration must be one of {PIJS_GATE_TOOL_CALL_COUNTS:?} (observed={tool_calls})"
        ));
    }

    let total_calls = record
        .get("total_calls")
        .and_then(Value::as_u64)
        .ok_or_else(|| "PiJS regression-gate total_calls must be an integer".to_string())?;
    let expected_total = iterations
        .checked_mul(tool_calls)
        .ok_or_else(|| "PiJS regression-gate sample count overflows u64".to_string())?;
    if total_calls != expected_total {
        return Err(format!(
            "PiJS regression-gate total_calls must equal iterations * tool_calls_per_iteration ({expected_total}); observed={total_calls}"
        ));
    }
    record
        .get("elapsed_us")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "PiJS regression-gate elapsed_us must be a positive integer".to_string())?;
    for field in ["elapsed_us_f64", "per_call_us_f64"] {
        record
            .get(field)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| {
                format!("PiJS regression-gate {field} must be a finite positive number")
            })?;
    }

    Ok(())
}

fn validate_workload_record(record: &Value) -> Result<(), String> {
    let missing = has_required_fields(record, WORKLOAD_REQUIRED);
    if !missing.is_empty() {
        return Err(format!(
            "workload event missing required fields: {missing:?}"
        ));
    }
    if record.get("eligible_for_regression_gate").is_some() {
        if is_pijs_workload_record(record) {
            validate_pijs_regression_gate_admission_record(record)?;
        } else {
            validate_regression_gate_admission_record(record)?;
        }
    }
    Ok(())
}

fn validate_protocol_record(record: &Value) -> Result<(), String> {
    let required = [
        "protocol_schema",
        "protocol_version",
        "partition",
        "evidence_class",
        "confidence",
        "correlation_id",
        "scenario_metadata",
    ];
    let missing = has_required_fields(record, &required);
    if !missing.is_empty() {
        return Err(format!("missing required fields: {missing:?}"));
    }

    let protocol_schema = record
        .get("protocol_schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if protocol_schema != BENCH_PROTOCOL_SCHEMA {
        return Err(format!("unexpected protocol_schema: {protocol_schema}"));
    }

    let protocol_version = record
        .get("protocol_version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if protocol_version != BENCH_PROTOCOL_VERSION {
        return Err(format!("unexpected protocol_version: {protocol_version}"));
    }

    let partition = record
        .get("partition")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(partition, PARTITION_MATCHED_STATE | PARTITION_REALISTIC) {
        return Err(format!("invalid partition: {partition}"));
    }

    let evidence_class = record
        .get("evidence_class")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        evidence_class,
        EVIDENCE_CLASS_MEASURED | EVIDENCE_CLASS_INFERRED
    ) {
        return Err(format!("invalid evidence_class: {evidence_class}"));
    }

    let confidence = record
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        confidence,
        CONFIDENCE_HIGH | CONFIDENCE_MEDIUM | CONFIDENCE_LOW
    ) {
        return Err(format!("invalid confidence: {confidence}"));
    }

    // The v1 admission overlay is additive. Historical records without the
    // opt-in eligibility marker retain their original validation semantics.
    if record.get("eligible_for_regression_gate").is_some() {
        validate_regression_gate_admission_record(record)?;
    }

    let correlation_id = record
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if correlation_id.trim().is_empty() {
        return Err("correlation_id must be non-empty".to_string());
    }

    let metadata = record
        .get("scenario_metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| "scenario_metadata must be an object".to_string())?;

    for key in &[
        "runtime",
        "build_profile",
        "host",
        "scenario_id",
        "replay_input",
    ] {
        if !metadata.contains_key(*key) {
            return Err(format!("scenario_metadata missing {key}"));
        }
    }

    let host = metadata
        .get("host")
        .and_then(Value::as_object)
        .ok_or_else(|| "scenario_metadata.host must be an object".to_string())?;
    for key in &["os", "arch", "cpu_model", "cpu_cores"] {
        if !host.contains_key(*key) {
            return Err(format!("scenario_metadata.host missing {key}"));
        }
    }

    let scenario_id = metadata
        .get("scenario_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if scenario_id.trim().is_empty() {
        return Err("scenario_metadata.scenario_id must be non-empty".to_string());
    }
    let scenario = record
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if scenario == "session_workload_matrix" {
        let swarm_metrics = record.get("swarm_metrics").ok_or_else(|| {
            "session_workload_matrix records must include swarm_metrics".to_string()
        })?;
        validate_swarm_metrics_value(
            swarm_metrics,
            "session_workload_matrix.swarm_metrics",
            false,
        )?;
    }

    if partition == PARTITION_REALISTIC {
        if !scenario_id.starts_with("realistic/session_") {
            return Err(format!(
                "realistic partition requires scenario_id prefixed with realistic/session_: {scenario_id}"
            ));
        }
        let replay = metadata
            .get("replay_input")
            .and_then(Value::as_object)
            .ok_or_else(|| "realistic partition requires object replay_input".to_string())?;
        let size = replay
            .get("session_messages")
            .and_then(Value::as_u64)
            .ok_or_else(|| "realistic replay_input requires session_messages".to_string())?;
        if !REALISTIC_SESSION_SIZES.contains(&size) {
            return Err(format!(
                "unsupported realistic session_messages: {size} (expected one of {REALISTIC_SESSION_SIZES:?})"
            ));
        }
    } else {
        if scenario == "session_workload_matrix" {
            if !scenario_id.starts_with("matched-state/session_") {
                return Err(format!(
                    "session_workload_matrix matched-state scenario_id must be matched-state/session_*, got: {scenario_id}"
                ));
            }
            let replay = metadata
                .get("replay_input")
                .and_then(Value::as_object)
                .ok_or_else(|| "matched-state matrix requires object replay_input".to_string())?;
            let size = replay
                .get("session_messages")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    "matched-state matrix replay_input requires session_messages".to_string()
                })?;
            if !REALISTIC_SESSION_SIZES.contains(&size) {
                return Err(format!(
                    "unsupported matched-state session_messages: {size} (expected one of {REALISTIC_SESSION_SIZES:?})"
                ));
            }
            return Ok(());
        }

        let matched_valid = [
            "cold_start",
            "warm_start",
            "tool_call",
            "event_dispatch",
            "matched-state/cold_start",
            "matched-state/warm_start",
            "matched-state/tool_call",
            "matched-state/event_dispatch",
        ];
        if !matched_valid.contains(&scenario_id) {
            return Err(format!(
                "matched-state partition requires canonical scenario_id, got: {scenario_id}"
            ));
        }
    }

    Ok(())
}

fn validate_extension_stratification_record(record: &Value) -> Result<(), String> {
    let required_top_level = [
        "schema",
        "run_id",
        "correlation_id",
        "layers",
        "claim_integrity",
        "lineage",
    ];
    let missing = has_required_fields(record, &required_top_level);
    if !missing.is_empty() {
        return Err(format!("missing required fields: {missing:?}"));
    }

    let schema = record
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != EXT_STRATIFICATION_SCHEMA {
        return Err(format!("unexpected schema: {schema}"));
    }

    let layers = record
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| "layers must be an array".to_string())?;
    if layers.len() < 3 {
        return Err("layers must include cold-load, per-call, and full-e2e entries".to_string());
    }

    let mut layer_ids = HashSet::new();
    for layer in layers {
        let layer_obj = layer
            .as_object()
            .ok_or_else(|| "layer must be an object".to_string())?;
        for field in &[
            "layer_id",
            "display_name",
            "scenario_tags",
            "absolute_metrics",
            "relative_metrics",
            "confidence",
            "evidence_state",
            "lineage",
        ] {
            if !layer_obj.contains_key(*field) {
                return Err(format!("layer missing {field}"));
            }
        }
        let layer_id = layer_obj
            .get("layer_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if layer_id.trim().is_empty() {
            return Err("layer_id must be non-empty".to_string());
        }
        layer_ids.insert(layer_id.to_string());

        let scenario_tags = layer_obj
            .get("scenario_tags")
            .and_then(Value::as_array)
            .ok_or_else(|| "scenario_tags must be an array".to_string())?;
        if scenario_tags.is_empty() {
            return Err(format!("layer {layer_id} must include scenario_tags"));
        }

        let absolute_metrics = layer_obj
            .get("absolute_metrics")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("layer {layer_id} absolute_metrics must be object"))?;
        for field in &["metric_name", "unit"] {
            if !absolute_metrics.contains_key(*field) {
                return Err(format!("layer {layer_id} absolute_metrics missing {field}"));
            }
        }

        let relative_metrics = layer_obj
            .get("relative_metrics")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("layer {layer_id} relative_metrics must be object"))?;
        for field in &[
            "rust_vs_node_ratio",
            "rust_vs_node_ratio_basis",
            "rust_vs_bun_ratio",
            "rust_vs_bun_ratio_basis",
        ] {
            if !relative_metrics.contains_key(*field) {
                return Err(format!("layer {layer_id} relative_metrics missing {field}"));
            }
        }

        let lineage = layer_obj
            .get("lineage")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("layer {layer_id} lineage must be object"))?;
        let run_id_lineage = lineage
            .get("run_id_lineage")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("layer {layer_id} lineage.run_id_lineage must be array"))?;
        if run_id_lineage.len() < 2 {
            return Err(format!(
                "layer {layer_id} lineage.run_id_lineage must include run_id + correlation_id"
            ));
        }
    }

    for expected in &[
        "cold_load_init",
        "per_call_dispatch_micro",
        "full_e2e_long_session",
    ] {
        if !layer_ids.contains(*expected) {
            return Err(format!("missing required layer_id: {expected}"));
        }
    }

    let claim_integrity = record
        .get("claim_integrity")
        .and_then(Value::as_object)
        .ok_or_else(|| "claim_integrity must be an object".to_string())?;
    for field in &["anti_conflation", "cherry_pick_guard", "partition_coverage"] {
        if !claim_integrity.contains_key(*field) {
            return Err(format!("claim_integrity missing {field}"));
        }
    }
    let cherry_pick_guard = claim_integrity
        .get("cherry_pick_guard")
        .and_then(Value::as_object)
        .ok_or_else(|| "claim_integrity.cherry_pick_guard must be object".to_string())?;
    for field in &[
        "requires_all_layers_for_global_claim",
        "layer_coverage",
        "global_claim_valid",
        "invalidity_reasons",
    ] {
        if !cherry_pick_guard.contains_key(*field) {
            return Err(format!("claim_integrity.cherry_pick_guard missing {field}"));
        }
    }

    let lineage = record
        .get("lineage")
        .and_then(Value::as_object)
        .ok_or_else(|| "lineage must be an object".to_string())?;
    let top_level_run_id_lineage = lineage
        .get("run_id_lineage")
        .and_then(Value::as_array)
        .ok_or_else(|| "lineage.run_id_lineage must be an array".to_string())?;
    if top_level_run_id_lineage.len() < 2 {
        return Err("lineage.run_id_lineage must include run_id + correlation_id".to_string());
    }

    Ok(())
}

fn validate_phase1_matrix_validation_record(record: &Value) -> Result<(), String> {
    let required_top_level = [
        "schema",
        "run_id",
        "correlation_id",
        "matrix_requirements",
        "matrix_cells",
        "stage_summary",
        "swarm_summary",
        "weighted_bottleneck_attribution",
        "primary_outcomes",
        "regression_guards",
        "evidence_links",
        "consumption_contract",
        "lineage",
    ];
    let missing = has_required_fields(record, &required_top_level);
    if !missing.is_empty() {
        return Err(format!("missing required fields: {missing:?}"));
    }

    let schema = record
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != PHASE1_MATRIX_SCHEMA {
        return Err(format!("unexpected schema: {schema}"));
    }
    let run_id = record
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "run_id must be a string".to_string())?;
    if run_id.trim().is_empty() {
        return Err("run_id must be non-empty".to_string());
    }
    let correlation_id = record
        .get("correlation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "correlation_id must be a string".to_string())?;
    if correlation_id.trim().is_empty() {
        return Err("correlation_id must be non-empty".to_string());
    }

    let matrix_requirements = record
        .get("matrix_requirements")
        .and_then(Value::as_object)
        .ok_or_else(|| "matrix_requirements must be an object".to_string())?;
    for field in &[
        "required_partition_tags",
        "required_session_message_sizes",
        "required_cell_count",
    ] {
        if !matrix_requirements.contains_key(*field) {
            return Err(format!("matrix_requirements missing {field}"));
        }
    }
    let required_partition_tags = matrix_requirements
        .get("required_partition_tags")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "matrix_requirements.required_partition_tags must be an array".to_string()
        })?;
    if required_partition_tags.is_empty() {
        return Err("matrix_requirements.required_partition_tags must not be empty".to_string());
    }
    let mut required_partitions = HashSet::new();
    for partition in required_partition_tags {
        let partition = partition.as_str().ok_or_else(|| {
            "matrix_requirements.required_partition_tags entries must be strings".to_string()
        })?;
        if partition.trim().is_empty() {
            return Err(
                "matrix_requirements.required_partition_tags entries must be non-empty strings"
                    .to_string(),
            );
        }
        required_partitions.insert(partition.to_string());
    }
    if required_partitions.len() != required_partition_tags.len() {
        return Err(
            "matrix_requirements.required_partition_tags must not contain duplicates".to_string(),
        );
    }

    let required_session_message_sizes = matrix_requirements
        .get("required_session_message_sizes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "matrix_requirements.required_session_message_sizes must be an array".to_string()
        })?;
    if required_session_message_sizes.is_empty() {
        return Err(
            "matrix_requirements.required_session_message_sizes must not be empty".to_string(),
        );
    }
    let mut required_sizes = HashSet::new();
    for size in required_session_message_sizes {
        let size = size.as_u64().ok_or_else(|| {
            "matrix_requirements.required_session_message_sizes entries must be integers"
                .to_string()
        })?;
        if size == 0 {
            return Err(
                "matrix_requirements.required_session_message_sizes entries must be > 0"
                    .to_string(),
            );
        }
        required_sizes.insert(size);
    }
    if required_sizes.len() != required_session_message_sizes.len() {
        return Err(
            "matrix_requirements.required_session_message_sizes must not contain duplicates"
                .to_string(),
        );
    }

    let required_cell_count = matrix_requirements
        .get("required_cell_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "matrix_requirements.required_cell_count must be a positive integer".to_string()
        })?;
    if required_cell_count == 0 {
        return Err("matrix_requirements.required_cell_count must be > 0".to_string());
    }
    let max_unique_cells = required_partitions.len() as u64 * required_sizes.len() as u64;
    if required_cell_count > max_unique_cells {
        return Err(format!(
            "matrix_requirements.required_cell_count ({required_cell_count}) exceeds unique partition-size combinations ({max_unique_cells})"
        ));
    }

    let matrix_cells = record
        .get("matrix_cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "matrix_cells must be an array".to_string())?;
    if matrix_cells.is_empty() {
        return Err("matrix_cells must not be empty".to_string());
    }
    if required_cell_count != matrix_cells.len() as u64 {
        return Err(format!(
            "matrix_requirements.required_cell_count ({required_cell_count}) does not match matrix_cells length ({})",
            matrix_cells.len()
        ));
    }
    let expected_required_stage_keys = ["open_ms", "append_ms", "save_ms", "index_ms"];
    let mut observed_stage_coverage: HashMap<&'static str, u64> = HashMap::new();
    for key in &expected_required_stage_keys {
        observed_stage_coverage.insert(*key, 0);
    }
    let mut observed_complete_stage_breakdown_cells = 0_u64;
    let mut observed_missing_stage_breakdown_cells = 0_u64;
    let mut observed_missing_stage_cell_keys: HashSet<(String, u64)> = HashSet::new();
    let mut observed_missing_stage_reasons_by_key: HashMap<(String, u64), BTreeSet<String>> =
        HashMap::new();
    let mut observed_complete_swarm_metric_cells = 0_u64;
    let mut observed_missing_swarm_metric_cells = 0_u64;
    let mut observed_missing_swarm_cell_keys: HashSet<(String, u64)> = HashSet::new();
    let mut observed_missing_swarm_reasons_by_key: HashMap<(String, u64), BTreeSet<String>> =
        HashMap::new();
    let mut observed_weighted_valid_cell_count = 0_u64;
    let mut observed_weighted_present_cell_keys: HashSet<(String, u64)> = HashSet::new();
    let mut seen_partition_size_cells = HashSet::new();
    for cell in matrix_cells {
        let cell_obj = cell
            .as_object()
            .ok_or_else(|| "matrix cell must be an object".to_string())?;
        for field in &[
            "workload_partition",
            "session_messages",
            "scenario_id",
            "status",
            "stage_attribution",
            "swarm_metrics",
            "primary_e2e",
            "lineage",
        ] {
            if !cell_obj.contains_key(*field) {
                return Err(format!("matrix cell missing {field}"));
            }
        }
        let workload_partition = cell_obj
            .get("workload_partition")
            .and_then(Value::as_str)
            .ok_or_else(|| "matrix cell workload_partition must be a string".to_string())?;
        if !required_partitions.contains(workload_partition) {
            return Err(format!(
                "matrix cell workload_partition '{workload_partition}' not listed in matrix_requirements.required_partition_tags"
            ));
        }
        let session_messages = cell_obj
            .get("session_messages")
            .and_then(Value::as_u64)
            .ok_or_else(|| "matrix cell session_messages must be an integer".to_string())?;
        if !required_sizes.contains(&session_messages) {
            return Err(format!(
                "matrix cell session_messages ({session_messages}) not listed in matrix_requirements.required_session_message_sizes"
            ));
        }
        let partition_size_key = (workload_partition.to_string(), session_messages);
        if !seen_partition_size_cells.insert(partition_size_key.clone()) {
            return Err(format!(
                "matrix cell duplicates partition-size key ({workload_partition}, {session_messages})"
            ));
        }

        let status = cell_obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(status, "pass" | "fail") {
            return Err(format!("matrix cell has invalid status: {status}"));
        }

        let stage = cell_obj
            .get("stage_attribution")
            .and_then(Value::as_object)
            .ok_or_else(|| "matrix cell stage_attribution must be object".to_string())?;
        for field in &[
            "open_ms",
            "append_ms",
            "save_ms",
            "index_ms",
            "total_stage_ms",
        ] {
            if !stage.contains_key(*field) {
                return Err(format!("matrix cell stage_attribution missing {field}"));
            }
        }
        let total_stage_ms = stage
            .get("total_stage_ms")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                "matrix cell stage_attribution.total_stage_ms must be a finite number".to_string()
            })?;
        if !total_stage_ms.is_finite() || total_stage_ms <= 0.0 {
            return Err(format!(
                "matrix cell stage_attribution.total_stage_ms must be > 0, got: {total_stage_ms}"
            ));
        }
        let mut missing_stage_metrics = 0_u64;
        for key in &expected_required_stage_keys {
            if stage.get(*key).is_some_and(|value| !value.is_null()) {
                *observed_stage_coverage.entry(*key).or_insert(0) += 1;
            } else {
                missing_stage_metrics += 1;
            }
        }
        if missing_stage_metrics == 0 {
            observed_complete_stage_breakdown_cells += 1;
        } else {
            observed_missing_stage_breakdown_cells += 1;
            observed_missing_stage_cell_keys.insert(partition_size_key.clone());
            let missing_reasons = cell_obj
                .get("missing_reasons")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    format!(
                        "matrix cell ({workload_partition}, {session_messages}) missing missing_reasons array despite incomplete stage attribution"
                    )
                })?;
            if missing_reasons.is_empty() {
                return Err(format!(
                    "matrix cell ({workload_partition}, {session_messages}) missing_reasons must not be empty when stage attribution is incomplete"
                ));
            }
            let mut missing_reason_set = BTreeSet::new();
            for reason in missing_reasons {
                let reason = reason.as_str().ok_or_else(|| {
                    format!(
                        "matrix cell ({workload_partition}, {session_messages}) missing_reasons entries must be strings"
                    )
                })?;
                if reason.trim().is_empty() {
                    return Err(format!(
                        "matrix cell ({workload_partition}, {session_messages}) missing_reasons entries must be non-empty strings"
                    ));
                }
                if !missing_reason_set.insert(reason.to_string()) {
                    return Err(format!(
                        "matrix cell ({workload_partition}, {session_messages}) missing_reasons must not contain duplicates: {reason}"
                    ));
                }
            }
            if !missing_reason_set
                .iter()
                .any(|reason| reason.starts_with("missing_stage_metrics:"))
            {
                return Err(format!(
                    "matrix cell ({workload_partition}, {session_messages}) missing_reasons must include at least one missing_stage_metrics:* reason when stage attribution is incomplete"
                ));
            }
            observed_missing_stage_reasons_by_key
                .insert(partition_size_key.clone(), missing_reason_set);
        }

        let swarm_complete = validate_swarm_metrics_value(
            cell_obj
                .get("swarm_metrics")
                .ok_or_else(|| "matrix cell missing swarm_metrics".to_string())?,
            "matrix cell swarm_metrics",
            true,
        )?;
        let missing_reason_set = match cell_obj.get("missing_reasons") {
            Some(value) => collect_string_set(
                value,
                &format!("matrix cell ({workload_partition}, {session_messages}) missing_reasons"),
            )?,
            None => BTreeSet::new(),
        };
        let has_missing_swarm_reason = missing_reason_set
            .iter()
            .any(|reason| reason.starts_with(SWARM_FAIL_CLOSED_REASON_PREFIX));
        if swarm_complete {
            observed_complete_swarm_metric_cells += 1;
            if has_missing_swarm_reason {
                return Err(format!(
                    "matrix cell ({workload_partition}, {session_messages}) has complete swarm_metrics but missing_reasons includes {SWARM_FAIL_CLOSED_REASON_PREFIX}"
                ));
            }
        } else {
            observed_missing_swarm_metric_cells += 1;
            observed_missing_swarm_cell_keys.insert(partition_size_key.clone());
            if !has_missing_swarm_reason {
                return Err(format!(
                    "matrix cell ({workload_partition}, {session_messages}) incomplete swarm_metrics must include {SWARM_FAIL_CLOSED_REASON_PREFIX} in missing_reasons"
                ));
            }
            observed_missing_swarm_reasons_by_key
                .insert(partition_size_key.clone(), missing_reason_set);
        }

        let primary = cell_obj
            .get("primary_e2e")
            .and_then(Value::as_object)
            .ok_or_else(|| "matrix cell primary_e2e must be object".to_string())?;
        for field in &["wall_clock_ms", "rust_vs_node_ratio", "rust_vs_bun_ratio"] {
            if !primary.contains_key(*field) {
                return Err(format!("matrix cell primary_e2e missing {field}"));
            }
            // Only require positive values for passing cells; "fail" cells
            // may have null metrics when the underlying data is missing.
            if status == "pass" {
                let _ = require_positive_metric(primary, "matrix cell primary_e2e", field)?;
            } else {
                let _ =
                    require_nullable_positive_metric(primary, "matrix cell primary_e2e", field)?;
            }
        }

        if status == "pass" {
            observed_weighted_valid_cell_count += 1;
            observed_weighted_present_cell_keys.insert(partition_size_key);
        }
    }

    let stage_summary = record
        .get("stage_summary")
        .and_then(Value::as_object)
        .ok_or_else(|| "stage_summary must be an object".to_string())?;
    for field in &[
        "required_stage_keys",
        "operation_stage_coverage",
        "cells_with_complete_stage_breakdown",
        "cells_missing_stage_breakdown",
        "covered_cells",
        "missing_cells",
    ] {
        if !stage_summary.contains_key(*field) {
            return Err(format!("stage_summary missing {field}"));
        }
    }
    let required_stage_keys = stage_summary
        .get("required_stage_keys")
        .and_then(Value::as_array)
        .ok_or_else(|| "stage_summary.required_stage_keys must be an array".to_string())?;
    let parsed_required_stage_keys = required_stage_keys
        .iter()
        .map(|value| {
            value.as_str().ok_or_else(|| {
                "stage_summary.required_stage_keys entries must be strings".to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed_required_stage_keys != expected_required_stage_keys {
        return Err(format!(
            "stage_summary.required_stage_keys must equal {expected_required_stage_keys:?}, got {parsed_required_stage_keys:?}"
        ));
    }
    let operation_stage_coverage = stage_summary
        .get("operation_stage_coverage")
        .and_then(Value::as_object)
        .ok_or_else(|| "stage_summary.operation_stage_coverage must be an object".to_string())?;
    for key in &expected_required_stage_keys {
        let reported = operation_stage_coverage
            .get(*key)
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                format!("stage_summary.operation_stage_coverage.{key} must be an integer count")
            })?;
        let observed = observed_stage_coverage.get(*key).copied().unwrap_or(0);
        if reported != observed {
            return Err(format!(
                "stage_summary.operation_stage_coverage.{key} ({reported}) must equal observed non-null stage_attribution count ({observed}) across matrix_cells"
            ));
        }
    }
    let unexpected_stage_coverage_keys = operation_stage_coverage
        .keys()
        .filter(|key| !expected_required_stage_keys.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected_stage_coverage_keys.is_empty() {
        return Err(format!(
            "stage_summary.operation_stage_coverage has unexpected keys: {unexpected_stage_coverage_keys:?}"
        ));
    }
    let covered_cells = stage_summary
        .get("covered_cells")
        .and_then(Value::as_u64)
        .ok_or_else(|| "stage_summary.covered_cells must be an integer".to_string())?;
    let complete_cells = stage_summary
        .get("cells_with_complete_stage_breakdown")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "stage_summary.cells_with_complete_stage_breakdown must be an integer".to_string()
        })?;
    let missing_cells_count = stage_summary
        .get("cells_missing_stage_breakdown")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "stage_summary.cells_missing_stage_breakdown must be an integer".to_string()
        })?;
    let missing_cells = stage_summary
        .get("missing_cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "stage_summary.missing_cells must be an array".to_string())?;
    if complete_cells + missing_cells_count != matrix_cells.len() as u64 {
        return Err(format!(
            "stage_summary complete+missing ({complete_cells}+{missing_cells_count}) must equal matrix_cells length ({})",
            matrix_cells.len()
        ));
    }
    if complete_cells != observed_complete_stage_breakdown_cells {
        return Err(format!(
            "stage_summary.cells_with_complete_stage_breakdown ({complete_cells}) must equal observed complete-stage cell count ({observed_complete_stage_breakdown_cells}) derived from matrix_cells.stage_attribution"
        ));
    }
    if missing_cells_count != observed_missing_stage_breakdown_cells {
        return Err(format!(
            "stage_summary.cells_missing_stage_breakdown ({missing_cells_count}) must equal observed missing-stage cell count ({observed_missing_stage_breakdown_cells}) derived from matrix_cells.stage_attribution"
        ));
    }
    if covered_cells != complete_cells {
        return Err(format!(
            "stage_summary.covered_cells ({covered_cells}) must equal cells_with_complete_stage_breakdown ({complete_cells})"
        ));
    }
    if missing_cells.len() as u64 != missing_cells_count {
        return Err(format!(
            "stage_summary.missing_cells length ({}) must equal cells_missing_stage_breakdown ({missing_cells_count})",
            missing_cells.len()
        ));
    }
    let mut reported_missing_stage_cell_keys = HashSet::new();
    for missing_cell in missing_cells {
        let missing_cell_obj = missing_cell
            .as_object()
            .ok_or_else(|| "stage_summary.missing_cells entries must be objects".to_string())?;
        let workload_partition = missing_cell_obj
            .get("workload_partition")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "stage_summary.missing_cells entries must include workload_partition string"
                    .to_string()
            })?;
        let session_messages = missing_cell_obj
            .get("session_messages")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "stage_summary.missing_cells entries must include session_messages integer"
                    .to_string()
            })?;
        let reasons = missing_cell_obj
            .get("reasons")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                "stage_summary.missing_cells entries must include reasons array".to_string()
            })?;
        if reasons.is_empty() {
            return Err(
                "stage_summary.missing_cells entries must include at least one reason".to_string(),
            );
        }
        let mut reason_set = BTreeSet::new();
        for reason in reasons {
            let reason = reason.as_str().ok_or_else(|| {
                "stage_summary.missing_cells reasons entries must be strings".to_string()
            })?;
            if reason.trim().is_empty() {
                return Err(
                    "stage_summary.missing_cells reasons entries must be non-empty strings"
                        .to_string(),
                );
            }
            if !reason_set.insert(reason.to_string()) {
                return Err(format!(
                    "stage_summary.missing_cells reasons must not contain duplicates: {reason}"
                ));
            }
        }
        if !reason_set
            .iter()
            .any(|reason| reason.starts_with("missing_stage_metrics:"))
        {
            return Err(format!(
                "stage_summary.missing_cells entry ({workload_partition}, {session_messages}) reasons must include at least one missing_stage_metrics:* token"
            ));
        }
        let missing_key = (workload_partition.to_string(), session_messages);
        if !reported_missing_stage_cell_keys.insert(missing_key.clone()) {
            return Err(format!(
                "stage_summary.missing_cells must not contain duplicate partition-size entries: ({workload_partition}, {session_messages})"
            ));
        }
        if !observed_missing_stage_cell_keys.contains(&missing_key) {
            return Err(format!(
                "stage_summary.missing_cells entry ({workload_partition}, {session_messages}) does not match any matrix cell with missing stage metrics"
            ));
        }
        let observed_reason_set = observed_missing_stage_reasons_by_key
            .get(&missing_key)
            .ok_or_else(|| {
                format!(
                    "stage_summary.missing_cells entry ({workload_partition}, {session_messages}) is missing observed matrix-cell reason linkage"
                )
            })?;
        if &reason_set != observed_reason_set {
            return Err(format!(
                "stage_summary.missing_cells entry ({workload_partition}, {session_messages}) reasons {reason_set:?} must equal matrix cell missing_reasons {observed_reason_set:?}",
            ));
        }
    }

    let swarm_summary = record
        .get("swarm_summary")
        .and_then(Value::as_object)
        .ok_or_else(|| "swarm_summary must be an object".to_string())?;
    for field in &[
        "required_latency_quantiles",
        "required_queue_depth_quantiles",
        "required_resource_usage_keys",
        "required_component_breakdown_keys",
        "required_stage_breakdown_keys",
        "cells_with_complete_swarm_metrics",
        "cells_missing_swarm_metrics",
        "missing_cells",
    ] {
        if !swarm_summary.contains_key(*field) {
            return Err(format!("swarm_summary missing {field}"));
        }
    }
    require_string_array_eq(
        swarm_summary,
        "required_latency_quantiles",
        SWARM_LATENCY_QUANTILES,
        "swarm_summary",
    )?;
    require_string_array_eq(
        swarm_summary,
        "required_queue_depth_quantiles",
        SWARM_QUEUE_DEPTH_QUANTILES,
        "swarm_summary",
    )?;
    require_string_array_eq(
        swarm_summary,
        "required_resource_usage_keys",
        SWARM_RESOURCE_USAGE_KEYS,
        "swarm_summary",
    )?;
    require_string_array_eq(
        swarm_summary,
        "required_component_breakdown_keys",
        SWARM_COMPONENT_BREAKDOWN_KEYS,
        "swarm_summary",
    )?;
    require_string_array_eq(
        swarm_summary,
        "required_stage_breakdown_keys",
        SWARM_STAGE_BREAKDOWN_KEYS,
        "swarm_summary",
    )?;

    let complete_swarm_cells = swarm_summary
        .get("cells_with_complete_swarm_metrics")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "swarm_summary.cells_with_complete_swarm_metrics must be an integer".to_string()
        })?;
    let missing_swarm_cells_count = swarm_summary
        .get("cells_missing_swarm_metrics")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "swarm_summary.cells_missing_swarm_metrics must be an integer".to_string()
        })?;
    let swarm_missing_cells = swarm_summary
        .get("missing_cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "swarm_summary.missing_cells must be an array".to_string())?;
    if complete_swarm_cells + missing_swarm_cells_count != matrix_cells.len() as u64 {
        return Err(format!(
            "swarm_summary complete+missing ({complete_swarm_cells}+{missing_swarm_cells_count}) must equal matrix_cells length ({})",
            matrix_cells.len()
        ));
    }
    if complete_swarm_cells != observed_complete_swarm_metric_cells {
        return Err(format!(
            "swarm_summary.cells_with_complete_swarm_metrics ({complete_swarm_cells}) must equal observed complete swarm_metrics cell count ({observed_complete_swarm_metric_cells})"
        ));
    }
    if missing_swarm_cells_count != observed_missing_swarm_metric_cells {
        return Err(format!(
            "swarm_summary.cells_missing_swarm_metrics ({missing_swarm_cells_count}) must equal observed missing swarm_metrics cell count ({observed_missing_swarm_metric_cells})"
        ));
    }
    if swarm_missing_cells.len() as u64 != missing_swarm_cells_count {
        return Err(format!(
            "swarm_summary.missing_cells length ({}) must equal cells_missing_swarm_metrics ({missing_swarm_cells_count})",
            swarm_missing_cells.len()
        ));
    }
    let mut reported_missing_swarm_cell_keys = HashSet::new();
    for missing_cell in swarm_missing_cells {
        let missing_cell_obj = missing_cell
            .as_object()
            .ok_or_else(|| "swarm_summary.missing_cells entries must be objects".to_string())?;
        let workload_partition = missing_cell_obj
            .get("workload_partition")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "swarm_summary.missing_cells entries must include workload_partition string"
                    .to_string()
            })?;
        let session_messages = missing_cell_obj
            .get("session_messages")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "swarm_summary.missing_cells entries must include session_messages integer"
                    .to_string()
            })?;
        let reasons = collect_string_set(
            missing_cell_obj.get("reasons").ok_or_else(|| {
                "swarm_summary.missing_cells entries must include reasons".to_string()
            })?,
            "swarm_summary.missing_cells.reasons",
        )?;
        if !reasons
            .iter()
            .any(|reason| reason.starts_with(SWARM_FAIL_CLOSED_REASON_PREFIX))
        {
            return Err(format!(
                "swarm_summary.missing_cells entry ({workload_partition}, {session_messages}) reasons must include {SWARM_FAIL_CLOSED_REASON_PREFIX}"
            ));
        }
        let missing_key = (workload_partition.to_string(), session_messages);
        if !reported_missing_swarm_cell_keys.insert(missing_key.clone()) {
            return Err(format!(
                "swarm_summary.missing_cells must not contain duplicate partition-size entries: ({workload_partition}, {session_messages})"
            ));
        }
        if !observed_missing_swarm_cell_keys.contains(&missing_key) {
            return Err(format!(
                "swarm_summary.missing_cells entry ({workload_partition}, {session_messages}) does not match any matrix cell with missing swarm_metrics"
            ));
        }
        let observed_reason_set = observed_missing_swarm_reasons_by_key
            .get(&missing_key)
            .ok_or_else(|| {
                format!(
                    "swarm_summary.missing_cells entry ({workload_partition}, {session_messages}) is missing observed matrix-cell reason linkage"
                )
            })?;
        if &reasons != observed_reason_set {
            return Err(format!(
                "swarm_summary.missing_cells entry ({workload_partition}, {session_messages}) reasons {reasons:?} must equal matrix cell missing_reasons {observed_reason_set:?}",
            ));
        }
    }

    let weighted_bottleneck_attribution = record
        .get("weighted_bottleneck_attribution")
        .and_then(Value::as_object)
        .ok_or_else(|| "weighted_bottleneck_attribution must be an object".to_string())?;
    for field in &[
        "schema",
        "status",
        "weighting_policy",
        "confidence_method",
        "per_scale",
        "global_ranking",
        "lineage",
    ] {
        if !weighted_bottleneck_attribution.contains_key(*field) {
            return Err(format!("weighted_bottleneck_attribution missing {field}"));
        }
    }
    let weighted_schema = weighted_bottleneck_attribution
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if weighted_schema != "pi.perf.phase1_weighted_bottleneck_attribution.v1" {
        return Err(format!(
            "weighted_bottleneck_attribution.schema must be pi.perf.phase1_weighted_bottleneck_attribution.v1, got: {weighted_schema}"
        ));
    }
    let weighted_status = weighted_bottleneck_attribution
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "weighted_bottleneck_attribution.status must be a string".to_string())?;
    if !matches!(weighted_status, "computed" | "missing") {
        return Err(format!(
            "weighted_bottleneck_attribution.status must be one of computed/missing, got: {weighted_status}"
        ));
    }
    let weighting_policy = weighted_bottleneck_attribution
        .get("weighting_policy")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if weighting_policy != "session_messages" {
        return Err(format!(
            "weighted_bottleneck_attribution.weighting_policy must be session_messages, got: {weighting_policy}"
        ));
    }
    let confidence_method = weighted_bottleneck_attribution
        .get("confidence_method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if confidence_method != "weighted_normal_approx_95" {
        return Err(format!(
            "weighted_bottleneck_attribution.confidence_method must be weighted_normal_approx_95, got: {confidence_method}"
        ));
    }

    let weighted_lineage = weighted_bottleneck_attribution
        .get("lineage")
        .and_then(Value::as_object)
        .ok_or_else(|| "weighted_bottleneck_attribution.lineage must be an object".to_string())?;
    for field in &["source_stream", "source_cell_count", "valid_cell_count"] {
        if !weighted_lineage.contains_key(*field) {
            return Err(format!(
                "weighted_bottleneck_attribution.lineage missing {field}"
            ));
        }
    }
    let source_stream = weighted_lineage
        .get("source_stream")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if source_stream != "phase1_matrix_validation.matrix_cells" {
        return Err(format!(
            "weighted_bottleneck_attribution.lineage.source_stream must be phase1_matrix_validation.matrix_cells, got: {source_stream}"
        ));
    }
    let source_cell_count = weighted_lineage
        .get("source_cell_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "weighted_bottleneck_attribution.lineage.source_cell_count must be an integer"
                .to_string()
        })?;
    if source_cell_count != matrix_cells.len() as u64 {
        return Err(format!(
            "weighted_bottleneck_attribution.lineage.source_cell_count ({source_cell_count}) must equal matrix_cells length ({})",
            matrix_cells.len()
        ));
    }
    let valid_cell_count = weighted_lineage
        .get("valid_cell_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "weighted_bottleneck_attribution.lineage.valid_cell_count must be an integer"
                .to_string()
        })?;
    if valid_cell_count > source_cell_count {
        return Err(format!(
            "weighted_bottleneck_attribution.lineage.valid_cell_count ({valid_cell_count}) must be <= source_cell_count ({source_cell_count})"
        ));
    }
    if valid_cell_count != observed_weighted_valid_cell_count {
        return Err(format!(
            "weighted_bottleneck_attribution.lineage.valid_cell_count ({valid_cell_count}) must equal observed pass-cell count with valid stage totals ({observed_weighted_valid_cell_count})"
        ));
    }

    let weighted_per_scale = weighted_bottleneck_attribution
        .get("per_scale")
        .and_then(Value::as_array)
        .ok_or_else(|| "weighted_bottleneck_attribution.per_scale must be an array".to_string())?;
    let weighted_global_ranking = weighted_bottleneck_attribution
        .get("global_ranking")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "weighted_bottleneck_attribution.global_ranking must be an array".to_string()
        })?;

    if weighted_status == "missing" {
        if valid_cell_count != 0 {
            return Err(format!(
                "weighted_bottleneck_attribution.status is missing but lineage.valid_cell_count is {valid_cell_count} (expected 0)"
            ));
        }
        if !weighted_per_scale.is_empty() {
            return Err(
                "weighted_bottleneck_attribution.status=missing requires empty per_scale"
                    .to_string(),
            );
        }
        if !weighted_global_ranking.is_empty() {
            return Err(
                "weighted_bottleneck_attribution.status=missing requires empty global_ranking"
                    .to_string(),
            );
        }
    } else {
        if valid_cell_count == 0 {
            return Err(
                "weighted_bottleneck_attribution.status=computed requires lineage.valid_cell_count > 0"
                    .to_string(),
            );
        }
        if weighted_per_scale.len() != required_sizes.len() {
            return Err(format!(
                "weighted_bottleneck_attribution.per_scale length ({}) must equal required session size count ({})",
                weighted_per_scale.len(),
                required_sizes.len()
            ));
        }
        let mut seen_weighted_scales = HashSet::new();
        let mut observed_weighted_present_keys_from_per_scale: HashSet<(String, u64)> =
            HashSet::new();
        for per_scale_row in weighted_per_scale {
            let row_obj = per_scale_row.as_object().ok_or_else(|| {
                "weighted_bottleneck_attribution.per_scale entries must be objects".to_string()
            })?;
            let session_messages = row_obj
                .get("session_messages")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    "weighted_bottleneck_attribution.per_scale.session_messages must be an integer"
                        .to_string()
                })?;
            if !required_sizes.contains(&session_messages) {
                return Err(format!(
                    "weighted_bottleneck_attribution.per_scale session_messages ({session_messages}) must be listed in matrix_requirements.required_session_message_sizes"
                ));
            }
            if !seen_weighted_scales.insert(session_messages) {
                return Err(format!(
                    "weighted_bottleneck_attribution.per_scale must not contain duplicate session_messages entries: {session_messages}"
                ));
            }
            let partitions = row_obj
                .get("partitions")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    "weighted_bottleneck_attribution.per_scale.partitions must be an array"
                        .to_string()
                })?;
            if partitions.len() != required_partitions.len() {
                return Err(format!(
                    "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}) partitions length ({}) must equal required partition count ({})",
                    partitions.len(),
                    required_partitions.len()
                ));
            }
            let mut seen_partitions_for_scale = HashSet::new();
            for partition_row in partitions {
                let partition_obj = partition_row.as_object().ok_or_else(|| {
                    "weighted_bottleneck_attribution.per_scale.partitions entries must be objects"
                        .to_string()
                })?;
                for field in &["workload_partition", "present", "scenario_id", "stage_pct"] {
                    if !partition_obj.contains_key(*field) {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale.partitions entry missing {field}"
                        ));
                    }
                }
                let workload_partition = partition_obj
                    .get("workload_partition")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "weighted_bottleneck_attribution.per_scale.partitions.workload_partition must be a string"
                            .to_string()
                    })?;
                if !required_partitions.contains(workload_partition) {
                    return Err(format!(
                        "weighted_bottleneck_attribution.per_scale includes partition '{workload_partition}' not declared in matrix_requirements.required_partition_tags"
                    ));
                }
                if !seen_partitions_for_scale.insert(workload_partition.to_string()) {
                    return Err(format!(
                        "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}) has duplicate partition entry: {workload_partition}"
                    ));
                }
                let present = partition_obj
                    .get("present")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        "weighted_bottleneck_attribution.per_scale.partitions.present must be a boolean"
                            .to_string()
                    })?;
                let scenario_id = partition_obj
                    .get("scenario_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "weighted_bottleneck_attribution.per_scale.partitions.scenario_id must be a string"
                            .to_string()
                    })?;
                if scenario_id.trim().is_empty() {
                    return Err(
                        "weighted_bottleneck_attribution.per_scale.partitions.scenario_id must be non-empty"
                            .to_string(),
                    );
                }
                let stage_pct = partition_obj
                    .get("stage_pct")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        "weighted_bottleneck_attribution.per_scale.partitions.stage_pct must be an object"
                            .to_string()
                    })?;
                for stage in &expected_required_stage_keys {
                    if !stage_pct.contains_key(*stage) {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale.partitions.stage_pct missing {stage}"
                        ));
                    }
                }
                let unexpected_stage_keys = stage_pct
                    .keys()
                    .filter(|key| !expected_required_stage_keys.contains(&key.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !unexpected_stage_keys.is_empty() {
                    return Err(format!(
                        "weighted_bottleneck_attribution.per_scale.partitions.stage_pct has unexpected keys: {unexpected_stage_keys:?}"
                    ));
                }
                let mut non_null_stage_pct_count = 0_u64;
                let mut stage_pct_sum = 0.0_f64;
                for stage in &expected_required_stage_keys {
                    match stage_pct.get(*stage) {
                        Some(value) if value.is_null() => {}
                        Some(value) => {
                            let value = value.as_f64().ok_or_else(|| {
                                format!(
                                    "weighted_bottleneck_attribution.per_scale.partitions.stage_pct.{stage} must be null or finite number"
                                )
                            })?;
                            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                                return Err(format!(
                                    "weighted_bottleneck_attribution.per_scale.partitions.stage_pct.{stage} must be in [0, 100], got: {value}"
                                ));
                            }
                            non_null_stage_pct_count += 1;
                            stage_pct_sum += value;
                        }
                        None => {
                            return Err(format!(
                                "weighted_bottleneck_attribution.per_scale.partitions.stage_pct missing {stage}"
                            ));
                        }
                    }
                }
                let has_total_stage_ms = partition_obj.contains_key("total_stage_ms")
                    && !partition_obj
                        .get("total_stage_ms")
                        .is_some_and(Value::is_null);
                if present {
                    observed_weighted_present_keys_from_per_scale
                        .insert((workload_partition.to_string(), session_messages));
                    if non_null_stage_pct_count > 0
                        && non_null_stage_pct_count != expected_required_stage_keys.len() as u64
                    {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}, workload_partition={workload_partition}) must provide either all stage_pct values or all nulls"
                        ));
                    }
                    if non_null_stage_pct_count == expected_required_stage_keys.len() as u64
                        && (stage_pct_sum - 100.0).abs() > 0.5
                    {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}, workload_partition={workload_partition}) stage_pct sum ({stage_pct_sum}) must be approximately 100"
                        ));
                    }
                    if has_total_stage_ms {
                        let total_stage_ms = partition_obj
                            .get("total_stage_ms")
                            .and_then(Value::as_f64)
                            .ok_or_else(|| {
                                "weighted_bottleneck_attribution.per_scale.partitions.total_stage_ms must be a positive number when present"
                                    .to_string()
                            })?;
                        if !total_stage_ms.is_finite() || total_stage_ms <= 0.0 {
                            return Err(format!(
                                "weighted_bottleneck_attribution.per_scale.partitions.total_stage_ms must be > 0, got: {total_stage_ms}"
                            ));
                        }
                    } else if non_null_stage_pct_count > 0 {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}, workload_partition={workload_partition}) must include total_stage_ms when stage_pct values are present"
                        ));
                    }
                } else {
                    if non_null_stage_pct_count != 0 {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}, workload_partition={workload_partition}) with present=false must provide null stage_pct values"
                        ));
                    }
                    if has_total_stage_ms {
                        return Err(format!(
                            "weighted_bottleneck_attribution.per_scale(session_messages={session_messages}, workload_partition={workload_partition}) with present=false must not include total_stage_ms"
                        ));
                    }
                }
            }
        }
        if seen_weighted_scales != required_sizes {
            return Err(format!(
                "weighted_bottleneck_attribution.per_scale must cover all required session sizes; observed {seen_weighted_scales:?}, required {required_sizes:?}"
            ));
        }
        if observed_weighted_present_keys_from_per_scale != observed_weighted_present_cell_keys {
            return Err(format!(
                "weighted_bottleneck_attribution.per_scale present keys {observed_weighted_present_keys_from_per_scale:?} must match observed pass-cell keys {observed_weighted_present_cell_keys:?}"
            ));
        }

        if weighted_global_ranking.len() != expected_required_stage_keys.len() {
            return Err(format!(
                "weighted_bottleneck_attribution.global_ranking length ({}) must equal stage key count ({})",
                weighted_global_ranking.len(),
                expected_required_stage_keys.len()
            ));
        }
        let mut seen_weighted_ranking_stages = HashSet::new();
        let mut previous_weighted_contribution_pct = f64::INFINITY;
        let mut weighted_contribution_sum = 0.0_f64;
        for ranking_row in weighted_global_ranking {
            let ranking_obj = ranking_row.as_object().ok_or_else(|| {
                "weighted_bottleneck_attribution.global_ranking entries must be objects".to_string()
            })?;
            for field in &[
                "stage",
                "weighted_stage_ms",
                "weighted_contribution_pct",
                "mean_share_pct",
                "ci95_lower_pct",
                "ci95_upper_pct",
                "sample_size",
            ] {
                if !ranking_obj.contains_key(*field) {
                    return Err(format!(
                        "weighted_bottleneck_attribution.global_ranking entry missing {field}"
                    ));
                }
            }
            let stage = ranking_obj
                .get("stage")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "weighted_bottleneck_attribution.global_ranking.stage must be a string"
                        .to_string()
                })?;
            if !expected_required_stage_keys.contains(&stage) {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking has unknown stage: {stage}"
                ));
            }
            if !seen_weighted_ranking_stages.insert(stage.to_string()) {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking must not contain duplicate stage entries: {stage}"
                ));
            }
            let weighted_stage_ms = require_non_negative_metric(
                ranking_obj,
                "weighted_bottleneck_attribution.global_ranking",
                "weighted_stage_ms",
            )?;
            if weighted_stage_ms == 0.0 {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} has zero weighted_stage_ms"
                ));
            }
            let weighted_contribution_pct = require_non_negative_metric(
                ranking_obj,
                "weighted_bottleneck_attribution.global_ranking",
                "weighted_contribution_pct",
            )?;
            if weighted_contribution_pct > 100.0 {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} has weighted_contribution_pct > 100: {weighted_contribution_pct}"
                ));
            }
            if weighted_contribution_pct > previous_weighted_contribution_pct + 1e-9 {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking must be sorted descending by weighted_contribution_pct; stage {stage} has {weighted_contribution_pct} after {previous_weighted_contribution_pct}"
                ));
            }
            previous_weighted_contribution_pct = weighted_contribution_pct;
            weighted_contribution_sum += weighted_contribution_pct;

            let mean_share_pct = require_nullable_percentage_metric(
                ranking_obj,
                "weighted_bottleneck_attribution.global_ranking",
                "mean_share_pct",
            )?;
            let ci95_lower_pct = require_nullable_percentage_metric(
                ranking_obj,
                "weighted_bottleneck_attribution.global_ranking",
                "ci95_lower_pct",
            )?;
            let ci95_upper_pct = require_nullable_percentage_metric(
                ranking_obj,
                "weighted_bottleneck_attribution.global_ranking",
                "ci95_upper_pct",
            )?;
            if ci95_lower_pct.is_some() != ci95_upper_pct.is_some() {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} must provide both ci95_lower_pct and ci95_upper_pct together"
                ));
            }
            if let (Some(lower), Some(upper)) = (ci95_lower_pct, ci95_upper_pct) {
                if lower > upper {
                    return Err(format!(
                        "weighted_bottleneck_attribution.global_ranking stage {stage} has ci95_lower_pct ({lower}) > ci95_upper_pct ({upper})"
                    ));
                }
                if let Some(mean) = mean_share_pct
                    && (mean < lower || mean > upper)
                {
                    return Err(format!(
                        "weighted_bottleneck_attribution.global_ranking stage {stage} mean_share_pct ({mean}) must lie within CI [{lower}, {upper}]"
                    ));
                }
            }
            let sample_size = ranking_obj
                .get("sample_size")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    "weighted_bottleneck_attribution.global_ranking.sample_size must be an integer"
                        .to_string()
                })?;
            if sample_size == 0 {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} sample_size must be > 0"
                ));
            }
            if sample_size > valid_cell_count {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} sample_size ({sample_size}) must be <= lineage.valid_cell_count ({valid_cell_count})"
                ));
            }
            if sample_size > 1 && (ci95_lower_pct.is_none() || ci95_upper_pct.is_none()) {
                return Err(format!(
                    "weighted_bottleneck_attribution.global_ranking stage {stage} sample_size ({sample_size}) > 1 requires CI bounds"
                ));
            }
        }
        if (weighted_contribution_sum - 100.0).abs() > 0.5 {
            return Err(format!(
                "weighted_bottleneck_attribution.global_ranking weighted_contribution_pct values must sum to approximately 100 (observed {weighted_contribution_sum})"
            ));
        }
    }

    let primary_outcomes = record
        .get("primary_outcomes")
        .and_then(Value::as_object)
        .ok_or_else(|| "primary_outcomes must be an object".to_string())?;
    for field in &[
        "status",
        "wall_clock_ms",
        "rust_vs_node_ratio",
        "rust_vs_bun_ratio",
        "ordering_policy",
    ] {
        if !primary_outcomes.contains_key(*field) {
            return Err(format!("primary_outcomes missing {field}"));
        }
    }
    let primary_status = primary_outcomes
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "primary_outcomes.status must be a string".to_string())?;
    if !matches!(primary_status, "pass" | "fail") {
        return Err(format!(
            "primary_outcomes.status has invalid value: {primary_status}"
        ));
    }
    if primary_status == "pass" {
        for field in &["wall_clock_ms", "rust_vs_node_ratio", "rust_vs_bun_ratio"] {
            let _ = require_positive_metric(primary_outcomes, "primary_outcomes", field)?;
        }
    } else {
        for field in &["wall_clock_ms", "rust_vs_node_ratio", "rust_vs_bun_ratio"] {
            let _ = require_nullable_positive_metric(primary_outcomes, "primary_outcomes", field)?;
        }
    }
    let ordering_policy = primary_outcomes
        .get("ordering_policy")
        .and_then(Value::as_str)
        .ok_or_else(|| "primary_outcomes.ordering_policy must be a string".to_string())?;
    if ordering_policy != "primary_e2e_before_microbench" {
        return Err(format!(
            "primary_outcomes.ordering_policy must be 'primary_e2e_before_microbench', got: {ordering_policy}"
        ));
    }

    let regression_guards = record
        .get("regression_guards")
        .and_then(Value::as_object)
        .ok_or_else(|| "regression_guards must be an object".to_string())?;
    for field in &[
        "memory",
        "correctness",
        "security",
        "failure_or_gap_reasons",
    ] {
        if !regression_guards.contains_key(*field) {
            return Err(format!("regression_guards missing {field}"));
        }
    }
    let failure_or_gap_reasons = regression_guards
        .get("failure_or_gap_reasons")
        .and_then(Value::as_array)
        .ok_or_else(|| "regression_guards.failure_or_gap_reasons must be an array".to_string())?;
    let mut reason_set = HashSet::new();
    let mut memory_guard_status = "";
    let mut correctness_guard_status = "";
    let mut security_guard_status = "";
    for reason in failure_or_gap_reasons {
        let reason = reason.as_str().ok_or_else(|| {
            "regression_guards.failure_or_gap_reasons entries must be non-empty strings".to_string()
        })?;
        if reason.trim().is_empty() {
            return Err(
                "regression_guards.failure_or_gap_reasons entries must be non-empty strings"
                    .to_string(),
            );
        }
        if !reason_set.insert(reason.to_string()) {
            return Err(format!(
                "regression_guards.failure_or_gap_reasons must not contain duplicates: {reason}"
            ));
        }
    }
    for guard_name in ["memory", "correctness", "security"] {
        let status = regression_guards
            .get(guard_name)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("regression_guards.{guard_name} must be one of pass/fail/missing")
            })?;
        match guard_name {
            "memory" => memory_guard_status = status,
            "correctness" => correctness_guard_status = status,
            "security" => security_guard_status = status,
            _ => {}
        }
        if !matches!(status, "pass" | "fail" | "missing") {
            return Err(format!(
                "regression_guards.{guard_name} must be one of pass/fail/missing, got: {status}"
            ));
        }
        let fail_reason = format!("{guard_name}_regression");
        let unverified_reason = format!("{guard_name}_regression_unverified");
        let has_fail_reason = reason_set.contains(&fail_reason);
        let has_unverified_reason = reason_set.contains(&unverified_reason);
        match status {
            "pass" if has_fail_reason || has_unverified_reason => {
                return Err(format!(
                    "regression_guards.{guard_name} is pass but failure_or_gap_reasons includes {fail_reason} or {unverified_reason}"
                ));
            }
            "fail" => {
                if !has_fail_reason {
                    return Err(format!(
                        "regression_guards.{guard_name} is fail and failure_or_gap_reasons must include {fail_reason}"
                    ));
                }
                if has_unverified_reason {
                    return Err(format!(
                        "regression_guards.{guard_name} is fail and failure_or_gap_reasons must not include {unverified_reason}"
                    ));
                }
            }
            "missing" => {
                if !has_unverified_reason {
                    return Err(format!(
                        "regression_guards.{guard_name} is missing and failure_or_gap_reasons must include {unverified_reason}"
                    ));
                }
                if has_fail_reason {
                    return Err(format!(
                        "regression_guards.{guard_name} is missing and failure_or_gap_reasons must not include {fail_reason}"
                    ));
                }
            }
            _ => {}
        }
    }
    for reason in &reason_set {
        let known = ["memory", "correctness", "security"]
            .iter()
            .any(|guard_name| {
                reason == &format!("{guard_name}_regression")
                    || reason == &format!("{guard_name}_regression_unverified")
            });
        if !known {
            return Err(format!(
                "regression_guards.failure_or_gap_reasons contains unknown reason: {reason}"
            ));
        }
    }

    let evidence_links = record
        .get("evidence_links")
        .and_then(Value::as_object)
        .ok_or_else(|| "evidence_links must be an object".to_string())?;
    for field in &[
        "phase1_unit_and_fault_injection",
        "required_artifacts",
        "source_identity",
    ] {
        if !evidence_links.contains_key(*field) {
            return Err(format!("evidence_links missing {field}"));
        }
    }
    let required_artifacts = evidence_links
        .get("required_artifacts")
        .and_then(Value::as_object)
        .ok_or_else(|| "evidence_links.required_artifacts must be an object".to_string())?;
    let scenario_runner_path = require_non_empty_string_field(
        required_artifacts,
        "evidence_links.required_artifacts",
        "scenario_runner",
    )?;
    let stratification_path = require_non_empty_string_field(
        required_artifacts,
        "evidence_links.required_artifacts",
        "stratification",
    )?;
    let baseline_confidence_path = require_non_empty_string_field(
        required_artifacts,
        "evidence_links.required_artifacts",
        "baseline_variance_confidence",
    )?;

    let source_identity = evidence_links
        .get("source_identity")
        .and_then(Value::as_object)
        .ok_or_else(|| "evidence_links.source_identity must be an object".to_string())?;
    let source_identity_run_id = require_non_empty_string_field(
        source_identity,
        "evidence_links.source_identity",
        "run_id",
    )?;
    let source_identity_correlation_id = require_non_empty_string_field(
        source_identity,
        "evidence_links.source_identity",
        "correlation_id",
    )?;
    if source_identity_run_id != run_id {
        return Err(format!(
            "evidence_links.source_identity.run_id ({source_identity_run_id}) must match run_id ({run_id})"
        ));
    }
    if source_identity_correlation_id != correlation_id {
        return Err(format!(
            "evidence_links.source_identity.correlation_id ({source_identity_correlation_id}) must match correlation_id ({correlation_id})"
        ));
    }

    let consumption_contract = record
        .get("consumption_contract")
        .and_then(Value::as_object)
        .ok_or_else(|| "consumption_contract must be an object".to_string())?;
    if !consumption_contract.contains_key("artifact_ready_for_phase5") {
        return Err("consumption_contract missing artifact_ready_for_phase5".to_string());
    }
    let artifact_ready_for_phase5 = consumption_contract
        .get("artifact_ready_for_phase5")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            "consumption_contract.artifact_ready_for_phase5 must be a boolean".to_string()
        })?;
    let expected_artifact_ready_for_phase5 = primary_status == "pass"
        && missing_cells_count == 0
        && complete_cells == required_cell_count
        && missing_swarm_cells_count == 0
        && complete_swarm_cells == required_cell_count
        && memory_guard_status == "pass"
        && correctness_guard_status == "pass"
        && security_guard_status == "pass";
    if artifact_ready_for_phase5 != expected_artifact_ready_for_phase5 {
        return Err(format!(
            "consumption_contract.artifact_ready_for_phase5 ({artifact_ready_for_phase5}) must equal expected deterministic value ({expected_artifact_ready_for_phase5}) from primary_outcomes.status={primary_status}, stage_summary(cells_with_complete_stage_breakdown={complete_cells}, cells_missing_stage_breakdown={missing_cells_count}, required_cell_count={required_cell_count}), swarm_summary(cells_with_complete_swarm_metrics={complete_swarm_cells}, cells_missing_swarm_metrics={missing_swarm_cells_count}), regression_guards(memory={memory_guard_status}, correctness={correctness_guard_status}, security={security_guard_status})"
        ));
    }
    let downstream_beads = consumption_contract
        .get("downstream_beads")
        .and_then(Value::as_array)
        .ok_or_else(|| "consumption_contract.downstream_beads must be an array".to_string())?;
    let mut downstream_bead_set = HashSet::new();
    for (index, bead) in downstream_beads.iter().enumerate() {
        let bead_id = bead
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!("consumption_contract.downstream_beads[{index}] must be a non-empty string")
            })?;
        downstream_bead_set.insert(bead_id.to_owned());
    }
    for required_bead in ["bd-3ar8v.6.1", "bd-3ar8v.6.2"] {
        if !downstream_bead_set.contains(required_bead) {
            return Err(format!(
                "consumption_contract.downstream_beads missing required phase-5 consumer bead {required_bead}"
            ));
        }
    }
    let downstream_consumers = consumption_contract
        .get("downstream_consumers")
        .and_then(Value::as_object)
        .ok_or_else(|| "consumption_contract.downstream_consumers must be an object".to_string())?;
    for (consumer_name, expected_bead_id, expected_selector) in [
        (
            "opportunity_matrix",
            "bd-3ar8v.6.1",
            "weighted_bottleneck_attribution.global_ranking",
        ),
        (
            "parameter_sweeps",
            "bd-3ar8v.6.2",
            "weighted_bottleneck_attribution.per_scale",
        ),
    ] {
        let consumer = downstream_consumers
            .get(consumer_name)
            .and_then(Value::as_object)
            .ok_or_else(|| {
                format!(
                    "consumption_contract.downstream_consumers.{consumer_name} must be an object"
                )
            })?;
        let field_path = format!("consumption_contract.downstream_consumers.{consumer_name}");
        let observed_bead_id = require_non_empty_string_field(consumer, &field_path, "bead_id")?;
        if observed_bead_id != expected_bead_id {
            return Err(format!(
                "{field_path}.bead_id must be {expected_bead_id}, got: {observed_bead_id}"
            ));
        }
        let observed_selector = require_non_empty_string_field(consumer, &field_path, "selector")?;
        if observed_selector != expected_selector {
            return Err(format!(
                "{field_path}.selector must be {expected_selector}, got: {observed_selector}"
            ));
        }
        let observed_source_artifact =
            require_non_empty_string_field(consumer, &field_path, "source_artifact")?;
        if observed_source_artifact != "phase1_matrix_validation" {
            return Err(format!(
                "{field_path}.source_artifact must be phase1_matrix_validation, got: {observed_source_artifact}"
            ));
        }
    }

    let lineage = record
        .get("lineage")
        .and_then(Value::as_object)
        .ok_or_else(|| "lineage must be an object".to_string())?;
    let run_id_lineage = lineage
        .get("run_id_lineage")
        .and_then(Value::as_array)
        .ok_or_else(|| "lineage.run_id_lineage must be an array".to_string())?;
    if run_id_lineage.len() < 2 {
        return Err("lineage.run_id_lineage must include run_id + correlation_id".to_string());
    }
    let lineage_run_id = run_id_lineage
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| "lineage.run_id_lineage[0] must be run_id string".to_string())?;
    if lineage_run_id != run_id {
        return Err(format!(
            "lineage.run_id_lineage[0] ({lineage_run_id}) must match run_id ({run_id})"
        ));
    }
    let lineage_correlation_id = run_id_lineage
        .get(1)
        .and_then(Value::as_str)
        .ok_or_else(|| "lineage.run_id_lineage[1] must be correlation_id string".to_string())?;
    if lineage_correlation_id != correlation_id {
        return Err(format!(
            "lineage.run_id_lineage[1] ({lineage_correlation_id}) must match correlation_id ({correlation_id})"
        ));
    }
    let _ = require_non_empty_string_field(lineage, "lineage", "source_manifest_path")?;
    let lineage_scenario_runner =
        require_non_empty_string_field(lineage, "lineage", "source_scenario_runner_path")?;
    let lineage_stratification =
        require_non_empty_string_field(lineage, "lineage", "source_stratification_path")?;
    let lineage_baseline_confidence =
        require_non_empty_string_field(lineage, "lineage", "source_baseline_confidence_path")?;
    let _ = require_non_empty_string_field(lineage, "lineage", "source_perf_sli_contract_path")?;
    if lineage_scenario_runner != scenario_runner_path {
        return Err(format!(
            "lineage.source_scenario_runner_path ({lineage_scenario_runner}) must match evidence_links.required_artifacts.scenario_runner ({scenario_runner_path})"
        ));
    }
    if lineage_stratification != stratification_path {
        return Err(format!(
            "lineage.source_stratification_path ({lineage_stratification}) must match evidence_links.required_artifacts.stratification ({stratification_path})"
        ));
    }
    if lineage_baseline_confidence != baseline_confidence_path {
        return Err(format!(
            "lineage.source_baseline_confidence_path ({lineage_baseline_confidence}) must match evidence_links.required_artifacts.baseline_variance_confidence ({baseline_confidence_path})"
        ));
    }

    Ok(())
}

fn require_positive_metric(
    obj: &serde_json::Map<String, Value>,
    context: &str,
    field: &str,
) -> Result<f64, String> {
    let value = obj
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{context}.{field} must be a positive finite number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!(
            "{context}.{field} must be a positive finite number, got: {value}"
        ));
    }
    Ok(value)
}

fn require_non_negative_metric(
    obj: &serde_json::Map<String, Value>,
    context: &str,
    field: &str,
) -> Result<f64, String> {
    let value = obj
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{context}.{field} must be a non-negative finite number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!(
            "{context}.{field} must be a non-negative finite number, got: {value}"
        ));
    }
    Ok(value)
}

fn require_nullable_positive_metric(
    obj: &serde_json::Map<String, Value>,
    context: &str,
    field: &str,
) -> Result<Option<f64>, String> {
    let Some(raw_value) = obj.get(field) else {
        return Err(format!(
            "{context}.{field} must be null or a positive finite number"
        ));
    };
    if raw_value.is_null() {
        return Ok(None);
    }
    let value = raw_value
        .as_f64()
        .ok_or_else(|| format!("{context}.{field} must be null or a positive finite number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!(
            "{context}.{field} must be null or a positive finite number, got: {value}"
        ));
    }
    Ok(Some(value))
}

fn require_nullable_percentage_metric(
    obj: &serde_json::Map<String, Value>,
    context: &str,
    field: &str,
) -> Result<Option<f64>, String> {
    let Some(raw_value) = obj.get(field) else {
        return Err(format!(
            "{context}.{field} must be null or a finite percentage in [0, 100]"
        ));
    };
    if raw_value.is_null() {
        return Ok(None);
    }
    let value = raw_value.as_f64().ok_or_else(|| {
        format!("{context}.{field} must be null or a finite percentage in [0, 100]")
    })?;
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(format!(
            "{context}.{field} must be null or a finite percentage in [0, 100], got: {value}"
        ));
    }
    Ok(Some(value))
}

fn require_non_empty_string_field(
    obj: &serde_json::Map<String, Value>,
    context: &str,
    field: &str,
) -> Result<String, String> {
    let value = obj
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context}.{field} must be a non-empty string"))?;
    if value.trim().is_empty() {
        return Err(format!("{context}.{field} must be a non-empty string"));
    }
    Ok(value.to_string())
}

fn swarm_metrics_fixture(
    total_ms: f64,
    open_ms: f64,
    append_ms: f64,
    save_ms: f64,
    index_ms: f64,
) -> Value {
    json!({
        "latency_quantiles_ms": {
            "p50": total_ms,
            "p95": total_ms * 1.15,
            "p99": total_ms * 1.35,
            "p999": total_ms * 1.75,
        },
        "queue_depth": {
            "p50": 1,
            "p95": 2,
            "p99": 3,
            "p999": 4,
            "max": 4,
        },
        "resource_usage": {
            "rss_mb": 64,
            "cpu_pct": 0.0,
        },
        "component_breakdown_ms": {
            "tool": 0.0,
            "provider": 0.0,
            "extension": 0.0,
            "session": total_ms,
        },
        "stage_breakdown_ms": {
            "open": open_ms,
            "append": append_ms,
            "save": save_ms,
            "index": index_ms,
        },
        "host_capacity": {
            "target_cpu_cores": 64,
            "observed_cpu_cores": 8,
            "mem_total_mb": 262_144,
        },
    })
}

fn generic_test_binary_provenance(root: &Path) -> (String, String) {
    let binary_path = root.join("target/perf/examples/bench_schema_fixture");
    fs::create_dir_all(binary_path.parent().expect("generic binary parent"))
        .expect("create generic binary parent");
    fs::write(&binary_path, b"generic-benchmark-test-binary")
        .expect("write generic benchmark binary");
    let binary_path = fs::canonicalize(binary_path).expect("canonicalize generic binary");
    let binary_sha256 = sha256_file(&binary_path).expect("hash generic benchmark binary");
    (binary_path.display().to_string(), binary_sha256)
}

fn regression_gate_protocol_fixture(root: &Path) -> Value {
    let (binary_path, binary_sha256) = generic_test_binary_provenance(root);
    let source_commit = "0123456789abcdef0123456789abcdef01234567";
    let compiled_features = ["sqlite-sessions", "tui"];
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
        compiled_features: &compiled_features,
        binary_path: &binary_path,
        binary_sha256: &binary_sha256,
        debug_assertions: false,
    });
    json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "tool_call",
        "extension": "hello",
        "iterations": 500,
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": PARTITION_MATCHED_STATE,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "eligible_for_regression_gate": true,
        "measurement_method": MEASUREMENT_METHOD_WALL_CLOCK,
        "measurement_boundary": "production_extension_manager",
        "measurement_contract_version": "production_extension_manager.v1",
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
        "build_profile": "perf",
        "executable_build_profile": "perf",
        "executable_profile_verified": true,
        "build_fingerprint_verified": true,
        "build_profile_verified": true,
        "build_fingerprint_contract": BUILD_FINGERPRINT_CONTRACT,
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "compiled_debug": "true",
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "debug_assertions": false,
        "source_commit": source_commit,
        "source_dirty": false,
        "compiled_features": compiled_features,
        "config_hash": config_hash,
        "correlation_id": "0123456789abcdef0123456789abcdef",
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "tool_call",
            "replay_input": { "iterations": 500 },
        },
    })
}

fn pijs_gate_workload_fixture(root: &Path, tool_calls_per_iteration: u64) -> Value {
    let total_calls = PIJS_GATE_ITERATIONS * tool_calls_per_iteration;
    let elapsed_us = total_calls * 99 / 2;
    let elapsed_us_f64 = elapsed_us as f64;
    let (binary_path, binary_sha256) = generic_test_binary_provenance(root);
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
        "schema": PIJS_GATE_SCHEMA,
        "timestamp": "2026-08-05T16:00:00Z",
        "run_id": "pijs-schema-test-run",
        "correlation_id": "pijs-schema-test-run",
        "source_commit": source_commit,
        "source_dirty": false,
        "tool": PIJS_GATE_TOOL,
        "scenario": PIJS_GATE_SCENARIO,
        "runtime_engine": PIJS_GATE_RUNTIME_ENGINE,
        "build_profile": PIJS_GATE_BUILD_PROFILE,
        "build_profile_verified": true,
        "build_fingerprint_contract": BUILD_FINGERPRINT_CONTRACT,
        "build_fingerprint_verified": true,
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "compiled_debug": "true",
        "compiled_features": CANONICAL_PIJS_PERF_FEATURES,
        "binary_path": binary_path,
        "executable_build_profile": "perf",
        "executable_profile_verified": true,
        "debug_assertions": false,
        "binary_sha256": binary_sha256,
        "config_hash": config_hash,
    })
    .as_object()
    .expect("PiJS provenance fixture object")
    .clone();
    record.extend(
        json!({
        "iterations": PIJS_GATE_ITERATIONS,
        "tool_calls_per_iteration": tool_calls_per_iteration,
        "total_calls": total_calls,
        "elapsed_ms": elapsed_us / 1_000,
        "elapsed_us": elapsed_us,
        "elapsed_us_f64": elapsed_us_f64,
        "per_call_us": elapsed_us / total_calls,
        "per_call_us_f64": 49.5,
        "calls_per_sec": total_calls * 1_000_000 / elapsed_us,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "eligible_for_regression_gate": true,
        "measurement_method": MEASUREMENT_METHOD_WALL_CLOCK,
        "measurement_boundary": PIJS_GATE_MEASUREMENT_BOUNDARY,
        "measurement_contract_version": PIJS_GATE_MEASUREMENT_CONTRACT_VERSION,
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
        "allocator_requested": "system",
        "allocator_request_source": "env",
        "allocator_effective": "system",
        "allocator_fallback_reason": null,
        })
        .as_object()
        .expect("PiJS measurement fixture object")
        .clone(),
    );
    Value::Object(record)
}

fn phase1_matrix_validation_golden_fixture() -> Value {
    json!({
        "schema": PHASE1_MATRIX_SCHEMA,
        "run_id": "20260216T010101Z",
        "correlation_id": "abc123def456",
        "matrix_requirements": {
            "required_partition_tags": ["matched-state", "realistic"],
            "required_session_message_sizes": [100_000],
            "required_cell_count": 2
        },
        "matrix_cells": [
            {
                "workload_partition": "matched-state",
                "session_messages": 100_000,
                "scenario_id": "matched-state/session_100000",
                "status": "pass",
                "missing_reasons": [],
                "stage_attribution": {
                    "open_ms": 48.0,
                    "append_ms": 36.0,
                    "save_ms": 22.0,
                    "index_ms": 11.0,
                    "total_stage_ms": 117.0
                },
                "swarm_metrics": swarm_metrics_fixture(117.0, 48.0, 36.0, 22.0, 11.0),
                "primary_e2e": {
                    "wall_clock_ms": 1200.0,
                    "rust_vs_node_ratio": 2.2,
                    "rust_vs_bun_ratio": 2.2
                },
                "microbench_context": {
                    "cold_load_ms": 18.0,
                    "per_call_us": 33.0
                },
                "lineage": {
                    "source_record_index": 2,
                    "source_artifacts": ["target/perf/scenario_runner.jsonl"]
                }
            },
            {
                "workload_partition": "realistic",
                "session_messages": 100_000,
                "scenario_id": "realistic/session_100000",
                "status": "pass",
                "missing_reasons": [],
                "stage_attribution": {
                    "open_ms": 44.0,
                    "append_ms": 32.0,
                    "save_ms": 19.0,
                    "index_ms": 10.0,
                    "total_stage_ms": 105.0
                },
                "swarm_metrics": swarm_metrics_fixture(105.0, 44.0, 32.0, 19.0, 10.0),
                "primary_e2e": {
                    "wall_clock_ms": 1200.0,
                    "rust_vs_node_ratio": 2.2,
                    "rust_vs_bun_ratio": 2.2
                },
                "microbench_context": {
                    "cold_load_ms": 18.0,
                    "per_call_us": 33.0
                },
                "lineage": {
                    "source_record_index": 7,
                    "source_artifacts": ["target/perf/scenario_runner.jsonl"]
                }
            }
        ],
        "stage_summary": {
            "required_stage_keys": ["open_ms", "append_ms", "save_ms", "index_ms"],
            "operation_stage_coverage": {
                "open_ms": 2,
                "append_ms": 2,
                "save_ms": 2,
                "index_ms": 2
            },
            "cells_with_complete_stage_breakdown": 2,
            "cells_missing_stage_breakdown": 0,
            "covered_cells": 2,
            "missing_cells": []
        },
        "swarm_summary": {
            "required_latency_quantiles": SWARM_LATENCY_QUANTILES,
            "required_queue_depth_quantiles": SWARM_QUEUE_DEPTH_QUANTILES,
            "required_resource_usage_keys": SWARM_RESOURCE_USAGE_KEYS,
            "required_component_breakdown_keys": SWARM_COMPONENT_BREAKDOWN_KEYS,
            "required_stage_breakdown_keys": SWARM_STAGE_BREAKDOWN_KEYS,
            "cells_with_complete_swarm_metrics": 2,
            "cells_missing_swarm_metrics": 0,
            "missing_cells": []
        },
        "weighted_bottleneck_attribution": {
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
                                "open_ms": 41.025_641_025_641_02,
                                "append_ms": 30.769_230_769_230_77,
                                "save_ms": 18.803_418_803_418_804,
                                "index_ms": 9.401_709_401_709_402
                            }
                        },
                        {
                            "workload_partition": "realistic",
                            "present": true,
                            "scenario_id": "realistic/session_100000",
                            "total_stage_ms": 105.0,
                            "stage_pct": {
                                "open_ms": 41.904_761_904_761_905,
                                "append_ms": 30.476_190_476_190_478,
                                "save_ms": 18.095_238_095_238_095,
                                "index_ms": 9.523_809_523_809_524
                            }
                        }
                    ]
                }
            ],
            "global_ranking": [
                {
                    "stage": "open_ms",
                    "weighted_stage_ms": 9_200_000.0,
                    "weighted_contribution_pct": 41.441_441_441_441_44,
                    "mean_share_pct": 41.465_201_465_201_46,
                    "ci95_lower_pct": 40.856_001_776_794_585,
                    "ci95_upper_pct": 42.074_401_153_608_335,
                    "sample_size": 2
                },
                {
                    "stage": "append_ms",
                    "weighted_stage_ms": 6_800_000.0,
                    "weighted_contribution_pct": 30.630_630_630_630_627,
                    "mean_share_pct": 30.622_710_622_710_624,
                    "ci95_lower_pct": 30.419_644_059_908_336,
                    "ci95_upper_pct": 30.825_777_185_512_916,
                    "sample_size": 2
                },
                {
                    "stage": "save_ms",
                    "weighted_stage_ms": 4_100_000.0,
                    "weighted_contribution_pct": 18.468_468_468_468_47,
                    "mean_share_pct": 18.449_328_449_328_455,
                    "ci95_lower_pct": 17.958_584_255_889_583,
                    "ci95_upper_pct": 18.940_072_642_767_323,
                    "sample_size": 2
                },
                {
                    "stage": "index_ms",
                    "weighted_stage_ms": 2_100_000.0,
                    "weighted_contribution_pct": 9.459_459_459_459_46,
                    "mean_share_pct": 9.462_759_462_759_463,
                    "ci95_lower_pct": 9.378_148_394_925_175,
                    "ci95_upper_pct": 9.547_370_530_593_751,
                    "sample_size": 2
                }
            ],
            "lineage": {
                "source_stream": "phase1_matrix_validation.matrix_cells",
                "source_cell_count": 2,
                "valid_cell_count": 2
            }
        },
        "primary_outcomes": {
            "status": "pass",
            "wall_clock_ms": 1200.0,
            "rust_vs_node_ratio": 2.2,
            "rust_vs_bun_ratio": 2.2,
            "ordering_policy": "primary_e2e_before_microbench"
        },
        "regression_guards": {
            "memory": "pass",
            "correctness": "pass",
            "security": "pass",
            "failure_or_gap_reasons": []
        },
        "evidence_links": {
            "phase1_unit_and_fault_injection": {
                "suite_logs": {},
                "fault_injection_script": "scripts/e2e/run_persistence_fault_injection.sh",
                "fault_injection_summary_path": null
            },
            "required_artifacts": {
                "scenario_runner": "target/perf/scenario_runner.jsonl",
                "stratification": "target/perf/extension_benchmark_stratification.json",
                "baseline_variance_confidence": "target/perf/baseline_variance_confidence.json"
            },
            "source_identity": {
                "run_id": "20260216T010101Z",
                "correlation_id": "abc123def456"
            }
        },
        "consumption_contract": {
            "downstream_beads": ["bd-3ar8v.2.12", "bd-3ar8v.6.1", "bd-3ar8v.6.2"],
            "downstream_consumers": {
                "opportunity_matrix": {
                    "bead_id": "bd-3ar8v.6.1",
                    "selector": "weighted_bottleneck_attribution.global_ranking",
                    "source_artifact": "phase1_matrix_validation"
                },
                "parameter_sweeps": {
                    "bead_id": "bd-3ar8v.6.2",
                    "selector": "weighted_bottleneck_attribution.per_scale",
                    "source_artifact": "phase1_matrix_validation"
                }
            },
            "artifact_ready_for_phase5": true,
            "fail_closed_conditions": ["missing_stage_metrics", "missing_swarm_metrics"]
        },
        "lineage": {
            "run_id_lineage": ["20260216T010101Z", "abc123def456"],
            "source_manifest_path": "target/perf/runs/20260216T010101Z/manifest.json",
            "source_scenario_runner_path": "target/perf/scenario_runner.jsonl",
            "source_stratification_path": "target/perf/extension_benchmark_stratification.json",
            "source_baseline_confidence_path": "target/perf/baseline_variance_confidence.json",
            "source_perf_sli_contract_path": "docs/perf_sli_matrix.json"
        }
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn schema_registry_is_complete() {
    assert!(
        SCHEMAS.len() >= 5,
        "should have at least 5 registered schemas"
    );
    for (name, desc) in SCHEMAS {
        assert!(!name.is_empty(), "schema name must not be empty");
        assert!(!desc.is_empty(), "schema description must not be empty");
        assert!(
            name.starts_with("pi."),
            "schema names should start with 'pi.': {name}"
        );
    }
    eprintln!("[schema] {} schemas registered", SCHEMAS.len());
}

#[test]
fn resource_governor_admission_schema_accepts_live_decision_payload() {
    let budgets = pi::resource_governor::HostResourceBudgets::fixed(10.0, 1_000, 100, 100, 1_000);
    let governor = pi::resource_governor::ResourceGovernor::with_budgets(budgets);
    let request = pi::resource_governor::ResourceRequest::new(
        pi::resource_governor::ResourceOperationKind::Tool,
        "read",
    )
    .with_queue_depth(4)
    .with_estimated_tool_output_bytes(900);
    let sample = pi::resource_governor::HostResourceSample {
        load_avg_1m: Some(2.0),
        rss_bytes: Some(200),
        process_count: Some(20),
        fd_count: Some(20),
    };
    let decision = governor.admit_sample(&request, sample);
    let telemetry = decision.telemetry(&request);

    validate_resource_governor_admission_record(&telemetry)
        .expect("resource governor admission telemetry should validate");
    assert_eq!(
        telemetry.get("schema").and_then(Value::as_str),
        Some(RESOURCE_GOVERNOR_ADMISSION_SCHEMA)
    );
    assert_eq!(
        telemetry
            .get("decision")
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str),
        Some("backpressure")
    );
}

#[test]
fn env_fingerprint_fields_documented() {
    assert!(
        ENV_FINGERPRINT_FIELDS.len() >= 7,
        "should document at least 7 env fingerprint fields"
    );
    for (name, desc) in ENV_FINGERPRINT_FIELDS {
        assert!(!name.is_empty());
        assert!(!desc.is_empty());
    }
    eprintln!(
        "[schema] {} env fingerprint fields documented",
        ENV_FINGERPRINT_FIELDS.len()
    );
}

#[test]
fn protocol_contract_covers_realistic_and_matched_state_partitions() {
    let contract = canonical_protocol_contract();
    assert_eq!(
        contract.get("schema").and_then(Value::as_str),
        Some(BENCH_PROTOCOL_SCHEMA)
    );
    assert_eq!(
        contract.get("version").and_then(Value::as_str),
        Some(BENCH_PROTOCOL_VERSION)
    );

    let partitions: Vec<&str> = contract["partition_tags"]
        .as_array()
        .expect("partition_tags array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(partitions.contains(&PARTITION_MATCHED_STATE));
    assert!(partitions.contains(&PARTITION_REALISTIC));
}

#[test]
fn protocol_contract_defines_partition_weighting_and_guardrails() {
    let contract = canonical_protocol_contract();

    let matched_weight = contract["partition_weighting"][PARTITION_MATCHED_STATE]
        .as_f64()
        .expect("matched-state weight");
    let realistic_weight = contract["partition_weighting"][PARTITION_REALISTIC]
        .as_f64()
        .expect("realistic weight");
    let weights_sum_to = contract["partition_weighting"]["weights_sum_to"]
        .as_f64()
        .expect("weights_sum_to");
    assert!((weights_sum_to - 1.0).abs() < f64::EPSILON);
    assert!(
        ((matched_weight + realistic_weight) - weights_sum_to).abs() < f64::EPSILON,
        "partition weights must sum to 1.0"
    );
    assert!(
        realistic_weight > matched_weight,
        "realistic partition should carry higher release-facing weight"
    );

    assert_eq!(
        contract["partition_interpretation"]["primary_partition"].as_str(),
        Some(PARTITION_REALISTIC)
    );
    assert_eq!(
        contract["partition_interpretation"]["secondary_partition"].as_str(),
        Some(PARTITION_MATCHED_STATE)
    );
    assert_eq!(
        contract["partition_interpretation"]["forbid_single_partition_conclusion"].as_bool(),
        Some(true)
    );

    let required_partitions =
        contract["partition_interpretation"]["global_claim_requires_partitions"]
            .as_array()
            .expect("global_claim_requires_partitions array");
    let required: HashSet<&str> = required_partitions
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        required.contains(PARTITION_MATCHED_STATE) && required.contains(PARTITION_REALISTIC),
        "global claim rules must require both partitions"
    );
}

#[test]
fn protocol_contract_defines_swarm_scale_measurement_requirements() {
    let contract = canonical_protocol_contract();
    let requirements = contract["swarm_scale_requirements"]
        .as_object()
        .expect("swarm_scale_requirements object");

    assert_eq!(
        requirements["target_cpu_cores"].as_u64(),
        Some(64),
        "swarm harness target must be explicit"
    );
    assert_eq!(
        requirements["fail_closed_on_missing_measurements"].as_bool(),
        Some(true),
        "missing swarm measurements must fail closed"
    );
    for (field, expected) in [
        ("required_latency_quantiles", SWARM_LATENCY_QUANTILES),
        (
            "required_queue_depth_quantiles",
            SWARM_QUEUE_DEPTH_QUANTILES,
        ),
        ("required_resource_usage_keys", SWARM_RESOURCE_USAGE_KEYS),
        (
            "required_component_breakdown_keys",
            SWARM_COMPONENT_BREAKDOWN_KEYS,
        ),
    ] {
        let observed = requirements[field]
            .as_array()
            .expect("requirement array")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(observed, expected, "unexpected swarm requirement {field}");
    }

    let run_commands = requirements["documented_run_commands"]
        .as_object()
        .expect("documented run commands object");
    assert!(
        run_commands
            .get("rch_required")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains("--require-rch")),
        "swarm harness must document an RCH-required run command"
    );
}

#[test]
fn protocol_contract_contains_realistic_size_matrix() {
    let contract = canonical_protocol_contract();
    let sizes: Vec<u64> = contract["realistic_session_sizes"]
        .as_array()
        .expect("realistic_session_sizes array")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert_eq!(
        sizes, REALISTIC_SESSION_SIZES,
        "realistic session sizes must match canonical 100k/200k/500k/1M/5M matrix"
    );

    let replay_inputs = contract["realistic_replay_inputs"]
        .as_array()
        .expect("realistic_replay_inputs array");
    assert_eq!(
        replay_inputs.len(),
        REALISTIC_SESSION_SIZES.len(),
        "realistic replay inputs must cover each canonical size"
    );
    for expected_size in REALISTIC_SESSION_SIZES {
        assert!(
            replay_inputs.iter().any(|entry| {
                entry.get("session_messages").and_then(Value::as_u64) == Some(*expected_size)
            }),
            "missing realistic replay input for size {expected_size}"
        );
    }
}

#[test]
fn protocol_contract_contains_matched_state_replay_inputs() {
    let contract = canonical_protocol_contract();
    let scenarios = contract["matched_state_scenarios"]
        .as_array()
        .expect("matched_state_scenarios array");
    for expected in &["cold_start", "warm_start", "tool_call", "event_dispatch"] {
        let entry = scenarios
            .iter()
            .find(|scenario| scenario.get("scenario").and_then(Value::as_str) == Some(*expected));
        assert!(entry.is_some(), "missing matched-state scenario {expected}");
        assert!(
            entry
                .and_then(|v| v.get("replay_input"))
                .is_some_and(Value::is_object),
            "matched-state scenario {expected} must include replay_input object"
        );
    }
}

#[test]
fn protocol_contract_labels_evidence_and_confidence() {
    let contract = canonical_protocol_contract();
    let evidence_classes: Vec<&str> = contract["evidence_labels"]["evidence_class"]
        .as_array()
        .expect("evidence_class labels")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        evidence_classes,
        vec![EVIDENCE_CLASS_MEASURED, EVIDENCE_CLASS_INFERRED]
    );

    let confidence_labels: Vec<&str> = contract["evidence_labels"]["confidence"]
        .as_array()
        .expect("confidence labels")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        confidence_labels,
        vec![CONFIDENCE_HIGH, CONFIDENCE_MEDIUM, CONFIDENCE_LOW]
    );
}

#[test]
fn budget_summary_contract_defines_comparison_and_digest_semantics() {
    let contract = canonical_budget_summary_contract();
    assert_eq!(
        contract["schema"].as_str(),
        Some(PERF_BUDGET_SUMMARY_SCHEMA)
    );
    assert_eq!(
        contract["budget_definition_required_fields"],
        json!(PERF_BUDGET_DEFINITION_FIELDS)
    );
    assert_eq!(
        contract["comparison_values"],
        json!(PERF_BUDGET_COMPARISON_VALUES)
    );
    assert_eq!(
        contract["comparison_semantics"]["maximum"].as_str(),
        Some("actual <= threshold")
    );
    assert_eq!(
        contract["comparison_semantics"]["minimum"].as_str(),
        Some("actual >= threshold")
    );
    assert_eq!(
        contract["inventory_digest"]["threshold_representation"].as_str(),
        Some("exactly_six_decimal_places")
    );
    assert_eq!(
        contract["inventory_digest"]["canonical_v0_2_0_sha256"].as_str(),
        Some(PERF_BUDGET_V0_2_0_INVENTORY_SHA256)
    );
    assert_eq!(
        contract["claim_readiness"]["scope"].as_str(),
        Some("all_declared_budgets")
    );
    assert_eq!(
        contract["claim_readiness"]["blocked_lineage_evaluation"].as_str(),
        Some("canonical_all_no_data_sentinel_without_artifact_discovery")
    );
    assert_eq!(
        contract["claim_readiness"]["blocking_reason_codes"],
        json!(PERF_CLAIM_READINESS_BLOCKER_CODES)
    );
}

#[test]
fn protocol_contract_defines_regression_gate_admission() {
    let contract = canonical_protocol_contract();
    let admission = contract["regression_gate_admission"]
        .as_object()
        .expect("regression_gate_admission object");

    assert_eq!(
        admission["scope"].as_str(),
        Some(REGRESSION_GATE_GENERIC_SCOPE),
        "generic admission metadata must not imply a workload-specific gate"
    );

    require_string_array_eq(
        admission,
        "required_record_fields",
        REGRESSION_GATE_REQUIRED_RECORD_FIELDS,
        "regression_gate_admission",
    )
    .expect("canonical regression-gate record fields");
    require_string_array_eq(
        admission,
        "load_scenario_required_fields",
        REGRESSION_GATE_LOAD_REQUIRED_RECORD_FIELDS,
        "regression_gate_admission",
    )
    .expect("canonical regression-gate load fields");
    require_string_array_eq(
        admission,
        "allowed_measurement_methods",
        &[MEASUREMENT_METHOD_WALL_CLOCK, MEASUREMENT_METHOD_SYNTHETIC],
        "regression_gate_admission",
    )
    .expect("canonical measurement methods");
    require_string_array_eq(
        admission,
        "allowed_measurement_boundaries",
        REGRESSION_GATE_ALLOWED_BOUNDARIES,
        "regression_gate_admission",
    )
    .expect("canonical measurement boundaries");
    require_string_array_eq(
        admission,
        "eligible_measurement_boundaries",
        REGRESSION_GATE_ELIGIBLE_BOUNDARIES,
        "regression_gate_admission",
    )
    .expect("canonical eligible measurement boundaries");
    require_string_array_eq(
        admission,
        "allowed_disk_cache_policies",
        REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES,
        "regression_gate_admission",
    )
    .expect("canonical disk-cache policies");
    require_string_array_eq(
        admission,
        "allowed_host_page_cache_policies",
        REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES,
        "regression_gate_admission",
    )
    .expect("canonical host-page-cache policies");
    require_string_array_eq(
        admission,
        "required_eligible_provenance_fields",
        REGRESSION_GATE_REQUIRED_ELIGIBLE_PROVENANCE_FIELDS,
        "regression_gate_admission",
    )
    .expect("canonical eligible provenance fields");
    require_string_array_eq(
        admission,
        "positive_sample_count_fields",
        REGRESSION_GATE_POSITIVE_SAMPLE_FIELDS,
        "regression_gate_admission",
    )
    .expect("canonical positive sample fields");

    assert_eq!(
        admission["eligible_evidence_class"].as_str(),
        Some(EVIDENCE_CLASS_MEASURED)
    );
    assert_eq!(
        admission["eligible_confidence"].as_str(),
        Some(CONFIDENCE_HIGH)
    );
    assert_eq!(
        admission["eligible_measurement_method"].as_str(),
        Some(MEASUREMENT_METHOD_WALL_CLOCK)
    );
    assert_eq!(
        admission["required_eligible_host_page_cache_policy"].as_str(),
        Some("not_applicable_measured_region")
    );
    assert_eq!(
        admission["require_positive_sample_count"].as_bool(),
        Some(true)
    );
    assert_eq!(
        admission["uncontrolled_host_page_cache_eligible"].as_bool(),
        Some(false)
    );
}

#[test]
fn protocol_contract_defines_separate_pijs_regression_gate_admission() {
    let contract = canonical_protocol_contract();
    let admission = contract["pijs_regression_gate_admission"]
        .as_object()
        .expect("pijs_regression_gate_admission object");

    require_string_array_eq(
        admission,
        "required_record_fields",
        PIJS_GATE_REQUIRED_RECORD_FIELDS,
        "pijs_regression_gate_admission",
    )
    .expect("canonical PiJS regression-gate record fields");

    for (field, expected) in [
        ("scope", PIJS_GATE_SCOPE),
        ("inherits", "regression_gate_admission"),
        ("required_schema", PIJS_GATE_SCHEMA),
        ("required_tool", PIJS_GATE_TOOL),
        ("required_scenario", PIJS_GATE_SCENARIO),
        ("required_runtime_engine", PIJS_GATE_RUNTIME_ENGINE),
        ("required_build_profile", PIJS_GATE_BUILD_PROFILE),
        (
            "required_measurement_boundary",
            PIJS_GATE_MEASUREMENT_BOUNDARY,
        ),
        (
            "required_measurement_contract_version",
            PIJS_GATE_MEASUREMENT_CONTRACT_VERSION,
        ),
    ] {
        assert_eq!(
            admission[field].as_str(),
            Some(expected),
            "unexpected PiJS admission field {field}"
        );
    }
    assert_eq!(
        admission["required_iterations"].as_u64(),
        Some(PIJS_GATE_ITERATIONS)
    );
    assert_eq!(
        admission["required_build_profile_verified"].as_bool(),
        Some(true)
    );
    assert_eq!(
        admission["required_tool_calls_per_iteration"],
        json!(PIJS_GATE_TOOL_CALL_COUNTS)
    );
}

#[test]
fn protocol_contract_exposes_user_perceived_sli_matrix() {
    let contract = canonical_protocol_contract();
    let catalog = contract["user_perceived_sli_catalog"]
        .as_array()
        .expect("user_perceived_sli_catalog array");
    assert_eq!(
        catalog.len(),
        USER_PERCEIVED_SLI_IDS.len(),
        "expected fixed user-perceived SLI catalog cardinality"
    );

    let catalog_ids = catalog
        .iter()
        .map(|entry| {
            entry
                .get("sli_id")
                .and_then(Value::as_str)
                .expect("catalog entries must expose sli_id")
                .to_string()
        })
        .collect::<HashSet<_>>();
    for expected in USER_PERCEIVED_SLI_IDS {
        assert!(
            catalog_ids.contains(*expected),
            "missing canonical SLI id {expected}"
        );
    }

    let matrix = contract["scenario_sli_matrix"]
        .as_array()
        .expect("scenario_sli_matrix array");

    let mut expected_scenarios = ["cold_start", "warm_start", "tool_call", "event_dispatch"]
        .into_iter()
        .map(std::string::ToString::to_string)
        .collect::<HashSet<_>>();
    expected_scenarios.extend(
        REALISTIC_SESSION_SIZES
            .iter()
            .map(|messages| format!("realistic/session_{messages}")),
    );

    assert_eq!(
        matrix.len(),
        expected_scenarios.len(),
        "scenario_sli_matrix must cover every canonical benchmark scenario"
    );

    let mut seen_scenarios = HashSet::new();
    for row in matrix {
        let scenario_id = row
            .get("scenario_id")
            .and_then(Value::as_str)
            .expect("matrix row must contain scenario_id");
        seen_scenarios.insert(scenario_id.to_string());

        let sli_ids = row
            .get("sli_ids")
            .and_then(Value::as_array)
            .expect("matrix row must contain sli_ids array");
        assert!(
            !sli_ids.is_empty(),
            "matrix row {scenario_id} has empty sli_ids"
        );
        for sli_id in sli_ids {
            let sli_id = sli_id
                .as_str()
                .expect("sli_ids values must be strings in scenario_sli_matrix");
            assert!(
                catalog_ids.contains(sli_id),
                "scenario {scenario_id} references unknown SLI {sli_id}"
            );
        }

        let phase_beads = row
            .get("phase_validation_beads")
            .and_then(Value::as_array)
            .expect("matrix row must contain phase_validation_beads");
        assert!(
            phase_beads.iter().all(|id| {
                id.as_str()
                    .is_some_and(|bead_id| bead_id.starts_with("bd-3ar8v."))
            }),
            "matrix row {scenario_id} has invalid phase_validation_beads"
        );
    }

    assert_eq!(
        seen_scenarios, expected_scenarios,
        "scenario_sli_matrix scenarios must exactly match protocol scenarios"
    );
}

#[test]
fn protocol_record_validator_accepts_golden_fixture() {
    let golden = json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "tool_call",
        "extension": "hello",
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": PARTITION_REALISTIC,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "correlation_id": "0123456789abcdef0123456789abcdef",
        "swarm_metrics": swarm_metrics_fixture(117.0, 48.0, 36.0, 22.0, 11.0),
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "realistic/session_100000",
            "replay_input": {
                "session_messages": 100_000,
                "fixture": "tests/artifacts/perf/session_100000.jsonl",
            },
        },
    });
    assert!(
        validate_protocol_record(&golden).is_ok(),
        "golden protocol fixture should pass validation"
    );
}

#[test]
fn regression_gate_admission_accepts_measured_production_evidence() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let fixture = regression_gate_protocol_fixture(tmp.path());
    if let Err(err) = validate_protocol_record(&fixture) {
        panic!("eligible production evidence should pass validation: {err}");
    }
}

#[test]
fn regression_gate_admission_rejects_unsupported_eligibility_claims() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cases = [
        (
            "evidence_class",
            json!(EVIDENCE_CLASS_INFERRED),
            "must be measured",
        ),
        (
            "confidence",
            json!(CONFIDENCE_MEDIUM),
            "must have high confidence",
        ),
        (
            "measurement_method",
            json!(MEASUREMENT_METHOD_SYNTHETIC),
            "must use wall_clock_observation",
        ),
        (
            "measurement_boundary",
            json!("test_harness"),
            "invalid regression-gate measurement_boundary token",
        ),
        (
            "iterations",
            json!(0),
            "iterations must be a positive integer",
        ),
        (
            "total_calls",
            json!(0),
            "total_calls must be a positive integer",
        ),
    ];

    for (field, value, expected_error) in cases {
        let mut fixture = regression_gate_protocol_fixture(tmp.path());
        fixture[field] = value;
        let err = match validate_protocol_record(&fixture) {
            Ok(()) => panic!("eligible record with invalid {field} must fail"),
            Err(err) => err,
        };
        assert!(
            err.contains(expected_error),
            "invalid {field} returned unexpected error: {err}"
        );
    }
}

#[test]
fn regression_gate_admission_recomputes_generic_build_provenance() {
    let cases = [
        (
            "build_profile_verified",
            json!(false),
            "build_profile_verified must equal true",
        ),
        (
            "compiled_opt_level",
            json!("z"),
            "compiled_opt_level must equal \"3\"",
        ),
        (
            "binary_sha256",
            json!("0".repeat(64)),
            "binary_sha256 does not match binary_path",
        ),
        ("source_dirty", json!(true), "source_dirty must equal false"),
        (
            "config_hash",
            json!("0".repeat(64)),
            "config_hash does not match asserted provenance",
        ),
    ];
    for (field, value, expected_error) in cases {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut fixture = regression_gate_protocol_fixture(tmp.path());
        fixture[field] = value;
        let err = validate_protocol_record(&fixture)
            .expect_err("forged generic provenance must fail closed");
        assert!(err.contains(expected_error), "unexpected error: {err}");
    }
}

#[test]
fn regression_gate_admission_requires_controlled_load_cache_policy() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let mut fixture = regression_gate_protocol_fixture(tmp.path());
    fixture["scenario"] = json!("cold_start");
    fixture["scenario_metadata"]["scenario_id"] = json!("cold_start");
    fixture["runs"] = json!(5);
    fixture
        .as_object_mut()
        .expect("regression fixture object")
        .remove("host_page_cache_policy");

    let missing_policy_err = validate_protocol_record(&fixture)
        .expect_err("eligible load record without cache policy must fail");
    assert!(
        missing_policy_err.contains("cache-policy fields"),
        "unexpected missing cache-policy error: {missing_policy_err}"
    );

    fixture["disk_cache_policy"] = json!("disabled");
    fixture["host_page_cache_policy"] = json!(HOST_PAGE_CACHE_UNCONTROLLED);
    let uncontrolled_err = validate_protocol_record(&fixture)
        .expect_err("uncontrolled host page cache must be gate-ineligible");
    assert!(
        uncontrolled_err.contains("requires host_page_cache_policy"),
        "unexpected uncontrolled-cache error: {uncontrolled_err}"
    );

    fixture["eligible_for_regression_gate"] = json!(false);
    validate_protocol_record(&fixture)
        .expect("explicitly ineligible load evidence may document uncontrolled host cache");
}

#[test]
fn regression_gate_admission_allows_explicitly_ineligible_diagnostics() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let mut fixture = regression_gate_protocol_fixture(tmp.path());
    fixture["evidence_class"] = json!(EVIDENCE_CLASS_INFERRED);
    fixture["confidence"] = json!(CONFIDENCE_LOW);
    fixture["eligible_for_regression_gate"] = json!(false);
    fixture["measurement_method"] = json!(MEASUREMENT_METHOD_SYNTHETIC);
    fixture["measurement_boundary"] = json!("synthetic_seed_generation");
    fixture["measurement_contract_version"] = json!("synthetic_seed_generation.v1");
    fixture["iterations"] = json!(0);

    validate_protocol_record(&fixture)
        .expect("explicitly ineligible diagnostics should remain valid evidence records");
}

#[test]
fn generic_workload_record_validator_applies_generic_regression_gate_admission() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let (binary_path, binary_sha256) = generic_test_binary_provenance(tmp.path());
    let source_commit = "0123456789abcdef0123456789abcdef01234567";
    let compiled_features = ["sqlite-sessions", "tui"];
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
        compiled_features: &compiled_features,
        binary_path: &binary_path,
        binary_sha256: &binary_sha256,
        debug_assertions: false,
    });
    let mut fixture = json!({
        "scenario": "tool_call_roundtrip",
        "iterations": 200,
        "tool_calls_per_iteration": 1,
        "total_calls": 200,
        "elapsed_ms": 10,
        "per_call_us": 50,
        "calls_per_sec": 20_000,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "eligible_for_regression_gate": true,
        "measurement_method": MEASUREMENT_METHOD_WALL_CLOCK,
        "measurement_boundary": "production_extension_manager",
        "measurement_contract_version": "production_extension_manager.v1",
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
        "build_profile": "perf",
        "executable_build_profile": "perf",
        "executable_profile_verified": true,
        "build_fingerprint_verified": true,
        "build_profile_verified": true,
        "build_fingerprint_contract": BUILD_FINGERPRINT_CONTRACT,
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "compiled_debug": "true",
        "compiled_features": compiled_features,
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "debug_assertions": false,
        "source_commit": source_commit,
        "source_dirty": false,
        "config_hash": config_hash,
    });
    validate_workload_record(&fixture)
        .expect("generic eligible production workload should be valid without PiJS-only fields");

    fixture["schema"] = json!(PIJS_GATE_SCHEMA);
    fixture["tool"] = json!("another_workload");
    validate_workload_record(&fixture).expect(
        "the shared workload schema must not impose PiJS-only rules on another workload tool",
    );

    let mut missing_disk_policy = fixture.clone();
    missing_disk_policy
        .as_object_mut()
        .expect("workload fixture object")
        .remove("disk_cache_policy");
    let err = validate_workload_record(&missing_disk_policy)
        .expect_err("workload admission must require disk-cache policy metadata");
    assert!(
        err.contains("disk_cache_policy"),
        "unexpected missing disk-cache policy error: {err}"
    );

    let mut null_disk_policy = fixture.clone();
    null_disk_policy["disk_cache_policy"] = Value::Null;
    let err = validate_workload_record(&null_disk_policy)
        .expect_err("workload admission must reject null disk-cache policy metadata");
    assert!(
        err.contains("disk_cache_policy must be a non-empty string"),
        "unexpected null disk-cache policy error: {err}"
    );

    for (field, value, expected_error) in [
        (
            "measurement_boundary",
            json!("production_bogus"),
            "invalid regression-gate measurement_boundary token",
        ),
        (
            "disk_cache_policy",
            json!("trust_me_cached"),
            "invalid regression-gate disk_cache_policy token",
        ),
        (
            "host_page_cache_policy",
            json!("mostly_controlled"),
            "invalid regression-gate host_page_cache_policy token",
        ),
    ] {
        let mut bogus = fixture.clone();
        bogus[field] = value;
        let err = validate_workload_record(&bogus)
            .expect_err("unknown regression-gate enum token must fail closed");
        assert!(err.contains(expected_error), "unexpected error: {err}");
    }

    fixture["total_calls"] = json!(0);
    let err = validate_workload_record(&fixture)
        .expect_err("workload admission must reject a zero secondary sample count");
    assert!(
        err.contains("total_calls must be a positive integer"),
        "unexpected workload admission error: {err}"
    );
}

#[test]
fn pijs_workload_admission_requires_exact_quickjs_perf_contract() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    for tool_calls in PIJS_GATE_TOOL_CALL_COUNTS {
        validate_workload_record(&pijs_gate_workload_fixture(tmp.path(), *tool_calls))
            .expect("canonical PiJS gate lane should be valid");
    }

    let cases = [
        (
            "runtime_engine",
            json!("native_rust_runtime"),
            "runtime_engine must equal \"quickjs\"",
        ),
        (
            "build_profile",
            json!("release"),
            "build_profile must equal \"perf\"",
        ),
        (
            "build_profile_verified",
            json!(false),
            "build_profile_verified must equal true",
        ),
        ("binary_path", json!(""), "binary_path must be non-empty"),
        (
            "binary_sha256",
            json!("not-a-digest"),
            "binary_sha256 must be lowercase SHA-256",
        ),
        (
            "allocator_effective",
            json!("jemalloc"),
            "allocator_effective must equal \"system\"",
        ),
        (
            "iterations",
            json!(PIJS_GATE_ITERATIONS - 1),
            "iterations must equal 2000",
        ),
        (
            "tool_calls_per_iteration",
            json!(2),
            "tool_calls_per_iteration must be one of [1, 10]",
        ),
    ];
    for (field, value, expected_error) in cases {
        let mut fixture = pijs_gate_workload_fixture(tmp.path(), 1);
        fixture[field] = value;
        let err = validate_workload_record(&fixture)
            .expect_err("non-canonical PiJS eligibility claim must fail");
        assert!(
            err.contains(expected_error),
            "invalid PiJS {field} returned unexpected error: {err}"
        );
    }
}

#[test]
fn pijs_workload_admission_allows_explicitly_ineligible_native_diagnostics() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let mut fixture = pijs_gate_workload_fixture(tmp.path(), 1);
    fixture["runtime_engine"] = json!("native_rust_runtime");
    fixture["measurement_boundary"] = json!("production_extension_runtime");
    fixture["measurement_contract_version"] = json!("production_extension_runtime.v1");
    fixture["disk_cache_policy"] = json!("not_applicable");
    fixture["confidence"] = json!(CONFIDENCE_MEDIUM);
    fixture["eligible_for_regression_gate"] = json!(false);

    validate_workload_record(&fixture)
        .expect("explicitly ineligible native comparison remains valid diagnostic evidence");
}

#[test]
fn protocol_record_validator_accepts_matched_state_session_matrix_fixture() {
    let fixture = json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "session_workload_matrix",
        "extension": "core",
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": PARTITION_MATCHED_STATE,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "correlation_id": "0123456789abcdef0123456789abcdef",
        "swarm_metrics": swarm_metrics_fixture(117.0, 48.0, 36.0, 22.0, 11.0),
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "matched-state/session_100000",
            "replay_input": {
                "session_messages": 100_000,
            },
        },
    });
    if let Err(err) = validate_protocol_record(&fixture) {
        panic!("matched-state matrix fixture should pass validation: {err}");
    }
}

#[test]
fn protocol_record_validator_rejects_missing_correlation_id() {
    let malformed = json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "cold_start",
        "extension": "hello",
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": PARTITION_MATCHED_STATE,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "matched-state/cold_start",
            "replay_input": { "runs": 5 },
        },
    });

    let err = validate_protocol_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("correlation_id"),
        "expected correlation_id failure, got: {err}"
    );
}

#[test]
fn protocol_record_validator_rejects_invalid_partition_or_size() {
    let bad_partition = json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "tool_call",
        "extension": "hello",
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": "invalid-partition",
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "correlation_id": "abc",
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "invalid/thing",
            "replay_input": { "runs": 5 },
        },
    });
    assert!(
        validate_protocol_record(&bad_partition).is_err(),
        "invalid partition fixture must fail"
    );

    let bad_size = json!({
        "schema": "pi.ext.rust_bench.v1",
        "runtime": "pi_agent_rust",
        "scenario": "tool_call",
        "extension": "hello",
        "protocol_schema": BENCH_PROTOCOL_SCHEMA,
        "protocol_version": BENCH_PROTOCOL_VERSION,
        "partition": PARTITION_REALISTIC,
        "evidence_class": EVIDENCE_CLASS_MEASURED,
        "confidence": CONFIDENCE_HIGH,
        "correlation_id": "abc",
        "scenario_metadata": {
            "runtime": "pi_agent_rust",
            "build_profile": "release",
            "host": {
                "os": "linux",
                "arch": "x86_64",
                "cpu_model": "test-cpu",
                "cpu_cores": 8,
            },
            "scenario_id": "realistic/session_bad",
            "replay_input": { "session_messages": 42 },
        },
    });
    assert!(
        validate_protocol_record(&bad_size).is_err(),
        "realistic scenario with unsupported size must fail"
    );
}

#[test]
fn extension_stratification_validator_accepts_golden_fixture() {
    let golden = json!({
        "schema": EXT_STRATIFICATION_SCHEMA,
        "run_id": "20260216T010101Z",
        "correlation_id": "abc123def456",
        "layers": [
            {
                "layer_id": "cold_load_init",
                "display_name": "Cold-load and initialization",
                "scenario_tags": ["cold-load", "init", "microbench"],
                "absolute_metrics": {"metric_name": "cold_load_p95", "value": 12.4, "unit": "ms"},
                "relative_metrics": {
                    "rust_vs_node_ratio": 1.8,
                    "rust_vs_node_ratio_basis": "direct_or_derived",
                    "rust_vs_bun_ratio": 1.8,
                    "rust_vs_bun_ratio_basis": "node_proxy"
                },
                "confidence": CONFIDENCE_MEDIUM,
                "evidence_state": EVIDENCE_CLASS_INFERRED,
                "lineage": {
                    "run_id_lineage": ["20260216T010101Z", "abc123def456"],
                    "source_artifacts": ["target/perf/ext_bench_harness.jsonl"],
                    "suite_logs": {},
                    "source_manifest_path": "target/perf/runs/20260216T010101Z/manifest.json"
                }
            },
            {
                "layer_id": "per_call_dispatch_micro",
                "display_name": "Per-call dispatch microbench",
                "scenario_tags": ["per-call", "dispatch", "microbench"],
                "absolute_metrics": {"metric_name": "dispatch_per_call", "value": 42.0, "unit": "us"},
                "relative_metrics": {
                    "rust_vs_node_ratio": 1.2,
                    "rust_vs_node_ratio_basis": "direct_or_derived",
                    "rust_vs_bun_ratio": 1.2,
                    "rust_vs_bun_ratio_basis": "node_proxy"
                },
                "confidence": CONFIDENCE_HIGH,
                "evidence_state": EVIDENCE_CLASS_MEASURED,
                "lineage": {
                    "run_id_lineage": ["20260216T010101Z", "abc123def456"],
                    "source_artifacts": ["target/perf/scenario_runner.jsonl"],
                    "suite_logs": {},
                    "source_manifest_path": "target/perf/runs/20260216T010101Z/manifest.json"
                }
            },
            {
                "layer_id": "full_e2e_long_session",
                "display_name": "Full end-to-end long-session workload",
                "scenario_tags": ["full-e2e", "long-session", "release-facing"],
                "absolute_metrics": {"metric_name": "long_session_elapsed", "value": 950.0, "unit": "ms"},
                "relative_metrics": {
                    "rust_vs_node_ratio": null,
                    "rust_vs_node_ratio_basis": "missing",
                    "rust_vs_bun_ratio": null,
                    "rust_vs_bun_ratio_basis": "missing"
                },
                "confidence": CONFIDENCE_LOW,
                "evidence_state": "absolute_only",
                "lineage": {
                    "run_id_lineage": ["20260216T010101Z", "abc123def456"],
                    "source_artifacts": ["target/perf/pijs_workload.jsonl"],
                    "suite_logs": {},
                    "source_manifest_path": "target/perf/runs/20260216T010101Z/manifest.json"
                }
            }
        ],
        "claim_integrity": {
            "anti_conflation": {
                "cold_load_wins_do_not_imply_per_call_or_e2e": true,
                "per_call_wins_do_not_imply_full_e2e": true,
                "full_e2e_is_release_facing_primary_signal": true
            },
            "cherry_pick_guard": {
                "requires_all_layers_for_global_claim": true,
                "layer_coverage": {
                    "cold_load_init": true,
                    "per_call_dispatch_micro": true,
                    "full_e2e_long_session": false
                },
                "global_claim_valid": false,
                "invalidity_reasons": ["missing_layer_coverage:full_e2e_long_session"]
            },
            "partition_coverage": {"matched-state": true, "realistic": false}
        },
        "lineage": {
            "run_id_lineage": ["20260216T010101Z", "abc123def456"],
            "source_manifest_path": "target/perf/runs/20260216T010101Z/manifest.json"
        }
    });

    assert!(
        validate_extension_stratification_record(&golden).is_ok(),
        "golden extension stratification fixture should pass validation"
    );
}

#[test]
fn extension_stratification_validator_rejects_missing_claim_integrity() {
    let malformed = json!({
        "schema": EXT_STRATIFICATION_SCHEMA,
        "run_id": "20260216T010101Z",
        "correlation_id": "abc123def456",
        "layers": [],
        "lineage": { "run_id_lineage": ["20260216T010101Z", "abc123def456"] }
    });

    let err = validate_extension_stratification_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("claim_integrity"),
        "expected missing claim_integrity failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_accepts_golden_fixture() {
    let golden = phase1_matrix_validation_golden_fixture();

    assert!(
        validate_phase1_matrix_validation_record(&golden).is_ok(),
        "golden phase1 matrix fixture should pass validation"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_weighted_bottleneck_attribution() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed
        .as_object_mut()
        .expect("phase1 matrix object")
        .remove("weighted_bottleneck_attribution");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("weighted_bottleneck_attribution"),
        "expected weighted_bottleneck_attribution missing failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_lineage_valid_cell_count_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["lineage"]["valid_cell_count"] = json!(1);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("lineage.valid_cell_count")
            && err.contains("observed pass-cell count with valid stage totals"),
        "expected weighted lineage count parity failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_per_scale_present_key_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["per_scale"][0]["partitions"][1]["present"] =
        json!(false);
    malformed["weighted_bottleneck_attribution"]["per_scale"][0]["partitions"][1]["stage_pct"] = json!({
        "open_ms": null,
        "append_ms": null,
        "save_ms": null,
        "index_ms": null
    });
    malformed["weighted_bottleneck_attribution"]["per_scale"][0]["partitions"][1]
        .as_object_mut()
        .expect("partition row")
        .remove("total_stage_ms");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("per_scale present keys"),
        "expected weighted per_scale present-key parity failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_global_ranking_not_sorted() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    let tmp = malformed["weighted_bottleneck_attribution"]["global_ranking"][0].clone();
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0] =
        malformed["weighted_bottleneck_attribution"]["global_ranking"][1].clone();
    malformed["weighted_bottleneck_attribution"]["global_ranking"][1] = tmp;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("sorted descending by weighted_contribution_pct"),
        "expected weighted global ranking order failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_global_ranking_ci_inversion() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_lower_pct"] =
        json!(45.0);
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_upper_pct"] =
        json!(40.0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("ci95_lower_pct") && err.contains("ci95_upper_pct") && err.contains('>'),
        "expected weighted global ranking CI inversion failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_mean_outside_ci_bounds() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    // Set mean_share_pct above ci95_upper_pct
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["mean_share_pct"] =
        json!(50.0);
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_lower_pct"] =
        json!(35.0);
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_upper_pct"] =
        json!(45.0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("mean_share_pct") && err.contains("within CI"),
        "expected mean outside CI bounds failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_wrong_weighted_schema_version() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["schema"] =
        json!("pi.perf.phase1_weighted_bottleneck_attribution.v2");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("weighted_bottleneck_attribution.schema"),
        "expected wrong weighted schema version failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_wrong_weighting_policy() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["weighting_policy"] = json!("uniform");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("weighting_policy"),
        "expected wrong weighting_policy failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_wrong_confidence_method() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["confidence_method"] = json!("bootstrap_95");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("confidence_method"),
        "expected wrong confidence_method failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_ci_bounds_partially_null() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    // Set only ci95_lower_pct, leave ci95_upper_pct as null
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_lower_pct"] =
        json!(35.0);
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_upper_pct"] =
        Value::Null;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("ci95_lower_pct") && err.contains("ci95_upper_pct") && err.contains("both"),
        "expected partial CI bounds failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_sample_size_zero() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["sample_size"] = json!(0);
    // Remove CI bounds since sample_size=0 would be invalid regardless
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_lower_pct"] =
        Value::Null;
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_upper_pct"] =
        Value::Null;
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["mean_share_pct"] =
        Value::Null;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("sample_size") && err.contains("> 0"),
        "expected zero sample_size failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_sample_size_exceeds_valid_cells() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    // Set sample_size to something much larger than valid_cell_count
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["sample_size"] = json!(999);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("sample_size") && err.contains("valid_cell_count"),
        "expected sample_size > valid_cell_count failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_weighted_multi_sample_without_ci_bounds() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    // sample_size > 1 but CI bounds are null
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["sample_size"] = json!(2);
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_lower_pct"] =
        Value::Null;
    malformed["weighted_bottleneck_attribution"]["global_ranking"][0]["ci95_upper_pct"] =
        Value::Null;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("sample_size") && err.contains("requires CI bounds"),
        "expected multi-sample without CI failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_downstream_consumer_wrong_bead_id() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["consumption_contract"]["downstream_consumers"]["opportunity_matrix"]["bead_id"] =
        json!("bd-wrong.1");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("bead_id") && err.contains("bd-3ar8v.6.1"),
        "expected wrong downstream consumer bead_id failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_downstream_consumer_wrong_selector() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["consumption_contract"]["downstream_consumers"]["parameter_sweeps"]["selector"] =
        json!("weighted_bottleneck_attribution.wrong_path");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("selector") && err.contains("weighted_bottleneck_attribution.per_scale"),
        "expected wrong downstream consumer selector failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_downstream_consumer() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["consumption_contract"]["downstream_consumers"]
        .as_object_mut()
        .expect("downstream_consumers object")
        .remove("opportunity_matrix");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("opportunity_matrix"),
        "expected missing downstream consumer failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_required_downstream_bead() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    // Remove bd-3ar8v.6.2 from downstream_beads array
    let beads = malformed["consumption_contract"]["downstream_beads"]
        .as_array_mut()
        .expect("downstream_beads array");
    beads.retain(|v| v.as_str() != Some("bd-3ar8v.6.2"));

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("bd-3ar8v.6.2"),
        "expected missing required downstream bead failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_numeric_primary_e2e_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]["primary_e2e"]["wall_clock_ms"] = json!("1200ms");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell primary_e2e.wall_clock_ms"),
        "expected primary_e2e wall_clock type failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_positive_primary_outcomes_ratio() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["rust_vs_bun_ratio"] = json!(0.0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("primary_outcomes.rust_vs_bun_ratio"),
        "expected primary_outcomes ratio positivity failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_positive_pass_cell_primary_e2e_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]["primary_e2e"]["wall_clock_ms"] = json!(0.0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell primary_e2e.wall_clock_ms"),
        "expected pass-cell non-positive metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_numeric_primary_outcomes_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["wall_clock_ms"] = json!("unknown");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("primary_outcomes.wall_clock_ms"),
        "expected non-numeric primary_outcomes metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_null_pass_cell_primary_e2e_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]["primary_e2e"]["rust_vs_node_ratio"] = Value::Null;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell primary_e2e.rust_vs_node_ratio"),
        "expected pass-cell null metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_null_pass_primary_outcomes_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["wall_clock_ms"] = Value::Null;

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("primary_outcomes.wall_clock_ms"),
        "expected pass primary_outcomes null metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_numeric_fail_cell_primary_e2e_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]["status"] = json!("fail");
    malformed["matrix_cells"][0]["primary_e2e"]["rust_vs_node_ratio"] = json!("unknown");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell primary_e2e.rust_vs_node_ratio"),
        "expected fail-cell primary_e2e type failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_positive_fail_cell_primary_e2e_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]["status"] = json!("fail");
    malformed["matrix_cells"][0]["primary_e2e"]["rust_vs_bun_ratio"] = json!(0.0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell primary_e2e.rust_vs_bun_ratio"),
        "expected fail-cell non-positive metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_numeric_fail_primary_outcomes_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["status"] = json!("fail");
    malformed["primary_outcomes"]["wall_clock_ms"] = json!("n/a");
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("primary_outcomes.wall_clock_ms"),
        "expected fail primary_outcomes type failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_positive_fail_primary_outcomes_metric() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["status"] = json!("fail");
    malformed["primary_outcomes"]["rust_vs_node_ratio"] = json!(0.0);
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("primary_outcomes.rust_vs_node_ratio"),
        "expected fail primary_outcomes non-positive metric failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_accepts_nullable_fail_metrics() {
    let mut candidate = phase1_matrix_validation_golden_fixture();
    candidate["matrix_cells"][0]["status"] = json!("fail");
    candidate["matrix_cells"][1]["status"] = json!("fail");
    candidate["matrix_cells"][0]["primary_e2e"]["wall_clock_ms"] = Value::Null;
    candidate["matrix_cells"][0]["primary_e2e"]["rust_vs_node_ratio"] = Value::Null;
    candidate["matrix_cells"][0]["primary_e2e"]["rust_vs_bun_ratio"] = Value::Null;
    candidate["matrix_cells"][1]["primary_e2e"]["wall_clock_ms"] = Value::Null;
    candidate["matrix_cells"][1]["primary_e2e"]["rust_vs_node_ratio"] = Value::Null;
    candidate["matrix_cells"][1]["primary_e2e"]["rust_vs_bun_ratio"] = Value::Null;
    candidate["primary_outcomes"]["status"] = json!("fail");
    candidate["primary_outcomes"]["wall_clock_ms"] = Value::Null;
    candidate["primary_outcomes"]["rust_vs_node_ratio"] = Value::Null;
    candidate["primary_outcomes"]["rust_vs_bun_ratio"] = Value::Null;
    candidate["weighted_bottleneck_attribution"]["status"] = json!("missing");
    candidate["weighted_bottleneck_attribution"]["per_scale"] = json!([]);
    candidate["weighted_bottleneck_attribution"]["global_ranking"] = json!([]);
    candidate["weighted_bottleneck_attribution"]["lineage"]["valid_cell_count"] = json!(0);
    candidate["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    assert!(
        validate_phase1_matrix_validation_record(&candidate).is_ok(),
        "fail-status records should allow nullable primary metrics while remaining schema-valid"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_stage_attribution() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][0]
        .as_object_mut()
        .expect("matrix_cells[0] object")
        .remove("stage_attribution");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("matrix cell missing stage_attribution"),
        "expected stage_attribution validation failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_required_cell_count_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_requirements"]["required_cell_count"] = json!(1);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("required_cell_count"),
        "expected required_cell_count mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_stage_summary_count_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["stage_summary"]["cells_with_complete_stage_breakdown"] = json!(0);
    malformed["stage_summary"]["covered_cells"] = json!(0);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("stage_summary complete+missing"),
        "expected stage_summary mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_complete_stage_count_mismatch_vs_attribution() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["stage_summary"]["cells_with_complete_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["cells_missing_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["covered_cells"] = json!(1);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("cells_with_complete_stage_breakdown"),
        "expected complete-stage count mismatch failure, got: {err}"
    );
    assert!(
        err.contains("observed complete-stage cell count"),
        "expected observed complete-stage count detail, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_cells_identity_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][1]["stage_attribution"]["index_ms"] = Value::Null;
    malformed["matrix_cells"][1]["missing_reasons"] = json!(["missing_stage_metrics:index_ms"]);
    malformed["stage_summary"]["operation_stage_coverage"]["index_ms"] = json!(1);
    malformed["stage_summary"]["cells_with_complete_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["cells_missing_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["covered_cells"] = json!(1);
    malformed["stage_summary"]["missing_cells"] = json!([
        {
            "workload_partition": "matched-state",
            "session_messages": 100_000,
            "reasons": ["missing_stage_metrics:index_ms"]
        }
    ]);
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("stage_summary.missing_cells entry"),
        "expected missing_cells identity mismatch failure, got: {err}"
    );
    assert!(
        err.contains("missing stage metrics"),
        "expected observed missing-stage parity detail, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_cells_reason_mismatch_vs_matrix_cell() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][1]["stage_attribution"]["index_ms"] = Value::Null;
    malformed["matrix_cells"][1]["missing_reasons"] = json!([
        "missing_stage_metrics:index_ms",
        "missing_matrix_source_record"
    ]);
    malformed["stage_summary"]["operation_stage_coverage"]["index_ms"] = json!(1);
    malformed["stage_summary"]["cells_with_complete_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["cells_missing_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["covered_cells"] = json!(1);
    malformed["stage_summary"]["missing_cells"] = json!([
        {
            "workload_partition": "realistic",
            "session_messages": 100_000,
            "reasons": ["missing_stage_metrics:index_ms"]
        }
    ]);
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("must equal matrix cell missing_reasons"),
        "expected missing_cells reason mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_stage_cell_without_missing_stage_reason_token() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_cells"][1]["stage_attribution"]["index_ms"] = Value::Null;
    malformed["matrix_cells"][1]["missing_reasons"] = json!(["missing_matrix_source_record"]);
    malformed["stage_summary"]["operation_stage_coverage"]["index_ms"] = json!(1);
    malformed["stage_summary"]["cells_with_complete_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["cells_missing_stage_breakdown"] = json!(1);
    malformed["stage_summary"]["covered_cells"] = json!(1);
    malformed["stage_summary"]["missing_cells"] = json!([
        {
            "workload_partition": "realistic",
            "session_messages": 100_000,
            "reasons": ["missing_matrix_source_record"]
        }
    ]);
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("missing_reasons must include at least one missing_stage_metrics:* reason"),
        "expected missing-stage reason-token enforcement failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_unexpected_stage_coverage_key() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["stage_summary"]["operation_stage_coverage"]["unexpected_ms"] = json!(1);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("unexpected keys"),
        "expected unexpected stage coverage key failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_deflated_stage_coverage_count() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["stage_summary"]["operation_stage_coverage"]["index_ms"] = json!(1);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("operation_stage_coverage.index_ms"),
        "expected index_ms stage coverage mismatch failure, got: {err}"
    );
    assert!(
        err.contains("observed non-null stage_attribution count"),
        "expected observed stage attribution count mismatch detail, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_inflated_stage_coverage_count() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["stage_summary"]["operation_stage_coverage"]["open_ms"] = json!(99);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("operation_stage_coverage.open_ms"),
        "expected open_ms stage coverage mismatch failure, got: {err}"
    );
    assert!(
        err.contains("observed non-null stage_attribution count"),
        "expected observed stage attribution count mismatch detail, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_non_primary_ordering_policy() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["ordering_policy"] = json!("microbench_before_primary_e2e");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("ordering_policy"),
        "expected ordering policy failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_required_cell_count_exceeding_partition_size_space() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_requirements"]["required_partition_tags"] = json!(["matched-state"]);
    malformed["matrix_requirements"]["required_session_message_sizes"] = json!([100_000]);
    malformed["matrix_requirements"]["required_cell_count"] = json!(2);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("exceeds unique partition-size combinations"),
        "expected partition/size cardinality failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_duplicate_partition_size_cells() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_requirements"]["required_partition_tags"] =
        json!(["matched-state", "realistic"]);
    malformed["matrix_requirements"]["required_session_message_sizes"] = json!([100_000]);
    malformed["matrix_requirements"]["required_cell_count"] = json!(2);
    malformed["matrix_cells"][1]["workload_partition"] = json!("matched-state");
    malformed["matrix_cells"][1]["scenario_id"] = json!("matched-state/session_100000_dup");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("duplicates partition-size key"),
        "expected duplicate partition-size cell failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_cell_partition_not_in_requirements() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["matrix_requirements"]["required_partition_tags"] =
        json!(["matched-state", "realistic"]);
    malformed["matrix_requirements"]["required_session_message_sizes"] = json!([100_000]);
    malformed["matrix_requirements"]["required_cell_count"] = json!(2);
    malformed["matrix_cells"][0]["workload_partition"] = json!("experimental");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("workload_partition 'experimental'"),
        "expected unknown partition failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_empty_run_id() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["run_id"] = json!(" ");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("run_id must be non-empty"),
        "expected non-empty run_id failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_lineage_mismatch_with_top_level_ids() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["lineage"]["run_id_lineage"][0] = json!("other-run");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("must match run_id"),
        "expected lineage/run_id mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_evidence_source_identity() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["evidence_links"]
        .as_object_mut()
        .expect("evidence_links object")
        .remove("source_identity");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("evidence_links missing source_identity"),
        "expected evidence_links.source_identity failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_lineage_required_artifact_mismatch() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["lineage"]["source_stratification_path"] = json!("target/perf/other.json");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("lineage.source_stratification_path")
            && err.contains("required_artifacts.stratification"),
        "expected lineage/evidence_links stratification mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_invalid_regression_guard_status() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["regression_guards"]["memory"] = json!("warn");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("regression_guards.memory must be one of pass/fail/missing"),
        "expected regression_guards status enum failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_missing_reason_for_failed_regression_guard() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["regression_guards"]["correctness"] = json!("fail");
    malformed["regression_guards"]["failure_or_gap_reasons"] = json!([]);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("must include correctness_regression"),
        "expected missing fail reason failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_reason_for_passing_regression_guard() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["regression_guards"]["failure_or_gap_reasons"] = json!(["security_regression"]);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("regression_guards.security is pass"),
        "expected pass/reason mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_unknown_regression_guard_reason() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["regression_guards"]["memory"] = json!("missing");
    malformed["regression_guards"]["failure_or_gap_reasons"] =
        json!(["memory_regression_unverified", "unexpected_reason"]);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("contains unknown reason"),
        "expected unknown regression reason failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_phase5_ready_true_when_prerequisites_fail() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["primary_outcomes"]["status"] = json!("fail");

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("artifact_ready_for_phase5 (true)")
            && err.contains("expected deterministic value (false)"),
        "expected phase5 deterministic mismatch failure, got: {err}"
    );
}

#[test]
fn phase1_matrix_validator_rejects_phase5_ready_false_when_prerequisites_pass() {
    let mut malformed = phase1_matrix_validation_golden_fixture();
    malformed["consumption_contract"]["artifact_ready_for_phase5"] = json!(false);

    let err = validate_phase1_matrix_validation_record(&malformed).expect_err("fixture must fail");
    assert!(
        err.contains("artifact_ready_for_phase5 (false)")
            && err.contains("expected deterministic value (true)"),
        "expected phase5 deterministic mismatch failure, got: {err}"
    );
}

#[test]
fn evidence_contract_schema_includes_benchmark_protocol_definition() {
    let schema_path = project_root().join("docs/evidence-contract-schema.json");
    let content = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", schema_path.display()));
    let parsed: Value = serde_json::from_str(&content).expect("valid evidence contract JSON");
    let benchmark_protocol = parsed["definitions"]["benchmark_protocol"]
        .as_object()
        .expect("definitions.benchmark_protocol object must exist");

    assert_eq!(
        benchmark_protocol["properties"]["schema"]["const"]
            .as_str()
            .unwrap_or_default(),
        BENCH_PROTOCOL_SCHEMA
    );

    let partition_values: Vec<&str> =
        benchmark_protocol["properties"]["partition_tags"]["items"]["enum"]
            .as_array()
            .expect("partition enum array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
    assert!(partition_values.contains(&PARTITION_MATCHED_STATE));
    assert!(partition_values.contains(&PARTITION_REALISTIC));

    let size_values: Vec<u64> =
        benchmark_protocol["properties"]["realistic_session_sizes"]["items"]["enum"]
            .as_array()
            .expect("realistic session size enum array")
            .iter()
            .filter_map(Value::as_u64)
            .collect();
    assert_eq!(size_values, REALISTIC_SESSION_SIZES);

    let required_fields: Vec<&str> = benchmark_protocol["required"]
        .as_array()
        .expect("benchmark_protocol.required array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        required_fields.contains(&"user_perceived_sli_catalog"),
        "benchmark protocol schema must require user_perceived_sli_catalog"
    );
    assert!(
        required_fields.contains(&"scenario_sli_matrix"),
        "benchmark protocol schema must require scenario_sli_matrix"
    );
    assert!(
        required_fields.contains(&"partition_weighting"),
        "benchmark protocol schema must require partition_weighting"
    );
    assert!(
        required_fields.contains(&"budget_input_negative_controls"),
        "benchmark protocol schema must require release-budget negative controls"
    );
    assert!(
        required_fields.contains(&"partition_interpretation"),
        "benchmark protocol schema must require partition_interpretation"
    );
    assert!(
        !required_fields.contains(&"regression_gate_admission"),
        "regression-gate admission must remain an optional v1 overlay"
    );
    assert!(
        !required_fields.contains(&"pijs_regression_gate_admission"),
        "PiJS-specific admission must remain an optional v1 overlay"
    );

    let budget_controls = &benchmark_protocol["properties"]["budget_input_negative_controls"];
    assert_eq!(
        budget_controls["properties"]["unproven_input_status"]["const"],
        "NO_DATA"
    );
    assert_eq!(
        budget_controls["properties"]["release_binary"]["properties"]["schema"]["const"],
        "pi.perf.binary_size_measurement.v1"
    );
    assert_eq!(
        budget_controls["properties"]["idle_rss"]["properties"]["schema"]["const"],
        "pi.perf.idle_rss_measurement.v1"
    );
    let idle_rss_fields =
        budget_controls["properties"]["idle_rss"]["properties"]["required_fields"]["items"]["enum"]
            .as_array()
            .expect("idle RSS required-field enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<HashSet<_>>();
    for field in [
        "sample_count",
        "samples",
        "rss_spread_bytes",
        "settle_ms",
        "bench_env_source",
        "bench_env",
        "bench_env_sha256",
    ] {
        assert!(
            idle_rss_fields.contains(field),
            "idle RSS negative control must require {field}"
        );
    }
    assert_eq!(
        budget_controls["properties"]["criterion_cold_load"]["properties"]["schema"]["const"],
        "pi.perf.cold_load_measurement.v1"
    );
    assert_eq!(
        budget_controls["properties"]["criterion_cold_load"]["properties"]["max_noise_score"]["const"],
        0
    );

    let admission = benchmark_protocol["properties"]["regression_gate_admission"]
        .as_object()
        .expect("benchmark protocol must define regression_gate_admission");
    assert_eq!(
        admission["additionalProperties"].as_bool(),
        Some(false),
        "regression-gate admission schema must reject unknown policy keys"
    );
    let admission_required = admission["required"]
        .as_array()
        .expect("regression_gate_admission.required array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    for field in [
        "scope",
        "required_record_fields",
        "load_scenario_required_fields",
        "allowed_measurement_methods",
        "allowed_measurement_boundaries",
        "eligible_measurement_boundaries",
        "allowed_disk_cache_policies",
        "allowed_host_page_cache_policies",
        "required_eligible_provenance_fields",
        "eligible_evidence_class",
        "eligible_confidence",
        "eligible_measurement_method",
        "required_eligible_host_page_cache_policy",
        "positive_sample_count_fields",
        "require_positive_sample_count",
        "uncontrolled_host_page_cache_eligible",
    ] {
        assert!(
            admission_required.contains(field),
            "regression-gate schema must require {field}"
        );
    }
    assert_eq!(
        admission["properties"]["scope"]["const"].as_str(),
        Some(REGRESSION_GATE_GENERIC_SCOPE)
    );
    assert_eq!(
        admission["properties"]["eligible_evidence_class"]["const"].as_str(),
        Some(EVIDENCE_CLASS_MEASURED)
    );
    assert_eq!(
        admission["properties"]["eligible_confidence"]["const"].as_str(),
        Some(CONFIDENCE_HIGH)
    );
    assert_eq!(
        admission["properties"]["eligible_measurement_method"]["const"].as_str(),
        Some(MEASUREMENT_METHOD_WALL_CLOCK)
    );
    for (field, expected) in [
        (
            "allowed_measurement_boundaries",
            REGRESSION_GATE_ALLOWED_BOUNDARIES,
        ),
        (
            "eligible_measurement_boundaries",
            REGRESSION_GATE_ELIGIBLE_BOUNDARIES,
        ),
        (
            "allowed_disk_cache_policies",
            REGRESSION_GATE_ALLOWED_DISK_CACHE_POLICIES,
        ),
        (
            "allowed_host_page_cache_policies",
            REGRESSION_GATE_ALLOWED_HOST_PAGE_CACHE_POLICIES,
        ),
        (
            "required_eligible_provenance_fields",
            REGRESSION_GATE_REQUIRED_ELIGIBLE_PROVENANCE_FIELDS,
        ),
    ] {
        let values = admission["properties"][field]["items"]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{field} must expose an enum"))
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(values, expected, "unexpected {field} enum");
        assert_eq!(
            admission["properties"][field]["minItems"].as_u64(),
            Some(expected.len() as u64),
            "unexpected {field} minItems"
        );
        assert_eq!(
            admission["properties"][field]["maxItems"].as_u64(),
            Some(expected.len() as u64),
            "unexpected {field} maxItems"
        );
    }
    assert_eq!(
        admission["properties"]["required_eligible_host_page_cache_policy"]["const"].as_str(),
        Some("not_applicable_measured_region")
    );
    assert_eq!(
        admission["properties"]["required_record_fields"]["minItems"].as_u64(),
        Some(REGRESSION_GATE_REQUIRED_RECORD_FIELDS.len() as u64)
    );
    assert_eq!(
        admission["properties"]["required_record_fields"]["maxItems"].as_u64(),
        Some(REGRESSION_GATE_REQUIRED_RECORD_FIELDS.len() as u64)
    );
    let required_record_field_values =
        admission["properties"]["required_record_fields"]["items"]["enum"]
            .as_array()
            .expect("regression-gate required record field enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<HashSet<_>>();
    assert!(
        required_record_field_values.contains("disk_cache_policy"),
        "regression-gate schema must require explicit disk-cache policy metadata"
    );
    assert_eq!(
        admission["properties"]["require_positive_sample_count"]["const"].as_bool(),
        Some(true)
    );
    assert_eq!(
        admission["properties"]["uncontrolled_host_page_cache_eligible"]["const"].as_bool(),
        Some(false)
    );

    let pijs_admission = benchmark_protocol["properties"]["pijs_regression_gate_admission"]
        .as_object()
        .expect("benchmark protocol must define pijs_regression_gate_admission");
    assert_eq!(
        pijs_admission["additionalProperties"].as_bool(),
        Some(false)
    );
    let pijs_admission_required = pijs_admission["required"]
        .as_array()
        .expect("pijs_regression_gate_admission.required array");
    assert!(
        pijs_admission_required
            .iter()
            .any(|field| field.as_str() == Some("required_record_fields")),
        "PiJS admission schema must require its complete record-field inventory"
    );
    assert!(
        pijs_admission_required
            .iter()
            .any(|field| field.as_str() == Some("required_build_profile_verified")),
        "PiJS admission schema must require executable-path profile verification"
    );
    assert_eq!(
        pijs_admission["properties"]["scope"]["const"].as_str(),
        Some(PIJS_GATE_SCOPE)
    );
    assert_eq!(
        pijs_admission["properties"]["required_runtime_engine"]["const"].as_str(),
        Some(PIJS_GATE_RUNTIME_ENGINE)
    );
    assert_eq!(
        pijs_admission["properties"]["required_build_profile"]["const"].as_str(),
        Some(PIJS_GATE_BUILD_PROFILE)
    );
    assert_eq!(
        pijs_admission["properties"]["required_build_profile_verified"]["const"].as_bool(),
        Some(true)
    );
    assert_eq!(
        pijs_admission["properties"]["required_iterations"]["const"].as_u64(),
        Some(PIJS_GATE_ITERATIONS)
    );
    assert_eq!(
        pijs_admission["properties"]["required_record_fields"]["minItems"].as_u64(),
        Some(PIJS_GATE_REQUIRED_RECORD_FIELDS.len() as u64)
    );
    assert_eq!(
        pijs_admission["properties"]["required_record_fields"]["maxItems"].as_u64(),
        Some(PIJS_GATE_REQUIRED_RECORD_FIELDS.len() as u64)
    );
    let pijs_required_record_fields =
        pijs_admission["properties"]["required_record_fields"]["items"]["enum"]
            .as_array()
            .expect("PiJS required record field enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
    assert_eq!(
        pijs_required_record_fields, PIJS_GATE_REQUIRED_RECORD_FIELDS,
        "PiJS admission schema must enumerate every required record field exactly"
    );

    assert_eq!(
        benchmark_protocol["properties"]["partition_interpretation"]["properties"]
            ["forbid_single_partition_conclusion"]["const"]
            .as_bool(),
        Some(true),
        "schema must enforce no single-partition release conclusion"
    );
}

#[test]
fn protocol_is_referenced_by_benchmark_and_conformance_harnesses() {
    let refs = vec![
        ("tests/bench_scenario_runner.rs", BENCH_PROTOCOL_SCHEMA),
        ("tests/perf_bench_harness.rs", "pi.ext.rust_bench.v1"),
        ("tests/ext_bench_harness.rs", "pi.ext.rust_bench.v1"),
        ("tests/perf_comparison.rs", "pi.ext.perf_comparison.v1"),
        ("tests/ext_conformance_scenarios.rs", "conformance"),
    ];

    for (rel_path, marker) in refs {
        let abs = project_root().join(rel_path);
        let text = std::fs::read_to_string(&abs)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", abs.display()));
        assert!(
            text.contains(marker),
            "{rel_path} must reference marker `{marker}`"
        );
    }
}

#[test]
fn orchestrate_script_emits_extension_stratification_contract() {
    let script_path = project_root().join("scripts/perf/orchestrate.sh");
    let content = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", script_path.display()));

    for token in &[
        "extension_benchmark_stratification.json",
        EXT_STRATIFICATION_SCHEMA,
        "\"cold_load_init\"",
        "\"per_call_dispatch_micro\"",
        "\"full_e2e_long_session\"",
        "microbench_only_claim",
        "global_claim_missing_partition_coverage",
    ] {
        assert!(
            content.contains(token),
            "orchestrate stratification phase must include token: {token}"
        );
    }
}

#[test]
fn orchestrate_script_emits_budget_input_negative_controls_before_consumption() {
    let script_path = project_root().join("scripts/perf/orchestrate.sh");
    let content = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", script_path.display()));

    for token in [
        "pi.perf.binary_size_measurement.v1",
        "binary_size_measurement.json",
        "Cargo.toml#profile.release",
        "cargo build --bin pi --release",
        "pi.perf.cold_load_measurement.v1",
        "cold_load_measurement.json",
        "pi.perf.idle_rss_measurement.v1",
        "idle_memory_rss.raw.json",
        "idle_memory_rss.json",
        "PI_IDLE_RSS_RAW_RELATIVE_PATH",
        "[idle-rss-control] ",
        "benches/bench_env.rs",
        "PERF_MAX_BENCH_ENV_NOISE_SCORE",
        "deferred_perf_budgets=true",
    ] {
        assert!(
            content.contains(token),
            "orchestrate budget-control phase must include token: {token}"
        );
    }
    let cold_control = content
        .find("write_cold_load_measurement_control \"$result_dir\" \"$exit_code\"")
        .expect("cold-load control producer call");
    let idle_control = content
        .find("write_idle_rss_measurement_control")
        .expect("idle-RSS control producer call");
    let budget_consumer = content
        .rfind("run_test_suite \"perf_budgets\"")
        .expect("deferred budget consumer call");
    assert!(
        cold_control < budget_consumer,
        "cold-load proof must be emitted before the budget consumer runs"
    );
    assert!(
        idle_control < budget_consumer,
        "idle-RSS proof must be emitted before the budget consumer runs"
    );
}

#[test]
fn orchestrate_final_evidence_gates_run_after_derived_artifact_generation() {
    let script_path = project_root().join("scripts/perf/orchestrate.sh");
    let content = fs::read_to_string(&script_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", script_path.display()));

    let phase1_generation = content
        .find("Phase-1 matrix validation written")
        .expect("phase1 derived-artifact generation");
    let post_generation_budget = content
        .find("POST_GENERATION_BUDGET_DIR=")
        .expect("post-generation Rust budget gate");
    let final_preflight = content
        .find("run_budget_preflight \"$PREFLIGHT_AFTER_RUN_PATH\"")
        .expect("final budget preflight");
    let initial_preflight = content
        .find("run_budget_preflight \"$PREFLIGHT_BEFORE_REFRESH_PATH\"")
        .expect("initial full-readiness preflight");
    let final_staging = content
        .find("run_artifact_staging_manifest \"$STAGING_MANIFEST_PATH\"")
        .expect("final artifact staging");
    let checksums = content
        .find("# ─── Phase 6: Generate checksums")
        .expect("checksum generation phase");

    for token in [
        "--message-format=json-render-diagnostics",
        "pi.perf.test_binary_attestation.v1",
        "perf_budgets binary attestation commit mismatch",
        "perf_budgets test binary checksum mismatch",
        "for required_env in PERF_EVIDENCE_DIR PI_PERF_POST_GENERATION PI_PERF_EXPECTED_SOURCE_COMMIT; do",
        "POST_GENERATION_STAGE_RELATIVE=\".rch-tmp/pi-perf-evidence/$post_generation_stage_key\"",
        "pi.perf.post_generation_evidence_inventory.v1",
        "--overlay-path\" \"$POST_GENERATION_STAGE_RELATIVE",
        "\"${POST_GENERATION_RUNNER_ARGS[@]}\" test --test perf_budgets --profile \"$CARGO_PROFILE\"",
        "clean-overlay receipt: base=$GIT_COMMIT_FULL",
        "source_dataset_checksum_mismatch",
        "timestamp_before_run_start",
        "RCH post-generation staging precondition",
        "RCH post-generation budget postcondition",
        "RCH checksum precondition",
        "RCH final-success precondition",
    ] {
        assert!(
            content.contains(token),
            "post-generation evidence gate must include token: {token}"
        );
    }
    assert!(
        !content.contains("post_generation_budget_binary"),
        "the controller must not execute a downloaded RCH test binary directly"
    );

    assert!(
        phase1_generation < post_generation_budget
            && post_generation_budget < final_preflight
            && final_preflight < final_staging
            && final_staging < checksums,
        "phase1 generation, Rust consumption, final preflight/staging, and checksums must remain causally ordered"
    );
    assert!(
        initial_preflight < phase1_generation,
        "the initial full-readiness preflight must remain before derived artifact generation"
    );
    assert_eq!(
        content.matches("--artifact-readiness-only").count(),
        1,
        "only the final preflight after the authoritative Rust budget gate may use artifact-only readiness"
    );
    assert!(
        content.contains(
            "run_budget_preflight \"$PREFLIGHT_AFTER_RUN_PATH\" --artifact-readiness-only"
        ),
        "the final preflight must use correlation-bound artifact-only readiness"
    );
}

#[test]
fn orchestrate_script_emits_phase1_matrix_validation_contract() {
    let script_path = project_root().join("scripts/perf/orchestrate.sh");
    let content = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", script_path.display()));

    for token in &[
        "phase1_matrix_validation.json",
        PHASE1_MATRIX_SCHEMA,
        "required_stage_keys = [\"open_ms\", \"append_ms\", \"save_ms\", \"index_ms\"]",
        "\"required_session_message_sizes\"",
        "\"cells_with_complete_stage_breakdown\"",
        "\"swarm_summary\"",
        "\"cells_with_complete_swarm_metrics\"",
        "\"missing_swarm_metrics\"",
        "\"weighted_bottleneck_attribution\"",
        "\"pi.perf.phase1_weighted_bottleneck_attribution.v1\"",
        "\"global_ranking\"",
        "\"weighted_contribution_pct\"",
        "\"confidence_method\"",
        "\"primary_e2e_before_microbench\"",
        "\"downstream_consumers\"",
        "weighted_bottleneck_attribution.global_ranking",
        "weighted_bottleneck_attribution.per_scale",
        "\"artifact_ready_for_phase5\"",
        "\"failure_or_gap_reasons\"",
        "_regression_unverified",
        "\"source_identity\"",
        "\"source_manifest_path\"",
        "\"source_scenario_runner_path\"",
        "\"source_workload_path\"",
        "\"source_stratification_path\"",
        "\"source_baseline_confidence_path\"",
        "\"source_perf_sli_contract_path\"",
    ] {
        assert!(
            content.contains(token),
            "orchestrate phase-1 matrix phase must include token: {token}"
        );
    }
}

#[cfg(unix)]
fn run_orchestrate_with_fake_toolchain_with_env(
    extra_env: &[(&str, &str)],
) -> (std::process::Output, PathBuf) {
    let temp_root = unique_temp_dir("orchestrate-stratification");
    let bin_dir = temp_root.join("bin");
    let target_dir = temp_root.join("target");
    let output_dir = temp_root.join("run");
    let fault_injection_root = temp_root.join("fault-injection");

    fs::create_dir_all(&bin_dir).expect("create bin dir");
    fs::create_dir_all(&target_dir).expect("create target dir");
    fs::create_dir_all(&output_dir).expect("create output dir");
    install_fake_orchestrate_toolchain(&bin_dir);
    install_fake_orchestrate_staging_artifacts(&target_dir);
    install_fake_orchestrate_rch_attestation(&target_dir, &output_dir);
    if extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_PRECREATE_RCH_EXTENSION_ARTIFACT" && *value == "1")
    {
        let stale_artifact = target_dir
            .join("nextest")
            .join("pi-perf")
            .join(FAKE_ORCHESTRATE_CORRELATION_ID)
            .join("perf_bench_harness")
            .join("extension_bench.jsonl");
        fs::create_dir_all(stale_artifact.parent().expect("stale artifact parent"))
            .expect("create stale RCH artifact directory");
        fs::write(&stale_artifact, "{\"stale\":true}\n")
            .expect("write stale RCH artifact negative control");
    }
    let fault_injection_summary = fault_injection_root.join("stub/integrity-summary.json");
    fs::create_dir_all(
        fault_injection_summary
            .parent()
            .expect("fault-injection summary parent"),
    )
    .expect("create fake fault-injection evidence directory");
    let fault_injection_passed = !extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_FAILED_PERSISTENCE_SUMMARY" && *value == "1");
    let fault_injection_source_commit = if extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_FOREIGN_PERSISTENCE_SOURCE" && *value == "1")
    {
        "ffffffffffffffffffffffffffffffffffffffff"
    } else {
        FAKE_ORCHESTRATE_SOURCE_COMMIT
    };
    let fault_injection_source_tree =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fs::write(
        &fault_injection_summary,
        serde_json::to_vec(&json!({
            "schema": "pi.e2e.persistence_fault_injection.summary.v1",
            "run_id": FAKE_ORCHESTRATE_CORRELATION_ID,
            "correlation_id": FAKE_ORCHESTRATE_CORRELATION_ID,
            "source_commit": fault_injection_source_commit,
            "source_dirty": false,
            "source_tree_sha256": fault_injection_source_tree,
            "source_commit_final": fault_injection_source_commit,
            "source_dirty_final": false,
            "source_tree_sha256_final": fault_injection_source_tree,
            "source_tree_stable": true,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "overall_passed": fault_injection_passed,
        }))
        .expect("encode fake fault-injection evidence"),
    )
    .expect("write fake fault-injection evidence");

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let profile = if extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_PROFILE_QUICK" && *value == "1")
    {
        "quick"
    } else {
        "full"
    };
    let omit_require_rch_cli = extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_OMIT_REQUIRE_RCH_CLI" && *value == "1");

    let mut command = Command::new("bash");
    command
        .arg("scripts/perf/orchestrate.sh")
        .arg("--profile")
        .arg(profile)
        .arg("--skip-build")
        .arg("--skip-env-check")
        .current_dir(project_root())
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("PERF_OUTPUT_DIR", &output_dir)
        .env("PERF_FAULT_INJECTION_ROOT", &fault_injection_root)
        .env(
            "PI_FAKE_PERF_BENCH_INVOCATION_MARKER",
            target_dir.join("perf-bench-invoked"),
        )
        .env(
            "PI_FAKE_GIT_REV_PARSE_STATE_FILE",
            target_dir.join("git-rev-parse-count"),
        )
        .env("CI_CORRELATION_ID", FAKE_ORCHESTRATE_CORRELATION_ID);
    if !omit_require_rch_cli {
        command.arg("--require-rch");
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    if extra_env
        .iter()
        .any(|(key, value)| *key == "PI_FAKE_PERF_ONLY" && *value == "1")
    {
        command.arg("--suite").arg("perf_bench_harness");
    }

    let output = command.output().expect("run orchestrate.sh");

    (output, temp_root)
}

#[cfg(unix)]
fn run_orchestrate_with_fake_toolchain() -> (std::process::Output, PathBuf) {
    run_orchestrate_with_fake_toolchain_with_env(&[])
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_failed_persistence_summary_for_phase5() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_FAILED_PERSISTENCE_SUMMARY",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "strict orchestration must fail when persistence fault injection reports failure"
    );

    let matrix_path = temp_root
        .join("run")
        .join("results")
        .join("phase1_matrix_validation.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    assert_eq!(
        matrix["regression_guards"]["security"].as_str(),
        Some("fail"),
        "overall_passed=false must fail the security regression guard"
    );
    assert!(
        matrix["regression_guards"]["failure_or_gap_reasons"]
            .as_array()
            .is_some_and(|reasons| {
                reasons
                    .iter()
                    .any(|reason| reason.as_str() == Some("security_regression"))
            }),
        "failed persistence evidence must name the security regression"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "failed persistence evidence must block Phase 5 readiness"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_foreign_persistence_summary_source() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_FOREIGN_PERSISTENCE_SOURCE",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "strict orchestration must reject persistence evidence from another source commit"
    );

    let matrix_path = temp_root
        .join("run")
        .join("results")
        .join("phase1_matrix_validation.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    assert_eq!(
        matrix["regression_guards"]["security"].as_str(),
        Some("missing"),
        "foreign-source persistence evidence must not be admitted as current"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "foreign-source persistence evidence must block Phase 5 readiness"
    );
    assert!(
        matrix["evidence_links"]["phase1_unit_and_fault_injection"]["fault_injection_summary_path"]
            .is_null(),
        "a rejected persistence summary must not be linked as consumed evidence"
    );
}

#[cfg(unix)]
fn assert_orchestrate_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "orchestrate.sh should succeed with stub toolchain. stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_retrieves_nextest_artifact() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain();
    assert_orchestrate_success(&output);

    let artifact_path = temp_root
        .join("run")
        .join("results")
        .join("perf_bench_harness")
        .join("extension_bench.jsonl");
    let artifact = fs::read_to_string(&artifact_path)
        .expect("RCH perf harness artifact must be copied from target/nextest");
    let first_row: Value = serde_json::from_str(
        artifact
            .lines()
            .next()
            .expect("retrieved extension benchmark artifact must be non-empty"),
    )
    .expect("retrieved extension benchmark artifact must contain JSONL");
    assert_eq!(first_row["schema"].as_str(), Some("pi.ext.rust_bench.v1"));
    assert_eq!(
        first_row["env"]["git_commit"].as_str(),
        Some(FAKE_ORCHESTRATE_SOURCE_COMMIT),
        "retrieved extension benchmark must retain the full source commit"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_default_rch_perf_harness_uses_clean_fail_closed_runner() {
    let (output, _temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_OMIT_REQUIRE_RCH_CLI", "1"),
        ("PI_FAKE_RCH_CHECK_OK", "1"),
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
    ]);
    assert_orchestrate_success(&output);
}

#[cfg(unix)]
#[test]
fn orchestrate_env_only_remote_requirement_selects_clean_runner() {
    let (output, _temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_OMIT_REQUIRE_RCH_CLI", "1"),
        ("RCH_REQUIRE_REMOTE", "1"),
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
    ]);
    assert_orchestrate_success(&output);
}

#[cfg(unix)]
#[test]
fn orchestrate_env_only_remote_requirement_rejects_local_runner() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_OMIT_REQUIRE_RCH_CLI", "1"),
        ("RCH_REQUIRE_REMOTE", "1"),
        ("PERF_CARGO_RUNNER", "local"),
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
    ]);
    assert!(
        !output.status.success(),
        "environment-only remote proof must reject an explicitly local runner"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH proof mode requires PERF_CARGO_RUNNER=rch"),
        "local proof-mode rejection must name the required runner: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "local proof-mode rejection must happen before benchmark invocation"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_env_only_remote_requirement_rejects_auto_runner() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_OMIT_REQUIRE_RCH_CLI", "1"),
        ("RCH_REQUIRE_REMOTE", "1"),
        ("PERF_CARGO_RUNNER", "auto"),
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
    ]);
    assert!(
        !output.status.success(),
        "environment-only remote proof must reject an auto runner that could fall back locally"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH proof mode requires PERF_CARGO_RUNNER=rch"),
        "auto proof-mode rejection must name the required runner: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "auto proof-mode rejection must happen before benchmark invocation"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_explicit_quick_suite_forwards_benchmark_controls() {
    let (output, _temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
        ("PI_FAKE_REQUIRE_BENCH_CONTROLS", "1"),
        ("BENCH_ITERATIONS", "1"),
    ]);
    assert_orchestrate_success(&output);
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_non_positive_build_jobs_before_remote_invocation() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PERF_BUILD_JOBS", "0"),
    ]);
    assert!(
        !output.status.success(),
        "zero build parallelism must fail before RCH invocation"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("PERF_BUILD_JOBS must be a positive integer"),
        "invalid build-jobs failure must name the bad control: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "invalid build jobs must be rejected before benchmark invocation"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_path_unsafe_correlation_id_before_remote_invocation() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("CI_CORRELATION_ID", "../escaped-run"),
    ]);
    assert!(
        !output.status.success(),
        "a correlation ID used in artifact paths must reject traversal"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("CI_CORRELATION_ID must be 1-128 path-safe characters"),
        "unsafe correlation ID failure must name the path-safety contract: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "unsafe correlation IDs must fail before benchmark invocation"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_non_perf_rch_extension_profile_before_invocation() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PERF_PROFILE", "release"),
    ]);
    assert!(
        !output.status.success(),
        "the exact RCH extension proof must reject a non-perf Cargo profile"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("requires PERF_PROFILE=perf"),
        "profile mismatch must fail with an actionable diagnostic: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "profile mismatch must fail before benchmark invocation"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_rch_local_fallback_marker() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_RCH_LOCAL_FALLBACK", "1")]);
    assert!(
        !output.status.success(),
        "local fallback must not satisfy remote benchmark acceptance"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("no remote-success marker")
            || combined.contains("reported local execution"),
        "local fallback rejection must name the missing remote proof: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "local fallback output must not enter the accepted result directory"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_stale_benchmark_invocation_id() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_STALE_RCH_EXTENSION_ARTIFACT",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "artifact from another benchmark invocation must fail freshness admission"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("benchmark_run_id does not match current invocation"),
        "stale invocation rejection must identify benchmark_run_id: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "stale invocation output must not enter the accepted result directory"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_unbound_benchmark_config_hash() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_INVALID_RCH_EXTENSION_CONFIG_HASH",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "self-asserted provenance with an invalid config hash must fail admission"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("config_hash does not bind provenance fields"),
        "invalid provenance hash rejection must name config_hash: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "unbound provenance output must not enter the accepted result directory"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_benchmark_binary_from_non_perf_path() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_WRONG_RCH_EXTENSION_BINARY_PROFILE",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "a self-consistent record must not relabel a release-path executable as perf"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("binary path does not identify profile perf"),
        "binary path/profile mismatch must be diagnosed independently of config_hash: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "wrong-profile executable evidence must not enter the accepted result directory"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_fails_when_nextest_artifact_is_missing() {
    let (output, _temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_DROP_RCH_EXTENSION_ARTIFACT",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must fail when nextest returns without the JSONL artifact"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("without retrieving extension_bench.jsonl"),
        "missing-artifact failure must name the failed RCH writeback postcondition: {combined}"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_incomplete_scenario_coverage() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_DROP_RCH_EXTENSION_COVERAGE", "1"),
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_PROFILE_QUICK", "1"),
    ]);
    assert!(
        !output.status.success(),
        "a benchmark artifact missing required event-hook coverage must fail admission"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("extension benchmark coverage mismatch"),
        "incomplete coverage rejection must identify the exact matrix mismatch: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "incomplete benchmark coverage must not enter accepted results"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_refuses_preexisting_nextest_artifact() {
    let (output, _temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_PRECREATE_RCH_EXTENSION_ARTIFACT",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must not credit a preexisting JSONL artifact"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Refusing stale RCH extension benchmark artifact"),
        "stale-artifact failure must identify the freshness postcondition: {combined}"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_wrong_source_commit() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_WRONG_RCH_EXTENSION_COMMIT",
        "1",
    )]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject an extension benchmark from another commit"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("git commit does not match")
            && combined.contains("RCH retrieved an invalid extension_bench.jsonl"),
        "wrong-commit failure must identify both the lineage mismatch and rejected artifact: {combined}"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "a wrong-commit extension benchmark must not enter the accepted result directory"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_unknown_source_commit() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_GIT_IDENTITY_UNAVAILABLE", "1")]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject an unavailable Git commit identity"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("requires a full Git commit identity, got: unknown"),
        "unknown source identity must fail before the remote benchmark starts: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "unknown source identity must not invoke the remote benchmark"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_dirty_source_tree() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_GIT_DIRTY", "1")]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject a dirty source tree"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("requires a clean source tree"),
        "dirty source identity must fail before the remote benchmark starts: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "dirty source identity must not invoke the remote benchmark"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_unavailable_git_status() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_GIT_STATUS_UNAVAILABLE", "1")]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject an unavailable Git status"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("requires an available Git status"),
        "unavailable Git status must fail before the remote benchmark starts: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "unavailable Git status must not invoke the remote benchmark"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_head_drift_before_invocation() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_GIT_DRIFT_FROM_REV_PARSE_CALL", "4"),
    ]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject HEAD drift before benchmark invocation"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH extension benchmark precondition: Git HEAD drifted"),
        "pre-invocation HEAD drift must trip the immediate source fence: {combined}"
    );
    assert!(
        !temp_root.join("target/perf-bench-invoked").exists(),
        "pre-invocation HEAD drift must not invoke the remote benchmark"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rch_perf_harness_rejects_head_drift_after_invocation() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[
        ("PI_FAKE_PERF_ONLY", "1"),
        ("PI_FAKE_GIT_DRIFT_FROM_REV_PARSE_CALL", "6"),
    ]);
    assert!(
        !output.status.success(),
        "strict RCH orchestration must reject HEAD drift after benchmark invocation"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH extension benchmark postcondition: Git HEAD drifted"),
        "post-invocation HEAD drift must trip the immediate source fence: {combined}"
    );
    assert!(
        temp_root.join("target/perf-bench-invoked").is_file(),
        "post-invocation control must prove the remote benchmark actually ran"
    );
    assert!(
        !temp_root
            .join("run/results/perf_bench_harness/extension_bench.jsonl")
            .exists(),
        "post-invocation HEAD drift must prevent the JSONL from entering accepted results"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_head_drift_before_post_generation_staging() {
    let (output, _) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_GIT_DRIFT_AFTER_OUTPUT_RELATIVE",
        "results/phase1_matrix_validation.json",
    )]);
    assert!(!output.status.success());
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH post-generation staging precondition: Git HEAD drifted"),
        "late pre-staging source drift must fail before packaging evidence: {combined}"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_head_drift_before_checksums() {
    let (output, _) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_GIT_DRIFT_AFTER_OUTPUT_RELATIVE",
        "results/perf_budget_preflight.json",
    )]);
    assert!(!output.status.success());
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH checksum precondition: Git HEAD drifted"),
        "source drift after evidence consumption must fail before checksums: {combined}"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_head_drift_before_final_success() {
    let (output, _) = run_orchestrate_with_fake_toolchain_with_env(&[(
        "PI_FAKE_GIT_DRIFT_AFTER_OUTPUT_RELATIVE",
        "checksums.sha256",
    )]);
    assert!(!output.status.success());
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("RCH final-success precondition: Git HEAD drifted"),
        "source drift after checksum generation must prevent a green result: {combined}"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_generates_extension_stratification_artifact() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain();
    assert_orchestrate_success(&output);

    let output_dir = temp_root.join("run");
    let manifest_path = output_dir.join("manifest.json");
    let stratification_path = output_dir
        .join("results")
        .join("extension_benchmark_stratification.json");

    assert!(
        stratification_path.exists(),
        "stratification artifact must be written: {}",
        stratification_path.display()
    );
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest.json"))
            .expect("parse manifest.json");
    let stratification: Value = serde_json::from_str(
        &fs::read_to_string(&stratification_path)
            .expect("read extension_benchmark_stratification.json"),
    )
    .expect("parse extension_benchmark_stratification.json");

    if let Err(err) = validate_extension_stratification_record(&stratification) {
        panic!("stratification artifact violates schema contract: {err}");
    }

    assert_eq!(
        stratification.get("schema").and_then(Value::as_str),
        Some(EXT_STRATIFICATION_SCHEMA)
    );
    assert_eq!(
        stratification.get("run_id").and_then(Value::as_str),
        manifest.get("timestamp").and_then(Value::as_str),
        "stratification run_id must match manifest timestamp"
    );
    assert_eq!(
        stratification.get("correlation_id").and_then(Value::as_str),
        manifest.get("correlation_id").and_then(Value::as_str),
        "stratification correlation_id must match manifest"
    );
    let run_id_lineage = stratification["lineage"]["run_id_lineage"]
        .as_array()
        .expect("lineage.run_id_lineage array");
    assert_eq!(
        run_id_lineage[0].as_str(),
        manifest.get("timestamp").and_then(Value::as_str),
        "lineage[0] must be manifest timestamp"
    );
    assert_eq!(
        run_id_lineage[1].as_str(),
        manifest.get("correlation_id").and_then(Value::as_str),
        "lineage[1] must be manifest correlation_id"
    );

    assert_eq!(
        manifest["extension_benchmark_stratification"]["schema"].as_str(),
        Some(EXT_STRATIFICATION_SCHEMA),
        "manifest must reference stratification schema"
    );
    assert!(
        stratification["claim_integrity"]["anti_conflation"]
            ["cold_load_wins_do_not_imply_per_call_or_e2e"]
            .as_bool()
            .is_some_and(|v| v),
        "anti-conflation guardrail must be explicit in claim_integrity"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn orchestrate_generates_phase1_matrix_validation_artifact() {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain();
    assert_orchestrate_success(&output);

    let output_dir = temp_root.join("run");
    let manifest_path = output_dir.join("manifest.json");
    let matrix_path = output_dir
        .join("results")
        .join("phase1_matrix_validation.json");

    assert!(
        matrix_path.exists(),
        "phase-1 matrix artifact must be written: {}",
        matrix_path.display()
    );

    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest.json"))
            .expect("parse manifest.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    if let Err(err) = validate_phase1_matrix_validation_record(&matrix) {
        panic!("phase1 matrix artifact violates schema contract: {err}");
    }

    assert_eq!(
        matrix.get("schema").and_then(Value::as_str),
        Some(PHASE1_MATRIX_SCHEMA)
    );
    assert_eq!(
        matrix.get("run_id").and_then(Value::as_str),
        manifest.get("timestamp").and_then(Value::as_str),
        "phase1 run_id must match manifest timestamp"
    );
    assert_eq!(
        matrix.get("correlation_id").and_then(Value::as_str),
        manifest.get("correlation_id").and_then(Value::as_str),
        "phase1 correlation_id must match manifest"
    );
    assert_eq!(
        matrix["evidence_links"]["source_identity"]["run_id"].as_str(),
        matrix.get("run_id").and_then(Value::as_str),
        "evidence_links.source_identity.run_id must match top-level run_id"
    );
    assert_eq!(
        matrix["evidence_links"]["source_identity"]["correlation_id"].as_str(),
        matrix.get("correlation_id").and_then(Value::as_str),
        "evidence_links.source_identity.correlation_id must match top-level correlation_id"
    );
    assert_eq!(
        matrix["lineage"]["source_scenario_runner_path"].as_str(),
        matrix["evidence_links"]["required_artifacts"]["scenario_runner"].as_str(),
        "lineage scenario_runner path must match required_artifacts.scenario_runner"
    );
    assert_eq!(
        matrix["lineage"]["source_workload_path"].as_str(),
        matrix["evidence_links"]["required_artifacts"]["workload"].as_str(),
        "lineage workload path must match required_artifacts.workload"
    );
    assert_eq!(
        matrix["lineage"]["source_stratification_path"].as_str(),
        matrix["evidence_links"]["required_artifacts"]["stratification"].as_str(),
        "lineage stratification path must match required_artifacts.stratification"
    );
    assert_eq!(
        matrix["lineage"]["source_baseline_confidence_path"].as_str(),
        matrix["evidence_links"]["required_artifacts"]["baseline_variance_confidence"].as_str(),
        "lineage baseline path must match required_artifacts.baseline_variance_confidence"
    );
    let perf_sli_contract_path = matrix["lineage"]["source_perf_sli_contract_path"]
        .as_str()
        .expect("lineage.source_perf_sli_contract_path string");
    assert!(
        perf_sli_contract_path.ends_with("docs/perf_sli_matrix.json"),
        "lineage must include canonical perf_sli contract path, got: {perf_sli_contract_path}"
    );
    let regression_guards = matrix["regression_guards"]
        .as_object()
        .expect("regression_guards object");
    let failure_or_gap_reasons = regression_guards
        .get("failure_or_gap_reasons")
        .and_then(Value::as_array)
        .expect("regression_guards.failure_or_gap_reasons array");
    let mut reason_set = HashSet::new();
    for reason in failure_or_gap_reasons {
        let reason = reason
            .as_str()
            .expect("regression_guards.failure_or_gap_reasons entries must be strings")
            .to_string();
        assert!(
            reason_set.insert(reason.clone()),
            "regression_guards.failure_or_gap_reasons must not contain duplicates: {reason}"
        );
    }
    for guard_name in ["memory", "correctness", "security"] {
        let status = regression_guards
            .get(guard_name)
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            matches!(status, "pass" | "fail" | "missing"),
            "regression_guards.{guard_name} must be pass/fail/missing, got: {status}"
        );
        let fail_reason = format!("{guard_name}_regression");
        let unverified_reason = format!("{guard_name}_regression_unverified");
        let has_fail_reason = reason_set.contains(&fail_reason);
        let has_unverified_reason = reason_set.contains(&unverified_reason);
        match status {
            "pass" => {
                assert!(
                    !has_fail_reason && !has_unverified_reason,
                    "regression_guards.{guard_name}=pass must not emit {fail_reason} or {unverified_reason}"
                );
            }
            "fail" => {
                assert!(
                    has_fail_reason && !has_unverified_reason,
                    "regression_guards.{guard_name}=fail must emit {fail_reason} (without {unverified_reason})"
                );
            }
            "missing" => {
                assert!(
                    has_unverified_reason && !has_fail_reason,
                    "regression_guards.{guard_name}=missing must emit {unverified_reason} (without {fail_reason})"
                );
            }
            _ => {}
        }
    }
    let artifact_ready_for_phase5 = matrix["consumption_contract"]["artifact_ready_for_phase5"]
        .as_bool()
        .expect("consumption_contract.artifact_ready_for_phase5 bool");
    let expected_artifact_ready_for_phase5 = matrix["primary_outcomes"]["status"]
        .as_str()
        .is_some_and(|status| status == "pass")
        && matrix["stage_summary"]["cells_missing_stage_breakdown"]
            .as_u64()
            .is_some_and(|value| value == 0)
        && matrix["stage_summary"]["cells_with_complete_stage_breakdown"].as_u64()
            == matrix["matrix_requirements"]["required_cell_count"].as_u64()
        && matrix["swarm_summary"]["cells_missing_swarm_metrics"]
            .as_u64()
            .is_some_and(|value| value == 0)
        && matrix["swarm_summary"]["cells_with_complete_swarm_metrics"].as_u64()
            == matrix["matrix_requirements"]["required_cell_count"].as_u64()
        && ["memory", "correctness", "security"]
            .into_iter()
            .all(|guard_name| {
                matrix["regression_guards"][guard_name]
                    .as_str()
                    .is_some_and(|status| status == "pass")
            });
    assert_eq!(
        artifact_ready_for_phase5, expected_artifact_ready_for_phase5,
        "consumption_contract.artifact_ready_for_phase5 must match deterministic readiness prerequisites"
    );
    let downstream_beads = matrix["consumption_contract"]["downstream_beads"]
        .as_array()
        .expect("consumption_contract.downstream_beads array");
    let downstream_bead_set: HashSet<&str> =
        downstream_beads.iter().filter_map(Value::as_str).collect();
    for required_bead in ["bd-3ar8v.6.1", "bd-3ar8v.6.2"] {
        assert!(
            downstream_bead_set.contains(required_bead),
            "consumption_contract.downstream_beads must include {required_bead}"
        );
    }
    let downstream_consumers = matrix["consumption_contract"]["downstream_consumers"]
        .as_object()
        .expect("consumption_contract.downstream_consumers object");
    for (consumer_name, expected_bead_id, expected_selector) in [
        (
            "opportunity_matrix",
            "bd-3ar8v.6.1",
            "weighted_bottleneck_attribution.global_ranking",
        ),
        (
            "parameter_sweeps",
            "bd-3ar8v.6.2",
            "weighted_bottleneck_attribution.per_scale",
        ),
    ] {
        let consumer = downstream_consumers
            .get(consumer_name)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("downstream consumer entry missing: {consumer_name}"));
        assert_eq!(
            consumer.get("bead_id").and_then(Value::as_str),
            Some(expected_bead_id),
            "downstream consumer bead_id mismatch for {consumer_name}"
        );
        assert_eq!(
            consumer.get("selector").and_then(Value::as_str),
            Some(expected_selector),
            "downstream consumer selector mismatch for {consumer_name}"
        );
        assert_eq!(
            consumer.get("source_artifact").and_then(Value::as_str),
            Some("phase1_matrix_validation"),
            "downstream consumer source_artifact mismatch for {consumer_name}"
        );
    }

    assert_eq!(
        manifest["phase1_matrix_validation"]["schema"].as_str(),
        Some(PHASE1_MATRIX_SCHEMA),
        "manifest must reference phase1 matrix schema"
    );
    assert_eq!(
        matrix["matrix_requirements"]["required_cell_count"].as_u64(),
        Some(10),
        "phase1 matrix should require 10 partition/size cells"
    );
    assert_eq!(
        matrix["stage_summary"]["cells_with_complete_stage_breakdown"].as_u64(),
        Some(10),
        "stub matrix should provide complete open/append/save/index attribution for every cell"
    );
    assert_eq!(
        matrix["swarm_summary"]["cells_with_complete_swarm_metrics"].as_u64(),
        Some(10),
        "stub matrix should provide complete swarm latency/resource/breakdown metrics for every cell"
    );

    let cells = matrix["matrix_cells"]
        .as_array()
        .expect("matrix_cells array");
    assert_eq!(
        cells.len(),
        10,
        "matrix artifact should contain one cell per requirement"
    );

    let seen_partitions: HashSet<&str> = cells
        .iter()
        .filter_map(|cell| cell.get("workload_partition").and_then(Value::as_str))
        .collect();
    assert!(seen_partitions.contains("matched-state"));
    assert!(seen_partitions.contains("realistic"));

    let seen_sizes: HashSet<u64> = cells
        .iter()
        .filter_map(|cell| cell.get("session_messages").and_then(Value::as_u64))
        .collect();
    assert_eq!(
        seen_sizes,
        REALISTIC_SESSION_SIZES.iter().copied().collect(),
        "phase1 matrix must cover canonical 100k..5M sizes"
    );

    let weighted = matrix["weighted_bottleneck_attribution"]
        .as_object()
        .expect("weighted_bottleneck_attribution object");
    assert_eq!(
        weighted["schema"].as_str(),
        Some("pi.perf.phase1_weighted_bottleneck_attribution.v1"),
        "weighted attribution schema must be pinned"
    );
    assert_eq!(
        weighted["status"].as_str(),
        Some("computed"),
        "stub phase1 matrix should emit computed weighted attribution"
    );
    assert_eq!(
        weighted["weighting_policy"].as_str(),
        Some("session_messages"),
        "weighted attribution must use session_messages weighting"
    );
    assert_eq!(
        weighted["confidence_method"].as_str(),
        Some("weighted_normal_approx_95"),
        "weighted attribution confidence method must be explicit"
    );
    let weighted_lineage = weighted["lineage"]
        .as_object()
        .expect("weighted lineage object");
    assert_eq!(
        weighted_lineage["source_stream"].as_str(),
        Some("phase1_matrix_validation.matrix_cells"),
        "weighted lineage source stream must point to matrix_cells"
    );
    assert_eq!(
        weighted_lineage["source_cell_count"].as_u64(),
        Some(cells.len() as u64),
        "weighted lineage source_cell_count must match matrix cell count"
    );
    assert_eq!(
        weighted_lineage["valid_cell_count"].as_u64(),
        Some(cells.len() as u64),
        "stub matrix should have all cells counted as valid for weighted attribution"
    );

    let per_scale = weighted["per_scale"]
        .as_array()
        .expect("weighted per_scale array");
    assert_eq!(
        per_scale.len(),
        REALISTIC_SESSION_SIZES.len(),
        "weighted per_scale must cover every required session size"
    );
    let observed_per_scale_sizes: HashSet<u64> = per_scale
        .iter()
        .filter_map(|row| row.get("session_messages").and_then(Value::as_u64))
        .collect();
    assert_eq!(
        observed_per_scale_sizes,
        REALISTIC_SESSION_SIZES.iter().copied().collect(),
        "weighted per_scale session_messages must match canonical size set"
    );
    for row in per_scale {
        let partitions = row["partitions"]
            .as_array()
            .expect("per_scale.partitions array");
        assert_eq!(
            partitions.len(),
            2,
            "weighted per_scale rows must include matched-state + realistic partitions"
        );
        let partition_set: HashSet<&str> = partitions
            .iter()
            .filter_map(|entry| entry.get("workload_partition").and_then(Value::as_str))
            .collect();
        assert!(partition_set.contains("matched-state"));
        assert!(partition_set.contains("realistic"));
    }

    let global_ranking = weighted["global_ranking"]
        .as_array()
        .expect("weighted global_ranking array");
    assert_eq!(
        global_ranking.len(),
        4,
        "weighted global_ranking must include open/append/save/index"
    );
    let observed_stages: HashSet<&str> = global_ranking
        .iter()
        .filter_map(|row| row.get("stage").and_then(Value::as_str))
        .collect();
    let expected_stages: HashSet<&str> = ["open_ms", "append_ms", "save_ms", "index_ms"]
        .iter()
        .copied()
        .collect();
    assert_eq!(
        observed_stages, expected_stages,
        "weighted global_ranking stage coverage must match required stage keys"
    );
    let mut previous_contribution = f64::INFINITY;
    let mut contribution_sum = 0.0_f64;
    for row in global_ranking {
        let contribution = row["weighted_contribution_pct"]
            .as_f64()
            .expect("weighted_contribution_pct number");
        let mean_share = row["mean_share_pct"]
            .as_f64()
            .expect("mean_share_pct number");
        let ci95_lower = row["ci95_lower_pct"]
            .as_f64()
            .expect("ci95_lower_pct number");
        let ci95_upper = row["ci95_upper_pct"]
            .as_f64()
            .expect("ci95_upper_pct number");
        assert!(
            ci95_lower <= mean_share + 1e-9 && mean_share <= ci95_upper + 1e-9,
            "mean_share_pct must lie within CI bounds"
        );
        assert!(
            ci95_lower <= ci95_upper + 1e-9,
            "ci95_lower_pct must be <= ci95_upper_pct"
        );
        assert_eq!(
            row["sample_size"].as_u64(),
            Some(cells.len() as u64),
            "weighted global_ranking sample_size should match valid matrix cell count in stub run"
        );
        assert!(
            contribution <= previous_contribution + 1e-9,
            "weighted global_ranking must be sorted descending by weighted_contribution_pct"
        );
        previous_contribution = contribution;
        contribution_sum += contribution;
    }
    assert!(
        (contribution_sum - 100.0).abs() <= 0.5,
        "weighted global_ranking weighted_contribution_pct values should sum to ~100, got {contribution_sum}"
    );

    let staging_path = output_dir.join("results/perf_artifact_staging_manifest.json");
    let staging: Value = serde_json::from_str(
        &fs::read_to_string(&staging_path).expect("read final artifact staging manifest"),
    )
    .expect("parse final artifact staging manifest");
    let staging_entries = staging["entries"]
        .as_array()
        .expect("artifact staging entries array");
    let source_commit = FAKE_ORCHESTRATE_SOURCE_COMMIT;
    let expected_correlation_id = manifest["correlation_id"]
        .as_str()
        .expect("manifest correlation_id");
    for (contract_id, artifact_name) in [
        (
            "extension_benchmark_stratification",
            "extension_benchmark_stratification.json",
        ),
        ("phase1_matrix_validation", "phase1_matrix_validation.json"),
    ] {
        let expected_source_path = output_dir.join("results").join(artifact_name);
        let entry = staging_entries
            .iter()
            .find(|entry| {
                entry["contract_id"].as_str() == Some(contract_id)
                    && entry["evidence_source"].as_str() == Some("direct")
                    && entry["status"].as_str() == Some("present")
                    && entry["source_path"].as_str()
                        == Some(expected_source_path.to_string_lossy().as_ref())
            })
            .unwrap_or_else(|| {
                panic!("missing direct current-run staging entry for {contract_id}")
            });
        assert_eq!(
            entry["correlation_id"].as_str(),
            Some(expected_correlation_id),
            "staged {contract_id} correlation must match the run manifest"
        );
        assert_eq!(
            entry["source_commit"].as_str(),
            Some(source_commit),
            "staged {contract_id} commit must match the source checkout"
        );
        assert_eq!(
            entry["source_dirty"].as_bool(),
            Some(false),
            "staged {contract_id} must be clean-source evidence"
        );
    }
    assert!(
        manifest["suite_results"].as_array().is_some_and(|results| {
            results.iter().any(|result| {
                result["suite"].as_str() == Some("perf_budgets_post_generation")
                    && result["status"].as_str() == Some("pass")
                    && result["exit_code"].as_i64() == Some(0)
            })
        }),
        "manifest must record a passing post-generation perf budget consumer"
    );
    let post_generation_stdout =
        fs::read_to_string(output_dir.join("results/perf_budgets_post_generation/stdout.log"))
            .expect("read post-generation budget stdout");
    let post_generation_invocation: Value = post_generation_stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| {
            value["schema"].as_str() == Some("pi.perf.fake_post_generation_invocation.v1")
        })
        .expect("post-generation stdout must prove current-evidence consumption");
    assert_eq!(
        post_generation_invocation["correlation_id"].as_str(),
        Some(expected_correlation_id),
        "post-generation budget invocation must consume the current run"
    );
    assert_eq!(
        post_generation_invocation["test_filter"].as_str(),
        Some("ci_enforced_budgets_fail_on_regression_or_missing_data"),
        "post-generation budget invocation must select the data-contract test"
    );
    assert_eq!(
        post_generation_invocation["exact"].as_bool(),
        Some(true),
        "post-generation budget invocation must use an exact test filter"
    );
    let post_generation_contract: Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("results/post_generation_evidence_contract.json"))
            .expect("read positive post-generation evidence contract"),
    )
    .expect("parse positive post-generation evidence contract");
    assert_eq!(
        post_generation_contract["status"].as_str(),
        Some("ready"),
        "current direct derived artifacts must pass the post-generation contract"
    );
    assert_eq!(
        post_generation_contract["failure_count"].as_u64(),
        Some(0),
        "current direct phase1 and stratification artifacts must have zero contract failures"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
fn assert_orchestrate_rejects_scenario_mutation(env_name: &str, expected_reason: &str) {
    let (output, temp_root) = run_orchestrate_with_fake_toolchain_with_env(&[(env_name, "1")]);
    assert!(
        !output.status.success(),
        "strict orchestration must fail when a mutated source row is present"
    );

    let results_dir = temp_root.join("run/results");
    let matrix: Value = serde_json::from_str(
        &fs::read_to_string(results_dir.join("phase1_matrix_validation.json"))
            .expect("read phase1 matrix artifact"),
    )
    .expect("parse phase1 matrix artifact");
    let scenario_dataset = matrix["source_datasets"]
        .as_array()
        .and_then(|datasets| {
            datasets.iter().find(|dataset| {
                dataset["correlation_field"].as_str() == Some("orchestration_correlation_id")
            })
        })
        .expect("phase1 scenario source dataset");
    assert!(
        scenario_dataset["accepted_record_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "the current-correlation control rows must remain admissible"
    );
    assert_eq!(
        scenario_dataset["rejected_record_count"].as_u64(),
        Some(1),
        "the single source mutation must be rejected"
    );
    assert!(
        scenario_dataset["rejections"]
            .as_array()
            .is_some_and(|rejections| {
                rejections.iter().any(|rejection| {
                    rejection["reasons"].as_array().is_some_and(|reasons| {
                        reasons
                            .iter()
                            .any(|reason| reason.as_str() == Some(expected_reason))
                    })
                })
            }),
        "scenario mutation must report its causal rejection reason: {expected_reason}"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "mixed lineage must fail the phase1 consumption contract closed"
    );

    let contract: Value = serde_json::from_str(
        &fs::read_to_string(results_dir.join("post_generation_evidence_contract.json"))
            .expect("read post-generation contract"),
    )
    .expect("parse post-generation contract");
    assert_eq!(contract["status"].as_str(), Some("blocked"));
    assert!(
        contract["failures"].as_array().is_some_and(|failures| {
            failures.iter().any(|failure| {
                failure["reason"].as_str() == Some("mixed_source_lineage")
                    && failure["source_path"]
                        .as_str()
                        .is_some_and(|path| path.ends_with("scenario_runner.jsonl"))
            })
        }),
        "post-generation validation must identify the mixed scenario lineage"
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_foreign_source_lineage_before_finalization() {
    assert_orchestrate_rejects_scenario_mutation(
        "PI_FAKE_INJECT_FOREIGN_SCENARIO_ROW",
        "correlation_id_mismatch",
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_stale_same_lineage_before_finalization() {
    assert_orchestrate_rejects_scenario_mutation(
        "PI_FAKE_INJECT_STALE_SCENARIO_ROW",
        "timestamp_before_run_start",
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_rejects_malformed_source_row_before_finalization() {
    assert_orchestrate_rejects_scenario_mutation(
        "PI_FAKE_INJECT_MALFORMED_SCENARIO_ROW",
        "invalid_json",
    );
}

#[cfg(unix)]
#[test]
fn orchestrate_phase1_matrix_treats_missing_index_as_incomplete() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_DROP_INDEX_STAGE_SAMPLE", "1")]);
    assert!(
        !output.status.success(),
        "strict orchestration must fail when a required stage sample is missing"
    );

    let matrix_path = temp_root
        .join("run")
        .join("results")
        .join("phase1_matrix_validation.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    assert_eq!(
        matrix["stage_summary"]["cells_with_complete_stage_breakdown"].as_u64(),
        Some(9),
        "missing index_ms in one cell must reduce complete-stage cell count"
    );
    assert_eq!(
        matrix["stage_summary"]["cells_missing_stage_breakdown"].as_u64(),
        Some(1),
        "missing index_ms in one cell must increase missing-stage cell count"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "phase5 readiness must fail closed when any required stage metric is missing"
    );

    let matrix_cells = matrix["matrix_cells"]
        .as_array()
        .expect("matrix_cells array");
    let has_missing_index_reason = matrix_cells.iter().any(|cell| {
        cell["stage_attribution"]["index_ms"].is_null()
            && cell["missing_reasons"].as_array().is_some_and(|reasons| {
                reasons
                    .iter()
                    .any(|reason| reason.as_str() == Some("missing_stage_metrics:index_ms"))
            })
    });
    assert!(
        has_missing_index_reason,
        "matrix_cells must record missing_stage_metrics:index_ms when index attribution is absent"
    );
    let weighted = matrix["weighted_bottleneck_attribution"]
        .as_object()
        .expect("weighted_bottleneck_attribution object");
    assert_eq!(
        weighted["status"].as_str(),
        Some("computed"),
        "weighted attribution should stay computed when at least one valid matrix cell remains"
    );
    assert_eq!(
        weighted["lineage"]["valid_cell_count"].as_u64(),
        Some(9),
        "dropping a required stage in one matrix cell should reduce weighted valid cell count by one"
    );
    let per_scale = weighted["per_scale"]
        .as_array()
        .expect("weighted_bottleneck_attribution.per_scale array");
    let affected_partition = per_scale
        .iter()
        .find(|row| row["session_messages"].as_u64() == Some(100_000))
        .and_then(|row| row["partitions"].as_array())
        .and_then(|partitions| {
            partitions
                .iter()
                .find(|partition| partition["workload_partition"].as_str() == Some("matched-state"))
        })
        .expect("must locate matched-state/session_100000 partition");
    assert_eq!(
        affected_partition["present"].as_bool(),
        Some(false),
        "dropped-stage partition should be excluded from weighted attribution per_scale output"
    );
    assert!(
        affected_partition["total_stage_ms"].is_null(),
        "present=false per-scale partition should not include total_stage_ms"
    );
    for stage in ["open_ms", "append_ms", "save_ms", "index_ms"] {
        assert!(
            affected_partition["stage_pct"][stage].is_null(),
            "present=false per-scale partition should emit null stage_pct for {stage}"
        );
    }
    let global_ranking = weighted["global_ranking"]
        .as_array()
        .expect("weighted_bottleneck_attribution.global_ranking array");
    for row in global_ranking {
        assert_eq!(
            row["sample_size"].as_u64(),
            Some(9),
            "weighted global ranking sample_size should track valid cell count"
        );
    }
    let missing_cells = matrix["stage_summary"]["missing_cells"]
        .as_array()
        .expect("stage_summary.missing_cells array");
    let observed_missing_index_keys: HashSet<(String, u64)> = matrix_cells
        .iter()
        .filter(|cell| {
            cell["stage_attribution"]["index_ms"].is_null()
                && cell["missing_reasons"].as_array().is_some_and(|reasons| {
                    reasons
                        .iter()
                        .any(|reason| reason.as_str() == Some("missing_stage_metrics:index_ms"))
                })
        })
        .filter_map(|cell| {
            Some((
                cell.get("workload_partition")?.as_str()?.to_string(),
                cell.get("session_messages")?.as_u64()?,
            ))
        })
        .collect();
    let summary_missing_index_keys: HashSet<(String, u64)> = missing_cells
        .iter()
        .filter(|cell| {
            cell["reasons"].as_array().is_some_and(|reasons| {
                reasons
                    .iter()
                    .any(|reason| reason.as_str() == Some("missing_stage_metrics:index_ms"))
            })
        })
        .filter_map(|cell| {
            Some((
                cell.get("workload_partition")?.as_str()?.to_string(),
                cell.get("session_messages")?.as_u64()?,
            ))
        })
        .collect();
    assert_eq!(
        summary_missing_index_keys, observed_missing_index_keys,
        "stage_summary.missing_cells must include the same partition-size keys that matrix_cells mark with missing_stage_metrics:index_ms"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn orchestrate_phase1_matrix_treats_missing_swarm_metrics_as_incomplete() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_DROP_SWARM_METRICS", "1")]);
    assert!(
        !output.status.success(),
        "strict orchestration must fail when required swarm metrics are missing"
    );

    let matrix_path = temp_root
        .join("run")
        .join("results")
        .join("phase1_matrix_validation.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    if let Err(err) = validate_phase1_matrix_validation_record(&matrix) {
        panic!("missing-swarm phase1 matrix artifact violates schema contract: {err}");
    }

    assert_eq!(
        matrix["swarm_summary"]["cells_with_complete_swarm_metrics"].as_u64(),
        Some(0),
        "dropping swarm metrics should leave zero complete swarm metric cells"
    );
    assert_eq!(
        matrix["swarm_summary"]["cells_missing_swarm_metrics"].as_u64(),
        Some(10),
        "dropping swarm metrics should mark every required cell as missing"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "phase5 readiness must fail closed when required swarm metrics are missing"
    );
    let matrix_cells = matrix["matrix_cells"]
        .as_array()
        .expect("matrix_cells array");
    assert!(
        matrix_cells.iter().all(|cell| {
            cell["missing_reasons"].as_array().is_some_and(|reasons| {
                reasons.iter().any(|reason| {
                    reason
                        .as_str()
                        .is_some_and(|value| value.starts_with(SWARM_FAIL_CLOSED_REASON_PREFIX))
                })
            })
        }),
        "every cell must carry a missing_swarm_metrics reason when source swarm telemetry is absent"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn orchestrate_phase1_weighted_attribution_missing_when_no_stage_cells_are_valid() {
    let (output, temp_root) =
        run_orchestrate_with_fake_toolchain_with_env(&[("PI_FAKE_DROP_ALL_STAGE_SAMPLES", "1")]);
    assert!(
        !output.status.success(),
        "strict orchestration must fail when every stage attribution is missing"
    );

    let matrix_path = temp_root
        .join("run")
        .join("results")
        .join("phase1_matrix_validation.json");
    let matrix: Value =
        serde_json::from_str(&fs::read_to_string(&matrix_path).expect("read matrix artifact"))
            .expect("parse matrix artifact");

    assert_eq!(
        matrix["stage_summary"]["cells_with_complete_stage_breakdown"].as_u64(),
        Some(0),
        "dropping all stage metrics should leave zero complete stage-attribution cells"
    );
    assert_eq!(
        matrix["stage_summary"]["cells_missing_stage_breakdown"].as_u64(),
        Some(10),
        "dropping all stage metrics should mark every required cell as missing"
    );
    let weighted = matrix["weighted_bottleneck_attribution"]
        .as_object()
        .expect("weighted_bottleneck_attribution object");
    assert_eq!(
        weighted["status"].as_str(),
        Some("missing"),
        "weighted attribution should fail closed when no pass cells have stage totals"
    );
    assert_eq!(
        weighted["reason"].as_str(),
        Some("no_pass_cells_with_stage_totals"),
        "weighted attribution missing status should include explicit reason"
    );
    assert_eq!(
        weighted["lineage"]["source_cell_count"].as_u64(),
        Some(10),
        "weighted lineage source_cell_count should still track observed matrix cells"
    );
    assert_eq!(
        weighted["lineage"]["valid_cell_count"].as_u64(),
        Some(0),
        "weighted lineage valid_cell_count should be zero when all stage metrics are absent"
    );
    assert_eq!(
        weighted["per_scale"].as_array().map(Vec::len),
        Some(0),
        "missing weighted attribution should emit empty per_scale"
    );
    assert_eq!(
        weighted["global_ranking"].as_array().map(Vec::len),
        Some(0),
        "missing weighted attribution should emit empty global_ranking"
    );
    assert_eq!(
        matrix["consumption_contract"]["artifact_ready_for_phase5"].as_bool(),
        Some(false),
        "phase5 readiness must fail closed when weighted attribution has no valid source cells"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn validate_rust_bench_schema() {
    let root = project_root();
    let events = read_jsonl_file_or_panic(&root.join("target/perf/scenario_runner.jsonl"));
    if events.is_empty() {
        eprintln!("[schema] No scenario_runner.jsonl data — skipping");
        return;
    }

    for event in &events {
        let missing = has_required_fields(event, RUST_BENCH_REQUIRED);
        assert!(
            missing.is_empty(),
            "rust bench event missing required fields: {missing:?}"
        );
        assert_eq!(
            event.get("schema").and_then(Value::as_str),
            Some("pi.ext.rust_bench.v1"),
            "rust bench should use pi.ext.rust_bench.v1 schema"
        );
    }
    eprintln!("[schema] Validated {} rust bench events", events.len());
}

#[test]
fn validate_workload_schema() {
    let root = project_root();
    let Some((source, events)) =
        load_selected_pijs_schema_artifact(&root).unwrap_or_else(|err| panic!("{err}"))
    else {
        eprintln!("[schema] No pijs_workload.jsonl data — skipping");
        return;
    };

    for event in &events {
        if let Err(err) = validate_workload_record(event) {
            panic!("invalid workload event in {}: {err}", source.display());
        }
    }
    eprintln!("[schema] Validated {} pijs_workload events", events.len());
}

#[test]
fn validate_legacy_bench_schema() {
    let root = project_root();
    let events =
        read_jsonl_file_or_panic(&root.join("target/perf/legacy_extension_workloads.jsonl"));
    if events.is_empty() {
        eprintln!("[schema] No legacy benchmark data — skipping");
        return;
    }

    for event in &events {
        let missing = has_required_fields(event, LEGACY_BENCH_REQUIRED);
        assert!(
            missing.is_empty(),
            "legacy bench event missing required fields: {missing:?}"
        );
        assert_eq!(
            event.get("schema").and_then(Value::as_str),
            Some("pi.ext.legacy_bench.v1"),
            "legacy bench should use pi.ext.legacy_bench.v1 schema"
        );
    }
    eprintln!("[schema] Validated {} legacy bench events", events.len());
}

#[test]
fn validate_budget_events_schema() {
    let root = project_root();
    let events = read_jsonl_file_or_panic(&root.join("tests/perf/reports/budget_events.jsonl"));
    if events.is_empty() {
        eprintln!("[schema] No budget events — skipping");
        return;
    }

    let budget_required = &[
        "budget_name",
        "category",
        "threshold",
        "unit",
        "status",
        "source",
    ];

    for event in &events {
        let missing = has_required_fields(event, budget_required);
        assert!(
            missing.is_empty(),
            "budget event missing required fields: {missing:?}"
        );
    }
    eprintln!("[schema] Validated {} budget events", events.len());
}

#[test]
fn validate_conformance_events_schema() {
    let root = project_root();
    let events = read_jsonl_file_or_panic(
        &root.join("tests/ext_conformance/reports/conformance_events.jsonl"),
    );
    if events.is_empty() {
        eprintln!("[schema] No conformance events — skipping");
        return;
    }

    let required = &[
        "schema",
        "extension_id",
        "source_tier",
        "conformance_tier",
        "overall_status",
    ];

    for event in &events {
        let missing = has_required_fields(event, required);
        assert!(
            missing.is_empty(),
            "conformance event missing required fields: {missing:?}"
        );
    }
    eprintln!("[schema] Validated {} conformance events", events.len());
}

#[test]
fn validate_scenario_runner_protocol_contract() {
    let root = project_root();
    let events = read_jsonl_file_or_panic(&root.join("target/perf/scenario_runner.jsonl"));
    if events.is_empty() {
        eprintln!("[schema] No scenario_runner.jsonl data — skipping");
        return;
    }

    for (index, event) in events.iter().enumerate() {
        if let Err(err) = validate_protocol_record(event) {
            panic!("scenario_runner record {index} violates protocol contract: {err}");
        }
    }
    eprintln!(
        "[schema] Validated benchmark protocol contract on {} scenario_runner records",
        events.len()
    );
}

#[test]
fn jsonl_records_have_stable_key_ordering() {
    let root = project_root();

    // Check that legacy bench records have deterministic key ordering
    let events =
        read_jsonl_file_or_panic(&root.join("target/perf/legacy_extension_workloads.jsonl"));
    if !events.is_empty() {
        // All records with same schema should have same top-level key set
        let first_keys: Vec<String> = events[0]
            .as_object()
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();

        for (i, event) in events.iter().enumerate() {
            if let Some(obj) = event.as_object() {
                // Same scenario records should have same structure
                if event.get("scenario") == events[0].get("scenario") {
                    assert_eq!(
                        obj.keys().count(),
                        first_keys.len(),
                        "record {i} has different key count than record 0"
                    );
                }
            }
        }
        eprintln!(
            "[schema] Key ordering stable across {} legacy events",
            events.len()
        );
    }

    // Check workload records
    let events = load_selected_pijs_schema_artifact(&root)
        .unwrap_or_else(|err| panic!("{err}"))
        .map_or_else(Vec::new, |(_, events)| events);
    if events.len() >= 2 {
        let keys_0: Vec<String> = events[0]
            .as_object()
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();
        let keys_1: Vec<String> = events[1]
            .as_object()
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();
        assert_eq!(keys_0, keys_1, "workload records should have same key set");
        eprintln!(
            "[schema] Key ordering stable across {} workload events",
            events.len()
        );
    }
}

#[test]
fn generate_schema_doc() {
    if !schema_doc_generation_requested() {
        eprintln!(
            "[schema] Documentation generation skipped; set PI_GENERATE_BENCH_SCHEMA_DOCS=1 to write tracked schema files"
        );
        return;
    }
    let root = project_root();
    let reports_dir = root.join("tests/perf/reports");
    let _ = std::fs::create_dir_all(&reports_dir);

    let mut md = String::with_capacity(8 * 1024);

    md.push_str("# Benchmark JSONL Schema Reference\n\n");
    md.push_str("> Auto-generated. Do not edit manually.\n\n");

    // Schema registry
    md.push_str("## Registered Schemas\n\n");
    md.push_str("| Schema | Description |\n");
    md.push_str("|---|---|\n");
    for (name, desc) in SCHEMAS {
        let _ = writeln!(md, "| `{name}` | {desc} |");
    }
    md.push('\n');

    // Environment fingerprint
    md.push_str("## Environment Fingerprint\n\n");
    md.push_str("Every benchmark record SHOULD include an `env` object with:\n\n");
    md.push_str("| Field | Type | Description |\n");
    md.push_str("|---|---|---|\n");
    for (name, desc) in ENV_FINGERPRINT_FIELDS {
        let typ = match *name {
            "cpu_cores" | "mem_total_mb" => "integer",
            "features" => "string[]",
            _ => "string",
        };
        let _ = writeln!(md, "| `{name}` | {typ} | {desc} |");
    }
    md.push('\n');

    // Per-schema required fields
    md.push_str("## Required Fields by Schema\n\n");

    md.push_str("### `pi.ext.rust_bench.v1`\n\n");
    md.push_str("| Field | Type | Description |\n");
    md.push_str("|---|---|---|\n");
    md.push_str("| `schema` | string | Always `\"pi.ext.rust_bench.v1\"` |\n");
    md.push_str("| `runtime` | string | Always `\"pi_agent_rust\"` |\n");
    md.push_str(
        "| `scenario` | string | Benchmark scenario (e.g., `ext_load_init/load_init_cold`) |\n",
    );
    md.push_str("| `extension` | string | Extension ID being benchmarked |\n");
    md.push_str("| `runs` | integer | Number of runs (load scenarios) |\n");
    md.push_str("| `iterations` | integer | Number of iterations (throughput scenarios) |\n");
    md.push_str("| `summary` | object | `{count, min_ms, p50_ms, p95_ms, p99_ms, max_ms}` |\n");
    md.push_str("| `elapsed_ms` | float | Total elapsed time in milliseconds |\n");
    md.push_str("| `per_call_us` | float | Per-call latency in microseconds |\n");
    md.push_str("| `calls_per_sec` | float | Throughput (calls per second) |\n\n");

    md.push_str("### `pi.ext.legacy_bench.v1`\n\n");
    md.push_str("Same structure as `pi.ext.rust_bench.v1` with:\n");
    md.push_str("- `runtime` = `\"legacy_pi_mono\"`\n");
    md.push_str("- `node` object: `{version, platform, arch}`\n\n");

    md.push_str("### `pi.perf.workload.v1`\n\n");
    md.push_str("| Field | Type | Description |\n");
    md.push_str("|---|---|---|\n");
    for field in WORKLOAD_REQUIRED {
        let (typ, desc) = match *field {
            "scenario" => ("string", "Workload scenario name"),
            "iterations" => ("integer", "Number of outer iterations"),
            "tool_calls_per_iteration" => ("integer", "Tool calls per iteration"),
            "total_calls" => ("integer", "Total tool calls executed"),
            "elapsed_ms" => ("number", "Total elapsed milliseconds"),
            "per_call_us" => ("number", "Per-call latency in microseconds"),
            "calls_per_sec" => ("number", "Throughput (calls per second)"),
            _ => ("unknown", ""),
        };
        let _ = writeln!(md, "| `{field}` | {typ} | {desc} |");
    }
    md.push('\n');

    md.push_str("### `pi.perf.budget_summary.v2`\n\n");
    md.push_str(
        "Each `budgets` entry requires `name`, `category`, `metric`, `unit`, `threshold`, `comparison`, `ci_enforced`, and `methodology`. `comparison` is the exact enum `maximum` (`actual <= threshold`) or `minimum` (`actual >= threshold`); consumers must never infer direction from a budget name. Blanket performance claims are authorized only when strict, source-bound, same-run evidence gives every declared budget data and PASS status with zero data-contract failures; aggregate `budget_data_missing` and `budget_failed` blockers prevent non-CI results from escaping that rule. Incomplete lineage produces a canonical all-`NO_DATA` blocked sentinel without inspecting ambient artifacts, target paths, or mtimes. Inventory SHA-256 uses compact JSON in producer declaration order and the listed field order, with every threshold rendered using exactly six decimal places. The canonical v0.2.0 digest is `4e24380af0ca4fe8fd94850d63e607868d15d704a42d434bdb1c762e7e327663`.\n\n",
    );

    let protocol_contract = canonical_protocol_contract();

    md.push_str("### `pi.bench.protocol.v1`\n\n");
    md.push_str("| Field | Type | Description |\n");
    md.push_str("|---|---|---|\n");
    md.push_str("| `schema` | string | Always `\"pi.bench.protocol.v1\"` |\n");
    md.push_str("| `version` | string | Protocol version used by all benchmark harnesses |\n");
    md.push_str("| `partition_tags` | string[] | Must include `matched-state` and `realistic` |\n");
    md.push_str(
        "| `realistic_session_sizes` | integer[] | Canonical matrix: 100k, 200k, 500k, 1M, 5M |\n",
    );
    md.push_str(
        "| `matched_state_scenarios` | object[] | `cold_start`, `warm_start`, `tool_call`, `event_dispatch` with replay inputs |\n",
    );
    md.push_str(
        "| `required_metadata_fields` | string[] | `runtime`, `build_profile`, `host`, `scenario_id`, `correlation_id` |\n",
    );
    md.push_str(
        "| `evidence_labels` | object | `evidence_class` (`measured/inferred`) + `confidence` (`high/medium/low`) |\n",
    );
    md.push_str(
        "| `regression_gate_admission` | object | Generic additive metadata policy only: measured, high-confidence wall-clock evidence at an enumerated production boundary with a positive sample count, exact cache-policy tokens, a clean source commit, and a verified canonical perf executable/config fingerprint |\n",
    );
    md.push_str(
        "| `pijs_regression_gate_admission` | object | PiJS-specific release gate layered on the generic policy: exactly 2000 path-verified perf-profile QuickJS iterations through the production extension manager, with 1-call mean-latency and 10-call throughput lanes |\n",
    );
    md.push_str(
        "| `partition_weighting` | object | Machine-readable partition weights (`realistic` + `matched-state`) with explicit sum-to-one contract |\n",
    );
    md.push_str(
        "| `partition_interpretation` | object | Primary/secondary partition roles and release guardrail forbidding single-partition conclusions |\n",
    );
    md.push_str(
        "| `user_perceived_sli_catalog` | object[] | Versioned user-facing SLI targets with UX interpretation guidance |\n",
    );
    md.push_str(
        "| `scenario_sli_matrix` | object[] | Canonical mapping from benchmark scenarios to user-perceived SLIs and consuming validation beads |\n\n",
    );

    md.push_str("## User-Perceived SLI Catalog\n\n");
    md.push_str("| SLI ID | Unit | Target | UX Guidance |\n");
    md.push_str("|---|---|---|---|\n");
    for entry in protocol_contract["user_perceived_sli_catalog"]
        .as_array()
        .unwrap_or(&Vec::new())
    {
        let sli_id = entry["sli_id"].as_str().unwrap_or("unknown");
        let unit = entry["unit"].as_str().unwrap_or("unknown");
        let comparator = entry["objective"]["comparator"].as_str().unwrap_or("?");
        let threshold = entry["objective"]["threshold"].to_string();
        let guidance = entry["ux_interpretation"]["good"]
            .as_str()
            .unwrap_or("no guidance");
        let _ = writeln!(
            md,
            "| `{sli_id}` | `{unit}` | `{comparator} {threshold}` | {guidance} |"
        );
    }
    md.push('\n');

    md.push_str("## Protocol Matrix\n\n");
    md.push_str("| Partition | Scenario ID | Replay Input | SLI IDs | UX Outcome |\n");
    md.push_str("|---|---|---|---|---|\n");

    let empty_matrix = Vec::new();
    let scenario_sli_matrix = protocol_contract["scenario_sli_matrix"]
        .as_array()
        .unwrap_or(&empty_matrix);

    let lookup_matrix = |scenario_id: &str| -> (String, String) {
        let Some(row) = scenario_sli_matrix
            .iter()
            .find(|row| row["scenario_id"].as_str() == Some(scenario_id))
        else {
            return ("(missing)".to_string(), "No UX mapping".to_string());
        };
        let sli_ids = row["sli_ids"].as_array().map_or_else(
            || "(missing)".to_string(),
            |ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        let ux_outcome = row["ux_outcome"]
            .as_str()
            .unwrap_or("No UX outcome specified")
            .to_string();
        (sli_ids, ux_outcome)
    };

    for scenario in protocol_contract["matched_state_scenarios"]
        .as_array()
        .unwrap_or(&Vec::new())
    {
        let scenario_name = scenario["scenario"].as_str().unwrap_or("unknown");
        let replay = scenario["replay_input"].to_string();
        let (sli_ids, ux_outcome) = lookup_matrix(scenario_name);
        let _ = writeln!(
            md,
            "| `{PARTITION_MATCHED_STATE}` | `{scenario_name}` | `{replay}` | `{sli_ids}` | {ux_outcome} |"
        );
    }
    for scenario in protocol_contract["realistic_replay_inputs"]
        .as_array()
        .unwrap_or(&Vec::new())
    {
        let scenario_id = scenario["scenario_id"].as_str().unwrap_or("unknown");
        let replay = scenario["replay_input"].to_string();
        let (sli_ids, ux_outcome) = lookup_matrix(scenario_id);
        let _ = writeln!(
            md,
            "| `{PARTITION_REALISTIC}` | `{scenario_id}` | `{replay}` | `{sli_ids}` | {ux_outcome} |"
        );
    }
    md.push('\n');

    // Determinism notes
    md.push_str("## Determinism Requirements\n\n");
    md.push_str(
        "1. **Stable key ordering**: JSON keys are sorted alphabetically within each record\n",
    );
    md.push_str("2. **No floating point in keys**: Use string or integer identifiers\n");
    md.push_str(
        "3. **Timestamps**: canonical RFC 3339 UTC; release-facing v2 summaries and PiJS records use millisecond precision (`2026-02-06T01:00:00.000Z`)\n",
    );
    md.push_str("4. **Config hash**: SHA-256 of concatenated env fields for dedup\n");
    md.push_str("5. **One record per line**: Standard JSONL (newline-delimited JSON)\n");

    let md_path = reports_dir.join("BENCH_SCHEMA.md");
    std::fs::write(&md_path, &md).expect("write BENCH_SCHEMA.md");

    // Write machine-readable schema registry
    let registry = json!({
        "schema": "pi.bench.schema_registry.v1",
        "schemas": SCHEMAS.iter().map(|(name, desc)| json!({
            "name": name,
            "description": desc,
        })).collect::<Vec<_>>(),
        "protocol_contract": canonical_protocol_contract(),
        "budget_summary_contract": canonical_budget_summary_contract(),
        "env_fingerprint_fields": ENV_FINGERPRINT_FIELDS.iter().map(|(name, desc)| json!({
            "field": name,
            "description": desc,
        })).collect::<Vec<_>>(),
    });

    let registry_path = reports_dir.join("bench_schema_registry.json");
    let mut registry_json = serde_json::to_string_pretty(&registry).unwrap_or_default();
    registry_json.push('\n');
    std::fs::write(&registry_path, registry_json).expect("write bench_schema_registry.json");

    eprintln!("[schema] Generated:");
    eprintln!("  {}", md_path.display());
    eprintln!("  {}", registry_path.display());
}
