//! Native lifecycle adapters, not replacement journals or provider simulations.
//! The terminal lane invokes its real parser/receipt/append path; no run control
//! may be published. RPC tests invoke the adapter behind the existing idle route.

use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::agent_cx::AgentCx;
use crate::compaction::ResolvedCompactionSettings;
use crate::error::Result;
use crate::goal::{GoalBudget, Record, Store, append, rpc_goal_command, rpc_goal_state};
#[cfg(feature = "ftui")]
use crate::goal::GoalStatus;
use crate::model::{AssistantMessage, StreamEvent, Usage};
use crate::provider::{Context, Provider, StreamOptions};
use crate::sdk::{AgentSessionHandle, EventListeners};
use crate::session::{Session, SessionMessage};
use crate::tools::ToolRegistry;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct NoProvider(AtomicUsize);

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for NoProvider {
    fn name(&self) -> &str { "goal-reconciliation-surfaces" }
    fn api(&self) -> &str { "goal-reconciliation-surfaces" }
    fn model_id(&self) -> &str { "provider-must-not-run" }

    async fn stream(
        &self,
        _: &Context<'_>,
        _: &StreamOptions,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(crate::error::Error::validation("lifecycle command requested provider work"))
    }
}

fn execute<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8).build().unwrap().block_on(future)
}

fn original(exhausted: bool) -> Record {
    let mut record = Record::new("Inspect interrupted migration", GoalBudget {
        max_prompts: if exhausted { 1 } else { 3 },
        token_budget: if exhausted { 7 } else { 100 },
    }).unwrap();
    record.prompts_started = 1;
    record.tokens_spent = 7;
    record.in_flight = true;
    record
}

fn stored(record: &Record) -> Session {
    let mut session = Session::in_memory();
    append(&mut session, record).unwrap();
    session
}

fn fixture(session: Session, save: bool) -> (AgentSessionHandle, Arc<NoProvider>) {
    let provider = Arc::new(NoProvider::default());
    let agent = Agent::new(provider.clone(), ToolRegistry::from_tools(Vec::new()), AgentConfig::default());
    let session = AgentSession::new(agent, Arc::new(Store::new(session)), save,
        ResolvedCompactionSettings::default());
    (AgentSessionHandle::from_session_with_listeners(session, EventListeners::default()), provider)
}

fn state(handle: &AgentSessionHandle) -> Value {
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    rpc_goal_state(&session).unwrap()
}

fn history(handle: &AgentSessionHandle) -> Value {
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    json!({"leaf":session.leaf_id(),"entries":session.entries})
}

fn request(handle: &AgentSessionHandle, outcome: &str) -> Value {
    json!({"type":"goal", "op":"pause", "revision":state(handle)["revision"],
        "resolution":{"kind":"reconcile","outcome":outcome,
            "evidence":"  Inspected 日本語 output and real side effects.\nNo replay.  "}})
}

fn log(case: &str) {
    eprintln!("{}", json!({"event":"goal_reconciliation_surface_test","case":case}));
}

#[test]
fn rpc_reconciles_all_outcomes_without_starting_or_resetting_work() {
    log("rpc_outcomes");
    for exhausted in [false, true] {
        for outcome in ["pause", "complete", "drop"] {
            let original = original(exhausted);
            let (mut handle, provider) = fixture(stored(&original), false);
            execute(async {
                let owner = AgentCx::for_current_or_request();
                let request = request(&handle, outcome);
                let result = rpc_goal_command(handle.session_mut(), &owner, &request).await.unwrap();
                assert_eq!(result["resolution"], json!({"kind":"reconcile","journalChanged":true}));
                assert_eq!(result["persistence"], "memory_only");
                let expected = match outcome {
                    "pause" if exhausted => "budget-limited",
                    "pause" => "paused",
                    "complete" => "complete",
                    _ => "dropped",
                };
                assert_eq!(result["goal"]["status"], expected);
                assert_eq!(result["state"]["enabled"], false);
                assert_eq!(result["goal"]["interrupted"], false);
                assert_eq!(result["goal"]["id"], original.id);
                assert_eq!(result["goal"]["objective"], original.objective);
                assert_eq!(result["goal"]["tokenBudget"], original.token_budget);
                assert_eq!(result["goal"]["maxPrompts"], original.max_prompts);
                assert_eq!(result["goal"]["tokensUsed"], 7);
                assert_eq!(result["goal"]["promptsStarted"], 1);
                assert_eq!(result["goal"]["note"], request["resolution"]["evidence"]);
                assert_ne!(result["revision"], request["revision"]);
                assert_eq!(history(&handle)["entries"].as_array().unwrap().len(), 2);
                assert!(rpc_goal_command(handle.session_mut(), &owner, &request).await.unwrap_err()
                    .to_string().contains("GOAL_STALE"));
                let fresh = request_for_revision(&request, &result["revision"]);
                let error = rpc_goal_command(handle.session_mut(), &owner, &fresh).await.unwrap_err();
                assert!(error.to_string().contains(if outcome == "pause" { "GOAL_NOT_INTERRUPTED" } else { "GOAL_TERMINAL" }));
            });
            assert_eq!(provider.0.load(Ordering::SeqCst), 0);
        }
    }
}

fn request_for_revision(request: &Value, revision: &Value) -> Value {
    let mut request = request.clone();
    request["revision"] = revision.clone();
    request
}

#[test]
fn rpc_requires_one_explicit_outcome_and_strict_nonconflicting_resolution_fields() {
    log("rpc_admission");
    let (mut handle, provider) = fixture(stored(&original(false)), false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let valid = request(&handle, "complete");
        let before = history(&handle);
        for resolution in [
            Value::Null, json!({}), json!({"kind":"reconcile"}),
            json!({"kind":"reconcile","outcome":"complete"}),
            json!({"kind":"reconcile","evidence":"reviewed"}),
            json!({"kind":"reconcile","outcome":null,"evidence":"reviewed"}),
            json!({"kind":"reconcile","outcome":"resume","evidence":"reviewed"}),
            json!({"kind":"reconcile","outcome":"complete","evidence":null}),
            json!({"kind":"reconcile","outcome":"complete","evidence":" \n"}),
            json!({"kind":"reconcile","outcome":"complete","evidence":"x".repeat(16 * 1024 + 1)}),
            json!({"kind":"reconcile","outcome":"complete","evidence":"reviewed","retry":true}),
        ] {
            let mut input = valid.clone();
            input["resolution"] = resolution;
            assert!(rpc_goal_command(handle.session_mut(), &owner, &input).await.is_err());
            assert_eq!(history(&handle), before);
        }
        for (field, value) in [
            ("revision", Value::Null), ("revision", json!("gr1-invalid")),
            ("op", json!("get")), ("op", json!("resume")), ("op", json!("drop")),
            ("budget", Value::Null), ("objective", json!("replacement")),
            ("token_budget", json!(200)), ("max_prompts", json!(4)), ("reason", json!("ambiguous")),
        ] {
            let mut input = valid.clone();
            input[field] = value;
            assert!(rpc_goal_command(handle.session_mut(), &owner, &input).await.is_err());
            assert_eq!(history(&handle), before);
        }
        // Existing completion cannot be used instead of reconciliation.
        let mut ordinary = valid;
        ordinary["resolution"] = json!({"kind":"complete","evidence":"did not reconcile"});
        assert!(rpc_goal_command(handle.session_mut(), &owner, &ordinary).await.unwrap_err()
            .to_string().contains("GOAL_INTERRUPTED"));
        assert_eq!(history(&handle), before);
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}

#[test]
fn rpc_stale_usage_and_branch_reviews_cannot_reconcile_a_different_revision() {
    log("rpc_fresh_revision");
    let (mut handle, provider) = fixture(stored(&original(false)), false);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let old = request(&handle, "complete");
        let checkpoint = handle.current_goal().unwrap().unwrap().checkpoint_id;
        handle.session_store().try_lock().unwrap().append_message(SessionMessage::Assistant {
            message: AssistantMessage { usage: Usage { total_tokens: 5, ..Usage::default() }, ..AssistantMessage::default() },
        });
        let before = history(&handle);
        assert!(rpc_goal_command(handle.session_mut(), &owner, &old).await.unwrap_err()
            .to_string().contains("GOAL_STALE"));
        assert_eq!(history(&handle), before);
        let with_usage = request(&handle, "complete");
        assert!(handle.session_store().try_lock().unwrap().navigate_to(&checkpoint));
        let branch = history(&handle);
        assert!(rpc_goal_command(handle.session_mut(), &owner, &with_usage).await.unwrap_err()
            .to_string().contains("GOAL_STALE"));
        assert_eq!(history(&handle), branch);
        let fresh = request(&handle, "complete");
        let result = rpc_goal_command(handle.session_mut(), &owner, &fresh).await.unwrap();
        assert_eq!(result["goal"]["tokensUsed"], 7);
        assert_eq!(history(&handle)["entries"].as_array().unwrap().len(), 3);
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}

#[test]
fn rpc_unconfirmed_reconciliation_uses_existing_save_only_recovery() {
    log("rpc_save_recovery");
    let directory = tempfile::tempdir().unwrap();
    let mut session = stored(&original(true));
    session.path = Some(directory.path().to_path_buf());
    session.session_dir = Some(directory.path().to_path_buf());
    let (mut handle, provider) = fixture(session, true);
    execute(async {
        let owner = AgentCx::for_current_or_request();
        let request = request(&handle, "complete");
        let result = rpc_goal_command(handle.session_mut(), &owner, &request).await.unwrap();
        assert_eq!(result["persistence"], "unconfirmed");
        assert_eq!(result["goal"]["status"], "complete");
        assert!(result["persistenceError"].is_string());
        let before = history(&handle);
        assert!(rpc_goal_command(handle.session_mut(), &owner, &request).await.is_err());
        let path = directory.path().join("rpc-reconciled.jsonl");
        handle.session_store().try_lock().unwrap().path = Some(path.clone());
        let save = json!({"op":"pause","revision":result["revision"],"resolution":{"kind":"save"}});
        let saved = rpc_goal_command(handle.session_mut(), &owner, &save).await.unwrap();
        assert_eq!(saved["persistence"], "saved");
        assert_eq!(saved["resolution"]["journalChanged"], false);
        assert_eq!(saved["goal"], result["goal"]);
        assert_eq!(saved["revision"], result["revision"]);
        assert_eq!(history(&handle), before);
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(rpc_goal_state(&reopened).unwrap()["goal"], result["goal"]);
        assert_eq!(reopened.entries.len(), 2);
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "ftui")]
async fn terminal(handle: &mut AgentSessionHandle, args: &str) -> Result<String> {
    crate::goal::terminal::run_with_control(handle, args, |_| {
        panic!("reconciliation must not create a live goal run");
    }).await
}

#[cfg(feature = "ftui")]
fn review(output: &str) -> String {
    output.lines().find_map(|line| line.strip_prefix("Review: ")).unwrap().to_owned()
}

#[cfg(feature = "ftui")]
#[test]
fn terminal_outcomes_are_discoverable_reviewed_and_never_publish_run_control() {
    log("terminal_outcomes");
    for (flag, status) in [("--pause", "budget-limited"), ("--complete", "complete"), ("--drop", "dropped")] {
        let (mut handle, provider) = fixture(stored(&original(true)), false);
        execute(async {
            let before = terminal(&mut handle, "status").await.unwrap();
            let token = review(&before);
            for flag in ["--pause", "--complete", "--drop"] {
                assert!(before.contains(&format!("/goal reconcile {token} {flag} -- <evidence>")));
            }
            assert!(!before.contains("Attest completion: /goal complete"));
            assert!(!before.contains("Execute one prompt:"));
            let evidence = "Checked 日本語 result\n--complete is evidence here; $(no-shell)";
            let command = format!("reconcile {token} {flag} -- {evidence}");
            let after = terminal(&mut handle, &command).await.unwrap();
            assert!(after.contains("interrupted: false"));
            assert_ne!(review(&after), token);
            assert_eq!(state(&handle)["goal"]["status"], status);
            assert_eq!(state(&handle)["goal"]["note"], evidence);
            assert_eq!(state(&handle)["goal"]["promptsStarted"], 1);
            assert_eq!(state(&handle)["goal"]["tokensUsed"], 7);
            assert!(!after.contains("/goal reconcile"));
            assert!(after.contains("/goal save"), "terminal states must retain save recovery guidance");
            assert!(terminal(&mut handle, &command).await.unwrap_err().to_string().contains("GOAL_STALE"));
            assert_eq!(history(&handle)["entries"].as_array().unwrap().len(), 2);
        });
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }
}

#[cfg(feature = "ftui")]
#[test]
fn terminal_rejects_missing_ambiguous_oversized_and_stale_evidence_without_mutation() {
    log("terminal_admission");
    let (mut handle, provider) = fixture(stored(&original(false)), false);
    execute(async {
        let token = review(&terminal(&mut handle, "status").await.unwrap());
        let before = history(&handle);
        for suffix in [
            String::new(), "-- evidence".into(), "--complete".into(), "--complete --   ".into(),
            "--complete --pause -- evidence".into(), "--pause --pause -- evidence".into(),
            "--resume -- evidence".into(), "--complete missing-separator evidence".into(),
            "--complete -- bad\0evidence".into(), format!("--complete -- {}", "x".repeat(16 * 1024 + 1)),
        ] {
            assert!(terminal(&mut handle, &format!("reconcile {token} {suffix}")).await.is_err());
            assert_eq!(history(&handle), before);
        }
        assert!(terminal(&mut handle, &format!("pause {token}")).await.unwrap_err()
            .to_string().contains("GOAL_INTERRUPTED"));
        let command = format!("reconcile {token} --complete -- Reviewed the old branch");
        handle.session_store().try_lock().unwrap().append_custom_entry("changed context".into(), None);
        let changed = history(&handle);
        assert!(terminal(&mut handle, &command).await.unwrap_err().to_string().contains("GOAL_STALE"));
        assert_eq!(history(&handle), changed);
        let fresh = review(&terminal(&mut handle, "status").await.unwrap());
        terminal(&mut handle, &format!("reconcile {fresh} --pause -- Reviewed current branch")).await.unwrap();
        assert_eq!(state(&handle)["goal"]["status"], "paused");
        assert_eq!(state(&handle)["goal"]["interrupted"], false);
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "ftui")]
#[test]
fn terminal_noninterrupted_and_missing_goals_are_not_reinterpreted_as_reconciliation() {
    log("terminal_state_refusal");
    for status in [GoalStatus::Active, GoalStatus::Paused, GoalStatus::Complete, GoalStatus::Dropped] {
        let mut record = original(false);
        record.in_flight = false;
        record.status = status;
        let (mut handle, provider) = fixture(stored(&record), false);
        execute(async {
            let output = terminal(&mut handle, "status").await.unwrap();
            assert!(!output.contains("/goal reconcile"));
            let token = review(&output);
            let before = history(&handle);
            let error = terminal(&mut handle, &format!("reconcile {token} --complete -- Wrong state")).await.unwrap_err();
            assert!(error.to_string().contains(if status.terminal() { "GOAL_TERMINAL" } else { "GOAL_NOT_INTERRUPTED" }));
            assert_eq!(history(&handle), before);
        });
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }
    let (mut empty, provider) = fixture(Session::in_memory(), false);
    execute(async {
        let token = review(&terminal(&mut empty, "status").await.unwrap());
        assert!(terminal(&mut empty, &format!("reconcile {token} --pause -- No goal")).await.unwrap_err()
            .to_string().contains("GOAL_MISSING"));
        assert!(history(&empty)["entries"].as_array().unwrap().is_empty());
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "ftui")]
#[test]
fn terminal_failed_save_keeps_reconciled_state_and_offers_a_nonreplaying_save() {
    log("terminal_save_recovery");
    let directory = tempfile::tempdir().unwrap();
    let mut session = stored(&original(true));
    session.path = Some(directory.path().to_path_buf());
    session.session_dir = Some(directory.path().to_path_buf());
    let (mut handle, provider) = fixture(session, true);
    execute(async {
        let token = review(&terminal(&mut handle, "status").await.unwrap());
        let output = terminal(&mut handle, &format!("reconcile {token} --complete -- Inspected final output")).await.unwrap();
        assert!(output.contains("Saving was NOT confirmed"));
        let current = review(&output);
        assert!(output.contains(&format!("/goal save {current}")));
        assert_eq!(state(&handle)["goal"]["status"], "complete");
        let before = history(&handle);
        let path = directory.path().join("terminal-reconciled.jsonl");
        handle.session_store().try_lock().unwrap().path = Some(path.clone());
        let output = terminal(&mut handle, &format!("save {current}")).await.unwrap();
        assert!(output.contains("Goal journal saved"));
        assert_eq!(review(&output), current);
        assert_eq!(history(&handle), before);
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        let state = rpc_goal_state(&reopened).unwrap();
        assert_eq!(state["goal"]["status"], "complete");
        assert_eq!(state["goal"]["interrupted"], false);
        assert_eq!(reopened.entries.len(), 2);
    });
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
}
