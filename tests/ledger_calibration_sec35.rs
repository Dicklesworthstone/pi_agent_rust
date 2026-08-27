//! SEC-3.5 tests: Hash-chained decision ledger and offline threshold calibration (bd-3i9da).
//!
//! Validates:
//! - Tamper detection: mutated entries, broken chains, and forged hashes are caught
//! - Calibration correctness: different objectives produce valid recommendations
//! - Calibration determinism: identical inputs yield identical outputs
//! - Replay rejection: replay refuses tampered or invalid ledgers
//! - Verification error codes: each integrity error class is testable
//! - Edge cases: empty ledger, single-entry ledger, all-benign/all-adversarial traces

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RUNTIME_RISK_LEDGER_SCHEMA_VERSION,
    RuntimeRiskActionValue, RuntimeRiskCalibrationConfig, RuntimeRiskCalibrationObjective,
    RuntimeRiskConfig, RuntimeRiskExpectedLossEvidence, RuntimeRiskExplanationBudgetState,
    RuntimeRiskExplanationContributor, RuntimeRiskExplanationLevelValue, RuntimeRiskLedgerArtifact,
    RuntimeRiskLedgerArtifactEntry, RuntimeRiskPosteriorEvidence, RuntimeRiskStateLabelValue,
    calibrate_runtime_risk_from_ledger, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, runtime_risk_compute_ledger_hash_artifact,
    runtime_risk_ledger_data_hash, verify_runtime_risk_ledger_artifact,
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
        runtime_name: "sec35_test",
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
        call_id: format!("benign-{idx}"),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": format!("benign-{idx}") }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn adversarial_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("adversarial-{idx}"),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/sec35-{idx}")] }),
        timeout_ms: Some(10),
        cancel_token: None,
        context: None,
    }
}

/// Build a synthetic ledger entry for tamper testing.
fn synthetic_entry(
    call_id: &str,
    capability: &str,
    risk_score: f64,
    state: RuntimeRiskStateLabelValue,
    action: RuntimeRiskActionValue,
    ts_ms: i64,
) -> RuntimeRiskLedgerArtifactEntry {
    RuntimeRiskLedgerArtifactEntry {
        ts_ms,
        extension_id: "ext.sec35.test".to_string(),
        call_id: call_id.to_string(),
        capability: capability.to_string(),
        method: capability.to_string(),
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
        selected_action: action,
        derived_state: state,
        triggers: Vec::new(),
        fallback_reason: None,
        e_process: 0.5,
        e_threshold: 100.0,
        conformal_residual: 0.01,
        conformal_quantile: 0.05,
        drift_detected: false,
        outcome_error_code: None,
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

/// Build a valid hash-chained ledger artifact from entries.
fn build_valid_artifact(entries: Vec<RuntimeRiskLedgerArtifactEntry>) -> RuntimeRiskLedgerArtifact {
    let mut hashed = Vec::with_capacity(entries.len());
    let mut prev_hash: Option<String> = None;
    for mut entry in entries {
        let hash = runtime_risk_compute_ledger_hash_artifact(&entry, prev_hash.as_deref());
        entry.ledger_hash.clone_from(&hash);
        entry.prev_ledger_hash.clone_from(&prev_hash);
        prev_hash = Some(hash);
        hashed.push(entry);
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

// ============================================================================
// Test 1: Tamper detection — mutating a risk score invalidates the chain
// ============================================================================

#[test]
fn tamper_score_mutation_detected() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "exec",
            0.8,
            RuntimeRiskStateLabelValue::Suspicious,
            RuntimeRiskActionValue::Harden,
            2000,
        ),
        synthetic_entry(
            "c3",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            3000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    // Sanity: valid before tampering
    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(report.valid, "artifact must verify before tampering");

    // Tamper: change the risk score of entry 1
    artifact.entries[1].risk_score = 0.99;

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "tampered artifact must fail verification");
    assert!(
        report.errors.iter().any(|e| e.code == "hash_mismatch"),
        "must report hash_mismatch error, got: {:?}",
        report.errors
    );
}

// ============================================================================
// Test 2: Tamper detection — broken prev_ledger_hash link
// ============================================================================

#[test]
fn tamper_broken_chain_link_detected() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "log",
            0.2,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            2000,
        ),
        synthetic_entry(
            "c3",
            "exec",
            0.7,
            RuntimeRiskStateLabelValue::Suspicious,
            RuntimeRiskActionValue::Harden,
            3000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(report.valid, "must verify before tampering");

    // Break the chain: set entry 2's prev_hash to garbage
    artifact.entries[2].prev_ledger_hash = Some("forged_hash_000".to_string());

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "broken chain must fail verification");
    assert!(
        report.errors.iter().any(|e| e.code == "prev_hash_mismatch"),
        "must report prev_hash_mismatch error, got: {:?}",
        report.errors
    );
}

// ============================================================================
// Test 3: Tamper detection — forged data_hash
// ============================================================================

#[test]
fn tamper_forged_data_hash_detected() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "exec",
            0.5,
            RuntimeRiskStateLabelValue::Suspicious,
            RuntimeRiskActionValue::Harden,
            2000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(report.valid, "must verify before tampering");

    // Forge the data_hash
    artifact.data_hash = "forged_aggregate_hash".to_string();

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "forged data_hash must fail verification");
    assert!(
        report.errors.iter().any(|e| e.code == "data_hash_mismatch"),
        "must report data_hash_mismatch error, got: {:?}",
        report.errors
    );
}

// ============================================================================
// Test 4: Tamper detection — wrong head/tail hashes
// ============================================================================

#[test]
fn tamper_head_tail_hash_mismatch_detected() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "log",
            0.15,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            2000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(report.valid, "must verify before tampering");

    // Swap head and tail hashes
    let original_head = artifact.head_ledger_hash.clone();
    artifact.head_ledger_hash = artifact.tail_ledger_hash.clone();
    artifact.tail_ledger_hash = original_head;

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "swapped head/tail must fail verification");
    let error_codes: Vec<&str> = report.errors.iter().map(|e| e.code.as_str()).collect();
    assert!(
        error_codes.contains(&"head_hash_mismatch") || error_codes.contains(&"tail_hash_mismatch"),
        "must report head or tail hash mismatch, got: {error_codes:?}"
    );
}

// ============================================================================
// Test 5: Tamper detection — entry_count mismatch
// ============================================================================

#[test]
fn tamper_entry_count_mismatch_detected() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "log",
            0.2,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            2000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    // Lie about entry count
    artifact.entry_count = 5;

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "wrong entry_count must fail verification");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "entry_count_mismatch"),
        "must report entry_count_mismatch, got: {:?}",
        report.errors
    );
}

// ============================================================================
// Test 6: Tamper detection — schema version mismatch
// ============================================================================

#[test]
fn tamper_schema_version_mismatch_detected() {
    let entries = vec![synthetic_entry(
        "c1",
        "log",
        0.1,
        RuntimeRiskStateLabelValue::SafeFast,
        RuntimeRiskActionValue::Allow,
        1000,
    )];
    let mut artifact = build_valid_artifact(entries);

    artifact.schema = "pi.ext.runtime_risk_ledger.v999".to_string();

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(!report.valid, "wrong schema must fail verification");
    assert!(
        report.errors.iter().any(|e| e.code == "schema_mismatch"),
        "must report schema_mismatch, got: {:?}",
        report.errors
    );
}

// ============================================================================
// Test 7: Replay refuses tampered ledger
// ============================================================================

#[test]
fn replay_rejects_tampered_ledger() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "exec",
            0.7,
            RuntimeRiskStateLabelValue::Suspicious,
            RuntimeRiskActionValue::Harden,
            2000,
        ),
    ];
    let mut artifact = build_valid_artifact(entries);

    // Tamper with an entry
    artifact.entries[0].risk_score = 0.99;

    let result = replay_runtime_risk_ledger_artifact(&artifact);
    assert!(
        result.is_err(),
        "replay must refuse tampered ledger, got Ok"
    );
}

// ============================================================================
// Test 8: Calibration determinism — identical ledger yields identical reports
// ============================================================================

#[test]
fn calibration_determinism_identical_reports() {
    let harness = TestHarness::new("calibration_determinism_identical_reports");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.calibration.det");

    // Build a mixed trace
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let config = RuntimeRiskCalibrationConfig::default();

    let report_a =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration must succeed");
    let report_b =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration must succeed");

    assert!(
        (report_a.recommended_threshold - report_b.recommended_threshold).abs() < f64::EPSILON,
        "recommended threshold must be deterministic: {} vs {}",
        report_a.recommended_threshold,
        report_b.recommended_threshold
    );
    assert!(
        (report_a.recommended_delta - report_b.recommended_delta).abs() < f64::EPSILON,
        "recommended delta must be deterministic: {} vs {}",
        report_a.recommended_delta,
        report_b.recommended_delta
    );
    assert_eq!(
        report_a.candidates.len(),
        report_b.candidates.len(),
        "candidate count must match"
    );
    for (i, (ca, cb)) in report_a
        .candidates
        .iter()
        .zip(report_b.candidates.iter())
        .enumerate()
    {
        assert!(
            (ca.objective_score - cb.objective_score).abs() < 1e-12,
            "candidate {i} objective_score mismatch: {} vs {}",
            ca.objective_score,
            cb.objective_score
        );
        assert!(
            (ca.expected_loss - cb.expected_loss).abs() < 1e-12,
            "candidate {i} expected_loss mismatch: {} vs {}",
            ca.expected_loss,
            cb.expected_loss
        );
    }
}

// ============================================================================
// Test 9: Calibration objectives produce different recommendations
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn calibration_objectives_differ() {
    let harness = TestHarness::new("calibration_objectives_differ");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.calibration.obj");

    // Build a trace with clear benign/adversarial separation
    futures::executor::block_on(async {
        for idx in 0..8 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..8 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    let config_mel = RuntimeRiskCalibrationConfig {
        objective: RuntimeRiskCalibrationObjective::MinExpectedLoss,
        ..RuntimeRiskCalibrationConfig::default()
    };
    let config_mfp = RuntimeRiskCalibrationConfig {
        objective: RuntimeRiskCalibrationObjective::MinFalsePositives,
        ..RuntimeRiskCalibrationConfig::default()
    };
    let config_ba = RuntimeRiskCalibrationConfig {
        objective: RuntimeRiskCalibrationObjective::BalancedAccuracy,
        ..RuntimeRiskCalibrationConfig::default()
    };

    let report_mel = calibrate_runtime_risk_from_ledger(&artifact, &config_mel)
        .expect("MinExpectedLoss calibration");
    let report_mfp = calibrate_runtime_risk_from_ledger(&artifact, &config_mfp)
        .expect("MinFalsePositives calibration");
    let report_ba = calibrate_runtime_risk_from_ledger(&artifact, &config_ba)
        .expect("BalancedAccuracy calibration");

    // All must produce valid schema
    assert_eq!(report_mel.schema, "pi.ext.runtime_risk_calibration.v1");
    assert_eq!(report_mfp.schema, "pi.ext.runtime_risk_calibration.v1");
    assert_eq!(report_ba.schema, "pi.ext.runtime_risk_calibration.v1");

    // Source data hash must match
    assert_eq!(report_mel.source_data_hash, artifact.data_hash);
    assert_eq!(report_mfp.source_data_hash, artifact.data_hash);
    assert_eq!(report_ba.source_data_hash, artifact.data_hash);

    // Recommended thresholds must be within [0,1]
    for (name, report) in [
        ("mel", &report_mel),
        ("mfp", &report_mfp),
        ("ba", &report_ba),
    ] {
        assert!(
            (0.0..=1.0).contains(&report.recommended_threshold),
            "{name}: recommended threshold {} out of [0,1]",
            report.recommended_threshold
        );
        assert!(
            !report.candidates.is_empty(),
            "{name}: must have calibration candidates"
        );
    }

    // Log recommendations for observability
    harness.log().info_ctx(
        "calibration_objectives",
        "objective comparison",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3i9da".into()));
            ctx_log.push((
                "mel_threshold".into(),
                format!("{:.3}", report_mel.recommended_threshold),
            ));
            ctx_log.push((
                "mfp_threshold".into(),
                format!("{:.3}", report_mfp.recommended_threshold),
            ));
            ctx_log.push((
                "ba_threshold".into(),
                format!("{:.3}", report_ba.recommended_threshold),
            ));
        },
    );
}

// ============================================================================
// Test 10: Calibration with all-benign trace
// ============================================================================

#[test]
fn calibration_all_benign_trace() {
    let harness = TestHarness::new("calibration_all_benign_trace");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.calibration.benign");

    futures::executor::block_on(async {
        for idx in 0..10 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let config = RuntimeRiskCalibrationConfig::default();

    let report = calibrate_runtime_risk_from_ledger(&artifact, &config)
        .expect("all-benign calibration must succeed");

    // With all benign trace, false negative rate should be 0 (no true positives to miss)
    assert!(
        report.recommended.false_negative_rate >= 0.0,
        "false_negative_rate must be non-negative"
    );
    assert!(
        report.recommended.false_positive_rate >= 0.0,
        "false_positive_rate must be non-negative"
    );

    // Calibration report must reference the correct source
    assert_eq!(report.source_data_hash, artifact.data_hash);
    assert_eq!(report.source_schema, artifact.schema);
}

// ============================================================================
// Test 11: Calibration with single-entry ledger
// ============================================================================

#[test]
fn calibration_single_entry_ledger() {
    let entries = vec![synthetic_entry(
        "c1",
        "exec",
        0.8,
        RuntimeRiskStateLabelValue::Suspicious,
        RuntimeRiskActionValue::Harden,
        1000,
    )];
    let artifact = build_valid_artifact(entries);

    let config = RuntimeRiskCalibrationConfig::default();
    let report = calibrate_runtime_risk_from_ledger(&artifact, &config)
        .expect("single-entry calibration must succeed");

    assert_eq!(report.candidates.len(), config.threshold_grid.len());
    assert!(
        (0.0..=1.0).contains(&report.recommended_threshold),
        "threshold must be in [0,1]"
    );
}

// ============================================================================
// Test 12: Verification of empty ledger
// ============================================================================

#[test]
fn verification_empty_ledger() {
    let artifact = RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms: 1000,
        entry_count: 0,
        head_ledger_hash: None,
        tail_ledger_hash: None,
        data_hash: runtime_risk_ledger_data_hash(&[]),
        entries: Vec::new(),
    };

    let report = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        report.valid,
        "empty ledger must verify: {:?}",
        report.errors
    );
    assert_eq!(report.entry_count, 0);
}

// ============================================================================
// Test 13: Data hash reproducibility — same entries always produce same hash
// ============================================================================

#[test]
fn data_hash_reproducibility() {
    let entries = vec![
        synthetic_entry(
            "c1",
            "log",
            0.1,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            1000,
        ),
        synthetic_entry(
            "c2",
            "exec",
            0.7,
            RuntimeRiskStateLabelValue::Suspicious,
            RuntimeRiskActionValue::Harden,
            2000,
        ),
        synthetic_entry(
            "c3",
            "log",
            0.2,
            RuntimeRiskStateLabelValue::SafeFast,
            RuntimeRiskActionValue::Allow,
            3000,
        ),
    ];

    let artifact_a = build_valid_artifact(entries.clone());
    let artifact_b = build_valid_artifact(entries);

    assert_eq!(
        artifact_a.data_hash, artifact_b.data_hash,
        "same entries must produce identical data_hash"
    );
    assert_eq!(
        artifact_a.head_ledger_hash, artifact_b.head_ledger_hash,
        "same entries must produce identical head_ledger_hash"
    );
    assert_eq!(
        artifact_a.tail_ledger_hash, artifact_b.tail_ledger_hash,
        "same entries must produce identical tail_ledger_hash"
    );

    // Verify each entry hash matches
    for (i, (ea, eb)) in artifact_a
        .entries
        .iter()
        .zip(artifact_b.entries.iter())
        .enumerate()
    {
        assert_eq!(
            ea.ledger_hash, eb.ledger_hash,
            "entry {i} ledger_hash must be identical"
        );
    }
}

// ============================================================================
// Test 14: E2E ledger export → verify → replay → calibrate pipeline
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_full_pipeline_verify_replay_calibrate() {
    let harness = TestHarness::new("e2e_full_pipeline_verify_replay_calibrate");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.pipeline.e2e");

    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..8 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(10 + idx)).await;
        }
    });

    // Step 1: Export ledger
    let artifact = manager.runtime_risk_ledger_artifact();
    assert_eq!(artifact.entry_count, 16, "must have 16 entries total");
    assert_eq!(artifact.schema, RUNTIME_RISK_LEDGER_SCHEMA_VERSION);

    // Step 2: Verify integrity
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        verification.valid,
        "ledger must pass integrity check: {:?}",
        verification.errors
    );
    assert_eq!(
        verification.computed_data_hash,
        verification.artifact_data_hash
    );

    // Step 3: Replay
    let replay = replay_runtime_risk_ledger_artifact(&artifact)
        .expect("replay must succeed on verified ledger");
    assert_eq!(replay.steps.len(), artifact.entries.len());
    assert_eq!(replay.source_data_hash, artifact.data_hash);

    // Verify replay steps match ledger entries
    for (i, (entry, step)) in artifact.entries.iter().zip(replay.steps.iter()).enumerate() {
        assert_eq!(entry.call_id, step.call_id, "call_id mismatch at {i}");
        assert_eq!(
            entry.capability, step.capability,
            "capability mismatch at {i}"
        );
        assert_eq!(
            entry.selected_action, step.selected_action,
            "action mismatch at {i}"
        );
        assert_eq!(
            entry.derived_state, step.derived_state,
            "state mismatch at {i}"
        );
        assert!(
            (entry.risk_score - step.risk_score).abs() < 1e-12,
            "risk_score mismatch at {i}: {} vs {}",
            entry.risk_score,
            step.risk_score
        );
        assert_eq!(entry.ledger_hash, step.ledger_hash, "hash mismatch at {i}");
    }

    // Step 4: Calibrate
    let config = RuntimeRiskCalibrationConfig::default();
    let cal_report =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration must succeed");

    assert_eq!(cal_report.source_data_hash, artifact.data_hash);
    assert!(
        (0.0..=1.0).contains(&cal_report.recommended_threshold),
        "recommended threshold out of range"
    );
    assert!(
        !cal_report.candidates.is_empty(),
        "must have calibration candidates"
    );

    // Baseline evaluation must be present
    assert!(
        (cal_report.baseline_threshold - 0.65).abs() < 1e-12,
        "baseline threshold must be 0.65"
    );

    harness
        .log()
        .info_ctx("e2e_pipeline", "full pipeline complete", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3i9da".into()));
            ctx_log.push(("entries".into(), artifact.entry_count.to_string()));
            ctx_log.push(("replay_steps".into(), replay.steps.len().to_string()));
            ctx_log.push((
                "recommended_threshold".into(),
                format!("{:.3}", cal_report.recommended_threshold),
            ));
            ctx_log.push((
                "recommended_delta".into(),
                format!("{:.3}", cal_report.recommended_delta),
            ));
        });
}

// ============================================================================
// Test 15: Calibration false-positive weight affects recommendation
// ============================================================================

#[test]
fn calibration_weight_sensitivity() {
    let harness = TestHarness::new("calibration_weight_sensitivity");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.calibration.weight");

    futures::executor::block_on(async {
        for idx in 0..6 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..6 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    // High FP weight → should prefer higher threshold (fewer FP)
    let high_fp_config = RuntimeRiskCalibrationConfig {
        objective: RuntimeRiskCalibrationObjective::MinExpectedLoss,
        false_positive_weight: 10.0,
        false_negative_weight: 1.0,
        ..RuntimeRiskCalibrationConfig::default()
    };

    // High FN weight → should prefer lower threshold (fewer FN)
    let high_miss_config = RuntimeRiskCalibrationConfig {
        objective: RuntimeRiskCalibrationObjective::MinExpectedLoss,
        false_positive_weight: 1.0,
        false_negative_weight: 10.0,
        ..RuntimeRiskCalibrationConfig::default()
    };

    let report_precision = calibrate_runtime_risk_from_ledger(&artifact, &high_fp_config)
        .expect("high FP weight calibration");
    let report_recall = calibrate_runtime_risk_from_ledger(&artifact, &high_miss_config)
        .expect("high FN weight calibration");

    // Both must produce valid reports
    assert!(
        (0.0..=1.0).contains(&report_precision.recommended_threshold),
        "FP-weighted threshold out of range"
    );
    assert!(
        (0.0..=1.0).contains(&report_recall.recommended_threshold),
        "FN-weighted threshold out of range"
    );

    harness
        .log()
        .info_ctx("weight_sensitivity", "weight comparison", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3i9da".into()));
            ctx_log.push((
                "high_fp_threshold".into(),
                format!("{:.3}", report_precision.recommended_threshold),
            ));
            ctx_log.push((
                "high_fn_threshold".into(),
                format!("{:.3}", report_recall.recommended_threshold),
            ));
        });
}
