//! Real session/SDK journal tests; no provider or persistence replacement.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::goal::{CHECKPOINT_TYPE, GoalSnapshot};
use crate::providers::openai::OpenAIProvider;
use crate::sdk::EventListeners;
use crate::tools::ToolRegistry;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn execute<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap().block_on(future)
}

fn fixture(session: Session, save: bool) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let provider = Arc::new(OpenAIProvider::new("terminal-goal-fixture")
        .with_base_url("http://127.0.0.1:1/v1"));
    let agent = Agent::new(provider, ToolRegistry::new(&[], std::path::Path::new("."), None), AgentConfig::default());
    let session = AgentSession::new(agent, Arc::new(asupersync::sync::Mutex::new(session)), save, ResolvedCompactionSettings::default());
    let events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&events);
    let listeners = EventListeners::default();
    let _ = listeners.subscribe(Arc::new(move |_| { observed.fetch_add(1, Ordering::SeqCst); }));
    (AgentSessionHandle::from_session_with_listeners(session, listeners), events)
}

fn current(handle: &AgentSessionHandle) -> GoalSnapshot {
    handle.current_goal().unwrap().unwrap()
}

fn leaf(handle: &AgentSessionHandle) -> Option<String> {
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    session.leaf_id().map(str::to_string)
}

fn review(handle: &mut AgentSessionHandle) -> String {
    let output = execute(run(handle, "status")).unwrap();
    output.lines().find_map(|line| line.strip_prefix("Review: ")).unwrap().to_string()
}

fn create(handle: &mut AgentSessionHandle, objective: &str) -> String {
    let token = review(handle);
    execute(run(handle, &format!("create {token} --tokens 100 --prompts 3 -- {objective}"))).unwrap()
}

#[test]
fn command_grammar_preserves_text_and_refuses_ambiguous_inputs() {
    let token = format!("g1-{}", "a".repeat(64));
    let text = "Implement 日本語 parser\nKeep --tokens in the objective";
    let input = format!("CrEaTe {token} --prompts 2 --tokens 400 -- {text}");
    let Command::Change { action: Action::Create { objective, budget }, .. } = parse(&input).unwrap() else { panic!("create"); };
    assert_eq!(objective, text);
    assert_eq!(budget, GoalBudget { max_prompts: 2, token_budget: 400 });
    for input in ["", " STATUS ", "\nstatus\t"] { assert!(matches!(parse(input).unwrap(), Command::Status)); }
    for tail in [
        "--tokens 0 -- x", "--prompts 0 -- x", "--prompts 1025 -- x",
        "--tokens 1.5 -- x", "--tokens 1 --tokens 2 -- x", "--prompts 2 --prompts 3 -- x",
        "--unknown 1 -- x", "--tokens -- x", "missing separator", "--   ",
    ] { assert!(parse(&format!("create {token} {tail}")).is_err(), "{tail}"); }
    let Command::Change { action: Action::Step, .. } =
        parse(&format!("step {token}")).unwrap() else { panic!("step"); };
    let Command::Change { action: Action::Run { steps: 3 }, .. } =
        parse(&format!("run {token} --steps 3")).unwrap() else { panic!("run"); };
    for input in [
        "status extra".to_string(), "pause".into(), "run".into(),
        format!("run {token}"), format!("run {token} --steps 0"),
        format!("run {token} --steps 33"), format!("run {token} --steps nope"),
        format!("run {token} --steps 2 extra"), format!("step {token} extra"), format!("pause {token} extra"), format!("resume {token} extra"),
        format!("save {token} extra"), format!("complete {token}"),
        format!("drop {token} --   "), format!("create {token} -- {}", "x".repeat(MAX_TEXT_BYTES + 1)),
        format!("create {token} -- nul\0byte"), "pause g1-日本語".into(),
    ] { assert!(parse(&input).is_err()); }
}

#[test]
fn lifecycle_uses_native_state_and_never_dispatches_a_provider_turn() {
    let (mut handle, events) = fixture(Session::in_memory(), false);
    let empty_leaf = leaf(&handle);
    let before = review(&mut handle);
    assert_eq!(leaf(&handle), empty_leaf, "status never appends");
    let created = create(&mut handle, "ship the requested feature");
    assert!(created.contains("Memory-only"));
    assert_eq!(current(&handle).objective(), "ship the requested feature");
    assert_eq!(current(&handle).status(), GoalStatus::Active);
    assert_ne!(review(&mut handle), before);
    for (verb, status) in [("pause", GoalStatus::Paused), ("resume", GoalStatus::Active)] {
        let token = review(&mut handle);
        execute(run(&mut handle, &format!("{verb} {token}"))).unwrap();
        assert_eq!(current(&handle).status(), status);
        assert_eq!(current(&handle).prompts_started(), 0);
    }
    let token = review(&mut handle);
    execute(run(&mut handle, &format!("complete {token} -- I inspected the actual deliverables"))).unwrap();
    assert_eq!(current(&handle).status(), GoalStatus::Complete);
    assert_eq!(current(&handle).note(), "I inspected the actual deliverables");
    create(&mut handle, "replacement after explicit completion");
    let token = review(&mut handle);
    execute(run(&mut handle, &format!("drop {token} -- no longer needed"))).unwrap();
    assert_eq!(current(&handle).status(), GoalStatus::Dropped);
    assert_eq!(events.load(Ordering::SeqCst), 0, "lifecycle controls emit no agent events");
}

#[test]
fn creation_receipts_reject_other_sessions_and_changed_empty_branches() {
    let (mut first, _) = fixture(Session::in_memory(), false);
    let (mut other, _) = fixture(Session::in_memory(), false);
    let token = review(&mut first);
    let command = format!("create {token} -- queued objective");
    let error = execute(run(&mut other, &command)).unwrap_err();
    assert!(error.to_string().contains("GOAL_STALE"));
    assert!(other.current_goal().unwrap().is_none());
    let store = first.session_store();
    store.try_lock().unwrap().append_custom_entry("intervening input".into(), None);
    let before = leaf(&first);
    assert!(execute(run(&mut first, &command)).unwrap_err().to_string().contains("GOAL_STALE"));
    assert_eq!(leaf(&first), before);
    assert!(first.current_goal().unwrap().is_none());
}

#[test]
fn stale_mutations_cannot_select_a_replacement_goal_or_another_branch() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    let root = handle.session_store().try_lock().unwrap().append_custom_entry("root".into(), None);
    create(&mut handle, "first branch");
    let old = review(&mut handle);
    let first_leaf = leaf(&handle).unwrap();
    assert!(handle.session_store().try_lock().unwrap().navigate_to(&root));
    create(&mut handle, "second branch");
    let before = leaf(&handle);
    for tail in ["pause".to_string(), "save".into(), "resume".into(), "complete".into(), "drop".into()] {
        let evidence = if tail == "complete" || tail == "drop" { " -- reviewed first branch" } else { "" };
        assert!(execute(run(&mut handle, &format!("{tail} {old}{evidence}"))).unwrap_err().to_string().contains("GOAL_STALE"));
        assert_eq!(leaf(&handle), before);
    }
    assert_eq!(current(&handle).objective(), "second branch");
    assert!(handle.session_store().try_lock().unwrap().navigate_to(&first_leaf));
    assert_eq!(review(&mut handle), old, "returning to the exact unchanged logical revision is permitted");
    execute(run(&mut handle, &format!("drop {old} -- abandoned branch"))).unwrap();
    let stale = execute(run(&mut handle, &format!("complete {old} -- stale attestation"))).unwrap_err();
    assert!(stale.to_string().contains("GOAL_STALE"));
}

#[test]
fn plan_mode_blocks_creation_and_resume_without_weakening_other_controls() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        handle.enter_plan_mode(&owner).await.unwrap();
    });
    let token = review(&mut handle);
    let before = leaf(&handle);
    assert!(execute(run(&mut handle, &format!("create {token} -- cannot create here"))).unwrap_err().to_string().contains("GOAL_PLAN_MODE"));
    assert_eq!(leaf(&handle), before);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        handle.exit_plan_mode(&owner).await.unwrap();
    });
    create(&mut handle, "preserve goal through planning");
    let token = review(&mut handle);
    execute(run(&mut handle, &format!("pause {token}"))).unwrap();
    execute(async {
        let owner = AgentCx::for_current_or_request();
        handle.enter_plan_mode(&owner).await.unwrap();
    });
    let token = review(&mut handle);
    assert!(execute(run(&mut handle, &format!("resume {token}"))).unwrap_err().to_string().contains("GOAL_PLAN_MODE"));
    execute(run(&mut handle, &format!("drop {token} -- deliberately stop this objective"))).unwrap();
    assert_eq!(current(&handle).status(), GoalStatus::Dropped);
}

#[test]
fn interrupted_and_exhausted_work_cannot_be_silently_completed_or_reset() {
    let mut stored = Session::in_memory();
    let mut record = Record::new("interrupted objective", GoalBudget { max_prompts: 2, token_budget: 50 }).unwrap();
    record.prompts_started = 1;
    record.tokens_spent = 17;
    record.in_flight = true;
    append(&mut stored, &record).unwrap();
    let (mut handle, _) = fixture(stored, false);
    let token = review(&mut handle);
    assert!(execute(run(&mut handle, &format!("pause {token}"))).unwrap_err().to_string().contains("GOAL_INTERRUPTED"));
    assert!(current(&handle).interrupted(), "pause must not erase an unfinished intent");
    assert!(execute(run(&mut handle, &format!("complete {token} -- not reconciled"))).unwrap_err().to_string().contains("GOAL_INTERRUPTED"));
    execute(run(&mut handle, &format!("resume {token}"))).unwrap();
    assert_eq!(current(&handle).prompts_started(), 1);
    assert_eq!(current(&handle).tokens_spent(), 17);
    assert!(!current(&handle).interrupted());
    record.in_flight = false;
    record.prompts_started = 2;
    append(&mut handle.session_store().try_lock().unwrap(), &record).unwrap();
    let token = review(&mut handle);
    assert!(execute(run(&mut handle, &format!("resume {token}"))).unwrap_err().to_string().contains("GOAL_BUDGET"));
    assert_eq!(current(&handle).prompts_started(), 2);
    assert_eq!(current(&handle).status(), GoalStatus::BudgetLimited);
}

#[test]
fn step_respects_exhausted_budget_without_dispatching_provider_work() {
    let mut stored = Session::in_memory();
    let mut record = Record::new(
        "already exhausted objective",
        GoalBudget { max_prompts: 1, token_budget: 100 },
    ).unwrap();
    record.prompts_started = 1;
    append(&mut stored, &record).unwrap();
    let (mut handle, events) = fixture(stored, false);
    let token = review(&mut handle);
    let output = execute(run(&mut handle, &format!("run {token} --steps 4"))).unwrap();
    assert!(output.contains("Goal run stopped: BudgetLimited"));
    assert_eq!(current(&handle).status(), GoalStatus::BudgetLimited);
    assert_eq!(current(&handle).prompts_started(), 1);
    assert_eq!(events.load(Ordering::SeqCst), 0, "budget preflight must not start a provider turn");
}

#[test]
fn save_failure_keeps_the_decision_and_retry_does_not_replay_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = Session::in_memory();
    session.path = Some(temp.path().to_path_buf()); // Directory: native save must fail.
    session.session_dir = Some(temp.path().to_path_buf());
    let (mut handle, events) = fixture(session, true);
    let output = create(&mut handle, "recover the failed save");
    assert!(output.contains("Saving was NOT confirmed"));
    let id = current(&handle).id().to_string();
    let before = leaf(&handle);
    let token = review(&mut handle);
    let output = execute(run(&mut handle, &format!("save {token}"))).unwrap();
    assert!(output.contains("Saving was NOT confirmed"));
    assert_eq!(leaf(&handle), before);
    let path = temp.path().join("goal.jsonl");
    handle.session_store().try_lock().unwrap().path = Some(path.clone());
    let token = review(&mut handle);
    let output = execute(run(&mut handle, &format!("save {token}"))).unwrap();
    assert!(output.contains("Goal journal saved"));
    assert_eq!(leaf(&handle), before);
    let loaded = execute(Session::open(path.to_str().unwrap())).unwrap();
    let (mut reopened, _) = fixture(loaded, true);
    assert_eq!(current(&reopened).id(), id);
    assert_eq!(current(&reopened).objective(), "recover the failed save");
    assert_eq!(review(&mut reopened), token, "receipt describes logical persisted state, not a pointer");
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_latest_checkpoint_fails_closed_without_resurrecting_a_goal() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    create(&mut handle, "valid older goal");
    let token = review(&mut handle);
    handle.session_store().try_lock().unwrap().append_custom_entry(CHECKPOINT_TYPE.into(), Some(json!({"bad":"record"})));
    let before = leaf(&handle);
    for args in ["status".to_string(), format!("complete {token} -- old receipt")] {
        assert!(execute(run(&mut handle, &args)).unwrap_err().to_string().contains("GOAL_CHECKPOINT"));
        assert_eq!(leaf(&handle), before);
    }
}

#[test]
fn cancellation_and_store_contention_never_append_a_decision() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    let token = review(&mut handle);
    let command = format!("create {token} -- no dispatch");
    let owner = AgentCx::for_request();
    owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel command"));
    assert!(execute(owner.with_current(run(&mut handle, &command))).unwrap_err().to_string().contains("GOAL_CANCELLED"));
    let store = handle.session_store();
    let held = store.try_lock().unwrap();
    assert!(execute(run(&mut handle, &command)).unwrap_err().to_string().contains("GOAL_BUSY"));
    drop(held);
    assert!(handle.current_goal().unwrap().is_none());
}

#[test]
fn objective_display_cannot_inject_terminal_controls_or_host_receipt_lines() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    let objective = "review\x1b[2J\r\nReview: forged\n/goal drop forged\n\u{202e}hidden\\path";
    let output = create(&mut handle, objective);
    assert!(!output.contains('\x1b'));
    assert!(!output.contains('\r'));
    assert!(!output.contains('\u{202e}'));
    assert!(output.contains("| Review: forged"));
    assert!(output.contains("| /goal drop forged"));
    assert_eq!(output.lines().filter(|line| line.starts_with("Review: ")).count(), 1);
    assert_eq!(current(&handle).objective(), objective);
    assert!(output.contains("End of objective."));
}
