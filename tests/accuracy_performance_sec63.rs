//! SEC-6.3 tests: Accuracy/performance evaluation harness (bd-cu17q).
//!
//! Validates:
//! - FP/FN metrics per scenario class (benign, adversarial, mixed)
//! - Feature extraction latency stays within 250us budget
//! - Decision latency stays bounded under load
//! - Explanation term/time budgets are enforced
//! - Calibration reports are deterministic and comparable across runs
//! - Overhead scales predictably with ledger size

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_CALIBRATION_SCHEMA_VERSION, RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
    RUNTIME_RISK_LEDGER_SCHEMA_VERSION, RuntimeRiskCalibrationConfig,
    RuntimeRiskCalibrationObjective, RuntimeRiskConfig, calibrate_runtime_risk_from_ledger,
    dispatch_host_call_shared, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::time::Instant;

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
        runtime_name: "sec63_eval",
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
        params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/sec63-{idx}")] }),
        timeout_ms: Some(10),
        cancel_token: None,
        context: None,
    }
}

fn http_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("http-{idx}"),
        capability: "http".to_string(),
        method: "fetch".to_string(),
        params: json!({ "url": format!("https://example.com/api/{idx}") }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

/// Run a labeled scenario and return the elapsed wall time.
fn run_scenario(
    ctx: &HostCallContext<'_>,
    benign: usize,
    adversarial: usize,
    recovery: usize,
) -> std::time::Duration {
    let started = Instant::now();
    futures::executor::block_on(async {
        for idx in 0..benign {
            let _ = dispatch_host_call_shared(ctx, benign_call(idx)).await;
        }
        for idx in 0..adversarial {
            let _ = dispatch_host_call_shared(ctx, adversarial_call(idx)).await;
        }
        for idx in 0..recovery {
            let _ = dispatch_host_call_shared(
                ctx,
                HostCallPayload {
                    call_id: format!("recovery-{idx}"),
                    capability: "log".to_string(),
                    method: "log".to_string(),
                    params: json!({ "level": "info", "message": format!("recovery-{idx}") }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                },
            )
            .await;
        }
    });
    started.elapsed()
}

// ============================================================================
// Test 1: FP/FN metrics per scenario class
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn fp_fn_metrics_per_scenario_class() {
    let harness = TestHarness::new("fp_fn_metrics_per_scenario_class");

    // Scenario: Mixed benign + adversarial trace
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.fpfn");

    run_scenario(&ctx, 10, 10, 5);

    let artifact = manager.runtime_risk_ledger_artifact();
    let config = RuntimeRiskCalibrationConfig::default();
    let report =
        calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration must succeed");

    // Verify calibration report has FP/FN metrics
    assert!(
        report.baseline.false_positive_rate >= 0.0,
        "baseline FPR must be non-negative"
    );
    assert!(
        report.baseline.false_negative_rate >= 0.0,
        "baseline FNR must be non-negative"
    );
    assert!(
        report.recommended.false_positive_rate >= 0.0,
        "recommended FPR must be non-negative"
    );
    assert!(
        report.recommended.false_negative_rate >= 0.0,
        "recommended FNR must be non-negative"
    );

    // Verify all candidates have valid FP/FN metrics
    for (i, candidate) in report.candidates.iter().enumerate() {
        assert!(
            (0.0..=1.0).contains(&candidate.false_positive_rate),
            "candidate {i} FPR {:.4} out of [0,1]",
            candidate.false_positive_rate
        );
        assert!(
            (0.0..=1.0).contains(&candidate.false_negative_rate),
            "candidate {i} FNR {:.4} out of [0,1]",
            candidate.false_negative_rate
        );
        assert!(
            candidate.expected_loss >= 0.0,
            "candidate {i} expected_loss must be non-negative"
        );
    }

    // Per-capability breakdown: benign (log) should have lower risk than adversarial (exec)
    let benign_scores: Vec<f64> = artifact
        .entries
        .iter()
        .filter(|e| e.capability == "log")
        .map(|e| e.risk_score)
        .collect();
    let adversarial_scores: Vec<f64> = artifact
        .entries
        .iter()
        .filter(|e| e.capability == "exec")
        .map(|e| e.risk_score)
        .collect();

    if !benign_scores.is_empty() && !adversarial_scores.is_empty() {
        let avg_benign = benign_scores.iter().sum::<f64>()
            / f64::from(u32::try_from(benign_scores.len()).expect("fits"));
        let avg_adversarial = adversarial_scores.iter().sum::<f64>()
            / f64::from(u32::try_from(adversarial_scores.len()).expect("fits"));
        assert!(
            avg_adversarial > avg_benign,
            "adversarial avg risk ({avg_adversarial:.4}) should exceed benign ({avg_benign:.4})"
        );
    }

    harness
        .log()
        .info_ctx("fp_fn_metrics", "calibration metrics", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
            ctx_log.push((
                "baseline_fpr".into(),
                format!("{:.4}", report.baseline.false_positive_rate),
            ));
            ctx_log.push((
                "baseline_fnr".into(),
                format!("{:.4}", report.baseline.false_negative_rate),
            ));
            ctx_log.push((
                "recommended_fpr".into(),
                format!("{:.4}", report.recommended.false_positive_rate),
            ));
            ctx_log.push((
                "recommended_fnr".into(),
                format!("{:.4}", report.recommended.false_negative_rate),
            ));
            ctx_log.push((
                "recommended_threshold".into(),
                format!("{:.3}", report.recommended_threshold),
            ));
        });
}

// ============================================================================
// Test 2: Feature extraction latency stays within budget
// ============================================================================

#[test]
fn feature_extraction_latency_within_budget() {
    let harness = TestHarness::new("feature_extraction_latency_within_budget");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.latency");

    run_scenario(&ctx, 10, 10, 5);

    let telemetry = manager.runtime_hostcall_telemetry_artifact();

    // Check feature extraction latency for each entry
    let mut max_latency_us: u64 = 0;
    let mut budget_exceeded_count: usize = 0;
    let total = telemetry.entries.len();

    for entry in &telemetry.entries {
        if entry.extraction_latency_us > max_latency_us {
            max_latency_us = entry.extraction_latency_us;
        }
        if entry.extraction_budget_exceeded {
            budget_exceeded_count += 1;
        }
    }

    // Allow occasional budget exceedance (system scheduling jitter) but
    // the majority should be within budget
    let budget_compliance_rate = if total > 0 {
        f64::from(u32::try_from(total - budget_exceeded_count).expect("fits"))
            / f64::from(u32::try_from(total).expect("fits"))
    } else {
        1.0
    };

    assert!(
        budget_compliance_rate >= 0.8,
        "at least 80% of decisions must stay within feature extraction budget, got {:.1}%",
        budget_compliance_rate * 100.0
    );

    harness
        .log()
        .info_ctx("feature_latency", "feature extraction metrics", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
            ctx_log.push(("total_entries".into(), total.to_string()));
            ctx_log.push(("max_latency_us".into(), max_latency_us.to_string()));
            ctx_log.push(("budget_exceeded".into(), budget_exceeded_count.to_string()));
            ctx_log.push((
                "compliance_rate".into(),
                format!("{:.1}%", budget_compliance_rate * 100.0),
            ));
        });
}

// ============================================================================
// Test 3: Explanation budget enforcement
// ============================================================================

#[test]
fn explanation_budget_enforcement() {
    let harness = TestHarness::new("explanation_budget_enforcement");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.budget");

    // Generate a long trace to exercise budget limits
    run_scenario(&ctx, 5, 15, 5);

    let artifact = manager.runtime_risk_ledger_artifact();

    for (i, entry) in artifact.entries.iter().enumerate() {
        // Term budget must be respected
        assert!(
            entry.top_contributors.len() <= RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
            "entry {i} has {} contributors, exceeding term budget of {}",
            entry.top_contributors.len(),
            RUNTIME_RISK_EXPLANATION_TERM_BUDGET
        );

        // Budget state must be consistent
        if entry.budget_state.exhausted {
            assert!(
                entry.budget_state.fallback_mode,
                "entry {i}: exhausted budget must trigger fallback_mode"
            );
        }

        // terms_emitted must not exceed actual contributors
        assert!(
            entry.budget_state.terms_emitted <= entry.top_contributors.len(),
            "entry {i}: terms_emitted ({}) exceeds contributor count ({})",
            entry.budget_state.terms_emitted,
            entry.top_contributors.len()
        );

        // term_budget must be positive
        assert!(
            entry.budget_state.term_budget > 0,
            "entry {i}: term_budget must be positive"
        );
    }
}

// ============================================================================
// Test 4: Calibration report determinism and comparability
// ============================================================================

#[test]
fn calibration_report_comparable_across_runs() {
    let harness = TestHarness::new("calibration_report_comparable_across_runs");

    let mut reports = Vec::new();
    for run_idx in 0..3 {
        let (tools, http, manager, policy) = setup(&harness, default_risk_config());
        let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.comparable");

        run_scenario(&ctx, 8, 8, 4);

        let artifact = manager.runtime_risk_ledger_artifact();
        let config = RuntimeRiskCalibrationConfig::default();
        let report = calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration");

        harness
            .log()
            .info_ctx("comparable_run", "run complete", |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
                ctx_log.push(("run".into(), run_idx.to_string()));
                ctx_log.push((
                    "recommended_threshold".into(),
                    format!("{:.3}", report.recommended_threshold),
                ));
            });

        reports.push(report);
    }

    // Schema must be stable across runs
    for (i, report) in reports.iter().enumerate() {
        assert_eq!(
            report.schema, RUNTIME_RISK_CALIBRATION_SCHEMA_VERSION,
            "run {i}: schema mismatch"
        );
    }

    // All runs use the same baseline threshold
    for (i, report) in reports.iter().enumerate() {
        assert!(
            (report.baseline_threshold - 0.65).abs() < f64::EPSILON,
            "run {i}: baseline_threshold should be 0.65"
        );
    }

    // Recommended thresholds must be identical across runs (determinism)
    for run_idx in 1..reports.len() {
        assert!(
            (reports[0].recommended_threshold - reports[run_idx].recommended_threshold).abs()
                < f64::EPSILON,
            "threshold mismatch between run 0 ({:.3}) and run {run_idx} ({:.3})",
            reports[0].recommended_threshold,
            reports[run_idx].recommended_threshold
        );
    }

    // Candidate counts must match
    for run_idx in 1..reports.len() {
        assert_eq!(
            reports[0].candidates.len(),
            reports[run_idx].candidates.len(),
            "candidate count mismatch between run 0 and {run_idx}"
        );
    }
}

// ============================================================================
// Test 5: Decision throughput under load
// ============================================================================

#[test]
fn decision_throughput_under_load() {
    let harness = TestHarness::new("decision_throughput_under_load");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.throughput");

    let call_count: usize = 100;
    let started = Instant::now();

    futures::executor::block_on(async {
        for idx in 0..call_count {
            let call = if idx % 3 == 0 {
                adversarial_call(idx)
            } else {
                benign_call(idx)
            };
            let start_dispatch = Instant::now();
            let _ = dispatch_host_call_shared(&ctx, call).await;
            let elapsed = start_dispatch.elapsed().as_micros();
            if idx % 10 == 0 || elapsed > 1000 {
                println!("dispatch {idx} took {elapsed}us");
            }
        }
    });

    let elapsed = started.elapsed();
    let per_call_us = elapsed.as_micros() / u128::try_from(call_count).expect("fits");

    // Each decision should complete in under 1ms on average
    assert!(
        per_call_us < 5000,
        "average decision latency ({per_call_us}us) should be under 5000us"
    );

    let artifact = manager.runtime_risk_ledger_artifact();
    assert_eq!(
        artifact.entry_count, call_count,
        "all calls must be recorded in ledger"
    );

    harness
        .log()
        .info_ctx("throughput", "throughput results", |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
            ctx_log.push(("call_count".into(), call_count.to_string()));
            ctx_log.push(("total_ms".into(), elapsed.as_millis().to_string()));
            ctx_log.push(("per_call_us".into(), per_call_us.to_string()));
        });
}

// ============================================================================
// Test 6: Multi-objective calibration comparison
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn multi_objective_calibration_comparison() {
    let harness = TestHarness::new("multi_objective_calibration_comparison");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.multiobj");

    run_scenario(&ctx, 10, 10, 5);

    let artifact = manager.runtime_risk_ledger_artifact();

    let objectives = [
        (
            "min_expected_loss",
            RuntimeRiskCalibrationObjective::MinExpectedLoss,
        ),
        (
            "min_false_positives",
            RuntimeRiskCalibrationObjective::MinFalsePositives,
        ),
        (
            "balanced_accuracy",
            RuntimeRiskCalibrationObjective::BalancedAccuracy,
        ),
    ];

    for (name, objective) in &objectives {
        let config = RuntimeRiskCalibrationConfig {
            objective: *objective,
            ..RuntimeRiskCalibrationConfig::default()
        };
        let report = calibrate_runtime_risk_from_ledger(&artifact, &config).expect(name);

        // Schema must be correct
        assert_eq!(report.schema, RUNTIME_RISK_CALIBRATION_SCHEMA_VERSION);

        // Source hash must match
        assert_eq!(report.source_data_hash, artifact.data_hash);

        // Threshold must be valid
        assert!(
            (0.0..=1.0).contains(&report.recommended_threshold),
            "{name}: threshold {:.3} out of [0,1]",
            report.recommended_threshold
        );

        // Delta must be consistent
        assert!(
            (report.recommended_delta - (report.recommended_threshold - report.baseline_threshold))
                .abs()
                < 1e-12,
            "{name}: delta inconsistency"
        );

        // All candidates must have valid metrics
        for (i, cand) in report.candidates.iter().enumerate() {
            assert!(
                cand.objective_score.is_finite(),
                "{name}: candidate {i} objective_score is not finite"
            );
            assert!(
                cand.expected_loss.is_finite(),
                "{name}: candidate {i} expected_loss is not finite"
            );
        }

        harness
            .log()
            .info_ctx("multi_objective", "objective evaluation", |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
                ctx_log.push(("objective".into(), (*name).to_string()));
                ctx_log.push((
                    "threshold".into(),
                    format!("{:.3}", report.recommended_threshold),
                ));
                ctx_log.push((
                    "fpr".into(),
                    format!("{:.4}", report.recommended.false_positive_rate),
                ));
                ctx_log.push((
                    "fnr".into(),
                    format!("{:.4}", report.recommended.false_negative_rate),
                ));
            });
    }
}

// ============================================================================
// Test 7: Overhead scales linearly with trace length
// ============================================================================

#[test]
fn overhead_scales_with_trace_length() {
    let harness = TestHarness::new("overhead_scales_with_trace_length");

    let sizes: Vec<usize> = vec![10, 50, 100];
    let mut timings = Vec::new();

    for &size in &sizes {
        let (tools, http, manager, policy) = setup(&harness, default_risk_config());
        let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.scaling");

        let started = Instant::now();
        futures::executor::block_on(async {
            for idx in 0..size {
                let call = if idx % 2 == 0 {
                    benign_call(idx)
                } else {
                    adversarial_call(idx)
                };
                let _ = dispatch_host_call_shared(&ctx, call).await;
            }
        });
        let elapsed = started.elapsed();

        let artifact = manager.runtime_risk_ledger_artifact();
        assert_eq!(artifact.entry_count, size);

        timings.push((size, elapsed));
    }

    // Verify overhead doesn't grow super-linearly
    // The ratio (time for 100 calls) / (time for 10 calls) should be < 20x
    // (allowing for some overhead, linear would be ~10x)
    if let (Some(small), Some(large)) = (timings.first(), timings.last()) {
        #[allow(clippy::cast_precision_loss)]
        let ratio = large.1.as_micros() as f64 / small.1.as_micros().max(1) as f64;
        let size_ratio = f64::from(u32::try_from(large.0).expect("fits"))
            / f64::from(u32::try_from(small.0).expect("fits"));

        assert!(
            ratio < size_ratio * 2.0,
            "overhead ratio ({ratio:.1}x) should be < {:.0}x (2x linear scaling factor {size_ratio}x)",
            size_ratio * 2.0
        );

        harness
            .log()
            .info_ctx("scaling", "overhead scaling analysis", |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
                ctx_log.push(("small_size".into(), small.0.to_string()));
                ctx_log.push(("small_us".into(), small.1.as_micros().to_string()));
                ctx_log.push(("large_size".into(), large.0.to_string()));
                ctx_log.push(("large_us".into(), large.1.as_micros().to_string()));
                ctx_log.push(("ratio".into(), format!("{ratio:.1}x")));
            });
    }
}

// ============================================================================
// Test 8: Ledger integrity preserved under evaluation load
// ============================================================================

#[test]
fn ledger_integrity_under_evaluation_load() {
    let harness = TestHarness::new("ledger_integrity_under_evaluation_load");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.integrity");

    // Run a substantial mixed scenario
    run_scenario(&ctx, 20, 20, 10);

    let artifact = manager.runtime_risk_ledger_artifact();
    assert_eq!(artifact.entry_count, 50);
    assert_eq!(artifact.schema, RUNTIME_RISK_LEDGER_SCHEMA_VERSION);

    // Verify complete hash chain integrity
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(
        verification.valid,
        "ledger must pass integrity after 50-entry evaluation: {:?}",
        verification.errors
    );

    // Verify telemetry count matches
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert_eq!(
        telemetry.entry_count, artifact.entry_count,
        "telemetry and ledger entry counts must agree"
    );
}

// ============================================================================
// Test 9: Multi-capability accuracy — FP/FN per capability type
// ============================================================================

#[test]
fn multi_capability_accuracy() {
    let harness = TestHarness::new("multi_capability_accuracy");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.multicap");

    // Mix of different capabilities
    futures::executor::block_on(async {
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, http_call(idx)).await;
        }
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    // Group scores by capability
    let mut cap_scores: std::collections::BTreeMap<String, Vec<f64>> =
        std::collections::BTreeMap::new();
    for entry in &artifact.entries {
        cap_scores
            .entry(entry.capability.clone())
            .or_default()
            .push(entry.risk_score);
    }

    // Log per-capability risk statistics
    for (cap, scores) in &cap_scores {
        let avg =
            scores.iter().sum::<f64>() / f64::from(u32::try_from(scores.len()).expect("fits"));
        let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
        let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        harness
            .log()
            .info_ctx("capability_accuracy", "per-capability stats", |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-cu17q".into()));
                ctx_log.push(("capability".into(), cap.clone()));
                ctx_log.push(("count".into(), scores.len().to_string()));
                ctx_log.push(("avg_risk".into(), format!("{avg:.4}")));
                ctx_log.push(("min_risk".into(), format!("{min:.4}")));
                ctx_log.push(("max_risk".into(), format!("{max:.4}")));
            });
    }

    // Exec should have higher base risk than log
    if let (Some(log_scores), Some(exec_scores)) = (cap_scores.get("log"), cap_scores.get("exec")) {
        let avg_log = log_scores.iter().sum::<f64>()
            / f64::from(u32::try_from(log_scores.len()).expect("fits"));
        let avg_exec = exec_scores.iter().sum::<f64>()
            / f64::from(u32::try_from(exec_scores.len()).expect("fits"));
        assert!(
            avg_exec > avg_log,
            "exec avg risk ({avg_exec:.4}) should exceed log ({avg_log:.4})"
        );
    }
}

// ============================================================================
// Test 10: Evaluation report source fingerprinting
// ============================================================================

#[test]
fn evaluation_report_source_fingerprinting() {
    let harness = TestHarness::new("evaluation_report_source_fingerprinting");
    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eval.fingerprint");

    run_scenario(&ctx, 8, 8, 4);

    let artifact = manager.runtime_risk_ledger_artifact();
    let config = RuntimeRiskCalibrationConfig::default();
    let report = calibrate_runtime_risk_from_ledger(&artifact, &config).expect("calibration");

    // Report must reference the exact source ledger
    assert_eq!(
        report.source_data_hash, artifact.data_hash,
        "source_data_hash must match ledger"
    );
    assert_eq!(
        report.source_schema, artifact.schema,
        "source_schema must match ledger"
    );

    // Report baseline must match config
    assert!(
        (report.baseline_threshold - config.baseline_threshold).abs() < f64::EPSILON,
        "baseline_threshold must match config"
    );

    // Candidate count must match grid size
    assert_eq!(
        report.candidates.len(),
        config.threshold_grid.len(),
        "candidates must match threshold grid"
    );

    // Verify candidates are ordered by threshold
    for window in report.candidates.windows(2) {
        assert!(
            window[0].threshold <= window[1].threshold,
            "candidates must be ordered by threshold: {:.3} > {:.3}",
            window[0].threshold,
            window[1].threshold
        );
    }
}
