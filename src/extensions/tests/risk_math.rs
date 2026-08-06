//! Runtime-risk math, baseline primitives, and quota tests.

use super::*;

// ========================================================================
// Quantile selection semantics (bd-xqipg)
// ========================================================================

#[test]
fn quantile_empty_vec_returns_zero() {
    assert!((runtime_risk_quantile(vec![], 0.0) - 0.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(vec![], 0.5) - 0.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(vec![], 1.0) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_single_element_multi_q() {
    let v = vec![42.0];
    assert!((runtime_risk_quantile(v.clone(), 0.0) - 42.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(v.clone(), 0.5) - 42.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(v, 1.0) - 42.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_q_zero_returns_minimum() {
    let v = vec![5.0, 1.0, 9.0, 3.0, 7.0];
    assert!((runtime_risk_quantile(v, 0.0) - 1.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_q_one_returns_maximum() {
    let v = vec![5.0, 1.0, 9.0, 3.0, 7.0];
    assert!((runtime_risk_quantile(v, 1.0) - 9.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_odd_sample_count_median_integer_values() {
    // 5 elements: sorted = [1,3,5,7,9], median index = (4*0.5).round() = 2
    let v = vec![5.0, 1.0, 9.0, 3.0, 7.0];
    assert!((runtime_risk_quantile(v, 0.5) - 5.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_even_sample_count_median_integer_values() {
    // 4 elements: sorted = [2,4,6,8], median index = (3*0.5).round() = 2
    let v = vec![8.0, 2.0, 6.0, 4.0];
    assert!((runtime_risk_quantile(v, 0.5) - 6.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_all_duplicate_values() {
    let v = vec![3.0, 3.0, 3.0, 3.0, 3.0];
    assert!((runtime_risk_quantile(v.clone(), 0.0) - 3.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(v.clone(), 0.5) - 3.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(v, 1.0) - 3.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_partial_duplicates() {
    // sorted = [1,1,1,5,5,9], q=0.5 → idx = (5*0.5).round() = 3 → 5.0
    let v = vec![5.0, 1.0, 9.0, 1.0, 5.0, 1.0];
    assert!((runtime_risk_quantile(v, 0.5) - 5.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_negative_q_clamped_to_zero() {
    let v = vec![10.0, 20.0, 30.0];
    // clamp01 maps negative to 0.0 → returns minimum
    assert!((runtime_risk_quantile(v, -5.0) - 10.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_large_q_clamped_to_one() {
    let v = vec![10.0, 20.0, 30.0];
    // clamp01 maps >1 to 1.0 → returns maximum
    assert!((runtime_risk_quantile(v, 100.0) - 30.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_nan_q_clamped_to_zero() {
    let v = vec![10.0, 20.0, 30.0];
    // clamp01 maps NaN to 0.0 → returns minimum
    assert!((runtime_risk_quantile(v, f64::NAN) - 10.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_sorted_vs_unsorted_identical() {
    let sorted = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let unsorted = vec![3.0, 1.0, 5.0, 2.0, 4.0];
    for q in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let a = runtime_risk_quantile(sorted.clone(), q);
        let b = runtime_risk_quantile(unsorted.clone(), q);
        assert!(
            (a - b).abs() < f64::EPSILON,
            "mismatch at q={q}: sorted={a}, unsorted={b}"
        );
    }
}

#[test]
fn quantile_monotonicity() {
    let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
    let mut prev = f64::NEG_INFINITY;
    for q_pct in 0..=100 {
        let q = f64::from(q_pct) / 100.0;
        let val = runtime_risk_quantile(v.clone(), q);
        assert!(
            val >= prev,
            "monotonicity violated: q={q} gave {val} < prev {prev}"
        );
        prev = val;
    }
}

#[test]
fn quantile_two_elements_boundary() {
    let v = vec![0.0, 1.0];
    // q=0.0 → idx=(1*0.0).round()=0 → 0.0
    assert!((runtime_risk_quantile(v.clone(), 0.0) - 0.0).abs() < f64::EPSILON);
    // q=0.25 → idx=(1*0.25).round()=0 → 0.0
    assert!((runtime_risk_quantile(v.clone(), 0.25) - 0.0).abs() < f64::EPSILON);
    // q=0.5 → idx=(1*0.5).round()=1 → 1.0
    assert!((runtime_risk_quantile(v.clone(), 0.5) - 1.0).abs() < f64::EPSILON);
    // q=1.0 → idx=(1*1.0).round()=1 → 1.0
    assert!((runtime_risk_quantile(v, 1.0) - 1.0).abs() < f64::EPSILON);
}

#[test]
fn quantile_large_sample() {
    let v: Vec<f64> = (1..=1000).map(f64::from).collect();
    // q=0.0 → 1.0 (min), q=1.0 → 1000.0 (max)
    assert!((runtime_risk_quantile(v.clone(), 0.0) - 1.0).abs() < f64::EPSILON);
    assert!((runtime_risk_quantile(v.clone(), 1.0) - 1000.0).abs() < f64::EPSILON);
    // q=0.5 → idx=(999*0.5).round()=500 → 501.0
    let median = runtime_risk_quantile(v, 0.5);
    assert!(
        (median - 500.0).abs() <= 1.0,
        "expected median ~500, got {median}"
    );
}

#[test]
fn quantile_conformal_residual_integration() {
    // Simulate conformal residual quantile computation used in decision flow:
    // residual_window filled with residuals, then quantile(window, 1.0 - alpha)
    let alpha = 0.01;
    let residuals: Vec<f64> = (0..64).map(|i| (f64::from(i) / 63.0) * 0.5).collect();
    let quantile_val = runtime_risk_quantile(residuals, 1.0 - alpha);
    // At q=0.99, should be close to the maximum residual (~0.5)
    assert!(
        quantile_val >= 0.45,
        "conformal quantile at 1-alpha should be near max: got {quantile_val}"
    );
    assert!(
        quantile_val <= 0.5 + f64::EPSILON,
        "conformal quantile should not exceed max: got {quantile_val}"
    );
}

// ========================================================================
// Clamp01 edge cases (bd-xqipg)
// ========================================================================

#[test]
fn clamp01_nan_returns_zero() {
    assert!((runtime_risk_clamp01(f64::NAN) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn clamp01_negative_infinity_returns_zero() {
    assert!((runtime_risk_clamp01(f64::NEG_INFINITY) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn clamp01_positive_infinity_returns_one() {
    assert!((runtime_risk_clamp01(f64::INFINITY) - 1.0).abs() < f64::EPSILON);
}

#[test]
fn clamp01_normal_values_pass_through() {
    assert!((runtime_risk_clamp01(0.5) - 0.5).abs() < f64::EPSILON);
    assert!((runtime_risk_clamp01(0.0) - 0.0).abs() < f64::EPSILON);
    assert!((runtime_risk_clamp01(1.0) - 1.0).abs() < f64::EPSILON);
}

// ========================================================================
// Baseline modeling (bd-153pv)
// ========================================================================

#[test]
fn baseline_median_empty_returns_zero() {
    assert!((baseline_median(&[]) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn baseline_median_single_element() {
    assert!((baseline_median(&[4.2]) - 4.2).abs() < f64::EPSILON);
}

#[test]
fn baseline_median_odd_count() {
    assert!((baseline_median(&[1.0, 3.0, 5.0]) - 3.0).abs() < f64::EPSILON);
}

#[test]
fn baseline_median_even_count() {
    // midpoint of 3.0 and 5.0 = 4.0
    assert!((baseline_median(&[1.0, 3.0, 5.0, 7.0]) - 4.0).abs() < f64::EPSILON);
}

#[test]
fn baseline_mad_constant_data_is_zero() {
    assert!((baseline_mad(&[5.0, 5.0, 5.0, 5.0]) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn baseline_mad_known_values() {
    // data = [1, 2, 3, 4, 5], median = 3
    // deviations = [2, 1, 0, 1, 2], sorted = [0, 1, 1, 2, 2], median = 1
    let sorted = &[1.0, 2.0, 3.0, 4.0, 5.0];
    assert!((baseline_mad(sorted) - 1.0).abs() < f64::EPSILON);
}

#[test]
fn state_label_index_mapping() {
    assert_eq!(
        state_label_to_index(RuntimeRiskStateLabelValue::SafeFast),
        0
    );
    assert_eq!(
        state_label_to_index(RuntimeRiskStateLabelValue::Suspicious),
        1
    );
    assert_eq!(state_label_to_index(RuntimeRiskStateLabelValue::Unsafe), 2);
}

#[test]
fn markov_matrix_empty_states_uses_uniform_prior() {
    let matrix = build_markov_transition_matrix(&[], 1.0);
    assert_eq!(matrix.total_transitions, 0);
    // With no data, each row should be uniform (1/3 each due to smoothing)
    for row in &matrix.probabilities {
        for &prob in row {
            assert!((prob - 1.0 / 3.0).abs() < 1e-10);
        }
    }
}

#[test]
fn markov_matrix_single_transition() {
    let states = vec![
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::Suspicious,
    ];
    let matrix = build_markov_transition_matrix(&states, 1.0);
    assert_eq!(matrix.total_transitions, 1);
    // Row 0 (SafeFast): 1 transition to Suspicious + 1.0 prior each
    // counts[0] = [0, 1, 0], row_total = 1 + 3*1.0 = 4
    assert!((matrix.probabilities[0][1] - 2.0 / 4.0).abs() < 1e-10);
    assert!((matrix.probabilities[0][0] - 1.0 / 4.0).abs() < 1e-10);
}

#[test]
fn markov_matrix_deterministic() {
    let states = vec![
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::Suspicious,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::SafeFast,
    ];
    let m1 = build_markov_transition_matrix(&states, 1.0);
    let m2 = build_markov_transition_matrix(&states, 1.0);
    assert_eq!(m1, m2, "Markov matrix must be deterministic");
}

#[test]
fn markov_stationary_sums_to_one() {
    let states = vec![
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::Suspicious,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::Suspicious,
    ];
    let matrix = build_markov_transition_matrix(&states, 1.0);
    let sum: f64 = matrix.stationary_distribution.iter().sum();
    assert!(
        (sum - 1.0).abs() < 1e-8,
        "stationary distribution should sum to 1.0, got {sum}"
    );
}

#[test]
fn kl_divergence_identical_is_zero() {
    let p = [0.5, 0.3, 0.2];
    assert!((kl_divergence_discrete3(&p, &p) - 0.0).abs() < 1e-12);
}

#[test]
fn kl_divergence_different_is_positive() {
    let p = [0.9, 0.05, 0.05];
    let q = [0.33, 0.34, 0.33];
    assert!(kl_divergence_discrete3(&p, &q) > 0.0);
}

#[test]
fn build_baseline_deterministic() {
    use crate::extensions::RuntimeRiskConfig;
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

    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    let tools = crate::tools::ToolRegistry::new(&[], std::path::Path::new("/tmp"), None);
    let http = crate::connectors::http::HttpConnector::with_defaults();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test.baseline"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        for i in 0..10 {
            let call = HostCallPayload {
                call_id: format!("baseline-{i}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: serde_json::json!({ "message": format!("test-{i}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let b1 = manager.build_baseline("ext.test.baseline").unwrap();
    let b2 = manager.build_baseline("ext.test.baseline").unwrap();

    // Schema, profiles, and transition matrix should match (timestamps may differ)
    assert_eq!(b1.schema, b2.schema);
    assert_eq!(b1.capability_profiles, b2.capability_profiles);
    assert_eq!(b1.transition_matrix, b2.transition_matrix);
    assert_eq!(b1.source_entry_count, b2.source_entry_count);
}

#[test]
fn build_baseline_sparse_data_has_fallback() {
    use crate::extensions::RuntimeRiskConfig;
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

    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    let tools = crate::tools::ToolRegistry::new(&[], std::path::Path::new("/tmp"), None);
    let http = crate::connectors::http::HttpConnector::with_defaults();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test.sparse"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    // Only 1 call - very sparse data
    futures::executor::block_on(async {
        let call = HostCallPayload {
            call_id: "sparse-0".to_string(),
            capability: "log".to_string(),
            method: "log".to_string(),
            params: serde_json::json!({ "message": "sparse" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let _ = dispatch_host_call_shared(&ctx, call).await;
    });

    let baseline = manager.build_baseline("ext.test.sparse").unwrap();
    assert_eq!(baseline.source_entry_count, 1);
    assert_eq!(baseline.capability_profiles.len(), 1);
    assert_eq!(baseline.capability_profiles[0].sample_count, 1);
    // Markov matrix should have uniform prior (no transitions from 1 entry)
    assert_eq!(baseline.transition_matrix.total_transitions, 0);
}

#[test]
fn build_baseline_wrong_extension_returns_error() {
    use crate::extensions::RuntimeRiskConfig;
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

    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    let tools = crate::tools::ToolRegistry::new(&[], std::path::Path::new("/tmp"), None);
    let http = crate::connectors::http::HttpConnector::with_defaults();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.test.exists"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        let call = HostCallPayload {
            call_id: "exists-0".to_string(),
            capability: "log".to_string(),
            method: "log".to_string(),
            params: serde_json::json!({ "message": "exists" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let _ = dispatch_host_call_shared(&ctx, call).await;
    });

    let result = manager.build_baseline("ext.nonexistent");
    assert!(result.is_err(), "should fail for nonexistent extension");
}

#[test]
fn drift_detection_no_anomaly_for_baseline_data() {
    let profile = BaselineCapabilityProfile {
        capability: "log".to_string(),
        sample_count: 100,
        risk_score_median: 0.10,
        risk_score_mad: 0.02,
        risk_score_p5: 0.06,
        risk_score_p95: 0.14,
        error_rate_median: 0.0,
        burst_density_1s_median: 0.1,
        burst_density_10s_median: 0.05,
    };
    let baseline = RuntimeRiskBaselineModel {
        schema: RUNTIME_RISK_BASELINE_SCHEMA_VERSION.to_string(),
        extension_id: "ext.test".to_string(),
        generated_at_ms: 0,
        source_data_hash: "test".to_string(),
        source_entry_count: 100,
        capability_profiles: vec![profile],
        transition_matrix: build_markov_transition_matrix(&[], 1.0),
        anomaly_threshold_mads: 3.0,
        transition_divergence_threshold: 0.5,
    };

    let report = detect_baseline_drift(
        &baseline,
        "ext.test",
        "log",
        0.10, // close to median
        0.0,
        0.1,
        0.05,
        &[],
    );
    assert!(
        !report.drift_detected,
        "should not detect drift for baseline-matching data"
    );
    assert!(report.anomalies.is_empty());
}

#[test]
fn drift_detection_flags_outlier_risk_score() {
    let profile = BaselineCapabilityProfile {
        capability: "log".to_string(),
        sample_count: 100,
        risk_score_median: 0.10,
        risk_score_mad: 0.02,
        risk_score_p5: 0.06,
        risk_score_p95: 0.14,
        error_rate_median: 0.0,
        burst_density_1s_median: 0.1,
        burst_density_10s_median: 0.05,
    };
    let baseline = RuntimeRiskBaselineModel {
        schema: RUNTIME_RISK_BASELINE_SCHEMA_VERSION.to_string(),
        extension_id: "ext.test".to_string(),
        generated_at_ms: 0,
        source_data_hash: "test".to_string(),
        source_entry_count: 100,
        capability_profiles: vec![profile],
        transition_matrix: build_markov_transition_matrix(&[], 1.0),
        anomaly_threshold_mads: 3.0,
        transition_divergence_threshold: 0.5,
    };

    let report = detect_baseline_drift(
        &baseline,
        "ext.test",
        "log",
        0.90, // far from baseline median 0.10
        0.0,
        0.1,
        0.05,
        &[],
    );
    assert!(
        report.drift_detected,
        "should detect drift for outlier score"
    );
    assert!(
        report.anomalies.iter().any(|a| a.metric == "risk_score"),
        "should have risk_score anomaly"
    );
}

#[test]
fn drift_detection_transition_anomaly() {
    // Baseline: mostly SafeFast transitions
    let baseline_states = vec![
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskStateLabelValue::SafeFast,
    ];
    let baseline = RuntimeRiskBaselineModel {
        schema: RUNTIME_RISK_BASELINE_SCHEMA_VERSION.to_string(),
        extension_id: "ext.test".to_string(),
        generated_at_ms: 0,
        source_data_hash: "test".to_string(),
        source_entry_count: 50,
        capability_profiles: vec![],
        transition_matrix: build_markov_transition_matrix(&baseline_states, 1.0),
        anomaly_threshold_mads: 3.0,
        transition_divergence_threshold: 0.1, // low threshold for sensitivity
    };

    // Live: mostly Unsafe transitions
    let live_states = vec![
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
    ];
    let report = detect_baseline_drift(
        &baseline,
        "ext.test",
        "log",
        0.10,
        0.0,
        0.0,
        0.0,
        &live_states,
    );
    assert!(
        report.transition_anomalous,
        "should detect transition anomaly when live pattern differs from baseline"
    );
    assert!(report.transition_divergence > 0.0);
}

#[test]
fn baseline_model_json_roundtrip() {
    let baseline = RuntimeRiskBaselineModel {
        schema: RUNTIME_RISK_BASELINE_SCHEMA_VERSION.to_string(),
        extension_id: "ext.roundtrip".to_string(),
        generated_at_ms: 1_234_567_890,
        source_data_hash: "abc123".to_string(),
        source_entry_count: 42,
        capability_profiles: vec![BaselineCapabilityProfile {
            capability: "log".to_string(),
            sample_count: 42,
            risk_score_median: 0.1,
            risk_score_mad: 0.02,
            risk_score_p5: 0.06,
            risk_score_p95: 0.14,
            error_rate_median: 0.0,
            burst_density_1s_median: 0.1,
            burst_density_10s_median: 0.05,
        }],
        transition_matrix: build_markov_transition_matrix(&[], 1.0),
        anomaly_threshold_mads: 3.0,
        transition_divergence_threshold: 0.5,
    };

    let json = serde_json::to_string(&baseline).unwrap();
    let deserialized: RuntimeRiskBaselineModel = serde_json::from_str(&json).unwrap();
    assert_eq!(
        baseline, deserialized,
        "roundtrip should preserve all fields"
    );
}

// ── SEC-4.1: Per-Extension Resource Quota Tests ──────────────────────

#[test]
fn quota_default_matches_prompt_mode() {
    let default = ExtensionQuotaConfig::default();
    let prompt = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Prompt);
    assert_eq!(
        default.max_hostcalls_per_second,
        prompt.max_hostcalls_per_second
    );
    assert_eq!(
        default.max_hostcalls_per_minute,
        prompt.max_hostcalls_per_minute
    );
    assert_eq!(default.max_subprocesses, prompt.max_subprocesses);
}

#[test]
fn quota_strict_more_restrictive_than_prompt() {
    let strict = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Strict);
    let prompt = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Prompt);
    assert!(strict.max_hostcalls_per_second.unwrap() < prompt.max_hostcalls_per_second.unwrap());
    assert!(strict.max_hostcalls_per_minute.unwrap() < prompt.max_hostcalls_per_minute.unwrap());
    assert!(strict.max_subprocesses.unwrap() < prompt.max_subprocesses.unwrap());
    assert!(strict.max_hostcalls_total.is_some());
    assert!(prompt.max_hostcalls_total.is_none());
}

#[test]
fn quota_permissive_more_relaxed_than_prompt() {
    let permissive = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Permissive);
    let prompt = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Prompt);
    assert!(
        permissive.max_hostcalls_per_second.unwrap() > prompt.max_hostcalls_per_second.unwrap()
    );
    assert!(
        permissive.max_hostcalls_per_minute.unwrap() > prompt.max_hostcalls_per_minute.unwrap()
    );
    assert!(permissive.max_subprocesses.unwrap() > prompt.max_subprocesses.unwrap());
}

#[test]
fn quota_check_allows_within_limits() {
    let config = ExtensionQuotaConfig::default();
    let mut state = ExtensionQuotaState::default();
    let result = check_extension_quota(&config, &mut state, 1000, "tool");
    assert_eq!(result, QuotaCheckResult::Allowed);
    assert_eq!(state.hostcalls_total, 1);
}

#[test]
fn quota_per_second_burst_exceeded() {
    let config = ExtensionQuotaConfig {
        max_hostcalls_per_second: Some(3),
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    for i in 0..3 {
        let r = check_extension_quota(&config, &mut state, 1000 + i64::from(i), "tool");
        assert_eq!(r, QuotaCheckResult::Allowed);
    }
    let r = check_extension_quota(&config, &mut state, 1002, "tool");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
}

#[test]
fn quota_per_minute_rate_exceeded() {
    let config = ExtensionQuotaConfig {
        max_hostcalls_per_minute: Some(5),
        max_hostcalls_per_second: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    for i in 0..5 {
        let r = check_extension_quota(&config, &mut state, 1000 + i * 10_000, "tool");
        assert_eq!(r, QuotaCheckResult::Allowed);
    }
    let r = check_extension_quota(&config, &mut state, 41_000, "tool");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
}

#[test]
fn quota_sliding_window_expiry() {
    let config = ExtensionQuotaConfig {
        max_hostcalls_per_minute: Some(3),
        max_hostcalls_per_second: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    for _ in 0..3 {
        let _ = check_extension_quota(&config, &mut state, 1000, "tool");
    }
    let r = check_extension_quota(&config, &mut state, 1000, "tool");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
    let r = check_extension_quota(&config, &mut state, 62_000, "tool");
    assert_eq!(r, QuotaCheckResult::Allowed);
}

#[test]
fn quota_total_budget_exceeded() {
    let config = ExtensionQuotaConfig {
        max_hostcalls_total: Some(2),
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    let _ = check_extension_quota(&config, &mut state, 1000, "tool");
    let _ = check_extension_quota(&config, &mut state, 2000, "tool");
    let r = check_extension_quota(&config, &mut state, 3000, "tool");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
}

#[test]
fn quota_subprocess_limit_enforced() {
    let config = ExtensionQuotaConfig {
        max_subprocesses: Some(2),
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState {
        active_subprocesses: 2,
        ..Default::default()
    };
    let r = check_extension_quota(&config, &mut state, 1000, "exec");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
    let r2 = check_extension_quota(&config, &mut state, 2000, "tool");
    assert_eq!(r2, QuotaCheckResult::Allowed);
}

#[test]
fn quota_http_request_limit_enforced() {
    let config = ExtensionQuotaConfig {
        max_http_requests: Some(2),
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    let r1 = check_extension_quota(&config, &mut state, 1000, "http");
    assert_eq!(r1, QuotaCheckResult::Allowed);
    let r2 = check_extension_quota(&config, &mut state, 2000, "http");
    assert_eq!(r2, QuotaCheckResult::Allowed);
    let r3 = check_extension_quota(&config, &mut state, 3000, "http");
    assert!(matches!(r3, QuotaCheckResult::Exceeded { .. }));
}

#[test]
fn quota_write_bytes_limit_enforced() {
    let config = ExtensionQuotaConfig {
        max_write_bytes: Some(1024),
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState {
        write_bytes_total: 1024,
        ..Default::default()
    };
    let r = check_extension_quota(&config, &mut state, 1000, "write");
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
    let mut state2 = ExtensionQuotaState {
        write_bytes_total: 500,
        ..Default::default()
    };
    let r2 = check_extension_quota(&config, &mut state2, 1000, "write");
    assert_eq!(r2, QuotaCheckResult::Allowed);
}

#[test]
fn quota_manager_per_extension_override() {
    let manager = ExtensionManager::new();
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "test-ext".to_string(),
        ExtensionOverride {
            quota: Some(ExtensionQuotaConfig {
                max_hostcalls_per_second: Some(1),
                max_hostcalls_per_minute: Some(5),
                max_hostcalls_total: None,
                max_subprocesses: Some(1),
                max_write_bytes: None,
                max_http_requests: None,
            }),
            ..Default::default()
        },
    );
    let r1 = manager.check_quota(Some("test-ext"), "tool", 1000, &policy);
    assert_eq!(r1, QuotaCheckResult::Allowed);
    let r2 = manager.check_quota(Some("test-ext"), "tool", 1000, &policy);
    assert!(matches!(r2, QuotaCheckResult::Exceeded { .. }));
}

#[test]
fn quota_manager_global_default_no_override() {
    let manager = ExtensionManager::new();
    let policy = ExtensionPolicy::default();
    for i in 0..50 {
        let r = manager.check_quota(Some("other-ext"), "tool", 1000 + i, &policy);
        assert_eq!(r, QuotaCheckResult::Allowed);
    }
}

#[test]
fn quota_manager_no_ext_id_always_allowed() {
    let manager = ExtensionManager::new();
    let policy = ExtensionPolicy::default();
    let r = manager.check_quota(None, "tool", 1000, &policy);
    assert_eq!(r, QuotaCheckResult::Allowed);
}

#[test]
fn quota_subprocess_spawn_exit_tracking() {
    let manager = ExtensionManager::new();
    assert_eq!(manager.quota_state("ext-1"), None);
    manager.record_subprocess_spawn("ext-1");
    let (_, active, _, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(active, 1);
    manager.record_subprocess_spawn("ext-1");
    let (_, active, _, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(active, 2);
    manager.record_subprocess_exit("ext-1");
    let (_, active, _, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(active, 1);
    manager.record_subprocess_exit("ext-1");
    manager.record_subprocess_exit("ext-1");
    let (_, active, _, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(active, 0);
}

#[test]
fn quota_write_bytes_tracking() {
    let manager = ExtensionManager::new();
    manager.record_write_bytes("ext-1", 512);
    let (_, _, wb, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(wb, 512);
    manager.record_write_bytes("ext-1", 1024);
    let (_, _, wb, _) = manager.quota_state("ext-1").unwrap();
    assert_eq!(wb, 1536);
}

#[test]
fn quota_breach_telemetry_recorded() {
    let manager = ExtensionManager::new();
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "bad-ext".to_string(),
        ExtensionOverride {
            quota: Some(ExtensionQuotaConfig {
                max_hostcalls_per_second: Some(1),
                max_hostcalls_per_minute: Some(1),
                max_hostcalls_total: None,
                max_subprocesses: None,
                max_write_bytes: None,
                max_http_requests: None,
            }),
            ..Default::default()
        },
    );
    let _ = manager.check_quota(Some("bad-ext"), "tool", 1000, &policy);
    assert_eq!(manager.quota_breach_count(), 0);
    let r = manager.check_quota(Some("bad-ext"), "tool", 1000, &policy);
    assert!(matches!(r, QuotaCheckResult::Exceeded { .. }));
    assert_eq!(manager.quota_breach_count(), 1);
    let events = manager.drain_quota_breach_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].extension_id, "bad-ext");
    assert_eq!(events[0].capability, "tool");
    assert_eq!(events[0].quota_config_source, "per_extension");
    assert_eq!(manager.quota_breach_count(), 0);
}

#[test]
fn quota_reset_clears_state() {
    let manager = ExtensionManager::new();
    let policy = ExtensionPolicy::default();
    let _ = manager.check_quota(Some("ext-1"), "tool", 1000, &policy);
    manager.record_subprocess_spawn("ext-1");
    manager.record_write_bytes("ext-1", 1000);
    assert!(manager.quota_state("ext-1").is_some());
    manager.reset_quota_state("ext-1");
    assert!(manager.quota_state("ext-1").is_none());
}

#[test]
fn quota_config_serialization_roundtrip() {
    let config = ExtensionQuotaConfig::for_mode(ExtensionPolicyMode::Strict);
    let json = serde_json::to_string(&config).unwrap();
    let restored: ExtensionQuotaConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(
        config.max_hostcalls_per_second,
        restored.max_hostcalls_per_second
    );
    assert_eq!(config.max_subprocesses, restored.max_subprocesses);
    assert_eq!(config.max_write_bytes, restored.max_write_bytes);
}

#[test]
fn quota_per_extension_override_policy_serialization() {
    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "my-ext".to_string(),
        ExtensionOverride {
            quota: Some(ExtensionQuotaConfig {
                max_hostcalls_per_second: Some(10),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let json = serde_json::to_string(&policy).unwrap();
    let restored: ExtensionPolicy = serde_json::from_str(&json).unwrap();
    let ovr = restored.per_extension.get("my-ext").unwrap();
    assert_eq!(
        ovr.quota.as_ref().unwrap().max_hostcalls_per_second,
        Some(10)
    );
}

#[test]
fn quota_monotonic_total_never_decreases() {
    let config = ExtensionQuotaConfig {
        max_hostcalls_per_second: None,
        max_hostcalls_per_minute: None,
        ..Default::default()
    };
    let mut state = ExtensionQuotaState::default();
    for i in 0..100 {
        let _ = check_extension_quota(&config, &mut state, i * 1000, "tool");
    }
    assert_eq!(state.hostcalls_total, 100);
}
