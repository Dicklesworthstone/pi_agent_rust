#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops
)]
//! Integration tests for SEC-3.2: Baseline modeling with robust statistics
//! and Markov transition profiles.
//!
//! Validates the public API for building per-extension baseline models from
//! approved runtime risk ledger traces, including deterministic artifact
//! generation, sparse-data fallbacks, and explainable drift detection.

use pi::extensions::{
    BaselineDriftReport, RUNTIME_RISK_BASELINE_SCHEMA_VERSION,
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RUNTIME_RISK_LEDGER_SCHEMA_VERSION,
    RuntimeRiskActionValue, RuntimeRiskBaselineModel, RuntimeRiskExpectedLossEvidence,
    RuntimeRiskExplanationBudgetState, RuntimeRiskExplanationContributor,
    RuntimeRiskExplanationLevelValue, RuntimeRiskLedgerArtifact, RuntimeRiskLedgerArtifactEntry,
    RuntimeRiskPosteriorEvidence, RuntimeRiskStateLabelValue, build_baseline_from_ledger,
    build_baseline_from_ledger_with_options, detect_baseline_drift,
    runtime_risk_compute_ledger_hash_artifact, runtime_risk_ledger_data_hash,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn make_entry(
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

fn make_artifact(entries: Vec<RuntimeRiskLedgerArtifactEntry>) -> RuntimeRiskLedgerArtifact {
    let mut hashed = Vec::with_capacity(entries.len());
    let mut prev: Option<String> = None;
    for mut e in entries {
        let hash = runtime_risk_compute_ledger_hash_artifact(&e, prev.as_deref());
        e.ledger_hash.clone_from(&hash);
        e.prev_ledger_hash.clone_from(&prev);
        prev = Some(hash);
        hashed.push(e);
    }
    let data_hash = runtime_risk_ledger_data_hash(&hashed);
    RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1000,
        entry_count: hashed.len(),
        head_ledger_hash: hashed.first().map(|e| e.ledger_hash.clone()),
        tail_ledger_hash: hashed.last().map(|e| e.ledger_hash.clone()),
        data_hash,
        entries: hashed,
    }
}

// ---------------------------------------------------------------------------
// Schema and determinism
// ---------------------------------------------------------------------------

#[test]
fn baseline_schema_is_stable() {
    assert_eq!(
        RUNTIME_RISK_BASELINE_SCHEMA_VERSION,
        "pi.ext.runtime_risk_baseline.v1"
    );
}

#[test]
fn baseline_generation_deterministic_across_calls() {
    let entries = vec![
        make_entry(
            "ext.det",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.det",
            "exec",
            "spawn",
            0.80,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.det",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.det",
            "http",
            "fetch",
            0.55,
            RuntimeRiskStateLabelValue::Suspicious,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.det",
            "log",
            "log",
            0.09,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);

    let m1 = build_baseline_from_ledger(&artifact, "ext.det").unwrap();
    let m2 = build_baseline_from_ledger(&artifact, "ext.det").unwrap();

    assert_eq!(m1.schema, m2.schema);
    assert_eq!(m1.extension_id, m2.extension_id);
    assert_eq!(m1.source_data_hash, m2.source_data_hash);
    assert_eq!(m1.source_entry_count, m2.source_entry_count);
    assert_eq!(m1.capability_profiles, m2.capability_profiles);
    assert_eq!(m1.transition_matrix, m2.transition_matrix);
}

#[test]
fn baseline_json_roundtrip_preserves_all_fields() {
    let entries = vec![
        make_entry(
            "ext.serde",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.serde",
            "exec",
            "exec",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            Some("denied"),
        ),
        make_entry(
            "ext.serde",
            "log",
            "log",
            0.20,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.serde").unwrap();

    let json = serde_json::to_string_pretty(&model).unwrap();
    let restored: RuntimeRiskBaselineModel = serde_json::from_str(&json).unwrap();

    assert_eq!(model.schema, restored.schema);
    assert_eq!(model.extension_id, restored.extension_id);
    assert_eq!(model.source_entry_count, restored.source_entry_count);
    // Capability profiles and transition matrix use epsilon comparison
    // because JSON float roundtrip may lose precision at the last ULP
    assert_eq!(
        model.capability_profiles.len(),
        restored.capability_profiles.len()
    );
    for (a, b) in model
        .capability_profiles
        .iter()
        .zip(&restored.capability_profiles)
    {
        assert_eq!(a.capability, b.capability);
        assert_eq!(a.sample_count, b.sample_count);
        assert!((a.risk_score_median - b.risk_score_median).abs() < 1e-12);
    }
    assert_eq!(
        model.transition_matrix.counts,
        restored.transition_matrix.counts
    );
    assert_eq!(
        model.transition_matrix.total_transitions,
        restored.transition_matrix.total_transitions
    );
    for (a, b) in model
        .transition_matrix
        .stationary_distribution
        .iter()
        .zip(&restored.transition_matrix.stationary_distribution)
    {
        assert!(
            (a - b).abs() < 1e-12,
            "stationary distribution should survive JSON roundtrip"
        );
    }
    assert!((model.anomaly_threshold_mads - restored.anomaly_threshold_mads).abs() < 1e-12);
    assert!(
        (model.transition_divergence_threshold - restored.transition_divergence_threshold).abs()
            < 1e-12
    );
}

#[test]
fn baseline_byte_determinism_across_serialization() {
    let entries = vec![
        make_entry(
            "ext.bytes",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.bytes",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let m1 = build_baseline_from_ledger(&artifact, "ext.bytes").unwrap();
    let m2 = build_baseline_from_ledger(&artifact, "ext.bytes").unwrap();

    let j1 = serde_json::to_string(&m1).unwrap();
    let j2 = serde_json::to_string(&m2).unwrap();

    // Mask generated_at_ms (wall clock) for comparison
    let mask = |s: String| -> String {
        let re = regex::Regex::new(r#""generated_at_ms":\d+"#).unwrap();
        re.replace_all(&s, r#""generated_at_ms":0"#).to_string()
    };
    assert_eq!(
        mask(j1),
        mask(j2),
        "serialized baselines must be byte-identical (modulo timestamp)"
    );
}

// ---------------------------------------------------------------------------
// Capability profile statistics
// ---------------------------------------------------------------------------

#[test]
fn baseline_computes_per_capability_profiles() {
    let entries = vec![
        make_entry(
            "ext.cap",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.cap",
            "log",
            "log",
            0.20,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.cap",
            "log",
            "log",
            0.30,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.cap",
            "exec",
            "spawn",
            0.80,
            RuntimeRiskStateLabelValue::Suspicious,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.cap",
            "exec",
            "spawn",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.cap").unwrap();

    assert_eq!(
        model.capability_profiles.len(),
        2,
        "should have log and exec profiles"
    );

    let log_prof = model
        .capability_profiles
        .iter()
        .find(|p| p.capability == "log")
        .unwrap();
    assert_eq!(log_prof.sample_count, 3);
    assert!(
        (log_prof.risk_score_median - 0.20).abs() < 1e-10,
        "median of [0.10, 0.20, 0.30] = 0.20"
    );

    let exec_prof = model
        .capability_profiles
        .iter()
        .find(|p| p.capability == "exec")
        .unwrap();
    assert_eq!(exec_prof.sample_count, 2);
    assert!(
        (exec_prof.risk_score_median - 0.85).abs() < 1e-10,
        "median of [0.80, 0.90] = 0.85"
    );
}

#[test]
fn baseline_mad_is_zero_for_constant_scores() {
    let entries = vec![
        make_entry(
            "ext.const",
            "log",
            "log",
            0.25,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.const",
            "log",
            "log",
            0.25,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.const",
            "log",
            "log",
            0.25,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.const").unwrap();

    let prof = &model.capability_profiles[0];
    assert!(
        (prof.risk_score_mad - 0.0).abs() < 1e-10,
        "MAD of constant data should be 0"
    );
}

#[test]
fn baseline_p5_and_p95_bracket_data() {
    let mut entries = Vec::new();
    for i in 0_u32..100 {
        let score = f64::from(i) / 100.0;
        entries.push(make_entry(
            "ext.quant",
            "log",
            "log",
            score,
            RuntimeRiskStateLabelValue::SafeFast,
            i64::from(i + 1) * 1000,
            &format!("c{i}"),
            None,
        ));
    }
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.quant").unwrap();

    let prof = &model.capability_profiles[0];
    assert!(prof.risk_score_p5 < prof.risk_score_median, "p5 < median");
    assert!(prof.risk_score_p95 > prof.risk_score_median, "p95 > median");
    assert!(prof.risk_score_p5 >= 0.0, "p5 >= 0");
    assert!(prof.risk_score_p95 <= 1.0, "p95 <= 1");
}

#[test]
fn baseline_error_rate_from_outcome_errors() {
    let entries = vec![
        make_entry(
            "ext.err",
            "exec",
            "spawn",
            0.80,
            RuntimeRiskStateLabelValue::Suspicious,
            1000,
            "c1",
            Some("denied"),
        ),
        make_entry(
            "ext.err",
            "exec",
            "spawn",
            0.82,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.err",
            "exec",
            "spawn",
            0.78,
            RuntimeRiskStateLabelValue::Suspicious,
            3000,
            "c3",
            Some("timeout"),
        ),
        make_entry(
            "ext.err",
            "exec",
            "spawn",
            0.75,
            RuntimeRiskStateLabelValue::Suspicious,
            4000,
            "c4",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.err").unwrap();

    let prof = &model.capability_profiles[0];
    assert!(
        (prof.error_rate_median - 0.5).abs() < 1e-10,
        "2/4 errors = 0.5 error rate"
    );
}

// ---------------------------------------------------------------------------
// Markov transition matrix
// ---------------------------------------------------------------------------

#[test]
fn baseline_markov_matrix_rows_sum_to_one() {
    let entries = vec![
        make_entry(
            "ext.mk",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.mk",
            "exec",
            "exec",
            0.80,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.mk",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.mk",
            "exec",
            "exec",
            0.95,
            RuntimeRiskStateLabelValue::Unsafe,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.mk",
            "log",
            "log",
            0.08,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.mk").unwrap();

    for (i, row) in model.transition_matrix.probabilities.iter().enumerate() {
        let sum: f64 = row.iter().copied().sum();
        assert!(
            (sum - 1.0).abs() < 1e-8,
            "row {i} should sum to 1.0, got {sum}"
        );
    }
}

#[test]
fn baseline_markov_stationary_distribution_sums_to_one() {
    let entries = vec![
        make_entry(
            "ext.stat",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.stat",
            "exec",
            "exec",
            0.80,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.stat",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.stat",
            "exec",
            "exec",
            0.95,
            RuntimeRiskStateLabelValue::Unsafe,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.stat",
            "log",
            "log",
            0.08,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
        make_entry(
            "ext.stat",
            "exec",
            "exec",
            0.85,
            RuntimeRiskStateLabelValue::Suspicious,
            6000,
            "c6",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.stat").unwrap();

    let sum: f64 = model
        .transition_matrix
        .stationary_distribution
        .iter()
        .copied()
        .sum();
    assert!(
        (sum - 1.0).abs() < 1e-8,
        "stationary distribution should sum to 1.0, got {sum}"
    );
}

#[test]
fn baseline_markov_captures_transition_counts() {
    // Sequence: SafeFast → Suspicious → Unsafe → SafeFast
    let entries = vec![
        make_entry(
            "ext.cnt",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.cnt",
            "exec",
            "exec",
            0.70,
            RuntimeRiskStateLabelValue::Suspicious,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.cnt",
            "exec",
            "exec",
            0.95,
            RuntimeRiskStateLabelValue::Unsafe,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.cnt",
            "log",
            "log",
            0.08,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.cnt").unwrap();

    assert_eq!(model.transition_matrix.total_transitions, 3);
    assert_eq!(
        model.transition_matrix.counts[0][1], 1,
        "SafeFast→Suspicious: 1"
    );
    assert_eq!(
        model.transition_matrix.counts[1][2], 1,
        "Suspicious→Unsafe: 1"
    );
    assert_eq!(
        model.transition_matrix.counts[2][0], 1,
        "Unsafe→SafeFast: 1"
    );
}

#[test]
fn baseline_markov_empty_transitions_uses_uniform_prior() {
    // Single entry means 0 transitions
    let entries = vec![make_entry(
        "ext.uni",
        "log",
        "log",
        0.10,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.uni").unwrap();

    assert_eq!(model.transition_matrix.total_transitions, 0);
    for row in &model.transition_matrix.probabilities {
        for prob in row {
            assert!(
                (prob - 1.0 / 3.0_f64).abs() < 1e-8,
                "with no data, uniform prior should give 1/3 per cell"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Sparse data handling
// ---------------------------------------------------------------------------

#[test]
fn baseline_single_entry_produces_valid_model() {
    let entries = vec![make_entry(
        "ext.sparse",
        "log",
        "log",
        0.10,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.sparse").unwrap();

    assert_eq!(model.source_entry_count, 1);
    assert_eq!(model.capability_profiles.len(), 1);
    assert_eq!(model.capability_profiles[0].sample_count, 1);
    assert!((model.capability_profiles[0].risk_score_median - 0.10).abs() < 1e-10);
    assert!((model.capability_profiles[0].risk_score_mad - 0.0).abs() < 1e-10);
}

#[test]
fn baseline_two_entries_different_capabilities() {
    let entries = vec![
        make_entry(
            "ext.two",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.two",
            "exec",
            "spawn",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.two").unwrap();

    assert_eq!(model.capability_profiles.len(), 2);
    assert_eq!(model.transition_matrix.total_transitions, 1);
}

// ---------------------------------------------------------------------------
// Error cases
// ---------------------------------------------------------------------------

#[test]
fn baseline_rejects_empty_ledger() {
    let artifact = RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1000,
        entry_count: 0,
        head_ledger_hash: None,
        tail_ledger_hash: None,
        data_hash: runtime_risk_ledger_data_hash(&[]),
        entries: Vec::new(),
    };
    let result = build_baseline_from_ledger(&artifact, "ext.any");
    assert!(result.is_err());
}

#[test]
fn baseline_rejects_nonexistent_extension() {
    let entries = vec![make_entry(
        "ext.exists",
        "log",
        "log",
        0.10,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_artifact(entries);
    let result = build_baseline_from_ledger(&artifact, "ext.missing");
    assert!(result.is_err());
}

#[test]
fn baseline_filters_by_extension_id() {
    let entries = vec![
        make_entry(
            "ext.a",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.b",
            "log",
            "log",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.a",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);

    let model_a = build_baseline_from_ledger(&artifact, "ext.a").unwrap();
    assert_eq!(model_a.source_entry_count, 2);

    let model_b = build_baseline_from_ledger(&artifact, "ext.b").unwrap();
    assert_eq!(model_b.source_entry_count, 1);
}

// ---------------------------------------------------------------------------
// Custom thresholds
// ---------------------------------------------------------------------------

#[test]
fn baseline_custom_thresholds_propagate() {
    let entries = vec![
        make_entry(
            "ext.thresh",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.thresh",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model =
        build_baseline_from_ledger_with_options(&artifact, "ext.thresh", 5.0, 2.0, 0.1).unwrap();

    assert!((model.anomaly_threshold_mads - 5.0).abs() < 1e-10);
    assert!((model.transition_divergence_threshold - 2.0).abs() < 1e-10);
}

#[test]
fn baseline_smoothing_prior_affects_transition_probabilities() {
    let entries = vec![
        make_entry(
            "ext.smooth",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.smooth",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);

    let small_prior =
        build_baseline_from_ledger_with_options(&artifact, "ext.smooth", 3.0, 0.5, 0.01).unwrap();
    let large_prior =
        build_baseline_from_ledger_with_options(&artifact, "ext.smooth", 3.0, 0.5, 10.0).unwrap();

    // With large prior, probabilities should be closer to uniform
    let sp = &small_prior.transition_matrix.probabilities;
    let lp = &large_prior.transition_matrix.probabilities;

    // SafeFast→SafeFast should be higher with small prior (less smoothing)
    assert!(
        sp[0][0] > lp[0][0],
        "small prior should give more weight to observed transitions"
    );
}

// ---------------------------------------------------------------------------
// Drift detection
// ---------------------------------------------------------------------------

#[test]
fn drift_not_detected_for_normal_observations() {
    let entries = vec![
        make_entry(
            "ext.norm",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.norm",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.norm",
            "log",
            "log",
            0.14,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.norm",
            "log",
            "log",
            0.11,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.norm",
            "log",
            "log",
            0.13,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.norm").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.norm",
        "log",
        0.12, // risk: near median
        0.0,  // error rate: 0
        0.1,  // burst 1s: normal
        0.05, // burst 10s: normal
        &[],  // no recent states
    );

    assert!(
        !report.drift_detected,
        "baseline-matching data should not trigger drift"
    );
    assert_eq!(
        report.anomalies,
        [] as [pi::extensions::BaselineDriftAnomaly; 0]
    );
}

#[test]
fn drift_detected_for_risk_score_outlier() {
    let entries = vec![
        make_entry(
            "ext.out",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.out",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.out",
            "log",
            "log",
            0.11,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.out",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.out",
            "log",
            "log",
            0.13,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.out").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.out",
        "log",
        0.95, // risk: extremely high vs baseline ~0.11 median
        0.0,
        0.1,
        0.05,
        &[],
    );

    assert!(
        report.drift_detected,
        "extreme risk score should trigger drift"
    );
    assert_ne!(
        report.anomalies,
        [] as [pi::extensions::BaselineDriftAnomaly; 0]
    );

    let risk_anomaly = report
        .anomalies
        .iter()
        .find(|a| a.metric == "risk_score")
        .unwrap();
    assert!(
        risk_anomaly.deviation_mads > 3.0,
        "deviation should exceed threshold"
    );
}

#[test]
fn drift_anomaly_has_human_readable_explanation() {
    let entries = vec![
        make_entry(
            "ext.explain",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.explain",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.explain",
            "log",
            "log",
            0.11,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.explain").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.explain",
        "log",
        0.90, // outlier
        0.0,
        0.1,
        0.05,
        &[],
    );

    assert!(report.drift_detected);
    let anomaly = &report.anomalies[0];
    assert!(
        anomaly.explanation.contains("MADs"),
        "explanation should mention MADs"
    );
    assert!(
        anomaly.explanation.contains("baseline median"),
        "explanation should mention baseline median"
    );
    assert!(
        anomaly.explanation.contains("risk_score"),
        "explanation should mention the metric"
    );
}

#[test]
fn drift_markov_transition_anomaly() {
    let entries = vec![
        make_entry(
            "ext.tr",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.tr",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.tr",
            "log",
            "log",
            0.11,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.tr",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    // Use very sensitive transition threshold
    let model =
        build_baseline_from_ledger_with_options(&artifact, "ext.tr", 3.0, 0.01, 1.0).unwrap();

    // Baseline: all SafeFast. Live: lots of Unsafe transitions → high KL divergence
    let report = detect_baseline_drift(
        &model,
        "ext.tr",
        "log",
        0.10,
        0.0,
        0.1,
        0.05,
        &[
            RuntimeRiskStateLabelValue::Unsafe,
            RuntimeRiskStateLabelValue::Unsafe,
            RuntimeRiskStateLabelValue::Unsafe,
            RuntimeRiskStateLabelValue::Unsafe,
        ],
    );

    assert!(
        report.transition_anomalous,
        "Unsafe-heavy transitions should diverge from SafeFast baseline"
    );
    assert!(report.transition_divergence > 0.01);
}

#[test]
fn drift_not_triggered_when_transitions_match_baseline() {
    let entries = vec![
        make_entry(
            "ext.match",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.match",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.match",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.match").unwrap();

    let report = detect_baseline_drift(
        &model,
        "ext.match",
        "log",
        0.11,
        0.0,
        0.1,
        0.05,
        &[
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskStateLabelValue::SafeFast,
        ],
    );

    assert!(
        !report.transition_anomalous,
        "matching transitions should not trigger anomaly"
    );
}

#[test]
fn drift_unknown_capability_returns_empty_anomalies() {
    let entries = vec![make_entry(
        "ext.unk",
        "log",
        "log",
        0.10,
        RuntimeRiskStateLabelValue::SafeFast,
        1000,
        "c1",
        None,
    )];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.unk").unwrap();

    // Query a capability not in the baseline
    let report = detect_baseline_drift(&model, "ext.unk", "exec", 0.95, 0.5, 1.0, 1.0, &[]);

    // No profile for "exec" → no metric anomalies (but also no crash)
    assert!(
        report.anomalies.is_empty(),
        "unknown capability should produce no metric anomalies"
    );
}

// ---------------------------------------------------------------------------
// Multi-extension isolation
// ---------------------------------------------------------------------------

#[test]
fn baseline_isolates_extensions_in_shared_ledger() {
    let entries = vec![
        make_entry(
            "ext.alpha",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.beta",
            "exec",
            "exec",
            0.90,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.alpha",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.beta",
            "exec",
            "exec",
            0.85,
            RuntimeRiskStateLabelValue::Suspicious,
            4000,
            "c4",
            None,
        ),
    ];
    let artifact = make_artifact(entries);

    let alpha = build_baseline_from_ledger(&artifact, "ext.alpha").unwrap();
    let beta = build_baseline_from_ledger(&artifact, "ext.beta").unwrap();

    assert_eq!(alpha.capability_profiles.len(), 1);
    assert_eq!(alpha.capability_profiles[0].capability, "log");
    assert_eq!(alpha.source_entry_count, 2);

    assert_eq!(beta.capability_profiles.len(), 1);
    assert_eq!(beta.capability_profiles[0].capability, "exec");
    assert_eq!(beta.source_entry_count, 2);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn baseline_handles_many_capabilities() {
    let capabilities = ["log", "exec", "http", "env", "events", "session", "ui"];
    let mut entries = Vec::new();
    for (i, cap) in capabilities.iter().enumerate() {
        let idx = u32::try_from(i).expect("capability index must fit u32");
        entries.push(make_entry(
            "ext.many",
            cap,
            cap,
            f64::from(idx).mul_add(0.1, 0.10),
            RuntimeRiskStateLabelValue::SafeFast,
            (i64::from(idx) + 1) * 1000,
            &format!("c{i}"),
            None,
        ));
    }
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.many").unwrap();

    assert_eq!(model.capability_profiles.len(), capabilities.len());
}

#[test]
fn baseline_handles_duplicate_risk_scores() {
    let entries = vec![
        make_entry(
            "ext.dup",
            "log",
            "log",
            0.50,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.dup",
            "log",
            "log",
            0.50,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.dup",
            "log",
            "log",
            0.50,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
        make_entry(
            "ext.dup",
            "log",
            "log",
            0.50,
            RuntimeRiskStateLabelValue::SafeFast,
            4000,
            "c4",
            None,
        ),
        make_entry(
            "ext.dup",
            "log",
            "log",
            0.50,
            RuntimeRiskStateLabelValue::SafeFast,
            5000,
            "c5",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.dup").unwrap();

    let prof = &model.capability_profiles[0];
    assert!((prof.risk_score_median - 0.50).abs() < 1e-10);
    assert!((prof.risk_score_mad - 0.0).abs() < 1e-10);
    assert!((prof.risk_score_p5 - 0.50).abs() < 1e-10);
    assert!((prof.risk_score_p95 - 0.50).abs() < 1e-10);
}

#[test]
fn baseline_extreme_risk_scores_bounded() {
    let entries = vec![
        make_entry(
            "ext.ext",
            "log",
            "log",
            0.0,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.ext",
            "log",
            "log",
            1.0,
            RuntimeRiskStateLabelValue::Unsafe,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.ext").unwrap();

    let prof = &model.capability_profiles[0];
    assert!(prof.risk_score_p5 >= 0.0);
    assert!(prof.risk_score_p95 <= 1.0);
}

#[test]
fn drift_report_fields_populated_correctly() {
    let entries = vec![
        make_entry(
            "ext.fields",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.fields",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.fields").unwrap();

    let report = detect_baseline_drift(&model, "ext.fields", "log", 0.11, 0.0, 0.1, 0.05, &[]);

    assert_eq!(report.extension_id, "ext.fields");
    assert_eq!(report.capability, "log");
    assert!(!report.drift_detected);
    assert!((report.transition_divergence - 0.0).abs() < 1e-10);
    assert!(!report.transition_anomalous);
}

#[test]
fn drift_report_json_roundtrip() {
    let entries = vec![
        make_entry(
            "ext.drt",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.drt",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
    ];
    let artifact = make_artifact(entries);
    let model = build_baseline_from_ledger(&artifact, "ext.drt").unwrap();

    let report = detect_baseline_drift(&model, "ext.drt", "log", 0.80, 0.0, 0.1, 0.05, &[]);

    let json = serde_json::to_string(&report).unwrap();
    let restored: BaselineDriftReport = serde_json::from_str(&json).unwrap();
    // Compare with epsilon for float fields since JSON roundtrip may lose ULP precision
    assert_eq!(report.extension_id, restored.extension_id);
    assert_eq!(report.capability, restored.capability);
    assert_eq!(report.drift_detected, restored.drift_detected);
    assert_eq!(report.anomalies.len(), restored.anomalies.len());
    for (a, b) in report.anomalies.iter().zip(&restored.anomalies) {
        assert_eq!(a.metric, b.metric);
        assert!((a.observed - b.observed).abs() < 1e-12);
        assert!((a.deviation_mads - b.deviation_mads).abs() < 1e-12);
        assert!((a.baseline_median - b.baseline_median).abs() < 1e-12);
        assert!((a.baseline_mad - b.baseline_mad).abs() < 1e-12);
    }
    assert!((report.transition_divergence - restored.transition_divergence).abs() < 1e-12);
    assert_eq!(report.transition_anomalous, restored.transition_anomalous);
}

// ---------------------------------------------------------------------------
// Ledger hash integrity for artifact construction
// ---------------------------------------------------------------------------

#[test]
fn artifact_hash_chain_is_valid() {
    let entries = vec![
        make_entry(
            "ext.hc",
            "log",
            "log",
            0.10,
            RuntimeRiskStateLabelValue::SafeFast,
            1000,
            "c1",
            None,
        ),
        make_entry(
            "ext.hc",
            "log",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            2000,
            "c2",
            None,
        ),
        make_entry(
            "ext.hc",
            "log",
            "log",
            0.12,
            RuntimeRiskStateLabelValue::SafeFast,
            3000,
            "c3",
            None,
        ),
    ];
    let artifact = make_artifact(entries);

    // First entry has no prev hash
    assert!(artifact.entries[0].prev_ledger_hash.is_none());
    // Second entry's prev hash is first entry's hash
    assert_eq!(
        artifact.entries[1].prev_ledger_hash.as_deref(),
        Some(artifact.entries[0].ledger_hash.as_str())
    );
    // Third entry's prev hash is second entry's hash
    assert_eq!(
        artifact.entries[2].prev_ledger_hash.as_deref(),
        Some(artifact.entries[1].ledger_hash.as_str())
    );
    // Head and tail hashes are correct
    assert_eq!(
        artifact.head_ledger_hash,
        Some(artifact.entries[0].ledger_hash.clone())
    );
    assert_eq!(
        artifact.tail_ledger_hash,
        Some(artifact.entries[2].ledger_hash.clone())
    );
}
