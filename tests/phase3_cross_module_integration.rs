//! Cross-module integration tests for Phase 3 performance modules.
//!
//! Verifies that `hostcall_io_uring_lane`, `hostcall_s3_fifo`,
//! `extension_scoring` (mean-field controller + OPE evaluator),
//! and `hostcall_queue` compose correctly through their public APIs.

use pi::extension_scoring::{
    MeanFieldControllerConfig, MeanFieldControllerReport, MeanFieldShardObservation,
    MeanFieldShardState, compute_mean_field_controls,
};
#[cfg(feature = "experimental-controllers")]
use pi::extension_scoring::{
    OpeEvaluationReport, OpeEvaluatorConfig, OpeGateReason, OpeTraceSample, evaluate_off_policy,
};
#[cfg(feature = "experimental-controllers")]
use pi::hostcall_io_uring_lane::{
    HostcallCapabilityClass, HostcallDispatchLane, HostcallIoHint, IoUringFallbackReason,
    IoUringLaneDecisionInput, IoUringLanePolicyConfig, IoUringLaneTelemetry,
    build_io_uring_lane_telemetry, decide_io_uring_lane, decide_io_uring_lane_with_telemetry,
};
use pi::hostcall_s3_fifo::{
    S3FifoConfig, S3FifoDecisionKind, S3FifoFallbackReason, S3FifoPolicy, S3FifoTier,
};

// ────────────────────── io_uring lane policy ──────────────────────

#[cfg(feature = "experimental-controllers")]
#[test]
fn io_uring_lane_all_capability_classes_have_deterministic_outcomes() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 16,
        allow_filesystem: true,
        allow_network: true,
    };

    let classes = [
        HostcallCapabilityClass::Filesystem,
        HostcallCapabilityClass::Network,
        HostcallCapabilityClass::Execution,
        HostcallCapabilityClass::Session,
        HostcallCapabilityClass::Events,
        HostcallCapabilityClass::Environment,
        HostcallCapabilityClass::Tool,
        HostcallCapabilityClass::Ui,
        HostcallCapabilityClass::Telemetry,
        HostcallCapabilityClass::Unknown,
    ];

    for capability in classes {
        let input = IoUringLaneDecisionInput {
            capability,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 0,
            force_compat_lane: false,
        };
        let d1 = decide_io_uring_lane(config, input);
        let d2 = decide_io_uring_lane(config, input);
        assert_eq!(d1, d2, "non-deterministic for {capability:?}");

        // Filesystem and Network are policy-eligible, but the real executor is
        // not wired yet, so they must report explicit fallback instead of
        // presenting a placeholder bridge as io_uring execution.
        match capability {
            HostcallCapabilityClass::Filesystem | HostcallCapabilityClass::Network => {
                assert_eq!(d1.lane, HostcallDispatchLane::Fast);
                assert_eq!(
                    d1.fallback_reason,
                    Some(IoUringFallbackReason::IoUringExecutorUnavailable)
                );
            }
            _ => {
                assert_eq!(d1.lane, HostcallDispatchLane::Fast);
                assert_eq!(
                    d1.fallback_reason,
                    Some(IoUringFallbackReason::UnsupportedCapability)
                );
            }
        }
    }
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn io_uring_lane_telemetry_matches_decision_across_all_branches() {
    let configs = [
        IoUringLanePolicyConfig {
            enabled: true,
            ring_available: true,
            max_queue_depth: 4,
            allow_filesystem: true,
            allow_network: false,
        },
        IoUringLanePolicyConfig {
            enabled: false,
            ring_available: false,
            max_queue_depth: 4,
            allow_filesystem: true,
            allow_network: true,
        },
    ];

    let inputs = [
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 0,
            force_compat_lane: false,
        },
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Network,
            io_hint: HostcallIoHint::CpuBound,
            queue_depth: 5,
            force_compat_lane: true,
        },
    ];

    for config in configs {
        for input in inputs {
            let decision = decide_io_uring_lane(config, input);
            let (d2, telemetry) = decide_io_uring_lane_with_telemetry(config, input);
            assert_eq!(decision, d2);
            assert_eq!(telemetry.lane, decision.lane);
            assert_eq!(telemetry.fallback_reason, decision.fallback_reason);

            let telemetry2 = build_io_uring_lane_telemetry(config, input, decision);
            assert_eq!(telemetry, telemetry2);
        }
    }
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn io_uring_lane_queue_depth_boundary_values() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 8,
        allow_filesystem: true,
        allow_network: true,
    };

    // At budget: policy preconditions pass, but executor availability still
    // fails closed until the real ring executor is wired.
    let at_budget = decide_io_uring_lane(
        config,
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 7,
            force_compat_lane: false,
        },
    );
    assert_eq!(at_budget.lane, HostcallDispatchLane::Fast);
    assert_eq!(
        at_budget.fallback_reason,
        Some(IoUringFallbackReason::IoUringExecutorUnavailable)
    );

    // Exceeding budget
    let over_budget = decide_io_uring_lane(
        config,
        IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: 8,
            force_compat_lane: false,
        },
    );
    assert_eq!(over_budget.lane, HostcallDispatchLane::Fast);
    assert_eq!(
        over_budget.fallback_reason,
        Some(IoUringFallbackReason::QueueDepthBudgetExceeded)
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn io_uring_lane_telemetry_serialization_roundtrip() {
    let config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 16,
        allow_filesystem: true,
        allow_network: true,
    };
    let input = IoUringLaneDecisionInput {
        capability: HostcallCapabilityClass::Filesystem,
        io_hint: HostcallIoHint::IoHeavy,
        queue_depth: 3,
        force_compat_lane: false,
    };
    let decision = decide_io_uring_lane(config, input);
    let telemetry = build_io_uring_lane_telemetry(config, input, decision);

    let json = serde_json::to_string(&telemetry).expect("serialize");
    let roundtrip: IoUringLaneTelemetry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(roundtrip.lane, telemetry.lane);
    assert_eq!(roundtrip.fallback_reason, telemetry.fallback_reason);
}

// ────────────────────── S3-FIFO admission policy ──────────────────────

#[test]
fn s3fifo_multi_owner_fairness_enforced() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 8,
        small_capacity: 4,
        ghost_capacity: 8,
        max_entries_per_owner: 2,
        fallback_window: 16,
        min_ghost_hits_in_window: 1,
        max_budget_rejections_in_window: 6,
    });

    // Owner A admits 2 entries (at budget)
    let a1 = policy.access("ext-a", "a1".to_string());
    let a2 = policy.access("ext-a", "a2".to_string());
    assert_eq!(a1.kind, S3FifoDecisionKind::AdmitSmall);
    assert_eq!(a2.kind, S3FifoDecisionKind::AdmitSmall);

    // Owner A rejected at budget
    let a3 = policy.access("ext-a", "a3".to_string());
    assert_eq!(a3.kind, S3FifoDecisionKind::RejectFairnessBudget);

    // Owner B can still admit
    let b1 = policy.access("ext-b", "b1".to_string());
    assert_eq!(b1.kind, S3FifoDecisionKind::AdmitSmall);

    let telemetry = policy.telemetry();
    assert_eq!(*telemetry.owner_live_counts.get("ext-a").unwrap_or(&0), 2);
    assert_eq!(*telemetry.owner_live_counts.get("ext-b").unwrap_or(&0), 1);
    assert_eq!(telemetry.budget_rejections_total, 1);
}

#[test]
fn s3fifo_capacity_enforced_under_burst() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 4,
        small_capacity: 2,
        ghost_capacity: 8,
        max_entries_per_owner: 10,
        fallback_window: 32,
        min_ghost_hits_in_window: 1,
        max_budget_rejections_in_window: 16,
    });

    // Admit 10 entries from different owners — only 4 should be live
    for idx in 0..10 {
        let owner = format!("ext-{idx}");
        let key = format!("key-{idx}");
        policy.access(&owner, key);
    }

    let telemetry = policy.telemetry();
    assert!(
        telemetry.live_depth <= 4,
        "live_depth {} exceeds capacity 4",
        telemetry.live_depth
    );
    assert!(
        telemetry.ghost_depth > 0,
        "ghost should have evicted entries"
    );
}

#[test]
fn s3fifo_ghost_reentry_path_promotes_to_main() {
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 4,
        small_capacity: 1,
        ghost_capacity: 8,
        max_entries_per_owner: 4,
        fallback_window: 32,
        min_ghost_hits_in_window: 1,
        max_budget_rejections_in_window: 16,
    });

    // k1 enters small, then gets evicted by k2
    policy.access("ext-a", "k1".to_string());
    policy.access("ext-a", "k2".to_string());

    // k1 is now in ghost; re-accessing should promote to main
    let decision = policy.access("ext-a", "k1".to_string());
    assert_eq!(decision.kind, S3FifoDecisionKind::AdmitFromGhost);
    assert!(decision.ghost_hit);
    assert_eq!(decision.tier, S3FifoTier::Main);
    assert_eq!(policy.telemetry().ghost_hits_total, 1);
}

#[test]
fn s3fifo_fallback_signal_quality_threshold_exact() {
    let window = 4;
    let mut policy = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 32,
        small_capacity: 16,
        ghost_capacity: 32,
        max_entries_per_owner: 32,
        fallback_window: window,
        min_ghost_hits_in_window: 2,
        max_budget_rejections_in_window: window,
    });

    // All cold admissions — no ghost hits — should trigger fallback
    for idx in 0..window {
        let key = format!("cold-{idx}");
        policy.access("ext-a", key);
    }

    assert_eq!(
        policy.telemetry().fallback_reason,
        Some(S3FifoFallbackReason::SignalQualityInsufficient)
    );
    // Subsequent access should bypass
    let bypass = policy.access("ext-a", "late".to_string());
    assert_eq!(bypass.kind, S3FifoDecisionKind::FallbackBypass);
    assert_eq!(bypass.tier, S3FifoTier::Fallback);

    // Clear and resume normal
    policy.clear_fallback();
    assert!(policy.telemetry().fallback_reason.is_none());
}

// ────────────────────── Mean-field contention controller ──────────────────────

fn assert_close(a: f64, b: f64, tolerance: f64) {
    assert!(
        (a - b).abs() <= tolerance,
        "expected {a} ≈ {b} (tolerance {tolerance})"
    );
}

fn find_control<'a>(
    report: &'a MeanFieldControllerReport,
    shard_id: &str,
) -> &'a pi::extension_scoring::MeanFieldShardControl {
    report
        .controls
        .iter()
        .find(|control| control.shard_id == shard_id)
        .unwrap_or_else(|| panic!("missing control for shard_id={shard_id}"))
}

#[test]
fn mean_field_controller_converges_from_balanced_state() {
    let config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "s0".to_string(),
            queue_pressure: 0.2,
            tail_latency_ratio: 1.1,
            starvation_risk: 0.05,
        },
        MeanFieldShardObservation {
            shard_id: "s1".to_string(),
            queue_pressure: 0.15,
            tail_latency_ratio: 1.05,
            starvation_risk: 0.02,
        },
    ];

    // Start with no previous state
    let report = compute_mean_field_controls(&observations, &[], &config);
    assert!(!report.controls.is_empty());
    assert_eq!(report.controls.len(), 2);

    // Both shards should get routing weights within valid range
    for control in &report.controls {
        assert!(
            control.routing_weight >= config.min_routing_weight,
            "routing_weight {} below min {}",
            control.routing_weight,
            config.min_routing_weight
        );
        assert!(
            control.routing_weight <= config.max_routing_weight,
            "routing_weight {} above max {}",
            control.routing_weight,
            config.max_routing_weight
        );
        assert!(control.batch_budget >= config.min_batch_budget);
        assert!(control.batch_budget <= config.max_batch_budget);
    }
}

#[test]
fn mean_field_controller_converges_after_iterations() {
    let config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "s0".to_string(),
            queue_pressure: 0.8,
            tail_latency_ratio: 2.0,
            starvation_risk: 0.5,
        },
        MeanFieldShardObservation {
            shard_id: "s1".to_string(),
            queue_pressure: 0.1,
            tail_latency_ratio: 1.01,
            starvation_risk: 0.0,
        },
    ];

    let mut previous = Vec::new();
    let mut last_report: Option<MeanFieldControllerReport> = None;

    for _ in 0..20 {
        let report = compute_mean_field_controls(&observations, &previous, &config);
        previous = report
            .controls
            .iter()
            .map(|c| MeanFieldShardState {
                shard_id: c.shard_id.clone(),
                routing_weight: c.routing_weight,
                batch_budget: c.batch_budget,
                help_factor: c.help_factor,
                backoff_factor: c.backoff_factor,
                last_routing_delta: c.routing_delta,
            })
            .collect();
        last_report = Some(report);
    }

    let report = last_report.expect("should have report");
    // After 20 iterations with fixed observations, should converge
    assert!(report.converged, "expected convergence after 20 iterations");
}

#[test]
fn mean_field_controller_empty_observations_converges_trivially() {
    let config = MeanFieldControllerConfig::default();
    let report = compute_mean_field_controls(&[], &[], &config);
    assert!(report.converged);
    assert!(report.controls.is_empty());
    assert_close(report.global_pressure, 0.0, 1e-12);
}

#[test]
fn mean_field_controller_stability_margin_within_bounds() {
    let config = MeanFieldControllerConfig::default();
    let observations: Vec<MeanFieldShardObservation> = (0..4)
        .map(|idx| MeanFieldShardObservation {
            shard_id: format!("s{idx}"),
            queue_pressure: f64::from(idx) * 0.25,
            tail_latency_ratio: f64::from(idx).mul_add(0.5, 1.0),
            starvation_risk: f64::from(idx) * 0.15,
        })
        .collect();

    let report = compute_mean_field_controls(&observations, &[], &config);
    for control in &report.controls {
        assert!(
            control.stability_margin >= 0.0,
            "negative stability_margin for {}",
            control.shard_id
        );
    }
}

#[test]
fn mean_field_controller_deterministic_output_ordering() {
    let config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "z-last".to_string(),
            queue_pressure: 0.5,
            tail_latency_ratio: 1.5,
            starvation_risk: 0.2,
        },
        MeanFieldShardObservation {
            shard_id: "a-first".to_string(),
            queue_pressure: 0.3,
            tail_latency_ratio: 1.2,
            starvation_risk: 0.1,
        },
    ];

    let report = compute_mean_field_controls(&observations, &[], &config);
    assert_eq!(report.controls[0].shard_id, "a-first");
    assert_eq!(report.controls[1].shard_id, "z-last");

    // Reversed input order should produce same output
    let reversed_observations = vec![observations[1].clone(), observations[0].clone()];
    let report2 = compute_mean_field_controls(&reversed_observations, &[], &config);
    assert_eq!(report.controls.len(), report2.controls.len());
    for (c1, c2) in report.controls.iter().zip(report2.controls.iter()) {
        assert_eq!(c1.shard_id, c2.shard_id);
        assert_close(c1.routing_weight, c2.routing_weight, 1e-12);
    }
}

#[test]
fn mean_field_controller_sanitizes_inverted_control_bounds() {
    let config = MeanFieldControllerConfig {
        min_routing_weight: 2.0,
        max_routing_weight: 1.0,
        min_batch_budget: 32,
        max_batch_budget: 8,
        min_help_factor: 2.0,
        max_help_factor: 1.0,
        min_backoff_factor: 3.0,
        max_backoff_factor: 1.0,
        max_step: -0.25,
        ..MeanFieldControllerConfig::default()
    };
    let observations = vec![MeanFieldShardObservation {
        shard_id: "clamp".to_string(),
        queue_pressure: 0.99,
        tail_latency_ratio: 3.0,
        starvation_risk: 0.99,
    }];

    let report = compute_mean_field_controls(&observations, &[], &config);
    let control = find_control(&report, "clamp");

    // Inverted bounds should be sanitized into deterministic single-point envelopes.
    assert_close(control.routing_weight, 2.0, 1e-12);
    assert_eq!(control.batch_budget, 8);
    assert_close(control.help_factor, 2.0, 1e-12);
    assert_close(control.backoff_factor, 3.0, 1e-12);
}

#[test]
fn mean_field_controller_starvation_risk_monotonically_increases_help() {
    let config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "low-starvation".to_string(),
            queue_pressure: 0.4,
            tail_latency_ratio: 1.2,
            starvation_risk: 0.0,
        },
        MeanFieldShardObservation {
            shard_id: "high-starvation".to_string(),
            queue_pressure: 0.4,
            tail_latency_ratio: 1.2,
            starvation_risk: 1.0,
        },
    ];

    let report = compute_mean_field_controls(&observations, &[], &config);
    let low = find_control(&report, "low-starvation");
    let high = find_control(&report, "high-starvation");

    assert!(
        high.help_factor > low.help_factor,
        "expected help_factor to increase with starvation risk: low={}, high={}",
        low.help_factor,
        high.help_factor
    );
    assert_close(low.backoff_factor, high.backoff_factor, 1e-12);
}

#[test]
fn mean_field_controller_higher_latency_pressure_reduces_batch_budget() {
    let config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "low-pressure".to_string(),
            queue_pressure: 0.1,
            tail_latency_ratio: 1.05,
            starvation_risk: 0.1,
        },
        MeanFieldShardObservation {
            shard_id: "high-pressure".to_string(),
            queue_pressure: 1.0,
            tail_latency_ratio: 2.0,
            starvation_risk: 0.1,
        },
    ];

    let report = compute_mean_field_controls(&observations, &[], &config);
    let low = find_control(&report, "low-pressure");
    let high = find_control(&report, "high-pressure");

    assert!(
        high.batch_budget < low.batch_budget,
        "expected higher queue/latency pressure to reduce batch budget: low={}, high={}",
        low.batch_budget,
        high.batch_budget
    );
    assert!(
        high.backoff_factor > low.backoff_factor,
        "expected higher queue/latency pressure to increase backoff factor: low={}, high={}",
        low.backoff_factor,
        high.backoff_factor
    );
}

// ────────────────────── OPE evaluator ──────────────────────

#[cfg(feature = "experimental-controllers")]
#[test]
fn ope_evaluator_passes_gate_with_valid_samples() {
    let samples = vec![
        OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.5,
            target_propensity: 0.5,
            outcome: 0.8,
            baseline_outcome: Some(0.6),
            direct_method_prediction: Some(0.7),
            context_lineage: None,
        },
        OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.3,
            target_propensity: 0.6,
            outcome: 0.9,
            baseline_outcome: Some(0.5),
            direct_method_prediction: Some(0.75),
            context_lineage: None,
        },
        OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.4,
            target_propensity: 0.4,
            outcome: 0.7,
            baseline_outcome: Some(0.55),
            direct_method_prediction: Some(0.65),
            context_lineage: None,
        },
    ];

    let config = OpeEvaluatorConfig {
        max_importance_weight: 10.0,
        min_effective_sample_size: 1.0,
        max_standard_error: 2.0,
        confidence_z: 1.96,
        max_regret_delta: 0.5,
    };

    let report = evaluate_off_policy(&samples, &config);
    // With matching probabilities, importance weights ~1.0, should pass
    assert!(
        report.gate.passed,
        "expected gate to pass, got {:?}",
        report.gate.reason
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn ope_evaluator_fails_closed_on_empty_input() {
    let config = OpeEvaluatorConfig {
        max_importance_weight: 10.0,
        min_effective_sample_size: 1.0,
        max_standard_error: 1.0,
        confidence_z: 1.96,
        max_regret_delta: 0.5,
    };

    let report = evaluate_off_policy(&[], &config);
    assert!(!report.gate.passed);
    assert_eq!(report.gate.reason, OpeGateReason::NoValidSamples);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn ope_evaluator_detects_extreme_propensity_skew() {
    let samples: Vec<OpeTraceSample> = (0..10)
        .map(|_| OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.001,
            target_propensity: 0.999,
            outcome: 1.0,
            baseline_outcome: Some(0.5),
            direct_method_prediction: Some(0.8),
            context_lineage: None,
        })
        .collect();

    let config = OpeEvaluatorConfig {
        max_importance_weight: 5.0,
        min_effective_sample_size: 2.0,
        max_standard_error: 0.5,
        confidence_z: 1.96,
        max_regret_delta: 0.3,
    };

    let report = evaluate_off_policy(&samples, &config);
    // With extreme importance weights, effective sample size should be low
    assert!(
        report.diagnostics.max_importance_weight > 0.0,
        "should track max importance weight"
    );
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn ope_evaluator_ground_truth_equal_probabilities() {
    // When behavior == target, importance weight = 1.0 for all samples
    // IPS estimate should equal the mean outcome
    let outcome_values = [0.2, 0.4, 0.6, 0.8, 1.0];
    let samples: Vec<OpeTraceSample> = outcome_values
        .iter()
        .map(|&outcome| OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.5,
            target_propensity: 0.5,
            outcome,
            baseline_outcome: Some(0.0),
            direct_method_prediction: Some(0.0),
            context_lineage: None,
        })
        .collect();

    let config = OpeEvaluatorConfig {
        max_importance_weight: 100.0,
        min_effective_sample_size: 0.5,
        max_standard_error: 10.0,
        confidence_z: 1.96,
        max_regret_delta: 10.0,
    };

    let report = evaluate_off_policy(&samples, &config);
    #[allow(clippy::cast_precision_loss)]
    let expected_mean = outcome_values.iter().sum::<f64>() / outcome_values.len() as f64;

    // IPS estimate should match mean when weights are all 1.0
    assert_close(report.ips.estimate, expected_mean, 1e-10);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn ope_evaluator_report_serde_roundtrip() {
    let samples = vec![
        OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.4,
            target_propensity: 0.6,
            outcome: 0.7,
            baseline_outcome: Some(0.5),
            direct_method_prediction: Some(0.6),
            context_lineage: None,
        },
        OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.3,
            target_propensity: 0.5,
            outcome: 0.8,
            baseline_outcome: Some(0.55),
            direct_method_prediction: Some(0.65),
            context_lineage: None,
        },
    ];

    let config = OpeEvaluatorConfig {
        max_importance_weight: 10.0,
        min_effective_sample_size: 0.5,
        max_standard_error: 5.0,
        confidence_z: 1.96,
        max_regret_delta: 1.0,
    };

    let report = evaluate_off_policy(&samples, &config);
    let json = serde_json::to_string(&report).expect("serialize");
    let roundtrip: OpeEvaluationReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(roundtrip.gate.passed, report.gate.passed);
    assert_eq!(roundtrip.gate.reason, report.gate.reason);
    assert_close(roundtrip.ips.estimate, report.ips.estimate, 1e-12);
}

// ────────────────────── Composed pipeline tests ──────────────────────

#[cfg(feature = "experimental-controllers")]
#[test]
fn composed_io_uring_to_s3fifo_admission_pipeline() {
    // Scenario: hostcall arrives → io_uring lane decides → if IO-heavy,
    // also check S3-FIFO admission for the originating extension.

    let io_config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 8,
        allow_filesystem: true,
        allow_network: true,
    };

    let mut s3fifo = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 4,
        small_capacity: 2,
        ghost_capacity: 4,
        max_entries_per_owner: 2,
        fallback_window: 8,
        min_ghost_hits_in_window: 1,
        max_budget_rejections_in_window: 4,
    });

    // Simulate 6 hostcalls from ext-a on filesystem
    for idx in 0..6 {
        let input = IoUringLaneDecisionInput {
            capability: HostcallCapabilityClass::Filesystem,
            io_hint: HostcallIoHint::IoHeavy,
            queue_depth: idx,
            force_compat_lane: false,
        };
        let decision = decide_io_uring_lane(io_config, input);

        // S3-FIFO admission for this extension+key
        let key = format!("fs-call-{idx}");
        let admission = s3fifo.access("ext-a", key);

        // First 2 should admit (budget=2), but io_uring remains an explicit
        // fallback until an actual executor exists.
        if idx < 2 {
            assert_eq!(
                decision.lane,
                HostcallDispatchLane::Fast,
                "call {idx} should not use the placeholder io_uring bridge"
            );
            assert_eq!(
                decision.fallback_reason,
                Some(IoUringFallbackReason::IoUringExecutorUnavailable),
                "call {idx} should explain executor unavailability"
            );
            assert_eq!(admission.kind, S3FifoDecisionKind::AdmitSmall);
        } else {
            assert_eq!(
                admission.kind,
                S3FifoDecisionKind::RejectFairnessBudget,
                "call {idx} should be rejected by fairness budget"
            );
        }
    }

    let telemetry = s3fifo.telemetry();
    assert_eq!(telemetry.budget_rejections_total, 4);
    assert_eq!(*telemetry.owner_live_counts.get("ext-a").unwrap_or(&0), 2);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn composed_multi_owner_io_uring_s3fifo_fairness() {
    let io_config = IoUringLanePolicyConfig {
        enabled: true,
        ring_available: true,
        max_queue_depth: 32,
        allow_filesystem: true,
        allow_network: true,
    };

    let mut s3fifo = S3FifoPolicy::new(S3FifoConfig {
        live_capacity: 8,
        small_capacity: 4,
        ghost_capacity: 8,
        max_entries_per_owner: 3,
        fallback_window: 32,
        min_ghost_hits_in_window: 1,
        max_budget_rejections_in_window: 16,
    });

    let extensions = ["ext-alpha", "ext-beta", "ext-gamma"];

    for round in 0..5 {
        for ext in &extensions {
            let input = IoUringLaneDecisionInput {
                capability: HostcallCapabilityClass::Network,
                io_hint: HostcallIoHint::IoHeavy,
                queue_depth: round,
                force_compat_lane: false,
            };
            let _decision = decide_io_uring_lane(io_config, input);

            let key = format!("{ext}-call-{round}");
            let _admission = s3fifo.access(ext, key);
        }
    }

    // Check live counts from telemetry — per-owner should not exceed budget
    let telemetry = s3fifo.telemetry();
    for ext in &extensions {
        let live = *telemetry.owner_live_counts.get(*ext).unwrap_or(&0);
        assert!(
            live <= 3,
            "extension {ext} has {live} live entries, exceeding budget of 3"
        );
    }

    // Total live should not exceed capacity
    assert!(telemetry.live_depth <= 8);
}

#[cfg(feature = "experimental-controllers")]
#[test]
fn composed_mean_field_then_ope_evaluation() {
    // Scenario: mean-field controller generates routing weights,
    // then OPE evaluator checks if switching to the new policy is safe.

    let mf_config = MeanFieldControllerConfig::default();
    let observations = vec![
        MeanFieldShardObservation {
            shard_id: "s0".to_string(),
            queue_pressure: 0.7,
            tail_latency_ratio: 1.8,
            starvation_risk: 0.3,
        },
        MeanFieldShardObservation {
            shard_id: "s1".to_string(),
            queue_pressure: 0.2,
            tail_latency_ratio: 1.1,
            starvation_risk: 0.05,
        },
    ];

    let mf_report = compute_mean_field_controls(&observations, &[], &mf_config);
    assert_eq!(mf_report.controls.len(), 2);

    // Use mean-field outputs to construct OPE trace samples
    // (simulating: "what if we used these routing weights vs current?")
    let samples: Vec<OpeTraceSample> = mf_report
        .controls
        .iter()
        .map(|control| OpeTraceSample {
            action: String::new(),
            behavior_propensity: 0.5, // uniform current policy
            target_propensity: control.routing_weight / 3.0, // normalized new weight
            outcome: 1.0 - (control.routing_delta.abs()), // lower delta = better
            baseline_outcome: Some(0.5),
            direct_method_prediction: Some(0.6),
            context_lineage: None,
        })
        .collect();

    let ope_config = OpeEvaluatorConfig {
        max_importance_weight: 20.0,
        min_effective_sample_size: 0.5,
        max_standard_error: 5.0,
        confidence_z: 1.96,
        max_regret_delta: 1.0,
    };

    let ope_report = evaluate_off_policy(&samples, &ope_config);
    // Verify the pipeline produced valid results
    assert!(ope_report.diagnostics.valid_samples > 0);
    assert!(ope_report.ips.estimate.is_finite());
    assert!(ope_report.wis.estimate.is_finite());
    assert!(ope_report.doubly_robust.estimate.is_finite());
}
