//! Shared admission and accounting for a session tree's model spend.
//!
//! Limits apply before a request is sent. Already admitted requests finish and
//! may exceed the cap; token pricing and provider-reported costs are estimates,
//! not a reservation of the provider's final invoice.

use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionMessage};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const CHECKPOINT_TYPE: &str = "cost_budget";

/// Positive finite USD values accepted by the CLI, settings and SDK.
pub fn parse_threshold(value: &str) -> std::result::Result<f64, String> {
    let amount = value
        .parse::<f64>()
        .map_err(|_| "expected a positive finite USD amount".to_string())?;
    if !amount.is_finite() || amount <= 0.0 {
        return Err("expected a positive finite USD amount".to_string());
    }
    Ok(amount)
}

fn validate_threshold(value: Option<f64>, name: &str) -> Result<()> {
    if value.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err(Error::config(format!(
            "{name} must be a positive finite USD amount"
        )));
    }
    Ok(())
}

fn add_cost(total: f64, cost: f64) -> f64 {
    if cost.is_nan() || cost <= 0.0 {
        total
    } else {
        (total + cost).min(f64::MAX)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSnapshot {
    pub spent: f64,
    pub warn_cost: Option<f64>,
    pub max_cost: Option<f64>,
    pub warned: bool,
}

impl CostSnapshot {
    fn validate(&self) -> Result<()> {
        validate_threshold(self.warn_cost, "session.warnCost")?;
        validate_threshold(self.max_cost, "session.maxCost")?;
        if !self.spent.is_finite() || self.spent < 0.0 {
            return Err(Error::session("invalid recorded model spend"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostCapRefusal {
    pub spent: f64,
    pub cap: f64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostWarning {
    pub spent: f64,
    pub threshold: f64,
    pub message: String,
}

/// Authenticated parent accounting for native child processes. Implementations
/// must not replay a record after an ambiguous transport failure.
pub trait CostRemote: Send + Sync {
    fn snapshot(&self) -> Result<CostSnapshot>;
    fn record(&self, cost: f64) -> Result<CostSnapshot>;
}

struct CostState {
    snapshot: CostSnapshot,
    warning: Option<CostWarning>,
    failure: Option<String>,
}

/// Clones share one monotonic account, including across model changes/retries.
#[derive(Clone)]
pub struct CostBudget {
    state: Arc<Mutex<CostState>>,
    remote: Option<Arc<dyn CostRemote>>,
}

impl std::fmt::Debug for CostBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CostBudget")
            .field("snapshot", &self.snapshot())
            .field("remote", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

impl CostBudget {
    pub fn new(warn_cost: Option<f64>, max_cost: Option<f64>, recorded_spend: f64) -> Result<Self> {
        let snapshot = CostSnapshot {
            spent: recorded_spend,
            warn_cost,
            max_cost,
            // A resumed account has already crossed this warning threshold.
            warned: warn_cost.is_some_and(|threshold| recorded_spend >= threshold),
        };
        snapshot.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(CostState {
                snapshot,
                warning: None,
                failure: None,
            })),
            remote: None,
        })
    }

    pub fn remote(remote: Arc<dyn CostRemote>) -> Result<Self> {
        let snapshot = remote.snapshot()?;
        snapshot.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(CostState {
                snapshot,
                warning: None,
                failure: None,
            })),
            remote: Some(remote),
        })
    }

    #[must_use]
    pub fn same_tree(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// A native child always remains charged to its original parent tree.
    pub fn for_replacement(&self, session: &Session, preserve: bool) -> Result<Self> {
        if preserve || self.remote.is_some() {
            return Ok(self.clone());
        }
        let snapshot = self.snapshot();
        Self::new(
            snapshot.warn_cost,
            snapshot.max_cost,
            recorded_spend(session)?,
        )
    }

    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    #[must_use]
    pub fn spent(&self) -> f64 {
        self.snapshot().spent
    }

    /// Refresh remote accounting before every admission. A failed accounting
    /// ACK permanently quarantines this handle; retrying cannot hide spend.
    pub fn refresh(&self) -> Result<CostSnapshot> {
        self.check_health()?;
        if let Some(remote) = &self.remote {
            let result = remote.snapshot();
            self.accept_remote(result)?;
        }
        // A concurrent record may have lost its acknowledgement while this
        // snapshot was in flight. A successful read cannot clear quarantine.
        self.check_health()?;
        Ok(self.snapshot())
    }

    fn check_health(&self) -> Result<()> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(failure) = &state.failure {
            return Err(Error::session(format!(
                "model cost accounting unavailable; no further requests admitted: {failure}"
            )));
        }
        Ok(())
    }

    fn accept_remote(&self, result: Result<CostSnapshot>) -> Result<()> {
        let result = result.and_then(|snapshot| {
            snapshot.validate()?;
            Ok(snapshot)
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(mut snapshot) => {
                // Concurrent observations may arrive out of order. Never
                // lower the locally known balance on a later-arriving reply.
                snapshot.spent = snapshot.spent.max(state.snapshot.spent);
                snapshot.warned |= state.snapshot.warned;
                state.snapshot = snapshot;
                Ok(())
            }
            Err(error) => {
                state.failure = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Cached refusal for presentation. Admission must use [`Self::admit`].
    #[must_use]
    pub fn check(&self) -> Option<CostCapRefusal> {
        let snapshot = self.snapshot();
        let cap = snapshot.max_cost?;
        (snapshot.spent >= cap).then(|| CostCapRefusal {
            spent: snapshot.spent,
            cap,
            message: Error::CostCap {
                spent: snapshot.spent,
                cap,
            }
            .to_string(),
        })
    }

    pub fn admit(&self) -> Result<()> {
        let snapshot = self.refresh()?;
        if let Some(cap) = snapshot.max_cost
            && snapshot.spent >= cap
        {
            return Err(Error::CostCap {
                spent: snapshot.spent,
                cap,
            });
        }
        Ok(())
    }

    /// Record exactly once per provider attempt, including known usage from a
    /// failed/aborted response. Invalid usage quarantines subsequent admission.
    pub fn record(&self, cost: f64) -> Result<()> {
        if !cost.is_finite() || cost < 0.0 {
            let message = "provider returned invalid model cost; further requests are blocked";
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failure = Some(message.to_string());
            return Err(Error::session(message));
        }
        if cost == 0.0 {
            return Ok(());
        }
        self.check_health()?;
        let cost = cost.min(f64::MAX);
        if let Some(remote) = &self.remote {
            return self.accept_remote(remote.record(cost));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.snapshot.spent = add_cost(state.snapshot.spent, cost);
        if !state.snapshot.warned
            && let Some(threshold) = state.snapshot.warn_cost
            && state.snapshot.spent >= threshold
        {
            state.snapshot.warned = true;
            let spent = state.snapshot.spent;
            state.warning = Some(CostWarning {
                spent,
                threshold,
                message: format!(
                    "Cost warning: this session has spent ${spent:.4}, reaching its ${threshold:.4} warning threshold (--warn-cost / session.warnCost)."
                ),
            });
        }
        Ok(())
    }

    pub fn take_warning(&self) -> Option<CostWarning> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .warning
            .take()
    }

    /// Persist the full shared account alongside the amount already represented
    /// by primary assistant messages. Resume adds only later primary usage, so
    /// compaction, forks and rewinds cannot refund a completed request.
    pub fn checkpoint(&self, session: &mut Session) -> Result<()> {
        // Cold resume may load only the active suffix. The persisted watermark
        // must include every retained branch, or resume would count it twice.
        session.ensure_full_v2_hydration_before_save()?;
        let spent = self.spent();
        let main_spent = main_spend(session);
        let unchanged = session
            .entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Custom(entry) if entry.custom_type == CHECKPOINT_TYPE => {
                    entry.data.as_ref()
                }
                _ => None,
            })
            .is_some_and(|data| {
                data.get("spent").and_then(serde_json::Value::as_f64) == Some(spent)
                    && data.get("mainSpent").and_then(serde_json::Value::as_f64) == Some(main_spent)
            });
        if !unchanged && spent > 0.0 {
            let active_leaf = session.leaf_id().map(str::to_owned);
            session.append_custom_entry(
                CHECKPOINT_TYPE.to_string(),
                Some(serde_json::json!({
                    "spent": spent,
                    "mainSpent": main_spent,
                })),
            );
            // Accounting belongs to the retained session tree, outside the
            // selected conversation. A failed assistant must remain the leaf
            // so retry/failover can abandon its output without refunding the
            // charge or losing completed tool cycles.
            if let Some(active_leaf) = active_leaf {
                if !session.navigate_to(&active_leaf) {
                    return Err(Error::session(
                        "cost checkpoint lost the active conversation leaf",
                    ));
                }
            } else {
                session.reset_leaf();
            }
        }
        Ok(())
    }
}

fn main_spend(session: &Session) -> f64 {
    session
        .entries
        .iter()
        .fold(0.0, |total, entry| match entry {
            SessionEntry::Message(entry) => match &entry.message {
                SessionMessage::Assistant { message } => add_cost(total, message.usage.cost.total),
                _ => total,
            },
            _ => total,
        })
}

/// Account for every retained branch, plus auxiliary/child spend checkpoints.
pub fn recorded_spend(session: &Session) -> Result<f64> {
    if session.has_partial_v2_hydration() {
        let mut complete = session.clone();
        complete.ensure_full_v2_hydration_before_save()?;
        return recorded_spend(&complete);
    }
    let main_spent = main_spend(session);
    Ok(session.entries.iter().fold(main_spent, |spent, entry| {
        let SessionEntry::Custom(entry) = entry else {
            return spent;
        };
        if entry.custom_type != CHECKPOINT_TYPE {
            return spent;
        }
        let Some(data) = &entry.data else {
            return spent;
        };
        let Some(recorded) = data.get("spent").and_then(serde_json::Value::as_f64) else {
            return spent;
        };
        let Some(covered) = data.get("mainSpent").and_then(serde_json::Value::as_f64) else {
            return spent;
        };
        if !recorded.is_finite() || recorded < 0.0 || !covered.is_finite() || covered < 0.0 {
            return spent;
        }
        spent.max(add_cost(recorded, (main_spent - covered).max(0.0)))
    }))
}

/// Resolve launch overrides without letting a native child replace its parent's
/// authoritative account. Explicit shared SDK budgets otherwise take precedence.
pub fn resolve_budget(
    session: &Session,
    settings: Option<&crate::config::SessionSettings>,
    warn_cost: Option<f64>,
    max_cost: Option<f64>,
    shared: Option<CostBudget>,
) -> Result<Option<CostBudget>> {
    if let Some(inherited) = crate::agent_hub::ipc::cost::inherited_budget()? {
        return Ok(Some(inherited));
    }
    if shared.is_some() {
        return Ok(shared);
    }
    let warn_cost = warn_cost.or_else(|| settings.and_then(|settings| settings.warn_cost));
    let max_cost = max_cost.or_else(|| settings.and_then(|settings| settings.max_cost));
    if warn_cost.is_none() && max_cost.is_none() {
        return Ok(None);
    }
    CostBudget::new(warn_cost, max_cost, recorded_spend(session)?).map(Some)
}

/// A live registry binding: tools retain the scope when the owner changes.
#[derive(Clone, Default)]
pub struct CostScope(Arc<Mutex<Option<CostBudget>>>);

impl CostScope {
    #[must_use]
    pub fn current(&self) -> Option<CostBudget> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, budget: Option<CostBudget>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = budget;
    }
}

impl std::fmt::Debug for CostScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CostScope").field(&self.current()).finish()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact integer/quarter-dollar fixtures verify account invariants.
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, Cost, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assistant(cost: f64) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                usage: Usage {
                    cost: Cost {
                        total: cost,
                        ..Cost::default()
                    },
                    ..Usage::default()
                },
                ..AssistantMessage::default()
            },
        }
    }

    #[test]
    fn thresholds_are_positive_finite_usd() {
        assert_eq!(parse_threshold("0.25"), Ok(0.25));
        assert_eq!(parse_threshold("1e2"), Ok(100.0));
        for value in ["0", "-1", "NaN", "inf", "Infinity", "1e999", "usd"] {
            assert!(parse_threshold(value).is_err(), "accepted {value}");
        }
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(CostBudget::new(Some(value), None, 0.0).is_err());
            assert!(CostBudget::new(None, Some(value), 0.0).is_err());
        }
    }

    #[test]
    fn siblings_share_warning_and_refuse_only_the_next_request() {
        let budget = CostBudget::new(Some(2.0), Some(3.0), 0.0).expect("account");
        let child = budget.clone();
        budget.admit().expect("initial request");
        child.admit().expect("concurrent request");
        budget.record(2.0).expect("first completion");
        let warning = child.take_warning().expect("shared crossing notice");
        assert_eq!(warning.spent, 2.0);
        assert_eq!(warning.threshold, 2.0);
        assert!(budget.take_warning().is_none());
        child
            .record(4.0)
            .expect("in-flight completion can exceed cap");
        assert_eq!(budget.spent(), 6.0);
        assert!(matches!(
            budget.admit(),
            Err(Error::CostCap {
                spent: 6.0,
                cap: 3.0
            })
        ));
        assert!(child.take_warning().is_none());
        let resumed = CostBudget::new(Some(2.0), Some(10.0), 6.0).expect("resume");
        resumed.record(1.0).expect("later completion");
        assert!(
            resumed.take_warning().is_none(),
            "resume does not warn twice"
        );
    }

    #[test]
    fn invalid_usage_quarantines_without_refunding_known_cost() {
        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            let budget = CostBudget::new(None, Some(10.0), 2.0).expect("account");
            assert!(budget.record(invalid).is_err());
            assert_eq!(budget.spent(), 2.0);
            assert!(budget.admit().is_err());
            assert!(budget.record(1.0).is_err());
        }
    }

    #[test]
    fn completed_parallel_requests_are_charged_once_each() {
        let budget = CostBudget::new(None, Some(100.0), 0.0).expect("account");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let budget = budget.clone();
                scope.spawn(move || {
                    for _ in 0..64 {
                        budget.record(0.25).expect("record request");
                    }
                });
            }
        });
        assert_eq!(budget.spent(), 128.0);
        assert!(budget.admit().is_err());
    }

    #[test]
    fn checkpoints_preserve_auxiliary_spend_and_all_branches_without_double_charge() {
        let mut session = Session::in_memory();
        let first = session.append_message(assistant(2.0));
        let budget = CostBudget::new(None, Some(20.0), 2.0).expect("account");
        budget.record(3.0).expect("side request");
        budget.checkpoint(&mut session).expect("checkpoint");
        let count = session.entries.len();
        budget
            .checkpoint(&mut session)
            .expect("idempotent checkpoint");
        assert_eq!(session.entries.len(), count);
        assert_eq!(recorded_spend(&session).expect("recorded cost"), 5.0);

        session.append_message(assistant(4.0));
        assert!(session.create_branch_from(&first));
        session.append_message(assistant(1.0));
        assert_eq!(recorded_spend(&session).expect("all branches"), 10.0);

        let switched = budget.for_replacement(&session, false).expect("switch");
        assert!(!budget.same_tree(&switched));
        assert_eq!(switched.spent(), 10.0);
        let fresh = budget
            .for_replacement(&Session::in_memory(), false)
            .expect("new session");
        assert_eq!(fresh.spent(), 0.0);
        let fork = switched
            .for_replacement(&Session::in_memory(), true)
            .expect("fork");
        assert!(switched.same_tree(&fork));
        let mut forked = Session::in_memory();
        forked.append_message(assistant(2.0));
        fork.checkpoint(&mut forked).expect("fork checkpoint");
        assert_eq!(recorded_spend(&forked).expect("fork resume"), 10.0);
        fork.record(2.0).expect("fork work");
        assert_eq!(switched.spent(), 12.0);
    }

    #[test]
    fn persisted_accounting_preserves_retry_tail_and_never_refunds_failed_requests() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for with_v2_sidecar in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("retry-cost.jsonl");
            let mut session = Session::create();
            session.path = Some(path.clone());
            let completed = session.append_message(assistant(1.0));
            let mut failed = assistant(0.5);
            if let SessionMessage::Assistant { message } = &mut failed {
                message.stop_reason = crate::model::StopReason::Error;
                message.error_message = Some("HTTP 503: interrupted response".to_string());
            }
            let incomplete = session.append_message(failed);
            let budget = CostBudget::new(None, Some(1.75), 1.5).unwrap();
            budget.record(0.25).unwrap();
            budget.checkpoint(&mut session).unwrap();
            let checkpoint = session.entries.last().unwrap().base_id().unwrap().clone();
            assert_eq!(session.leaf_id(), Some(incomplete.as_str()));
            runtime.block_on(session.save()).unwrap();

            let (mut reopened, diagnostics) = if with_v2_sidecar {
                let store = crate::session::create_v2_sidecar_from_jsonl(&path).unwrap();
                let (mut reopened, diagnostics) = Session::open_from_v2(
                    &store,
                    session.header.clone(),
                    crate::session::V2OpenMode::Full,
                )
                .unwrap();
                reopened.path = Some(path.clone());
                (reopened, diagnostics)
            } else {
                runtime
                    .block_on(Session::open_with_diagnostics(path.to_str().unwrap()))
                    .unwrap()
            };
            assert!(diagnostics.skipped_entries.is_empty());
            assert!(diagnostics.orphaned_parent_links.is_empty());
            assert_eq!(reopened.leaf_id(), Some(incomplete.as_str()));
            assert_eq!(recorded_spend(&reopened).unwrap(), 1.75);
            assert!(
                reopened.revert_incomplete_response(),
                "checkpoint must not hide retry tail"
            );
            assert_eq!(reopened.leaf_id(), Some(completed.as_str()));
            assert!(
                reopened.get_entry(&incomplete).is_some(),
                "abandoned output is retained"
            );
            assert!(
                reopened.get_entry(&checkpoint).is_some(),
                "accounting remains retained"
            );
            assert_eq!(recorded_spend(&reopened).unwrap(), 1.75);
            runtime.block_on(reopened.save()).unwrap();

            let (mut retried, diagnostics) = runtime
                .block_on(Session::open_with_diagnostics(path.to_str().unwrap()))
                .unwrap();
            assert!(diagnostics.skipped_entries.is_empty());
            assert!(diagnostics.orphaned_parent_links.is_empty());
            retried.ensure_full_v2_hydration_before_save().unwrap();
            assert_eq!(retried.leaf_id(), Some(completed.as_str()));
            let charge = retried.get_entry(&checkpoint).unwrap();
            assert_eq!(
                charge.base().parent_id.as_deref(),
                Some(incomplete.as_str())
            );
            assert!(
                retried.get_entry(&incomplete).is_some(),
                "accounting parent must not be orphaned"
            );
            let resumed = budget.for_replacement(&retried, false).unwrap();
            assert_eq!(resumed.spent(), 1.75);
            assert!(matches!(resumed.admit(), Err(Error::CostCap { .. })));
        }
    }

    #[test]
    fn a_cost_checkpoint_does_not_create_a_conversation_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
        let budget = CostBudget::new(None, Some(1.0), 0.75).unwrap();
        budget.checkpoint(&mut session).unwrap();
        assert!(session.leaf_id().is_none());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(session.save()).unwrap();
        let reopened = runtime
            .block_on(Session::open(
                session.path.as_ref().unwrap().to_str().unwrap(),
            ))
            .unwrap();
        assert!(reopened.leaf_id().is_none());
        assert!(reopened.to_messages_for_current_path().is_empty());
        assert_eq!(recorded_spend(&reopened).unwrap(), 0.75);
    }

    struct AmbiguousRemote {
        snapshot_calls: AtomicUsize,
        record_calls: AtomicUsize,
    }

    impl CostRemote for AmbiguousRemote {
        fn snapshot(&self) -> Result<CostSnapshot> {
            self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CostSnapshot {
                spent: 1.0,
                warn_cost: None,
                max_cost: Some(5.0),
                warned: false,
            })
        }

        fn record(&self, _cost: f64) -> Result<CostSnapshot> {
            self.record_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::session(
                "parent applied charge but acknowledgement was lost",
            ))
        }
    }

    #[test]
    fn lost_record_ack_is_never_replayed_and_quarantines_future_admission() {
        let remote = Arc::new(AmbiguousRemote {
            snapshot_calls: AtomicUsize::new(0),
            record_calls: AtomicUsize::new(0),
        });
        let budget = CostBudget::remote(remote.clone()).expect("join parent account");
        assert!(budget.record(2.0).is_err());
        assert!(budget.record(2.0).is_err());
        assert!(budget.admit().is_err());
        assert_eq!(remote.record_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remote.snapshot_calls.load(Ordering::SeqCst), 1);
        let replacement = budget
            .for_replacement(&Session::in_memory(), false)
            .expect("child replacement");
        assert!(budget.same_tree(&replacement));
        assert!(replacement.admit().is_err());
    }

    #[test]
    fn stale_remote_snapshot_cannot_refund_or_clear_quarantine() {
        let budget = CostBudget::new(None, Some(5.0), 4.0).expect("account");
        budget
            .accept_remote(Ok(CostSnapshot {
                spent: 1.0,
                warn_cost: None,
                max_cost: Some(5.0),
                warned: false,
            }))
            .expect("stale observation");
        assert_eq!(budget.spent(), 4.0);
        budget
            .accept_remote(Err(Error::session("lost ACK")))
            .expect_err("quarantine");
        budget
            .accept_remote(Ok(CostSnapshot {
                spent: 2.0,
                warn_cost: None,
                max_cost: Some(5.0),
                warned: false,
            }))
            .expect("late successful read");
        assert!(budget.refresh().is_err());
        assert_eq!(budget.spent(), 4.0);
    }

    #[test]
    fn captured_tool_scope_keeps_departing_account_after_session_switch() {
        let old = CostBudget::new(None, Some(3.0), 2.0).expect("old account");
        let scope = CostScope::default();
        scope.set(Some(old.clone()));
        let queued = scope.current().expect("captured account");
        let fresh = old
            .for_replacement(&Session::in_memory(), false)
            .expect("new account");
        scope.set(Some(fresh));
        queued.record(1.0).expect("queued completion");
        assert!(old.admit().is_err());
        assert_eq!(scope.current().expect("new current account").spent(), 0.0);
    }
}
