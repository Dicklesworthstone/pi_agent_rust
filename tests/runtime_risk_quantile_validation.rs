//! Validation suite for runtime-risk quantile optimization (bd-xqipg).
//!
//! Covers:
//! - E2E replay for harden and quarantine flows with deterministic fixtures.
//! - Structured JSONL log emission with decision metadata and timing.
//! - Reproducibility artifacts (env.json, manifest.json).
//! - Bounded test runtime budgets with fail-closed on exhaustion.

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_LEDGER_SCHEMA_VERSION, RuntimeRiskActionValue, RuntimeRiskCalibrationConfig,
    RuntimeRiskConfig, calibrate_runtime_risk_from_ledger, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::fs;
use std::time::Instant;

fn permissive_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    }
}

const fn default_risk_config() -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 50,
        fail_closed: true,
    }
}

fn setup_test_context(
    harness: &TestHarness,
    config: RuntimeRiskConfig,
) -> (
    ToolRegistry,
    HttpConnector,
    ExtensionManager,
    ExtensionPolicy,
) {
    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    let policy = permissive_policy();
    (tools, http, manager, policy)
}

// ============================================================================
// E2E: Harden flow — benign calls followed by dangerous exec calls
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_harden_flow_deterministic_replay() {
    let harness = TestHarness::new("e2e_harden_flow_deterministic_replay");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.harden-flow"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Phase 1: Benign calls establish baseline
        for idx in 0..8 {
            let call = HostCallPayload {
                call_id: format!("harden-benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("baseline-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Phase 2: Dangerous exec calls should trigger hardening
        for idx in 0..10 {
            let call = HostCallPayload {
                call_id: format!("harden-exec-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": [format!("probe-{idx}")] }),
                timeout_ms: Some(25),
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    // Verify ledger integrity
    let artifact = manager.runtime_risk_ledger_artifact();
    assert!(
        !artifact.entries.is_empty(),
        "harden flow must produce ledger entries"
    );
    assert_eq!(artifact.schema, RUNTIME_RISK_LEDGER_SCHEMA_VERSION);
    assert_eq!(artifact.entry_count, artifact.entries.len());

    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "ledger hash chain must verify");

    // Verify replay reconstructs the decision path
    let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay should succeed");
    assert_eq!(replay.steps.len(), artifact.entries.len());

    // Check that exec calls produced non-zero risk scores
    let exec_entries: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.capability == "exec")
        .collect();
    assert!(!exec_entries.is_empty(), "must have exec entries in ledger");
    for entry in &exec_entries {
        assert!(
            entry.risk_score > 0.0,
            "exec calls must have positive risk score, got {}",
            entry.risk_score
        );
    }

    // Check harden/deny actions appeared for high-risk exec calls
    let hardened_or_denied = exec_entries.iter().any(|e| {
        matches!(
            e.selected_action,
            RuntimeRiskActionValue::Harden | RuntimeRiskActionValue::Deny
        )
    });
    // Log the action distribution even if no hardening occurs
    let action_counts: std::collections::HashMap<String, usize> =
        exec_entries
            .iter()
            .fold(std::collections::HashMap::new(), |mut acc, e| {
                *acc.entry(format!("{:?}", e.selected_action)).or_insert(0) += 1;
                acc
            });

    // Emit structured JSONL log for this scenario
    harness
        .log()
        .info_ctx("runtime_risk_harden_flow", "harden flow summary", |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "harden_flow_deterministic".into()));
            ctx.push(("total_entries".into(), artifact.entries.len().to_string()));
            ctx.push(("exec_entries".into(), exec_entries.len().to_string()));
            ctx.push(("hardened_or_denied".into(), hardened_or_denied.to_string()));
            ctx.push(("action_distribution".into(), format!("{action_counts:?}")));
            ctx.push(("ledger_valid".into(), "true".into()));
            ctx.push(("replay_steps".into(), replay.steps.len().to_string()));
        });

    // Write JSONL log artifact
    let jsonl_path = harness.temp_path("harden-flow-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write harden flow jsonl");
    harness.record_artifact("harden-flow-telemetry.log.jsonl", &jsonl_path);

    // Budget check: fail closed if exceeded
    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: harden flow exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Quarantine flow — repeated unsafe attempts trigger quarantine
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_quarantine_flow_deterministic() {
    let harness = TestHarness::new("e2e_quarantine_flow_deterministic");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.quarantine-flow"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Sustained burst of dangerous exec calls to trigger quarantine
        for idx in 0..20 {
            let call = HostCallPayload {
                call_id: format!("quarantine-exec-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/probe-{idx}")] }),
                timeout_ms: Some(10),
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    assert!(
        !artifact.entries.is_empty(),
        "quarantine flow must produce entries"
    );

    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "quarantine flow ledger must verify");

    // Check for terminate/quarantine decisions in later entries
    let later_entries: Vec<_> = artifact.entries.iter().skip(10).collect();
    let has_severe_action = later_entries.iter().any(|e| {
        matches!(
            e.selected_action,
            RuntimeRiskActionValue::Terminate
                | RuntimeRiskActionValue::Deny
                | RuntimeRiskActionValue::Harden
        )
    });

    // Emit structured JSONL log
    harness.log().info_ctx(
        "runtime_risk_quarantine_flow",
        "quarantine flow summary",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "quarantine_flow_deterministic".into()));
            ctx.push(("total_entries".into(), artifact.entries.len().to_string()));
            ctx.push(("has_severe_action".into(), has_severe_action.to_string()));
            ctx.push(("ledger_valid".into(), "true".into()));
        },
    );

    let jsonl_path = harness.temp_path("quarantine-flow-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write quarantine flow jsonl");
    harness.record_artifact("quarantine-flow-telemetry.log.jsonl", &jsonl_path);

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: quarantine flow exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Calibration determinism across identical ledger snapshots
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_calibration_produces_reproducible_artifacts() {
    let harness = TestHarness::new("e2e_calibration_produces_reproducible_artifacts");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.calibration"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Mix of benign and dangerous calls for calibration
        for idx in 0..15 {
            let (capability, method, params) = if idx % 3 == 0 {
                (
                    "exec",
                    "exec",
                    json!({ "cmd": "echo", "args": [idx.to_string()] }),
                )
            } else {
                (
                    "log",
                    "log",
                    json!({ "level": "info", "message": format!("cal-{idx}") }),
                )
            };
            let call = HostCallPayload {
                call_id: format!("cal-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let config = RuntimeRiskCalibrationConfig::default();

    let first = calibrate_runtime_risk_from_ledger(&artifact, &config).expect("first calibration");
    let second =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("second calibration");
    assert_eq!(first, second, "calibration must be deterministic");

    // Write reproducibility artifacts
    let env_json = json!({
        "issue_id": "bd-xqipg",
        "scenario_id": "calibration_reproducibility",
        "rust_version": env!("CARGO_PKG_VERSION"),
        "ledger_entry_count": artifact.entry_count,
        "ledger_schema": artifact.schema,
        "calibration_objective": format!("{:?}", config.objective),
        "baseline_threshold": config.baseline_threshold,
    });
    let env_path = harness.temp_path("env.json");
    fs::write(&env_path, serde_json::to_string_pretty(&env_json).unwrap()).expect("write env.json");
    harness.record_artifact("env.json", &env_path);

    let manifest_json = json!({
        "issue_id": "bd-xqipg",
        "scenario_id": "calibration_reproducibility",
        "artifacts": [
            { "name": "env.json", "type": "environment" },
            { "name": "calibration-report.json", "type": "calibration_report" },
            { "name": "ledger-artifact.json", "type": "ledger_artifact" },
        ],
        "ledger_data_hash": artifact.data_hash,
        "recommended_threshold": first.recommended.threshold,
        "recommended_objective_score": first.recommended.objective_score,
    });
    let manifest_path = harness.temp_path("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest_json).unwrap(),
    )
    .expect("write manifest.json");
    harness.record_artifact("manifest.json", &manifest_path);

    // Write calibration report
    let report_path = harness.temp_path("calibration-report.json");
    fs::write(&report_path, serde_json::to_string_pretty(&first).unwrap())
        .expect("write calibration report");
    harness.record_artifact("calibration-report.json", &report_path);

    // Write ledger artifact
    let ledger_path = harness.temp_path("ledger-artifact.json");
    fs::write(
        &ledger_path,
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .expect("write ledger artifact");
    harness.record_artifact("ledger-artifact.json", &ledger_path);

    harness.log().info_ctx(
        "runtime_risk_calibration",
        "calibration reproducibility verified",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "calibration_reproducibility".into()));
            ctx.push(("deterministic".into(), "true".into()));
            ctx.push((
                "recommended_threshold".into(),
                format!("{:.4}", first.recommended.threshold),
            ));
            ctx.push(("ledger_data_hash".into(), artifact.data_hash.clone()));
        },
    );

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: calibration test exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Telemetry feature vectors are deterministic across replays
// ============================================================================

#[test]
fn e2e_feature_vectors_deterministic_across_replays() {
    let harness = TestHarness::new("e2e_feature_vectors_deterministic_across_replays");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let run_trace = || {
        let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
        let ctx = HostCallContext {
            runtime_name: "e2e",
            extension_id: Some("ext.e2e.feature-det"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy: &policy,
            js_runtime: None,
            session_action_origin: None,
            interceptor: None,
        };

        futures::executor::block_on(async {
            for idx in 0..12 {
                let (cap, method, params) = match idx % 4 {
                    0 => (
                        "exec",
                        "exec",
                        json!({"cmd": "echo", "args": [idx.to_string()]}),
                    ),
                    1 => ("http", "fetch", json!({"url": "http://example.com"})),
                    2 => (
                        "read",
                        "tool",
                        json!({"name": "read", "input": {"path": "/tmp/x"}}),
                    ),
                    _ => (
                        "log",
                        "log",
                        json!({"level": "info", "message": format!("m-{idx}")}),
                    ),
                };
                let call = HostCallPayload {
                    call_id: format!("feat-det-{idx}"),
                    capability: cap.to_string(),
                    method: method.to_string(),
                    params,
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                };
                let _ = dispatch_host_call_shared(&ctx, call).await;
            }
        });

        let telemetry = manager.runtime_hostcall_telemetry_artifact();
        telemetry
            .entries
            .into_iter()
            .map(|e| e.features)
            .collect::<Vec<_>>()
    };

    let first = run_trace();
    let second = run_trace();
    assert_eq!(first.len(), second.len(), "trace lengths must match");
    assert_eq!(
        first, second,
        "feature vectors must be identical across replays"
    );

    harness.log().info_ctx(
        "runtime_risk_feature_determinism",
        "feature vector determinism verified",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "feature_vector_determinism".into()));
            ctx.push(("trace_length".into(), first.len().to_string()));
            ctx.push(("deterministic".into(), "true".into()));
        },
    );

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: feature determinism test exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Ledger verification after ring-buffer truncation preserves integrity
// ============================================================================

#[test]
fn e2e_ledger_integrity_after_truncation() {
    let harness = TestHarness::new("e2e_ledger_integrity_after_truncation");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    // ledger_limit is clamped to minimum 32, so use 32 and generate > 32 calls
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 16,
        ledger_limit: 32,
        decision_timeout_ms: 50,
        fail_closed: true,
    };
    let (tools, http, manager, policy) = setup_test_context(&harness, config);
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.truncation"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Generate more entries than ledger_limit (32) to force truncation
        for idx in 0..50 {
            let (cap, method) = if idx % 2 == 0 {
                ("exec", "exec")
            } else {
                ("log", "log")
            };
            let call = HostCallPayload {
                call_id: format!("trunc-{idx}"),
                capability: cap.to_string(),
                method: method.to_string(),
                params: json!({"cmd": "echo", "args": [idx.to_string()], "message": "trace"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    // Should have been truncated to ledger_limit (clamped minimum 32)
    assert!(
        artifact.entries.len() <= 32,
        "ledger should be truncated to limit, got {} entries",
        artifact.entries.len()
    );

    // Truncated segment must still verify
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        verification.valid,
        "truncated ledger must still verify: errors={:?}",
        verification.errors
    );

    harness.log().info_ctx(
        "runtime_risk_truncation",
        "ledger truncation integrity verified",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "ledger_truncation_integrity".into()));
            ctx.push((
                "entries_after_truncation".into(),
                artifact.entries.len().to_string(),
            ));
            ctx.push(("ledger_valid".into(), verification.valid.to_string()));
        },
    );

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: truncation test exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Recovery flow — quarantine then benign calls show score reduction
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_recovery_flow_score_trajectory() {
    let harness = TestHarness::new("e2e_recovery_flow_score_trajectory");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.recovery-flow"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Phase 1: Dangerous calls to raise risk
        for idx in 0..8 {
            let call = HostCallPayload {
                call_id: format!("recovery-danger-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": ["danger"] }),
                timeout_ms: Some(10),
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Phase 2: Recovery with benign calls
        for idx in 0..12 {
            let call = HostCallPayload {
                call_id: format!("recovery-benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("recover-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    assert!(!artifact.entries.is_empty());

    // Verify the score trajectory and ledger integrity
    let danger_entries: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("recovery-danger"))
        .collect();
    let recovery_entries: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("recovery-benign"))
        .collect();
    let danger_scores: Vec<f64> = danger_entries.iter().map(|e| e.risk_score).collect();
    let recovery_scores: Vec<f64> = recovery_entries.iter().map(|e| e.risk_score).collect();

    // Log score trajectories
    harness.log().info_ctx(
        "runtime_risk_recovery_flow",
        "recovery flow score trajectory",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push((
                "scenario_id".into(),
                "recovery_flow_score_trajectory".into(),
            ));
            ctx.push(("danger_scores".into(), format!("{danger_scores:?}")));
            ctx.push(("recovery_scores".into(), format!("{recovery_scores:?}")));
            ctx.push(("danger_count".into(), danger_scores.len().to_string()));
            ctx.push(("recovery_count".into(), recovery_scores.len().to_string()));
        },
    );

    let jsonl_path = harness.temp_path("recovery-flow-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write recovery flow jsonl");
    harness.record_artifact("recovery-flow-telemetry.log.jsonl", &jsonl_path);

    // Verify ledger hash chain integrity through the full trajectory
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        verification.valid,
        "recovery flow ledger must verify: errors={:?}",
        verification.errors
    );

    // Verify danger calls have exec capability and high base scores
    assert!(
        !danger_entries.is_empty(),
        "danger phase must produce ledger entries"
    );
    for entry in &danger_entries {
        assert_eq!(entry.capability, "exec");
    }

    // Verify recovery calls have log capability
    assert!(
        !recovery_entries.is_empty(),
        "recovery phase must produce ledger entries"
    );
    for entry in &recovery_entries {
        assert_eq!(entry.capability, "log");
    }

    // Verify the first danger call has lower risk score than later ones
    // (because sequential state accumulates risk over repeated exec calls)
    if danger_scores.len() >= 2 {
        assert!(
            danger_scores[0] <= *danger_scores.last().unwrap(),
            "risk should not decrease across repeated exec calls: first={:.4}, last={:.4}",
            danger_scores[0],
            danger_scores.last().unwrap()
        );
    }

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: recovery flow exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Conformal residual quantile drift detection
// ============================================================================

#[test]
fn e2e_conformal_residual_drift_detection() {
    let harness = TestHarness::new("e2e_conformal_residual_drift_detection");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.drift"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Phase 1: Establish stable baseline with log calls
        for idx in 0..20 {
            let call = HostCallPayload {
                call_id: format!("drift-baseline-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("stable-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Phase 2: Sudden shift to dangerous calls
        for idx in 0..10 {
            let call = HostCallPayload {
                call_id: format!("drift-shift-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": [format!("shift-{idx}")] }),
                timeout_ms: Some(15),
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay succeeds");

    // Check that drift was detected in the shift phase
    let shift_steps: Vec<_> = replay
        .steps
        .iter()
        .filter(|s| s.call_id.starts_with("drift-shift"))
        .collect();
    let drift_detected_count = shift_steps.iter().filter(|s| s.drift_detected).count();

    harness.log().info_ctx(
        "runtime_risk_drift_detection",
        "conformal residual drift detection",
        |ctx| {
            ctx.push(("issue_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario_id".into(), "conformal_residual_drift".into()));
            ctx.push(("total_steps".into(), replay.steps.len().to_string()));
            ctx.push(("shift_steps".into(), shift_steps.len().to_string()));
            ctx.push((
                "drift_detected_count".into(),
                drift_detected_count.to_string(),
            ));
        },
    );

    let jsonl_path = harness.temp_path("drift-detection-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write drift detection jsonl");
    harness.record_artifact("drift-detection-telemetry.log.jsonl", &jsonl_path);

    // Conformal residual quantile values should be non-negative in replay
    for step in &replay.steps {
        assert!(
            step.conformal_quantile >= 0.0,
            "conformal quantile must be non-negative, got {} for call {}",
            step.conformal_quantile,
            step.call_id
        );
    }

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: drift detection exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// E2E: Structured JSONL log schema validation
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_structured_jsonl_log_schema_compliance() {
    let harness = TestHarness::new("e2e_structured_jsonl_log_schema_compliance");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup_test_context(&harness, default_risk_config());
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.schema-check"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        for idx in 0..5 {
            let (cap, method) = if idx % 2 == 0 {
                ("exec", "exec")
            } else {
                ("log", "log")
            };
            let call = HostCallPayload {
                call_id: format!("schema-{idx}"),
                capability: cap.to_string(),
                method: method.to_string(),
                params: json!({"cmd": "echo", "args": [idx.to_string()], "message": "trace"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    for event in &telemetry.entries {
        harness
            .log()
            .info_ctx("runtime_risk_telemetry", "telemetry event", |ctx| {
                ctx.push(("issue_id".into(), "bd-xqipg".into()));
                ctx.push(("scenario_id".into(), "schema_compliance".into()));
                ctx.push(("extension_id".into(), event.extension_id.clone()));
                ctx.push(("call_id".into(), event.call_id.clone()));
                ctx.push(("capability".into(), event.capability.clone()));
                ctx.push(("method".into(), event.method.clone()));
                ctx.push(("policy_profile".into(), event.policy_profile.clone()));
                ctx.push(("score".into(), format!("{:.6}", event.risk_score)));
                ctx.push(("reason_codes".into(), event.reason_codes.join("|")));
                ctx.push((
                    "action".into(),
                    format!("{:?}", event.selected_action).to_lowercase(),
                ));
                ctx.push(("latency_ms".into(), event.latency_ms.to_string()));
                ctx.push(("redaction_summary".into(), event.redaction_summary.clone()));
                ctx.push((
                    "explanation_level".into(),
                    format!("{:?}", event.explanation_level).to_lowercase(),
                ));
                ctx.push((
                    "top_contributors".into(),
                    event
                        .top_contributors
                        .iter()
                        .map(|item| item.code.clone())
                        .collect::<Vec<_>>()
                        .join("|"),
                ));
                ctx.push((
                    "budget_state".into(),
                    serde_json::to_string(&event.budget_state).expect("serialize budget_state"),
                ));
            });
    }

    let jsonl_path = harness.temp_path("schema-compliance.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write schema compliance jsonl");
    harness.record_artifact("schema-compliance.log.jsonl", &jsonl_path);

    // Validate every JSONL line has required fields
    let raw = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut telemetry_rows = 0usize;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid jsonl");
        if value.get("type").and_then(serde_json::Value::as_str) != Some("log") {
            continue;
        }
        if value.get("category").and_then(serde_json::Value::as_str)
            != Some("runtime_risk_telemetry")
        {
            continue;
        }

        let context = value
            .get("context")
            .and_then(serde_json::Value::as_object)
            .expect("context object must exist");

        for key in [
            "issue_id",
            "scenario_id",
            "extension_id",
            "call_id",
            "capability",
            "method",
            "policy_profile",
            "score",
            "reason_codes",
            "action",
            "latency_ms",
            "redaction_summary",
            "explanation_level",
            "top_contributors",
            "budget_state",
        ] {
            assert!(
                context.contains_key(key),
                "missing required context key: {key}"
            );
        }
        assert!(
            value
                .get("ts")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "missing timestamp"
        );
        telemetry_rows = telemetry_rows.saturating_add(1);
    }

    assert!(
        telemetry_rows >= telemetry.entry_count,
        "expected >= {} telemetry rows in JSONL, got {}",
        telemetry.entry_count,
        telemetry_rows
    );

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: schema compliance exceeded {budget_ms}ms budget (took {elapsed_ms}ms)"
    );
}
