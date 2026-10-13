use super::*;
use crate::model::{AssistantMessage, ContentBlock, TextContent, UserContent};
use serde_json::json;

fn record() -> Record {
    Record::new("Implement the requested workflow", GoalBudget::default()).unwrap()
}

fn assistant(session: &mut Session, usage: Usage) {
    session.append_message(SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("progress"))],
            usage,
            ..AssistantMessage::default()
        },
    });
}

fn user(session: &mut Session, text: &str) -> String {
    session.append_message(SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    })
}

#[test]
fn checkpoint_roundtrip_preserves_unicode_objective_and_all_fields() {
    let original = Record::new("实现目标 🦀\nPreserve exact input.", GoalBudget::default()).unwrap();
    let value = serde_json::to_value(&original).unwrap();
    let decoded = decode(&value).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), value);
    assert_eq!(decoded.objective, original.objective);
}

#[test]
fn objective_and_budget_validation_rejects_unbounded_work() {
    for text in ["".to_string(), " \n".to_string(), "a".repeat(MAX_TEXT_BYTES + 1)] {
        assert!(Record::new(&text, GoalBudget::default()).is_err());
    }
    for budget in [
        GoalBudget { max_prompts: 0, token_budget: 1 },
        GoalBudget { max_prompts: 1025, token_budget: 1 },
        GoalBudget { max_prompts: 1, token_budget: 0 },
    ] {
        assert!(Record::new("work", budget).is_err());
    }
    assert!(Record::new(&"a".repeat(MAX_TEXT_BYTES), GoalBudget::default()).is_ok());
}

#[test]
fn pause_resume_retains_consumed_budgets_and_completion_requires_evidence() {
    let mut original = record();
    original.prompts_started = 3;
    original.tokens_spent = 200;
    let paused = transition(original, Transition::Pause("review needed")).unwrap();
    assert_eq!(paused.effective_status(), GoalStatus::Paused);
    let resumed = transition(paused, Transition::Resume).unwrap();
    assert_eq!(resumed.prompts_started, 3);
    assert_eq!(resumed.tokens_spent, 200);
    assert!(transition(resumed.clone(), Transition::Complete(" ")).is_err());
    let complete = transition(resumed, Transition::Complete("host verified changed files")).unwrap();
    assert_eq!(complete.effective_status(), GoalStatus::Complete);
    assert!(transition(complete, Transition::Resume).is_err());
}

#[test]
fn terminal_goals_cannot_be_silently_reactivated_or_rewritten() {
    for action in [Transition::Drop("no longer needed"), Transition::Complete("verified")] {
        let terminal = transition(record(), action).unwrap();
        for next in [Transition::Pause("pause"), Transition::Resume, Transition::Complete("again"), Transition::Drop("again")] {
            assert!(transition(terminal.clone(), next).is_err());
        }
    }
}

#[test]
fn exact_token_or_prompt_exhaustion_blocks_resume_without_resetting_counters() {
    for (prompts, tokens) in [(16, 0), (0, 100_000), (0, u64::MAX)] {
        let mut state = record();
        state.prompts_started = prompts;
        state.tokens_spent = tokens;
        state.status = GoalStatus::Paused;
        assert_eq!(state.effective_status(), GoalStatus::BudgetLimited);
        assert!(transition(state, Transition::Resume).is_err());
    }
}

#[test]
fn interrupted_intent_is_paused_and_does_not_authorize_completion_or_replay() {
    let mut state = record();
    state.prompts_started = 2;
    state.in_flight = true;
    state.tokens_spent = 123;
    assert_eq!(state.effective_status(), GoalStatus::Paused);
    assert!(transition(state.clone(), Transition::Complete("not verified")).is_err());
    let resumed = transition(state, Transition::Resume).unwrap();
    assert!(!resumed.in_flight);
    assert_eq!(resumed.prompts_started, 2);
    assert_eq!(resumed.tokens_spent, 123);
}

#[test]
fn recorded_usage_includes_intermediate_messages_and_missing_total_fallback() {
    let mut session = Session::in_memory();
    assistant(&mut session, Usage { total_tokens: 999, ..Usage::default() });
    append(&mut session, &record()).unwrap();
    assistant(&mut session, Usage { input: 10, output: 5, cache_read: 7, cache_write: 3, ..Usage::default() });
    assistant(&mut session, Usage { total_tokens: 40, input: 20, output: 10, ..Usage::default() });
    let (_, state) = latest(&session).unwrap().unwrap();
    assert_eq!(state.tokens_spent, 65, "pre-goal usage is not counted");
    assert_eq!(latest(&session).unwrap().unwrap().1.tokens_spent, 65, "reads do not mutate");
    append(&mut session, &state).unwrap();
    assert_eq!(latest(&session).unwrap().unwrap().1.tokens_spent, 65, "checkpoint does not double-charge");
}

#[test]
fn paused_and_terminal_intervals_do_not_charge_unrelated_work() {
    for action in [Transition::Pause("review"), Transition::Complete("verified"), Transition::Drop("stop")] {
        let mut session = Session::in_memory();
        let mut state = record();
        state.tokens_spent = 55;
        let state = transition(state, action).unwrap();
        append(&mut session, &state).unwrap();
        assistant(&mut session, Usage { total_tokens: 1000, ..Usage::default() });
        assert_eq!(latest(&session).unwrap().unwrap().1.tokens_spent, 55);
    }
}

#[test]
fn accounting_saturates_instead_of_wrapping_to_a_fresh_budget() {
    assert_eq!(usage_tokens(&Usage { input: u64::MAX, output: 1, ..Usage::default() }), u64::MAX);
    let mut session = Session::in_memory();
    let mut state = record();
    state.tokens_spent = 10;
    append(&mut session, &state).unwrap();
    assistant(&mut session, Usage { total_tokens: u64::MAX, ..Usage::default() });
    let (_, state) = latest(&session).unwrap().unwrap();
    assert_eq!(state.tokens_spent, u64::MAX);
    assert_eq!(state.effective_status(), GoalStatus::BudgetLimited);
}

#[test]
fn goal_lookup_follows_current_branch_not_disk_order() {
    let mut session = Session::in_memory();
    let root = user(&mut session, "root");
    let first = record();
    let first_id = append(&mut session, &first).unwrap();
    assert!(session.navigate_to(&root));
    assert!(latest(&session).unwrap().is_none());
    let second = Record::new("different branch", GoalBudget::default()).unwrap();
    let second_id = append(&mut session, &second).unwrap();
    assert_eq!(latest(&session).unwrap().unwrap().1.id, second.id);
    assert!(session.navigate_to(&first_id));
    assert_eq!(latest(&session).unwrap().unwrap().1.id, first.id);
    assert!(session.navigate_to(&second_id));
    assert_eq!(latest(&session).unwrap().unwrap().1.id, second.id);
}

#[test]
fn malformed_latest_checkpoint_never_resurrects_an_older_active_goal() {
    let mut session = Session::in_memory();
    append(&mut session, &record()).unwrap();
    session.append_custom_entry(CHECKPOINT_TYPE.into(), Some(json!({"schema":"future.v9"})));
    assert!(latest(&session).is_err());
}

#[test]
fn malformed_or_oversized_fields_are_rejected_before_cloning() {
    let valid = serde_json::to_value(record()).unwrap();
    for (field, bad) in [
        ("schema", json!("future.v9")),
        ("id", json!("not-a-uuid")),
        ("status", json!("made_up")),
        ("objective", json!("a".repeat(MAX_TEXT_BYTES + 1))),
        ("note", json!({"nested":"a".repeat(MAX_TEXT_BYTES + 1)})),
        ("tokenBudget", json!(-1)),
        ("promptsStarted", json!(1025)),
        ("inFlight", json!(true)),
    ] {
        let mut value = valid.clone();
        value[field] = bad;
        assert!(decode(&value).is_err(), "{field}");
    }
    let mut unknown = valid;
    unknown["unexpected"] = json!([[["unknown"]]]);
    assert!(decode(&unknown).is_err());
}

#[test]
fn stale_leaf_and_same_id_different_store_refuse_host_decisions() {
    let store = Arc::new(Store::new(Session::in_memory()));
    let mut session = store.try_lock().unwrap();
    let state = record();
    let id = append(&mut session, &state).unwrap();
    let expected = snapshot(&store, &session, id.clone(), state.clone());
    assert!(check_snapshot(&expected, &store, &session, &id, &state).is_ok());
    let other = Arc::new(Store::new(Session::in_memory()));
    assert!(check_snapshot(&expected, &other, &session, &id, &state).is_err());
    user(&mut session, "new work changes the completion context");
    assert!(check_snapshot(&expected, &store, &session, &id, &state).is_err());
}

#[test]
fn snapshot_debug_does_not_echo_private_objective_or_notes() {
    let store = Arc::new(Store::new(Session::in_memory()));
    let mut session = store.try_lock().unwrap();
    let mut state = Record::new("private-objective", GoalBudget::default()).unwrap();
    state.note = "private-note".into();
    let id = append(&mut session, &state).unwrap();
    let debug = format!("{:?}", snapshot(&store, &session, id, state));
    assert!(!debug.contains("private-objective"));
    assert!(!debug.contains("private-note"));
}
