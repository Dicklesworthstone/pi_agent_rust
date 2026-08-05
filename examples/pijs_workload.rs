//! `PiJS` workload harness for deterministic perf baselines.
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

use clap::{Parser, ValueEnum};
use futures::executor::block_on;
use pi::error::{Error, Result};
use pi::extensions::{
    ExtensionManager, ExtensionRuntimeHandle, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    NativeRustExtensionLoadSpec, NativeRustExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::perf_build;
use pi::scheduler::HostcallOutcome;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const QUICKJS_RUNTIME_TOOL_NAME: &str = "hello";
const NATIVE_RUNTIME_TOOL_NAME: &str = "bench_tool";
const REGRESSION_GATE_ITERATIONS: usize = 2_000;
const REGRESSION_GATE_TOOL_CALLS: [usize; 2] = [1, 10];
const BENCH_RUN_ID_ENV: &str = "PI_BENCH_RUN_ID";
const BENCH_CORRELATION_ID_ENV: &str = "PI_BENCH_CORRELATION_ID";

const NATIVE_RUNTIME_DESCRIPTOR: &str = r#"
{
  "id": "ext.native.bench",
  "name": "Native Bench",
  "version": "0.0.0",
  "apiVersion": "1.0.0",
  "tools": [
    {
      "name": "bench_tool",
      "description": "Benchmark tool",
      "parameters": {
        "type": "object",
        "properties": {
          "value": { "type": "number" }
        }
      }
    }
  ],
  "toolOutputs": {
    "bench_tool": {
      "content": [
        { "type": "text", "text": "ok" }
      ],
      "details": { "ok": true, "runtime": "native-rust-runtime" },
      "is_error": false
    }
  }
}
"#;

#[derive(Parser, Debug)]
#[command(name = "pijs_workload")]
#[command(about = "Deterministic PiJS workload runner for perf baselines")]
struct Args {
    /// Outer loop iterations.
    #[arg(
        long,
        default_value_t = NonZeroUsize::new(REGRESSION_GATE_ITERATIONS)
            .expect("regression-gate iteration count is nonzero")
    )]
    iterations: NonZeroUsize,
    /// Tool calls per iteration.
    #[arg(long, default_value_t = NonZeroUsize::MIN)]
    tool_calls: NonZeroUsize,
    /// Runtime engine used by the benchmark harness.
    #[arg(long, value_enum, default_value_t = WorkloadRuntimeEngine::Quickjs)]
    runtime_engine: WorkloadRuntimeEngine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WorkloadRuntimeEngine {
    Quickjs,
    NativeRustPreview,
    NativeRustRuntime,
}

impl WorkloadRuntimeEngine {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Quickjs => "quickjs",
            Self::NativeRustPreview => "native_rust_preview",
            Self::NativeRustRuntime => "native_rust_runtime",
        }
    }

    const fn measurement_boundary(self) -> &'static str {
        match self {
            Self::Quickjs => "production_extension_manager",
            Self::NativeRustRuntime => "production_extension_runtime",
            Self::NativeRustPreview => "in_process_preview",
        }
    }

    const fn measurement_contract_version(self) -> &'static str {
        match self {
            Self::Quickjs => "production_extension_manager.v1",
            Self::NativeRustRuntime => "production_extension_runtime.v1",
            Self::NativeRustPreview => "in_process_preview.v1",
        }
    }
}

#[derive(Clone, Copy)]
enum RegressionGateRequirement {
    BuildFingerprint = 1 << 0,
    BinaryProfile = 1 << 1,
    CanonicalFeatures = 1 << 2,
    CanonicalAllocator = 1 << 3,
    RunIdentity = 1 << 4,
    SourceIdentity = 1 << 5,
    OptimizedBinary = 1 << 6,
}

impl RegressionGateRequirement {
    const ALL: [Self; 7] = [
        Self::BuildFingerprint,
        Self::BinaryProfile,
        Self::CanonicalFeatures,
        Self::CanonicalAllocator,
        Self::RunIdentity,
        Self::SourceIdentity,
        Self::OptimizedBinary,
    ];

    const fn bit(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy)]
struct RegressionGateVerifications(u8);

impl RegressionGateVerifications {
    const ALL: Self = Self((1 << RegressionGateRequirement::ALL.len()) - 1);

    fn from_results(results: [(RegressionGateRequirement, bool); 7]) -> Self {
        let mut verified = 0;
        for (requirement, passed) in results {
            if passed {
                verified |= requirement.bit();
            }
        }
        Self(verified)
    }

    #[cfg(test)]
    const fn without(self, requirement: RegressionGateRequirement) -> Self {
        Self(self.0 & !requirement.bit())
    }

    const fn is_complete(self) -> bool {
        self.0 == Self::ALL.0
    }
}

#[derive(Clone, Copy)]
struct RegressionGateInputs<'a> {
    runtime_engine: WorkloadRuntimeEngine,
    build_profile: &'a str,
    verifications: RegressionGateVerifications,
    iterations: usize,
    tool_calls: usize,
}

fn is_regression_gate_eligible(inputs: RegressionGateInputs<'_>) -> bool {
    inputs.verifications.is_complete()
        && inputs.build_profile == "perf"
        && inputs.runtime_engine == WorkloadRuntimeEngine::Quickjs
        && inputs.iterations == REGRESSION_GATE_ITERATIONS
        && REGRESSION_GATE_TOOL_CALLS.contains(&inputs.tool_calls)
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn run_identity_is_canonical(run_id: Option<&str>, correlation_id: Option<&str>) -> bool {
    matches!(
        (run_id.map(str::trim), correlation_id.map(str::trim)),
        (Some(run_id), Some(correlation_id))
            if !run_id.is_empty() && run_id == correlation_id
    )
}

fn canonical_executable_path(path: &Path) -> Result<std::path::PathBuf> {
    std::fs::canonicalize(path).map_err(|err| {
        Error::extension(format!(
            "failed to canonicalize workload executable {}: {err}",
            path.display()
        ))
    })
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checked_total_calls(iterations: usize, tool_calls: usize) -> Result<(usize, u32)> {
    let total_calls = iterations
        .checked_mul(tool_calls)
        .ok_or_else(|| Error::extension("iterations * tool-calls exceeds usize"))?;
    let total_calls_u32 = u32::try_from(total_calls).map_err(|_| {
        Error::extension(format!(
            "iterations * tool-calls ({total_calls}) exceeds the exact u32 evidence range"
        ))
    })?;
    Ok((total_calls, total_calls_u32))
}

#[derive(Debug)]
struct NativeHostcallRequest {
    call_id: u64,
}

struct QuickJsBenchRuntime {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
}

#[derive(Debug, Default)]
struct NativeBenchRuntime {
    next_call_id: u64,
    pending: VecDeque<NativeHostcallRequest>,
    inflight_call_id: Option<u64>,
    roundtrip_done: bool,
}

impl NativeBenchRuntime {
    fn begin_roundtrip(&mut self) {
        self.roundtrip_done = false;
        self.next_call_id = self.next_call_id.saturating_add(1);
        let call_id = self.next_call_id;
        self.inflight_call_id = Some(call_id);
        self.pending.push_back(NativeHostcallRequest { call_id });
    }

    fn drain_hostcall_request(&mut self) -> Result<NativeHostcallRequest> {
        self.pending.pop_front().ok_or_else(|| {
            Error::extension("native workload: missing pending hostcall request".to_string())
        })
    }

    fn complete_hostcall(&mut self, call_id: u64, outcome: HostcallOutcome) -> Result<()> {
        let expected = self.inflight_call_id.take().ok_or_else(|| {
            Error::extension("native workload: no inflight hostcall to complete".to_string())
        })?;

        if expected != call_id {
            return Err(Error::extension(format!(
                "native workload: call_id mismatch (expected {expected}, got {call_id})"
            )));
        }

        match outcome {
            HostcallOutcome::Success(value) => {
                if value.as_bool() == Some(true) {
                    self.roundtrip_done = true;
                    Ok(())
                } else {
                    Err(Error::extension(
                        "native workload: completion payload missing boolean true".to_string(),
                    ))
                }
            }
            other => Err(Error::extension(format!(
                "native workload: unsupported completion outcome: {other:?}"
            ))),
        }
    }

    fn assert_roundtrip(&self) -> Result<()> {
        if self.roundtrip_done && self.pending.is_empty() {
            Ok(())
        } else {
            Err(Error::extension(
                "native workload: tool roundtrip did not resolve".to_string(),
            ))
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<()> {
    let args = Args::parse();
    let build_profile = perf_build::detect_build_profile();
    let current_exe = std::env::current_exe().map_err(|err| {
        Error::extension(format!(
            "failed to resolve current workload executable: {err}"
        ))
    })?;
    let current_exe = canonical_executable_path(&current_exe)?;
    let binary_path_profile = perf_build::profile_from_target_path(&current_exe);
    let binary_profile_verified = binary_path_profile.as_deref() == Some("perf");
    let build_fingerprint_verified = perf_build::has_canonical_perf_build_fingerprint();
    let canonical_features = perf_build::has_canonical_pijs_perf_features();
    let build_profile_verified = build_fingerprint_verified && binary_profile_verified;
    let allocator = perf_build::resolve_bench_allocator();
    let canonical_allocator = allocator.requested == "system"
        && allocator.requested_source == "env"
        && allocator.effective == perf_build::AllocatorKind::System
        && allocator.fallback_reason.is_none();
    let supplied_run_id = nonempty_env(BENCH_RUN_ID_ENV);
    let supplied_correlation_id = nonempty_env(BENCH_CORRELATION_ID_ENV);
    let run_identity_verified = run_identity_is_canonical(
        supplied_run_id.as_deref(),
        supplied_correlation_id.as_deref(),
    );
    let fallback_run_id = uuid::Uuid::new_v4().to_string();
    let run_id = supplied_run_id.unwrap_or_else(|| fallback_run_id.clone());
    let correlation_id = supplied_correlation_id.unwrap_or(fallback_run_id);
    let source_commit = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let source_dirty = option_env!("VERGEN_GIT_DIRTY") != Some("false");
    let source_identity_verified = is_full_git_sha(source_commit) && !source_dirty;
    let eligible_for_regression_gate = is_regression_gate_eligible(RegressionGateInputs {
        runtime_engine: args.runtime_engine,
        build_profile: &build_profile,
        verifications: RegressionGateVerifications::from_results([
            (
                RegressionGateRequirement::BuildFingerprint,
                build_fingerprint_verified,
            ),
            (
                RegressionGateRequirement::BinaryProfile,
                binary_profile_verified,
            ),
            (
                RegressionGateRequirement::CanonicalFeatures,
                canonical_features,
            ),
            (
                RegressionGateRequirement::CanonicalAllocator,
                canonical_allocator,
            ),
            (
                RegressionGateRequirement::RunIdentity,
                run_identity_verified,
            ),
            (
                RegressionGateRequirement::SourceIdentity,
                source_identity_verified,
            ),
            (
                RegressionGateRequirement::OptimizedBinary,
                !cfg!(debug_assertions),
            ),
        ]),
        iterations: args.iterations.get(),
        tool_calls: args.tool_calls.get(),
    });
    let (total_calls, total_calls_u32) =
        checked_total_calls(args.iterations.get(), args.tool_calls.get())?;
    let binary_sha256 = perf_build::sha256_file(&current_exe).map_err(|err| {
        Error::extension(format!(
            "failed to hash workload executable {}: {err}",
            current_exe.display()
        ))
    })?;
    let binary_path = current_exe.display().to_string();
    let compiled_features = perf_build::compiled_feature_set();
    let executable_build_profile = binary_path_profile.as_deref().unwrap_or("unknown");
    let config_hash =
        perf_build::benchmark_provenance_config_hash(&perf_build::BenchmarkProvenance {
            source_commit,
            source_dirty,
            build_profile: &build_profile,
            executable_build_profile,
            verification: perf_build::BenchmarkBuildVerification {
                executable_profile: binary_profile_verified,
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
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let quickjs_runtime = if args.runtime_engine == WorkloadRuntimeEngine::Quickjs {
        Some(setup_quickjs_runtime()?)
    } else {
        None
    };
    let mut native_runtime = if args.runtime_engine == WorkloadRuntimeEngine::NativeRustPreview {
        Some(NativeBenchRuntime::default())
    } else {
        None
    };
    let native_runtime_handle = if args.runtime_engine == WorkloadRuntimeEngine::NativeRustRuntime {
        Some(setup_native_runtime_bench_handle()?)
    } else {
        None
    };

    let start = Instant::now();
    for _ in 0..args.iterations.get() {
        for _ in 0..args.tool_calls.get() {
            match args.runtime_engine {
                WorkloadRuntimeEngine::Quickjs => {
                    if let Some(runtime) = quickjs_runtime.as_ref() {
                        run_tool_roundtrip_quickjs(runtime)?;
                    } else {
                        return Err(Error::extension(
                            "quickjs runtime unexpectedly unavailable".to_string(),
                        ));
                    }
                }
                WorkloadRuntimeEngine::NativeRustPreview => {
                    if let Some(runtime) = native_runtime.as_mut() {
                        run_tool_roundtrip_native(runtime)?;
                    } else {
                        return Err(Error::extension(
                            "native runtime unexpectedly unavailable".to_string(),
                        ));
                    }
                }
                WorkloadRuntimeEngine::NativeRustRuntime => {
                    if let Some(runtime) = native_runtime_handle.as_ref() {
                        run_tool_roundtrip_native_runtime(runtime)?;
                    } else {
                        return Err(Error::extension(
                            "native runtime handle unexpectedly unavailable".to_string(),
                        ));
                    }
                }
            }
        }
    }
    let elapsed = start.elapsed();

    let elapsed_millis = elapsed.as_millis();
    let elapsed_micros = elapsed.as_micros();
    let elapsed_micros_f64 = elapsed.as_secs_f64() * 1_000_000.0;
    let total_calls_u128 = total_calls as u128;

    let per_call_us = elapsed_micros.checked_div(total_calls_u128).unwrap_or(0);
    let calls_count_float = f64::from(total_calls_u32);
    let per_call_micros_f64 = if total_calls_u128 == 0 {
        0.0
    } else {
        elapsed_micros_f64 / calls_count_float
    };
    let per_call_nanos_f64 = if total_calls_u128 == 0 {
        0.0
    } else {
        elapsed.as_secs_f64() * 1_000_000_000.0 / calls_count_float
    };
    let calls_per_sec = total_calls_u128
        .saturating_mul(1_000_000)
        .checked_div(elapsed_micros)
        .unwrap_or(0);

    if let Some(runtime) = native_runtime_handle
        && !block_on(runtime.shutdown(Duration::from_secs(5)))
    {
        return Err(Error::extension(
            "native workload runtime did not shut down",
        ));
    }
    if let Some(runtime) = quickjs_runtime
        && !block_on(runtime.manager.shutdown(Duration::from_secs(5)))
    {
        return Err(Error::extension(
            "quickjs workload runtime did not shut down",
        ));
    }

    println!(
        "{}",
        json!({
            "schema": "pi.perf.workload.v1",
            "timestamp": timestamp,
            "run_id": run_id,
            "correlation_id": correlation_id,
            "source_commit": source_commit,
            "source_dirty": source_dirty,
            "tool": "pijs_workload",
            "scenario": "tool_call_roundtrip",
            "iterations": args.iterations.get(),
            "tool_calls_per_iteration": args.tool_calls.get(),
            "total_calls": total_calls,
            "elapsed_ms": elapsed_millis,
            "elapsed_us": elapsed_micros,
            "elapsed_us_f64": elapsed_micros_f64,
            "per_call_us": per_call_us,
            "per_call_us_f64": per_call_micros_f64,
            "per_call_ns_f64": per_call_nanos_f64,
            "calls_per_sec": calls_per_sec,
            "build_profile": build_profile,
            "build_profile_verified": build_profile_verified,
            "build_fingerprint_contract": perf_build::BUILD_FINGERPRINT_CONTRACT,
            "build_fingerprint_verified": build_fingerprint_verified,
            "compiled_profile_family": perf_build::COMPILED_PROFILE_FAMILY,
            "compiled_opt_level": perf_build::COMPILED_OPT_LEVEL,
            "compiled_debug": perf_build::COMPILED_DEBUG,
            "compiled_features": compiled_features,
            "executable_build_profile": executable_build_profile,
            "executable_profile_verified": binary_profile_verified,
            "debug_assertions": cfg!(debug_assertions),
            "config_hash": config_hash,
            "runtime_engine": args.runtime_engine.as_str(),
            "evidence_class": "measured",
            "confidence": if eligible_for_regression_gate {
                "high"
            } else {
                "medium"
            },
            "eligible_for_regression_gate": eligible_for_regression_gate,
            "measurement_method": "wall_clock_observation",
            "measurement_boundary": args.runtime_engine.measurement_boundary(),
            "measurement_contract_version": args.runtime_engine.measurement_contract_version(),
            "disk_cache_policy": if args.runtime_engine == WorkloadRuntimeEngine::Quickjs {
                "disabled"
            } else {
                "not_applicable"
            },
            "host_page_cache_policy": "not_applicable_measured_region",
            "allocator_requested": allocator.requested,
            "allocator_request_source": allocator.requested_source,
            "allocator_effective": allocator.effective.as_str(),
            "allocator_fallback_reason": allocator.fallback_reason,
            "binary_path": binary_path,
            "binary_sha256": binary_sha256,
        })
    );

    Ok(())
}

fn setup_quickjs_runtime() -> Result<QuickJsBenchRuntime> {
    let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let entry = cwd.join("tests/ext_conformance/artifacts/hello/hello.ts");
    let spec = JsExtensionLoadSpec::from_entry_path(&entry)?;
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let runtime = block_on(JsExtensionRuntimeHandle::start(
        PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            disk_cache_dir: None,
            ..Default::default()
        },
        tools,
        manager.clone(),
    ))?;
    manager.set_js_runtime(runtime.clone());
    block_on(manager.load_js_extensions(vec![spec]))?;
    Ok(QuickJsBenchRuntime { manager, runtime })
}

fn setup_native_runtime_bench_handle() -> Result<ExtensionRuntimeHandle> {
    let descriptor_path = std::env::temp_dir().join(format!(
        "pi_agent_rust_native_bench_descriptor_{}.native.json",
        std::process::id()
    ));
    fs::write(&descriptor_path, NATIVE_RUNTIME_DESCRIPTOR).map_err(|err| {
        Error::extension(format!(
            "native workload: failed to write descriptor {}: {err}",
            descriptor_path.display()
        ))
    })?;

    let runtime = block_on(NativeRustExtensionRuntimeHandle::start())?;
    let manager = ExtensionManager::new();
    manager.set_runtime(ExtensionRuntimeHandle::NativeRust(runtime.clone()));

    let spec = NativeRustExtensionLoadSpec::from_entry_path(&*descriptor_path.to_string_lossy())?;
    block_on(manager.load_native_extensions(vec![spec]))?;
    Ok(ExtensionRuntimeHandle::NativeRust(runtime))
}

fn run_tool_roundtrip_quickjs(runtime: &QuickJsBenchRuntime) -> Result<()> {
    block_on(async {
        let output = runtime
            .runtime
            .execute_tool(
                QUICKJS_RUNTIME_TOOL_NAME.to_string(),
                "bench-quickjs-call".to_string(),
                json!({ "name": "Pi" }),
                Arc::new(json!({})),
                60_000,
            )
            .await?;
        if output
            .get("details")
            .and_then(|details| details.get("greeted"))
            .and_then(serde_json::Value::as_str)
            == Some("Pi")
        {
            Ok(())
        } else {
            Err(Error::extension(format!(
                "quickjs workload returned an unexpected tool result: {output}"
            )))
        }
    })
}

fn run_tool_roundtrip_native(runtime: &mut NativeBenchRuntime) -> Result<()> {
    runtime.begin_roundtrip();
    let request = runtime.drain_hostcall_request()?;
    runtime.complete_hostcall(request.call_id, HostcallOutcome::Success(json!(true)))?;
    runtime.assert_roundtrip()
}

fn run_tool_roundtrip_native_runtime(runtime: &ExtensionRuntimeHandle) -> Result<()> {
    block_on(async {
        let output = runtime
            .execute_tool(
                NATIVE_RUNTIME_TOOL_NAME.to_string(),
                "bench-native-call".to_string(),
                json!({ "value": 1 }),
                Arc::new(json!({})),
                60_000,
            )
            .await?;
        if output.get("is_error").and_then(serde_json::Value::as_bool) == Some(false) {
            Ok(())
        } else {
            Err(Error::extension(format!(
                "native workload: runtime output indicates error: {output}"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pi::perf_build::profile_from_target_path;
    use std::path::Path;
    use std::time::Duration;

    use crate::{
        Args, NativeBenchRuntime, REGRESSION_GATE_ITERATIONS, RegressionGateInputs,
        RegressionGateRequirement, RegressionGateVerifications, WorkloadRuntimeEngine,
        checked_total_calls, is_full_git_sha, is_regression_gate_eligible,
        run_identity_is_canonical, run_tool_roundtrip_native, run_tool_roundtrip_native_runtime,
        setup_native_runtime_bench_handle,
    };

    #[test]
    fn workload_args_reject_zero_work() {
        assert!(
            Args::try_parse_from(["pijs_workload", "--iterations", "0"]).is_err(),
            "zero outer iterations must not produce benchmark evidence"
        );
        assert!(
            Args::try_parse_from(["pijs_workload", "--tool-calls", "0"]).is_err(),
            "zero tool calls must not produce benchmark evidence"
        );
    }

    #[test]
    fn total_call_count_rejects_inexact_evidence_range() {
        assert_eq!(
            checked_total_calls(2_000, 10).expect("canonical count"),
            (20_000, 20_000)
        );

        if let Ok(too_many_calls) = usize::try_from(u64::from(u32::MAX) + 1) {
            let err = checked_total_calls(too_many_calls, 1)
                .expect_err("counts beyond u32 must fail rather than clamp");
            assert!(
                err.to_string()
                    .contains("exceeds the exact u32 evidence range")
            );
        }
    }

    #[test]
    fn regression_gate_eligibility_requires_perf_production_runtime() {
        let canonical = RegressionGateInputs {
            runtime_engine: WorkloadRuntimeEngine::Quickjs,
            build_profile: "perf",
            verifications: RegressionGateVerifications::ALL,
            iterations: REGRESSION_GATE_ITERATIONS,
            tool_calls: 1,
        };
        assert!(is_regression_gate_eligible(canonical));

        for invalid in [
            RegressionGateInputs {
                runtime_engine: WorkloadRuntimeEngine::NativeRustRuntime,
                ..canonical
            },
            RegressionGateInputs {
                runtime_engine: WorkloadRuntimeEngine::NativeRustPreview,
                ..canonical
            },
            RegressionGateInputs {
                build_profile: "release",
                ..canonical
            },
            RegressionGateInputs {
                iterations: REGRESSION_GATE_ITERATIONS - 1,
                ..canonical
            },
            RegressionGateInputs {
                tool_calls: 2,
                ..canonical
            },
        ] {
            assert!(!is_regression_gate_eligible(invalid));
        }
        for missing in RegressionGateRequirement::ALL {
            assert!(!is_regression_gate_eligible(RegressionGateInputs {
                verifications: RegressionGateVerifications::ALL.without(missing),
                ..canonical
            }));
        }
    }

    #[test]
    fn release_identity_requires_one_nonempty_shared_identifier() {
        assert!(run_identity_is_canonical(Some("run-123"), Some("run-123")));
        assert!(!run_identity_is_canonical(
            Some("run-123"),
            Some("other-run")
        ));
        assert!(!run_identity_is_canonical(Some(""), Some("")));
        assert!(!run_identity_is_canonical(Some("run-123"), None));
    }

    #[test]
    fn release_source_identity_requires_full_git_sha() {
        assert!(is_full_git_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_git_sha("abc123"));
        assert!(!is_full_git_sha("0123456789abcdef0123456789abcdef0123456g"));
    }

    #[test]
    fn profile_from_target_path_detects_perf() {
        let path = Path::new("/tmp/repo/target/perf/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_detects_release_deps_binary() {
        let path = Path::new("/tmp/repo/target/release/deps/pijs_workload-abc123");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("release"));
    }

    #[test]
    fn profile_from_target_path_detects_target_triple_perf() {
        let path = Path::new("/tmp/repo/target/x86_64-unknown-linux-gnu/perf/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_detects_target_triple_perf_deps() {
        let path =
            Path::new("/tmp/repo/target/x86_64-unknown-linux-gnu/perf/deps/pijs_workload-abc123");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_returns_none_outside_target() {
        let path = Path::new("/tmp/repo/bin/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("bin"));
    }

    #[test]
    fn profile_from_target_path_supports_custom_target_dir() {
        let path = Path::new("/tmp/pi-build/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn native_runtime_roundtrip_resolves() {
        let mut runtime = NativeBenchRuntime::default();
        run_tool_roundtrip_native(&mut runtime).expect("native runtime roundtrip");
    }

    #[test]
    fn native_runtime_handle_roundtrip_resolves() {
        let runtime = setup_native_runtime_bench_handle().expect("native runtime setup");
        run_tool_roundtrip_native_runtime(&runtime).expect("native runtime handle roundtrip");
        let _ = futures::executor::block_on(runtime.shutdown(Duration::from_secs(1)));
    }
}
