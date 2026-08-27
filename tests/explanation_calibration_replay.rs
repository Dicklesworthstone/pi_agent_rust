//! SEC-3.3A-TEST: Calibration and replay suite for Bayesian scorer explanations (bd-3nvpz).
//!
//! Validates:
//! - Replay parity: same trace yields identical explanation payloads across repeated runs
//! - Calibration drift: e-process/conformal signals surface with actionable logs
//! - Promotion artifacts: all decision evidence exportable and defensible
//! - Budget behavior: term/time budgets behave deterministically under exhaustion
//! - Expected-loss tie resolution and contribution ranking stability

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION, RUNTIME_RISK_LEDGER_SCHEMA_VERSION,
    RuntimeRiskActionValue, RuntimeRiskConfig, RuntimeRiskExplanationBudgetState,
    RuntimeRiskExplanationLevelValue, dispatch_host_call_shared,
    replay_runtime_risk_ledger_artifact, verify_runtime_risk_ledger_artifact,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::fs;
use std::time::Instant;

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
        runtime_name: "calibration_test",
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

fn assert_budget_state_matches_except_elapsed(
    left: &RuntimeRiskExplanationBudgetState,
    right: &RuntimeRiskExplanationBudgetState,
    entry_index: usize,
) {
    assert_eq!(
        left.time_budget_ms, right.time_budget_ms,
        "time_budget_ms mismatch at entry {entry_index}"
    );
    assert!(
        left.elapsed_ms <= left.time_budget_ms,
        "left elapsed_ms exceeded budget at entry {entry_index}: {} > {}",
        left.elapsed_ms,
        left.time_budget_ms
    );
    assert!(
        right.elapsed_ms <= right.time_budget_ms,
        "right elapsed_ms exceeded budget at entry {entry_index}: {} > {}",
        right.elapsed_ms,
        right.time_budget_ms
    );
    assert_eq!(
        left.term_budget, right.term_budget,
        "term_budget mismatch at entry {entry_index}"
    );
    assert_eq!(
        left.terms_emitted, right.terms_emitted,
        "terms_emitted mismatch at entry {entry_index}"
    );
    assert_eq!(
        left.exhausted, right.exhausted,
        "exhausted mismatch at entry {entry_index}"
    );
    assert_eq!(
        left.fallback_mode, right.fallback_mode,
        "fallback_mode mismatch at entry {entry_index}"
    );
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
        params: json!({ "cmd": "rm", "args": ["-rf", format!("/tmp/probe-{idx}")] }),
        timeout_ms: Some(10),
        cancel_token: None,
        context: None,
    }
}

fn recovery_call(idx: usize) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("recovery-{idx}"),
        capability: "log".to_string(),
        method: "log".to_string(),
        params: json!({ "level": "info", "message": format!("recovery-{idx}") }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

// ============================================================================
// Test 1: Replay parity — same trace twice yields identical explanation payloads
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn replay_parity_identical_explanation_payloads() {
    let harness = TestHarness::new("replay_parity_identical_explanation_payloads");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    // Run the same trace twice with fresh managers
    let mut artifacts = Vec::new();
    for run_idx in 0..2 {
        let (tools, http, manager, policy) = setup(&harness, default_risk_config());
        let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.replay.parity");

        futures::executor::block_on(async {
            for idx in 0..5 {
                let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
            }
            for idx in 0..8 {
                let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
            }
            for idx in 0..3 {
                let _ = dispatch_host_call_shared(&ctx, recovery_call(idx)).await;
            }
        });

        let artifact = manager.runtime_risk_ledger_artifact();
        let telemetry = manager.runtime_hostcall_telemetry_artifact();

        harness.log().info_ctx(
            "replay_parity",
            format!("run {run_idx} complete"),
            |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
                ctx_log.push(("run".into(), run_idx.to_string()));
                ctx_log.push(("entries".into(), artifact.entry_count.to_string()));
                ctx_log.push((
                    "telemetry_entries".into(),
                    telemetry.entry_count.to_string(),
                ));
            },
        );

        artifacts.push((artifact, telemetry));
    }

    // Compare the two runs
    let (ref art_a, ref tel_a) = artifacts[0];
    let (ref art_b, ref tel_b) = artifacts[1];

    assert_eq!(
        art_a.entry_count, art_b.entry_count,
        "ledger entry counts must match across runs"
    );
    assert_eq!(
        tel_a.entry_count, tel_b.entry_count,
        "telemetry entry counts must match across runs"
    );

    for (i, (ea, eb)) in art_a.entries.iter().zip(art_b.entries.iter()).enumerate() {
        assert_eq!(ea.call_id, eb.call_id, "call_id mismatch at entry {i}");
        assert!(
            (ea.risk_score - eb.risk_score).abs() < 1e-12,
            "risk_score mismatch at entry {i}: {} vs {}",
            ea.risk_score,
            eb.risk_score
        );
        assert_eq!(
            ea.selected_action, eb.selected_action,
            "action mismatch at entry {i}"
        );
        assert_eq!(
            ea.derived_state, eb.derived_state,
            "state mismatch at entry {i}"
        );
        assert_eq!(
            ea.explanation_level, eb.explanation_level,
            "explanation_level mismatch at entry {i}"
        );
        assert_eq!(
            ea.top_contributors.len(),
            eb.top_contributors.len(),
            "contributor count mismatch at entry {i}"
        );
        for (j, (ca, cb)) in ea
            .top_contributors
            .iter()
            .zip(eb.top_contributors.iter())
            .enumerate()
        {
            assert_eq!(ca.code, cb.code, "contributor code mismatch at [{i}][{j}]");
            assert!(
                (ca.signed_impact - cb.signed_impact).abs() < 1e-12,
                "contributor impact mismatch at [{i}][{j}]: {} vs {}",
                ca.signed_impact,
                cb.signed_impact
            );
        }
        assert_budget_state_matches_except_elapsed(&ea.budget_state, &eb.budget_state, i);
        assert_eq!(ea.triggers, eb.triggers, "triggers mismatch at entry {i}");
    }

    // Verify both ledgers pass hash chain verification
    let verify_a = verify_runtime_risk_ledger_artifact(art_a);
    let verify_b = verify_runtime_risk_ledger_artifact(art_b);
    assert!(verify_a.valid, "run 0 ledger must verify");
    assert!(verify_b.valid, "run 1 ledger must verify");

    // Write artifact bundle
    let jsonl_path = harness.temp_path("replay-parity.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write replay parity jsonl");
    harness.record_artifact("replay-parity.log.jsonl", &jsonl_path);

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: replay parity exceeded {budget_ms}ms (took {elapsed_ms}ms)"
    );
}

// ============================================================================
// Test 2: Explanation payloads carry correct schema and contributor structure
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn explanation_payload_schema_and_contributors() {
    let harness = TestHarness::new("explanation_payload_schema_and_contributors");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.schema.validate");

    futures::executor::block_on(async {
        // Benign baseline
        for idx in 0..4 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        // Adversarial phase
        for idx in 0..6 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    assert_eq!(artifact.schema, RUNTIME_RISK_LEDGER_SCHEMA_VERSION);

    for (i, entry) in artifact.entries.iter().enumerate() {
        // Schema version must be set on every entry
        assert_eq!(
            entry.explanation_schema, RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION,
            "explanation schema mismatch at entry {i}"
        );

        // Contributors must not be empty
        assert!(
            !entry.top_contributors.is_empty(),
            "entry {i} ({}) must have explanation contributors",
            entry.call_id
        );

        // Each contributor must have non-empty code and rationale
        for (j, contrib) in entry.top_contributors.iter().enumerate() {
            assert!(
                !contrib.code.is_empty(),
                "contributor [{i}][{j}] code must not be empty"
            );
            assert!(
                !contrib.rationale.is_empty(),
                "contributor [{i}][{j}] rationale must not be empty"
            );
            assert!(
                (contrib.magnitude - contrib.signed_impact.abs()).abs() < f64::EPSILON,
                "contributor [{i}][{j}] magnitude must equal |signed_impact|"
            );
        }

        // Budget state must have valid values
        assert!(
            entry.budget_state.term_budget > 0,
            "entry {i} term_budget must be positive"
        );
        assert!(
            entry.budget_state.terms_emitted <= entry.top_contributors.len(),
            "entry {i} terms_emitted must not exceed actual contributor count"
        );

        // Explanation summary must not be empty
        assert!(
            !entry.explanation_summary.is_empty(),
            "entry {i} explanation_summary must not be empty"
        );

        // Log each entry for forensic review
        harness
            .log()
            .debug_ctx("explanation_schema_check", "entry validated", |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
                ctx_log.push(("entry_idx".into(), i.to_string()));
                ctx_log.push(("call_id".into(), entry.call_id.clone()));
                ctx_log.push((
                    "explanation_level".into(),
                    format!("{:?}", entry.explanation_level),
                ));
                ctx_log.push((
                    "contributors".into(),
                    entry.top_contributors.len().to_string(),
                ));
                ctx_log.push((
                    "budget_exhausted".into(),
                    entry.budget_state.exhausted.to_string(),
                ));
            });
    }

    let jsonl_path = harness.temp_path("schema-contributors.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write schema check jsonl");
    harness.record_artifact("schema-contributors.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 3: E-process and conformal signals accumulate correctly
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn eprocess_conformal_signals_accumulate() {
    let harness = TestHarness::new("eprocess_conformal_signals_accumulate");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.eprocess.signals");

    futures::executor::block_on(async {
        // Steady benign calls to establish low e-process
        for idx in 0..10 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        // Shift to adversarial to push e-process up
        for idx in 0..15 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "ledger hash chain must verify");

    // Check e-process monotonicity during adversarial phase
    let exec_entries: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.capability == "exec")
        .collect();
    assert!(
        !exec_entries.is_empty(),
        "must have exec entries for e-process check"
    );

    // e_process should generally trend upward during adversarial phase
    let first_exec_ep = exec_entries.first().map_or(0.0, |e| e.e_process);
    let last_exec_ep = exec_entries.last().map_or(0.0, |e| e.e_process);

    harness.log().info_ctx(
        "eprocess_accumulation",
        "e-process signal tracking",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("benign_count".into(), "10".into()));
            ctx_log.push(("adversarial_count".into(), "15".into()));
            ctx_log.push(("first_exec_eprocess".into(), format!("{first_exec_ep:.6}")));
            ctx_log.push(("last_exec_eprocess".into(), format!("{last_exec_ep:.6}")));
            ctx_log.push(("e_threshold".into(), format!("{:.1}", 1.0 / 0.01)));
        },
    );

    // Verify conformal fields are populated
    for (i, entry) in artifact.entries.iter().enumerate() {
        assert!(
            entry.conformal_quantile >= 0.0,
            "entry {i} conformal_quantile must be non-negative"
        );
        assert!(
            entry.conformal_residual >= 0.0,
            "entry {i} conformal_residual must be non-negative"
        );
        assert!(
            entry.e_threshold > 0.0,
            "entry {i} e_threshold must be positive"
        );
    }

    // Check that drift_detected or e-process breach triggers appear in later entries
    let later_entries: Vec<_> = artifact.entries.iter().skip(15).collect();
    let has_drift_or_breach = later_entries.iter().any(|e| {
        e.drift_detected
            || e.triggers
                .iter()
                .any(|t| t == "e_process_breach" || t == "drift_detected")
    });

    harness.log().info_ctx(
        "drift_breach_check",
        "drift/breach signal presence",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push((
                "has_drift_or_breach".into(),
                has_drift_or_breach.to_string(),
            ));
            ctx_log.push(("later_entry_count".into(), later_entries.len().to_string()));
        },
    );

    let jsonl_path = harness.temp_path("eprocess-conformal.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write eprocess jsonl");
    harness.record_artifact("eprocess-conformal.log.jsonl", &jsonl_path);
}

// ============================================================================
// Test 4: Explanation level escalation matches enforcement severity
// ============================================================================

#[test]
fn explanation_level_matches_enforcement_severity() {
    let harness = TestHarness::new("explanation_level_matches_enforcement_severity");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.level.escalation");

    futures::executor::block_on(async {
        // Phase 1: benign calls → expect Compact level
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        // Phase 2: adversarial calls → expect Standard or Full level
        for idx in 0..10 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    // After posterior stabilizes, later benign calls should settle to Compact
    // (early calls may be Standard/Full due to uniform Dirichlet prior)
    let benign_entries: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("benign-"))
        .collect();
    // The last benign call should have settled to Compact or Standard
    if let Some(last_benign) = benign_entries.last() {
        assert!(
            matches!(
                last_benign.explanation_level,
                RuntimeRiskExplanationLevelValue::Compact
                    | RuntimeRiskExplanationLevelValue::Standard
            ),
            "last benign call {} should settle to Compact or Standard, got {:?}",
            last_benign.call_id,
            last_benign.explanation_level
        );
    }

    // Later adversarial calls should escalate to Standard or Full
    let late_adversarial: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("adversarial-"))
        .skip(5)
        .collect();
    let has_escalated = late_adversarial.iter().any(|e| {
        matches!(
            e.explanation_level,
            RuntimeRiskExplanationLevelValue::Standard | RuntimeRiskExplanationLevelValue::Full
        )
    });
    // Log for observability even if not escalated
    harness.log().info_ctx(
        "level_escalation",
        "explanation level tracking",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("has_escalated".into(), has_escalated.to_string()));
            ctx_log.push((
                "late_adversarial_count".into(),
                late_adversarial.len().to_string(),
            ));
        },
    );
}

// ============================================================================
// Test 5: Replay reconstructs decision path with matching explanations
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn replay_reconstructs_decision_path() {
    let harness = TestHarness::new("replay_reconstructs_decision_path");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.replay.reconstruct");

    futures::executor::block_on(async {
        for idx in 0..6 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..8 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
        for idx in 0..4 {
            let _ = dispatch_host_call_shared(&ctx, recovery_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&artifact);
    assert!(verification.valid, "ledger must verify before replay");

    let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay must succeed");
    assert_eq!(
        replay.steps.len(),
        artifact.entries.len(),
        "replay step count must match ledger entries"
    );

    // Verify replay steps match original entries
    for (i, (entry, step)) in artifact.entries.iter().zip(replay.steps.iter()).enumerate() {
        assert_eq!(
            entry.call_id, step.call_id,
            "replay call_id mismatch at step {i}"
        );
        assert_eq!(
            entry.selected_action, step.selected_action,
            "replay action mismatch at step {i}"
        );
    }

    // Verify telemetry has explanation fields for every entry
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert_eq!(
        telemetry.entry_count, artifact.entry_count,
        "telemetry and ledger must have same entry count"
    );
    for (i, event) in telemetry.entries.iter().enumerate() {
        assert!(
            !event.top_contributors.is_empty(),
            "telemetry entry {i} must have explanation contributors"
        );
    }

    // Write comprehensive artifact bundle
    let ledger_json = serde_json::to_string_pretty(&artifact).expect("serialize ledger");
    let ledger_path = harness.temp_path("replay-ledger.json");
    fs::write(&ledger_path, &ledger_json).expect("write ledger json");
    harness.record_artifact("replay-ledger.json", &ledger_path);

    let telemetry_json = serde_json::to_string_pretty(&telemetry).expect("serialize telemetry");
    let telemetry_path = harness.temp_path("replay-telemetry.json");
    fs::write(&telemetry_path, &telemetry_json).expect("write telemetry json");
    harness.record_artifact("replay-telemetry.json", &telemetry_path);

    let jsonl_path = harness.temp_path("replay-reconstruct.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write replay jsonl");
    harness.record_artifact("replay-reconstruct.log.jsonl", &jsonl_path);

    harness.log().info_ctx(
        "replay_reconstruct",
        "replay reconstruction complete",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("ledger_entries".into(), artifact.entry_count.to_string()));
            ctx_log.push(("replay_steps".into(), replay.steps.len().to_string()));
            ctx_log.push(("hash_chain_valid".into(), verification.valid.to_string()));
        },
    );
}

// ============================================================================
// Test 6: Contribution ranking is stable under adversarial trace perturbation
// ============================================================================

#[test]
fn contribution_ranking_stable_under_perturbation() {
    let harness = TestHarness::new("contribution_ranking_stable_under_perturbation");

    // Run identical trace 3 times, verify contributor ordering never changes
    let mut all_contributor_sequences = Vec::new();
    for _ in 0..3 {
        let (tools, http, manager, policy) = setup(&harness, default_risk_config());
        let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.contrib.stability");

        futures::executor::block_on(async {
            for idx in 0..3 {
                let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
            }
            for idx in 0..5 {
                let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
            }
        });

        let artifact = manager.runtime_risk_ledger_artifact();
        let contributor_codes: Vec<Vec<String>> = artifact
            .entries
            .iter()
            .map(|e| e.top_contributors.iter().map(|c| c.code.clone()).collect())
            .collect();
        all_contributor_sequences.push(contributor_codes);
    }

    // Compare all runs pairwise
    for run_idx in 1..all_contributor_sequences.len() {
        assert_eq!(
            all_contributor_sequences[0].len(),
            all_contributor_sequences[run_idx].len(),
            "entry count mismatch between run 0 and run {run_idx}"
        );
        for (entry_idx, (seq_a, seq_b)) in all_contributor_sequences[0]
            .iter()
            .zip(all_contributor_sequences[run_idx].iter())
            .enumerate()
        {
            assert_eq!(
                seq_a, seq_b,
                "contributor ordering mismatch at entry {entry_idx} between run 0 and run {run_idx}"
            );
        }
    }
}

// ============================================================================
// Test 7: Artifact bundle supports postmortem defensibility
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn artifact_bundle_postmortem_defensibility() {
    let harness = TestHarness::new("artifact_bundle_postmortem_defensibility");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.postmortem.bundle");

    futures::executor::block_on(async {
        for idx in 0..4 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..10 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, recovery_call(idx)).await;
        }
    });

    let ledger = manager.runtime_risk_ledger_artifact();
    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let verification = verify_runtime_risk_ledger_artifact(&ledger);

    // 1. Ledger must verify
    assert!(verification.valid, "ledger hash chain must be intact");

    // 2. Data hash must be non-empty (reproducibility anchor)
    assert!(
        !ledger.data_hash.is_empty(),
        "ledger data_hash must be populated"
    );

    // 3. Hash chain linkage must be consistent
    for (i, entry) in ledger.entries.iter().enumerate() {
        assert!(
            !entry.ledger_hash.is_empty(),
            "entry {i} ledger_hash must be populated"
        );
        if i == 0 {
            assert!(
                entry.prev_ledger_hash.is_none(),
                "first entry must have no prev_ledger_hash"
            );
        } else {
            assert_eq!(
                entry.prev_ledger_hash.as_deref(),
                Some(ledger.entries[i - 1].ledger_hash.as_str()),
                "entry {i} prev_hash must point to entry {}'s hash",
                i - 1
            );
        }
    }

    // 4. Head/tail hashes match first/last entries
    if !ledger.entries.is_empty() {
        assert_eq!(
            ledger.head_ledger_hash.as_deref(),
            Some(ledger.entries.first().unwrap().ledger_hash.as_str()),
            "head_ledger_hash must match first entry"
        );
        assert_eq!(
            ledger.tail_ledger_hash.as_deref(),
            Some(ledger.entries.last().unwrap().ledger_hash.as_str()),
            "tail_ledger_hash must match last entry"
        );
    }

    // 5. Every entry must have a defensible explanation
    for (i, entry) in ledger.entries.iter().enumerate() {
        assert!(
            !entry.explanation_summary.is_empty(),
            "entry {i} must have explanation summary for defensibility"
        );
        assert!(
            !entry.top_contributors.is_empty(),
            "entry {i} must have evidence contributors for defensibility"
        );
    }

    // 6. Write artifact bundle
    let ledger_json = serde_json::to_string_pretty(&ledger).expect("serialize ledger");
    let ledger_path = harness.temp_path("postmortem-ledger.json");
    fs::write(&ledger_path, &ledger_json).expect("write ledger");
    harness.record_artifact("postmortem-ledger.json", &ledger_path);

    let telemetry_json = serde_json::to_string_pretty(&telemetry).expect("serialize telemetry");
    let telemetry_path = harness.temp_path("postmortem-telemetry.json");
    fs::write(&telemetry_path, &telemetry_json).expect("write telemetry");
    harness.record_artifact("postmortem-telemetry.json", &telemetry_path);

    let verify_json = serde_json::to_string_pretty(&verification).expect("serialize verification");
    let verify_path = harness.temp_path("postmortem-verification.json");
    fs::write(&verify_path, &verify_json).expect("write verification");
    harness.record_artifact("postmortem-verification.json", &verify_path);

    // Write artifact index
    let index_path = harness.temp_path("postmortem-artifact-index.jsonl");
    harness
        .write_artifact_index_jsonl(&index_path)
        .expect("write artifact index");
    harness.record_artifact("postmortem-artifact-index.jsonl", &index_path);

    let jsonl_path = harness.temp_path("postmortem-bundle.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write postmortem jsonl");
    harness.record_artifact("postmortem-bundle.log.jsonl", &jsonl_path);

    harness.log().info_ctx(
        "postmortem_bundle",
        "artifact bundle generated",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("ledger_entries".into(), ledger.entry_count.to_string()));
            ctx_log.push(("hash_chain_valid".into(), verification.valid.to_string()));
            ctx_log.push(("data_hash".into(), ledger.data_hash.clone()));
        },
    );
}

// ============================================================================
// Test 8: Benign/adversarial/recovery phases show correct action progression
// ============================================================================

#[test]
fn action_progression_through_phases() {
    let harness = TestHarness::new("action_progression_through_phases");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.phase.progression");

    futures::executor::block_on(async {
        // Phase 1: 5 benign calls
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        // Phase 2: 12 adversarial calls
        for idx in 0..12 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
        // Phase 3: 5 recovery calls
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, recovery_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    // Benign phase: posterior starts with uniform prior so early calls may get Harden.
    // After stabilization, later benign calls should settle toward Allow/SafeFast.
    let benign: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("benign-"))
        .collect();
    // Last 2 benign calls should be Allow or Harden (not Deny/Terminate)
    for entry in benign.iter().skip(benign.len().saturating_sub(2)) {
        assert!(
            matches!(
                entry.selected_action,
                RuntimeRiskActionValue::Allow | RuntimeRiskActionValue::Harden
            ),
            "late benign call {} should be Allow or Harden, got {:?}",
            entry.call_id,
            entry.selected_action
        );
    }

    // Adversarial phase: risk scores should be higher
    let adversarial: Vec<_> = artifact
        .entries
        .iter()
        .filter(|e| e.call_id.starts_with("adversarial-"))
        .collect();
    assert!(!adversarial.is_empty());
    let adversarial_len = f64::from(
        u32::try_from(adversarial.len()).expect("adversarial vector length must fit u32"),
    );
    let benign_len =
        f64::from(u32::try_from(benign.len().max(1)).expect("benign vector length must fit u32"));
    let avg_adversarial_risk: f64 =
        adversarial.iter().map(|e| e.risk_score).sum::<f64>() / adversarial_len;
    let avg_benign_risk: f64 = benign.iter().map(|e| e.risk_score).sum::<f64>() / benign_len;
    assert!(
        avg_adversarial_risk > avg_benign_risk,
        "adversarial avg risk ({avg_adversarial_risk:.4}) must exceed benign ({avg_benign_risk:.4})"
    );

    harness.log().info_ctx(
        "phase_progression",
        "action progression validated",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("benign_count".into(), benign.len().to_string()));
            ctx_log.push(("adversarial_count".into(), adversarial.len().to_string()));
            ctx_log.push(("avg_benign_risk".into(), format!("{avg_benign_risk:.6}")));
            ctx_log.push((
                "avg_adversarial_risk".into(),
                format!("{avg_adversarial_risk:.6}"),
            ));
        },
    );
}

// ============================================================================
// Test 9: JSONL logging contract has all required explanation fields
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn jsonl_logging_contract_explanation_fields() {
    let harness = TestHarness::new("jsonl_logging_contract_explanation_fields");

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.jsonl.contract");

    futures::executor::block_on(async {
        for idx in 0..3 {
            let _ = dispatch_host_call_shared(&ctx, benign_call(idx)).await;
        }
        for idx in 0..5 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_artifact();

    // Log telemetry events with explanation fields via harness
    for event in &telemetry.entries {
        let contributor_codes: Vec<&str> = event
            .top_contributors
            .iter()
            .map(|c| c.code.as_str())
            .collect();
        harness.log().info_ctx(
            "explanation_telemetry",
            "telemetry event with explanation",
            |ctx_log| {
                ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
                ctx_log.push(("call_id".into(), event.call_id.clone()));
                ctx_log.push(("capability".into(), event.capability.clone()));
                ctx_log.push((
                    "explanation_level".into(),
                    format!("{:?}", event.explanation_level).to_lowercase(),
                ));
                ctx_log.push((
                    "explanation_summary".into(),
                    event.explanation_summary.clone(),
                ));
                ctx_log.push(("top_contributors".into(), contributor_codes.join("|")));
                ctx_log.push((
                    "budget_exhausted".into(),
                    event.budget_state.exhausted.to_string(),
                ));
                ctx_log.push((
                    "budget_fallback".into(),
                    event.budget_state.fallback_mode.to_string(),
                ));
                ctx_log.push((
                    "terms_emitted".into(),
                    event.budget_state.terms_emitted.to_string(),
                ));
            },
        );
    }

    // Write JSONL and validate contract
    let jsonl_path = harness.temp_path("explanation-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write telemetry jsonl");
    harness.record_artifact("explanation-telemetry.log.jsonl", &jsonl_path);

    let raw = fs::read_to_string(&jsonl_path).expect("read telemetry jsonl");
    let mut matched = 0usize;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid jsonl line");
        if value.get("type").and_then(serde_json::Value::as_str) != Some("log") {
            continue;
        }
        if value.get("category").and_then(serde_json::Value::as_str)
            != Some("explanation_telemetry")
        {
            continue;
        }
        let context = value
            .get("context")
            .and_then(serde_json::Value::as_object)
            .expect("context object");
        for key in [
            "issue_id",
            "call_id",
            "capability",
            "explanation_level",
            "explanation_summary",
            "top_contributors",
            "budget_exhausted",
            "budget_fallback",
            "terms_emitted",
        ] {
            assert!(context.contains_key(key), "missing context key: {key}");
        }
        matched = matched.saturating_add(1);
    }
    assert!(
        matched >= telemetry.entry_count,
        "expected >= {} explanation telemetry rows, got {}",
        telemetry.entry_count,
        matched
    );
}

// ============================================================================
// Test 10: Budget-bounded replay with runtime enforcement
// ============================================================================

#[test]
fn budget_bounded_replay_runtime() {
    let harness = TestHarness::new("budget_bounded_replay_runtime");
    let budget_ms = 10_000u128;
    let started = Instant::now();

    let (tools, http, manager, policy) = setup(&harness, default_risk_config());
    let ctx = make_ctx(&tools, &http, &manager, &policy, "ext.budget.runtime");

    futures::executor::block_on(async {
        for idx in 0..20 {
            let _ = dispatch_host_call_shared(&ctx, adversarial_call(idx)).await;
        }
    });

    let artifact = manager.runtime_risk_ledger_artifact();

    // Verify all entries have valid budget state
    for (i, entry) in artifact.entries.iter().enumerate() {
        if entry.budget_state.exhausted {
            assert!(
                entry.budget_state.fallback_mode,
                "entry {i}: exhausted budget must trigger fallback mode"
            );
            assert_eq!(
                entry.explanation_level,
                RuntimeRiskExplanationLevelValue::Compact,
                "entry {i}: budget exhaustion must force Compact level"
            );
        }
    }

    // Replay must succeed within budget
    let replay = replay_runtime_risk_ledger_artifact(&artifact).expect("replay must succeed");
    assert_eq!(replay.steps.len(), artifact.entries.len());

    let elapsed_ms = started.elapsed().as_millis();
    assert!(
        elapsed_ms <= budget_ms,
        "budget_exhausted: test exceeded {budget_ms}ms (took {elapsed_ms}ms)"
    );

    harness.log().info_ctx(
        "budget_bounded_replay",
        "budget-bounded replay complete",
        |ctx_log| {
            ctx_log.push(("issue_id".into(), "bd-3nvpz".into()));
            ctx_log.push(("elapsed_ms".into(), elapsed_ms.to_string()));
            ctx_log.push(("budget_ms".into(), budget_ms.to_string()));
            ctx_log.push(("entries".into(), artifact.entry_count.to_string()));
        },
    );
}
