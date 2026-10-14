//! Native journal and SDK coverage for explicit total-budget revisions.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{AssistantMessage, ContentBlock, TextContent};
use crate::providers::openai::OpenAIProvider;
use crate::sdk::EventListeners;
use crate::tools::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};

fn execute<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build().unwrap().block_on(future)
}

fn spent() -> Record {
    let mut record = Record::new("Finish the migration", GoalBudget {
        max_prompts: 2, token_budget: 80,
    }).unwrap();
    record.prompts_started = 2;
    record.tokens_spent = 80;
    record
}

fn larger() -> GoalBudget {
    GoalBudget { max_prompts: 4, token_budget: 160 }
}

fn fixture(session: Session, persistent: bool) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let provider = Arc::new(OpenAIProvider::new("goal-budget-fixture")
        .with_base_url("http://127.0.0.1:1/v1"));
    let agent = Agent::new(provider,
        ToolRegistry::new(&[], std::path::Path::new("."), None), AgentConfig::default());
    let session = AgentSession::new(agent, Arc::new(Store::new(session)), persistent,
        ResolvedCompactionSettings::default());
    let events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&events);
    let listeners = EventListeners::default();
    let _ = listeners.subscribe(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    (AgentSessionHandle::from_session_with_listeners(session, listeners), events)
}

fn stored(record: &Record) -> Session {
    let mut session = Session::in_memory();
    append(&mut session, record).unwrap();
    session
}

fn current(handle: &AgentSessionHandle) -> GoalSnapshot {
    handle.current_goal().unwrap().unwrap()
}

fn usage(session: &mut Session, tokens: u64) {
    session.append_message(SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("progress"))],
            usage: Usage { total_tokens: tokens, ..Usage::default() },
            ..AssistantMessage::default()
        },
    });
}

#[test]
fn larger_totals_preserve_identity_spend_and_require_explicit_resume() {
    let original = spent();
    let revised = transition(original.clone(), Transition::Budget {
        budget: larger(), reason: "Reviewed the remaining migration work",
    }).unwrap();
    assert_eq!(revised.id, original.id);
    assert_eq!(revised.objective, original.objective);
    assert_eq!(revised.prompts_started, 2);
    assert_eq!(revised.tokens_spent, 80);
    assert_eq!(revised.effective_status(), GoalStatus::Paused);
    assert_eq!(revised.note, "Reviewed the remaining migration work");
    let resumed = transition(revised, Transition::Resume).unwrap();
    assert_eq!(resumed.effective_status(), GoalStatus::Active);
    assert_eq!(resumed.prompts_started, 2);
    assert_eq!(resumed.tokens_spent, 80);
}

#[test]
fn reducing_a_total_below_usage_does_not_fabricate_remaining_budget() {
    let revised = transition(spent(), Transition::Budget {
        budget: GoalBudget { max_prompts: 4, token_budget: 40 }, reason: "Reduce allowance",
    }).unwrap();
    assert_eq!(revised.tokens_spent, 80);
    assert_eq!(revised.effective_status(), GoalStatus::BudgetLimited);
    assert!(transition(revised, Transition::Resume).is_err());
    let mut saturated = spent();
    saturated.tokens_spent = u64::MAX;
    let revised = transition(saturated, Transition::Budget {
        budget: GoalBudget { max_prompts: 4, token_budget: u64::MAX }, reason: "Review saturation",
    }).unwrap();
    assert_eq!(revised.tokens_spent, u64::MAX);
    assert_eq!(revised.effective_status(), GoalStatus::BudgetLimited);
}

#[test]
fn invalid_or_unchanged_limits_and_missing_reasons_are_refused() {
    for budget in [
        GoalBudget { max_prompts: 0, token_budget: 160 },
        GoalBudget { max_prompts: 1025, token_budget: 160 },
        GoalBudget { max_prompts: 4, token_budget: 0 },
        GoalBudget { max_prompts: 1, token_budget: 160 },
        GoalBudget { max_prompts: 2, token_budget: 80 },
    ] {
        assert!(transition(spent(), Transition::Budget { budget, reason: "reviewed" }).is_err());
    }
    for reason in [String::new(), " \n".into(), "x".repeat(MAX_TEXT_BYTES + 1)] {
        assert!(transition(spent(), Transition::Budget { budget: larger(), reason: &reason }).is_err());
    }
}

#[test]
fn changing_an_interrupted_budget_cannot_acknowledge_unfinished_work() {
    let mut original = spent();
    original.in_flight = true;
    let revised = transition(original, Transition::Budget {
        budget: larger(), reason: "Authorize more allowance, not replay",
    }).unwrap();
    assert!(revised.in_flight);
    assert_eq!(revised.status, GoalStatus::Active); // Required by the checkpoint schema.
    assert_eq!(revised.effective_status(), GoalStatus::Paused);
    assert!(transition(revised.clone(), Transition::Complete("not reconciled")).is_err());
    let decoded = decode(&serde_json::to_value(&revised).unwrap()).unwrap();
    assert!(decoded.in_flight);
    assert_eq!(decoded.tokens_spent, 80);
    let resumed = transition(decoded, Transition::Resume).unwrap();
    assert!(!resumed.in_flight);
    assert_eq!(resumed.prompts_started, 2);
}

#[test]
fn finished_goals_cannot_be_reopened_by_a_budget_change() {
    for action in [Transition::Complete("verified"), Transition::Drop("cancelled objective")] {
        let ended = transition(spent(), action).unwrap();
        let error = transition(ended, Transition::Budget { budget: larger(), reason: "more work" })
            .err().expect("terminal goal must reject revised limits");
        assert!(error.to_string().contains("GOAL_TERMINAL"));
    }
}

#[test]
fn sdk_revisions_append_history_invalidate_old_snapshots_and_emit_no_agent_work() {
    let original = spent();
    let (mut handle, events) = fixture(stored(&original), false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let before = current(&handle);
        let changed = handle.set_goal_budget(&owner, &before, larger(), "User reviewed scope").await.unwrap();
        assert_eq!(changed.persistence, GoalPersistence::MemoryOnly);
        assert_eq!(changed.goal.status(), GoalStatus::Paused);
        assert_eq!(changed.goal.remaining_tokens(), 80);
        assert_eq!(changed.goal.id(), before.id());
        assert_ne!(changed.goal.checkpoint_id, before.checkpoint_id);
        let error = handle.set_goal_budget(&owner, &before,
            GoalBudget { max_prompts: 5, token_budget: 200 }, "stale decision").await.unwrap_err();
        assert!(error.to_string().contains("GOAL_STALE"));
        let store = handle.session_store();
        let journal = store.try_lock().unwrap();
        let SessionEntry::Custom(previous) = journal.get_entry(&before.checkpoint_id).unwrap() else {
            panic!("previous goal checkpoint retained");
        };
        assert_eq!(previous.data.as_ref().unwrap()["tokenBudget"], 80);
        assert_eq!(latest(&journal).unwrap().unwrap().1.token_budget, 160);
    });
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn sdk_accounts_active_usage_once_and_pauses_unrelated_accounting() {
    let mut record = spent();
    record.tokens_spent = 55;
    let mut session = stored(&record);
    usage(&mut session, 25);
    let (mut handle, _) = fixture(session, false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let before = current(&handle);
        assert_eq!(before.tokens_spent(), 80);
        let changed = handle.set_goal_budget(&owner, &before, larger(), "Reviewed recorded usage").await.unwrap();
        assert_eq!(changed.goal.tokens_spent(), 80);
        assert_eq!(current(&handle).tokens_spent(), 80);
        assert_eq!(current(&handle).tokens_spent(), 80);
        usage(&mut handle.session_store().try_lock().unwrap(), 999);
        assert_eq!(current(&handle).tokens_spent(), 80);
        assert!(handle.resume_goal(&owner, &changed.goal).await.unwrap_err().to_string().contains("GOAL_STALE"));
        let reviewed = current(&handle);
        handle.resume_goal(&owner, &reviewed).await.unwrap();
        usage(&mut handle.session_store().try_lock().unwrap(), 10);
        assert_eq!(current(&handle).tokens_spent(), 90);
    });
}

#[test]
fn sdk_rejects_cancelled_contended_and_wrong_store_decisions_without_mutation() {
    let record = spent();
    let session = stored(&record);
    let mut other_session = stored(&record);
    other_session.header.id.clone_from(&session.header.id);
    let (mut other, _) = fixture(other_session, false);
    let (mut handle, _) = fixture(session, false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let before = current(&handle);
        let error = other.set_goal_budget(&owner, &before, larger(), "wrong store").await.unwrap_err();
        assert!(error.to_string().contains("GOAL_STALE"));
        let cancelled = AgentCx::for_request();
        cancelled.cancel_with(asupersync::types::CancelKind::User, Some("cancel budget edit"));
        let error = handle.set_goal_budget(&cancelled, &before, larger(), "cancelled").await.unwrap_err();
        assert!(error.to_string().contains("GOAL_CANCELLED"));
        let store = handle.session_store();
        let held = store.try_lock().unwrap();
        let error = handle.set_goal_budget(&owner, &before, larger(), "contended").await.unwrap_err();
        assert!(error.to_string().contains("GOAL_BUSY"));
        drop(held);
        assert_eq!(current(&handle).checkpoint_id, before.checkpoint_id);
        assert_eq!(current(&other).budget(), before.budget());
    });
}

#[test]
fn sdk_branch_changes_refuse_a_budget_decision_for_the_previously_reviewed_goal() {
    let mut session = Session::in_memory();
    let root = session.append_custom_entry("root".into(), None);
    append(&mut session, &spent()).unwrap();
    let (mut handle, _) = fixture(session, false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let old = current(&handle);
        let store = handle.session_store();
        {
            let mut journal = store.try_lock().unwrap();
            assert!(journal.navigate_to(&root));
            let replacement = Record::new("Other branch", GoalBudget::default()).unwrap();
            append(&mut journal, &replacement).unwrap();
        }
        let before = current(&handle);
        assert!(handle.set_goal_budget(&owner, &old, larger(), "old branch").await.unwrap_err()
            .to_string().contains("GOAL_STALE"));
        assert_eq!(current(&handle).checkpoint_id, before.checkpoint_id);
        assert_eq!(current(&handle).objective(), "Other branch");
    });
}

#[test]
fn failed_budget_save_can_be_flushed_and_reopened_without_replaying_the_revision() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = stored(&spent());
    session.path = Some(temp.path().to_path_buf()); // Native save to a directory must fail.
    session.session_dir = Some(temp.path().to_path_buf());
    let (mut handle, _) = fixture(session, true);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let before = current(&handle);
        let changed = handle.set_goal_budget(&owner, &before, larger(), "Reviewed recovery work").await.unwrap();
        assert!(matches!(changed.persistence, GoalPersistence::Unconfirmed { .. }));
        assert_eq!(changed.goal.budget(), larger());
        assert_eq!(changed.goal.status(), GoalStatus::Paused);
        let path = temp.path().join("goal.jsonl");
        handle.session_store().try_lock().unwrap().path = Some(path.clone());
        assert_eq!(handle.flush_goal(&owner).await.unwrap(), GoalPersistence::Saved);
        assert_eq!(current(&handle).checkpoint_id, changed.goal.checkpoint_id);
        let loaded = Session::open(path.to_str().unwrap()).await.unwrap();
        let (reopened, _) = fixture(loaded, true);
        let restored = current(&reopened);
        assert_eq!(restored.id(), before.id());
        assert_eq!(restored.budget(), larger());
        assert_eq!(restored.prompts_started(), 2);
        assert_eq!(restored.tokens_spent(), 80);
        assert_eq!(restored.status(), GoalStatus::Paused);
        assert_eq!(restored.note(), "Reviewed recovery work");
    });
}
