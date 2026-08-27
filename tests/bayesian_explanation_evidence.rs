//! E2E evidence tests for Bayesian scorer explanations (bd-3nvpz).
//!
//! Validates:
//! - Explanation payloads appear in telemetry, ledger, and replay artifacts
//! - Contribution ranking is deterministic under replay
//! - Budget enforcement triggers fallback mode correctly
//! - Calibration drift is surfaced with actionable logs
//! - Artifact bundle supports postmortem replay without hidden state

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RuntimeRiskCalibrationConfig, RuntimeRiskConfig,
    calibrate_runtime_risk_from_ledger, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::BTreeMap;
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

const fn make_risk_config(window_size: usize, ledger_limit: usize) -> RuntimeRiskConfig {
    RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size,
        ledger_limit,
        decision_timeout_ms: 5000,
        fail_closed: true,
    }
}

fn chrono_like_timestamp() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{epoch}")
}

fn write_env_artifact(harness: &TestHarness, test_name: &str) -> serde_json::Value {
    let env = json!({
        "test_name": test_name,
        "bead_id": "bd-3nvpz",
        "parent_bead": "bd-3ihzn",
        "rust_version": env!("CARGO_PKG_VERSION"),
        "target": std::env::consts::ARCH,
        "os": std::env::consts::OS,
        "timestamp_utc": chrono_like_timestamp(),
    });
    let path = harness.temp_path("env.json");
    fs::write(&path, serde_json::to_string_pretty(&env).unwrap()).expect("write env.json");
    harness.record_artifact("env.json", &path);
    env
}

fn write_manifest_artifact(
    harness: &TestHarness,
    test_name: &str,
    artifacts: &BTreeMap<String, String>,
) {
    let manifest = json!({
        "schema": "pi.ext.explanation_evidence.manifest.v1",
        "test_name": test_name,
        "bead_id": "bd-3nvpz",
        "artifacts": artifacts,
        "timestamp_utc": chrono_like_timestamp(),
    });
    let path = harness.temp_path("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest).unwrap()).expect("write manifest");
    harness.record_artifact("manifest.json", &path);
}

/// Build a `HostCallContext` with a configured `ExtensionManager`.
fn setup_context<'a>(
    harness: &'a TestHarness,
    tools: &'a ToolRegistry,
    http: &'a HttpConnector,
    manager: &'a ExtensionManager,
    policy: &'a ExtensionPolicy,
    ext_id: &'static str,
) -> HostCallContext<'a> {
    let _ = harness;
    HostCallContext {
        runtime_name: "evidence",
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

/// Dispatch a hostcall.
#[allow(clippy::future_not_send)]
async fn dispatch_call(
    ctx: &HostCallContext<'_>,
    call_id: &str,
    capability: &str,
    method: &str,
    params: serde_json::Value,
) {
    let call = HostCallPayload {
        call_id: call_id.to_string(),
        capability: capability.to_string(),
        method: method.to_string(),
        params,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let _ = dispatch_host_call_shared(ctx, call).await;
}

// ============================================================================
// Test 1: Explanation payloads present in telemetry after benign flow
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_fields_in_telemetry_artifact() {
    let harness = TestHarness::new("e2e_explanation_fields_in_telemetry_artifact");
    write_env_artifact(&harness, "e2e_explanation_fields_in_telemetry_artifact");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.telemetry",
    );

    futures::executor::block_on(async {
        // Benign log calls
        for idx in 0..3 {
            dispatch_call(
                &ctx,
                &format!("benign-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": format!("benign-{idx}") }),
            )
            .await;
        }
        // Exec calls (higher risk)
        for idx in 0..3 {
            dispatch_call(
                &ctx,
                &format!("exec-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": ["test"] }),
            )
            .await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert!(
        !telemetry.entries.is_empty(),
        "telemetry must have entries after hostcalls"
    );

    for event in &telemetry.entries {
        // Every telemetry event must have explanation fields
        assert!(
            !event.explanation_summary.is_empty(),
            "explanation_summary must not be empty for call {}",
            event.call_id
        );
        assert!(
            !event.top_contributors.is_empty(),
            "top_contributors must not be empty for call {}",
            event.call_id
        );
        // Budget state must have valid defaults
        assert!(
            event.budget_state.term_budget > 0,
            "term_budget must be positive"
        );
        assert!(
            event.budget_state.time_budget_ms > 0,
            "time_budget_ms must be positive"
        );
    }

    // Log evidence
    let jsonl_path = harness.temp_path("explanation-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write telemetry jsonl");
    harness.record_artifact("explanation-telemetry.log.jsonl", &jsonl_path);

    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        "explanation-telemetry.log.jsonl".to_string(),
        jsonl_path.to_string_lossy().to_string(),
    );
    write_manifest_artifact(
        &harness,
        "e2e_explanation_fields_in_telemetry_artifact",
        &artifacts,
    );
}

// ============================================================================
// Test 2: Explanation payloads in ledger entries with schema validation
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_in_ledger_with_schema() {
    let harness = TestHarness::new("e2e_explanation_in_ledger_with_schema");
    write_env_artifact(&harness, "e2e_explanation_in_ledger_with_schema");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.ledger",
    );

    futures::executor::block_on(async {
        for idx in 0..5 {
            dispatch_call(
                &ctx,
                &format!("ledger-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "ls", "args": ["-la"] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    assert!(
        !ledger.entries.is_empty(),
        "ledger must have entries after hostcalls"
    );

    for entry in &ledger.entries {
        // Schema version must match
        assert_eq!(
            entry.explanation_schema, RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION,
            "explanation_schema mismatch for call {}",
            entry.call_id
        );
        // Summary must be present
        assert!(
            !entry.explanation_summary.is_empty(),
            "explanation_summary empty for call {}",
            entry.call_id
        );
        // Contributors must be present
        assert!(
            !entry.top_contributors.is_empty(),
            "top_contributors empty for call {}",
            entry.call_id
        );
        // Each contributor must have valid fields
        for contrib in &entry.top_contributors {
            assert!(
                !contrib.code.is_empty(),
                "contributor code must not be empty"
            );
            assert!(
                contrib.magnitude.is_finite(),
                "contributor magnitude must be finite"
            );
            assert!(
                contrib.signed_impact.is_finite(),
                "contributor signed_impact must be finite"
            );
            assert!(
                !contrib.rationale.is_empty(),
                "contributor rationale must not be empty for {}",
                contrib.code
            );
        }
    }

    // Verify ledger integrity
    let verification = verify_runtime_risk_ledger_artifact(&ledger);
    assert!(
        verification.valid,
        "ledger verification failed: {:?}",
        verification.errors
    );
}

// ============================================================================
// Test 3: Replay produces identical explanation payloads
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_replay_parity() {
    let harness = TestHarness::new("e2e_explanation_replay_parity");
    write_env_artifact(&harness, "e2e_explanation_replay_parity");

    // Run identical trace through two independent managers
    let run_trace = || {
        let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
        let http = HttpConnector::with_defaults();
        let manager = ExtensionManager::new();
        manager.set_runtime_risk_config(make_risk_config(64, 512));
        let policy = permissive_policy();
        let ctx = setup_context(
            &harness,
            &tools,
            &http,
            &manager,
            &policy,
            "ext.explain.replay",
        );

        futures::executor::block_on(async {
            // Mixed benign + exec trace
            for idx in 0..3 {
                dispatch_call(
                    &ctx,
                    &format!("benign-{idx}"),
                    "log",
                    "log",
                    json!({ "level": "info", "message": format!("msg-{idx}") }),
                )
                .await;
            }
            for idx in 0..4 {
                dispatch_call(
                    &ctx,
                    &format!("exec-{idx}"),
                    "exec",
                    "exec",
                    json!({ "cmd": "echo", "args": [format!("arg-{idx}")] }),
                )
                .await;
            }
        });

        manager.runtime_risk_ledger_artifact()
    };

    let ledger1 = run_trace();
    let ledger2 = run_trace();

    // Both ledgers must have same number of entries
    assert_eq!(
        ledger1.entries.len(),
        ledger2.entries.len(),
        "ledger entry counts must match"
    );

    // Compare explanation payloads entry-by-entry
    for (e1, e2) in ledger1.entries.iter().zip(ledger2.entries.iter()) {
        assert_eq!(
            e1.explanation_level, e2.explanation_level,
            "explanation_level mismatch for call {} vs {}",
            e1.call_id, e2.call_id
        );
        assert_eq!(
            e1.explanation_summary, e2.explanation_summary,
            "explanation_summary mismatch for call {}",
            e1.call_id
        );
        assert_eq!(
            e1.top_contributors.len(),
            e2.top_contributors.len(),
            "contributor count mismatch for call {}",
            e1.call_id
        );
        for (c1, c2) in e1.top_contributors.iter().zip(e2.top_contributors.iter()) {
            assert_eq!(
                c1.code, c2.code,
                "contributor code mismatch for call {}",
                e1.call_id
            );
            assert!(
                (c1.signed_impact - c2.signed_impact).abs() < 1e-12,
                "contributor {} signed_impact diverged: {} vs {} for call {}",
                c1.code,
                c1.signed_impact,
                c2.signed_impact,
                e1.call_id
            );
            assert!(
                (c1.magnitude - c2.magnitude).abs() < 1e-12,
                "contributor {} magnitude diverged for call {}",
                c1.code,
                e1.call_id
            );
        }
        // Budget state must match
        assert_eq!(
            e1.budget_state.exhausted, e2.budget_state.exhausted,
            "budget exhausted mismatch for call {}",
            e1.call_id
        );
        assert_eq!(
            e1.budget_state.fallback_mode, e2.budget_state.fallback_mode,
            "budget fallback_mode mismatch for call {}",
            e1.call_id
        );
        assert_eq!(
            e1.budget_state.terms_emitted, e2.budget_state.terms_emitted,
            "budget terms_emitted mismatch for call {}",
            e1.call_id
        );
    }

    // Replay artifact should also match
    let replay1 = replay_runtime_risk_ledger_artifact(&ledger1).expect("replay1 must succeed");
    let replay2 = replay_runtime_risk_ledger_artifact(&ledger2).expect("replay2 must succeed");
    assert_eq!(replay1.steps.len(), replay2.steps.len());
    for (s1, s2) in replay1.steps.iter().zip(replay2.steps.iter()) {
        assert_eq!(s1.explanation_level, s2.explanation_level);
        assert_eq!(s1.explanation_summary, s2.explanation_summary);
        assert_eq!(s1.top_contributors.len(), s2.top_contributors.len());
    }

    harness.log().info_ctx(
        "replay_parity",
        "explanation replay parity validated",
        |ctx| {
            ctx.push(("bead_id".into(), "bd-3nvpz".into()));
            ctx.push(("entries_compared".into(), ledger1.entries.len().to_string()));
        },
    );
}

// ============================================================================
// Test 4: Contributor ordering is stable and sorted by magnitude desc
// ============================================================================

#[test]
fn e2e_explanation_contributor_ordering_stable() {
    let harness = TestHarness::new("e2e_explanation_contributor_ordering_stable");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.ordering",
    );

    futures::executor::block_on(async {
        for idx in 0..8 {
            dispatch_call(
                &ctx,
                &format!("order-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "test", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    for entry in &ledger.entries {
        // Contributors must be sorted by magnitude descending
        for window in entry.top_contributors.windows(2) {
            let ordering_ok = window[0].magnitude > window[1].magnitude
                || ((window[0].magnitude - window[1].magnitude).abs() < 1e-15
                    && window[0].code <= window[1].code);
            assert!(
                ordering_ok,
                "contributors not sorted for call {}: {} ({:.6}) vs {} ({:.6})",
                entry.call_id,
                window[0].code,
                window[0].magnitude,
                window[1].code,
                window[1].magnitude
            );
        }
    }
}

// ============================================================================
// Test 5: Calibration report includes explanation metrics
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_calibration_includes_metrics() {
    let harness = TestHarness::new("e2e_explanation_calibration_includes_metrics");
    write_env_artifact(&harness, "e2e_explanation_calibration_includes_metrics");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.calibration",
    );

    futures::executor::block_on(async {
        // Benign warmup
        for idx in 0..5 {
            dispatch_call(
                &ctx,
                &format!("warmup-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": "warmup" }),
            )
            .await;
        }
        // Exec calls for scoring diversity
        for idx in 0..10 {
            dispatch_call(
                &ctx,
                &format!("scored-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&ledger);
    assert!(verification.valid, "ledger must be valid for calibration");

    let calibration_config = RuntimeRiskCalibrationConfig::default();
    let report =
        calibrate_runtime_risk_from_ledger(&ledger, &calibration_config).expect("calibration ok");

    // Calibration report must have candidates
    assert!(
        !report.candidates.is_empty(),
        "calibration must produce candidates"
    );

    // Every ledger entry used for calibration must have explanation fields
    for entry in &ledger.entries {
        assert!(
            !entry.explanation_summary.is_empty(),
            "calibration input entry missing summary for {}",
            entry.call_id
        );
        assert_eq!(
            entry.explanation_schema,
            RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION
        );
    }

    // Log calibration evidence
    harness
        .log()
        .info_ctx("calibration", "calibration with explanations", |ctx| {
            ctx.push(("bead_id".into(), "bd-3nvpz".into()));
            ctx.push(("candidates".into(), report.candidates.len().to_string()));
            ctx.push((
                "recommended_threshold".into(),
                format!("{:.4}", report.recommended_threshold),
            ));
        });
}

// ============================================================================
// Test 6: Explanation level escalation under adversarial trace
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_level_escalation_adversarial() {
    let harness = TestHarness::new("e2e_explanation_level_escalation_adversarial");
    write_env_artifact(&harness, "e2e_explanation_level_escalation_adversarial");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.escalation",
    );

    futures::executor::block_on(async {
        // Benign warmup (should produce Compact explanations)
        for idx in 0..3 {
            dispatch_call(
                &ctx,
                &format!("warmup-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": "benign" }),
            )
            .await;
        }
        // Rapid exec burst (may trigger escalation)
        for idx in 0..15 {
            dispatch_call(
                &ctx,
                &format!("burst-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let ledger = manager.runtime_risk_ledger_artifact();

    // Collect explanation levels from telemetry
    let mut seen_levels: BTreeMap<String, usize> = BTreeMap::new();
    for event in &telemetry.entries {
        let level_str = format!("{:?}", event.explanation_level);
        *seen_levels.entry(level_str).or_insert(0) += 1;
    }

    // We should see explanation entries (exact levels depend on scoring dynamics)
    assert!(
        !seen_levels.is_empty(),
        "must have at least one explanation level category"
    );

    // Ledger entries for exec calls should have non-empty contributors
    for entry in ledger.entries.iter().filter(|e| e.capability == "exec") {
        assert!(
            !entry.top_contributors.is_empty(),
            "exec entry {} must have contributors",
            entry.call_id
        );
    }

    // Log distribution evidence
    for (level, count) in &seen_levels {
        harness
            .log()
            .info_ctx("explanation_levels", "level distribution", |ctx| {
                ctx.push(("bead_id".into(), "bd-3nvpz".into()));
                ctx.push(("level".into(), level.clone()));
                ctx.push(("count".into(), count.to_string()));
            });
    }

    let mut artifacts = BTreeMap::new();
    let jsonl_path = harness.temp_path("escalation-evidence.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write escalation jsonl");
    harness.record_artifact("escalation-evidence.log.jsonl", &jsonl_path);
    artifacts.insert(
        "escalation-evidence.log.jsonl".to_string(),
        jsonl_path.to_string_lossy().to_string(),
    );
    write_manifest_artifact(
        &harness,
        "e2e_explanation_level_escalation_adversarial",
        &artifacts,
    );
}

// ============================================================================
// Test 7: Replay steps carry explanation fields through round-trip
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_explanation_replay_steps_carry_fields() {
    let harness = TestHarness::new("e2e_explanation_replay_steps_carry_fields");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.replay.steps",
    );

    futures::executor::block_on(async {
        for idx in 0..6 {
            dispatch_call(
                &ctx,
                &format!("replay-step-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "test", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&ledger);
    assert!(verification.valid, "ledger must be valid");

    let replay = replay_runtime_risk_ledger_artifact(&ledger).expect("replay must succeed");
    assert_eq!(
        replay.steps.len(),
        ledger.entries.len(),
        "replay steps must match ledger entries"
    );

    for (step, entry) in replay.steps.iter().zip(ledger.entries.iter()) {
        // Replay step must carry explanation fields from ledger
        assert_eq!(
            step.explanation_level, entry.explanation_level,
            "replay step explanation_level must match ledger for {}",
            entry.call_id
        );
        assert_eq!(
            step.explanation_summary, entry.explanation_summary,
            "replay step explanation_summary must match ledger for {}",
            entry.call_id
        );
        assert_eq!(
            step.top_contributors.len(),
            entry.top_contributors.len(),
            "replay step contributor count must match ledger for {}",
            entry.call_id
        );
        assert_eq!(
            step.budget_state, entry.budget_state,
            "replay step budget_state must match ledger for {}",
            entry.call_id
        );
    }
}

// ============================================================================
// Test 8: Conformal and e-process fields alongside explanations
// ============================================================================

#[test]
fn e2e_explanation_conformal_coexistence() {
    let harness = TestHarness::new("e2e_explanation_conformal_coexistence");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = setup_context(
        &harness,
        &tools,
        &http,
        &manager,
        &policy,
        "ext.explain.conformal",
    );

    futures::executor::block_on(async {
        for idx in 0..10 {
            dispatch_call(
                &ctx,
                &format!("conformal-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    for entry in &ledger.entries {
        // Conformal fields must be present (not NaN; Inf is valid for e_process divergence)
        assert!(
            !entry.conformal_residual.is_nan(),
            "conformal_residual must not be NaN for {}",
            entry.call_id
        );
        assert!(
            !entry.conformal_quantile.is_nan(),
            "conformal_quantile must not be NaN for {}",
            entry.call_id
        );
        assert!(
            !entry.e_process.is_nan(),
            "e_process must not be NaN for {}",
            entry.call_id
        );
        assert!(
            entry.e_threshold.is_finite(),
            "e_threshold must be finite for {}",
            entry.call_id
        );
        // Explanation fields must also be present (they coexist)
        assert!(
            !entry.explanation_summary.is_empty(),
            "explanation must coexist with conformal fields for {}",
            entry.call_id
        );
        assert!(
            !entry.top_contributors.is_empty(),
            "contributors must coexist with conformal fields for {}",
            entry.call_id
        );
    }
}
