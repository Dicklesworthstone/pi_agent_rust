use pi::extension_scoring::{
    OpeEvaluatorConfig, OpeGateReason, OpeTraceSample, evaluate_off_policy,
};

fn sample(
    action: &str,
    behavior_propensity: f64,
    target_propensity: f64,
    outcome: f64,
    baseline_outcome: f64,
    direct_method_prediction: f64,
) -> OpeTraceSample {
    OpeTraceSample {
        action: action.to_string(),
        behavior_propensity,
        target_propensity,
        outcome,
        baseline_outcome: Some(baseline_outcome),
        direct_method_prediction: Some(direct_method_prediction),
        context_lineage: Some(format!("ctx:{action}")),
    }
}

const fn permissive_thresholds() -> OpeEvaluatorConfig {
    OpeEvaluatorConfig {
        max_importance_weight: 100.0,
        min_effective_sample_size: 1.0,
        max_standard_error: 10.0,
        confidence_z: 1.96,
        max_regret_delta: 10.0,
    }
}

#[test]
fn ope_gate_no_valid_samples_integration() {
    let config = OpeEvaluatorConfig::default();
    let samples = vec![
        sample("invalid-a", 0.0, 0.5, 1.0, 1.0, 1.0),
        sample("invalid-b", -1.0, 0.5, 1.0, 1.0, 1.0),
    ];

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::NoValidSamples);
    assert!(!report.gate.passed);
    assert_eq!(report.diagnostics.valid_samples, 0);
}

#[test]
fn ope_gate_insufficient_support_integration() {
    let config = OpeEvaluatorConfig {
        min_effective_sample_size: 4.0,
        ..permissive_thresholds()
    };

    let mut samples = vec![sample("candidate", 0.02, 1.0, 0.0, 0.0, 0.0)];
    for _ in 0..9 {
        samples.push(sample("candidate", 1.0, 0.02, 1.0, 1.0, 1.0));
    }

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::InsufficientSupport);
    assert!(!report.gate.passed);
    assert!(report.diagnostics.effective_sample_size < 2.0);
}

#[test]
fn ope_gate_high_uncertainty_integration() {
    let config = OpeEvaluatorConfig {
        max_standard_error: 0.05,
        ..permissive_thresholds()
    };
    let samples = (0..20)
        .map(|idx| {
            let outcome = if idx % 2 == 0 { 0.0 } else { 1.0 };
            sample("uncertain", 0.5, 0.5, outcome, outcome, outcome)
        })
        .collect::<Vec<_>>();

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::HighUncertainty);
    assert!(!report.gate.passed);
    assert!(report.doubly_robust.standard_error > config.max_standard_error);
}

#[test]
fn ope_gate_excessive_regret_integration() {
    let config = OpeEvaluatorConfig {
        max_regret_delta: 0.1,
        ..permissive_thresholds()
    };
    let samples = (0..16)
        .map(|_| sample("regretful", 0.5, 0.5, 0.0, 1.0, 0.0))
        .collect::<Vec<_>>();

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::ExcessiveRegret);
    assert!(!report.gate.passed);
    assert!(report.estimated_regret_delta > config.max_regret_delta);
}

#[test]
fn ope_gate_approved_integration() {
    let config = OpeEvaluatorConfig {
        max_regret_delta: 0.25,
        ..permissive_thresholds()
    };
    let samples = vec![
        sample("approved", 0.5, 0.5, 0.9, 0.85, 0.9),
        sample("approved", 0.5, 0.5, 0.8, 0.75, 0.8),
        sample("approved", 0.5, 0.5, 0.7, 0.65, 0.7),
        sample("approved", 0.5, 0.5, 0.95, 0.9, 0.95),
        sample("approved", 0.5, 0.5, 0.85, 0.8, 0.85),
        sample("approved", 0.5, 0.5, 0.75, 0.7, 0.75),
    ];

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::Approved);
    assert!(report.gate.passed);
    assert_eq!(report.diagnostics.valid_samples, samples.len());
}

#[test]
fn ope_missing_direct_method_prediction_fails_closed_under_low_overlap() {
    let config = OpeEvaluatorConfig::default();
    let samples = (0..32)
        .map(|idx| OpeTraceSample {
            action: format!("missing-dm-{idx}"),
            behavior_propensity: 1.0,
            target_propensity: 0.05,
            outcome: 1.0,
            baseline_outcome: Some(0.2),
            direct_method_prediction: None,
            context_lineage: Some("ctx:missing-dm".to_string()),
        })
        .collect::<Vec<_>>();

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(
        report.diagnostics.direct_method_fallback_samples,
        samples.len()
    );
    assert_eq!(report.gate.reason, OpeGateReason::ExcessiveRegret);
    assert!(!report.gate.passed);
    assert!(report.doubly_robust.estimate < report.baseline_mean);
    assert!(report.estimated_regret_delta > config.max_regret_delta);
}

#[test]
fn ope_numeric_instability_fails_closed_and_stays_serializable() {
    let config = OpeEvaluatorConfig {
        max_importance_weight: 1.0,
        min_effective_sample_size: 1.0,
        max_standard_error: 10.0,
        confidence_z: 1.96,
        max_regret_delta: 10.0,
    };
    let samples = vec![
        sample("dominant", 1.0, 1.0, 1.0e308, 1.0e308, 1.0e308),
        sample("tiny", 1.0, 1.0e-308, 0.0, 0.0, 0.0),
    ];

    let report = evaluate_off_policy(&samples, &config);
    assert_eq!(report.gate.reason, OpeGateReason::HighUncertainty);
    assert!(!report.gate.passed);
    assert!(report.ips.estimate.is_finite());
    assert!(report.wis.estimate.is_finite());
    assert!(report.doubly_robust.estimate.is_finite());
    assert!(report.baseline_mean.is_finite());
    assert!(report.estimated_regret_delta.is_finite());
    assert!(report.wis.standard_error >= f64::MAX / 2.0);
    serde_json::to_string(&report).expect("report must serialize");
}

const fn target_policy_reward(context: f64) -> f64 {
    context.mul_add(0.5, 0.25)
}

const fn behavior_only_reward(context: f64) -> f64 {
    context.mul_add(0.2, 0.10)
}

fn deterministic_ope_trace(sample_count: u32) -> Vec<OpeTraceSample> {
    (0..sample_count)
        .map(|idx| {
            let context = f64::from(idx) / f64::from(sample_count);
            let target_action = idx % 2 == 0;
            let action = if target_action { "target" } else { "behavior" };
            let target_reward = target_policy_reward(context);
            let outcome = if target_action {
                target_reward
            } else {
                behavior_only_reward(context)
            };

            sample(
                action,
                0.5,
                if target_action { 1.0 } else { 0.0 },
                outcome,
                0.5,
                target_reward,
            )
        })
        .collect()
}

fn estimator_rmse(estimate: f64, expected_value: f64) -> f64 {
    (estimate - expected_value).abs()
}

fn ope_estimator_errors(sample_count: u32) -> [f64; 3] {
    let config = OpeEvaluatorConfig {
        max_importance_weight: 10.0,
        min_effective_sample_size: 40.0,
        max_standard_error: 1.0,
        confidence_z: 1.96,
        max_regret_delta: 1.0,
    };
    let samples = deterministic_ope_trace(sample_count);
    let report = evaluate_off_policy(&samples, &config);
    let expected_value = 0.5;

    assert_eq!(report.gate.reason, OpeGateReason::Approved);
    assert!(report.gate.passed);
    assert_eq!(
        report.diagnostics.valid_samples,
        usize::try_from(sample_count).expect("sample count fits usize")
    );
    assert_eq!(report.diagnostics.skipped_invalid_samples, 0);
    assert_eq!(report.diagnostics.direct_method_fallback_samples, 0);
    assert_eq!(report.diagnostics.clipped_weight_samples, 0);

    [
        estimator_rmse(report.ips.estimate, expected_value),
        estimator_rmse(report.wis.estimate, expected_value),
        estimator_rmse(report.doubly_robust.estimate, expected_value),
    ]
}

#[test]
fn ope_estimators_converge_to_known_target_policy_value() {
    let small = ope_estimator_errors(100);
    let medium = ope_estimator_errors(1_000);
    let large = ope_estimator_errors(10_000);

    for (idx, name) in [(0, "IPS"), (1, "WIS"), (2, "DR")] {
        assert!(
            medium[idx] < small[idx],
            "{name} RMSE should drop from n=100 to n=1000: small={}, medium={}",
            small[idx],
            medium[idx]
        );
        assert!(
            large[idx] < medium[idx],
            "{name} RMSE should drop from n=1000 to n=10000: medium={}, large={}",
            medium[idx],
            large[idx]
        );
        assert!(
            large[idx] <= 0.0001,
            "{name} RMSE should be near zero at n=10000: large={}",
            large[idx]
        );
    }
}
