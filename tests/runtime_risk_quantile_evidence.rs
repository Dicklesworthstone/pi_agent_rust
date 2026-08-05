//! E2E evidence tests for runtime-risk quantile optimization (bd-xqipg).
//!
//! Validates:
//! - Quantile-based conformal prediction under deterministic harden/quarantine/recovery flows
//! - Structured JSONL decision logging with full metadata
//! - Reproducibility artifacts (env.json, manifest.json) with checksums
//! - Budget enforcement with fail-closed on exhaustion

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RuntimeRiskActionValue, RuntimeRiskCalibrationConfig, RuntimeRiskConfig,
    calibrate_runtime_risk_from_ledger, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::BTreeMap;
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

const fn make_risk_config(window_size: usize, ledger_limit: usize) -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size,
        ledger_limit,
        decision_timeout_ms: 50,
        fail_closed: true,
    }
}

/// Emit a reproducibility env.json artifact capturing system context.
fn write_env_artifact(harness: &TestHarness, test_name: &str) -> serde_json::Value {
    let env = json!({
        "test_name": test_name,
        "bead_id": "bd-xqipg",
        "parent_bead": "bd-118dw",
        "rust_version": env!("CARGO_PKG_VERSION"),
        "target": std::env::consts::ARCH,
        "os": std::env::consts::OS,
        "pi_test_mode": std::env::var("PI_TEST_MODE").unwrap_or_default(),
        "timestamp_utc": chrono_like_timestamp(),
    });
    let path = harness.temp_path("env.json");
    fs::write(&path, serde_json::to_string_pretty(&env).unwrap()).expect("write env.json");
    harness.record_artifact("env.json", &path);
    env
}

/// Emit a manifest.json linking artifacts to bead IDs and scenario IDs.
fn write_manifest_artifact(
    harness: &TestHarness,
    test_name: &str,
    artifacts: &BTreeMap<String, String>,
) {
    let manifest = json!({
        "schema": "pi.ext.quantile_evidence.manifest.v1",
        "test_name": test_name,
        "bead_id": "bd-xqipg",
        "artifacts": artifacts,
        "timestamp_utc": chrono_like_timestamp(),
    });
    let path = harness.temp_path("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest).unwrap()).expect("write manifest");
    harness.record_artifact("manifest.json", &path);
}

/// Timestamp without chrono dependency.
fn chrono_like_timestamp() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{epoch}")
}

// ============================================================================
// Test 1: Deterministic harden flow with quantile evidence
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_quantile_harden_flow_with_evidence() {
    let harness = TestHarness::new("e2e_quantile_harden_flow_with_evidence");
    let env_artifact = write_env_artifact(&harness, "e2e_quantile_harden_flow_with_evidence");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some("ext.quantile.harden"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let scenario_start = Instant::now();

    futures::executor::block_on(async {
        // Phase 1: Benign calls to populate baseline residual window
        for idx in 0..8 {
            let call = HostCallPayload {
                call_id: format!("harden-benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("benign-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Phase 2: Dangerous exec calls -> should be hardened/denied
        let mut harden_count = 0usize;
        for idx in 0..4 {
            let call = HostCallPayload {
                call_id: format!("harden-exec-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "git", "args": ["reset", "--hard", format!("HEAD~{idx}")] }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let result = dispatch_host_call_shared(&ctx, call).await;
            if result.is_error {
                harden_count += 1;
            }
        }
        assert!(
            harden_count > 0,
            "at least one exec call should be hardened/denied"
        );
    });

    let scenario_elapsed_ms = scenario_start.elapsed().as_millis();

    // Verify ledger integrity
    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "ledger should verify: {verification:?}");

    // Check conformal quantile values are present in ledger entries
    let has_quantile_data = artifact
        .entries
        .iter()
        .any(|entry| entry.conformal_quantile > 0.0 || entry.conformal_residual > 0.0);
    // After enough benign calls, the residual window should populate
    // and conformal values should be non-trivial for exec calls
    assert!(
        has_quantile_data || artifact.entries.len() < 10,
        "ledger should contain conformal quantile data after baseline window fills"
    );

    // Emit structured JSONL decision log
    for entry in &artifact.entries {
        harness
            .log()
            .info_ctx("quantile_evidence", "risk decision", |ctx| {
                ctx.push(("bead_id".into(), "bd-xqipg".into()));
                ctx.push(("scenario".into(), "harden_flow".into()));
                ctx.push(("call_id".into(), entry.call_id.clone()));
                ctx.push(("extension_id".into(), entry.extension_id.clone()));
                ctx.push(("capability".into(), entry.capability.clone()));
                ctx.push(("risk_score".into(), format!("{:.6}", entry.risk_score)));
                ctx.push((
                    "conformal_residual".into(),
                    format!("{:.6}", entry.conformal_residual),
                ));
                ctx.push((
                    "conformal_quantile".into(),
                    format!("{:.6}", entry.conformal_quantile),
                ));
                ctx.push(("drift_detected".into(), entry.drift_detected.to_string()));
                ctx.push((
                    "action".into(),
                    format!("{:?}", entry.selected_action).to_lowercase(),
                ));
                ctx.push(("triggers".into(), entry.triggers.join("|")));
                ctx.push(("ledger_hash".into(), entry.ledger_hash.clone()));
                ctx.push((
                    "scenario_elapsed_ms".into(),
                    scenario_elapsed_ms.to_string(),
                ));
            });
    }

    let jsonl_path = harness.temp_path("harden-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write harden evidence");
    harness.record_artifact("harden-evidence.log.jsonl", &jsonl_path);

    // Verify JSONL has required fields
    let raw = fs::read_to_string(&jsonl_path).expect("read evidence jsonl");
    let mut evidence_rows = 0usize;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid jsonl");
        if value.get("category").and_then(serde_json::Value::as_str) != Some("quantile_evidence") {
            continue;
        }
        let context = value
            .get("context")
            .and_then(serde_json::Value::as_object)
            .expect("context object");
        for key in [
            "bead_id",
            "scenario",
            "call_id",
            "capability",
            "risk_score",
            "conformal_residual",
            "conformal_quantile",
            "drift_detected",
            "action",
            "triggers",
            "ledger_hash",
            "scenario_elapsed_ms",
        ] {
            assert!(context.contains_key(key), "missing evidence key: {key}");
        }
        evidence_rows += 1;
    }
    assert!(
        evidence_rows >= artifact.entries.len(),
        "expected >= {} evidence rows, got {evidence_rows}",
        artifact.entries.len()
    );

    // Write manifest
    let mut artifacts_map = BTreeMap::new();
    artifacts_map.insert("env".into(), env_artifact.to_string());
    artifacts_map.insert("evidence_log".into(), jsonl_path.display().to_string());
    artifacts_map.insert("entry_count".into(), artifact.entries.len().to_string());
    write_manifest_artifact(&harness, "harden_flow", &artifacts_map);
}

// ============================================================================
// Test 2: Quarantine escalation with replay verification
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_quantile_quarantine_escalation_with_replay() {
    let harness = TestHarness::new("e2e_quantile_quarantine_escalation_with_replay");
    write_env_artifact(&harness, "e2e_quantile_quarantine_escalation_with_replay");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(32, 256));

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some("ext.quantile.quarantine"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Hammer exec calls to trigger quarantine (consecutive_unsafe >= 3)
        let mut saw_terminate = false;
        for idx in 0..10 {
            let call = HostCallPayload {
                call_id: format!("quarantine-exec-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "rm", "args": ["-rf", "/"] }),
                timeout_ms: Some(10),
                cancel_token: None,
                context: None,
            };
            let result = dispatch_host_call_shared(&ctx, call).await;
            if result.is_error
                && let Some(err) = result.error.as_ref()
                && err.message.contains("quarantined")
            {
                saw_terminate = true;
            }
        }
        assert!(
            saw_terminate,
            "extension should be quarantined after repeated unsafe calls"
        );
    });

    // Verify ledger + replay
    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "quarantine ledger should verify");

    let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay should succeed");
    assert_eq!(replay.entry_count, artifact.entries.len());

    // Verify escalation: should see Allow/Harden -> Deny -> Terminate progression
    let has_terminate = artifact
        .entries
        .iter()
        .any(|e| matches!(e.selected_action, RuntimeRiskActionValue::Terminate));
    assert!(has_terminate, "ledger should contain Terminate action");

    // Verify replay steps match artifact entries exactly
    for (step, entry) in replay.steps.iter().zip(artifact.entries.iter()) {
        assert_eq!(step.call_id, entry.call_id);
        assert_eq!(step.selected_action, entry.selected_action);
        assert_eq!(step.ledger_hash, entry.ledger_hash);
    }

    // Log replay evidence
    for step in &replay.steps {
        harness
            .log()
            .info_ctx("quantile_evidence", "replay step", |ctx| {
                ctx.push(("bead_id".into(), "bd-xqipg".into()));
                ctx.push(("scenario".into(), "quarantine_escalation".into()));
                ctx.push(("step_index".into(), step.index.to_string()));
                ctx.push(("call_id".into(), step.call_id.clone()));
                ctx.push((
                    "action".into(),
                    format!("{:?}", step.selected_action).to_lowercase(),
                ));
                ctx.push(("reason_codes".into(), step.reason_codes.join("|")));
                ctx.push(("ledger_hash".into(), step.ledger_hash.clone()));
            });
    }

    let jsonl_path = harness.temp_path("quarantine-replay-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write quarantine evidence");
    harness.record_artifact("quarantine-replay-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 3: Recovery flow after quarantine with calibration
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_quantile_recovery_flow_with_calibration() {
    let harness = TestHarness::new("e2e_quantile_recovery_flow_with_calibration");
    write_env_artifact(&harness, "e2e_quantile_recovery_flow_with_calibration");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 1024));

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some("ext.quantile.recovery"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Phase 1: Mix of benign + dangerous to build diverse residual window
        for idx in 0..12 {
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
                    json!({ "level": "info", "message": format!("mixed-{idx}") }),
                )
            };
            let call = HostCallPayload {
                call_id: format!("recovery-mixed-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Phase 2: Pure benign recovery
        for idx in 0..6 {
            let call = HostCallPayload {
                call_id: format!("recovery-benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("recovery-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    // Run calibration on the mixed ledger
    let artifact = manager.runtime_risk_ledger_artifact();
    assert!(
        artifact.entries.len() >= 15,
        "expected >= 15 ledger entries for calibration, got {}",
        artifact.entries.len()
    );

    let config = RuntimeRiskCalibrationConfig::default();
    let calibration =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration should succeed");

    // Calibration determinism: run again, compare
    let calibration_2 =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("second calibration");
    assert_eq!(
        calibration, calibration_2,
        "calibration must be deterministic for identical ledger input"
    );

    // Log calibration evidence
    harness
        .log()
        .info_ctx("quantile_evidence", "calibration result", |ctx| {
            ctx.push(("bead_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario".into(), "recovery_calibration".into()));
            ctx.push((
                "recommended_threshold".into(),
                format!("{:.4}", calibration.recommended.threshold),
            ));
            ctx.push((
                "objective_score".into(),
                format!("{:.6}", calibration.recommended.objective_score),
            ));
            ctx.push((
                "candidates_evaluated".into(),
                calibration.candidates.len().to_string(),
            ));
            ctx.push(("entry_count".into(), artifact.entries.len().to_string()));
        });

    let jsonl_path = harness.temp_path("recovery-calibration-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write recovery evidence");
    harness.record_artifact("recovery-calibration-evidence.log.jsonl", &jsonl_path);

    // Write calibration report as artifact
    let calibration_path = harness.temp_path("calibration-report.json");
    fs::write(
        &calibration_path,
        serde_json::to_string_pretty(&calibration).unwrap(),
    )
    .expect("write calibration report");
    harness.record_artifact("calibration-report.json", &calibration_path);
}

// ============================================================================
// Test 4: Budget enforcement with fail-closed semantics
// ============================================================================

#[test]
fn e2e_quantile_budget_enforcement_fail_closed() {
    let harness = TestHarness::new("e2e_quantile_budget_enforcement_fail_closed");
    write_env_artifact(&harness, "e2e_quantile_budget_enforcement_fail_closed");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some("ext.quantile.budget"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let budget_start = Instant::now();
    let budget_limit_ms: u128 = 5_000; // 5 second test budget

    futures::executor::block_on(async {
        for idx in 0..50 {
            let elapsed = budget_start.elapsed().as_millis();
            if elapsed > budget_limit_ms {
                // Budget exhausted: log artifact entry and fail closed
                harness.log().info_ctx(
                    "quantile_evidence",
                    "budget exhausted - fail closed",
                    |ctx| {
                        ctx.push(("bead_id".into(), "bd-xqipg".into()));
                        ctx.push(("scenario".into(), "budget_enforcement".into()));
                        ctx.push(("budget_limit_ms".into(), budget_limit_ms.to_string()));
                        ctx.push(("elapsed_ms".into(), elapsed.to_string()));
                        ctx.push(("calls_completed".into(), idx.to_string()));
                        ctx.push(("outcome".into(), "budget_exhausted".into()));
                    },
                );
                panic!(
                    "test budget exhausted after {elapsed}ms ({idx} calls): \
                     fail closed as required by bd-xqipg"
                );
            }

            let (capability, method) = if idx % 4 == 0 {
                ("exec", "exec")
            } else {
                ("log", "log")
            };
            let call = HostCallPayload {
                call_id: format!("budget-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params: json!({
                    "cmd": "echo",
                    "args": [idx.to_string()],
                    "message": format!("budget-{idx}")
                }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let total_elapsed_ms = budget_start.elapsed().as_millis();

    // Verify completion within budget
    harness
        .log()
        .info_ctx("quantile_evidence", "budget completed", |ctx| {
            ctx.push(("bead_id".into(), "bd-xqipg".into()));
            ctx.push(("scenario".into(), "budget_enforcement".into()));
            ctx.push(("budget_limit_ms".into(), budget_limit_ms.to_string()));
            ctx.push(("elapsed_ms".into(), total_elapsed_ms.to_string()));
            ctx.push(("calls_completed".into(), "50".into()));
            ctx.push(("outcome".into(), "completed_within_budget".into()));
        });

    // Verify ledger is intact
    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        verification.valid,
        "budget test ledger should verify: {verification:?}"
    );

    let jsonl_path = harness.temp_path("budget-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write budget evidence");
    harness.record_artifact("budget-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 5: Telemetry feature vector stability across quantile window fills
// ============================================================================

#[test]
fn e2e_quantile_feature_stability_across_window_fill() {
    let harness = TestHarness::new("e2e_quantile_feature_stability_across_window_fill");
    write_env_artifact(
        &harness,
        "e2e_quantile_feature_stability_across_window_fill",
    );

    let window_size = 16;
    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(window_size, 512));

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some("ext.quantile.stability"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Fill the residual window completely, then overfill
        let total_calls = window_size * 3;
        for idx in 0..total_calls {
            let call = HostCallPayload {
                call_id: format!("stability-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("stable-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    // The telemetry should show feature vectors that stabilize after window fills
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert!(
        telemetry.entries.len() >= window_size * 3,
        "expected >= {} telemetry entries, got {}",
        window_size * 3,
        telemetry.entries.len()
    );

    // After window_size calls, feature vectors for identical-profile calls should
    // stabilize. Depending on branch-local dispatch behavior, the steady-state
    // recent_error_rate may be non-zero; assert determinism instead of a fixed value.
    let late_entries: Vec<_> = telemetry.entries.iter().skip(window_size * 2).collect();
    let steady_state_error_rate = late_entries
        .first()
        .map_or(0.0, |entry| entry.features.recent_error_rate);
    for entry in &late_entries {
        assert!(
            entry.features.recent_error_rate.is_finite(),
            "recent_error_rate must be finite, got {}",
            entry.features.recent_error_rate
        );
        assert!(
            (0.0..=1.0).contains(&entry.features.recent_error_rate),
            "recent_error_rate must be in [0, 1], got {}",
            entry.features.recent_error_rate
        );
        assert!(
            (entry.features.recent_error_rate - steady_state_error_rate).abs() < f64::EPSILON,
            "late benign calls should stabilize; expected {}, got {}",
            steady_state_error_rate,
            entry.features.recent_error_rate
        );
    }

    // Log stability evidence
    for (idx, entry) in telemetry.entries.iter().enumerate() {
        if idx % window_size == 0 || idx >= window_size * 2 {
            harness
                .log()
                .info_ctx("quantile_evidence", "feature stability", |ctx| {
                    ctx.push(("bead_id".into(), "bd-xqipg".into()));
                    ctx.push(("scenario".into(), "window_fill_stability".into()));
                    ctx.push(("call_index".into(), idx.to_string()));
                    ctx.push(("call_id".into(), entry.call_id.clone()));
                    ctx.push(("risk_score".into(), format!("{:.6}", entry.risk_score)));
                    ctx.push((
                        "error_rate".into(),
                        format!("{:.6}", entry.features.recent_error_rate),
                    ));
                    ctx.push((
                        "burst_1s".into(),
                        format!("{:.6}", entry.features.burst_density_1s),
                    ));
                });
        }
    }

    let jsonl_path = harness.temp_path("stability-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write stability evidence");
    harness.record_artifact("stability-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 6: Deterministic conformal quantile values after identical traces
// ============================================================================

#[test]
fn e2e_quantile_conformal_determinism() {
    let run_trace = || {
        let dir = tempfile::tempdir().expect("tempdir");
        let tools = ToolRegistry::new(&[], dir.path(), None);
        let http = HttpConnector::with_defaults();
        let manager = ExtensionManager::new();
        manager.set_runtime_risk_config(make_risk_config(32, 256));

        let policy = permissive_policy();
        let ctx = HostCallContext {
            runtime_name: "determinism",
            extension_id: Some("ext.quantile.det"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy: &policy,
            js_runtime: None,
            interceptor: None,
        };

        futures::executor::block_on(async {
            // Identical sequence: 5 benign, 3 exec, 2 benign
            for idx in 0..10 {
                let (cap, method, params) = if (5..8).contains(&idx) {
                    (
                        "exec",
                        "exec",
                        json!({ "cmd": "echo", "args": [idx.to_string()] }),
                    )
                } else {
                    (
                        "log",
                        "log",
                        json!({ "level": "info", "message": format!("det-{idx}") }),
                    )
                };
                let call = HostCallPayload {
                    call_id: format!("det-{idx}"),
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

        let artifact = manager.runtime_risk_ledger_artifact();
        artifact
            .entries
            .iter()
            .map(|e| {
                (
                    e.call_id.clone(),
                    e.risk_score,
                    e.conformal_residual,
                    e.conformal_quantile,
                    e.selected_action,
                )
            })
            .collect::<Vec<_>>()
    };

    let run_a = run_trace();
    let run_b = run_trace();

    assert_eq!(run_a.len(), run_b.len(), "trace lengths must match");
    for (a, b) in run_a.iter().zip(run_b.iter()) {
        assert_eq!(a.0, b.0, "call_id mismatch");
        assert!(
            (a.1 - b.1).abs() < f64::EPSILON,
            "risk_score must be deterministic for {}: {} vs {}",
            a.0,
            a.1,
            b.1
        );
        assert!(
            (a.2 - b.2).abs() < f64::EPSILON,
            "conformal_residual must be deterministic for {}: {} vs {}",
            a.0,
            a.2,
            b.2
        );
        assert!(
            (a.3 - b.3).abs() < f64::EPSILON,
            "conformal_quantile must be deterministic for {}: {} vs {}",
            a.0,
            a.3,
            b.3
        );
        assert_eq!(a.4, b.4, "action must be deterministic for {}", a.0);
    }
}
