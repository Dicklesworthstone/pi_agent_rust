use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionBudgetControllerConfig, ExtensionBudgetTier, ExtensionManager, ExtensionPolicy,
    ExtensionPolicyMode, ExtensionQuotaConfig, HostCallContext, HostCallPayload, OcoTunerConfig,
    OcoTunerSnapshot, RegimeShiftConfig, RuntimeRiskConfig, SafetyEnvelopeConfig,
    dispatch_host_call_shared,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::path::Path;

const EXT_ID: &str = "ext.oco.regret.rollback";

#[derive(Debug)]
struct OcoRun {
    snapshot: OcoTunerSnapshot,
    quota_error_message: String,
}

fn permissive_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    }
}

fn oco_budget_config(rollback_loss_threshold: f64) -> ExtensionBudgetControllerConfig {
    ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 100,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: true,
            learning_rate: 0.1,
            min_queue_budget: 4.0,
            max_queue_budget: 16.0,
            min_batch_budget: 2.0,
            max_batch_budget: 16.0,
            min_time_slice_ms: 4.0,
            max_time_slice_ms: 24.0,
            initial_queue_budget: 8.0,
            initial_batch_budget: 4.0,
            initial_time_slice_ms: 8.0,
            rollback_loss_threshold,
        },
    }
}

fn log_call(call_id: &str, message: &str) -> HostCallPayload {
    HostCallPayload {
        call_id: call_id.to_string(),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({
            "level": "info",
            "event": "oco_regret_rollback_probe",
            "message": message,
        }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn run_quota_loss_scenario(rollback_loss_threshold: f64) -> OcoRun {
    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tools = ToolRegistry::new(&[], cwd, None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: false,
        ..Default::default()
    });
    manager.set_quota_config(ExtensionQuotaConfig {
        max_hostcalls_total: Some(1),
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        max_subprocesses: None,
        max_write_bytes: None,
        max_http_requests: None,
    });
    manager.set_budget_controller_config(oco_budget_config(rollback_loss_threshold));
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "oco-regret-rollback-test",
        extension_id: Some(EXT_ID),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    let quota_error_message = futures::executor::block_on(async {
        let baseline = dispatch_host_call_shared(&ctx, log_call("oco-baseline", "baseline")).await;
        assert!(
            !baseline.is_error,
            "first host call should establish quota baseline without error: {baseline:?}"
        );

        let spike = dispatch_host_call_shared(&ctx, log_call("oco-spike", "quota spike")).await;
        assert!(
            spike.is_error,
            "second host call must breach the one-call quota and feed OCO loss"
        );
        let quota_error_message = spike
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_default();
        assert!(
            quota_error_message.contains("Quota exceeded"),
            "OCO scenario must be driven by the quota overload path, got: {quota_error_message}"
        );
        quota_error_message
    });

    let snapshot = manager.oco_tuner_snapshot(EXT_ID);
    assert!(
        snapshot.is_some(),
        "quota overload should initialize OCO tuner state"
    );
    OcoRun {
        snapshot: snapshot.unwrap_or(OcoTunerSnapshot {
            queue_budget: f64::NAN,
            batch_budget: f64::NAN,
            time_slice_ms: f64::NAN,
            rounds: 0,
            cumulative_loss: f64::NAN,
            cumulative_regret: f64::NAN,
            guardrail_rollbacks: 0,
        }),
        quota_error_message,
    }
}

#[test]
fn loss_spike_above_threshold_rolls_back_to_safe_profile() {
    let run = run_quota_loss_scenario(0.5);

    assert_eq!(run.snapshot.rounds, 1);
    assert_eq!(run.snapshot.guardrail_rollbacks, 1);
    assert!(
        run.quota_error_message.contains("Quota exceeded"),
        "scenario sanity check failed"
    );
    assert!(
        (run.snapshot.queue_budget - 8.0).abs() <= f64::EPSILON,
        "rollback must restore the queue budget to the configured safe profile: {run:?}"
    );
    assert!(
        (run.snapshot.batch_budget - 4.0).abs() <= f64::EPSILON,
        "rollback must restore the batch budget to the configured safe profile: {run:?}"
    );
    assert!(
        (run.snapshot.time_slice_ms - 8.0).abs() <= f64::EPSILON,
        "rollback must restore the time-slice budget to the configured safe profile: {run:?}"
    );
}

#[test]
fn loss_below_threshold_updates_regret_without_rollback() {
    let run = run_quota_loss_scenario(1.5);

    assert_eq!(run.snapshot.rounds, 1);
    assert_eq!(run.snapshot.guardrail_rollbacks, 0);
    assert!(
        run.snapshot.queue_budget > 8.0,
        "below-threshold overloaded update should adapt queue budget instead of rolling back: {run:?}"
    );
    assert!(
        run.snapshot.batch_budget > 4.0,
        "below-threshold overloaded update should adapt batch budget instead of rolling back: {run:?}"
    );
    assert!(
        run.snapshot.time_slice_ms > 8.0,
        "below-threshold overloaded update should adapt time-slice budget instead of rolling back: {run:?}"
    );
    assert!(
        run.snapshot.cumulative_loss.is_finite() && run.snapshot.cumulative_loss > 0.0,
        "OCO loss accounting should remain finite and positive: {run:?}"
    );
    assert!(
        run.snapshot.cumulative_regret.is_finite(),
        "OCO regret accounting should remain finite: {run:?}"
    );
}
