//! Deterministic benchmark harness for extension startup/exec (bd-20s9).
//!
//! Runs cold start, warm start, tool call overhead, and event hook dispatch
//! scenarios against real extensions from the conformance artifact corpus.
//! Emits JSONL records using `pi.ext.rust_bench.v1` schema with environment
//! fingerprint for repeatable, machine-readable performance tracking.
//!
//! Environment variables:
//!   BENCH_QUICK=1                  — PR-safe subset (3 extensions, fewer iterations)
//!   BENCH_ITERATIONS=N             — Override iterations per scenario (default: 20/5)
//!   BENCH_OUTPUT_DIR=path          — Override JSONL output directory
//!   BENCH_OUTPUT_TARGET_SUBDIR=dir — Write below the active Cargo target directory
//!
//! Run:
//!   cargo test --test perf_bench_harness -- --nocapture
//!   BENCH_QUICK=1 cargo test --test perf_bench_harness -- --nocapture

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::perf_build;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sysinfo::System;

// ─── Configuration ───────────────────────────────────────────────────────────

fn is_quick_mode() -> bool {
    std::env::var("BENCH_QUICK").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn iterations_override() -> Option<usize> {
    let Ok(raw) = std::env::var("BENCH_ITERATIONS") else {
        return None;
    };
    match raw.parse::<usize>() {
        Ok(value) if value > 0 => Some(value),
        _ => panic!("BENCH_ITERATIONS must be a positive integer, got {raw:?}"),
    }
}

fn cargo_target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from,
        )
}

fn prepare_target_output_dir(target_dir: &Path, subdir: &Path) -> Result<PathBuf, String> {
    if subdir.as_os_str().is_empty()
        || subdir.is_absolute()
        || !subdir
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "BENCH_OUTPUT_TARGET_SUBDIR must be a non-empty, normalized relative path: {}",
            subdir.display()
        ));
    }

    fs::create_dir_all(target_dir)
        .map_err(|error| format!("create CARGO_TARGET_DIR {}: {error}", target_dir.display()))?;
    let canonical_target = fs::canonicalize(target_dir).map_err(|error| {
        format!(
            "canonicalize CARGO_TARGET_DIR {}: {error}",
            target_dir.display()
        )
    })?;
    let output_dir = target_dir.join(subdir);

    let mut candidate = target_dir.to_path_buf();
    for component in subdir.components() {
        let Component::Normal(part) = component else {
            return Err(format!(
                "BENCH_OUTPUT_TARGET_SUBDIR contains an invalid component: {}",
                subdir.display()
            ));
        };
        candidate.push(part);
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&candidate).map_err(|create_error| {
                    format!(
                        "create BENCH_OUTPUT_TARGET_SUBDIR component {}: {create_error}",
                        candidate.display()
                    )
                })?;
                fs::symlink_metadata(&candidate).map_err(|inspect_error| {
                    format!(
                        "inspect created BENCH_OUTPUT_TARGET_SUBDIR component {}: {inspect_error}",
                        candidate.display()
                    )
                })?
            }
            Err(error) => {
                return Err(format!(
                    "inspect BENCH_OUTPUT_TARGET_SUBDIR component {}: {error}",
                    candidate.display()
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "BENCH_OUTPUT_TARGET_SUBDIR must not traverse a symlink: {}",
                candidate.display()
            ));
        }
        if !metadata.is_dir() {
            return Err(format!(
                "BENCH_OUTPUT_TARGET_SUBDIR component is not a directory: {}",
                candidate.display()
            ));
        }
    }

    let canonical_output = fs::canonicalize(&output_dir).map_err(|error| {
        format!(
            "canonicalize BENCH_OUTPUT_TARGET_SUBDIR {}: {error}",
            output_dir.display()
        )
    })?;
    if !canonical_output.starts_with(&canonical_target) {
        return Err(format!(
            "BENCH_OUTPUT_TARGET_SUBDIR escapes CARGO_TARGET_DIR: {}",
            canonical_output.display()
        ));
    }
    Ok(canonical_output)
}

fn output_dir() -> PathBuf {
    if let Some(subdir) = std::env::var_os("BENCH_OUTPUT_TARGET_SUBDIR") {
        return prepare_target_output_dir(&cargo_target_dir(), Path::new(&subdir))
            .unwrap_or_else(|message| panic!("{message}"));
    }

    std::env::var("BENCH_OUTPUT_DIR")
        .ok()
        .map_or_else(|| cargo_target_dir().join("perf"), PathBuf::from)
}

#[test]
fn target_output_subdir_is_confined_to_cargo_target_dir() {
    let temp = tempfile::tempdir().expect("create target confinement tempdir");
    let target_dir = temp.path().join("target-root");
    assert_eq!(
        prepare_target_output_dir(&target_dir, Path::new("nextest/pi-perf/run-1")),
        Ok(fs::canonicalize(target_dir.join("nextest/pi-perf/run-1"))
            .expect("canonical prepared output directory"))
    );
    for unsafe_path in ["", ".", "../outside", "nextest/../../outside", "/absolute"] {
        assert!(
            prepare_target_output_dir(&target_dir, Path::new(unsafe_path)).is_err(),
            "unsafe target-relative output path should be rejected: {unsafe_path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn target_output_subdir_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("create symlink confinement tempdir");
    let target_dir = temp.path().join("target");
    let outside_dir = temp.path().join("outside");
    fs::create_dir_all(&target_dir).expect("create target root");
    fs::create_dir_all(&outside_dir).expect("create outside directory");
    symlink(&outside_dir, target_dir.join("nextest")).expect("create escape symlink");

    let result = prepare_target_output_dir(&target_dir, Path::new("nextest/pi-perf/escaped-run"));
    assert!(
        result.is_err(),
        "symlinked target subdirectory must not redirect benchmark output: {result:?}"
    );
}

fn artifacts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

/// Extensions used in quick (PR) mode: one simple, one complex.
const QUICK_EXTENSIONS: &[&str] = &["hello", "pirate", "diff"];

/// Extensions used in full (nightly) mode: broader coverage.
const FULL_EXTENSIONS: &[&str] = &[
    "hello",
    "pirate",
    "diff",
    "bookmark",
    "custom-header",
    "custom-footer",
    "confirm-destructive",
    "dirty-repo-guard",
];

// ─── Environment Fingerprint ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvFingerprint {
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    mem_total_mb: u64,
    build_profile: String,
    executable_build_profile: String,
    executable_profile_verified: bool,
    build_fingerprint_verified: bool,
    build_profile_verified: bool,
    build_fingerprint_contract: String,
    compiled_profile_family: String,
    compiled_opt_level: String,
    compiled_debug: String,
    debug_assertions: bool,
    git_commit: String,
    source_dirty: bool,
    #[serde(default)]
    features: Vec<String>,
    binary_path: String,
    binary_sha256: String,
    config_hash: String,
}

fn env_config_hash(env: &EnvFingerprint) -> String {
    let compiled_features = env.features.iter().map(String::as_str).collect::<Vec<_>>();
    perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
        source_commit: &env.git_commit,
        source_dirty: env.source_dirty,
        build_profile: &env.build_profile,
        executable_build_profile: &env.executable_build_profile,
        verification: perf_build::BenchmarkBuildVerification {
            executable_profile: env.executable_profile_verified,
            build_fingerprint: env.build_fingerprint_verified,
            build_profile: env.build_profile_verified,
        },
        build_fingerprint_contract: &env.build_fingerprint_contract,
        compiled_profile_family: &env.compiled_profile_family,
        compiled_opt_level: &env.compiled_opt_level,
        compiled_debug: &env.compiled_debug,
        compiled_features: &compiled_features,
        binary_path: &env.binary_path,
        binary_sha256: &env.binary_sha256,
        debug_assertions: env.debug_assertions,
    })
}

fn collect_env_fingerprint() -> EnvFingerprint {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();

    let cpu_model = system
        .cpus()
        .first()
        .map_or_else(|| "unknown".to_string(), |cpu| cpu.brand().to_string());
    let cpu_cores = u32::try_from(system.cpus().len()).unwrap_or(u32::MAX);
    let mem_total_mb = system.total_memory() / (1024 * 1024);
    let os = System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string());
    let arch = std::env::consts::ARCH.to_string();
    let build_profile = perf_build::detect_build_profile();
    // Clean-overlay workers intentionally compile without a .git directory.
    // The controller-provided identity is therefore authoritative at runtime
    // and is independently bound to RCH's clean-overlay receipt by the
    // orchestrator. Fall back to vergen only for ordinary direct Cargo runs.
    let git_commit = std::env::var("VERGEN_GIT_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| option_env!("VERGEN_GIT_SHA").map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let source_dirty = std::env::var("VERGEN_GIT_DIRTY").map_or_else(
        |_| option_env!("VERGEN_GIT_DIRTY") != Some("false"),
        |value| !value.eq_ignore_ascii_case("false"),
    );
    let current_exe = std::env::current_exe().ok();
    let executable_build_profile = current_exe
        .as_deref()
        .and_then(perf_build::profile_from_target_path)
        .unwrap_or_else(|| "unknown".to_string());
    let executable_profile_verified = executable_build_profile == "perf";
    let build_fingerprint_verified = perf_build::has_canonical_perf_build_fingerprint();
    let build_profile_verified =
        build_profile == "perf" && executable_profile_verified && build_fingerprint_verified;
    let binary_path = current_exe
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |path| path.display().to_string());
    let binary_sha256 = current_exe
        .as_deref()
        .and_then(|path| perf_build::sha256_file(path).ok())
        .unwrap_or_else(|| "unknown".to_string());
    let compiled_features = perf_build::compiled_feature_set();
    let features = compiled_features
        .iter()
        .map(|feature| (*feature).to_string())
        .collect::<Vec<_>>();
    let mut fingerprint = EnvFingerprint {
        os,
        arch,
        cpu_model,
        cpu_cores,
        mem_total_mb,
        build_profile,
        executable_build_profile,
        executable_profile_verified,
        build_fingerprint_verified,
        build_profile_verified,
        build_fingerprint_contract: perf_build::BUILD_FINGERPRINT_CONTRACT.to_string(),
        compiled_profile_family: perf_build::COMPILED_PROFILE_FAMILY.to_string(),
        compiled_opt_level: perf_build::COMPILED_OPT_LEVEL.to_string(),
        compiled_debug: perf_build::COMPILED_DEBUG.to_string(),
        debug_assertions: cfg!(debug_assertions),
        git_commit,
        source_dirty,
        features,
        binary_path,
        binary_sha256,
        config_hash: String::new(),
    };
    fingerprint.config_hash = env_config_hash(&fingerprint);
    fingerprint
}

// ─── Statistics ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Summary {
    count: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    p999_ms: f64,
    max_ms: f64,
    mean_ms: f64,
}

fn compute_summary(samples_us: &[f64]) -> Summary {
    if samples_us.is_empty() {
        return Summary {
            count: 0,
            min_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            p999_ms: 0.0,
            max_ms: 0.0,
            mean_ms: 0.0,
        };
    }

    let mut sorted: Vec<f64> = samples_us.iter().map(|us| us / 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let count = sorted.len();
    let sum: f64 = sorted.iter().sum();

    Summary {
        count,
        min_ms: sorted[0],
        p50_ms: percentile_f64(&sorted, 50.0),
        p95_ms: percentile_f64(&sorted, 95.0),
        p99_ms: percentile_f64(&sorted, 99.0),
        p999_ms: percentile_f64(&sorted, 99.9),
        max_ms: sorted[count - 1],
        mean_ms: sum / count as f64,
    }
}

fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ─── JSONL Record ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchRecord {
    schema: String,
    runtime: String,
    run_id: String,
    correlation_id: String,
    benchmark_run_id: String,
    source_commit: String,
    source_dirty: bool,
    scenario: String,
    extension: String,
    runs: usize,
    summary: Summary,
    elapsed_ms: f64,
    per_call_us: f64,
    calls_per_sec: f64,
    env: EnvFingerprint,
    timestamp: String,
}

fn emit_jsonl_line(record: &BenchRecord) -> String {
    serde_json::to_string(record).expect("serialize extension benchmark record")
}

fn validate_bench_jsonl(content: &str) -> Result<usize, String> {
    let mut record_count = 0usize;
    let mut has_positive_cold_start = false;
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: BenchRecord = serde_json::from_str(line).map_err(|error| {
            format!("line {}: invalid benchmark record: {error}", line_index + 1)
        })?;
        if record.schema != "pi.ext.rust_bench.v1" {
            return Err(format!("line {}: unexpected schema", line_index + 1));
        }
        if record.runtime != "pi_agent_rust" {
            return Err(format!("line {}: unexpected runtime", line_index + 1));
        }
        if record.run_id != record.correlation_id {
            return Err(format!(
                "line {}: run_id and correlation_id differ",
                line_index + 1
            ));
        }
        if record.source_commit != record.env.git_commit
            || record.source_dirty != record.env.source_dirty
        {
            return Err(format!(
                "line {}: top-level and environment source identity differ",
                line_index + 1
            ));
        }
        if record.runs != record.summary.count {
            return Err(format!(
                "line {}: runs and summary.count differ",
                line_index + 1
            ));
        }
        for (field, value) in [
            ("run_id", record.run_id.as_str()),
            ("correlation_id", record.correlation_id.as_str()),
            ("benchmark_run_id", record.benchmark_run_id.as_str()),
            ("source_commit", record.source_commit.as_str()),
            ("timestamp", record.timestamp.as_str()),
            ("scenario", record.scenario.as_str()),
            ("extension", record.extension.as_str()),
            ("env.build_profile", record.env.build_profile.as_str()),
            (
                "env.executable_build_profile",
                record.env.executable_build_profile.as_str(),
            ),
            ("env.git_commit", record.env.git_commit.as_str()),
            ("env.config_hash", record.env.config_hash.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("line {}: {field} is empty", line_index + 1));
            }
        }
        if record.summary.count == 0 {
            return Err(format!(
                "line {}: benchmark record has no successful samples",
                line_index + 1
            ));
        }
        if record.env.config_hash != env_config_hash(&record.env) {
            return Err(format!(
                "line {}: environment config_hash does not bind its provenance fields",
                line_index + 1
            ));
        }
        has_positive_cold_start |= record.scenario == "cold_start" && record.summary.count > 0;
        record_count += 1;
    }
    if record_count == 0 {
        return Err("extension benchmark JSONL contains no records".to_string());
    }
    if !has_positive_cold_start {
        return Err(
            "extension benchmark JSONL contains no successful cold_start record".to_string(),
        );
    }
    Ok(record_count)
}

// ─── Extension Helpers ───────────────────────────────────────────────────────

fn find_entry_path(ext_name: &str) -> Option<PathBuf> {
    let dir = artifacts_dir().join(ext_name);
    if !dir.exists() {
        return None;
    }
    // Look for <name>.ts or index.ts
    let ts_file = dir.join(format!("{ext_name}.ts"));
    if ts_file.exists() {
        return Some(ts_file);
    }
    let index_file = dir.join("index.ts");
    if index_file.exists() {
        return Some(index_file);
    }
    // Check for package.json with main field
    let pkg_json = dir.join("package.json");
    if pkg_json.exists()
        && let Ok(content) = std::fs::read_to_string(&pkg_json)
        && let Ok(pkg) = serde_json::from_str::<Value>(&content)
        && let Some(main) = pkg.get("main").and_then(Value::as_str)
    {
        let main_path = dir.join(main);
        if main_path.exists() {
            return Some(main_path);
        }
    }
    None
}

fn create_runtime_and_load(
    ext_name: &str,
    entry_path: &Path,
    cwd: &Path,
) -> Option<(ExtensionManager, JsExtensionLoadSpec)> {
    let spec = JsExtensionLoadSpec::from_entry_path(entry_path).ok()?;

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .ok()
        }
    })?;

    manager.set_js_runtime(runtime);

    let load_result = common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });

    if load_result.is_err() {
        eprintln!("[bench] Failed to load {ext_name}: {load_result:?}");
        shutdown_manager(&manager);
        return None;
    }

    Some((manager, spec))
}

fn shutdown_manager(manager: &ExtensionManager) {
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_millis(250)).await }
    });
}

// ─── Scenario Runners ────────────────────────────────────────────────────────

/// Cold start: create fresh runtime + manager, load extension, measure total.
fn run_cold_start(_ext_name: &str, entry_path: &Path, cwd: &Path, iterations: usize) -> Vec<f64> {
    let mut samples_us = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let Ok(spec) = JsExtensionLoadSpec::from_entry_path(entry_path) else {
            continue;
        };

        let manager = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
        let js_config = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            ..Default::default()
        };

        let start = Instant::now();

        let runtime_ok = common::run_async({
            let manager = manager.clone();
            let tools = Arc::clone(&tools);
            async move {
                JsExtensionRuntimeHandle::start(js_config, tools, manager)
                    .await
                    .ok()
            }
        });

        let Some(runtime) = runtime_ok else {
            continue;
        };
        manager.set_js_runtime(runtime);

        let load_ok = common::run_async({
            let manager = manager.clone();
            async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
        });

        let elapsed_us = start.elapsed().as_micros() as f64;
        if load_ok {
            samples_us.push(elapsed_us);
        }

        shutdown_manager(&manager);
    }

    samples_us
}

/// Warm start: reuse existing runtime, reload extension.
fn run_warm_start(
    _ext_name: &str, // used for logging only in callers
    entry_path: &Path,
    cwd: &Path,
    iterations: usize,
) -> Vec<f64> {
    let Ok(spec) = JsExtensionLoadSpec::from_entry_path(entry_path) else {
        return Vec::new();
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime_ok = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .ok()
        }
    });

    let Some(runtime) = runtime_ok else {
        return Vec::new();
    };
    manager.set_js_runtime(runtime);

    // Warmup: load once to prime caches.
    let warmup_ok = common::run_async({
        let manager = manager.clone();
        let spec = spec.clone();
        async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
    });
    if !warmup_ok {
        shutdown_manager(&manager);
        return Vec::new();
    }

    // Measure subsequent loads.
    let mut samples_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let ok = common::run_async({
            let manager = manager.clone();
            let spec = spec.clone();
            async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
        });
        if ok {
            samples_us.push(start.elapsed().as_micros() as f64);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

/// Tool call overhead: load extension, then call its tool N times.
fn run_tool_call(ext_name: &str, entry_path: &Path, cwd: &Path, iterations: usize) -> Vec<f64> {
    let Some((manager, _spec)) = create_runtime_and_load(ext_name, entry_path, cwd) else {
        return Vec::new();
    };

    let Some(runtime) = manager.js_runtime() else {
        shutdown_manager(&manager);
        return Vec::new();
    };

    // Determine tool name: use the extension name as a best guess.
    let tool_name = ext_name.to_string();
    let ctx_payload = Arc::new(json!({ "hasUI": false, "cwd": cwd.display().to_string() }));

    let mut samples_us = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let call_id = format!("bench-{i}");
        let input = json!({"name": "bench"});

        let start = Instant::now();
        let result = futures::executor::block_on(runtime.execute_tool(
            tool_name.clone(),
            call_id,
            input,
            Arc::clone(&ctx_payload),
            5_000,
        ));

        let elapsed_us = start.elapsed().as_micros() as f64;
        // Only successful dispatches are evidence-bearing measurements.
        if result.is_ok() {
            samples_us.push(elapsed_us);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

/// Event hook dispatch: load extension, dispatch events N times.
fn run_event_dispatch(
    ext_name: &str,
    entry_path: &Path,
    cwd: &Path,
    iterations: usize,
) -> Vec<f64> {
    let Some((manager, _spec)) = create_runtime_and_load(ext_name, entry_path, cwd) else {
        return Vec::new();
    };

    let event_payload = json!({"systemPrompt": "You are Pi."});

    let mut samples_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let result = common::run_async({
            let manager = manager.clone();
            let payload = event_payload.clone();
            async move {
                manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(payload),
                        5_000,
                    )
                    .await
            }
        });

        let elapsed_us = start.elapsed().as_micros() as f64;
        if result.is_ok() {
            samples_us.push(elapsed_us);
        }
    }

    shutdown_manager(&manager);
    samples_us
}

// ─── Main Harness ────────────────────────────────────────────────────────────

struct HarnessConfig {
    extensions: Vec<String>,
    cold_iterations: usize,
    warm_iterations: usize,
    tool_iterations: usize,
    event_iterations: usize,
}

fn harness_config() -> HarnessConfig {
    let quick = is_quick_mode();
    let base = if quick { 5 } else { 20 };
    let iter_override = iterations_override();

    let extensions: Vec<String> = if quick {
        QUICK_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
    } else {
        FULL_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
    };

    // Filter to extensions that actually exist.
    let extensions: Vec<String> = extensions
        .into_iter()
        .filter(|name| find_entry_path(name).is_some())
        .collect();

    HarnessConfig {
        extensions,
        cold_iterations: iter_override.unwrap_or(base),
        warm_iterations: iter_override.unwrap_or(base),
        tool_iterations: iter_override.unwrap_or(base * 5),
        event_iterations: iter_override.unwrap_or(base * 5),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn bench_extension_scenarios() {
    let config = harness_config();
    let env = collect_env_fingerprint();
    let out_dir = output_dir();
    std::fs::create_dir_all(&out_dir).expect("create extension benchmark output directory");

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let correlation_id = std::env::var("CI_CORRELATION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "standalone".to_string());
    let benchmark_run_id = std::env::var("PI_BENCH_RUN_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{correlation_id}:{now}"));
    let cwd_nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let cwd_root = std::env::temp_dir().join(format!(
        "pi-bench-harness-{}-{cwd_nonce}",
        std::process::id()
    ));

    eprintln!("\n══════════════════════════════════════════════════════════");
    eprintln!("  Extension Benchmark Harness (bd-20s9)");
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!(
        "  Mode:       {}",
        if is_quick_mode() {
            "QUICK (PR)"
        } else {
            "FULL (nightly)"
        }
    );
    eprintln!("  Extensions: {}", config.extensions.len());
    eprintln!("  Cold:       {} iterations", config.cold_iterations);
    eprintln!("  Warm:       {} iterations", config.warm_iterations);
    eprintln!("  Tool call:  {} iterations", config.tool_iterations);
    eprintln!("  Event hook: {} iterations", config.event_iterations);
    eprintln!(
        "  Env:        {} {} {} cores, {}MB RAM",
        env.os, env.arch, env.cpu_cores, env.mem_total_mb
    );
    eprintln!("  Config:     {}", &env.config_hash[..16]);
    eprintln!("──────────────────────────────────────────────────────────\n");

    let mut records: Vec<BenchRecord> = Vec::new();

    for ext_name in &config.extensions {
        let Some(entry_path) = find_entry_path(ext_name) else {
            eprintln!("[bench] SKIP {ext_name}: no entry file found");
            continue;
        };

        let cwd = cwd_root.join(ext_name);
        std::fs::create_dir_all(&cwd).expect("create isolated extension benchmark cwd");

        // ── Cold Start ──
        {
            let samples = run_cold_start(ext_name, &entry_path, &cwd, config.cold_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[cold_start]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  p999={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms, summary.p999_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                run_id: correlation_id.clone(),
                correlation_id: correlation_id.clone(),
                benchmark_run_id: benchmark_run_id.clone(),
                source_commit: env.git_commit.clone(),
                source_dirty: env.source_dirty,
                scenario: "cold_start".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Warm Start ──
        {
            let samples = run_warm_start(ext_name, &entry_path, &cwd, config.warm_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[warm_start]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  p999={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms, summary.p999_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                run_id: correlation_id.clone(),
                correlation_id: correlation_id.clone(),
                benchmark_run_id: benchmark_run_id.clone(),
                source_commit: env.git_commit.clone(),
                source_dirty: env.source_dirty,
                scenario: "warm_start".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Tool Call Overhead ──
        // Only `hello` in this fixture set registers the `hello` tool. Emitting
        // zero-count rows for extensions with no matching tool would mislabel a
        // no-op/error path as a dispatch measurement.
        if ext_name == "hello" {
            let samples = run_tool_call(ext_name, &entry_path, &cwd, config.tool_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[tool_call]   {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  p999={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms, summary.p999_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                run_id: correlation_id.clone(),
                correlation_id: correlation_id.clone(),
                benchmark_run_id: benchmark_run_id.clone(),
                source_commit: env.git_commit.clone(),
                source_dirty: env.source_dirty,
                scenario: "tool_call".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        // ── Event Hook Dispatch ──
        // `pirate` is the fixture in this set that registers before_agent_start.
        if ext_name == "pirate" {
            let samples = run_event_dispatch(ext_name, &entry_path, &cwd, config.event_iterations);
            let summary = compute_summary(&samples);
            let total_elapsed: f64 = samples.iter().sum::<f64>() / 1000.0;

            eprintln!(
                "[event_hook]  {ext_name:30} n={:3}  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  p999={:.2}ms",
                summary.count, summary.p50_ms, summary.p95_ms, summary.p99_ms, summary.p999_ms,
            );

            records.push(BenchRecord {
                schema: "pi.ext.rust_bench.v1".to_string(),
                runtime: "pi_agent_rust".to_string(),
                run_id: correlation_id.clone(),
                correlation_id: correlation_id.clone(),
                benchmark_run_id: benchmark_run_id.clone(),
                source_commit: env.git_commit.clone(),
                source_dirty: env.source_dirty,
                scenario: "event_hook".to_string(),
                extension: ext_name.clone(),
                runs: summary.count,
                per_call_us: if summary.count > 0 {
                    samples.iter().sum::<f64>() / summary.count as f64
                } else {
                    0.0
                },
                calls_per_sec: if total_elapsed > 0.0 {
                    summary.count as f64 / (total_elapsed / 1000.0)
                } else {
                    0.0
                },
                elapsed_ms: total_elapsed,
                summary,
                env: env.clone(),
                timestamp: now.clone(),
            });
        }

        eprintln!();
    }

    // ── Write JSONL ──
    let jsonl_path = out_dir.join("extension_bench.jsonl");
    let jsonl: String = records
        .iter()
        .map(emit_jsonl_line)
        .collect::<Vec<_>>()
        .join("\n");
    let jsonl = format!("{jsonl}\n");
    let validated_record_count =
        validate_bench_jsonl(&jsonl).expect("validate generated extension benchmark JSONL");
    assert_eq!(validated_record_count, records.len());
    std::fs::write(&jsonl_path, jsonl).expect("write extension_bench.jsonl");

    // ── Write summary ──
    let scenarios = ["cold_start", "warm_start", "tool_call", "event_hook"];
    let mut summary_text = String::with_capacity(4096);
    summary_text.push_str("# Extension Benchmark Summary\n\n");
    let _ = writeln!(summary_text, "> Generated: {now}\n");
    let _ = writeln!(
        summary_text,
        "Mode: {}\n",
        if is_quick_mode() {
            "QUICK (PR)"
        } else {
            "FULL (nightly)"
        }
    );

    for scenario in &scenarios {
        let _ = writeln!(summary_text, "## {scenario}\n");
        summary_text.push_str(
            "| Extension | Runs | p50 (ms) | p95 (ms) | p99 (ms) | p999 (ms) | Mean (ms) |\n",
        );
        summary_text.push_str("|---|---|---|---|---|---|---|\n");

        for record in records.iter().filter(|r| r.scenario == *scenario) {
            let _ = writeln!(
                summary_text,
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
                record.extension,
                record.summary.count,
                record.summary.p50_ms,
                record.summary.p95_ms,
                record.summary.p99_ms,
                record.summary.p999_ms,
                record.summary.mean_ms,
            );
        }
        summary_text.push('\n');
    }

    let summary_path = out_dir.join("extension_bench_summary.md");
    std::fs::write(&summary_path, &summary_text).expect("write summary");

    // ── Print final summary ──
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!(
        "  Results: {} records across {} extensions",
        records.len(),
        config.extensions.len()
    );
    eprintln!("  JSONL:   {}", jsonl_path.display());
    eprintln!("  Summary: {}", summary_path.display());
    eprintln!("══════════════════════════════════════════════════════════");

    // ── Assertions ──
    // Verify we got at least some data (not all failures).
    assert!(
        records
            .iter()
            .any(|r| r.scenario == "cold_start" && r.summary.count > 0),
        "expected at least one successful cold_start measurement"
    );
    assert!(
        records.iter().all(|record| record.summary.count > 0),
        "every emitted benchmark row must contain a successful measurement"
    );

    // Budget gate: cold start p99 < 50ms for simple extensions (hello).
    // Only enforced in release builds — debug builds are ~2x slower.
    if !cfg!(debug_assertions)
        && let Some(hello_cold) = records
            .iter()
            .find(|r| r.scenario == "cold_start" && r.extension == "hello")
    {
        assert!(
            hello_cold.summary.p99_ms < 50.0,
            "hello cold start p99 ({:.2}ms) exceeds 50ms budget",
            hello_cold.summary.p99_ms,
        );
    }
}

#[test]
fn bench_jsonl_schema_validation_is_non_vacuous() {
    let mut fixture_env = EnvFingerprint {
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        cpu_model: "fixture".to_string(),
        cpu_cores: 8,
        mem_total_mb: 1_024,
        build_profile: "perf".to_string(),
        executable_build_profile: "perf".to_string(),
        executable_profile_verified: true,
        build_fingerprint_verified: true,
        build_profile_verified: true,
        build_fingerprint_contract: perf_build::BUILD_FINGERPRINT_CONTRACT.to_string(),
        compiled_profile_family: "release".to_string(),
        compiled_opt_level: "3".to_string(),
        compiled_debug: "true".to_string(),
        debug_assertions: false,
        git_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        source_dirty: false,
        features: vec!["sqlite-sessions".to_string()],
        binary_path: "/target/perf/deps/perf_bench_harness-fixture".to_string(),
        binary_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        config_hash: String::new(),
    };
    fixture_env.config_hash = env_config_hash(&fixture_env);
    let record = BenchRecord {
        schema: "pi.ext.rust_bench.v1".to_string(),
        runtime: "pi_agent_rust".to_string(),
        run_id: "test-correlation".to_string(),
        correlation_id: "test-correlation".to_string(),
        benchmark_run_id: "test-benchmark-run".to_string(),
        source_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        source_dirty: false,
        scenario: "cold_start".to_string(),
        extension: "hello".to_string(),
        runs: 1,
        summary: Summary {
            count: 1,
            min_ms: 1.0,
            p50_ms: 1.0,
            p95_ms: 1.0,
            p99_ms: 1.0,
            p999_ms: 1.0,
            max_ms: 1.0,
            mean_ms: 1.0,
        },
        elapsed_ms: 1.0,
        per_call_us: 1_000.0,
        calls_per_sec: 1_000.0,
        env: fixture_env,
        timestamp: "2026-08-25T00:00:00Z".to_string(),
    };
    let valid_jsonl = format!("{}\n", emit_jsonl_line(&record));
    assert_eq!(validate_bench_jsonl(&valid_jsonl), Ok(1));

    for empty in ["", "\n", " \n\t\n"] {
        assert!(
            validate_bench_jsonl(empty).is_err(),
            "empty or blank JSONL must not pass schema validation"
        );
    }

    let mut no_cold_start = record;
    no_cold_start.scenario = "warm_start".to_string();
    let no_cold_jsonl = format!("{}\n", emit_jsonl_line(&no_cold_start));
    assert!(
        validate_bench_jsonl(&no_cold_jsonl).is_err(),
        "JSONL without a positive cold-start record must fail"
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
#[allow(clippy::literal_string_with_formatting_args)] // bash ${VAR} syntax, not Rust fmt
fn install_fake_bench_toolchain(bin_dir: &Path) {
    let cargo_stub = r#"#!/usr/bin/env bash
set -euo pipefail
target_dir="${CARGO_TARGET_DIR:-target}"
profile="debug"
saw_pijs_example=0
for ((i=1; i<=$#; i++)); do
  if [[ "${!i}" == "--bin" ]]; then
    echo "pijs_workload must be built as a Cargo example, not --bin" >&2
    exit 44
  fi
  if [[ "${!i}" == "--example" ]]; then
    j=$((i+1))
    if [[ $j -le $# && "${!j}" == "pijs_workload" ]]; then
      saw_pijs_example=1
    fi
  fi
  if [[ "${!i}" == "--profile" ]]; then
    j=$((i+1))
    if [[ $j -le $# ]]; then
      profile="${!j}"
    fi
  fi
done
if [[ "${PI_FAKE_FAIL_JEMALLOC:-0}" == "1" ]]; then
  prev=""
  for arg in "$@"; do
    if [[ "$arg" == "--features=jemalloc" || "$arg" == "--features=jemalloc,"* ]]; then
      echo "simulated jemalloc build failure" >&2
      exit 43
    fi
    if [[ "$prev" == "--features" && "$arg" == *"jemalloc"* ]]; then
      echo "simulated jemalloc build failure" >&2
      exit 43
    fi
    prev="$arg"
  done
fi
if [[ "$saw_pijs_example" != "1" ]]; then
  echo "missing --example pijs_workload" >&2
  exit 45
fi
bin="$target_dir/$profile/examples/pijs_workload"
mkdir -p "$(dirname "$bin")"
cat >"$bin" <<'EOS'
#!/usr/bin/env bash
set -euo pipefail
iterations=0
tool_calls=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --iterations)
      iterations="$2"
      shift 2
      ;;
    --tool-calls)
      tool_calls="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
printf '{"schema":"pi.perf.synthetic_workload.v1","iterations":%s,"tool_calls":%s}\n' "$iterations" "$tool_calls"
EOS
chmod +x "$bin"
"#;
    write_executable(&bin_dir.join("cargo"), cargo_stub);

    let hyperfine_stub = r#"#!/usr/bin/env bash
set -euo pipefail
export_json=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --export-json)
      export_json="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
if [[ -z "$export_json" ]]; then
  echo "missing --export-json path" >&2
  exit 1
fi
mkdir -p "$(dirname "$export_json")"
cat >"$export_json" <<'JSON'
{"results":[{"mean":1.0}]}
JSON
"#;
    write_executable(&bin_dir.join("hyperfine"), hyperfine_stub);
}

#[cfg(unix)]
fn run_bench_workloads_with_mode(
    profile_state: &str,
    allow_fallback: bool,
    pgo_mode: &str,
) -> (std::process::Output, PathBuf, PathBuf) {
    run_bench_workloads_with_config(
        profile_state,
        allow_fallback,
        pgo_mode,
        "system",
        "system",
        false,
    )
}

#[cfg(unix)]
fn run_bench_workloads_with_config(
    profile_state: &str,
    allow_fallback: bool,
    pgo_mode: &str,
    allocators_csv: &str,
    allocator_fallback: &str,
    fail_jemalloc_build: bool,
) -> (std::process::Output, PathBuf, PathBuf) {
    let temp_root = unique_temp_dir("pgo-fallback");
    let bin_dir = temp_root.join("bin");
    let target_dir = temp_root.join("target");
    let out_dir = temp_root.join("out");
    let profile_dir = temp_root.join("profiles");
    let profile_data = profile_dir.join("pijs_workload.profdata");
    let events_path = out_dir.join("pgo_events.jsonl");

    fs::create_dir_all(&bin_dir).expect("create bin dir");
    fs::create_dir_all(&target_dir).expect("create target dir");
    fs::create_dir_all(&out_dir).expect("create out dir");
    fs::create_dir_all(&profile_dir).expect("create profile dir");

    match profile_state {
        "corrupt" => {
            fs::write(&profile_data, b"").expect("create empty profile data");
        }
        "present" => {
            fs::write(&profile_data, b"not-real-profdata").expect("create synthetic profile data");
        }
        _ => {}
    }

    install_fake_bench_toolchain(&bin_dir);

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new("bash")
        .arg("scripts/bench_extension_workloads.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("OUT_DIR", &out_dir)
        .env("JSONL_OUT", out_dir.join("bench.jsonl"))
        .env("BENCH_CARGO_PROFILE", "perf")
        .env("BENCH_CARGO_RUNNER", "local")
        .env("BENCH_ALLOCATORS_CSV", allocators_csv)
        .env("BENCH_ALLOCATOR_FALLBACK", allocator_fallback)
        .env("BENCH_PGO_MODE", pgo_mode)
        .env(
            "BENCH_PGO_ALLOW_FALLBACK",
            if allow_fallback { "1" } else { "0" },
        )
        .env("BENCH_PGO_PROFILE_DIR", &profile_dir)
        .env("BENCH_PGO_PROFILE_DATA", &profile_data)
        .env("BENCH_PGO_EVENTS_JSONL", &events_path)
        .env("ITERATIONS", "1")
        .env("TOOL_CALLS_CSV", "1")
        .env("HYPERFINE_WARMUP", "0")
        .env("HYPERFINE_RUNS", "1")
        .env(
            "PI_FAKE_FAIL_JEMALLOC",
            if fail_jemalloc_build { "1" } else { "0" },
        )
        .output()
        .expect("run bench_extension_workloads.sh");

    (output, temp_root, events_path)
}

#[cfg(unix)]
fn load_jsonl(path: &Path) -> Vec<Value> {
    let content = fs::read_to_string(path).expect("read jsonl");
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse jsonl row"))
        .collect()
}

#[cfg(unix)]
fn first_build_event(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|event| event.get("phase").and_then(Value::as_str) == Some("build"))
        .expect("build event must exist")
}

#[cfg(unix)]
#[test]
fn pgo_use_mode_missing_profile_falls_back_with_explicit_reason() {
    let (output, temp_root, events_path) = run_bench_workloads_with_mode("missing", true, "use");
    assert!(
        output.status.success(),
        "script should succeed when fallback is enabled. stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = load_jsonl(&events_path);
    let build_event = first_build_event(&events);
    assert_eq!(
        build_event
            .get("profile_data_state")
            .and_then(Value::as_str),
        Some("missing")
    );
    assert_eq!(
        build_event
            .get("pgo_mode_effective")
            .and_then(Value::as_str),
        Some("baseline_fallback")
    );
    assert_eq!(
        build_event.get("fallback_reason").and_then(Value::as_str),
        Some("missing_profile_data")
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn pgo_use_mode_corrupt_profile_falls_back_with_explicit_reason() {
    let (output, temp_root, events_path) = run_bench_workloads_with_mode("corrupt", true, "use");
    assert!(
        output.status.success(),
        "script should succeed when fallback is enabled. stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = load_jsonl(&events_path);
    let build_event = first_build_event(&events);
    assert_eq!(
        build_event
            .get("profile_data_state")
            .and_then(Value::as_str),
        Some("corrupt")
    );
    assert_eq!(
        build_event
            .get("pgo_mode_effective")
            .and_then(Value::as_str),
        Some("baseline_fallback")
    );
    assert_eq!(
        build_event.get("fallback_reason").and_then(Value::as_str),
        Some("corrupt_profile_data")
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn pgo_use_mode_missing_profile_fails_when_fallback_disabled() {
    let (output, temp_root, _events_path) = run_bench_workloads_with_mode("missing", false, "use");
    assert!(
        !output.status.success(),
        "script must fail when fallback is disabled and profile data is missing"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fallback disabled"),
        "failure should mention fallback policy. stderr={stderr}"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn pgo_compare_mode_emits_delta_artifact_and_comparison_event() {
    let (output, temp_root, events_path) =
        run_bench_workloads_with_mode("missing", true, "compare");
    assert!(
        output.status.success(),
        "script should succeed in compare mode with fallback enabled. stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let out_dir = temp_root.join("out");
    let delta_path = fs::read_dir(&out_dir)
        .expect("read out dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("pgo_delta_")
                        && std::path::Path::new(name)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                })
        })
        .expect("compare mode must emit pgo_delta_*.json artifact");

    let delta_payload: Value =
        serde_json::from_str(&fs::read_to_string(&delta_path).expect("read pgo delta artifact"))
            .expect("parse pgo delta artifact json");
    assert_eq!(
        delta_payload.get("schema").and_then(Value::as_str),
        Some("pi.perf.pgo_comparison.v1")
    );

    let events = load_jsonl(&events_path);
    let comparison_event = events
        .iter()
        .find(|event| event.get("phase").and_then(Value::as_str) == Some("comparison"))
        .expect("compare mode must emit a comparison phase event");
    assert!(
        comparison_event
            .get("comparison_json")
            .and_then(Value::as_str)
            .is_some(),
        "comparison event must include comparison_json path"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn allocator_summary_artifact_emits_schema_and_recommendation() {
    let (output, temp_root, _events_path) =
        run_bench_workloads_with_config("missing", true, "off", "system,jemalloc", "system", false);
    assert!(
        output.status.success(),
        "script should succeed for allocator summary generation. stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let summary_path = temp_root.join("out/allocator_strategy_summary.json");
    assert!(
        summary_path.exists(),
        "allocator strategy summary artifact must be emitted"
    );

    let summary_payload: Value = serde_json::from_str(
        &fs::read_to_string(&summary_path).expect("read allocator strategy summary"),
    )
    .expect("parse allocator strategy summary");

    assert_eq!(
        summary_payload.get("schema").and_then(Value::as_str),
        Some("pi.perf.allocator_strategy_summary.v1")
    );
    assert!(
        summary_payload
            .get("recommended_allocator")
            .and_then(Value::as_str)
            .is_some(),
        "allocator summary must include recommended_allocator"
    );
    assert!(
        summary_payload
            .get("hyperfine_matrix")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty()),
        "allocator summary must include non-empty hyperfine matrix"
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn allocator_jemalloc_request_falls_back_to_system_when_enabled() {
    let (output, temp_root, events_path) =
        run_bench_workloads_with_config("missing", true, "off", "jemalloc", "system", true);
    assert!(
        output.status.success(),
        "script should succeed with jemalloc fallback enabled. stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = load_jsonl(&events_path);
    let build_event = first_build_event(&events);
    assert_eq!(
        build_event
            .get("allocator_requested")
            .and_then(Value::as_str),
        Some("jemalloc")
    );
    assert_eq!(
        build_event
            .get("allocator_effective")
            .and_then(Value::as_str),
        Some("system")
    );
    assert_eq!(
        build_event.get("fallback_reason").and_then(Value::as_str),
        Some("jemalloc_build_failed")
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[cfg(unix)]
#[test]
fn allocator_jemalloc_request_fails_closed_when_fallback_disabled() {
    let (output, temp_root, _events_path) =
        run_bench_workloads_with_config("missing", true, "off", "jemalloc", "none", true);
    assert!(
        !output.status.success(),
        "script must fail closed when jemalloc build fails and allocator fallback is disabled"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to build baseline binary for allocator 'jemalloc'"),
        "failure should indicate allocator build failure. stderr={stderr}"
    );

    let _ = fs::remove_dir_all(temp_root);
}
