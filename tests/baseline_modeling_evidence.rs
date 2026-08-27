//! E2E evidence tests for baseline modeling (bd-153pv).
//!
//! Validates:
//! - Deterministic baseline generation from approved traces
//! - Per-capability robust statistics (median/MAD/quantiles)
//! - Markov transition profiles with Dirichlet smoothing
//! - Drift detection with explainable anomalies
//! - Structured JSONL logging with required baseline fields
//! - Serialization roundtrip for baseline artifacts

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    BaselineDriftReport, ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext,
    HostCallPayload, RUNTIME_RISK_BASELINE_SCHEMA_VERSION, RuntimeRiskBaselineModel,
    RuntimeRiskConfig, RuntimeRiskStateLabelValue, build_baseline_from_ledger,
    build_baseline_from_ledger_with_options, detect_baseline_drift, dispatch_host_call_shared,
    verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::fs;

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
        decision_timeout_ms: 50,
        fail_closed: true,
    }
}

fn write_env_artifact(harness: &TestHarness, test_name: &str) {
    let env = json!({
        "test_name": test_name,
        "bead_id": "bd-153pv",
        "parent_bead": "bd-27qne",
        "rust_version": env!("CARGO_PKG_VERSION"),
        "target": std::env::consts::ARCH,
        "os": std::env::consts::OS,
    });
    let path = harness.temp_path("env.json");
    fs::write(&path, serde_json::to_string_pretty(&env).unwrap()).expect("write env.json");
    harness.record_artifact("env.json", &path);
}

// ============================================================================
// Test 1: Deterministic baseline from benign-only traces
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_baseline_deterministic_from_benign_traces() {
    let harness = TestHarness::new("e2e_baseline_deterministic_from_benign_traces");
    write_env_artifact(&harness, "e2e_baseline_deterministic_from_benign_traces");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.benign";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Generate 20 benign log calls
    futures::executor::block_on(async {
        for idx in 0..20 {
            let call = HostCallPayload {
                call_id: format!("benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("benign-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "ledger must be valid");

    // Build baseline twice - must be deterministic
    let b1 = build_baseline_from_ledger(&artifact, ext_id).expect("baseline 1");
    let b2 = build_baseline_from_ledger(&artifact, ext_id).expect("baseline 2");

    assert_eq!(b1.schema, RUNTIME_RISK_BASELINE_SCHEMA_VERSION);
    assert_eq!(b1.extension_id, ext_id);
    assert_eq!(b1.capability_profiles, b2.capability_profiles);
    assert_eq!(b1.transition_matrix, b2.transition_matrix);
    assert_eq!(b1.source_entry_count, b2.source_entry_count);

    // Verify capability profile
    assert_eq!(b1.capability_profiles.len(), 1);
    let log_profile = &b1.capability_profiles[0];
    assert_eq!(log_profile.capability, "log");
    assert_eq!(log_profile.sample_count, 20);
    assert!(log_profile.risk_score_median >= 0.0);
    assert!(log_profile.risk_score_median <= 1.0);
    assert!(log_profile.risk_score_mad >= 0.0);

    // Log evidence
    harness
        .log()
        .info_ctx("baseline_evidence", "baseline generated", |ctx| {
            ctx.push(("bead_id".into(), "bd-153pv".into()));
            ctx.push(("scenario".into(), "deterministic_benign".into()));
            ctx.push(("extension_id".into(), ext_id.to_string()));
            ctx.push(("entry_count".into(), b1.source_entry_count.to_string()));
            ctx.push(("capabilities".into(), "log".into()));
            ctx.push((
                "risk_score_median".into(),
                format!("{:.6}", log_profile.risk_score_median),
            ));
            ctx.push((
                "risk_score_mad".into(),
                format!("{:.6}", log_profile.risk_score_mad),
            ));
            ctx.push((
                "stationary_dist".into(),
                format!("{:?}", b1.transition_matrix.stationary_distribution),
            ));
        });

    // Write baseline as artifact
    let baseline_path = harness.temp_path("baseline-benign.json");
    fs::write(&baseline_path, serde_json::to_string_pretty(&b1).unwrap())
        .expect("write baseline artifact");
    harness.record_artifact("baseline-benign.json", &baseline_path);

    let jsonl_path = harness.temp_path("baseline-benign-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write evidence jsonl");
    harness.record_artifact("baseline-benign-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 2: Multi-capability baseline with mixed calls
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_baseline_multi_capability_profiles() {
    let harness = TestHarness::new("e2e_baseline_multi_capability_profiles");
    write_env_artifact(&harness, "e2e_baseline_multi_capability_profiles");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.multi";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Mixed capability calls
    futures::executor::block_on(async {
        for idx in 0..30 {
            let (capability, method) = match idx % 2 {
                0 => ("log", "log"),
                _ => ("exec", "exec"),
            };
            let call = HostCallPayload {
                call_id: format!("multi-{idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params: json!({ "message": format!("multi-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let baseline = build_baseline_from_ledger(&artifact, ext_id).expect("multi baseline");

    // Should have profiles for both capabilities
    let cap_names: Vec<&str> = baseline
        .capability_profiles
        .iter()
        .map(|p| p.capability.as_str())
        .collect();
    assert!(
        cap_names.contains(&"log"),
        "should have log profile: {cap_names:?}"
    );
    assert!(
        cap_names.contains(&"exec"),
        "should have exec profile: {cap_names:?}"
    );

    // exec should have higher risk_score_median than log
    let log_prof = baseline
        .capability_profiles
        .iter()
        .find(|p| p.capability == "log")
        .unwrap();
    let exec_prof = baseline
        .capability_profiles
        .iter()
        .find(|p| p.capability == "exec")
        .unwrap();
    assert!(
        exec_prof.risk_score_median > log_prof.risk_score_median,
        "exec ({:.4}) should have higher median risk than log ({:.4})",
        exec_prof.risk_score_median,
        log_prof.risk_score_median,
    );

    // Markov transition matrix should have > 0 transitions
    assert!(
        baseline.transition_matrix.total_transitions > 0,
        "should have observed transitions"
    );

    // Log each capability profile
    for profile in &baseline.capability_profiles {
        harness
            .log()
            .info_ctx("baseline_evidence", "capability profile", |ctx| {
                ctx.push(("bead_id".into(), "bd-153pv".into()));
                ctx.push(("scenario".into(), "multi_capability".into()));
                ctx.push(("capability".into(), profile.capability.clone()));
                ctx.push(("sample_count".into(), profile.sample_count.to_string()));
                ctx.push((
                    "risk_score_median".into(),
                    format!("{:.6}", profile.risk_score_median),
                ));
                ctx.push((
                    "risk_score_mad".into(),
                    format!("{:.6}", profile.risk_score_mad),
                ));
                ctx.push((
                    "risk_score_p5".into(),
                    format!("{:.6}", profile.risk_score_p5),
                ));
                ctx.push((
                    "risk_score_p95".into(),
                    format!("{:.6}", profile.risk_score_p95),
                ));
            });
    }

    let jsonl_path = harness.temp_path("multi-capability-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write evidence");
    harness.record_artifact("multi-capability-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 3: Drift detection detects adversarial workload shift
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_drift_detection_adversarial_shift() {
    let harness = TestHarness::new("e2e_drift_detection_adversarial_shift");
    write_env_artifact(&harness, "e2e_drift_detection_adversarial_shift");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.drift";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Phase 1: Build baseline from benign calls only
    futures::executor::block_on(async {
        for idx in 0..15 {
            let call = HostCallPayload {
                call_id: format!("baseline-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "message": format!("baseline-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let baseline = build_baseline_from_ledger_with_options(&artifact, ext_id, 3.0, 0.1, 1.0)
        .expect("baseline");

    // Phase 2: Simulate adversarial shift - exec calls have much higher risk scores
    // Use the baseline to detect drift with a high-risk score
    let log_profile = baseline
        .capability_profiles
        .iter()
        .find(|p| p.capability == "log")
        .unwrap();

    // Normal call - should not drift
    let normal_report = detect_baseline_drift(
        &baseline,
        ext_id,
        "log",
        log_profile.risk_score_median, // exactly baseline
        0.0,
        0.1,
        0.05,
        &[],
    );
    assert!(
        !normal_report.drift_detected,
        "normal call should not trigger drift"
    );

    // Adversarial call - high risk score far from baseline
    let adversarial_report = detect_baseline_drift(
        &baseline,
        ext_id,
        "log",
        0.95, // far above log baseline
        0.5,  // high error rate
        0.8,  // high burst
        0.7,
        &[
            RuntimeRiskStateLabelValue::Unsafe,
            RuntimeRiskStateLabelValue::Unsafe,
            RuntimeRiskStateLabelValue::Unsafe,
        ],
    );
    assert!(
        adversarial_report.drift_detected,
        "adversarial shift should trigger drift"
    );
    assert!(
        !adversarial_report.anomalies.is_empty(),
        "should have anomaly details"
    );

    // Log drift evidence
    log_drift_report(&harness, "normal", &normal_report);
    log_drift_report(&harness, "adversarial", &adversarial_report);

    let jsonl_path = harness.temp_path("drift-detection-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write drift evidence");
    harness.record_artifact("drift-detection-evidence.log.jsonl", &jsonl_path);
}

fn log_drift_report(harness: &TestHarness, scenario: &str, report: &BaselineDriftReport) {
    harness
        .log()
        .info_ctx("baseline_evidence", "drift report", |ctx| {
            ctx.push(("bead_id".into(), "bd-153pv".into()));
            ctx.push(("scenario".into(), format!("drift_{scenario}")));
            ctx.push(("extension_id".into(), report.extension_id.clone()));
            ctx.push(("capability".into(), report.capability.clone()));
            ctx.push(("drift_detected".into(), report.drift_detected.to_string()));
            ctx.push(("anomaly_count".into(), report.anomalies.len().to_string()));
            ctx.push((
                "transition_divergence".into(),
                format!("{:.6}", report.transition_divergence),
            ));
            ctx.push((
                "transition_anomalous".into(),
                report.transition_anomalous.to_string(),
            ));
        });
    for anomaly in &report.anomalies {
        harness
            .log()
            .info_ctx("baseline_evidence", "drift anomaly detail", |ctx| {
                ctx.push(("bead_id".into(), "bd-153pv".into()));
                ctx.push(("scenario".into(), format!("drift_{scenario}")));
                ctx.push(("metric".into(), anomaly.metric.clone()));
                ctx.push(("observed".into(), format!("{:.6}", anomaly.observed)));
                ctx.push((
                    "baseline_median".into(),
                    format!("{:.6}", anomaly.baseline_median),
                ));
                ctx.push((
                    "deviation_mads".into(),
                    format!("{:.2}", anomaly.deviation_mads),
                ));
                ctx.push(("explanation".into(), anomaly.explanation.clone()));
            });
    }
}

// ============================================================================
// Test 4: Markov transition anomaly detection
// ============================================================================

#[test]
fn e2e_markov_transition_anomaly() {
    let harness = TestHarness::new("e2e_markov_transition_anomaly");
    write_env_artifact(&harness, "e2e_markov_transition_anomaly");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.markov";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Build baseline from benign calls (produces SafeFast-heavy transitions)
    futures::executor::block_on(async {
        for idx in 0..20 {
            let call = HostCallPayload {
                call_id: format!("markov-benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "message": format!("benign-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let baseline = build_baseline_from_ledger_with_options(&artifact, ext_id, 3.0, 0.1, 1.0)
        .expect("baseline");

    // Baseline stationary distribution should be non-degenerate (all states > 0
    // due to Dirichlet smoothing)
    for (i, &prob) in baseline
        .transition_matrix
        .stationary_distribution
        .iter()
        .enumerate()
    {
        assert!(
            prob > 0.0,
            "stationary distribution[{i}] should be > 0, got {prob}"
        );
    }

    // Test with live states that are Unsafe-heavy
    let unsafe_states = vec![
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Suspicious,
        RuntimeRiskStateLabelValue::Unsafe,
        RuntimeRiskStateLabelValue::Unsafe,
    ];
    let report = detect_baseline_drift(
        &baseline,
        ext_id,
        "log",
        0.10,
        0.0,
        0.0,
        0.0,
        &unsafe_states,
    );
    assert!(
        report.transition_anomalous,
        "Unsafe-heavy live states should be anomalous vs SafeFast baseline"
    );
    assert!(
        report.transition_divergence > 0.0,
        "KL divergence should be positive"
    );

    harness
        .log()
        .info_ctx("baseline_evidence", "markov anomaly", |ctx| {
            ctx.push(("bead_id".into(), "bd-153pv".into()));
            ctx.push(("scenario".into(), "markov_anomaly".into()));
            ctx.push((
                "baseline_stationary".into(),
                format!("{:?}", baseline.transition_matrix.stationary_distribution),
            ));
            ctx.push((
                "transition_divergence".into(),
                format!("{:.6}", report.transition_divergence),
            ));
            ctx.push((
                "transition_anomalous".into(),
                report.transition_anomalous.to_string(),
            ));
        });

    let jsonl_path = harness.temp_path("markov-anomaly-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write markov evidence");
    harness.record_artifact("markov-anomaly-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 5: Baseline artifact serialization roundtrip
// ============================================================================

#[test]
fn e2e_baseline_artifact_roundtrip() {
    let harness = TestHarness::new("e2e_baseline_artifact_roundtrip");
    write_env_artifact(&harness, "e2e_baseline_artifact_roundtrip");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.serde";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Generate data
    futures::executor::block_on(async {
        for idx in 0..10 {
            let call = HostCallPayload {
                call_id: format!("serde-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "message": format!("serde-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let baseline = build_baseline_from_ledger(&artifact, ext_id).expect("baseline");

    // Serialize to JSON
    let json_str = serde_json::to_string_pretty(&baseline).expect("serialize");

    // Write to disk
    let baseline_path = harness.temp_path("baseline-roundtrip.json");
    fs::write(&baseline_path, &json_str).expect("write baseline");
    harness.record_artifact("baseline-roundtrip.json", &baseline_path);

    // Read back and deserialize
    let read_back = fs::read_to_string(&baseline_path).expect("read baseline");
    let deserialized: RuntimeRiskBaselineModel =
        serde_json::from_str(&read_back).expect("deserialize");

    assert_eq!(
        baseline, deserialized,
        "roundtrip should preserve all fields"
    );

    harness
        .log()
        .info_ctx("baseline_evidence", "roundtrip verified", |ctx| {
            ctx.push(("bead_id".into(), "bd-153pv".into()));
            ctx.push(("scenario".into(), "artifact_roundtrip".into()));
            ctx.push(("json_bytes".into(), json_str.len().to_string()));
            ctx.push(("schema".into(), baseline.schema.clone()));
        });

    let jsonl_path = harness.temp_path("roundtrip-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write evidence");
    harness.record_artifact("roundtrip-evidence.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 6: Structured JSONL log schema compliance
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_baseline_jsonl_schema_compliance() {
    let harness = TestHarness::new("e2e_baseline_jsonl_schema_compliance");
    write_env_artifact(&harness, "e2e_baseline_jsonl_schema_compliance");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(default_risk_config());

    let policy = permissive_policy();
    let ext_id = "ext.baseline.jsonl";
    let ctx = HostCallContext {
        runtime_name: "evidence",
        extension_id: Some(ext_id),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        for idx in 0..10 {
            let call = HostCallPayload {
                call_id: format!("jsonl-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "message": format!("jsonl-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let baseline = build_baseline_from_ledger(&artifact, ext_id).expect("baseline");

    // Log all baseline evidence fields
    for profile in &baseline.capability_profiles {
        harness
            .log()
            .info_ctx("baseline_evidence", "capability profile", |ctx| {
                ctx.push(("bead_id".into(), "bd-153pv".into()));
                ctx.push(("scenario".into(), "jsonl_compliance".into()));
                ctx.push(("extension_id".into(), baseline.extension_id.clone()));
                ctx.push(("capability".into(), profile.capability.clone()));
                ctx.push(("sample_count".into(), profile.sample_count.to_string()));
                ctx.push((
                    "risk_score_median".into(),
                    format!("{:.6}", profile.risk_score_median),
                ));
                ctx.push((
                    "risk_score_mad".into(),
                    format!("{:.6}", profile.risk_score_mad),
                ));
                ctx.push(("schema".into(), baseline.schema.clone()));
            });
    }

    let jsonl_path = harness.temp_path("baseline-jsonl-compliance.log.jsonl");
    harness.write_jsonl_logs(&jsonl_path).expect("write jsonl");
    harness.record_artifact("baseline-jsonl-compliance.log.jsonl", &jsonl_path);

    // Validate JSONL schema
    let raw = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut matched_rows = 0usize;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid jsonl line");
        if value.get("type").and_then(serde_json::Value::as_str) != Some("log") {
            continue;
        }
        if value.get("category").and_then(serde_json::Value::as_str) != Some("baseline_evidence") {
            continue;
        }

        let context = value
            .get("context")
            .and_then(serde_json::Value::as_object)
            .expect("context object");

        for key in [
            "bead_id",
            "scenario",
            "extension_id",
            "capability",
            "sample_count",
            "risk_score_median",
            "risk_score_mad",
            "schema",
        ] {
            assert!(context.contains_key(key), "missing context key: {key}");
        }
        assert!(
            value
                .get("ts")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "missing timestamp"
        );
        matched_rows += 1;
    }

    assert!(
        matched_rows >= 1,
        "expected >= 1 baseline_evidence rows, got {matched_rows}"
    );
}
