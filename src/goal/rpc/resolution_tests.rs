//! Native journal/persistence tests for the RPC adapter's reviewed stop route.
//! No provider stub, simulated journal, or alternate persistence path is used.

use super::*;
use crate::agent::{Agent, AgentConfig};
use crate::compaction::ResolvedCompactionSettings;
use crate::goal::{CHECKPOINT_TYPE, MAX_TEXT_BYTES};
use crate::model::{AssistantMessage, Usage};
use crate::providers::openai::OpenAIProvider;
use crate::session::SessionMessage;
use crate::tools::ToolRegistry;
use std::path::Path;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn case(name: &str) {
    eprintln!(
        "{}",
        json!({"event":"goal_rpc_resolution_test", "case":name})
    );
}

fn record() -> Record {
    let mut record = Record::new(
        "Finish the requested migration",
        GoalBudget {
            max_prompts: 4,
            token_budget: 200,
        },
    )
    .unwrap();
    record.prompts_started = 2;
    record.tokens_spent = 60;
    record
}

fn stored(record: &Record) -> Session {
    let mut session = Session::in_memory();
    append(&mut session, record).unwrap();
    session
}

fn fixture(session: Session, save: bool) -> AgentSession {
    let provider = Arc::new(
        OpenAIProvider::new("goal-resolution-fixture").with_base_url("http://127.0.0.1:1/v1"),
    );
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], Path::new("."), None),
        AgentConfig::default(),
    );
    AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(session)),
        save,
        ResolvedCompactionSettings::default(),
    )
}

fn state(agent: &AgentSession) -> Value {
    rpc_goal_state(&agent.session.try_lock().unwrap()).unwrap()
}

fn history(agent: &AgentSession) -> Value {
    let session = agent.session.try_lock().unwrap();
    json!({"leaf":session.leaf_id(), "entries":session.entries})
}

#[allow(clippy::needless_pass_by_value)] // callers build the resolution inline with json!
fn request(agent: &AgentSession, resolution: Value) -> Value {
    json!({"type":"goal", "op":"pause", "revision":state(agent)["revision"],
        "resolution":resolution})
}

#[test]
fn complete_is_one_native_decision_preserving_identity_usage_and_literal_evidence() {
    case("complete_native_state");
    let original = record();
    let mut agent = fixture(stored(&original), false);
    let before = state(&agent);
    let evidence = "  Reviewed 日本語 tests\nResults are an authored attestation, not a command.  ";
    let input = request(&agent, json!({"kind":"complete", "evidence":evidence}));
    let output = run(rpc_goal_command(
        &mut agent,
        &AgentCx::for_request(),
        &input,
    ))
    .unwrap();
    assert_eq!(output["goal"]["status"], "complete");
    assert_eq!(output["state"]["enabled"], false);
    assert_eq!(output["goal"]["note"], evidence);
    for field in [
        "id",
        "objective",
        "promptsStarted",
        "tokensUsed",
        "maxPrompts",
        "tokenBudget",
    ] {
        assert_eq!(output["goal"][field], before["goal"][field], "{field}");
    }
    assert_eq!(output["persistence"], "memory_only");
    assert_eq!(
        output["resolution"],
        json!({"kind":"complete","journalChanged":true})
    );
    assert_ne!(output["revision"], before["revision"]);
    assert_eq!(agent.session.try_lock().unwrap().entries.len(), 2);
    assert!(
        agent
            .session
            .try_lock()
            .unwrap()
            .entries
            .iter()
            .all(|entry| matches!(entry, crate::session::SessionEntry::Custom(_)))
    );
}

#[test]
fn repeat_completion_never_duplicates_a_decision_or_completes_a_replacement() {
    case("no_decision_replay");
    let mut agent = fixture(stored(&record()), false);
    let first = request(&agent, json!({"kind":"complete", "evidence":"Reviewed"}));
    run(async {
        let owner = AgentCx::for_current_or_request();
        rpc_goal_command(&mut agent, &owner, &first).await.unwrap();
        let after = history(&agent);
        assert!(
            rpc_goal_command(&mut agent, &owner, &first)
                .await
                .unwrap_err()
                .to_string()
                .contains("GOAL_STALE")
        );
        assert_eq!(history(&agent), after);
        let fresh = request(&agent, json!({"kind":"complete","evidence":"Again"}));
        assert!(
            rpc_goal_command(&mut agent, &owner, &fresh)
                .await
                .unwrap_err()
                .to_string()
                .contains("GOAL_TERMINAL")
        );
        assert_eq!(history(&agent), after);
        rpc_goal_command(
            &mut agent,
            &owner,
            &json!({"op":"create","objective":"New objective"}),
        )
        .await
        .unwrap();
        let replacement = history(&agent);
        assert!(
            rpc_goal_command(&mut agent, &owner, &fresh)
                .await
                .unwrap_err()
                .to_string()
                .contains("GOAL_STALE")
        );
        assert_eq!(history(&agent), replacement);
        assert_eq!(state(&agent)["goal"]["status"], "active");
    });
}

#[test]
fn save_only_preserves_every_native_status_counters_receipt_and_interrupted_intent() {
    case("save_is_not_a_transition");
    let active = record();
    let paused = transition(active.clone(), Transition::Pause("review")).unwrap();
    let complete = transition(active.clone(), Transition::Complete("attested")).unwrap();
    let dropped = transition(active.clone(), Transition::Drop("not needed")).unwrap();
    let mut exhausted = active.clone();
    exhausted.tokens_spent = exhausted.token_budget;
    let mut interrupted = active.clone();
    interrupted.in_flight = true;
    for record in [active, paused, complete, dropped, exhausted, interrupted] {
        let mut agent = fixture(stored(&record), false);
        let input = request(&agent, json!({"kind":"save"}));
        let before = history(&agent);
        let original = state(&agent);
        let output = run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &input,
        ))
        .unwrap();
        assert_eq!(output["goal"], original["goal"]);
        assert_eq!(output["revision"], original["revision"]);
        assert_eq!(output["persistence"], "memory_only");
        assert_eq!(
            output["resolution"],
            json!({"kind":"save","journalChanged":false})
        );
        assert_eq!(history(&agent), before);
    }
}

#[test]
fn malformed_resolution_conflicts_and_evidence_fail_without_mutating_history() {
    case("strict_admission");
    let mut agent = fixture(stored(&record()), false);
    let before = history(&agent);
    let invalid = [
        Value::Null,
        json!([]),
        json!(true),
        json!({}),
        json!({"kind":"unknown"}),
        json!({"kind":"complete"}),
        json!({"kind":"complete","evidence":null}),
        json!({"kind":"complete","evidence":" \n"}),
        json!({"kind":"complete","evidence":"雪".repeat(MAX_TEXT_BYTES / 3 + 1)}),
        json!({"kind":"complete","evidence":"CANARY","extra":true}),
        json!({"kind":"save","evidence":"CANARY"}),
    ];
    for resolution in invalid {
        let input = request(&agent, resolution);
        let error = run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &input,
        ))
        .unwrap_err();
        assert!(!error.to_string().contains("CANARY"));
        assert_eq!(history(&agent), before);
    }
    for op in ["get", "create", "resume", "drop", "unknown"] {
        let mut input = request(&agent, json!({"kind":"save"}));
        input["op"] = json!(op);
        assert!(
            run(rpc_goal_command(
                &mut agent,
                &AgentCx::for_request(),
                &input
            ))
            .is_err()
        );
        assert_eq!(history(&agent), before);
    }
    for field in [
        "budget",
        "objective",
        "token_budget",
        "max_prompts",
        "reason",
    ] {
        let mut input = request(&agent, json!({"kind":"save"}));
        input[field] = Value::Null;
        assert!(
            run(rpc_goal_command(
                &mut agent,
                &AgentCx::for_request(),
                &input
            ))
            .is_err()
        );
        assert_eq!(history(&agent), before);
    }
    for receipt in [
        Value::Null,
        json!(1),
        json!("gr1-"),
        json!("g1-foo"),
        json!(format!("gr1-{}", "A".repeat(64))),
        json!(format!("gr1-{}", "雪".repeat(22))),
    ] {
        let mut input = request(&agent, json!({"kind":"save"}));
        input["revision"] = receipt;
        assert!(
            run(rpc_goal_command(
                &mut agent,
                &AgentCx::for_request(),
                &input
            ))
            .is_err()
        );
        assert_eq!(history(&agent), before);
    }
}

#[test]
fn current_branch_and_live_usage_are_part_of_both_resolution_receipts() {
    case("stale_branch_and_usage");
    let session = stored(&record());
    let first_leaf = session.leaf_id().unwrap().to_owned();
    let mut agent = fixture(session, false);
    for kind in ["save", "complete"] {
        let resolution = if kind == "save" {
            json!({"kind":kind})
        } else {
            json!({"kind":kind,"evidence":"Reviewed"})
        };
        let input = request(&agent, resolution);
        agent
            .session
            .try_lock()
            .unwrap()
            .append_message(SessionMessage::Assistant {
                message: AssistantMessage {
                    usage: Usage {
                        total_tokens: 7,
                        ..Usage::default()
                    },
                    ..AssistantMessage::default()
                },
            });
        let before = history(&agent);
        let error = run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &input,
        ))
        .unwrap_err();
        assert!(error.to_string().contains("GOAL_STALE"));
        assert_eq!(history(&agent), before);
    }
    let reviewed = request(
        &agent,
        json!({"kind":"complete","evidence":"Reviewed this branch"}),
    );
    assert!(agent.session.try_lock().unwrap().navigate_to(&first_leaf));
    append(&mut agent.session.try_lock().unwrap(), &record()).unwrap();
    let before = history(&agent);
    assert!(
        run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &reviewed
        ))
        .unwrap_err()
        .to_string()
        .contains("GOAL_STALE")
    );
    assert_eq!(history(&agent), before);
    let mut other = fixture(stored(&record()), false);
    assert!(
        run(rpc_goal_command(
            &mut other,
            &AgentCx::for_request(),
            &reviewed
        ))
        .unwrap_err()
        .to_string()
        .contains("GOAL_STALE")
    );
}

#[test]
fn interruption_and_terminal_state_cannot_be_reinterpreted_as_success() {
    case("interrupted_and_dropped");
    let mut interrupted = record();
    interrupted.in_flight = true;
    let dropped = transition(record(), Transition::Drop("stopped")).unwrap();
    for (record, code) in [
        (interrupted, "GOAL_INTERRUPTED"),
        (dropped, "GOAL_TERMINAL"),
    ] {
        let mut agent = fixture(stored(&record), false);
        let input = request(
            &agent,
            json!({"kind":"complete","evidence":"External claim"}),
        );
        let before = history(&agent);
        assert!(
            run(rpc_goal_command(
                &mut agent,
                &AgentCx::for_request(),
                &input
            ))
            .unwrap_err()
            .to_string()
            .contains(code)
        );
        assert_eq!(history(&agent), before);
    }
}

#[test]
fn complete_then_save_persists_one_checkpoint_and_reopens_with_the_same_receipt() {
    case("native_save_and_reopen");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("goal.jsonl");
    let mut session = stored(&record());
    session.path = Some(path.clone());
    session.session_dir = Some(directory.path().to_path_buf());
    let mut agent = fixture(session, true);
    run(async {
        let owner = AgentCx::for_current_or_request();
        let input = request(
            &agent,
            json!({"kind":"complete","evidence":"Inspected actual output"}),
        );
        let completed = rpc_goal_command(&mut agent, &owner, &input).await.unwrap();
        assert_eq!(completed["persistence"], "saved");
        let before = history(&agent);
        let input = request(&agent, json!({"kind":"save"}));
        let saved = rpc_goal_command(&mut agent, &owner, &input).await.unwrap();
        assert_eq!(saved["persistence"], "saved");
        assert_eq!(history(&agent), before);
        assert_eq!(saved["revision"], completed["revision"]);
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(
            rpc_goal_state(&reopened).unwrap()["revision"],
            completed["revision"]
        );
        assert_eq!(reopened.entries.len(), 2);
        assert_eq!(
            latest(&reopened).unwrap().unwrap().1.status,
            GoalStatus::Complete
        );
    });
}

#[test]
fn failed_completion_save_can_be_retried_without_replaying_the_decision() {
    case("unconfirmed_then_save_only");
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    let path = blocked.join("goal.jsonl");
    let mut session = stored(&record());
    session.path = Some(path.clone());
    session.session_dir = Some(blocked.clone());
    let mut agent = fixture(session, true);
    run(async {
        let owner = AgentCx::for_current_or_request();
        let input = request(
            &agent,
            json!({"kind":"complete","evidence":"Already inspected"}),
        );
        let completed = rpc_goal_command(&mut agent, &owner, &input).await.unwrap();
        assert_eq!(completed["goal"]["status"], "complete");
        assert_eq!(completed["persistence"], "unconfirmed");
        assert!(completed["persistenceError"].is_string());
        let before = history(&agent);
        std::fs::rename(&blocked, directory.path().join("preserved-blocker")).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let retry = request(&agent, json!({"kind":"save"}));
        let saved = rpc_goal_command(&mut agent, &owner, &retry).await.unwrap();
        assert_eq!(saved["persistence"], "saved");
        assert!(saved.get("persistenceError").is_none());
        assert_eq!(saved["revision"], completed["revision"]);
        assert_eq!(history(&agent), before);
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(reopened.entries.len(), 2);
        assert_eq!(
            rpc_goal_state(&reopened).unwrap()["goal"]["note"],
            "Already inspected"
        );
    });
}

#[test]
fn cancelled_owner_and_contended_journal_refuse_without_a_partial_decision() {
    case("owner_and_lock_admission");
    let mut agent = fixture(stored(&record()), false);
    let input = request(&agent, json!({"kind":"complete","evidence":"Reviewed"}));
    let before = history(&agent);
    let owner = AgentCx::for_request();
    owner.set_cancel_requested(true);
    assert!(run(rpc_goal_command(&mut agent, &owner, &input)).is_err());
    assert_eq!(history(&agent), before);
    let store = Arc::clone(&agent.session);
    let locked = store.try_lock_owned().unwrap();
    assert!(
        run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &input
        ))
        .unwrap_err()
        .to_string()
        .contains("GOAL_BUSY")
    );
    drop(locked);
    assert_eq!(history(&agent), before);
}

#[test]
fn missing_or_corrupt_goal_never_falls_back_to_a_prior_checkpoint() {
    case("missing_and_corrupt");
    let mut empty = fixture(Session::in_memory(), false);
    let input = request(&empty, json!({"kind":"save"}));
    assert!(
        run(rpc_goal_command(
            &mut empty,
            &AgentCx::for_request(),
            &input
        ))
        .unwrap_err()
        .to_string()
        .contains("GOAL_MISSING")
    );
    assert!(empty.session.try_lock().unwrap().entries.is_empty());
    let mut agent = fixture(stored(&record()), false);
    let input = request(&agent, json!({"kind":"complete","evidence":"Reviewed"}));
    agent.session.try_lock().unwrap().append_custom_entry(
        CHECKPOINT_TYPE.to_owned(),
        Some(json!({"schema":"unknown"})),
    );
    let before = history(&agent);
    assert!(
        run(rpc_goal_command(
            &mut agent,
            &AgentCx::for_request(),
            &input
        ))
        .unwrap_err()
        .to_string()
        .contains("GOAL_CHECKPOINT")
    );
    assert_eq!(history(&agent), before);
}

#[test]
fn original_pause_and_budget_revision_keep_their_existing_contract() {
    case("existing_wire_contract");
    let mut agent = fixture(stored(&record()), false);
    run(async {
        let owner = AgentCx::for_current_or_request();
        let paused = rpc_goal_command(&mut agent, &owner, &json!({"op":"pause"}))
            .await
            .unwrap();
        assert_eq!(paused["goal"]["status"], "paused");
        assert!(paused.get("resolution").is_none());
        let input = json!({"op":"pause","revision":paused["revision"],
            "budget":{"max_prompts":8,"token_budget":400},"reason":"Reviewed remaining work"});
        let revised = rpc_goal_command(&mut agent, &owner, &input).await.unwrap();
        assert_eq!(revised["goal"]["status"], "paused");
        assert_eq!(revised["goal"]["maxPrompts"], 8);
        assert_eq!(revised["goal"]["tokensUsed"], 60);
        assert!(revised.get("resolution").is_none());
    });
}
