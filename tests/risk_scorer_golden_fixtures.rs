//! Golden fixture tests for the online deterministic risk scorer (bd-3f1ab).
//!
//! Validates:
//! - Multi-step decision sequences produce identical results across replays
//! - Reason codes are stable and deterministic for known input patterns
//! - Score composition follows documented weighted formula
//! - Ledger/replay/telemetry artifacts agree on decision outputs

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RuntimeRiskConfig, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::BTreeSet;

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

/// Run a standardized multi-step trace through a fresh manager and return the ledger.
fn run_golden_trace(
    harness: &TestHarness,
    ext_id: &'static str,
) -> pi::extensions::RuntimeRiskLedgerArtifact {
    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "golden",
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
        // Phase 1: Benign warmup (3 log calls)
        for idx in 0..3 {
            dispatch_call(
                &ctx,
                &format!("warmup-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": format!("warmup-{idx}") }),
            )
            .await;
        }
        // Phase 2: Exec burst (5 exec calls)
        for idx in 0..5 {
            dispatch_call(
                &ctx,
                &format!("exec-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": [idx.to_string()] }),
            )
            .await;
        }
        // Phase 3: Recovery (3 log calls)
        for idx in 0..3 {
            dispatch_call(
                &ctx,
                &format!("recovery-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": format!("recovery-{idx}") }),
            )
            .await;
        }
    });

    manager.runtime_risk_ledger_artifact()
}

// ============================================================================
// Test 1: Multi-step golden fixture determinism
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn golden_multi_step_replay_determinism() {
    let harness = TestHarness::new("golden_multi_step_replay_determinism");

    // Run the same trace twice through independent managers
    let ledger1 = run_golden_trace(&harness, "ext.golden.replay");
    let ledger2 = run_golden_trace(&harness, "ext.golden.replay");

    // Both must produce exactly 11 entries (3 + 5 + 3)
    assert_eq!(ledger1.entries.len(), 11, "ledger1 must have 11 entries");
    assert_eq!(ledger2.entries.len(), 11, "ledger2 must have 11 entries");

    // Entry-by-entry comparison
    for (e1, e2) in ledger1.entries.iter().zip(ledger2.entries.iter()) {
        // Call IDs must match
        assert_eq!(e1.call_id, e2.call_id, "call_id mismatch");
        assert_eq!(e1.capability, e2.capability, "capability mismatch");
        assert_eq!(e1.method, e2.method, "method mismatch");

        // Risk scores must be bit-identical
        assert!(
            (e1.risk_score - e2.risk_score).abs() < 1e-15,
            "risk_score diverged for {}: {} vs {}",
            e1.call_id,
            e1.risk_score,
            e2.risk_score
        );

        // Actions must match
        assert_eq!(
            e1.selected_action, e2.selected_action,
            "selected_action mismatch for {}",
            e1.call_id
        );

        // Derived state must match
        assert_eq!(
            e1.derived_state, e2.derived_state,
            "derived_state mismatch for {}",
            e1.call_id
        );

        // Triggers must match exactly
        assert_eq!(
            e1.triggers, e2.triggers,
            "triggers mismatch for {}",
            e1.call_id
        );

        // Posterior probabilities must match
        assert!(
            (e1.posterior.safe_fast - e2.posterior.safe_fast).abs() < 1e-12,
            "posterior.safe_fast diverged for {}",
            e1.call_id
        );
        assert!(
            (e1.posterior.suspicious - e2.posterior.suspicious).abs() < 1e-12,
            "posterior.suspicious diverged for {}",
            e1.call_id
        );

        // Expected loss values must match
        assert!(
            (e1.expected_loss.allow - e2.expected_loss.allow).abs() < 1e-12,
            "expected_loss.allow diverged for {}",
            e1.call_id
        );

        // E-process must match (may be Inf, but must be identical)
        assert_eq!(
            e1.e_process.to_bits(),
            e2.e_process.to_bits(),
            "e_process not bit-identical for {}",
            e1.call_id
        );

        // Drift detection must match
        assert_eq!(
            e1.drift_detected, e2.drift_detected,
            "drift_detected mismatch for {}",
            e1.call_id
        );

        // Explanation schema must match
        assert_eq!(
            e1.explanation_schema, e2.explanation_schema,
            "explanation_schema mismatch for {}",
            e1.call_id
        );

        // Explanation level must match
        assert_eq!(
            e1.explanation_level, e2.explanation_level,
            "explanation_level mismatch for {}",
            e1.call_id
        );
    }

    // Note: ledger hashes include timestamps, so they differ between runs.
    // We verify that the hash chains are internally consistent instead.
    let v1 = verify_runtime_risk_ledger_artifact(&ledger1);
    let v2 = verify_runtime_risk_ledger_artifact(&ledger2);
    assert!(v1.valid, "ledger1 hash chain invalid: {:?}", v1.errors);
    assert!(v2.valid, "ledger2 hash chain invalid: {:?}", v2.errors);

    harness.log().info_ctx(
        "golden_determinism",
        "multi-step golden fixture determinism verified",
        |ctx| {
            ctx.push(("bead_id".into(), "bd-3f1ab".into()));
            ctx.push(("entries".into(), "11".into()));
            ctx.push(("data_hash".into(), ledger1.data_hash.clone()));
        },
    );
}

// ============================================================================
// Test 2: Reason codes are stable for known patterns
// ============================================================================

#[test]
fn golden_reason_codes_stable() {
    let harness = TestHarness::new("golden_reason_codes_stable");
    let ledger = run_golden_trace(&harness, "ext.golden.reasons");

    // Collect all unique reason codes across the trace
    let all_codes: BTreeSet<String> = ledger
        .entries
        .iter()
        .flat_map(|e| e.triggers.iter().cloned())
        .collect();

    // Known valid reason codes (from the scoring engine)
    let known_codes: BTreeSet<&str> = [
        "quarantined",
        "feature_budget_exceeded",
        "e_process_breach",
        "drift_detected",
        "unsafe_streak",
        "decision_timeout",
        "burst_rate_anomaly",
        "high_error_rate",
        "consecutive_failure_escalation",
        "dangerous_capability_escalation",
        "unseen_capability_transition",
        "sensitive_target_mismatch",
    ]
    .into_iter()
    .collect();

    // Every emitted code must be from the known set
    for code in &all_codes {
        assert!(
            known_codes.contains(code.as_str()),
            "unknown reason code emitted: {code}"
        );
    }

    // Reason codes must be non-empty strings
    for entry in &ledger.entries {
        for trigger in &entry.triggers {
            assert!(
                !trigger.is_empty(),
                "empty trigger in call {}",
                entry.call_id
            );
            // Must be snake_case (no uppercase, no spaces)
            assert!(
                !trigger.chars().any(|c| c.is_uppercase() || c == ' '),
                "trigger {trigger} is not snake_case"
            );
        }
    }

    harness
        .log()
        .info_ctx("reason_codes", "reason code stability validated", |ctx| {
            ctx.push(("bead_id".into(), "bd-3f1ab".into()));
            ctx.push(("unique_codes".into(), all_codes.len().to_string()));
            ctx.push((
                "codes".into(),
                all_codes.iter().cloned().collect::<Vec<_>>().join(","),
            ));
        });
}

// ============================================================================
// Test 3: Score composition follows weighted formula
// ============================================================================

#[test]
fn golden_score_composition_bounded() {
    let harness = TestHarness::new("golden_score_composition_bounded");
    let ledger = run_golden_trace(&harness, "ext.golden.scores");

    for entry in &ledger.entries {
        // Risk score must be in [0, 1]
        assert!(
            entry.risk_score >= 0.0 && entry.risk_score <= 1.0,
            "risk_score {} out of [0,1] for call {}",
            entry.risk_score,
            entry.call_id
        );

        // Posterior probabilities must sum to ~1.0
        let posterior_sum =
            entry.posterior.safe_fast + entry.posterior.suspicious + entry.posterior.unsafe_;
        assert!(
            (posterior_sum - 1.0).abs() < 0.01,
            "posterior sum {} not ~1.0 for call {}",
            posterior_sum,
            entry.call_id
        );

        // Expected loss values must be non-negative
        assert!(
            entry.expected_loss.allow >= 0.0,
            "expected_loss.allow negative for {}",
            entry.call_id
        );
        assert!(
            entry.expected_loss.harden >= 0.0,
            "expected_loss.harden negative for {}",
            entry.call_id
        );
        assert!(
            entry.expected_loss.deny >= 0.0,
            "expected_loss.deny negative for {}",
            entry.call_id
        );
        assert!(
            entry.expected_loss.terminate >= 0.0,
            "expected_loss.terminate negative for {}",
            entry.call_id
        );

        // E-threshold must be 1/alpha = 100
        assert!(
            (entry.e_threshold - 100.0).abs() < 1e-10,
            "e_threshold {} != 100 for call {}",
            entry.e_threshold,
            entry.call_id
        );

        // Explanation schema must be present and correct
        assert_eq!(
            entry.explanation_schema, RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION,
            "wrong schema for {}",
            entry.call_id
        );

        // Contributors must have consistent magnitude = |signed_impact|
        for contrib in &entry.top_contributors {
            assert!(
                (contrib.magnitude - contrib.signed_impact.abs()).abs() < 1e-12,
                "magnitude ({}) != |signed_impact| ({}) for contributor {} in call {}",
                contrib.magnitude,
                contrib.signed_impact.abs(),
                contrib.code,
                entry.call_id
            );
        }
    }
}

// ============================================================================
// Test 4: Replay artifact matches ledger artifact exactly
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn golden_replay_matches_ledger() {
    let harness = TestHarness::new("golden_replay_matches_ledger");
    let ledger = run_golden_trace(&harness, "ext.golden.replay.match");

    // Verify ledger integrity
    let verification = verify_runtime_risk_ledger_artifact(&ledger);
    assert!(
        verification.valid,
        "ledger verification failed: {:?}",
        verification.errors
    );

    // Replay must succeed
    let replay = replay_runtime_risk_ledger_artifact(&ledger).expect("replay must succeed");
    assert_eq!(
        replay.steps.len(),
        ledger.entries.len(),
        "replay steps must match ledger entries"
    );

    // Every replay step must match the corresponding ledger entry
    for (step, entry) in replay.steps.iter().zip(ledger.entries.iter()) {
        assert_eq!(step.call_id, entry.call_id, "call_id mismatch in replay");
        assert_eq!(
            step.capability, entry.capability,
            "capability mismatch in replay"
        );
        assert_eq!(step.method, entry.method, "method mismatch in replay");

        // Action values must match (converted from internal enum to public value)
        assert_eq!(
            step.selected_action, entry.selected_action,
            "action mismatch for {} in replay",
            entry.call_id
        );

        // Risk score must match
        assert!(
            (step.risk_score - entry.risk_score).abs() < 1e-15,
            "risk_score diverged in replay for {}",
            entry.call_id
        );

        // Derived state must match
        assert_eq!(
            step.derived_state, entry.derived_state,
            "state mismatch in replay for {}",
            entry.call_id
        );

        // Reason codes must match
        assert_eq!(
            step.reason_codes, entry.triggers,
            "reason codes mismatch in replay for {}",
            entry.call_id
        );

        // Ledger hashes must match
        assert_eq!(
            step.ledger_hash, entry.ledger_hash,
            "ledger_hash mismatch in replay for {}",
            entry.call_id
        );

        // Explanation fields must carry through
        assert_eq!(
            step.explanation_level, entry.explanation_level,
            "explanation_level mismatch in replay for {}",
            entry.call_id
        );
        assert_eq!(
            step.explanation_summary, entry.explanation_summary,
            "explanation_summary mismatch in replay for {}",
            entry.call_id
        );
        assert_eq!(
            step.top_contributors.len(),
            entry.top_contributors.len(),
            "contributor count mismatch in replay for {}",
            entry.call_id
        );
    }
}

// ============================================================================
// Test 5: Telemetry and ledger agree on decisions
// ============================================================================

#[test]
fn golden_telemetry_ledger_agreement() {
    let harness = TestHarness::new("golden_telemetry_ledger_agreement");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "golden",
        extension_id: Some("ext.golden.agreement"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        for idx in 0..8 {
            let cap = if idx % 2 == 0 { "log" } else { "exec" };
            dispatch_call(
                &ctx,
                &format!("agree-{idx}"),
                cap,
                cap,
                json!({ "cmd": "echo", "args": ["test"] }),
            )
            .await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();

    // Same number of entries
    assert_eq!(
        ledger.entries.len(),
        telemetry.entries.len(),
        "ledger and telemetry entry counts must match"
    );

    // Entry-by-entry agreement
    for (l_entry, t_entry) in ledger.entries.iter().zip(telemetry.entries.iter()) {
        assert_eq!(
            l_entry.call_id, t_entry.call_id,
            "call_id order mismatch between ledger and telemetry"
        );
        assert_eq!(
            l_entry.capability, t_entry.capability,
            "capability mismatch between ledger and telemetry for {}",
            l_entry.call_id
        );

        // Risk scores must agree
        assert!(
            (l_entry.risk_score - t_entry.risk_score).abs() < 1e-15,
            "risk_score mismatch: ledger={} telemetry={} for {}",
            l_entry.risk_score,
            t_entry.risk_score,
            l_entry.call_id
        );

        // Selected actions must agree
        assert_eq!(
            l_entry.selected_action, t_entry.selected_action,
            "action mismatch for {}",
            l_entry.call_id
        );

        // Explanation summaries must agree
        assert_eq!(
            l_entry.explanation_summary, t_entry.explanation_summary,
            "explanation_summary mismatch for {}",
            l_entry.call_id
        );

        // Explanation levels must agree
        assert_eq!(
            l_entry.explanation_level, t_entry.explanation_level,
            "explanation_level mismatch for {}",
            l_entry.call_id
        );
    }
}

// ============================================================================
// Test 6: Score monotonicity under escalating risk
// ============================================================================

#[test]
fn golden_score_escalation_pattern() {
    let harness = TestHarness::new("golden_score_escalation_pattern");

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(make_risk_config(64, 512));
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "golden",
        extension_id: Some("ext.golden.escalation"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    // Phase 1: Pure benign
    futures::executor::block_on(async {
        for idx in 0..5 {
            dispatch_call(
                &ctx,
                &format!("benign-{idx}"),
                "log",
                "log",
                json!({ "level": "info", "message": "benign" }),
            )
            .await;
        }
    });

    let ledger_after_benign = manager.runtime_risk_ledger_artifact();
    let benign_scores: Vec<f64> = ledger_after_benign
        .entries
        .iter()
        .map(|e| e.risk_score)
        .collect();

    // Phase 2: Add exec calls
    futures::executor::block_on(async {
        for idx in 0..10 {
            dispatch_call(
                &ctx,
                &format!("escalate-{idx}"),
                "exec",
                "exec",
                json!({ "cmd": "echo", "args": [idx.to_string()] }),
            )
            .await;
        }
    });

    let ledger_after_exec = manager.runtime_risk_ledger_artifact();
    let exec_scores: Vec<f64> = ledger_after_exec
        .entries
        .iter()
        .skip(5) // skip benign entries
        .map(|e| e.risk_score)
        .collect();

    // Exec calls should generally have higher risk scores than benign calls
    #[allow(clippy::cast_precision_loss)]
    let avg_benign: f64 = benign_scores.iter().sum::<f64>() / benign_scores.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let avg_exec: f64 = exec_scores.iter().sum::<f64>() / exec_scores.len() as f64;

    assert!(
        avg_exec >= avg_benign,
        "exec avg score ({avg_exec:.4}) should be >= benign avg ({avg_benign:.4})"
    );

    harness
        .log()
        .info_ctx("escalation", "score escalation pattern validated", |ctx| {
            ctx.push(("bead_id".into(), "bd-3f1ab".into()));
            ctx.push(("avg_benign".into(), format!("{avg_benign:.6}")));
            ctx.push(("avg_exec".into(), format!("{avg_exec:.6}")));
        });
}

// ============================================================================
// Test 7: Hash chain integrity across full trace
// ============================================================================

#[test]
fn golden_hash_chain_integrity() {
    let harness = TestHarness::new("golden_hash_chain_integrity");
    let ledger = run_golden_trace(&harness, "ext.golden.hash");

    // Verify the hash chain via the verification API
    let verification = verify_runtime_risk_ledger_artifact(&ledger);
    assert!(
        verification.valid,
        "hash chain verification failed: {:?}",
        verification.errors
    );

    // Verify sequential hash linking
    for window in ledger.entries.windows(2) {
        assert_eq!(
            window[1].prev_ledger_hash.as_deref(),
            Some(window[0].ledger_hash.as_str()),
            "hash chain broken between {} and {}",
            window[0].call_id,
            window[1].call_id
        );
    }

    // First entry must have no prev hash
    assert!(
        ledger.entries[0].prev_ledger_hash.is_none(),
        "first entry must have no prev_ledger_hash"
    );

    // Head hash must match first entry
    assert_eq!(
        ledger.head_ledger_hash.as_deref(),
        Some(ledger.entries[0].ledger_hash.as_str()),
        "head_ledger_hash must match first entry"
    );

    // Tail hash must match last entry
    assert_eq!(
        ledger.tail_ledger_hash.as_deref(),
        ledger.entries.last().map(|e| e.ledger_hash.as_str()),
        "tail_ledger_hash must match last entry"
    );
}
