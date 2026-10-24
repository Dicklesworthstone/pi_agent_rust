//! Resolve an interrupted native intent without replaying work or resetting its
//! budget. Only the host's explicit evidence selects an outcome; this is not a
//! verifier, an automatic recovery rule, or permission to resume the provider.

use super::{
    AgentCx, AgentSessionHandle, GoalChange, GoalSnapshot, GoalStatus, Record, Result, Transition,
    change, failure, validate_text,
};

/// Host-attested outcome after inspecting an unfinished goal attempt.
/// None of these outcomes authorizes another prompt or refunds consumed budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalReconciliation {
    /// Retain the goal for review. Exhausted goals remain budget-limited.
    Pause,
    /// The host has independently inspected the deliverables and attests completion.
    Complete,
    /// The host abandons the objective, without claiming successful completion.
    Drop,
}

pub(super) fn apply(
    mut record: Record,
    outcome: GoalReconciliation,
    evidence: &str,
) -> Result<Record> {
    if !record.in_flight {
        return Err(failure(
            "GOAL_NOT_INTERRUPTED",
            "only an unfinished goal attempt can be reconciled; review its current state",
        ));
    }
    validate_text(evidence, "interruption reconciliation evidence")?;
    record.status = match outcome {
        GoalReconciliation::Pause => GoalStatus::Paused,
        GoalReconciliation::Complete => GoalStatus::Complete,
        GoalReconciliation::Drop => GoalStatus::Dropped,
    };
    record.in_flight = false;
    record.note = evidence.to_owned();
    Ok(record)
}

impl AgentSessionHandle {
    /// Resolve the exact reviewed unfinished intent without executing it again.
    ///
    /// Inspect session history, external side effects and any returned input
    /// before choosing an outcome. Evidence is preserved literally as a host
    /// attestation, not evaluated as code or accepted as independently verified
    /// proof. A successful transition appends one checkpoint, retaining the
    /// original intent, objective, identity, consumed counters and allowances.
    ///
    /// Reconciliation does not require remaining execution budget. Pause keeps
    /// exhausted goals budget-limited; Complete and Drop settle them without
    /// increasing an allowance merely to clear an interrupted marker. A later
    /// resume is separate and still checks remaining budget and native admission.
    ///
    /// Stale snapshots, non-interrupted/terminal goals, cancellation and lock
    /// contention refuse the decision. Inspect GoalChange::persistence: a save
    /// failure does not undo the live decision. Retry flush_goal, not this call.
    /// This operation does not clear provider-admission quarantine, replay tool
    /// effects, restore unsent input or prove asynchronous cleanup completed.
    pub async fn reconcile_goal(
        &mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        outcome: GoalReconciliation,
        evidence: &str,
    ) -> Result<GoalChange> {
        change(
            self,
            owner,
            expected,
            Transition::Reconcile { outcome, evidence },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::compaction::ResolvedCompactionSettings;
    use crate::goal::{
        GoalBudget, GoalPersistence, Store, append, latest, rpc_goal_command, rpc_goal_state,
    };
    use crate::model::{AssistantMessage, Usage};
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::sdk::EventListeners;
    use crate::session::{Session, SessionEntry, SessionMessage};
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
        fn name(&self) -> &str {
            "goal-reconciliation-test"
        }
        fn api(&self) -> &str {
            "goal-reconciliation-test"
        }
        fn model_id(&self) -> &str {
            "no-provider-work"
        }

        async fn stream(
            &self,
            _: &Context<'_>,
            _: &StreamOptions,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::Error::validation(
                "reconciliation must not invoke a provider",
            ))
        }
    }

    fn execute<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8)
            .build()
            .unwrap()
            .block_on(future)
    }

    fn interrupted(exhausted: bool) -> Record {
        let mut record = Record::new(
            "Finish the migration",
            GoalBudget {
                max_prompts: if exhausted { 1 } else { 3 },
                token_budget: if exhausted { 7 } else { 100 },
            },
        )
        .unwrap();
        record.prompts_started = 1;
        record.tokens_spent = 7;
        record.in_flight = true;
        record.note = "intent recorded before interruption".into();
        record
    }

    fn stored(record: &Record) -> Session {
        let mut session = Session::in_memory();
        append(&mut session, record).unwrap();
        session
    }

    fn fixture(session: Session, save: bool) -> (AgentSessionHandle, Arc<NoProvider>) {
        let provider = Arc::new(NoProvider::default());
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig::default(),
        );
        let session = AgentSession::new(
            agent,
            Arc::new(Store::new(session)),
            save,
            ResolvedCompactionSettings::default(),
        );
        (
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::default()),
            provider,
        )
    }

    fn current(handle: &AgentSessionHandle) -> GoalSnapshot {
        handle.current_goal().unwrap().unwrap()
    }

    fn journal(handle: &AgentSessionHandle) -> Value {
        let store = handle.session_store();
        let session = store.try_lock().unwrap();
        json!({"leaf":session.leaf_id(),"entries":session.entries})
    }

    fn log(case: &str) {
        eprintln!(
            "{}",
            json!({"event":"goal_reconciliation_test","case":case})
        );
    }

    #[test]
    fn ordinary_sdk_and_rpc_pause_cannot_erase_an_unfinished_intent() {
        log("pause_is_not_reconciliation");
        let (mut handle, provider) = fixture(stored(&interrupted(false)), false);
        execute(async {
            let owner = AgentCx::for_current_or_request();
            let before = current(&handle);
            let history = journal(&handle);
            assert!(
                handle
                    .pause_goal(&owner, &before, "just pause")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_INTERRUPTED")
            );
            assert!(
                rpc_goal_command(handle.session_mut(), &owner, &json!({"op":"pause"}))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_INTERRUPTED")
            );
            assert_eq!(journal(&handle), history);
            assert!(current(&handle).interrupted());
            assert!(
                handle
                    .complete_goal(&owner, &before, "no reconciliation")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_INTERRUPTED")
            );
            assert_eq!(journal(&handle), history);
        });
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn every_reviewed_outcome_preserves_budgets_and_the_original_intent() {
        log("outcomes_and_exhausted_budgets");
        let evidence = "  I inspected 日本語 output and side effects.\nNo replay.  ";
        for exhausted in [false, true] {
            for outcome in [
                GoalReconciliation::Pause,
                GoalReconciliation::Complete,
                GoalReconciliation::Drop,
            ] {
                let original = interrupted(exhausted);
                let (mut handle, provider) = fixture(stored(&original), false);
                execute(async {
                    let owner = AgentCx::for_current_or_request();
                    let before = current(&handle);
                    let result = handle
                        .reconcile_goal(&owner, &before, outcome, evidence)
                        .await
                        .unwrap();
                    assert_eq!(result.persistence, GoalPersistence::MemoryOnly);
                    assert_eq!(result.goal.id(), before.id());
                    assert_eq!(result.goal.objective(), before.objective());
                    assert_eq!(result.goal.budget(), before.budget());
                    assert_eq!(result.goal.prompts_started(), 1);
                    assert_eq!(result.goal.tokens_spent(), 7);
                    assert_eq!(result.goal.note(), evidence);
                    assert!(!result.goal.interrupted());
                    let status = match outcome {
                        GoalReconciliation::Pause if exhausted => GoalStatus::BudgetLimited,
                        GoalReconciliation::Pause => GoalStatus::Paused,
                        GoalReconciliation::Complete => GoalStatus::Complete,
                        GoalReconciliation::Drop => GoalStatus::Dropped,
                    };
                    assert_eq!(result.goal.status(), status);
                    let store = handle.session_store();
                    let session = store.try_lock().unwrap();
                    assert_eq!(session.entries.len(), 2);
                    let SessionEntry::Custom(entry) =
                        session.get_entry(&before.checkpoint_id).unwrap()
                    else {
                        panic!("original checkpoint retained");
                    };
                    assert_eq!(entry.data, Some(serde_json::to_value(&original).unwrap()));
                    assert_eq!(session.leaf_id(), Some(result.goal.checkpoint_id.as_str()));
                });
                assert_eq!(provider.0.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn pause_reconciliation_never_resumes_or_refunds_exhausted_work() {
        log("pause_requires_separate_resume");
        for exhausted in [false, true] {
            let (mut handle, provider) = fixture(stored(&interrupted(exhausted)), false);
            execute(async {
                let owner = AgentCx::for_current_or_request();
                let before = current(&handle);
                let paused = handle
                    .reconcile_goal(
                        &owner,
                        &before,
                        GoalReconciliation::Pause,
                        "Reviewed partial work",
                    )
                    .await
                    .unwrap();
                let repeated = handle
                    .reconcile_goal(&owner, &paused.goal, GoalReconciliation::Pause, "again")
                    .await
                    .unwrap_err();
                assert!(repeated.to_string().contains("GOAL_NOT_INTERRUPTED"));
                let resumed = handle.resume_goal(&owner, &paused.goal).await;
                if exhausted {
                    assert!(resumed.unwrap_err().to_string().contains("GOAL_BUDGET"));
                    assert_eq!(current(&handle).status(), GoalStatus::BudgetLimited);
                } else {
                    assert_eq!(resumed.unwrap().goal.status(), GoalStatus::Active);
                }
                assert_eq!(current(&handle).prompts_started(), 1);
                assert_eq!(current(&handle).tokens_spent(), 7);
            });
            assert_eq!(provider.0.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn settled_or_terminal_goals_cannot_be_reconciled_again() {
        log("no_reconciliation_replay");
        for status in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::BudgetLimited,
            GoalStatus::Complete,
            GoalStatus::Dropped,
        ] {
            let mut record = interrupted(true);
            record.in_flight = false;
            record.status = status;
            let (mut handle, _) = fixture(stored(&record), false);
            execute(async {
                let owner = AgentCx::for_current_or_request();
                let before = current(&handle);
                let history = journal(&handle);
                let error = handle
                    .reconcile_goal(
                        &owner,
                        &before,
                        GoalReconciliation::Complete,
                        "cannot rewrite history",
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains(if status.terminal() {
                    "GOAL_TERMINAL"
                } else {
                    "GOAL_NOT_INTERRUPTED"
                }));
                assert_eq!(journal(&handle), history);
            });
        }
    }

    #[test]
    fn invalid_evidence_never_changes_the_checkpoint() {
        log("bounded_literal_evidence");
        let (mut handle, _) = fixture(stored(&interrupted(false)), false);
        execute(async {
            let owner = AgentCx::for_current_or_request();
            let before = current(&handle);
            let history = journal(&handle);
            for evidence in [
                String::new(),
                " \n\t".into(),
                "雪".repeat(super::super::MAX_TEXT_BYTES / 3 + 1),
            ] {
                assert!(
                    handle
                        .reconcile_goal(&owner, &before, GoalReconciliation::Complete, &evidence)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("GOAL_TEXT")
                );
                assert_eq!(journal(&handle), history);
            }
        });
    }

    #[test]
    fn changed_usage_branch_or_store_requires_a_fresh_review() {
        log("snapshot_identity");
        let (mut handle, _) = fixture(stored(&interrupted(false)), false);
        let (mut other, _) = fixture(stored(&interrupted(false)), false);
        execute(async {
            let owner = AgentCx::for_current_or_request();
            let before = current(&handle);
            assert!(
                other
                    .reconcile_goal(&owner, &before, GoalReconciliation::Complete, "wrong store")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_STALE")
            );
            let store = handle.session_store();
            store
                .try_lock()
                .unwrap()
                .append_message(SessionMessage::Assistant {
                    message: AssistantMessage {
                        usage: Usage {
                            total_tokens: 5,
                            ..Usage::default()
                        },
                        ..AssistantMessage::default()
                    },
                });
            let history = journal(&handle);
            assert!(
                handle
                    .reconcile_goal(&owner, &before, GoalReconciliation::Complete, "stale usage")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_STALE")
            );
            assert_eq!(journal(&handle), history);
            let reviewed = current(&handle);
            assert_eq!(reviewed.tokens_spent(), 12);
            assert!(store.try_lock().unwrap().navigate_to(&before.checkpoint_id));
            assert!(
                handle
                    .reconcile_goal(
                        &owner,
                        &reviewed,
                        GoalReconciliation::Complete,
                        "other branch leaf"
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_STALE")
            );
            let fresh = current(&handle);
            let done = handle
                .reconcile_goal(
                    &owner,
                    &fresh,
                    GoalReconciliation::Complete,
                    "reviewed this leaf",
                )
                .await
                .unwrap();
            assert_eq!(done.goal.tokens_spent(), 7);
            assert_eq!(
                store.try_lock().unwrap().entries.len(),
                3,
                "off-path usage remains in the journal"
            );
        });
    }

    #[test]
    fn cancellation_contention_and_corrupt_state_refuse_without_a_decision() {
        log("admission_failures");
        let (mut handle, _) = fixture(stored(&interrupted(false)), false);
        execute(async {
            let owner = AgentCx::for_request();
            let before = current(&handle);
            let history = journal(&handle);
            owner.set_cancel_requested(true);
            assert!(
                handle
                    .reconcile_goal(&owner, &before, GoalReconciliation::Complete, "cancelled")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_CANCELLED")
            );
            assert_eq!(journal(&handle), history);
            let owner = AgentCx::for_current_or_request();
            let store = handle.session_store();
            let held = store.try_lock().unwrap();
            assert!(
                handle
                    .reconcile_goal(&owner, &before, GoalReconciliation::Complete, "busy")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_BUSY")
            );
            drop(held);
            assert_eq!(journal(&handle), history);
            store.try_lock().unwrap().append_custom_entry(
                super::super::CHECKPOINT_TYPE.into(),
                Some(json!({"schema":"unsupported"})),
            );
            let history = journal(&handle);
            assert!(
                handle
                    .reconcile_goal(&owner, &before, GoalReconciliation::Complete, "corrupt")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_CHECKPOINT")
            );
            assert_eq!(journal(&handle), history);
        });
    }

    #[test]
    fn saved_unfinished_attempt_can_be_reopened_reconciled_and_saved_without_execution() {
        log("native_reopen");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("interrupted.jsonl");
        let mut session = stored(&interrupted(true));
        session.path = Some(path.clone());
        session.session_dir = Some(directory.path().to_path_buf());
        execute(async {
            session.save().await.unwrap();
            let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
            let (mut handle, provider) = fixture(reopened, true);
            let owner = AgentCx::for_current_or_request();
            let before = current(&handle);
            let result = handle
                .reconcile_goal(
                    &owner,
                    &before,
                    GoalReconciliation::Complete,
                    "Inspected committed output after restart",
                )
                .await
                .unwrap();
            assert_eq!(result.persistence, GoalPersistence::Saved);
            let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
            let (_, record) = latest(&reopened).unwrap().unwrap();
            assert_eq!(record.status, GoalStatus::Complete);
            assert!(!record.in_flight);
            assert_eq!(record.prompts_started, 1);
            assert_eq!(record.tokens_spent, 7);
            assert_eq!(reopened.entries.len(), 2);
            assert!(reopened.get_entry(&before.checkpoint_id).is_some());
            assert_eq!(provider.0.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn unconfirmed_reconciliation_is_flushed_not_replayed() {
        log("failed_save_recovery");
        let directory = tempfile::tempdir().unwrap();
        let mut session = stored(&interrupted(true));
        session.path = Some(directory.path().to_path_buf());
        session.session_dir = Some(directory.path().to_path_buf());
        let (mut handle, provider) = fixture(session, true);
        execute(async {
            let owner = AgentCx::for_current_or_request();
            let before = current(&handle);
            let result = handle
                .reconcile_goal(
                    &owner,
                    &before,
                    GoalReconciliation::Complete,
                    "Inspected result; disk unavailable",
                )
                .await
                .unwrap();
            assert!(matches!(
                result.persistence,
                GoalPersistence::Unconfirmed { .. }
            ));
            assert_eq!(current(&handle).status(), GoalStatus::Complete);
            let history = journal(&handle);
            assert!(
                handle
                    .reconcile_goal(
                        &owner,
                        &before,
                        GoalReconciliation::Complete,
                        "retrying decision is wrong"
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_STALE")
            );
            assert!(
                handle
                    .reconcile_goal(
                        &owner,
                        &result.goal,
                        GoalReconciliation::Complete,
                        "already settled"
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("GOAL_TERMINAL")
            );
            assert_eq!(journal(&handle), history);
            let path = directory.path().join("recovered.jsonl");
            handle.session_store().try_lock().unwrap().path = Some(path.clone());
            assert_eq!(
                handle.flush_goal(&owner).await.unwrap(),
                GoalPersistence::Saved
            );
            let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
            let state = rpc_goal_state(&reopened).unwrap();
            assert_eq!(state["goal"]["status"], "complete");
            assert_eq!(state["goal"]["interrupted"], false);
            assert_eq!(state["goal"]["note"], result.goal.note());
            assert_eq!(reopened.entries.len(), 2);
        });
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }
}
