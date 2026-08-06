//! Shared hostcall dispatcher and policy-boundary tests.

use super::*;

// ========================================================================
// Shared dispatcher tests (bd-1uy.1.3)
// ========================================================================

/// Build a permissive `HostCallContext` for testing dispatch behaviour.
pub(super) fn test_host_call_context<'a>(
    tools: &'a ToolRegistry,
    http: &'a HttpConnector,
    policy: &'a ExtensionPolicy,
) -> HostCallContext<'a>
where
    'a: 'a,
{
    HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools,
        http,
        manager: None,
        policy,
        js_runtime: None,
        interceptor: None,
    }
}

pub(super) fn permissive_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    }
}

pub(super) fn deny_all_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: vec![
            "read".to_string(),
            "write".to_string(),
            "exec".to_string(),
            "http".to_string(),
            "tool".to_string(),
            "session".to_string(),
            "ui".to_string(),
            "events".to_string(),
        ],
        ..Default::default()
    }
}

#[test]
fn shared_dispatch_unknown_tool_returns_invalid_request() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-1".to_string(),
        capability: "tool".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "nonexistent_tool", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error, "expected error for unknown tool");
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
        assert!(
            err.message.contains("Unknown tool"),
            "message should mention unknown tool, got: {}",
            err.message
        );
        // output must be object per spec (not null)
        assert!(result.output.is_object(), "output must be {{}} on error");
    });
}

#[test]
fn shared_dispatch_denied_by_policy() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-deny".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": { "path": "/etc/passwd" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error, "expected denial");
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("denied"),
            "message should mention denial, got: {}",
            err.message
        );
    });
}

#[test]
fn shared_dispatch_policy_denial_skips_runtime_risk_ledger() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 256,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "call-deny-no-risk".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": { "path": "README.md" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "policy deny should short-circuit execution"
        );
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);

        let ledger = manager.runtime_risk_ledger_snapshot();
        assert!(
            ledger.is_empty(),
            "runtime risk ledger must remain empty when policy denies the call"
        );
    });
}

#[test]
fn shared_dispatch_prompt_without_manager_fails_closed() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-prompt-no-manager".to_string(),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({ "cmd": "echo", "args": ["hi"] }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "prompt flow must fail closed without manager"
        );
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("(shutdown)"),
            "expected shutdown reason in fail-closed denial, got: {}",
            err.message
        );
    });
}

#[test]
fn shared_dispatch_runtime_risk_disabled_is_isomorphic() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let baseline_ctx = test_host_call_context(&tools, &http, &policy);

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: false,
        ..Default::default()
    });
    let risk_ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "call-risk-off".to_string(),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": "isomorphism" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let baseline = dispatch_host_call_shared(&baseline_ctx, call.clone()).await;
        let with_risk = dispatch_host_call_shared(&risk_ctx, call).await;
        assert_eq!(baseline.is_error, with_risk.is_error);
        assert_eq!(baseline.error.is_some(), with_risk.error.is_some());
        if let (Some(a), Some(b)) = (baseline.error, with_risk.error) {
            assert_eq!(a.code, b.code);
            assert_eq!(a.message, b.message);
        }
    });
}

#[test]
fn shared_dispatch_runtime_risk_hardens_exec_calls() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "call-risk-harden".to_string(),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        // Use a clearly dangerous command pattern so hardening denial is deterministic.
        params: json!({ "cmd": "git", "args": ["reset", "--hard", "HEAD~1"] }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error, "exec should be denied by risk hardening");
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("runtime risk"),
            "expected runtime risk denial, got: {}",
            err.message
        );

        let ledger = manager.runtime_risk_ledger_snapshot();
        assert!(!ledger.is_empty(), "risk ledger should record decisions");
        let last = ledger.last().expect("last ledger entry");
        assert_ne!(last.selected_action, RuntimeRiskAction::Allow);
        assert!(
            !last.ledger_hash.is_empty(),
            "ledger entry should include hash chain"
        );
    });
}

#[test]
fn shared_dispatch_runtime_risk_quarantines_repeated_unsafe_attempts() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 32,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        let mut saw_quarantine = false;
        for idx in 0..6 {
            let call = HostCallPayload {
                call_id: format!("call-risk-unsafe-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                // Repeated dangerous commands should trigger deny -> quarantine.
                params: json!({
                    "cmd": "git",
                    "args": ["reset", "--hard", format!("HEAD~{}", idx + 1)]
                }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let result = dispatch_host_call_shared(&ctx, call).await;
            assert!(result.is_error, "unsafe exec attempt should be blocked");
            let err = result.error.expect("error payload");
            if err.message.contains("quarantined") {
                saw_quarantine = true;
            }
        }

        assert!(
            saw_quarantine,
            "controller should eventually quarantine repeated unsafe attempts"
        );
        let ledger = manager.runtime_risk_ledger_snapshot();
        assert!(
            ledger
                .iter()
                .any(|entry| matches!(entry.selected_action, RuntimeRiskAction::Terminate)),
            "ledger should include at least one terminate action"
        );
    });
}

#[test]
fn shared_dispatch_runtime_risk_ledger_is_tamper_evident() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        for idx in 0..3 {
            let call = HostCallPayload {
                call_id: format!("call-risk-tamper-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": [idx.to_string()] }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        let artifact = manager.runtime_risk_ledger_artifact();
        let verification = verify_runtime_risk_ledger_artifact(&artifact);
        assert!(verification.valid, "baseline ledger should verify");

        let mut tampered = artifact;
        let first = tampered.entries.first_mut().expect("at least one entry");
        first.risk_score = runtime_risk_clamp01(first.risk_score + 0.11);
        let tampered_verification = verify_runtime_risk_ledger_artifact(&tampered);
        assert!(
            !tampered_verification.valid,
            "tampered ledger should fail verification"
        );
        assert!(
            tampered_verification
                .errors
                .iter()
                .any(|err| { err.code == "hash_mismatch" || err.code == "data_hash_mismatch" }),
            "expected hash/data mismatch in verification errors"
        );
    });
}

#[test]
fn shared_dispatch_runtime_risk_ledger_replay_reconstructs_decision_path() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        for idx in 0..4 {
            let (capability, method) = if idx % 2 == 0 {
                ("exec", "exec")
            } else {
                ("log", "log")
            };
            let call = HostCallPayload {
                call_id: format!("call-risk-replay-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params: json!({ "cmd": "echo", "args": [idx.to_string()], "message": "ok" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        let artifact = manager.runtime_risk_ledger_artifact();
        let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay should verify");
        assert_eq!(replay.entry_count, artifact.entries.len());
        assert_eq!(replay.steps.len(), artifact.entries.len());
        for (idx, (step, entry)) in replay.steps.iter().zip(artifact.entries.iter()).enumerate() {
            assert_eq!(step.index, idx);
            assert_eq!(step.call_id, entry.call_id);
            assert_eq!(step.extension_id, entry.extension_id);
            assert_eq!(step.selected_action, entry.selected_action);
            assert_eq!(step.derived_state, entry.derived_state);
            assert_eq!(step.reason_codes, entry.triggers);
            assert_eq!(step.ledger_hash, entry.ledger_hash);
        }
    });
}

#[test]
fn shared_dispatch_runtime_risk_ledger_verifies_after_ring_buffer_truncation() {
    let ledger_limit = 32;
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 32,
        ledger_limit,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        for idx in 0..48 {
            let call = HostCallPayload {
                call_id: format!("call-risk-truncate-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": [idx.to_string()] }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        let artifact = manager.runtime_risk_ledger_artifact();
        assert_eq!(
            artifact.entries.len(),
            ledger_limit,
            "ring buffer should truncate"
        );
        assert_eq!(artifact.entry_count, ledger_limit);
        assert!(
            artifact
                .entries
                .first()
                .is_some_and(|entry| entry.prev_ledger_hash.is_some()),
            "truncated first entry should retain chain anchor"
        );

        let verification = verify_runtime_risk_ledger_artifact(&artifact);
        assert!(
            verification.valid,
            "truncated ledger segment should still verify"
        );
    });
}

#[test]
fn runtime_risk_calibration_is_deterministic_for_identical_ledger() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        for idx in 0..6 {
            let (capability, method) = if idx % 3 == 0 {
                ("log", "log")
            } else {
                ("exec", "exec")
            };
            let call = HostCallPayload {
                call_id: format!("call-risk-calibration-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params: json!({ "cmd": "echo", "args": [idx.to_string()], "message": "trace" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        let artifact = manager.runtime_risk_ledger_artifact();
        let config = RuntimeRiskCalibrationConfig::default();
        let first = calibrate_runtime_risk_from_ledger(&artifact, &config)
            .expect("first calibration should succeed");
        let second = calibrate_runtime_risk_from_ledger(&artifact, &config)
            .expect("second calibration should succeed");
        assert_eq!(
            first, second,
            "calibration output must be deterministic for identical input"
        );
    });
}

fn adaptive_diff_runtime_config() -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 256,
        decision_timeout_ms: 50,
        fail_closed: true,
    }
}

fn adaptive_diff_config(min_sample_count: usize) -> AdaptiveHostcallPolicyDiffConfig {
    AdaptiveHostcallPolicyDiffConfig {
        min_sample_count,
        min_matched_coverage_bps: 9_000,
        min_latency_improvement_bps: 100,
        max_compat_rate_increase_bps: 250,
        max_error_rate_increase_bps: 100,
        max_detailed_changes: 16,
    }
}

fn adaptive_diff_event(
    call_id: &str,
    lane: &str,
    lane_decision_reason: &str,
    lane_fallback_reason: Option<&str>,
    latency_ms: u64,
    selected_action: RuntimeRiskActionValue,
    risk_score: f64,
) -> RuntimeHostcallTelemetryEvent {
    RuntimeHostcallTelemetryEvent {
        call_id: call_id.to_string(),
        extension_id: "ext.adaptive".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params_hash: format!("params-{call_id}"),
        policy_reason: "fixture_replay".to_string(),
        policy_profile: "adaptive".to_string(),
        risk_score,
        latency_ms,
        lane: lane.to_string(),
        lane_decision_reason: lane_decision_reason.to_string(),
        lane_fallback_reason: lane_fallback_reason.map(str::to_string),
        lane_matrix_key: "tool|fixture|filesystem".to_string(),
        selected_action,
        outcome: "success".to_string(),
        ..RuntimeHostcallTelemetryEvent::default()
    }
}

fn adaptive_diff_artifact(
    entries: Vec<RuntimeHostcallTelemetryEvent>,
) -> RuntimeHostcallTelemetryArtifact {
    RuntimeHostcallTelemetryArtifact {
        schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1_700_000_000_000,
        entry_count: entries.len(),
        entries,
    }
}

fn adaptive_diff_report(
    baseline_entries: Vec<RuntimeHostcallTelemetryEvent>,
    candidate_entries: Vec<RuntimeHostcallTelemetryEvent>,
    baseline_config: &RuntimeRiskConfig,
    candidate_config: &RuntimeRiskConfig,
    diff_config: &AdaptiveHostcallPolicyDiffConfig,
) -> AdaptiveHostcallPolicyDiffReport {
    let baseline = adaptive_diff_artifact(baseline_entries);
    let candidate = adaptive_diff_artifact(candidate_entries);
    build_adaptive_hostcall_policy_diff_report(&AdaptiveHostcallPolicyDiffRequest {
        baseline_policy_id: "baseline",
        candidate_policy_id: "candidate",
        baseline_config,
        candidate_config,
        baseline_telemetry: &baseline,
        candidate_telemetry: &candidate,
        config: diff_config,
        generated_at_ms: 1_700_000_000_100,
    })
}

#[test]
fn adaptive_hostcall_policy_diff_accepts_supported_latency_improvement() {
    let baseline_config = adaptive_diff_runtime_config();
    let candidate_config = RuntimeRiskConfig {
        alpha: 0.005,
        decision_timeout_ms: 25,
        ..baseline_config
    };
    let diff_config = adaptive_diff_config(3);
    let baseline_entries = vec![
        adaptive_diff_event(
            "call-1",
            "compat",
            "reactor_lane_overflow",
            Some("reactor_lane_overflow"),
            100,
            RuntimeRiskActionValue::Allow,
            0.20,
        ),
        adaptive_diff_event(
            "call-2",
            "compat",
            "reactor_lane_overflow",
            Some("reactor_lane_overflow"),
            110,
            RuntimeRiskActionValue::Allow,
            0.22,
        ),
        adaptive_diff_event(
            "call-3",
            "compat",
            "reactor_lane_overflow",
            Some("reactor_lane_overflow"),
            90,
            RuntimeRiskActionValue::Allow,
            0.18,
        ),
    ];
    let candidate_entries = vec![
        adaptive_diff_event(
            "call-1",
            "fast",
            "typed_opcode_context_v1",
            None,
            60,
            RuntimeRiskActionValue::Allow,
            0.20,
        ),
        adaptive_diff_event(
            "call-2",
            "fast",
            "typed_opcode_context_v1",
            None,
            65,
            RuntimeRiskActionValue::Allow,
            0.22,
        ),
        adaptive_diff_event(
            "call-3",
            "fast",
            "typed_opcode_context_v1",
            None,
            55,
            RuntimeRiskActionValue::Allow,
            0.18,
        ),
    ];

    let report = adaptive_diff_report(
        baseline_entries,
        candidate_entries,
        &baseline_config,
        &candidate_config,
        &diff_config,
    );

    assert_eq!(report.schema, ADAPTIVE_HOSTCALL_POLICY_DIFF_SCHEMA_VERSION);
    assert_eq!(report.verdict, AdaptiveHostcallPolicyDiffVerdict::Accept);
    assert!(report.sample_support.sufficient);
    assert_eq!(report.latency_effect.expected_effect, "improved");
    assert_eq!(report.action_changes.len(), 0);
    assert_eq!(report.rollback_conditions.len(), 0);
    assert!(
        report
            .risk_threshold_changes
            .iter()
            .any(|change| change.field == "alpha" && change.direction == "tightened")
    );
    assert!(
        report
            .risk_threshold_changes
            .iter()
            .any(|change| change.field == "decision_timeout_ms")
    );
}

#[test]
fn adaptive_hostcall_policy_diff_monitors_weak_sample_support() {
    let baseline_config = adaptive_diff_runtime_config();
    let candidate_config = baseline_config.clone();
    let diff_config = adaptive_diff_config(5);
    let report = adaptive_diff_report(
        vec![adaptive_diff_event(
            "call-1",
            "compat",
            "reactor_lane_overflow",
            Some("reactor_lane_overflow"),
            100,
            RuntimeRiskActionValue::Allow,
            0.20,
        )],
        vec![adaptive_diff_event(
            "call-1",
            "fast",
            "typed_opcode_context_v1",
            None,
            50,
            RuntimeRiskActionValue::Allow,
            0.20,
        )],
        &baseline_config,
        &candidate_config,
        &diff_config,
    );

    assert_eq!(report.verdict, AdaptiveHostcallPolicyDiffVerdict::Monitor);
    assert!(!report.sample_support.sufficient);
    assert!(
        report
            .reason_codes
            .iter()
            .any(|code| code == "weak_sample_support")
    );
    assert!(
        report
            .rollback_conditions
            .iter()
            .any(|condition| condition.code == "weak_sample_support")
    );
}

#[test]
fn adaptive_hostcall_policy_diff_rolls_back_divergent_actions() {
    let baseline_config = adaptive_diff_runtime_config();
    let candidate_config = baseline_config.clone();
    let diff_config = adaptive_diff_config(2);
    let report = adaptive_diff_report(
        vec![
            adaptive_diff_event(
                "call-1",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
            adaptive_diff_event(
                "call-2",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
        ],
        vec![
            adaptive_diff_event(
                "call-1",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Deny,
                0.92,
            ),
            adaptive_diff_event(
                "call-2",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
        ],
        &baseline_config,
        &candidate_config,
        &diff_config,
    );

    assert_eq!(report.verdict, AdaptiveHostcallPolicyDiffVerdict::Rollback);
    assert_eq!(report.action_changes.len(), 1);
    assert!(
        report
            .rollback_conditions
            .iter()
            .any(|condition| condition.code == "policy_action_divergence")
    );
}

#[test]
fn adaptive_hostcall_policy_diff_rolls_back_forced_compat_kill_switch() {
    let baseline_config = adaptive_diff_runtime_config();
    let candidate_config = baseline_config.clone();
    let diff_config = adaptive_diff_config(2);
    let report = adaptive_diff_report(
        vec![
            adaptive_diff_event(
                "call-1",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
            adaptive_diff_event(
                "call-2",
                "fast",
                "typed_opcode_context_v1",
                None,
                50,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
        ],
        vec![
            adaptive_diff_event(
                "call-1",
                "compat",
                "forced_compat_global_kill_switch",
                Some("forced_compat_global_kill_switch"),
                95,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
            adaptive_diff_event(
                "call-2",
                "compat",
                "forced_compat_global_kill_switch",
                Some("forced_compat_global_kill_switch"),
                95,
                RuntimeRiskActionValue::Allow,
                0.20,
            ),
        ],
        &baseline_config,
        &candidate_config,
        &diff_config,
    );

    assert_eq!(report.verdict, AdaptiveHostcallPolicyDiffVerdict::Rollback);
    assert_eq!(report.candidate_metrics.forced_compat_count, 2);
    assert_eq!(report.lane_changes.len(), 2);
    assert!(
        report
            .rollback_conditions
            .iter()
            .any(|condition| condition.code == "forced_compat_kill_switch_active")
    );
}

#[test]
fn runtime_hostcall_feature_vectors_are_deterministic_for_identical_traces() {
    let run_trace = || {
        let _clock = RuntimeRiskTestClockGuard::set(1_700_000_000_000);
        let dir = tempdir().expect("tempdir");
        let tools = ToolRegistry::new(&[], dir.path(), None);
        let http = HttpConnector::with_defaults();
        let policy = permissive_policy();

        let manager = ExtensionManager::new();
        manager.set_runtime_risk_config(RuntimeRiskConfig {
            enabled: true,
            enforce: true,
            alpha: 0.01,
            window_size: 64,
            ledger_limit: 512,
            decision_timeout_ms: 50,
            fail_closed: true,
        });

        let ctx = HostCallContext {
            runtime_name: "test",
            extension_id: Some("ext.test"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy: &policy,
            js_runtime: None,
            interceptor: None,
        };

        run_async(async {
            for idx in 0..10 {
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
                        json!({ "level": "info", "message": format!("msg-{idx}") }),
                    )
                };
                let call = HostCallPayload {
                    call_id: format!("call-feature-det-{idx}"),
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

        manager
            .runtime_hostcall_telemetry_snapshot()
            .into_iter()
            .map(|entry| entry.features)
            .collect::<Vec<_>>()
    };

    let first = run_trace();
    let second = run_trace();
    assert_eq!(first.len(), second.len(), "trace lengths should match");
    assert_eq!(
        first, second,
        "identical traces must yield identical feature vectors"
    );
}

#[test]
fn runtime_hostcall_feature_extraction_overhead_stays_bounded() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 512,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    run_async(async {
        for idx in 0..96 {
            let call = HostCallPayload {
                call_id: format!("call-feature-budget-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("budget-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_snapshot();
    assert!(!telemetry.is_empty(), "telemetry should not be empty");

    let total_us = telemetry
        .iter()
        .map(|entry| u128::from(entry.extraction_latency_us))
        .sum::<u128>();
    let avg_us = total_us / u128::try_from(telemetry.len()).unwrap_or(1);
    let max_us = telemetry
        .iter()
        .map(|entry| entry.extraction_latency_us)
        .max()
        .unwrap_or(0);
    let budget = u128::from(RUNTIME_HOSTCALL_FEATURE_BUDGET_US);

    assert!(
        avg_us <= budget * 6,
        "average extraction overhead must remain bounded: avg={avg_us}us budget={RUNTIME_HOSTCALL_FEATURE_BUDGET_US}us"
    );
    assert!(
        u128::from(max_us) <= budget * 30,
        "worst-case extraction overhead too high: max={max_us}us budget={RUNTIME_HOSTCALL_FEATURE_BUDGET_US}us"
    );
}

#[test]
fn runtime_hostcall_telemetry_schema_is_backward_readable() {
    let raw = json!({
        "schema": RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION,
        "ts_ms": 1,
        "extension_id": "ext.legacy",
        "call_id": "legacy-1",
        "capability": "log",
        "method": "log",
        "params_hash": "abc",
        "args_shape_hash": "def",
        "resource_target_class": "telemetry.log",
        "policy_reason": "permissive",
        "policy_profile": "permissive",
        "latency_ms": 3,
        "outcome": "success",
        "selected_action": "allow",
        "reason_codes": []
    });
    let parsed: RuntimeHostcallTelemetryEvent =
        serde_json::from_value(raw).expect("deserialize legacy-compatible telemetry event");
    assert_eq!(
        parsed.features.schema,
        RUNTIME_HOSTCALL_FEATURE_SCHEMA_VERSION
    );
    assert_eq!(
        parsed.extraction_budget_us,
        RUNTIME_HOSTCALL_FEATURE_BUDGET_US
    );
    assert_eq!(
        parsed.redaction_summary,
        "params redacted via hash-only telemetry"
    );
    assert_eq!(
        parsed.explanation_level,
        RuntimeRiskExplanationLevelValue::Standard
    );
    assert_eq!(parsed.top_contributors.len(), 0);
    assert!(!parsed.budget_state.exhausted);
    assert_eq!(
        parsed.budget_state.term_budget,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET
    );
}

#[test]
fn runtime_risk_explanation_order_is_deterministic_with_ties() {
    let features = RuntimeHostcallFeatureVector::default();
    let posterior = RuntimeRiskPosterior {
        safe_fast: 0.0,
        suspicious: 0.0,
        unsafe_: 0.0,
    };
    let expected_loss = RuntimeRiskExpectedLoss {
        allow: 0.0,
        harden: 0.0,
        deny: 0.0,
        terminate: 0.0,
    };
    let triggers = vec!["zeta".to_string(), "alpha".to_string()];

    let (level_a, summary_a, contributors_a, budget_a) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.0,
        &posterior,
        &expected_loss,
        &features,
        &triggers,
        None,
        32,
        1_000,
    );
    let (level_b, summary_b, contributors_b, budget_b) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.0,
        &posterior,
        &expected_loss,
        &features,
        &triggers,
        None,
        32,
        1_000,
    );

    assert_eq!(level_a, RuntimeRiskExplanationLevelValue::Standard);
    assert_eq!(level_a, level_b);
    assert_eq!(summary_a, summary_b);
    assert_eq!(contributors_a, contributors_b);
    assert!(budget_a.elapsed_ms <= budget_a.time_budget_ms);
    assert!(budget_b.elapsed_ms <= budget_b.time_budget_ms);
    let mut comparable_budget_a = budget_a.clone();
    let mut comparable_budget_b = budget_b;
    comparable_budget_a.elapsed_ms = 0;
    comparable_budget_b.elapsed_ms = 0;
    assert_eq!(comparable_budget_a, comparable_budget_b);
    assert!(!budget_a.exhausted);
    assert!(
        contributors_a.len() >= 2,
        "expected at least two contributors for trigger tie test"
    );
    assert_eq!(contributors_a[0].code, "trigger_alpha");
    assert_eq!(contributors_a[1].code, "trigger_zeta");
}

#[test]
fn runtime_risk_explanation_budget_exhaustion_falls_back_conservatively() {
    let features = RuntimeHostcallFeatureVector::default();
    let posterior = RuntimeRiskPosterior {
        safe_fast: 0.1,
        suspicious: 0.2,
        unsafe_: 0.3,
    };
    let expected_loss = RuntimeRiskExpectedLoss {
        allow: 1.0,
        harden: 0.9,
        deny: 0.8,
        terminate: 0.7,
    };
    let triggers = (0..16)
        .map(|idx| format!("trigger-{idx}"))
        .collect::<Vec<_>>();

    let (level, summary, contributors, budget) = runtime_risk_build_explanation(
        RuntimeRiskAction::Deny,
        0.7,
        &posterior,
        &expected_loss,
        &features,
        &triggers,
        Some("decision_timeout"),
        2,
        1_000,
    );

    assert_eq!(level, RuntimeRiskExplanationLevelValue::Compact);
    assert!(budget.exhausted);
    assert!(budget.fallback_mode);
    assert_eq!(budget.term_budget, 2);
    assert_eq!(contributors.len(), 2);
    assert_eq!(contributors[0].code, "action_deny");
    assert_eq!(contributors[1].code, "budget_exhausted");
    assert!(
        summary.contains("conservative_explanation_fallback=true"),
        "expected conservative fallback summary, got: {summary}"
    );
}

// ========================================================================
// Quantile selection semantics: edge-case coverage (bd-xqipg)
// ========================================================================

#[test]
fn quantile_empty_input_returns_zero() {
    let result = runtime_risk_quantile(vec![], 0.5);
    assert!(
        (result - 0.0).abs() < f64::EPSILON,
        "empty input should return 0.0, got {result}"
    );
}

#[test]
fn quantile_single_element_bd_xqipg_full() {
    let result = runtime_risk_quantile(vec![0.42], 0.5);
    assert!(
        (result - 0.42).abs() < f64::EPSILON,
        "single-element input should return that element, got {result}"
    );
}

#[test]
fn quantile_q0_returns_minimum() {
    let result = runtime_risk_quantile(vec![0.3, 0.1, 0.5, 0.9, 0.7], 0.0);
    assert!(
        (result - 0.1).abs() < f64::EPSILON,
        "q=0 should return minimum value 0.1, got {result}"
    );
}

#[test]
fn quantile_q1_returns_maximum() {
    let result = runtime_risk_quantile(vec![0.3, 0.1, 0.5, 0.9, 0.7], 1.0);
    assert!(
        (result - 0.9).abs() < f64::EPSILON,
        "q=1 should return maximum value 0.9, got {result}"
    );
}

#[test]
fn quantile_odd_sample_count_median_bd_xqipg_full() {
    // 5 elements sorted: [0.1, 0.3, 0.5, 0.7, 0.9]
    // q=0.5 → idx = round((5-1)*0.5) = round(2.0) = 2 → values[2] = 0.5
    let result = runtime_risk_quantile(vec![0.9, 0.1, 0.7, 0.3, 0.5], 0.5);
    assert!(
        (result - 0.5).abs() < f64::EPSILON,
        "odd-count median should be 0.5, got {result}"
    );
}

#[test]
fn quantile_even_sample_count_median_bd_xqipg_full() {
    // 4 elements sorted: [0.1, 0.3, 0.7, 0.9]
    // q=0.5 → idx = round((4-1)*0.5) = round(1.5) = 2 → values[2] = 0.7
    let result = runtime_risk_quantile(vec![0.9, 0.1, 0.7, 0.3], 0.5);
    assert!(
        (result - 0.7).abs() < f64::EPSILON,
        "even-count median should be 0.7, got {result}"
    );
}

#[test]
fn quantile_duplicate_values_bd_xqipg_full() {
    let result = runtime_risk_quantile(vec![0.5, 0.5, 0.5, 0.5], 0.75);
    assert!(
        (result - 0.5).abs() < f64::EPSILON,
        "all-duplicate input should return 0.5 for any q, got {result}"
    );
}

#[test]
fn quantile_negative_q_clamps_to_zero() {
    // runtime_risk_clamp01(-0.5) → 0.0, so q=0 → minimum
    let result = runtime_risk_quantile(vec![0.2, 0.4, 0.6, 0.8], -0.5);
    assert!(
        (result - 0.2).abs() < f64::EPSILON,
        "negative q should clamp to 0 (minimum), got {result}"
    );
}

#[test]
fn quantile_q_greater_than_one_clamps_to_one() {
    // runtime_risk_clamp01(2.0) → 1.0, so q=1 → maximum
    let result = runtime_risk_quantile(vec![0.2, 0.4, 0.6, 0.8], 2.0);
    assert!(
        (result - 0.8).abs() < f64::EPSILON,
        "q>1 should clamp to 1 (maximum), got {result}"
    );
}

#[test]
fn quantile_nan_q_treated_as_zero_bd_xqipg_full() {
    // runtime_risk_clamp01(NaN) → 0.0, so returns minimum
    let result = runtime_risk_quantile(vec![0.2, 0.4, 0.6], f64::NAN);
    assert!(
        (result - 0.2).abs() < f64::EPSILON,
        "NaN q should be treated as 0 (minimum), got {result}"
    );
}

#[test]
fn quantile_nan_values_sorted_consistently() {
    // NaN sorts to beginning due to partial_cmp unwrap_or(Equal)
    let result = runtime_risk_quantile(vec![0.5, f64::NAN, 0.3], 1.0);
    // After sort with NaN as Equal: order depends on sort stability.
    // Key invariant: function does not panic.
    assert!(
        result.is_nan() || result.is_finite(),
        "should not panic on NaN values"
    );
}

#[test]
fn quantile_inf_values_handled() {
    let result = runtime_risk_quantile(vec![0.1, f64::INFINITY, 0.5], 1.0);
    assert!(
        result == f64::INFINITY,
        "q=1 with INFINITY should return INFINITY, got {result}"
    );
}

#[test]
fn quantile_large_input_deterministic() {
    let values: Vec<f64> = (0..1000).map(|i| f64::from(i) / 1000.0).collect();
    let result_a = runtime_risk_quantile(values.clone(), 0.95);
    let result_b = runtime_risk_quantile(values, 0.95);
    assert!(
        (result_a - result_b).abs() < f64::EPSILON,
        "quantile must be deterministic across runs"
    );
    // q=0.95, idx = round((999)*0.95) = round(949.05) = 949 → values[949] = 0.949
    assert!(
        (result_a - 0.949).abs() < f64::EPSILON,
        "expected 0.949 for large sorted input at q=0.95, got {result_a}"
    );
}

#[test]
fn quantile_two_elements() {
    // 2 elements sorted: [0.1, 0.9]
    // q=0.0 → idx = round(1*0) = 0 → 0.1
    // q=0.5 → idx = round(1*0.5) = round(0.5) = 0 → 0.1 (banker's rounding)
    // q=1.0 → idx = round(1*1) = 1 → 0.9
    let min = runtime_risk_quantile(vec![0.9, 0.1], 0.0);
    let max = runtime_risk_quantile(vec![0.9, 0.1], 1.0);
    assert!(
        (min - 0.1).abs() < f64::EPSILON,
        "q=0 with 2 elements should return min, got {min}"
    );
    assert!(
        (max - 0.9).abs() < f64::EPSILON,
        "q=1 with 2 elements should return max, got {max}"
    );
}

#[test]
fn quantile_boundary_quartiles() {
    // 5 elements sorted: [0.0, 0.25, 0.5, 0.75, 1.0]
    // q=0.25 → idx = round(4*0.25) = round(1.0) = 1 → 0.25
    // q=0.75 → idx = round(4*0.75) = round(3.0) = 3 → 0.75
    let q25 = runtime_risk_quantile(vec![0.5, 0.0, 1.0, 0.75, 0.25], 0.25);
    let q75 = runtime_risk_quantile(vec![0.5, 0.0, 1.0, 0.75, 0.25], 0.75);
    assert!(
        (q25 - 0.25).abs() < f64::EPSILON,
        "25th percentile should be 0.25, got {q25}"
    );
    assert!(
        (q75 - 0.75).abs() < f64::EPSILON,
        "75th percentile should be 0.75, got {q75}"
    );
}

#[test]
fn quantile_conformal_alpha_001() {
    // Mimics the actual conformal prediction use: q = 1 - alpha = 0.99
    // 10 residuals: [0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.10]
    // q=0.99 → idx = round(9*0.99) = round(8.91) = 9 → 0.10
    let residuals: Vec<f64> = (1..=10).map(|i| f64::from(i) / 100.0).collect();
    let result = runtime_risk_quantile(residuals, 0.99);
    assert!(
        (result - 0.10).abs() < f64::EPSILON,
        "99th percentile of 10 residuals should be 0.10, got {result}"
    );
}

// ========================================================================
// Per-extension override tests at hostcall boundary (bd-k5q5.4.3)
// ========================================================================

#[test]
fn shared_dispatch_per_extension_deny_overrides_global_allow() {
    // Global policy allows "read", but ext.test has a per-extension deny
    // for "read". The dispatch boundary should deny the call.
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let mut policy = permissive_policy();
    policy.per_extension.insert(
        "ext.test".to_string(),
        ExtensionOverride {
            deny: vec!["read".to_string()],
            ..Default::default()
        },
    );
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-ext-deny".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": { "path": "/tmp/test" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "expected denial from per-extension override"
        );
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("denied"),
            "message should mention denial, got: {}",
            err.message
        );
    });
}

#[test]
fn shared_dispatch_per_extension_allow_overrides_global_deny() {
    // Global policy denies "exec" (in deny_caps), but ext.trusted has a
    // per-extension allow for "exec". The dispatch boundary should allow it.
    // (It will fail downstream because no actual tool, but we check it's
    // not denied by policy.)
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: vec!["read".to_string()],
        deny_caps: vec!["exec".to_string()],
        per_extension: HashMap::new(),
        ..Default::default()
    };
    policy.per_extension.insert(
        "ext.trusted".to_string(),
        ExtensionOverride {
            allow: vec!["exec".to_string()],
            ..Default::default()
        },
    );
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.trusted"),
        tools: &tools,
        http: &http,
        manager: None,
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "call-ext-allow".to_string(),
        capability: "exec".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "exec", "input": { "command": "echo hi" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // Not denied by policy — may fail downstream (no tool registered),
        // but the error code should NOT be Denied.
        if result.is_error {
            let err = result.error.as_ref().expect("expected error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "per-extension allow should override global deny, got: {}",
                err.message
            );
        }
    });
}

#[test]
fn shared_dispatch_per_extension_deny_does_not_affect_other_extensions() {
    // ext.restricted has "read" denied, but ext.normal (ctx extension_id)
    // should still be allowed to read.
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let mut policy = permissive_policy();
    policy.per_extension.insert(
        "ext.restricted".to_string(),
        ExtensionOverride {
            deny: vec!["read".to_string()],
            ..Default::default()
        },
    );
    // ctx uses ext.test (not ext.restricted), so override should not apply
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-other-ext".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": { "path": "/tmp/test" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // Should NOT be denied — the deny override is for ext.restricted, not ext.test
        if result.is_error {
            let err = result.error.as_ref().expect("expected error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "deny for ext.restricted should not affect ext.test, got: {}",
                err.message
            );
        }
    });
}

#[test]
fn shared_dispatch_per_extension_mode_override_applies() {
    // Global mode is Strict (fallback → Deny), but ext.test has mode
    // overridden to Permissive. A capability not in any allow/deny list
    // should fall through to the effective mode and be allowed.
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        per_extension: HashMap::new(),
        ..Default::default()
    };
    policy.per_extension.insert(
        "ext.test".to_string(),
        ExtensionOverride {
            mode: Some(ExtensionPolicyMode::Permissive),
            ..Default::default()
        },
    );
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-mode-override".to_string(),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": "test" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // With Strict mode globally, "log" would be denied. But ext.test
        // overrides to Permissive, so it should be allowed (may fail
        // downstream for other reasons, but not denied).
        if result.is_error {
            let err = result.error.as_ref().expect("expected error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "per-extension mode override to Permissive should allow 'log', got: {}",
                err.message
            );
        }
    });
}

#[test]
fn shared_dispatch_unsupported_method_returns_invalid_request() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-bad-method".to_string(),
        capability: "unknown_cap".to_string(),
        method: "nonsense_method".to_string(),
        params: json!({}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
        assert!(
            err.message.contains("Unknown or invalid host call method")
                || err.message.contains("Unsupported hostcall method"),
            "unexpected error message: {}",
            err.message
        );
    });
}

#[test]
fn shared_dispatch_session_without_manager_returns_denied() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    // ctx.manager is None → session/ui/events should return "denied"
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "call-session".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "get_state" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    });
}
