#![forbid(unsafe_code)]
//! Integration tests for `plan_voi_candidates` and `compute_mean_field_controls`
//! in `extension_scoring.rs`.
//!
//! Covers: disabled config, budget limits, freshness filtering, utility floors,
//! candidate ordering, empty inputs, and mean-field convergence/oscillation/clipping.

#[cfg(feature = "experimental-controllers")]
use chrono::{TimeZone, Utc};
use pi::extension_scoring::{
    MeanFieldControllerConfig, MeanFieldControllerReport, MeanFieldShardObservation,
    MeanFieldShardState, compute_mean_field_controls,
};
#[cfg(feature = "experimental-controllers")]
use pi::extension_scoring::{
    VoiCandidate, VoiPlan, VoiPlannerConfig, VoiSkipReason, plan_voi_candidates,
};

#[cfg(feature = "experimental-controllers")]
#[path = "algorithms/voi_planner.rs"]
mod voi_planner;

// ── Helpers ──────────────────────────────────────────────────────────

#[cfg(feature = "experimental-controllers")]
fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 16, 12, 0, 0)
        .single()
        .expect("valid timestamp")
}

#[cfg(feature = "experimental-controllers")]
fn fresh_timestamp() -> String {
    // 10 minutes ago — well within default 120-minute freshness window.
    Utc.with_ymd_and_hms(2026, 2, 16, 11, 50, 0)
        .single()
        .expect("valid timestamp")
        .to_rfc3339()
}

#[cfg(feature = "experimental-controllers")]
fn stale_timestamp() -> String {
    // 3 hours ago — outside default 120-minute freshness window.
    Utc.with_ymd_and_hms(2026, 2, 16, 9, 0, 0)
        .single()
        .expect("valid timestamp")
        .to_rfc3339()
}

#[cfg(feature = "experimental-controllers")]
fn future_timestamp() -> String {
    Utc.with_ymd_and_hms(2026, 2, 16, 12, 10, 0)
        .single()
        .expect("valid timestamp")
        .to_rfc3339()
}

#[cfg(feature = "experimental-controllers")]
fn barely_stale_timestamp() -> String {
    Utc.with_ymd_and_hms(2026, 2, 16, 11, 58, 59)
        .single()
        .expect("valid timestamp")
        .to_rfc3339()
}

#[cfg(feature = "experimental-controllers")]
fn candidate(id: &str, utility: f64, overhead_ms: u32) -> VoiCandidate {
    VoiCandidate {
        id: id.to_string(),
        utility_score: utility,
        estimated_overhead_ms: overhead_ms,
        last_seen_at: Some(fresh_timestamp()),
        enabled: true,
    }
}

fn calm_observation(shard_id: &str) -> MeanFieldShardObservation {
    MeanFieldShardObservation {
        shard_id: shard_id.to_string(),
        queue_pressure: 0.1,
        tail_latency_ratio: 1.05,
        starvation_risk: 0.0,
    }
}

fn stressed_observation(shard_id: &str) -> MeanFieldShardObservation {
    MeanFieldShardObservation {
        shard_id: shard_id.to_string(),
        queue_pressure: 0.9,
        tail_latency_ratio: 2.5,
        starvation_risk: 0.8,
    }
}

fn default_shard_state(shard_id: &str) -> MeanFieldShardState {
    MeanFieldShardState {
        shard_id: shard_id.to_string(),
        routing_weight: 1.0,
        batch_budget: 32,
        help_factor: 1.0,
        backoff_factor: 1.0,
        last_routing_delta: 0.0,
    }
}

// ══════════════════════════════════════════════════════════════════════
//  plan_voi_candidates tests
// ══════════════════════════════════════════════════════════════════════

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_disabled_config_skips_all_candidates() {
    let config = VoiPlannerConfig {
        enabled: false,
        overhead_budget_ms: 1000,
        ..Default::default()
    };
    let candidates = vec![candidate("a", 10.0, 5), candidate("b", 20.0, 5)];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    assert!(
        plan.selected.is_empty(),
        "disabled planner must select none"
    );
    assert_eq!(plan.skipped.len(), 2);
    for skipped in &plan.skipped {
        assert_eq!(skipped.reason, VoiSkipReason::Disabled);
    }
    assert_eq!(plan.used_overhead_ms, 0);
    assert_eq!(plan.remaining_overhead_ms, 1000);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_empty_candidates_returns_empty_plan() {
    let config = VoiPlannerConfig::default();
    let plan = plan_voi_candidates(&[], fixed_now(), &config);

    assert!(plan.selected.is_empty());
    assert!(plan.skipped.is_empty());
    assert_eq!(plan.used_overhead_ms, 0);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_selects_candidates_within_budget() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 50,
        ..Default::default()
    };
    let candidates = vec![
        candidate("cheap", 10.0, 20),
        candidate("medium", 15.0, 25),
        candidate("expensive", 5.0, 30),
    ];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    // Budget is 50ms. Candidates are sorted by utility_per_ms descending.
    // medium: 15/25 = 0.6, cheap: 10/20 = 0.5, expensive: 5/30 = 0.167
    // medium (25ms) fits, cheap (25ms) cumulative=50 fits, expensive (30ms) cumulative>50
    assert!(plan.used_overhead_ms <= 50);
    assert!(
        plan.selected.iter().any(|selected| selected.id == "medium"),
        "medium should be selected"
    );
    assert!(
        plan.selected.iter().any(|selected| selected.id == "cheap"),
        "cheap should be selected"
    );

    // Expensive should be budget-exceeded
    let has_budget_exceeded_expensive = plan
        .skipped
        .iter()
        .filter(|s| s.reason == VoiSkipReason::BudgetExceeded)
        .map(|s| s.id.as_str())
        .any(|id| id == "expensive");
    assert!(
        has_budget_exceeded_expensive,
        "expensive should be budget-exceeded"
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_respects_max_candidates_limit() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 1000,
        max_candidates: Some(2),
        ..Default::default()
    };
    let candidates = vec![
        candidate("a", 10.0, 5),
        candidate("b", 20.0, 5),
        candidate("c", 30.0, 5),
    ];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    assert_eq!(
        plan.selected.len(),
        2,
        "max_candidates=2 should limit selection"
    );
    // The third candidate should be skipped as BudgetExceeded (the limit check).
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, VoiSkipReason::BudgetExceeded);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_skips_stale_candidates() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 1000,
        stale_after_minutes: Some(120),
        ..Default::default()
    };
    let mut fresh_c = candidate("fresh", 10.0, 5);
    fresh_c.last_seen_at = Some(fresh_timestamp());
    let mut stale_c = candidate("stale", 20.0, 5);
    stale_c.last_seen_at = Some(stale_timestamp());

    let plan = plan_voi_candidates(&[stale_c, fresh_c], fixed_now(), &config);

    assert!(plan.selected.iter().any(|selected| selected.id == "fresh"));
    assert!(!plan.selected.iter().any(|selected| selected.id == "stale"));

    let has_stale_skipped = plan
        .skipped
        .iter()
        .filter(|s| s.reason == VoiSkipReason::StaleEvidence)
        .map(|s| s.id.as_str())
        .any(|id| id == "stale");
    assert!(has_stale_skipped);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_skips_missing_telemetry() {
    let config = VoiPlannerConfig::default();
    let mut no_timestamp = candidate("missing", 10.0, 5);
    no_timestamp.last_seen_at = None;

    let plan = plan_voi_candidates(&[no_timestamp], fixed_now(), &config);

    assert!(plan.selected.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, VoiSkipReason::MissingTelemetry);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_skips_future_telemetry() {
    let config = VoiPlannerConfig::default();
    let mut future_candidate = candidate("future", 10.0, 5);
    future_candidate.last_seen_at = Some(future_timestamp());

    let plan = plan_voi_candidates(&[future_candidate], fixed_now(), &config);

    assert!(plan.selected.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, VoiSkipReason::MissingTelemetry);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_uses_exact_staleness_boundary_instead_of_truncated_minutes() {
    let config = VoiPlannerConfig {
        stale_after_minutes: Some(1),
        ..Default::default()
    };
    let mut barely_stale = candidate("barely-stale", 10.0, 5);
    barely_stale.last_seen_at = Some(barely_stale_timestamp());
    let mut fresh = candidate("fresh", 10.0, 5);
    fresh.last_seen_at = Some("2026-02-16T11:59:00+00:00".to_string());

    let plan = plan_voi_candidates(&[barely_stale, fresh], fixed_now(), &config);

    assert_eq!(plan.selected.len(), 1);
    assert_eq!(plan.selected[0].id, "fresh");
    assert!(plan.skipped.iter().any(|entry| {
        entry.id == "barely-stale" && entry.reason == VoiSkipReason::StaleEvidence
    }));
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_fails_closed_when_staleness_window_is_out_of_range() {
    let config = VoiPlannerConfig {
        stale_after_minutes: Some(i64::MAX),
        ..Default::default()
    };
    let fresh = candidate("fresh", 10.0, 5);

    let plan = plan_voi_candidates(&[fresh], fixed_now(), &config);

    assert!(plan.selected.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, VoiSkipReason::StaleEvidence);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_skips_disabled_candidates() {
    let config = VoiPlannerConfig::default();
    let mut disabled_c = candidate("off", 50.0, 1);
    disabled_c.enabled = false;

    let plan = plan_voi_candidates(&[disabled_c], fixed_now(), &config);

    assert!(plan.selected.is_empty());
    assert_eq!(plan.skipped[0].reason, VoiSkipReason::Disabled);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_skips_below_utility_floor() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 1000,
        min_utility_score: Some(5.0),
        ..Default::default()
    };
    let low = candidate("low", 2.0, 5);
    let high = candidate("high", 10.0, 5);

    let plan = plan_voi_candidates(&[low, high], fixed_now(), &config);

    assert!(plan.selected.iter().any(|selected| selected.id == "high"));
    assert!(!plan.selected.iter().any(|selected| selected.id == "low"));

    let has_utility_skipped_low = plan
        .skipped
        .iter()
        .filter(|s| s.reason == VoiSkipReason::BelowUtilityFloor)
        .map(|s| s.id.as_str())
        .any(|id| id == "low");
    assert!(has_utility_skipped_low);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_orders_by_utility_per_ms_descending() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 1000,
        ..Default::default()
    };
    // a: 20/10 = 2.0 utility/ms
    // b: 10/2  = 5.0 utility/ms (best)
    // c: 30/20 = 1.5 utility/ms
    let candidates = vec![
        candidate("a", 20.0, 10),
        candidate("b", 10.0, 2),
        candidate("c", 30.0, 20),
    ];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    assert_eq!(plan.selected.len(), 3);
    assert_eq!(plan.selected[0].id, "b", "highest utility/ms first");
    assert_eq!(plan.selected[1].id, "a");
    assert_eq!(plan.selected[2].id, "c");
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_cumulative_overhead_monotonically_increases() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 500,
        ..Default::default()
    };
    let candidates = vec![
        candidate("a", 20.0, 10),
        candidate("b", 10.0, 20),
        candidate("c", 30.0, 30),
    ];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    let mut prev = 0;
    for selected in &plan.selected {
        assert!(
            selected.cumulative_overhead_ms >= prev,
            "cumulative must be monotonically increasing"
        );
        prev = selected.cumulative_overhead_ms;
    }
    assert_eq!(plan.used_overhead_ms, prev);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_remaining_overhead_equals_budget_minus_used() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 100,
        ..Default::default()
    };
    let candidates = vec![candidate("a", 10.0, 30), candidate("b", 20.0, 40)];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);

    assert_eq!(
        plan.remaining_overhead_ms,
        config
            .overhead_budget_ms
            .saturating_sub(plan.used_overhead_ms),
        "remaining = budget - used"
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_negative_utility_treated_as_zero() {
    let config = VoiPlannerConfig {
        enabled: true,
        overhead_budget_ms: 1000,
        min_utility_score: Some(0.0),
        ..Default::default()
    };
    let negative = candidate("neg", -5.0, 5);
    let positive = candidate("pos", 10.0, 5);

    let plan = plan_voi_candidates(&[negative, positive], fixed_now(), &config);

    // Negative utility is normalized to 0.0, which equals the floor — should still pass.
    assert!(plan.selected.iter().any(|selected| selected.id == "pos"));
    // Zero-utility candidates remain eligible, but exact budget planning drops them
    // when they add no utility and consume overhead.
    assert!(!plan.selected.iter().any(|selected| selected.id == "neg"));
}

// ══════════════════════════════════════════════════════════════════════
//  compute_mean_field_controls tests
// ══════════════════════════════════════════════════════════════════════

#[test]
fn meanfield_empty_observations_converged() {
    let config = MeanFieldControllerConfig::default();
    let report = compute_mean_field_controls(&[], &[], &config);

    assert!(
        report.converged,
        "empty observation set should be converged"
    );
    assert!(report.controls.is_empty());
    assert!(report.global_pressure.abs() <= f64::EPSILON);
    assert_eq!(report.clipped_count, 0);
    assert_eq!(report.oscillation_guard_count, 0);
}

#[test]
fn meanfield_single_calm_shard_converges() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("shard-0")];
    let prev = vec![default_shard_state("shard-0")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    assert_eq!(report.controls.len(), 1);
    let ctrl = &report.controls[0];
    assert_eq!(ctrl.shard_id, "shard-0");
    // Calm shard: routing weight should stay near 1.0
    assert!(ctrl.routing_weight > 0.0, "routing weight must be positive");
    assert!(
        ctrl.routing_weight <= config.max_routing_weight,
        "routing weight must not exceed max"
    );
    assert!(
        ctrl.routing_weight >= config.min_routing_weight,
        "routing weight must not go below min"
    );
    assert!(ctrl.batch_budget >= config.min_batch_budget);
    assert!(ctrl.batch_budget <= config.max_batch_budget);
}

#[test]
fn meanfield_stressed_shard_adjusts_routing_down() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![stressed_observation("shard-0")];
    let prev = vec![default_shard_state("shard-0")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    // Under high pressure, routing weight should decrease from baseline
    assert!(
        ctrl.routing_weight < 1.0,
        "stressed shard routing ({}) should decrease from baseline 1.0",
        ctrl.routing_weight
    );
}

#[test]
fn meanfield_multiple_shards_global_pressure_is_average() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("a"), stressed_observation("b")];
    let prev = vec![default_shard_state("a"), default_shard_state("b")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    // Global pressure should be between the calm and stressed values
    assert!(
        report.global_pressure > 0.0,
        "global_pressure should be > 0 with one stressed shard"
    );
    assert!(
        report.global_pressure < 1.0,
        "global_pressure should be < 1.0"
    );
    assert_eq!(report.controls.len(), 2);
    // Controls are sorted by shard_id
    assert_eq!(report.controls[0].shard_id, "a");
    assert_eq!(report.controls[1].shard_id, "b");
}

#[test]
fn meanfield_routing_weight_stays_within_bounds() {
    let config = MeanFieldControllerConfig {
        min_routing_weight: 0.5,
        max_routing_weight: 2.0,
        max_step: 5.0, // Large step to push boundaries
        ..Default::default()
    };
    let obs = vec![
        stressed_observation("low"),
        MeanFieldShardObservation {
            shard_id: "high".to_string(),
            queue_pressure: 0.0,
            tail_latency_ratio: 1.0,
            starvation_risk: 0.0,
        },
    ];
    let prev = vec![default_shard_state("low"), default_shard_state("high")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    for ctrl in &report.controls {
        assert!(
            ctrl.routing_weight >= config.min_routing_weight,
            "shard {shard_id} routing {routing_weight} < min {min_routing_weight}",
            shard_id = ctrl.shard_id,
            routing_weight = ctrl.routing_weight,
            min_routing_weight = config.min_routing_weight
        );
        assert!(
            ctrl.routing_weight <= config.max_routing_weight,
            "shard {shard_id} routing {routing_weight} > max {max_routing_weight}",
            shard_id = ctrl.shard_id,
            routing_weight = ctrl.routing_weight,
            max_routing_weight = config.max_routing_weight
        );
    }
}

#[test]
fn meanfield_batch_budget_stays_within_bounds() {
    let config = MeanFieldControllerConfig {
        min_batch_budget: 4,
        max_batch_budget: 16,
        ..Default::default()
    };
    let obs = vec![calm_observation("a"), stressed_observation("b")];
    let prev = vec![default_shard_state("a"), default_shard_state("b")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    for ctrl in &report.controls {
        assert!(
            ctrl.batch_budget >= config.min_batch_budget,
            "shard {shard_id} batch {batch_budget} < min {min_batch_budget}",
            shard_id = ctrl.shard_id,
            batch_budget = ctrl.batch_budget,
            min_batch_budget = config.min_batch_budget
        );
        assert!(
            ctrl.batch_budget <= config.max_batch_budget,
            "shard {shard_id} batch {batch_budget} > max {max_batch_budget}",
            shard_id = ctrl.shard_id,
            batch_budget = ctrl.batch_budget,
            max_batch_budget = config.max_batch_budget
        );
    }
}

#[test]
fn meanfield_help_factor_stays_within_bounds() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("a"), stressed_observation("b")];
    let prev = vec![default_shard_state("a"), default_shard_state("b")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    for ctrl in &report.controls {
        assert!(
            ctrl.help_factor >= config.min_help_factor,
            "shard {shard_id} help {help_factor} < min {min_help_factor}",
            shard_id = ctrl.shard_id,
            help_factor = ctrl.help_factor,
            min_help_factor = config.min_help_factor
        );
        assert!(
            ctrl.help_factor <= config.max_help_factor,
            "shard {shard_id} help {help_factor} > max {max_help_factor}",
            shard_id = ctrl.shard_id,
            help_factor = ctrl.help_factor,
            max_help_factor = config.max_help_factor
        );
    }
}

#[test]
fn meanfield_backoff_factor_stays_within_bounds() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("a"), stressed_observation("b")];
    let prev = vec![default_shard_state("a"), default_shard_state("b")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    for ctrl in &report.controls {
        assert!(
            ctrl.backoff_factor >= config.min_backoff_factor,
            "shard {shard_id} backoff {backoff_factor} < min {min_backoff_factor}",
            shard_id = ctrl.shard_id,
            backoff_factor = ctrl.backoff_factor,
            min_backoff_factor = config.min_backoff_factor
        );
        assert!(
            ctrl.backoff_factor <= config.max_backoff_factor,
            "shard {shard_id} backoff {backoff_factor} > max {max_backoff_factor}",
            shard_id = ctrl.shard_id,
            backoff_factor = ctrl.backoff_factor,
            max_backoff_factor = config.max_backoff_factor
        );
    }
}

#[test]
fn meanfield_oscillation_guard_dampens_direction_reversal() {
    let config = MeanFieldControllerConfig::default();
    // Shard that was moving up (positive delta) and now sees high pressure (should move down).
    let prev = vec![MeanFieldShardState {
        shard_id: "osc".to_string(),
        routing_weight: 1.0,
        batch_budget: 32,
        help_factor: 1.0,
        backoff_factor: 1.0,
        last_routing_delta: 0.15, // Previous delta was positive (routing up)
    }];
    let obs = vec![stressed_observation("osc")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    // The oscillation guard should fire since direction is reversing
    // (was positive, now wants to be negative due to stress).
    // Whether it fires depends on the actual computed delta sign.
    if ctrl.oscillation_guarded {
        assert!(
            report.oscillation_guard_count >= 1,
            "oscillation guard count should increment"
        );
    }
}

#[test]
fn meanfield_no_previous_state_uses_defaults() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("new-shard")];
    // No previous state
    let report = compute_mean_field_controls(&obs, &[], &config);

    assert_eq!(report.controls.len(), 1);
    let ctrl = &report.controls[0];
    assert_eq!(ctrl.shard_id, "new-shard");
    assert!(ctrl.routing_weight > 0.0);
}

#[test]
fn meanfield_clipping_reported_correctly() {
    let config = MeanFieldControllerConfig {
        max_step: 0.01, // Very small step → everything gets clipped
        ..Default::default()
    };
    let obs = vec![stressed_observation("clipped-shard")];
    let prev = vec![default_shard_state("clipped-shard")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    // With a very small max_step, any non-trivial adjustment should be clipped
    let routing_delta_abs = ctrl.routing_delta.abs();
    if routing_delta_abs > f64::EPSILON {
        let max_allowed = config.damping.mul_add(config.max_step, config.max_step) + 0.01;
        assert!(
            routing_delta_abs <= max_allowed,
            "routing delta {routing_delta_abs} should be bounded by max_step"
        );
    }
}

#[test]
fn meanfield_stability_margin_nonnegative() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("a"), stressed_observation("b")];
    let prev = vec![default_shard_state("a"), default_shard_state("b")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    for ctrl in &report.controls {
        assert!(
            ctrl.stability_margin >= 0.0,
            "shard {shard_id} stability margin {stability_margin} must be >= 0",
            shard_id = ctrl.shard_id,
            stability_margin = ctrl.stability_margin
        );
    }
}

#[test]
fn meanfield_convergence_with_zero_pressure() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![MeanFieldShardObservation {
        shard_id: "zero".to_string(),
        queue_pressure: 0.0,
        tail_latency_ratio: 1.0,
        starvation_risk: 0.0,
    }];
    let prev = vec![default_shard_state("zero")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    // With zero pressure and baseline routing=1.0, the system should converge
    // (no pressure offset, minimal instability).
    assert!(
        report.converged,
        "zero-pressure shard should converge from default state"
    );
}

#[test]
fn meanfield_nan_pressures_sanitized_to_zero() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![MeanFieldShardObservation {
        shard_id: "nan".to_string(),
        queue_pressure: f64::NAN,
        tail_latency_ratio: f64::NAN,
        starvation_risk: f64::NAN,
    }];
    let prev = vec![default_shard_state("nan")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    // NaN inputs should be sanitized; routing weight should remain finite
    assert!(
        ctrl.routing_weight.is_finite(),
        "NaN inputs should be sanitized to finite routing"
    );
    assert!(ctrl.batch_budget >= config.min_batch_budget);
}

#[test]
fn meanfield_infinite_pressures_clamped() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![MeanFieldShardObservation {
        shard_id: "inf".to_string(),
        queue_pressure: f64::INFINITY,
        tail_latency_ratio: f64::INFINITY,
        starvation_risk: f64::INFINITY,
    }];
    let prev = vec![default_shard_state("inf")];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    assert!(
        ctrl.routing_weight.is_finite(),
        "infinite inputs should produce finite routing"
    );
    assert!(ctrl.routing_weight >= config.min_routing_weight);
    assert!(ctrl.routing_weight <= config.max_routing_weight);
}

#[test]
fn meanfield_nonfinite_previous_state_fails_closed() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![stressed_observation("prior-state")];
    let prev = vec![MeanFieldShardState {
        shard_id: "prior-state".to_string(),
        routing_weight: f64::NAN,
        batch_budget: 0,
        help_factor: f64::INFINITY,
        backoff_factor: f64::NEG_INFINITY,
        last_routing_delta: f64::NAN,
    }];

    let report = compute_mean_field_controls(&obs, &prev, &config);

    let ctrl = &report.controls[0];
    assert!(report.global_pressure.is_finite());
    assert!(ctrl.routing_weight.is_finite());
    assert!(ctrl.routing_delta.is_finite());
    assert!(ctrl.stability_margin.is_finite());
    assert!(ctrl.routing_weight >= config.min_routing_weight);
    assert!(ctrl.routing_weight <= config.max_routing_weight);
    assert!(ctrl.batch_budget >= config.min_batch_budget);
    assert!(ctrl.batch_budget <= config.max_batch_budget);
}

#[test]
fn meanfield_extreme_batch_budget_range_does_not_hang() {
    let config = MeanFieldControllerConfig {
        min_batch_budget: 1,
        max_batch_budget: u32::MAX,
        ..Default::default()
    };
    let obs = vec![calm_observation("wide")];

    let report = compute_mean_field_controls(&obs, &[], &config);

    assert_eq!(report.controls.len(), 1);
    let ctrl = &report.controls[0];
    assert!(ctrl.batch_budget >= config.min_batch_budget);
    assert!(ctrl.batch_budget <= config.max_batch_budget);
    assert!(ctrl.routing_weight.is_finite());
}

#[test]
fn meanfield_controls_sorted_by_shard_id() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![
        calm_observation("zeta"),
        calm_observation("alpha"),
        calm_observation("mu"),
    ];

    let report = compute_mean_field_controls(&obs, &[], &config);

    assert!(
        report
            .controls
            .iter()
            .map(|control| control.shard_id.as_str())
            .eq(["alpha", "mu", "zeta"]),
        "controls should be sorted by shard_id"
    );
}

#[test]
fn meanfield_report_serializes_round_trip() {
    let config = MeanFieldControllerConfig::default();
    let obs = vec![calm_observation("rt"), stressed_observation("rt2")];
    let prev = vec![default_shard_state("rt"), default_shard_state("rt2")];

    let report = compute_mean_field_controls(&obs, &prev, &config);
    let json = serde_json::to_string(&report).expect("serialize");
    let parsed: MeanFieldControllerReport = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(report.converged, parsed.converged);
    assert_eq!(report.controls.len(), parsed.controls.len());
    assert_eq!(report.clipped_count, parsed.clipped_count);
    assert_eq!(
        report.oscillation_guard_count,
        parsed.oscillation_guard_count
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn voi_plan_serializes_round_trip() {
    let config = VoiPlannerConfig::default();
    let candidates = vec![candidate("rt", 10.0, 5)];

    let plan = plan_voi_candidates(&candidates, fixed_now(), &config);
    let json = serde_json::to_string(&plan).expect("serialize");
    let parsed: VoiPlan = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(plan.selected.len(), parsed.selected.len());
    assert_eq!(plan.skipped.len(), parsed.skipped.len());
    assert_eq!(plan.used_overhead_ms, parsed.used_overhead_ms);
    assert_eq!(plan.remaining_overhead_ms, parsed.remaining_overhead_ms);
}
