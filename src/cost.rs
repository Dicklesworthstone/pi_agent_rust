//! Shared admission and accounting for a session tree's model spend.
//!
//! Limits apply before a request is sent. Already admitted requests finish and
//! may exceed the cap; token pricing and provider-reported costs are estimates,
//! not a reservation of the provider's final invoice.

use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionMessage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod durability;

use durability::{Coverage, Journal, Origin, Retirement};

const CHECKPOINT_TYPE: &str = "cost_budget";
const MAX_PENDING_ATTEMPTS: usize = 1024;
static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_attempt_id() -> Result<u64> {
    NEXT_ATTEMPT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| Error::session("model cost attempt identities exhausted"))
}

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
    /// Finalized charges only. Pending estimates never enter durable checkpoints.
    pub spent: f64,
    /// Known usage from live requests, replaced by final billing on settlement.
    pub pending: f64,
    /// Account-local revision orders receipts whose pending estimates may fall.
    pub revision: u64,
    pub warn_cost: Option<f64>,
    pub max_cost: Option<f64>,
    pub warned: bool,
}

impl CostSnapshot {
    fn validate(&self) -> Result<()> {
        validate_threshold(self.warn_cost, "session.warnCost")?;
        validate_threshold(self.max_cost, "session.maxCost")?;
        if !self.spent.is_finite()
            || self.spent < 0.0
            || !self.pending.is_finite()
            || self.pending < 0.0
        {
            return Err(Error::session("invalid recorded model spend"));
        }
        Ok(())
    }

    fn admission_spend(&self) -> f64 {
        add_cost(self.spent, self.pending)
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
    fn start_attempt(&self) -> Result<(CostSnapshot, Box<dyn CostRemoteAttempt>)>;
}

/// A capability for one already-admitted request's accounting. Settlement may
/// outlive work cancellation, but cannot admit another request or alter limits.
pub trait CostRemoteAttempt: Send {
    fn observe(&mut self, cost: f64) -> Result<CostSnapshot>;
    /// `None` reports invalid billing and quarantines the owning account while
    /// retaining the last valid partial charge. This operation is never replayed.
    /// A supplied deadline bounds cleanup of a group of owned attempts, so
    /// nested channel teardown cannot spend a fresh timeout on every charge.
    fn finish(&mut self, cost: Option<f64>, deadline: Option<Instant>) -> Result<CostSnapshot>;
}

struct CostState {
    snapshot: CostSnapshot,
    pending: BTreeMap<u64, f64>,
    warning: Option<CostWarning>,
    failure: Option<String>,
    account: String,
    own_finalized: f64,
    observed: Coverage,
    retired: BTreeMap<std::path::PathBuf, Retirement>,
}

impl CostState {
    fn finalize(&mut self, cost: f64, deadline: Option<Instant>) -> Result<()> {
        self.snapshot.spent = add_cost(self.snapshot.spent, cost);
        self.own_finalized = add_cost(self.own_finalized, cost);
        self.changed()?;
        let deadline = durability::settlement_deadline(deadline);
        for retired in self.retired.values() {
            if let Err(error) = retired.persist(&self.account, self.own_finalized, deadline) {
                // Keep the valid bill even if its durable acknowledgement fails.
                // A later request must not silently run with incomplete storage.
                self.failure = Some(error.to_string());
                return Err(error);
            }
        }
        Ok(())
    }

    fn import(&mut self, journal: &Journal) -> Result<()> {
        let mut added = 0.0;
        for (account, writer) in &journal.writers {
            if account == &self.account {
                continue;
            }
            let observed = self.observed.entry(account.clone()).or_default();
            added = add_cost(
                added,
                (writer.own_total - observed.max(writer.baseline_own)).max(0.0),
            );
            *observed = observed.max(writer.own_total);
        }
        durability::validate_coverage(&self.observed)?;
        if added > 0.0 {
            self.snapshot.spent = add_cost(self.snapshot.spent, added);
            self.changed()?;
        }
        Ok(())
    }

    fn changed(&mut self) -> Result<()> {
        self.snapshot.pending = self
            .pending
            .values()
            .fold(0.0, |sum, cost| add_cost(sum, *cost));
        let Some(revision) = self.snapshot.revision.checked_add(1) else {
            self.failure = Some("model cost account revisions exhausted".to_string());
            return Err(Error::session("model cost account revisions exhausted"));
        };
        self.snapshot.revision = revision;
        if !self.snapshot.warned
            && let Some(threshold) = self.snapshot.warn_cost
            && self.snapshot.spent >= threshold
        {
            self.snapshot.warned = true;
            let spent = self.snapshot.spent;
            self.warning = Some(CostWarning {
                spent,
                threshold,
                message: format!(
                    "Cost warning: this session has spent ${spent:.4}, reaching its ${threshold:.4} warning threshold (--warn-cost / session.warnCost)."
                ),
            });
        }
        Ok(())
    }
}

/// Clones share monotonic finalized spend, including across model changes and
/// retries. Provisional charges belong to active attempts, not completed bills.
#[derive(Clone)]
pub struct CostBudget {
    state: Arc<Mutex<CostState>>,
    remote: Option<Arc<dyn CostRemote>>,
    origin: Option<Arc<Origin>>,
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
            pending: 0.0,
            revision: 0,
            warn_cost,
            max_cost,
            // A resumed account has already crossed this warning threshold.
            warned: warn_cost.is_some_and(|threshold| recorded_spend >= threshold),
        };
        snapshot.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(CostState {
                snapshot,
                pending: BTreeMap::new(),
                warning: None,
                failure: None,
                account: uuid::Uuid::new_v4().to_string(),
                own_finalized: 0.0,
                observed: Coverage::new(),
                retired: BTreeMap::new(),
            })),
            remote: None,
            origin: None,
        })
    }

    pub fn remote(remote: Arc<dyn CostRemote>) -> Result<Self> {
        let snapshot = remote.snapshot()?;
        snapshot.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(CostState {
                snapshot,
                pending: BTreeMap::new(),
                warning: None,
                failure: None,
                account: uuid::Uuid::new_v4().to_string(),
                own_finalized: 0.0,
                observed: Coverage::new(),
                retired: BTreeMap::new(),
            })),
            remote: Some(remote),
            origin: None,
        })
    }

    #[must_use]
    pub fn same_tree(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// A native child always remains charged to its original parent tree.
    pub fn for_replacement(&self, session: &Session, preserve: bool) -> Result<Self> {
        if self.remote.is_some() {
            return Ok(self.clone());
        }
        if preserve {
            let mut replacement = self.clone();
            replacement.origin = Origin::for_session(session)?;
            replacement.refresh_origin()?;
            return Ok(replacement);
        }
        let snapshot = self.snapshot();
        Self::from_session(session, snapshot.warn_cost, snapshot.max_cost)
    }

    fn from_session(session: &Session, warn: Option<f64>, max: Option<f64>) -> Result<Self> {
        let origin = Origin::for_session(session)?;
        let journal = origin.as_ref().map(|origin| origin.read()).transpose()?;
        let (spent, mut covered) = recorded_account(session, journal.as_ref())?;
        if let Some(journal) = &journal {
            journal.cover(&mut covered)?;
        }
        let mut budget = Self::new(warn, max, spent)?;
        budget.origin = origin;
        budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observed = covered;
        Ok(budget)
    }

    fn refresh_origin(&self) -> Result<()> {
        let Some(origin) = &self.origin else {
            return Ok(());
        };
        let journal = origin
            .read()
            .inspect_err(|error| self.quarantine(&error.to_string()))?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = state.import(&journal) {
            state.failure = Some(error.to_string());
            return Err(error);
        }
        Ok(())
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

    /// Refresh parent accounting and late source charges before admission. A failed accounting
    /// ACK permanently quarantines this handle; retrying cannot hide spend.
    pub fn refresh(&self) -> Result<CostSnapshot> {
        self.check_health()?;
        if let Some(remote) = &self.remote {
            let result = remote.snapshot();
            self.accept_remote(result)?;
        } else {
            self.refresh_origin()?;
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
                // Final bills never fall, but provisional estimates can be
                // replaced by smaller final bills. Only a newer receipt may
                // change pending spend; late replies cannot resurrect it.
                if snapshot.revision <= state.snapshot.revision {
                    return Ok(());
                }
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
        let spent = snapshot.admission_spend();
        (spent >= cap).then(|| CostCapRefusal {
            spent,
            cap,
            message: Error::CostCap { spent, cap }.to_string(),
        })
    }

    pub fn admit(&self) -> Result<()> {
        let snapshot = self.refresh()?;
        let spent = snapshot.admission_spend();
        if let Some(cap) = snapshot.max_cost
            && spent >= cap
        {
            return Err(Error::CostCap { spent, cap });
        }
        Ok(())
    }

    /// Establish ownership before provider dispatch. Parent processes retain
    /// the matching attempt until final billing or child-channel teardown.
    pub fn start_attempt(&self) -> Result<CostAttempt> {
        self.admit()?;
        let source = if let Some(remote) = &self.remote {
            let (snapshot, attempt) = remote.start_attempt().inspect_err(|error| {
                if !matches!(error, Error::CostCap { .. }) {
                    self.quarantine(&error.to_string());
                }
            })?;
            self.accept_remote(Ok(snapshot))?;
            AttemptSource::Remote(attempt)
        } else {
            let id = next_attempt_id()?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.pending.len() >= MAX_PENDING_ATTEMPTS {
                return Err(Error::session("too many active model cost attempts"));
            }
            state.pending.insert(id, 0.0);
            state.changed()?;
            AttemptSource::Local(id)
        };
        Ok(CostAttempt {
            budget: self.clone(),
            source,
            known_cost: 0.0,
            invalid_usage: false,
            settled: false,
        })
    }

    fn quarantine(&self, message: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure = Some(message.to_string());
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
        state.finalize(cost, None)
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
        if self.remote.is_none() {
            self.refresh_origin()?;
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spent = state.snapshot.spent;
        let main_spent = main_spend(session);
        let mut covered = state.observed.clone();
        covered.insert(state.account.clone(), state.own_finalized);
        durability::validate_coverage(&covered)?;
        let data = serde_json::json!({
            "spent": spent,
            "mainSpent": main_spent,
            "account": state.account,
            "ownFinalized": state.own_finalized,
            "lateCovered": covered,
        });
        drop(state);
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
            .is_some_and(|existing| existing == &data);
        if !unchanged {
            let active_leaf = session.leaf_id().map(str::to_owned);
            session.append_custom_entry(
                CHECKPOINT_TYPE.to_string(),
                Some(data),
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

    /// Retain only final-bill persistence for a saved source leaving the
    /// foreground. Call after all fallible replacement preparation, immediately
    /// before installing the replacement, or after the final shutdown save.
    /// No journal is activated by an abandoned checkpoint or an unsaved session.
    pub fn retire_session(&self, session: &Session, owner: &asupersync::Cx) -> Result<()> {
        if self.remote.is_some() || session.path.is_none() {
            return Ok(());
        }
        let origin = Origin::for_session(session)?
            .ok_or_else(|| Error::session("late model cost source is not saved"))?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(retired) = state.retired.get(&origin.key) {
            let result = retired.persist(
                &state.account,
                state.own_finalized,
                durability::settlement_deadline(None),
            );
            if let Err(error) = &result {
                state.failure = Some(error.to_string());
            }
            return result;
        }
        if state.retired.len() >= durability::MAX_TARGETS {
            return Err(Error::session("too many retired model cost sources"));
        }
        let baseline = session
            .entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| {
                let SessionEntry::Custom(entry) = entry else {
                    return None;
                };
                if entry.custom_type != CHECKPOINT_TYPE || !session.entry_is_persisted(index) {
                    return None;
                }
                let data = entry.data.as_ref()?;
                if data.get("account").and_then(serde_json::Value::as_str)
                    != Some(state.account.as_str())
                {
                    return None;
                }
                data.get("ownFinalized").and_then(serde_json::Value::as_f64)
            })
            .filter(|baseline| baseline.is_finite() && *baseline >= 0.0)
            .filter(|baseline| *baseline <= state.own_finalized)
            .ok_or_else(|| Error::session("late model cost retirement requires a saved checkpoint"))?;
        let retired = Retirement::prepare(origin, baseline, owner)?;
        // Hold the account mutex across initial persistence and registration:
        // settlement during the asynchronous source save is included relative
        // to the exact saved counter, and no later settlement can miss its sink.
        if let Err(error) = retired.persist(
            &state.account,
            state.own_finalized,
            durability::settlement_deadline(None),
        ) {
            state.failure = Some(error.to_string());
            return Err(error);
        }
        state.retired.insert(retired.origin.key.clone(), retired);
        Ok(())
    }
}

enum AttemptSource {
    Local(u64),
    Remote(Box<dyn CostRemoteAttempt>),
}

/// Owned accounting for a single provider request. Dropping it settles the last
/// valid partial usage even after work cancellation; it never starts new work.
pub struct CostAttempt {
    budget: CostBudget,
    source: AttemptSource,
    known_cost: f64,
    invalid_usage: bool,
    settled: bool,
}

impl CostAttempt {
    pub fn observe(&mut self, cost: f64) -> Result<()> {
        if self.settled {
            return Err(Error::session("model cost attempt is already settled"));
        }
        if !cost.is_finite() || cost < 0.0 {
            self.invalid_usage = true;
            self.budget.quarantine("provider returned invalid model cost");
            return Err(Error::session("provider returned invalid model cost"));
        }
        if cost <= self.known_cost {
            return Ok(());
        }
        self.known_cost = cost;
        match &mut self.source {
            AttemptSource::Local(id) => {
                let mut state = self
                    .budget
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entry = state
                    .pending
                    .get_mut(id)
                    .ok_or_else(|| Error::session("model cost attempt is unavailable"))?;
                *entry = cost;
                state.changed()
            }
            AttemptSource::Remote(remote) => self.budget.accept_remote(remote.observe(cost)),
        }
    }

    /// Replace a provisional estimate with one final bill. Invalid billing
    /// retains known partial usage and quarantines the entire accounting path.
    /// Existing attempts may settle after quarantine; new admission cannot.
    pub fn finish(&mut self, cost: f64) -> Result<()> {
        self.finish_before(cost, None)
    }

    pub(crate) fn finish_known_before(&mut self, deadline: Instant) -> Result<()> {
        self.finish_before(self.known_cost, Some(deadline))
    }

    fn finish_before(&mut self, cost: f64, deadline: Option<Instant>) -> Result<()> {
        if self.settled {
            return Ok(());
        }
        self.settled = true;
        let valid = !self.invalid_usage && cost.is_finite() && cost >= 0.0;
        if !valid {
            self.budget.quarantine("provider returned invalid model cost");
        }
        let result = match &mut self.source {
            AttemptSource::Local(id) => {
                let mut state = self
                    .budget
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let known = state
                    .pending
                    .remove(id)
                    .ok_or_else(|| Error::session("model cost attempt is unavailable"))?;
                state.finalize(if valid { cost } else { known }, deadline)
            }
            AttemptSource::Remote(remote) => {
                self.budget
                    .accept_remote(remote.finish(valid.then_some(cost), deadline))
            }
        };
        result?;
        if valid {
            Ok(())
        } else {
            Err(Error::session("provider returned invalid model cost"))
        }
    }
}

impl Drop for CostAttempt {
    fn drop(&mut self) {
        if !self.settled
            && let Err(error) = self.finish(self.known_cost)
        {
            tracing::warn!(%error, "provider attempt accounting could not settle");
        }
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
    let origin = Origin::for_session(session)?;
    let journal = origin.as_ref().map(|origin| origin.read()).transpose()?;
    recorded_account(session, journal.as_ref()).map(|(spent, _)| spent)
}

fn recorded_account(session: &Session, journal: Option<&Journal>) -> Result<(f64, Coverage)> {
    if session.has_partial_v2_hydration() {
        let mut complete = session.clone();
        complete.ensure_full_v2_hydration_before_save()?;
        return recorded_account(&complete, journal);
    }
    let main_spent = main_spend(session);
    let mut spent = main_spent;
    let mut observed = Coverage::new();
    let mut checkpoint_found = false;
    for entry in &session.entries {
        let SessionEntry::Custom(entry) = entry else {
            continue;
        };
        if entry.custom_type != CHECKPOINT_TYPE {
            continue;
        }
        let Some(data) = &entry.data else {
            continue;
        };
        let Some(recorded) = data.get("spent").and_then(serde_json::Value::as_f64) else {
            continue;
        };
        let Some(covered) = data.get("mainSpent").and_then(serde_json::Value::as_f64) else {
            continue;
        };
        if !recorded.is_finite() || recorded < 0.0 || !covered.is_finite() || covered < 0.0 {
            continue;
        }
        checkpoint_found = true;
        let coverage = data
            .get("lateCovered")
            .map(|coverage| serde_json::from_value::<Coverage>(coverage.clone()))
            .transpose()?
            .unwrap_or_default();
        durability::validate_coverage(&coverage)?;
        let late = journal.map_or(0.0, |journal| journal.uncovered(&coverage));
        let candidate = add_cost(add_cost(recorded, (main_spent - covered).max(0.0)), late);
        if candidate >= spent {
            spent = candidate;
            observed = coverage;
        }
    }
    if !checkpoint_found
        && let Some(journal) = journal
    {
        spent = add_cost(spent, journal.uncovered(&observed));
    }
    Ok((spent, observed))
}

/// A reload of the same saved source can keep its account live. A copied file
/// with the same header ID is a distinct target and still needs retirement.
pub(crate) fn same_session_target(source: &Session, target: &Session) -> bool {
    if source.header.id != target.header.id {
        return false;
    }
    match (&source.path, &target.path) {
        (None, None) => true,
        (Some(source), Some(target)) if source == target => true,
        (Some(source), Some(target)) => {
            match (std::fs::canonicalize(source), std::fs::canonicalize(target)) {
                (Ok(source), Ok(target)) => source == target,
                _ => false,
            }
        }
        _ => false,
    }
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
    if let Some(shared) = shared {
        return shared.for_replacement(session, true).map(Some);
    }
    let warn_cost = warn_cost.or_else(|| settings.and_then(|settings| settings.warn_cost));
    let max_cost = max_cost.or_else(|| settings.and_then(|settings| settings.max_cost));
    if warn_cost.is_none() && max_cost.is_none() {
        return Ok(None);
    }
    CostBudget::from_session(session, warn_cost, max_cost).map(Some)
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
    fn provisional_usage_blocks_admission_and_final_billing_replaces_it_once() {
        let budget = CostBudget::new(Some(1.0), Some(2.0), 0.0).unwrap();
        let mut attempt = budget.start_attempt().unwrap();
        attempt.observe(2.0).unwrap();
        let provisional = budget.snapshot();
        assert_eq!(provisional.spent, 0.0);
        assert_eq!(provisional.pending, 2.0);
        assert!(matches!(
            budget.admit(),
            Err(Error::CostCap { spent: 2.0, cap: 2.0 })
        ));
        assert!(
            budget.take_warning().is_none(),
            "a provisional estimate is not a final bill"
        );

        let mut session = Session::in_memory();
        budget.checkpoint(&mut session).unwrap();
        assert_eq!(
            recorded_spend(&session).unwrap(),
            0.0,
            "pending estimates cannot become irreversible checkpoints"
        );

        attempt.finish(0.5).unwrap();
        attempt.finish(9.0).unwrap();
        drop(attempt);
        assert_eq!(budget.spent(), 0.5);
        assert_eq!(budget.snapshot().pending, 0.0);
        budget.admit().unwrap();
        budget.accept_remote(Ok(provisional)).unwrap();
        assert_eq!(
            budget.snapshot().pending,
            0.0,
            "a late provisional receipt cannot resurrect the estimate"
        );
        budget.checkpoint(&mut session).unwrap();
        assert_eq!(recorded_spend(&session).unwrap(), 0.5);
    }

    #[test]
    fn dropped_attempts_preserve_known_usage_and_invalid_bills_do_not_refund_it() {
        let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let mut first = budget.start_attempt().unwrap();
        let mut second = budget.start_attempt().unwrap();
        first.observe(0.25).unwrap();
        second.observe(0.5).unwrap();
        drop(first);
        assert_eq!(budget.spent(), 0.25);
        assert_eq!(budget.snapshot().pending, 0.5);
        assert!(second.finish(f64::NAN).is_err());
        assert_eq!(budget.spent(), 0.75);
        assert_eq!(budget.snapshot().pending, 0.0);
        assert!(budget.admit().is_err());
    }

    #[test]
    fn provisional_attempt_storage_is_bounded_and_released_on_drop() {
        let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let attempts = (0..MAX_PENDING_ATTEMPTS)
            .map(|_| budget.start_attempt().unwrap())
            .collect::<Vec<_>>();
        assert!(budget.start_attempt().is_err());
        drop(attempts);
        drop(budget.start_attempt().unwrap());
        assert_eq!(budget.snapshot().pending, 0.0);
        assert_eq!(budget.spent(), 0.0);
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
    fn late_charges_use_the_saved_baseline_and_reopen_without_duplicate_imports() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(100.0), 0.0).unwrap();
        budget.record(5.0).unwrap();
        let mut outstanding = budget.start_attempt().unwrap();
        outstanding.observe(4.0).unwrap();
        budget.checkpoint(&mut session).unwrap();
        runtime.block_on(session.save()).unwrap();
        let path = session.path.clone().unwrap();
        let transcript = std::fs::read(&path).unwrap();

        // This charge arrives after the checkpoint snapshot. A newer staged
        // checkpoint has no receipt and must not replace the saved baseline.
        budget.record(1.0).unwrap();
        budget.checkpoint(&mut session).unwrap();
        budget.retire_session(&session, owner.cx()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), transcript);
        let mut reopened = runtime
            .block_on(Session::open(path.to_str().unwrap()))
            .unwrap();
        let resumed = budget.for_replacement(&reopened, false).unwrap();
        assert_eq!(resumed.spent(), 6.0);

        outstanding.finish(2.0).unwrap();
        assert_eq!(resumed.refresh().unwrap().spent, 8.0);
        assert_eq!(resumed.refresh().unwrap().spent, 8.0);
        resumed.record(0.5).unwrap();
        resumed.checkpoint(&mut reopened).unwrap();
        runtime.block_on(reopened.save()).unwrap();
        assert_eq!(recorded_spend(&reopened).unwrap(), 8.5);
        budget.record(1.0).unwrap();
        assert_eq!(resumed.refresh().unwrap().spent, 9.5);
        assert_eq!(recorded_spend(&reopened).unwrap(), 9.5);
        assert_eq!(budget.spent(), 9.0, "imported or inherited fees are not own charges");
    }

    #[test]
    fn zero_cost_retirement_settles_under_its_canceled_owner_without_touching_the_transcript() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(0.5), 0.0).unwrap();
        let mut outstanding = budget.start_attempt().unwrap();
        outstanding.observe(1.0).unwrap();
        budget.checkpoint(&mut session).unwrap();
        runtime.block_on(session.save()).unwrap();
        let path = session.path.clone().unwrap();
        let transcript = std::fs::read(&path).unwrap();
        owner.set_cancel_requested(true);
        budget.retire_session(&session, owner.cx()).unwrap();
        outstanding.finish(0.5).unwrap();
        drop(outstanding);
        assert_eq!(std::fs::read(&path).unwrap(), transcript);
        let reopened = runtime
            .block_on(Session::open(path.to_str().unwrap()))
            .unwrap();
        assert_eq!(recorded_spend(&reopened).unwrap(), 0.5);
        let resumed = budget.for_replacement(&reopened, false).unwrap();
        assert!(matches!(resumed.admit(), Err(Error::CostCap { .. })));
    }

    #[test]
    fn fork_journals_do_not_import_the_same_foreign_charge_twice() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(100.0), 0.0).unwrap();
        budget.record(1.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        budget.retire_session(&source, owner.cx()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        budget.retire_session(&fork, owner.cx()).unwrap();
        budget.record(2.0).unwrap();

        let reopened = budget.for_replacement(&source, false).unwrap();
        let second_view = reopened.for_replacement(&fork, true).unwrap();
        assert_eq!(reopened.spent(), 3.0);
        assert_eq!(second_view.spent(), 3.0);
        budget.record(1.0).unwrap();
        assert_eq!(reopened.refresh().unwrap().spent, 4.0);
        assert_eq!(second_view.refresh().unwrap().spent, 4.0);
        assert_eq!(recorded_spend(&source).unwrap(), 4.0);
        assert_eq!(recorded_spend(&fork).unwrap(), 4.0);
    }

    #[test]
    fn a_configured_unsaved_path_does_not_create_a_journal_or_grant_retirement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-yet-saved.jsonl");
        let mut session = Session::create();
        session.path = Some(path.clone());
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::from_session(&session, None, Some(1.0)).unwrap();
        budget.checkpoint(&mut session).unwrap();
        assert!(budget.retire_session(&session, owner.cx()).is_err());
        budget.record(0.5).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
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
                pending: 0.0,
                revision: 0,
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

        fn start_attempt(&self) -> Result<(CostSnapshot, Box<dyn CostRemoteAttempt>)> {
            Err(Error::session("this fixture admits only one direct charge"))
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
                pending: 0.0,
                revision: 0,
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
                pending: 0.0,
                revision: 0,
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
