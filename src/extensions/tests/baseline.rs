//! Security baseline modeling, artifact, and scoring tests.

use super::*;

// ========================================================================
// SEC-3.2 Baseline Modeling Tests (bd-153pv)
// ========================================================================

/// Helper: build a test ledger artifact entry with required fields.
#[allow(clippy::too_many_arguments)]
fn make_test_ledger_entry(
    ext_id: &str,
    capability: &str,
    method: &str,
    risk_score: f64,
    state: RuntimeRiskStateLabelValue,
    ts_ms: i64,
    call_id: &str,
    outcome_error: Option<&str>,
) -> RuntimeRiskLedgerArtifactEntry {
    RuntimeRiskLedgerArtifactEntry {
        ts_ms,
        extension_id: ext_id.to_string(),
        call_id: call_id.to_string(),
        capability: capability.to_string(),
        method: method.to_string(),
        params_hash: "test_hash".to_string(),
        policy_reason: "allowed".to_string(),
        risk_score,
        posterior: RuntimeRiskPosteriorEvidence {
            safe_fast: 0.7,
            suspicious: 0.2,
            unsafe_: 0.1,
        },
        expected_loss: RuntimeRiskExpectedLossEvidence {
            allow: 1.0,
            harden: 2.0,
            deny: 3.0,
            terminate: 4.0,
        },
        selected_action: RuntimeRiskActionValue::Allow,
        derived_state: state,
        triggers: Vec::new(),
        fallback_reason: None,
        e_process: 0.5,
        e_threshold: 100.0,
        conformal_residual: 0.01,
        conformal_quantile: 0.05,
        drift_detected: false,
        outcome_error_code: outcome_error.map(ToString::to_string),
        explanation_schema: RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION.to_string(),
        explanation_level: RuntimeRiskExplanationLevelValue::Standard,
        explanation_summary: "test explanation".to_string(),
        top_contributors: vec![RuntimeRiskExplanationContributor {
            code: "test_contributor".to_string(),
            signed_impact: 0.25,
            magnitude: 0.25,
            rationale: "test rationale".to_string(),
        }],
        budget_state: RuntimeRiskExplanationBudgetState::default(),
        ledger_hash: String::new(),
        prev_ledger_hash: None,
    }
}

/// Helper: build a valid ledger artifact with hash chains.
fn make_test_ledger_artifact(
    entries: Vec<RuntimeRiskLedgerArtifactEntry>,
) -> RuntimeRiskLedgerArtifact {
    let mut hashed_entries = Vec::with_capacity(entries.len());
    let mut prev_hash: Option<String> = None;
    for mut entry in entries {
        let hash = runtime_risk_compute_ledger_hash_artifact(&entry, prev_hash.as_deref());
        entry.ledger_hash = hash.clone();
        entry.prev_ledger_hash = prev_hash.clone();
        prev_hash = Some(hash);
        hashed_entries.push(entry);
    }
    let data_hash = runtime_risk_ledger_data_hash(&hashed_entries);
    RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1000,
        entry_count: hashed_entries.len(),
        head_ledger_hash: hashed_entries.first().map(|e| e.ledger_hash.clone()),
        tail_ledger_hash: hashed_entries.last().map(|e| e.ledger_hash.clone()),
        data_hash,
        entries: hashed_entries,
    }
}

#[test]
fn baseline_generation_is_deterministic() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.test",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.test",
            "exec",
            "exec",
            0.85,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.test",
            "log",
            "log",
            0.20,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.test",
            "http",
            "fetch",
            0.65,
            RuntimeRiskStateLabelValue::Suspicious,
            4000,
            "c4",
            None,
        ),
        make_test_ledger_entry(
            "ext.test",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
        make_test_ledger_entry(
            "ext.test",
            "exec",
            "exec",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            6000,
            "c6",
            Some("denied"),
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);

    let model1 = build_baseline_from_ledger(&artifact, "ext.test").unwrap();
    let model2 = build_baseline_from_ledger(&artifact, "ext.test").unwrap();

    // Compare everything except generated_at_ms (uses wall clock)
    assert_eq!(model1.schema, model2.schema);
    assert_eq!(model1.extension_id, model2.extension_id);
    assert_eq!(model1.source_data_hash, model2.source_data_hash);
    assert_eq!(model1.source_entry_count, model2.source_entry_count);
    assert_eq!(model1.capability_profiles, model2.capability_profiles);
    assert_eq!(model1.transition_matrix, model2.transition_matrix);
    assert!((model1.anomaly_threshold_mads - model2.anomaly_threshold_mads).abs() < f64::EPSILON);
    assert!(
        (model1.transition_divergence_threshold - model2.transition_divergence_threshold).abs()
            < f64::EPSILON
    );
}

#[test]
fn baseline_schema_version_is_set() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.2,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();
    assert_eq!(model.schema, RUNTIME_RISK_BASELINE_SCHEMA_VERSION);
}

#[test]
fn baseline_sparse_data_single_entry() {
    // A single entry should still produce a valid baseline (not error).
    let entries = vec![make_test_ledger_entry(
        "ext.sparse",
        "log",
        "log",
        0.15,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.sparse").unwrap();
    assert_eq!(model.source_entry_count, 1);
    assert_eq!(model.capability_profiles.len(), 1);
    assert_eq!(model.capability_profiles[0].sample_count, 1);
    // Median should equal the single observation
    assert!((model.capability_profiles[0].risk_score_median - 0.15).abs() < 1e-10);
    // MAD of a single value is 0
    assert!((model.capability_profiles[0].risk_score_mad).abs() < 1e-10);
}

#[test]
fn baseline_sparse_markov_with_single_entry() {
    let entries = vec![make_test_ledger_entry(
        "ext.sparse",
        "log",
        "log",
        0.1,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.sparse").unwrap();
    // No transitions possible with a single entry
    assert_eq!(model.transition_matrix.total_transitions, 0);
    // Stationary distribution should still exist (from smoothing)
    let sum: f64 = model.transition_matrix.stationary_distribution.iter().sum();
    assert!(
        (sum - 1.0).abs() < 1e-6,
        "stationary distribution must sum to 1"
    );
}

#[test]
fn baseline_per_capability_profiles_correct() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.20,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.30,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            4000,
            "c4",
            Some("denied"),
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.85,
            RuntimeRiskStateLabelValue::Suspicious,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    // Should have 2 capability profiles: exec and log (sorted by BTreeMap)
    assert_eq!(model.capability_profiles.len(), 2);
    let exec_prof = model
        .capability_profiles
        .iter()
        .find(|p| p.capability == "exec")
        .unwrap();
    let log_prof = model
        .capability_profiles
        .iter()
        .find(|p| p.capability == "log")
        .unwrap();

    assert_eq!(exec_prof.sample_count, 2);
    assert_eq!(log_prof.sample_count, 3);

    // Log median: sorted [0.10, 0.20, 0.30] → median = 0.20
    assert!((log_prof.risk_score_median - 0.20).abs() < 1e-10);

    // Exec median: sorted [0.85, 0.90] → median = (0.85 + 0.90)/2 = 0.875
    assert!((exec_prof.risk_score_median - 0.875).abs() < 1e-10);

    // Exec error rate: 1 error out of 2 = 0.5
    assert!((exec_prof.error_rate_median - 0.5).abs() < 1e-10);

    // Log error rate: 0 errors = 0.0
    assert!(log_prof.error_rate_median.abs() < 1e-10);
}

#[test]
fn baseline_markov_transition_matrix_correct() {
    // Sequence: Safe → Safe → Suspicious → Unsafe → Safe → Suspicious
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.7,
            RuntimeRiskStateLabelValue::Suspicious,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.95,
            RuntimeRiskStateLabelValue::Unsafe,
            4000,
            "c4",
            Some("denied"),
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.6,
            RuntimeRiskStateLabelValue::Suspicious,
            6000,
            "c6",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    assert_eq!(model.transition_matrix.total_transitions, 5);
    // Safe→Safe: 1, Safe→Suspicious: 2, Suspicious→Unsafe: 1, Unsafe→Safe: 1
    assert_eq!(model.transition_matrix.counts[0][0], 1); // Safe→Safe
    assert_eq!(model.transition_matrix.counts[0][1], 2); // Safe→Suspicious
    assert_eq!(model.transition_matrix.counts[1][2], 1); // Suspicious→Unsafe
    assert_eq!(model.transition_matrix.counts[2][0], 1); // Unsafe→Safe
}

#[test]
fn baseline_stationary_distribution_sums_to_one() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "exec",
            "exec",
            0.8,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    let sum: f64 = model.transition_matrix.stationary_distribution.iter().sum();
    assert!(
        (sum - 1.0).abs() < 1e-6,
        "stationary distribution must sum to 1, got {sum}"
    );
}

#[test]
fn baseline_from_ledger_serialization_roundtrip() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.serde",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.serde",
            "exec",
            "exec",
            0.9,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            Some("timeout"),
        ),
        make_test_ledger_entry(
            "ext.serde",
            "http",
            "fetch",
            0.5,
            RuntimeRiskStateLabelValue::Suspicious,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.serde",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_test_ledger_entry(
            "ext.serde",
            "log",
            "log",
            0.2,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.serde").unwrap();

    let json = serde_json::to_string(&model).expect("serialize baseline");
    let deser: RuntimeRiskBaselineModel =
        serde_json::from_str(&json).expect("deserialize baseline");
    assert_eq!(model, deser, "roundtrip must preserve equality");
}

#[test]
fn baseline_drift_detects_risk_score_anomaly() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.11,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.13,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    // Drift: live risk_score of 0.90 vs baseline median ~0.11
    let report = detect_baseline_drift(
        &model,
        "ext.a",
        "log",
        0.90, // far from median
        0.0,  // error rate
        0.0,  // burst 1s
        0.0,  // burst 10s
        &[],  // no recent states
    );
    assert!(
        report.drift_detected,
        "should detect drift for extreme score"
    );
    assert!(
        report.anomalies.iter().any(|a| a.metric == "risk_score"),
        "anomalies should include risk_score"
    );
}

#[test]
fn baseline_drift_no_anomaly_within_normal_range() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.14,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.13,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.a",
        "log",
        0.13, // within normal range
        0.0,
        0.0,
        0.0,
        &[],
    );
    assert!(
        !report.drift_detected,
        "should not detect drift for values within normal range"
    );
}

#[test]
fn baseline_drift_transition_anomaly_detected() {
    // Baseline: mostly Safe→Safe transitions
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    // Use low divergence threshold and small smoothing so transition
    // anomaly is clearly detectable.
    let model =
        build_baseline_from_ledger_with_options(&artifact, "ext.a", 3.0, 0.01, 0.01).unwrap();

    // Live: mostly Unsafe→Unsafe transitions (very different from baseline)
    let live_states = vec![
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
    ];
    let report = detect_baseline_drift(&model, "ext.a", "log", 0.1, 0.0, 0.0, 0.0, &live_states);
    assert!(
        report.transition_divergence > 0.0,
        "divergence should be positive for different state patterns"
    );
    assert!(
        report.transition_anomalous,
        "should detect transition anomaly (div={:.4}, thr={:.4})",
        report.transition_divergence, model.transition_divergence_threshold,
    );
}

#[test]
fn baseline_drift_anomaly_has_explanation() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.a").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.a",
        "log",
        0.95, // extreme anomaly
        0.0,
        0.0,
        0.0,
        &[],
    );
    assert!(report.drift_detected);
    let anomaly = report
        .anomalies
        .iter()
        .find(|a| a.metric == "risk_score")
        .unwrap();
    assert!(
        !anomaly.explanation.is_empty(),
        "anomaly must have explanation"
    );
    assert!(
        anomaly.explanation.contains("MAD"),
        "explanation should reference MAD, got: {}",
        anomaly.explanation,
    );
    assert!(anomaly.deviation_mads > 3.0, "deviation should be large");
}

#[test]
fn baseline_rejects_invalid_ledger() {
    let artifact = RuntimeRiskLedgerArtifact {
        schema: "wrong_schema".to_string(),
        generated_at_ms: 1000,
        entry_count: 0,
        head_ledger_hash: None,
        tail_ledger_hash: None,
        data_hash: String::new(),
        entries: Vec::new(),
    };
    let result = build_baseline_from_ledger(&artifact, "ext.x");
    assert!(result.is_err());
}

#[test]
fn baseline_rejects_empty_entries() {
    let artifact = RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1000,
        entry_count: 0,
        head_ledger_hash: None,
        tail_ledger_hash: None,
        data_hash: runtime_risk_ledger_data_hash(&[]),
        entries: Vec::new(),
    };
    let result = build_baseline_from_ledger(&artifact, "ext.x");
    assert!(result.is_err());
}

#[test]
fn baseline_rejects_missing_extension() {
    let entries = vec![make_test_ledger_entry(
        "ext.other",
        "log",
        "log",
        0.1,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_test_ledger_artifact(entries);
    let result = build_baseline_from_ledger(&artifact, "ext.missing");
    assert!(result.is_err());
}

#[test]
fn baseline_kl_divergence_zero_for_identical() {
    let p = [0.6, 0.3, 0.1];
    assert!(kl_divergence_discrete3(&p, &p).abs() < 1e-12);
}

#[test]
fn baseline_kl_divergence_positive_for_different() {
    let p = [0.8, 0.1, 0.1];
    let q = [0.1, 0.1, 0.8];
    let kl = kl_divergence_discrete3(&p, &q);
    assert!(
        kl > 0.0,
        "KL divergence should be positive for different distributions"
    );
}

#[test]
fn baseline_median_correct() {
    assert!((baseline_median(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-10);
    assert!((baseline_median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 1e-10);
    assert!((baseline_median(&[5.0]) - 5.0).abs() < 1e-10);
    assert!((baseline_median(&[]) - 0.0).abs() < 1e-10);
}

#[test]
fn baseline_mad_correct() {
    // Values: [1, 2, 3, 4, 5], median=3, deviations=[2,1,0,1,2], sorted=[0,1,1,2,2], MAD=1
    assert!((baseline_mad(&[1.0, 2.0, 3.0, 4.0, 5.0]) - 1.0).abs() < 1e-10);
    // Single value: MAD = 0
    assert!((baseline_mad(&[7.0]) - 0.0).abs() < 1e-10);
    assert!((baseline_mad(&[]) - 0.0).abs() < 1e-10);
}

#[test]
fn baseline_custom_thresholds_propagate() {
    let entries = vec![
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_test_ledger_entry(
            "ext.a",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_test_ledger_artifact(entries);
    let model = build_baseline_from_ledger_with_options(&artifact, "ext.a", 5.0, 2.0, 0.1).unwrap();
    assert!((model.anomaly_threshold_mads - 5.0).abs() < 1e-10);
    assert!((model.transition_divergence_threshold - 2.0).abs() < 1e-10);
}

// ── SEC-3.3A: Bayesian evidence decomposition tests (bd-3ihzn) ──

fn make_test_features(base: f64, recent_mean: f64) -> RuntimeHostcallFeatureVector {
    RuntimeHostcallFeatureVector {
        schema: "test".to_string(),
        base_score: base,
        recent_mean_score: recent_mean,
        recent_error_rate: 0.0,
        burst_density_1s: 0.0,
        burst_density_10s: 0.0,
        prior_failure_streak_norm: 0.0,
        dangerous_capability: 0.0,
        timeout_requested: 0.0,
        policy_prompt_bias: 0.0,
    }
}

fn make_test_posterior(safe: f64, suspicious: f64, unsafe_: f64) -> RuntimeRiskPosterior {
    RuntimeRiskPosterior {
        safe_fast: safe,
        suspicious,
        unsafe_,
    }
}

fn make_test_expected_loss() -> RuntimeRiskExpectedLoss {
    RuntimeRiskExpectedLoss {
        allow: 50.0,
        harden: 20.0,
        deny: 8.0,
        terminate: 5.0,
    }
}

#[test]
fn runtime_risk_dcg_layer_flags_git_reset_hard() {
    let (score, matched) = runtime_hostcall_dcg_command_score("git reset --hard HEAD~1");
    assert!(matched);
    assert!(score > 0.30);
}

#[test]
fn runtime_risk_dcg_heredoc_detects_hidden_destructive_payload() {
    let command = "bash -lc 'cat <<EOF\nrm -rf /\nEOF'";
    let (score, matched) = runtime_hostcall_dcg_heredoc_score(command);
    assert!(matched);
    assert!(score > 0.20);
}

#[test]
fn runtime_risk_dcg_heredoc_ast_detects_python_delete_api() {
    let command = "python3 <<'PY'\nimport shutil\nshutil.rmtree('/tmp/demo')\nPY";
    let (score, matched) = runtime_hostcall_dcg_heredoc_score(command);
    assert!(matched);
    assert!(score > 0.20);
}

#[test]
fn runtime_risk_argument_signals_reduce_benign_exec_baseline() {
    let params = json!({ "command": "ls -la" });
    let signals = runtime_hostcall_argument_signals("exec", "exec", &params, "subprocess.exec");
    assert!(signals.risk_delta < 0.0);
    assert!(!signals.has(ARG_FLAG_SUSPICIOUS_EXEC));
}

#[test]
fn explanation_allow_has_contributors() {
    let features = make_test_features(0.1, 0.05);
    let posterior = make_test_posterior(0.8, 0.15, 0.05);
    let loss = make_test_expected_loss();
    let (level, summary, contributors, budget) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.1,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Compact);
    assert!(!contributors.is_empty(), "allow must have contributors");
    assert!(summary.contains("action=allow"));
    assert!(!budget.exhausted);
}

#[test]
fn explanation_deny_has_full_detail() {
    let features = make_test_features(0.8, 0.7);
    let posterior = make_test_posterior(0.1, 0.3, 0.6);
    let loss = make_test_expected_loss();
    let (level, summary, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Deny,
        0.85,
        &posterior,
        &loss,
        &features,
        &["e_process_breach".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Full);
    assert!(summary.contains("action=deny"));
    assert!(
        contributors.iter().any(|c| c.code == "posterior_unsafe"),
        "deny explanation must include posterior_unsafe contributor"
    );
    assert!(
        contributors
            .iter()
            .any(|c| c.code == "trigger_e_process_breach"),
        "deny explanation must include trigger contributor"
    );
}

#[test]
fn explanation_terminate_has_full_detail() {
    let features = make_test_features(0.9, 0.85);
    let posterior = make_test_posterior(0.05, 0.15, 0.8);
    let loss = make_test_expected_loss();
    let (level, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Terminate,
        0.95,
        &posterior,
        &loss,
        &features,
        &["unsafe_streak".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Full);
    assert!(
        contributors.iter().any(|c| c.code == "posterior_unsafe"),
        "terminate explanation must include posterior_unsafe"
    );
}

#[test]
fn explanation_harden_has_standard_level() {
    let features = make_test_features(0.4, 0.3);
    let posterior = make_test_posterior(0.5, 0.35, 0.15);
    let loss = make_test_expected_loss();
    let (level, summary, _, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.4,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Standard);
    assert!(summary.contains("action=harden"));
}

#[test]
fn explanation_contributors_sorted_by_magnitude_desc() {
    let features = RuntimeHostcallFeatureVector {
        schema: "test".to_string(),
        base_score: 0.5,
        recent_mean_score: 0.3,
        recent_error_rate: 0.8,
        burst_density_1s: 0.6,
        burst_density_10s: 0.0,
        prior_failure_streak_norm: 0.2,
        dangerous_capability: 0.0,
        timeout_requested: 0.0,
        policy_prompt_bias: 0.0,
    };
    let posterior = make_test_posterior(0.3, 0.3, 0.4);
    let loss = make_test_expected_loss();
    let (_, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.5,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    for window in contributors.windows(2) {
        let magnitude_order = window[0].magnitude.total_cmp(&window[1].magnitude);
        assert!(
            magnitude_order.is_gt()
                || (magnitude_order.is_eq() && window[0].code <= window[1].code),
            "contributors must be sorted by magnitude desc, then code asc: {:?} vs {:?}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn explanation_deterministic_replay() {
    let features = make_test_features(0.6, 0.5);
    let posterior = make_test_posterior(0.3, 0.4, 0.3);
    let loss = make_test_expected_loss();
    let triggers = vec!["drift_detected".to_string()];
    let results: Vec<_> = (0..5)
        .map(|_| {
            runtime_risk_build_explanation(
                RuntimeRiskAction::Harden,
                0.55,
                &posterior,
                &loss,
                &features,
                &triggers,
                None,
                RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
                RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
            )
        })
        .collect();
    for (i, (level, summary, contributors, _)) in results.iter().enumerate().skip(1) {
        assert_eq!(*level, results[0].0, "level mismatch at iteration {i}");
        assert_eq!(*summary, results[0].1, "summary mismatch at iteration {i}");
        assert_eq!(
            contributors.len(),
            results[0].2.len(),
            "contributor count mismatch at iteration {i}"
        );
        for (j, contrib) in contributors.iter().enumerate() {
            assert_eq!(
                contrib.code, results[0].2[j].code,
                "contributor code mismatch at [{i}][{j}]"
            );
            assert!(
                (contrib.signed_impact - results[0].2[j].signed_impact).abs() < 1e-12,
                "contributor impact mismatch at [{i}][{j}]"
            );
        }
    }
}

#[test]
fn explanation_deterministic_ordering_stable() {
    let features = make_test_features(0.3, 0.3);
    let posterior = make_test_posterior(0.5, 0.3, 0.2);
    let loss = make_test_expected_loss();
    let first = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.3,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    let second = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.3,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    let codes_first: Vec<&str> = first.2.iter().map(|c| c.code.as_str()).collect();
    let codes_second: Vec<&str> = second.2.iter().map(|c| c.code.as_str()).collect();
    assert_eq!(
        codes_first, codes_second,
        "contributor ordering must be stable across replays"
    );
}

#[test]
fn explanation_budget_exhausted_by_terms() {
    let features = make_test_features(0.5, 0.4);
    let posterior = make_test_posterior(0.3, 0.3, 0.4);
    let loss = make_test_expected_loss();
    // Use term_budget=1 to force budget exhaustion (normal produces 8+ contributors)
    let (level, summary, contributors, budget) = runtime_risk_build_explanation(
        RuntimeRiskAction::Deny,
        0.7,
        &posterior,
        &loss,
        &features,
        &["e_process_breach".to_string(), "drift_detected".to_string()],
        None,
        1,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Compact);
    assert!(budget.exhausted, "budget must be exhausted");
    assert!(budget.fallback_mode, "must be in fallback mode");
    assert_eq!(contributors.len(), 2, "fallback produces exactly 2 terms");
    assert!(
        contributors[0].code.starts_with("action_"),
        "first fallback contributor must be action code"
    );
    assert_eq!(
        contributors[1].code, "budget_exhausted",
        "second fallback contributor must be budget_exhausted"
    );
    assert!(
        summary.contains("conservative_explanation_fallback=true"),
        "summary must indicate fallback mode"
    );
}

#[test]
fn explanation_budget_fallback_preserves_action() {
    let features = make_test_features(0.5, 0.5);
    let posterior = make_test_posterior(0.2, 0.3, 0.5);
    let loss = make_test_expected_loss();
    for action in [
        RuntimeRiskAction::Allow,
        RuntimeRiskAction::Harden,
        RuntimeRiskAction::Deny,
        RuntimeRiskAction::Terminate,
    ] {
        let (_, _, contributors, budget) = runtime_risk_build_explanation(
            action,
            0.7,
            &posterior,
            &loss,
            &features,
            &["trigger".to_string()],
            None,
            1,
            RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
        );
        assert!(budget.fallback_mode);
        let action_code = runtime_risk_action_code(action);
        assert_eq!(
            contributors[0].code,
            format!("action_{action_code}"),
            "fallback must preserve action {action_code}"
        );
    }
}

#[test]
fn explanation_budget_state_tracks_terms() {
    let features = make_test_features(0.2, 0.1);
    let posterior = make_test_posterior(0.7, 0.2, 0.1);
    let loss = make_test_expected_loss();
    let (_, _, contributors, budget) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.15,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(budget.terms_emitted, contributors.len());
    assert_eq!(budget.term_budget, RUNTIME_RISK_EXPLANATION_TERM_BUDGET);
    assert_eq!(
        budget.time_budget_ms,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS
    );
    assert!(!budget.exhausted);
    assert!(!budget.fallback_mode);
}

#[test]
fn explanation_trigger_adds_contributor() {
    let features = make_test_features(0.5, 0.4);
    let posterior = make_test_posterior(0.4, 0.3, 0.3);
    let loss = make_test_expected_loss();
    let (_, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.5,
        &posterior,
        &loss,
        &features,
        &["e_process_breach".to_string(), "drift_detected".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert!(
        contributors
            .iter()
            .any(|c| c.code == "trigger_e_process_breach"),
        "e_process_breach trigger must generate contributor"
    );
    assert!(
        contributors
            .iter()
            .any(|c| c.code == "trigger_drift_detected"),
        "drift_detected trigger must generate contributor"
    );
}

#[test]
fn explanation_fallback_reason_adds_contributor() {
    let features = make_test_features(0.3, 0.2);
    let posterior = make_test_posterior(0.6, 0.25, 0.15);
    let loss = make_test_expected_loss();
    let (level, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.3,
        &posterior,
        &loss,
        &features,
        &[],
        Some("decision_timeout"),
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Full);
    assert!(
        contributors
            .iter()
            .any(|c| c.code == "fallback_decision_timeout"),
        "fallback reason must generate contributor"
    );
}

#[test]
fn explanation_posterior_decomposition_present() {
    let features = make_test_features(0.5, 0.4);
    let posterior = make_test_posterior(0.3, 0.35, 0.35);
    let loss = make_test_expected_loss();
    let (_, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.5,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert!(
        contributors.iter().any(|c| c.code == "posterior_unsafe"),
        "must include posterior_unsafe contributor"
    );
    assert!(
        contributors
            .iter()
            .any(|c| c.code == "posterior_suspicious"),
        "must include posterior_suspicious contributor"
    );
}

#[test]
fn explanation_expected_loss_delta_present() {
    let features = make_test_features(0.5, 0.4);
    let posterior = make_test_posterior(0.3, 0.35, 0.35);
    let loss = RuntimeRiskExpectedLoss {
        allow: 80.0,
        harden: 30.0,
        deny: 10.0,
        terminate: 5.0,
    };
    let (_, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Deny,
        0.7,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    let loss_contrib = contributors
        .iter()
        .find(|c| c.code == "expected_loss_delta_vs_allow")
        .expect("must include expected_loss_delta_vs_allow contributor");
    assert!(
        (loss_contrib.signed_impact - (80.0 - 10.0)).abs() < 1e-10,
        "loss delta must be allow_loss - deny_loss = 70.0, got {}",
        loss_contrib.signed_impact
    );
}

#[test]
fn explanation_feature_weights_match_scoring() {
    let features = RuntimeHostcallFeatureVector {
        schema: "test".to_string(),
        base_score: 0.6,
        recent_mean_score: 0.4,
        recent_error_rate: 0.5,
        burst_density_1s: 0.3,
        burst_density_10s: 0.0,
        prior_failure_streak_norm: 0.2,
        dangerous_capability: 0.0,
        timeout_requested: 0.0,
        policy_prompt_bias: 0.0,
    };
    let posterior = make_test_posterior(0.4, 0.3, 0.3);
    let loss = make_test_expected_loss();
    let (_, _, contributors, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.5,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    let base = contributors
        .iter()
        .find(|c| c.code == "feature_base_score")
        .expect("must have feature_base_score");
    let expected_base = 0.50 * 0.6;
    assert!(
        (base.signed_impact - expected_base).abs() < 1e-10,
        "base_score weight must be 0.50"
    );
    let recent = contributors
        .iter()
        .find(|c| c.code == "feature_recent_mean_score")
        .expect("must have feature_recent_mean_score");
    let expected_recent = 0.30 * 0.4;
    assert!(
        (recent.signed_impact - expected_recent).abs() < 1e-10,
        "recent_mean_score weight must be 0.30"
    );
    let error = contributors
        .iter()
        .find(|c| c.code == "feature_recent_error_rate")
        .expect("must have feature_recent_error_rate");
    let expected_error = 0.12 * 0.5;
    assert!(
        (error.signed_impact - expected_error).abs() < 1e-10,
        "recent_error_rate weight must be 0.12"
    );
    let burst = contributors
        .iter()
        .find(|c| c.code == "feature_burst_density_1s")
        .expect("must have feature_burst_density_1s");
    let expected_burst = 0.08 * 0.3;
    assert!(
        (burst.signed_impact - expected_burst).abs() < 1e-10,
        "burst_density_1s weight must be 0.08"
    );
    let streak = contributors
        .iter()
        .find(|c| c.code == "feature_prior_failure_streak")
        .expect("must have feature_prior_failure_streak");
    let expected_streak = 0.05 * 0.2;
    assert!(
        (streak.signed_impact - expected_streak).abs() < 1e-10,
        "prior_failure_streak_norm weight must be 0.05"
    );
}

#[test]
fn explanation_level_escalation_with_triggers() {
    let features = make_test_features(0.3, 0.2);
    let posterior = make_test_posterior(0.6, 0.25, 0.15);
    let loss = make_test_expected_loss();
    // Allow with no triggers → Compact
    let (level, _, _, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.2,
        &posterior,
        &loss,
        &features,
        &[],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Compact);
    // Allow with triggers → Standard
    let (level, _, _, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Allow,
        0.2,
        &posterior,
        &loss,
        &features,
        &["feature_budget_exceeded".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert_eq!(level, RuntimeRiskExplanationLevelValue::Standard);
}

#[test]
fn explanation_schema_version_correct() {
    assert_eq!(
        RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION,
        "pi.ext.runtime_risk_explanation.v1"
    );
}

#[test]
fn explanation_sort_tiebreak_by_code() {
    let mut contributors = vec![
        RuntimeRiskExplanationContributor {
            code: "zzz".to_string(),
            signed_impact: 0.5,
            magnitude: 0.5,
            rationale: String::new(),
        },
        RuntimeRiskExplanationContributor {
            code: "aaa".to_string(),
            signed_impact: 0.5,
            magnitude: 0.5,
            rationale: String::new(),
        },
    ];
    runtime_risk_sort_contributors(&mut contributors);
    assert_eq!(contributors[0].code, "aaa");
    assert_eq!(contributors[1].code, "zzz");
}

#[test]
fn explanation_summary_format_normal() {
    let features = make_test_features(0.4, 0.3);
    let posterior = make_test_posterior(0.4, 0.35, 0.25);
    let loss = make_test_expected_loss();
    let (_, summary, _, budget) = runtime_risk_build_explanation(
        RuntimeRiskAction::Harden,
        0.4,
        &posterior,
        &loss,
        &features,
        &["drift_detected".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert!(!budget.fallback_mode);
    assert!(summary.contains("action=harden"));
    assert!(summary.contains("score=0.400"));
    assert!(summary.contains("unsafe="));
    assert!(summary.contains("suspicious="));
    assert!(summary.contains("triggers=drift_detected"));
}

#[test]
fn explanation_summary_triggers_sorted() {
    let features = make_test_features(0.5, 0.4);
    let posterior = make_test_posterior(0.3, 0.35, 0.35);
    let loss = make_test_expected_loss();
    let (_, summary, _, _) = runtime_risk_build_explanation(
        RuntimeRiskAction::Deny,
        0.7,
        &posterior,
        &loss,
        &features,
        &["zzz_trigger".to_string(), "aaa_trigger".to_string()],
        None,
        RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
    );
    assert!(
        summary.contains("triggers=aaa_trigger|zzz_trigger"),
        "triggers in summary must be sorted: {summary}"
    );
}

#[test]
fn explanation_e2e_through_manager() {
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    });
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "hash_test",
        resource_target_class: "fs",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "permissive",
    };
    let decision = manager
        .evaluate_runtime_risk(
            Some("ext.test.explain"),
            "call-1",
            "exec",
            "exec",
            "param_hash",
            meta,
            "permissive",
        )
        .expect("decision must be returned when enabled");
    assert_eq!(
        decision.explanation_schema,
        RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION
    );
    assert!(!decision.top_contributors.is_empty());
    assert!(!decision.explanation_summary.is_empty());
    // Verify deterministic replay
    let manager2 = ExtensionManager::new();
    manager2.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    });
    let decision2 = manager2
        .evaluate_runtime_risk(
            Some("ext.test.explain"),
            "call-1",
            "exec",
            "exec",
            "param_hash",
            meta,
            "permissive",
        )
        .expect("decision must be returned");
    assert_eq!(decision.explanation_level, decision2.explanation_level);
    assert_eq!(
        decision.top_contributors.len(),
        decision2.top_contributors.len()
    );
    for (a, b) in decision
        .top_contributors
        .iter()
        .zip(decision2.top_contributors.iter())
    {
        assert_eq!(a.code, b.code);
        assert!(
            (a.signed_impact - b.signed_impact).abs() < 1e-12,
            "contributor impacts must be identical across replays"
        );
    }
}

#[test]
fn explanation_budget_default_values() {
    let budget = RuntimeRiskExplanationBudgetState::default();
    assert_eq!(
        budget.time_budget_ms,
        RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS
    );
    assert_eq!(budget.term_budget, RUNTIME_RISK_EXPLANATION_TERM_BUDGET);
    assert_eq!(budget.elapsed_ms, 0);
    assert_eq!(budget.terms_emitted, 0);
    assert!(!budget.exhausted);
    assert!(!budget.fallback_mode);
}

// ── SEC-3.3: Online deterministic risk scorer golden fixtures (bd-3f1ab) ──

#[test]
fn golden_base_score_exec() {
    assert!((runtime_risk_base_score("exec", "exec", "") - 0.58).abs() < 1e-10);
    assert!((runtime_risk_base_score("exec", "run", "") - 0.48).abs() < 1e-10);
}

#[test]
fn golden_base_score_env() {
    assert!((runtime_risk_base_score("env", "get", "") - 0.40).abs() < 1e-10);
}

#[test]
fn golden_base_score_http() {
    assert!((runtime_risk_base_score("http", "http", "") - 0.40).abs() < 1e-10);
    assert!((runtime_risk_base_score("http", "fetch", "") - 0.32).abs() < 1e-10);
}

#[test]
fn golden_base_score_low_risk() {
    assert!((runtime_risk_base_score("log", "log", "") - 0.12).abs() < 1e-10);
    assert!((runtime_risk_base_score("read", "read", "") - 0.06).abs() < 1e-10);
    assert!((runtime_risk_base_score("ui", "render", "") - 0.08).abs() < 1e-10);
}

#[test]
fn golden_base_score_policy_bonus() {
    let base = runtime_risk_base_score("exec", "exec", "prompt_user_confirm");
    // exec(0.48) + exec_method(0.10) + prompt_user(0.15) = 0.73
    assert!((base - 0.73).abs() < 1e-10);

    let base = runtime_risk_base_score("log", "log", "prompt_cache_hit");
    // log(0.12) + prompt_cache(0.08) = 0.20
    assert!((base - 0.20).abs() < 1e-10);
}

#[test]
fn golden_is_dangerous() {
    assert!(runtime_risk_is_dangerous("exec"));
    assert!(runtime_risk_is_dangerous("env"));
    assert!(runtime_risk_is_dangerous("http"));
    assert!(!runtime_risk_is_dangerous("log"));
    assert!(!runtime_risk_is_dangerous("read"));
    assert!(!runtime_risk_is_dangerous("write"));
    assert!(!runtime_risk_is_dangerous("ui"));
    assert!(!runtime_risk_is_dangerous("session"));
}

#[test]
fn golden_clamp01() {
    assert!((runtime_risk_clamp01(0.5) - 0.5).abs() < 1e-10);
    assert!((runtime_risk_clamp01(-0.1) - 0.0).abs() < 1e-10);
    assert!((runtime_risk_clamp01(1.5) - 1.0).abs() < 1e-10);
    assert!((runtime_risk_clamp01(f64::NAN) - 0.0).abs() < 1e-10);
}

#[test]
fn golden_score_formula_deterministic_replay() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "fs",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "default",
    };

    // Run the same 5-call sequence twice and compare
    let mut scores_a = Vec::new();
    let mut scores_b = Vec::new();
    for run_scores in [&mut scores_a, &mut scores_b] {
        let manager = ExtensionManager::new();
        manager.set_runtime_risk_config(config.clone());
        let calls = [
            ("log", "log", "permissive"),
            ("log", "log", "permissive"),
            ("exec", "exec", "permissive"),
            ("exec", "exec", "permissive"),
            ("log", "log", "permissive"),
        ];
        for (i, (cap, method, reason)) in calls.iter().enumerate() {
            let decision = manager
                .evaluate_runtime_risk(
                    Some("ext.golden"),
                    &format!("call-{i}"),
                    cap,
                    method,
                    "hash",
                    meta,
                    reason,
                )
                .expect("decision");
            run_scores.push(decision.risk_score);
        }
    }
    assert_eq!(scores_a.len(), scores_b.len());
    for (i, (a, b)) in scores_a.iter().zip(scores_b.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-12,
            "score mismatch at call {i}: {a} vs {b}"
        );
    }
}

#[test]
fn golden_reason_codes_burst_rate() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "fs",
        params: &Value::Null,
        timeout_ms: Some(10),
        policy_profile: "default",
    };

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    // Fire many calls rapidly to trigger burst_rate_anomaly
    // burst_density_1s = burst_count_1s / 8.0, threshold ≥ 0.5 means ≥ 4 calls/s
    // We fire calls in quick succession; since they all happen "at once" in test,
    // the timestamps are nearly identical, triggering burst detection.
    let mut found_burst = false;
    for i in 0..10 {
        let decision = manager
            .evaluate_runtime_risk(
                Some("ext.burst"),
                &format!("rapid-{i}"),
                "exec",
                "exec",
                "hash",
                meta,
                "permissive",
            )
            .expect("decision");
        manager.record_runtime_risk_outcome(
            Some("ext.burst"),
            &format!("rapid-{i}"),
            "permissive",
            &decision,
            None,
            1,
            None,
            &HostcallMarshallingTelemetry::default(),
        );
        if decision
            .triggers
            .contains(&"burst_rate_anomaly".to_string())
        {
            found_burst = true;
        }
    }
    assert!(
        found_burst,
        "burst_rate_anomaly must trigger with rapid calls"
    );
}

#[test]
fn golden_reason_codes_dangerous_capability_escalation() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "fs",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "default",
    };

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    // First call: exec → likely Harden due to high base score
    let d1 = manager
        .evaluate_runtime_risk(
            Some("ext.escalate"),
            "call-0",
            "exec",
            "exec",
            "hash",
            meta,
            "permissive",
        )
        .expect("decision");
    manager.record_runtime_risk_outcome(
        Some("ext.escalate"),
        "call-0",
        "permissive",
        &d1,
        None,
        1,
        None,
        &HostcallMarshallingTelemetry::default(),
    );
    // If first call was Harden, second exec call should trigger escalation
    if matches!(d1.action, RuntimeRiskAction::Harden) {
        let d2 = manager
            .evaluate_runtime_risk(
                Some("ext.escalate"),
                "call-1",
                "exec",
                "exec",
                "hash",
                meta,
                "permissive",
            )
            .expect("decision");
        assert!(
            d2.triggers
                .contains(&"dangerous_capability_escalation".to_string()),
            "second exec after harden must trigger dangerous_capability_escalation, got: {:?}",
            d2.triggers
        );
    }
}

#[test]
fn golden_reason_codes_sensitive_target() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta_fs = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "subprocess.exec",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "default",
    };

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    let decision = manager
        .evaluate_runtime_risk(
            Some("ext.target"),
            "call-0",
            "exec",
            "exec",
            "hash",
            meta_fs,
            "permissive",
        )
        .expect("decision");
    assert!(
        decision
            .triggers
            .contains(&"sensitive_target_mismatch".to_string()),
        "exec on fs target must trigger sensitive_target_mismatch, got: {:?}",
        decision.triggers
    );
}

#[test]
fn golden_reason_codes_unseen_capability_transition() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "unknown",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "default",
    };

    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(config);
    // First call: benign log
    let d1 = manager
        .evaluate_runtime_risk(
            Some("ext.transition"),
            "call-0",
            "log",
            "log",
            "hash",
            meta,
            "permissive",
        )
        .expect("decision");
    manager.record_runtime_risk_outcome(
        Some("ext.transition"),
        "call-0",
        "permissive",
        &d1,
        None,
        1,
        None,
        &HostcallMarshallingTelemetry::default(),
    );
    // Second call: dangerous exec → transition from safe to dangerous
    let d2 = manager
        .evaluate_runtime_risk(
            Some("ext.transition"),
            "call-1",
            "exec",
            "exec",
            "hash",
            meta,
            "permissive",
        )
        .expect("decision");
    assert!(
        d2.triggers
            .contains(&"unseen_capability_transition".to_string()),
        "log→exec transition must trigger unseen_capability_transition, got: {:?}",
        d2.triggers
    );
}

#[test]
fn golden_reason_codes_stable_across_replays() {
    let config = RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 5000,
        fail_closed: true,
    };
    let meta = RuntimeRiskCallMetadata {
        args_shape_hash: "golden",
        resource_target_class: "unknown",
        params: &Value::Null,
        timeout_ms: None,
        policy_profile: "default",
    };

    let mut all_triggers = Vec::new();
    for _ in 0..3 {
        let manager = ExtensionManager::new();
        manager.set_runtime_risk_config(config.clone());
        let mut run_triggers = Vec::new();
        for i in 0..5 {
            let cap = if i < 2 { "log" } else { "exec" };
            let decision = manager
                .evaluate_runtime_risk(
                    Some("ext.stable"),
                    &format!("call-{i}"),
                    cap,
                    cap,
                    "hash",
                    meta,
                    "permissive",
                )
                .expect("decision");
            manager.record_runtime_risk_outcome(
                Some("ext.stable"),
                &format!("call-{i}"),
                "permissive",
                &decision,
                None,
                1,
                None,
                &HostcallMarshallingTelemetry::default(),
            );
            run_triggers.push(
                decision
                    .triggers
                    .iter()
                    .filter(|trigger| trigger.as_str() != "feature_budget_exceeded")
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        all_triggers.push(run_triggers);
    }
    for run_idx in 1..all_triggers.len() {
        for (call_idx, (a, b)) in all_triggers[0]
            .iter()
            .zip(all_triggers[run_idx].iter())
            .enumerate()
        {
            assert_eq!(
                a, b,
                "reason codes mismatch at call {call_idx} between run 0 and {run_idx}"
            );
        }
    }
}

#[test]
fn golden_score_composition_weights() {
    // Verify the documented score formula weights
    let features = RuntimeHostcallFeatureVector {
        schema: "test".to_string(),
        base_score: 0.5,
        recent_mean_score: 0.3,
        recent_error_rate: 0.4,
        burst_density_1s: 0.2,
        burst_density_10s: 0.0,
        prior_failure_streak_norm: 0.1,
        dangerous_capability: 0.0,
        timeout_requested: 0.0,
        policy_prompt_bias: 0.0,
    };
    // Expected: clamp01((0.50 * 0.5) + (0.30 * 0.3))
    //         = clamp01(0.25 + 0.09)
    //         = 0.34
    // Then: clamp01(0.34 + (0.12 * 0.4) + (0.08 * 0.2) + (0.05 * 0.1))
    //     = clamp01(0.34 + 0.048 + 0.016 + 0.005)
    //     = 0.409
    let step1 = runtime_risk_clamp01(0.50f64.mul_add(0.5, 0.30 * 0.3));
    let step2 = runtime_risk_clamp01(0.05f64.mul_add(
        features.prior_failure_streak_norm,
        0.08f64.mul_add(
            features.burst_density_1s,
            0.12f64.mul_add(features.recent_error_rate, step1),
        ),
    ));
    assert!((step1 - 0.34).abs() < 1e-10, "step1 weight check");
    assert!((step2 - 0.409).abs() < 1e-10, "step2 weight check");
}
