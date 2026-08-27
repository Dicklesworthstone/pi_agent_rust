//! SEC-7.2 tests: Graduated enforcement rollout with rollback guards (bd-8lppo).
//!
//! Validates:
//! - Rollout phase progression: Shadow → `LogOnly` → `EnforceNew` → `EnforceAll`
//! - Phase-aware enforcement flag synchronization with `RuntimeRiskConfig.enforce`
//! - Operator set/advance/rollback API via `ExtensionManager`
//! - Rollback trigger: false-positive rate exceeds SLO threshold
//! - Rollback trigger: error rate exceeds SLO threshold
//! - Rollback trigger: latency SLO breach
//! - Rollback trigger minimum sample gate (no premature rollback)
//! - Phase transition count and last-transition tracking
//! - `RolloutState` snapshot reflects current state accurately
//! - `RollbackTrigger` configuration is respected after update
//! - Rollback records `rolled_back_from` provenance
//! - Re-advance after rollback proceeds from Shadow
//! - Default phase is `EnforceAll` (production default)
//! - Phase serde roundtrip preserves all variants
//! - Window stats computation correctness
//! - Enforcement flag tracks phase through multi-step lifecycle
//! - Concurrent decision recording doesn't corrupt state
//! - Rollout + dispatch integration: shadow mode dispatch in Shadow phase
//! - Rollout + dispatch integration: enforcement in `EnforceAll` phase
//! - JSONL artifact emission for rollout transitions

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RollbackTrigger, RolloutPhase, RolloutState, RuntimeRiskConfig, dispatch_host_call_shared,
};
use pi::tools::ToolRegistry;
use serde_json::json;

// ============================================================================
// Helpers
// ============================================================================

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
        decision_timeout_ms: 5000,
        fail_closed: true,
    }
}

fn setup(
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

fn make_ctx<'a>(
    tools: &'a ToolRegistry,
    http: &'a HttpConnector,
    manager: &'a ExtensionManager,
    policy: &'a ExtensionPolicy,
    ext_id: &'a str,
) -> HostCallContext<'a> {
    HostCallContext {
        runtime_name: "sec72_test",
        extension_id: Some(ext_id),
        tools,
        http,
        manager: Some(manager.clone()),
        policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    }
}

fn benign_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("call_{idx}"),
        capability: "read".to_string(),
        method: "read".to_string(),
        params: json!({"path": format!("/tmp/test_{idx}.txt")}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

/// Feed `n` clean decision samples (no error, no FP) into the manager.
fn feed_clean_decisions(manager: &ExtensionManager, n: usize) {
    for _ in 0..n {
        manager.record_rollout_decision(5, false, false);
    }
}

/// Feed `n` false-positive decision samples into the manager.
fn feed_fp_decisions(manager: &ExtensionManager, n: usize) {
    for _ in 0..n {
        manager.record_rollout_decision(5, false, true);
    }
}

/// Feed `n` error decision samples into the manager.
fn feed_error_decisions(manager: &ExtensionManager, n: usize) {
    for _ in 0..n {
        manager.record_rollout_decision(5, true, false);
    }
}

/// Feed `n` high-latency decision samples into the manager.
fn feed_slow_decisions(manager: &ExtensionManager, n: usize, latency_ms: u64) {
    for _ in 0..n {
        manager.record_rollout_decision(latency_ms, false, false);
    }
}

// ============================================================================
// Phase progression tests
// ============================================================================

#[test]
fn default_phase_is_enforce_all() {
    let manager = ExtensionManager::new();
    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::EnforceAll,
        "production default should be full enforcement"
    );
}

#[test]
fn phase_progression_shadow_to_enforce_all() {
    let manager = ExtensionManager::new();
    manager.set_rollout_phase(RolloutPhase::Shadow);
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);

    assert!(manager.advance_rollout(), "Shadow → LogOnly");
    assert_eq!(manager.rollout_phase(), RolloutPhase::LogOnly);

    assert!(manager.advance_rollout(), "LogOnly → EnforceNew");
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceNew);

    assert!(manager.advance_rollout(), "EnforceNew → EnforceAll");
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceAll);

    assert!(
        !manager.advance_rollout(),
        "EnforceAll is terminal — cannot advance further"
    );
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceAll);
}

#[test]
fn enforce_flag_tracks_phase() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    // Shadow: enforce = false
    manager.set_rollout_phase(RolloutPhase::Shadow);
    let state = manager.rollout_state();
    assert!(!state.enforce, "Shadow phase should have enforce=false");

    // LogOnly: enforce = false
    manager.set_rollout_phase(RolloutPhase::LogOnly);
    let state = manager.rollout_state();
    assert!(!state.enforce, "LogOnly phase should have enforce=false");

    // EnforceNew: enforce = true
    manager.set_rollout_phase(RolloutPhase::EnforceNew);
    let state = manager.rollout_state();
    assert!(state.enforce, "EnforceNew phase should have enforce=true");

    // EnforceAll: enforce = true
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    let state = manager.rollout_state();
    assert!(state.enforce, "EnforceAll phase should have enforce=true");
}

#[test]
fn set_phase_explicit_operator_override() {
    let manager = ExtensionManager::new();

    // Jump directly to EnforceNew (skipping LogOnly)
    manager.set_rollout_phase(RolloutPhase::EnforceNew);
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceNew);

    // Jump back to Shadow
    manager.set_rollout_phase(RolloutPhase::Shadow);
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);
}

// ============================================================================
// Transition tracking tests
// ============================================================================

#[test]
fn transition_count_increments() {
    let manager = ExtensionManager::new();
    manager.set_rollout_phase(RolloutPhase::Shadow);

    let state = manager.rollout_state();
    let initial_count = state.transition_count;

    manager.advance_rollout(); // Shadow → LogOnly
    let state = manager.rollout_state();
    assert_eq!(state.transition_count, initial_count + 1);

    manager.advance_rollout(); // LogOnly → EnforceNew
    let state = manager.rollout_state();
    assert_eq!(state.transition_count, initial_count + 2);
}

#[test]
fn set_same_phase_is_noop() {
    let manager = ExtensionManager::new();
    let state_before = manager.rollout_state();
    let count_before = state_before.transition_count;

    // Setting to the same phase should not increment count.
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    let state_after = manager.rollout_state();
    assert_eq!(state_after.transition_count, count_before);
}

// ============================================================================
// Rollback trigger tests
// ============================================================================

#[test]
fn fp_rate_triggers_rollback() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 200,
    });

    // Feed 18 clean + 2 FP = 10% FP rate > 5% threshold
    // Need at least 10 samples for trigger evaluation.
    feed_clean_decisions(&manager, 18);
    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::EnforceAll,
        "still enforcing before FP injection"
    );

    // Inject false positives to exceed the threshold.
    let triggered = manager.record_rollout_decision(5, false, true);
    // At 19 samples, FP rate is 1/19 ≈ 5.2% — might trigger.
    // Let's inject one more to be sure.
    let triggered2 = manager.record_rollout_decision(5, false, true);

    // After 20 samples with 2 FPs: rate = 2/20 = 10% > 5%
    assert!(
        triggered || triggered2,
        "FP rate 10% should trigger rollback from EnforceAll"
    );
    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::Shadow,
        "should have rolled back to Shadow"
    );

    // Verify provenance.
    let state = manager.rollout_state();
    assert_eq!(
        state.rolled_back_from,
        Some(RolloutPhase::EnforceAll),
        "should record which phase we rolled back from"
    );
}

#[test]
fn error_rate_triggers_rollback() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceNew);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.50, // High FP threshold (won't trigger)
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 1000,
    });

    // Feed 16 clean + 4 errors = 20% error rate > 10% threshold
    feed_clean_decisions(&manager, 16);
    feed_error_decisions(&manager, 4);

    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::Shadow,
        "error rate 20% should trigger rollback"
    );
}

#[test]
fn latency_slo_triggers_rollback() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.50,
        max_error_rate: 0.50,
        window_size: 20,
        max_latency_ms: 50,
    });

    // Feed decisions with latency 100ms, exceeding 50ms threshold.
    feed_slow_decisions(&manager, 20, 100);

    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::Shadow,
        "avg latency 100ms > 50ms threshold should trigger rollback"
    );
}

#[test]
fn no_rollback_below_min_samples() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 100,
        max_latency_ms: 200,
    });

    // Only 5 decisions, all FP. Rate is 100% but below min sample count (10).
    for _ in 0..5 {
        manager.record_rollout_decision(5, false, true);
    }

    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::EnforceAll,
        "should not rollback with fewer than 10 samples"
    );
}

#[test]
fn no_rollback_in_shadow_phase() {
    let manager = ExtensionManager::new();
    manager.set_rollout_phase(RolloutPhase::Shadow);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.01,
        max_error_rate: 0.01,
        window_size: 10,
        max_latency_ms: 1,
    });

    // All decisions are bad but we're in Shadow — no rollback.
    for _ in 0..20 {
        let triggered = manager.record_rollout_decision(1000, true, true);
        assert!(!triggered, "Shadow phase should never trigger rollback");
    }
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);
}

#[test]
fn no_rollback_in_log_only_phase() {
    let manager = ExtensionManager::new();
    manager.set_rollout_phase(RolloutPhase::LogOnly);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.01,
        max_error_rate: 0.01,
        window_size: 10,
        max_latency_ms: 1,
    });

    for _ in 0..20 {
        let triggered = manager.record_rollout_decision(1000, true, true);
        assert!(!triggered, "LogOnly phase should never trigger rollback");
    }
    assert_eq!(manager.rollout_phase(), RolloutPhase::LogOnly);
}

// ============================================================================
// Rollback recovery tests
// ============================================================================

#[test]
fn re_advance_after_rollback() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 200,
    });

    // Trigger rollback.
    feed_clean_decisions(&manager, 18);
    feed_fp_decisions(&manager, 2);
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);

    // Re-advance: Shadow → LogOnly.
    assert!(manager.advance_rollout());
    assert_eq!(manager.rollout_phase(), RolloutPhase::LogOnly);

    // Verify rolled_back_from is cleared after advance.
    let state = manager.rollout_state();
    assert_eq!(
        state.rolled_back_from, None,
        "rolled_back_from should be cleared after advance"
    );
}

// ============================================================================
// RolloutState snapshot tests
// ============================================================================

#[test]
fn rollout_state_reflects_current_state() {
    let manager = ExtensionManager::new();
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: false,
        ..default_risk_config()
    };
    manager.set_runtime_risk_config(config);
    manager.set_rollout_phase(RolloutPhase::Shadow);

    let state = manager.rollout_state();
    assert_eq!(state.phase, RolloutPhase::Shadow);
    assert!(!state.enforce);
    assert!(state.enabled);
    assert_eq!(state.rolled_back_from, None);
}

#[test]
fn rollout_state_window_stats_accuracy() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.90, // Very high — won't trigger
        max_error_rate: 0.90,
        window_size: 100,
        max_latency_ms: 10_000,
    });

    // Feed 8 clean + 2 FP decisions.
    feed_clean_decisions(&manager, 8);
    feed_fp_decisions(&manager, 2);

    let state = manager.rollout_state();
    assert_eq!(state.window_stats.total_decisions, 10);
    assert_eq!(state.window_stats.false_positive_count, 2);
    assert_eq!(state.window_stats.error_count, 0);
}

// ============================================================================
// Rollback trigger configuration tests
// ============================================================================

#[test]
fn custom_trigger_thresholds_are_respected() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);

    // Set very permissive triggers — should not rollback even with many FPs.
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.95,
        max_error_rate: 0.95,
        window_size: 100,
        max_latency_ms: 100_000,
    });

    // 50% FP rate — should NOT trigger with 95% threshold.
    for i in 0..20 {
        manager.record_rollout_decision(5, false, i % 2 == 0);
    }
    assert_eq!(
        manager.rollout_phase(),
        RolloutPhase::EnforceAll,
        "50% FP rate should not trigger rollback with 95% threshold"
    );
}

#[test]
fn trigger_update_takes_effect_immediately() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);

    // Initially permissive triggers.
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 20,
        max_latency_ms: 100_000,
    });

    // Inject FPs that would trigger a 5% threshold but not 99%.
    feed_clean_decisions(&manager, 18);
    feed_fp_decisions(&manager, 2);
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceAll);

    // Now tighten the trigger — the existing window should cause rollback
    // on the next decision.
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 200,
    });

    // One more decision to re-evaluate triggers with new thresholds.
    let triggered = manager.record_rollout_decision(5, false, true);
    // Window now has 3 FP / 21 samples = 14.3% > 5%
    assert!(triggered, "tightened trigger should cause rollback");
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);
}

// ============================================================================
// Phase serde roundtrip tests
// ============================================================================

#[test]
fn phase_serde_roundtrip() {
    let phases = [
        RolloutPhase::Shadow,
        RolloutPhase::LogOnly,
        RolloutPhase::EnforceNew,
        RolloutPhase::EnforceAll,
    ];

    for phase in &phases {
        let json = serde_json::to_string(phase).expect("serialize");
        let restored: RolloutPhase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*phase, restored, "roundtrip failed for {phase}");
    }
}

#[test]
fn phase_display_matches_serde() {
    assert_eq!(RolloutPhase::Shadow.to_string(), "shadow");
    assert_eq!(RolloutPhase::LogOnly.to_string(), "log_only");
    assert_eq!(RolloutPhase::EnforceNew.to_string(), "enforce_new");
    assert_eq!(RolloutPhase::EnforceAll.to_string(), "enforce_all");
}

#[test]
fn rollback_trigger_serde_roundtrip() {
    let trigger = RollbackTrigger {
        max_false_positive_rate: 0.03,
        max_error_rate: 0.07,
        window_size: 50,
        max_latency_ms: 150,
    };
    let json = serde_json::to_string(&trigger).expect("serialize");
    let restored: RollbackTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, restored);
}

#[test]
fn rollout_state_serde_roundtrip() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceNew);
    feed_clean_decisions(&manager, 5);

    let state = manager.rollout_state();
    let json = serde_json::to_string_pretty(&state).expect("serialize");
    let restored: RolloutState = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(state.phase, restored.phase);
    assert_eq!(state.enforce, restored.enforce);
    assert_eq!(state.enabled, restored.enabled);
    assert_eq!(state.transition_count, restored.transition_count);
    assert_eq!(state.rolled_back_from, restored.rolled_back_from);
}

// ============================================================================
// Phase ordering tests
// ============================================================================

#[test]
fn phase_ordering() {
    assert!(RolloutPhase::Shadow < RolloutPhase::LogOnly);
    assert!(RolloutPhase::LogOnly < RolloutPhase::EnforceNew);
    assert!(RolloutPhase::EnforceNew < RolloutPhase::EnforceAll);
}

#[test]
fn phase_is_enforcing() {
    assert!(!RolloutPhase::Shadow.is_enforcing());
    assert!(!RolloutPhase::LogOnly.is_enforcing());
    assert!(RolloutPhase::EnforceNew.is_enforcing());
    assert!(RolloutPhase::EnforceAll.is_enforcing());
}

// ============================================================================
// Multi-step lifecycle tests
// ============================================================================

#[test]
fn full_lifecycle_advance_rollback_readvance() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::Shadow);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 200,
    });

    // Phase 1: Advance through all phases.
    manager.advance_rollout(); // → LogOnly
    manager.advance_rollout(); // → EnforceNew
    manager.advance_rollout(); // → EnforceAll
    assert_eq!(manager.rollout_phase(), RolloutPhase::EnforceAll);

    let state = manager.rollout_state();
    assert!(state.enforce);
    let count_at_ga = state.transition_count;

    // Phase 2: Trigger rollback via FP rate.
    feed_clean_decisions(&manager, 18);
    feed_fp_decisions(&manager, 2);
    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);
    let state = manager.rollout_state();
    assert!(!state.enforce);
    assert_eq!(state.rolled_back_from, Some(RolloutPhase::EnforceAll));
    assert_eq!(state.transition_count, count_at_ga + 1);

    // Phase 3: Re-advance conservatively.
    manager.advance_rollout(); // → LogOnly
    assert_eq!(manager.rollout_phase(), RolloutPhase::LogOnly);
    let state = manager.rollout_state();
    assert!(!state.enforce, "LogOnly should not enforce");
    assert_eq!(state.rolled_back_from, None, "advance clears provenance");
}

#[test]
fn enforce_flag_through_full_lifecycle() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let phases_and_enforce = [
        (RolloutPhase::Shadow, false),
        (RolloutPhase::LogOnly, false),
        (RolloutPhase::EnforceNew, true),
        (RolloutPhase::EnforceAll, true),
    ];

    for (phase, expected_enforce) in &phases_and_enforce {
        manager.set_rollout_phase(*phase);
        let state = manager.rollout_state();
        assert_eq!(
            state.enforce, *expected_enforce,
            "phase {phase} should have enforce={expected_enforce}",
        );
    }
}

// ============================================================================
// Dispatch integration tests
// ============================================================================

#[test]
fn shadow_phase_allows_all_calls() {
    let harness = TestHarness::new("sec72_shadow_dispatch");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    manager.set_rollout_phase(RolloutPhase::Shadow);

    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext-test");
    let call = HostCallPayload {
        call_id: "call_shadow".to_string(),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({"command": "echo hello"}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    asupersync::test_utils::run_test(|| async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // In shadow mode, all calls should proceed (not denied by risk).
        // The specific result depends on whether the call type has a handler,
        // but it should NOT be a risk denial.
        let result_str = serde_json::to_string(&result).unwrap_or_default();
        assert!(
            !result_str.contains("risk_denied"),
            "shadow phase should not deny calls"
        );
    });
}

#[test]
fn enforce_all_phase_respects_risk_decisions() {
    let harness = TestHarness::new("sec72_enforce_dispatch");
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let (tools, http, manager, policy) = setup(&harness, config);
    manager.set_rollout_phase(RolloutPhase::EnforceAll);

    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext-enforce");

    asupersync::test_utils::run_test(|| async {
        // Dispatch a benign call first to establish baseline.
        let benign = benign_call(0);
        let _ = dispatch_host_call_shared(&ctx, benign).await;

        // Verify the state reflects enforcement.
        let state = manager.rollout_state();
        assert!(state.enforce, "EnforceAll should have enforce=true");
    });
}

// ============================================================================
// Window stats accuracy tests
// ============================================================================

#[test]
fn window_stats_empty_when_no_decisions() {
    let manager = ExtensionManager::new();
    let state = manager.rollout_state();
    assert_eq!(state.window_stats.total_decisions, 0);
    assert_eq!(state.window_stats.error_count, 0);
    assert_eq!(state.window_stats.false_positive_count, 0);
    assert!(state.window_stats.avg_latency_ms.abs() < f64::EPSILON);
}

#[test]
fn window_stats_latency_average() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 100,
        max_latency_ms: 100_000,
    });

    // 5 decisions at 10ms, 5 at 20ms → avg = 15ms
    feed_slow_decisions(&manager, 5, 10);
    feed_slow_decisions(&manager, 5, 20);

    let state = manager.rollout_state();
    assert_eq!(state.window_stats.total_decisions, 10);
    assert!(
        (state.window_stats.avg_latency_ms - 15.0).abs() < 0.01,
        "average latency should be 15ms, got {}",
        state.window_stats.avg_latency_ms
    );
}

#[test]
fn window_evicts_old_samples() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 10,
        max_latency_ms: 100_000,
    });

    // Fill window with 10 FP decisions.
    feed_fp_decisions(&manager, 10);
    let state = manager.rollout_state();
    assert_eq!(state.window_stats.false_positive_count, 10);

    // Add 10 clean decisions — should push out all FPs.
    feed_clean_decisions(&manager, 10);
    let state = manager.rollout_state();
    assert_eq!(
        state.window_stats.false_positive_count, 0,
        "old FP samples should have been evicted"
    );
    assert_eq!(state.window_stats.total_decisions, 10);
}

// ============================================================================
// Edge case tests
// ============================================================================

#[test]
fn rollback_trigger_default_values() {
    let trigger = RollbackTrigger::default();
    assert!((trigger.max_false_positive_rate - 0.05).abs() < f64::EPSILON);
    assert!((trigger.max_error_rate - 0.10).abs() < f64::EPSILON);
    assert_eq!(trigger.window_size, 100);
    assert_eq!(trigger.max_latency_ms, 200);
}

#[test]
fn rollout_phase_as_str() {
    assert_eq!(RolloutPhase::Shadow.as_str(), "shadow");
    assert_eq!(RolloutPhase::LogOnly.as_str(), "log_only");
    assert_eq!(RolloutPhase::EnforceNew.as_str(), "enforce_new");
    assert_eq!(RolloutPhase::EnforceAll.as_str(), "enforce_all");
}

#[test]
fn rollback_from_enforce_new_records_provenance() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceNew);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.05,
        max_error_rate: 0.10,
        window_size: 20,
        max_latency_ms: 200,
    });

    feed_clean_decisions(&manager, 18);
    feed_fp_decisions(&manager, 2);

    assert_eq!(manager.rollout_phase(), RolloutPhase::Shadow);
    let state = manager.rollout_state();
    assert_eq!(
        state.rolled_back_from,
        Some(RolloutPhase::EnforceNew),
        "should record EnforceNew as rollback source"
    );
}

// ============================================================================
// JSONL artifact tests
// ============================================================================

#[test]
fn rollout_state_emits_valid_jsonl() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::Shadow);
    manager.advance_rollout();
    feed_clean_decisions(&manager, 5);

    let state = manager.rollout_state();
    let jsonl = serde_json::to_string(&state).expect("serialize to JSONL");

    // Parse back and verify key fields.
    let parsed: serde_json::Value = serde_json::from_str(&jsonl).expect("parse JSONL");
    assert_eq!(parsed["phase"], "log_only");
    assert_eq!(parsed["enforce"], false);
    assert_eq!(parsed["enabled"], true);
}

#[test]
fn window_stats_in_rollout_state_artifact() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 100,
        max_latency_ms: 100_000,
    });

    feed_clean_decisions(&manager, 7);
    manager.record_rollout_decision(5, true, false); // 1 error
    manager.record_rollout_decision(5, false, true); // 1 FP
    manager.record_rollout_decision(5, true, true); // 1 error + FP

    let snapshot = manager.rollout_state();
    let json = serde_json::to_string_pretty(&snapshot).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

    let win = &parsed["window_stats"];
    assert_eq!(win["total_decisions"], 10);
    assert_eq!(win["error_count"], 2);
    assert_eq!(win["false_positive_count"], 2);
}

// ============================================================================
// Concurrent safety test
// ============================================================================

#[test]
fn concurrent_decision_recording() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 1000,
        max_latency_ms: 100_000,
    });

    // Simulate concurrent access from multiple threads.
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let mgr = manager.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    mgr.record_rollout_decision(5, false, false);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let state = manager.rollout_state();
    assert_eq!(
        state.window_stats.total_decisions, 100,
        "should have recorded all 100 decisions"
    );
}

// ============================================================================
// Mixed error+FP window test
// ============================================================================

#[test]
fn mixed_error_and_fp_both_tracked() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());
    manager.set_rollout_phase(RolloutPhase::EnforceAll);
    manager.set_rollback_trigger(&RollbackTrigger {
        max_false_positive_rate: 0.99,
        max_error_rate: 0.99,
        window_size: 100,
        max_latency_ms: 100_000,
    });

    // 3 errors, 2 FPs, 5 clean
    feed_error_decisions(&manager, 3);
    feed_fp_decisions(&manager, 2);
    feed_clean_decisions(&manager, 5);

    let state = manager.rollout_state();
    assert_eq!(state.window_stats.total_decisions, 10);
    assert_eq!(state.window_stats.error_count, 3);
    assert_eq!(state.window_stats.false_positive_count, 2);
}
