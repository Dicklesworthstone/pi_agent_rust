//! Shared admission and accounting for a session tree's model spend.
//!
//! Limits apply before a request is sent. Already admitted requests finish and
//! may exceed the cap; token pricing and provider-reported costs are estimates,
//! not a reservation of the provider's final invoice.

use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionMessage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod durability;

use durability::{Coverage, Journal, LedgerRef, Origin, ReadOrigin, Retirement, Writer, Writers};

type LedgerRefs = BTreeMap<String, LedgerRef>;

const CHECKPOINT_TYPE: &str = "cost_budget";
const RECORDED_BILLING_FAILURE: &str =
    "recorded model billing is unavailable; cost accounting cannot be restored";
const MAX_PENDING_ATTEMPTS: usize = 1024;
const MAX_ANCESTRY_BYTES: usize = 256 * 1024;
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
    imported: Writers,
    retired: BTreeMap<std::path::PathBuf, Retirement>,
    closure_deadline: Option<Instant>,
}

impl CostState {
    fn finalize(&mut self, cost: f64, deadline: Option<Instant>) -> Result<()> {
        self.closure_deadline = deadline;
        self.snapshot.spent = add_cost(self.snapshot.spent, cost);
        self.own_finalized = add_cost(self.own_finalized, cost);
        self.changed()?;
        self.persist_retired(deadline)
    }

    fn persist_retired(&mut self, deadline: Option<Instant>) -> Result<()> {
        let deadline = durability::settlement_deadline(deadline);
        let mut first_error = None;
        for retired in self.retired.values() {
            if let Err(error) = retired.persist(
                &self.account,
                self.own_finalized,
                self.failure.is_some(),
                &self.imported,
                deadline,
            ) {
                // Keep the valid bill even if its durable acknowledgement fails.
                // A later request must not silently run with incomplete storage.
                self.failure = Some(error.to_string());
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn quarantine(&mut self, message: &str) {
        self.failure = Some(message.to_string());
        // A retired source may never be checkpointed again. Persist the refusal
        // even when no charge is finalized, without changing its known spend.
        if let Err(error) = self.persist_retired(None) {
            tracing::warn!(%error, "retired model cost quarantine could not be persisted");
        }
    }

    fn coverage(&self) -> Coverage {
        self.imported
            .iter()
            .map(|(account, writer)| (account.clone(), writer.own_total))
            .collect()
    }

    fn validate_origins(&self, origins: &BTreeMap<std::path::PathBuf, ReadOrigin>) -> Result<()> {
        let mut keys: BTreeSet<_> = origins
            .iter()
            .filter(|(_, origin)| self.origin_is_live(origin))
            .map(|(path, _)| path)
            .collect();
        keys.extend(self.retired.keys());
        if keys.len() > durability::MAX_TARGETS {
            return Err(Error::session("too many retained model cost sources"));
        }
        let mut accounts: BTreeSet<_> = self.imported.keys().collect();
        accounts.insert(&self.account);
        for origin in origins
            .values()
            .filter(|origin| self.origin_is_live(origin))
        {
            accounts.extend(origin.accounts.iter());
        }
        if accounts.len() > durability::MAX_ACCOUNTS {
            return Err(Error::session("too many retained model cost accounts"));
        }
        Ok(())
    }

    fn origin_is_live(&self, origin: &ReadOrigin) -> bool {
        origin.all_accounts
            || origin.accounts.iter().any(|account| {
                account == &self.account
                    || self
                        .imported
                        .get(account)
                        .is_none_or(|writer| !writer.closed)
            })
    }

    /// Validate the complete merged view before changing the shared account.
    /// A candidate replacement with excessive coverage must leave its source
    /// and all previously retired journals untouched.
    fn import(&mut self, journal: &Journal) -> Result<bool> {
        let mut imported = self.imported.clone();
        let mut added = 0.0;
        let mut changed = false;
        for (account, writer) in &journal.writers {
            if account == &self.account {
                if writer.closed {
                    return Err(Error::session("live model cost account is already closed"));
                }
                continue;
            }
            let observed = imported.entry(account.clone()).or_insert_with(|| {
                changed = true;
                Writer {
                    baseline_own: writer.baseline_own,
                    own_total: writer.baseline_own,
                    billing_unavailable: false,
                    closed: false,
                }
            });
            // Once a writer is known, every later increase belongs to this
            // account. A newer fork's higher baseline must not hide increases
            // between our last observation and that fork's saved checkpoint.
            let delta = (writer.own_total - observed.own_total).max(0.0);
            added = add_cost(added, delta);
            changed |= delta > 0.0
                || (writer.billing_unavailable && !observed.billing_unavailable)
                || (writer.closed && !observed.closed);
            // The first import floor belongs to this tree. Keep it even when
            // another source carries a lower historical floor.
            let baseline = observed.baseline_own;
            durability::merge_writer(observed, writer)?;
            observed.baseline_own = baseline;
        }
        let mut coverage: Coverage = imported
            .iter()
            .map(|(account, writer)| (account.clone(), writer.own_total))
            .collect();
        // Every checkpoint and retirement also publishes the local writer.
        // Reserve that slot before accepting a candidate's foreign coverage.
        coverage.insert(self.account.clone(), self.own_finalized);
        durability::validate_coverage(&coverage)?;
        if added > 0.0 && self.snapshot.revision == u64::MAX {
            return Err(Error::session("model cost account revisions exhausted"));
        }
        self.imported = imported;
        if added > 0.0 {
            self.snapshot.spent = add_cost(self.snapshot.spent, added);
            self.changed()?;
        }
        // The refusal is valid accounting data. Import the known lower bound
        // and preserve it in checkpoints; admission checks health separately.
        if journal.billing_unavailable() && self.failure.is_none() {
            self.failure = Some(RECORDED_BILLING_FAILURE.to_string());
            changed = true;
        }
        Ok(changed)
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

impl Drop for CostState {
    fn drop(&mut self) {
        // Every CostAttempt retains this state until its terminal settlement.
        // A crash or failed final publication leaves the writer unresolved;
        // readers must never infer closure from time or process liveness.
        if !self.pending.is_empty() {
            return;
        }
        let deadline = durability::settlement_deadline(self.closure_deadline);
        for retired in self.retired.values() {
            if let Err(error) = retired.persist_with_closed(
                &self.account,
                self.own_finalized,
                self.failure.is_some(),
                true,
                &self.imported,
                deadline,
            ) {
                tracing::warn!(%error, "final model cost writer closure could not be persisted");
            }
        }
    }
}

/// Clones share monotonic finalized spend, including across model changes and
/// retries. Provisional charges belong to active attempts, not completed bills.
#[derive(Clone)]
pub struct CostBudget {
    state: Arc<Mutex<CostState>>,
    remote: Option<Arc<dyn CostRemote>>,
    // A replacement owns its read ancestry. Preparing and then abandoning a
    // candidate never registers its path in another live handle's ancestry.
    origins: BTreeMap<std::path::PathBuf, ReadOrigin>,
    // A prospective source belongs only to this prepared clone. It is not a
    // write sink until retire_session acknowledges the saved handoff.
    prepared_origin: Option<Arc<Origin>>,
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
                imported: Writers::new(),
                retired: BTreeMap::new(),
                closure_deadline: None,
            })),
            remote: None,
            origins: BTreeMap::new(),
            prepared_origin: None,
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
                imported: Writers::new(),
                retired: BTreeMap::new(),
                closure_deadline: None,
            })),
            remote: Some(remote),
            origins: BTreeMap::new(),
            prepared_origin: None,
        })
    }

    #[must_use]
    pub fn same_tree(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// A native child always remains charged to its original parent tree.
    pub fn for_replacement(&self, session: &Session, preserve: bool) -> Result<Self> {
        if self.remote.is_some() || preserve {
            let (account, authority) = self.reference_authority();
            let (origins, mut journal, _) =
                load_session_ledgers(session, Some(&account), authority.as_ref())?;
            if self.remote.is_some() {
                if recorded_account(session, Some(&journal))?.2 {
                    return Err(Error::session(RECORDED_BILLING_FAILURE));
                }
                return Ok(self.clone());
            }
            let mut replacement = self.clone();
            let mut inherited = BTreeMap::new();
            for (key, origin) in &replacement.origins {
                if origin.all_accounts {
                    let (observed, scoped) = origin.scoped(&account)?;
                    journal.merge(&observed)?;
                    if let Some(origin) = scoped {
                        inherited.insert(key.clone(), origin);
                    }
                } else {
                    inherited.insert(key.clone(), origin.clone());
                }
            }
            // Include terminal bills discovered while demoting the departing
            // native source. Dropping their refs must not drop their counters.
            // Validate everything before touching the source's shared account.
            if recorded_account(session, Some(&journal))?.2 {
                return Err(Error::session(RECORDED_BILLING_FAILURE));
            }
            replacement.origins = inherited;
            merge_origins(&mut replacement.origins, origins)?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            replacement
                .origins
                .retain(|_, origin| state.origin_is_live(origin));
            state.validate_origins(&replacement.origins)?;
            if state.import(&journal)? {
                state.persist_retired(None)?;
            }
            drop(state);
            return Ok(replacement);
        }
        let snapshot = self.snapshot();
        Self::from_session(session, snapshot.warn_cost, snapshot.max_cost)
    }

    fn from_session(session: &Session, warn: Option<f64>, max: Option<f64>) -> Result<Self> {
        let (origins, journal, closed) = load_session_ledgers(session, None, None)?;
        let (spent, mut covered, unavailable) = recorded_account(session, Some(&journal))?;
        journal.cover(&mut covered)?;
        let mut known_accounts: BTreeSet<_> = covered.keys().collect();
        for origin in origins.values() {
            known_accounts.extend(origin.accounts.iter());
        }
        if known_accounts.len() >= durability::MAX_ACCOUNTS {
            return Err(Error::session("too many imported model cost accounts"));
        }
        drop(known_accounts);
        let mut budget = Self::new(warn, max, spent)?;
        budget.origins = origins;
        let imported = covered
            .into_iter()
            .map(|(account, total)| {
                let closed = closed.contains(&account)
                    || journal
                        .writers
                        .get(&account)
                        .is_some_and(|writer| writer.closed);
                (
                    account,
                    Writer {
                        baseline_own: total,
                        own_total: total,
                        billing_unavailable: false,
                        closed,
                    },
                )
            })
            .collect();
        {
            let mut state = budget
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.imported = imported;
            if unavailable {
                state.failure = Some(RECORDED_BILLING_FAILURE.to_string());
            }
        }
        Ok(budget)
    }

    fn reference_authority(&self) -> (String, Option<LedgerRef>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.account.clone(),
            state
                .retired
                .values()
                .next()
                .map(|retired| retired.origin.reference())
                .or_else(|| {
                    self.prepared_origin
                        .as_ref()
                        .map(|origin| origin.reference())
                }),
        )
    }

    /// Prepare durable ancestry without activating a write sink or changing
    /// the live source account. The source must already have an acknowledged
    /// checkpoint. Save a checkpoint made by this clone into the source and
    /// candidate, then retire the source immediately before installing it.
    pub fn prepare_handoff(&self, saved_source: &Session) -> Result<Self> {
        if self.remote.is_some() {
            return Ok(self.clone());
        }
        let mut prepared = self.clone();
        let (account, authority) = self.reference_authority();
        saved_checkpoint(saved_source, &account)?;
        if self.prepared_origin.is_some()
            && self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retired
                .is_empty()
        {
            let source = Origin::for_session(saved_source)?
                .ok_or_else(|| Error::session("model cost handoff requires a saved source"))?;
            if self
                .prepared_origin
                .as_ref()
                .is_some_and(|origin| origin.reference() != source.reference())
            {
                return Err(Error::session("prepared model cost source changed"));
            }
        }
        if authority.is_none() {
            let origin = Origin::for_session(saved_source)?
                .ok_or_else(|| Error::session("model cost handoff requires a saved source"))?;
            origin.validate_source()?;
            origin.reference().validate()?;
            prepared.prepared_origin = Some(origin);
        }
        Ok(prepared)
    }

    fn retained_origins(&self) -> Result<BTreeMap<std::path::PathBuf, ReadOrigin>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut origins = self.origins.clone();
        origins.retain(|_, origin| state.origin_is_live(origin));
        for (path, retired) in &state.retired {
            let mut origin = retired.reader();
            if let Some(previous) = origins.remove(path) {
                origin.merge(previous)?;
            }
            origins.insert(path.clone(), origin);
        }
        state.validate_origins(&origins)?;
        Ok(origins)
    }

    fn refresh_origin(&self) -> Result<()> {
        let origins = self.retained_origins()?;
        let mut journals = Vec::with_capacity(origins.len());
        let mut read_error = None;
        for origin in origins.values() {
            // Read without holding the account mutex or another journal lock.
            match origin.read() {
                Ok(journal) => journals.push(journal),
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
            }
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        for journal in &journals {
            match state.import(journal) {
                Ok(imported) => changed |= imported,
                Err(error) => {
                    state.quarantine(&error.to_string());
                    return Err(error);
                }
            }
        }
        if let Some(error) = read_error {
            // Preserve charges from sources already read before reporting the
            // unavailable source, including their durable lower bounds.
            state.quarantine(&error.to_string());
            return Err(error);
        }
        if changed {
            // One deadline covers all sinks, regardless of how many ancestors
            // were read. Repeated unchanged imports do not rewrite journals.
            state.persist_retired(None)?;
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
            .quarantine(message);
    }

    /// Record exactly once per provider attempt, including known usage from a
    /// failed/aborted response. Invalid usage quarantines subsequent admission.
    pub fn record(&self, cost: f64) -> Result<()> {
        if !cost.is_finite() || cost < 0.0 {
            let message = "provider returned invalid model cost; further requests are blocked";
            self.quarantine(message);
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
        let origins = self.retained_origins()?;
        let (_, own_reference) = self.reference_authority();
        let mut source_accounts = BTreeMap::new();
        for (key, origin) in &origins {
            if origin.all_accounts {
                source_accounts.insert(
                    key.clone(),
                    origin.read()?.writers.into_keys().collect::<BTreeSet<_>>(),
                );
            }
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if recorded_billing_unavailable(session) && state.failure.is_none() {
            state.quarantine(RECORDED_BILLING_FAILURE);
        }
        let spent = state.snapshot.spent;
        let main_spent = main_spend(session);
        let mut covered = state.coverage();
        covered.insert(state.account.clone(), state.own_finalized);
        durability::validate_coverage(&covered)?;
        let mut references = LedgerRefs::new();
        for (key, origin) in origins
            .iter()
            .filter(|(_, origin)| !origin.all_accounts)
            .chain(origins.iter().filter(|(_, origin)| origin.all_accounts))
        {
            for account in source_accounts.get(key).unwrap_or(&origin.accounts) {
                if account != &state.account
                    && state
                        .imported
                        .get(account)
                        .is_none_or(|writer| !writer.closed)
                {
                    references
                        .entry(account.clone())
                        .or_insert_with(|| origin.reference());
                }
            }
        }
        if let Some(reference) = own_reference {
            reference.validate()?;
            references.insert(state.account.clone(), reference);
        }
        validate_references(&references)?;
        let mut all_accounts = covered.clone();
        for account in references.keys() {
            all_accounts.entry(account.clone()).or_insert(0.0);
        }
        durability::validate_coverage(&all_accounts)?;
        let closed: BTreeSet<_> = state
            .imported
            .iter()
            .filter(|(_, writer)| writer.closed)
            .map(|(account, _)| account.clone())
            .collect();
        let mut data = serde_json::json!({
            "spent": spent,
            "mainSpent": main_spent,
            "account": state.account,
            "ownFinalized": state.own_finalized,
            "lateCovered": covered,
            "lateVersion": 1,
        });
        if !references.is_empty() {
            data["lateSources"] = serde_json::to_value(references)?;
        }
        if !closed.is_empty() {
            data["lateClosed"] = serde_json::to_value(closed)?;
        }
        if state.failure.is_some() {
            // Persist only a bounded stable marker. Detailed live failures
            // may contain transport diagnostics and must not enter storage.
            data["failure"] = serde_json::json!(RECORDED_BILLING_FAILURE);
        }
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
        if self
            .prepared_origin
            .as_ref()
            .is_some_and(|prepared| prepared.reference() != origin.reference())
        {
            // A later fork is allowed to retain an already activated canonical
            // sink, but the first commit must target the prepared source.
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.retired.is_empty() {
                return Err(Error::session("prepared model cost source changed"));
            }
        }
        let mut origins = self.retained_origins()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let checkpoint = saved_checkpoint(session, &state.account)?;
        if let Some(canonical) = state.retired.values().next()
            && canonical.origin.key != origin.key
        {
            let references: LedgerRefs = checkpoint
                .get("lateSources")
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()?
                .unwrap_or_default();
            if references.get(&state.account) != Some(&canonical.origin.reference()) {
                return Err(Error::session(
                    "saved model cost checkpoint lost its canonical source",
                ));
            }
            // Every later session carries the same writer ref. Retaining a new
            // sink for each fork would make the safety bound a lifetime limit.
            return state.persist_retired(None);
        }
        origins.insert(
            origin.key.clone(),
            ReadOrigin::from_source(Arc::clone(&origin), BTreeSet::from([state.account.clone()])),
        );
        state.validate_origins(&origins)?;
        if let Some(retired) = state.retired.get(&origin.key) {
            let result = retired.persist(
                &state.account,
                state.own_finalized,
                state.failure.is_some(),
                &state.imported,
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
        let baseline = checkpoint
            .get("ownFinalized")
            .and_then(serde_json::Value::as_f64)
            .filter(|baseline| baseline.is_finite() && *baseline >= 0.0)
            .filter(|baseline| *baseline <= state.own_finalized)
            .ok_or_else(|| {
                Error::session("late model cost retirement requires a saved checkpoint")
            })?;
        let covered = checkpoint
            .get("lateCovered")
            .map(|value| serde_json::from_value::<Coverage>(value.clone()))
            .transpose()?
            .unwrap_or_default();
        if covered.iter().any(|(account, total)| {
            account != &state.account
                && state
                    .imported
                    .get(account)
                    .is_none_or(|writer| writer.own_total < *total)
        }) {
            return Err(Error::session(
                "saved model cost coverage exceeds the live account",
            ));
        }
        let retired = Retirement::prepare(origin, baseline, covered, owner)?;
        // Hold the account mutex across initial persistence and registration:
        // settlement during the asynchronous source save is included relative
        // to the exact saved counter, and no later settlement can miss its sink.
        if let Err(error) = retired.persist(
            &state.account,
            state.own_finalized,
            state.failure.is_some(),
            &state.imported,
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
        let result = match &mut self.source {
            AttemptSource::Local(id) => {
                let mut state = self
                    .budget
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !valid {
                    // Finalization writes the failure and known charge together
                    // under its supplied cleanup deadline. Do not start a
                    // separate quarantine write with a fresh timeout here.
                    state.failure = Some("provider returned invalid model cost".to_string());
                }
                let known = state
                    .pending
                    .remove(id)
                    .ok_or_else(|| Error::session("model cost attempt is unavailable"))?;
                state.finalize(if valid { cost } else { known }, deadline)
            }
            AttemptSource::Remote(remote) => {
                if !valid {
                    // Remote budgets never acquire retired local sinks. The
                    // parent receives the failure through finish(None) below.
                    self.budget
                        .quarantine("provider returned invalid model cost");
                }
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

fn recorded_billing_unavailable(session: &Session) -> bool {
    session.entries.iter().any(|entry| match entry {
        SessionEntry::Message(entry) => match &entry.message {
            SessionMessage::Assistant { message } => matches!(
                message.usage.billing,
                Some(crate::model::BillingUsage::Unavailable { .. })
            ),
            _ => false,
        },
        SessionEntry::Custom(entry) if entry.custom_type == CHECKPOINT_TYPE => entry
            .data
            .as_ref()
            .and_then(|data| data.get("failure"))
            .is_some_and(|failure| !failure.is_null()),
        _ => false,
    })
}

fn validate_recorded_billing(session: &Session, journal: Option<&Journal>) -> Result<()> {
    if session.has_partial_v2_hydration() {
        let mut complete = session.clone();
        complete.ensure_full_v2_hydration_before_save()?;
        return validate_recorded_billing(&complete, journal);
    }
    if recorded_billing_unavailable(session) {
        return Err(Error::session(RECORDED_BILLING_FAILURE));
    }
    if let Some(journal) = journal {
        journal.ensure_billing_available()?;
    }
    Ok(())
}

fn latest_checkpoint(session: &Session) -> Option<&serde_json::Value> {
    session.entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Custom(entry) if entry.custom_type == CHECKPOINT_TYPE => entry.data.as_ref(),
        _ => None,
    })
}

fn saved_checkpoint<'a>(session: &'a Session, account: &str) -> Result<&'a serde_json::Value> {
    session
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
            (data.get("account").and_then(serde_json::Value::as_str) == Some(account))
                .then_some(data)
        })
        .ok_or_else(|| Error::session("late model cost retirement requires a saved checkpoint"))
}

fn merge_origins(
    destination: &mut BTreeMap<std::path::PathBuf, ReadOrigin>,
    incoming: BTreeMap<std::path::PathBuf, ReadOrigin>,
) -> Result<()> {
    for (key, origin) in incoming {
        if let Some(previous) = destination.get_mut(&key) {
            previous.merge(origin)?;
        } else {
            destination.insert(key, origin);
        }
    }
    Ok(())
}

fn validate_references(references: &LedgerRefs) -> Result<()> {
    if references.len() > durability::MAX_ACCOUNTS
        || references
            .keys()
            .any(|account| uuid::Uuid::parse_str(account).is_err())
        || references
            .values()
            .map(LedgerRef::key)
            .collect::<BTreeSet<_>>()
            .len()
            > durability::MAX_TARGETS
    {
        return Err(Error::session("too many retained model cost sources"));
    }
    for reference in references.values() {
        reference.validate()?;
    }
    if serde_json::to_vec(references)?.len() > MAX_ANCESTRY_BYTES {
        return Err(Error::session(
            "persisted model cost ancestry exceeds its size limit",
        ));
    }
    Ok(())
}

/// Reconstruct only bounded ledger reads. References never consult source
/// transcripts, SQLite stores, parent-session links, or another ledger's refs.
fn load_session_ledgers(
    session: &Session,
    live_account: Option<&str>,
    own_authority: Option<&LedgerRef>,
) -> Result<(
    BTreeMap<std::path::PathBuf, ReadOrigin>,
    Journal,
    BTreeSet<String>,
)> {
    if session.has_partial_v2_hydration() {
        let mut complete = session.clone();
        complete.ensure_full_v2_hydration_before_save()?;
        return load_session_ledgers(&complete, live_account, own_authority);
    }
    let checkpoint = latest_checkpoint(session);
    if checkpoint
        .and_then(|data| data.get("lateVersion"))
        .is_some_and(|version| version.as_u64() != Some(1))
    {
        return Err(Error::session("unsupported model cost ancestry version"));
    }
    let mut references: LedgerRefs = checkpoint
        .and_then(|data| data.get("lateSources"))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    let closed: BTreeSet<String> = checkpoint
        .and_then(|data| data.get("lateClosed"))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    let covered: Coverage = checkpoint
        .and_then(|data| data.get("lateCovered"))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    durability::validate_coverage(&covered)?;
    if closed.len() > durability::MAX_ACCOUNTS
        || closed.iter().any(|account| !covered.contains_key(account))
    {
        return Err(Error::session("invalid persisted model cost ancestry"));
    }
    validate_references(&references)?;
    let source = Origin::for_session(session)?;
    let source_reference = source.as_ref().map(|source| source.reference());
    let mut journal = source.as_ref().map_or_else(
        || Ok(Journal::empty(&session.header.id)),
        |source| source.read(),
    )?;
    let root_accounts: BTreeSet<_> = journal.writers.keys().cloned().collect();
    if let Some(reference) = &source_reference {
        for account in journal.writers.keys() {
            references
                .entry(account.clone())
                .or_insert_with(|| reference.clone());
        }
        // The source may be saved before its first retirement. Remember its
        // checkpoint writer so a later committed registration is observable.
        if checkpoint.is_none_or(|data| data.get("lateVersion").is_none())
            && let Some(account) = checkpoint
                .and_then(|data| data.get("account"))
                .and_then(serde_json::Value::as_str)
        {
            if uuid::Uuid::parse_str(account).is_err() {
                return Err(Error::session("invalid persisted model cost account"));
            }
            references
                .entry(account.to_owned())
                .or_insert_with(|| reference.clone());
        }
    }
    for account in &closed {
        if let Some(writer) = journal.writers.get(account)
            && (writer.own_total > covered[account]
                || (writer.closed && writer.own_total != covered[account]))
        {
            return Err(Error::session("closed model cost counter changed"));
        }
        references.remove(account);
    }
    validate_references(&references)?;
    let mut grouped: BTreeMap<std::path::PathBuf, (LedgerRef, BTreeSet<String>)> = BTreeMap::new();
    for (account, reference) in references {
        if live_account == Some(account.as_str()) {
            if own_authority.is_some_and(|authority| authority != &reference)
                || (own_authority.is_none() && source_reference.as_ref() != Some(&reference))
            {
                return Err(Error::session("prepared model cost source changed"));
            }
            continue;
        }
        let group = grouped
            .entry(reference.key())
            .or_insert_with(|| (reference.clone(), BTreeSet::new()));
        if group.0 != reference {
            return Err(Error::session("model cost ledger identity changed"));
        }
        group.1.insert(account);
    }
    let source_extra = usize::from(
        source
            .as_ref()
            .is_some_and(|source| !grouped.contains_key(&source.key)),
    );
    if grouped.len().saturating_add(source_extra) > durability::MAX_TARGETS {
        return Err(Error::session("too many retained model cost sources"));
    }
    let mut origins = BTreeMap::new();
    for (key, (reference, accounts)) in grouped {
        let required = source_reference.as_ref() != Some(&reference);
        let origin = if let Some(source) = &source
            && source.reference() == reference
        {
            let prospective = checkpoint
                .and_then(|data| data.get("account"))
                .and_then(serde_json::Value::as_str);
            let required = accounts
                .iter()
                .filter(|account| {
                    prospective != Some(account.as_str()) || root_accounts.contains(*account)
                })
                .cloned()
                .collect();
            let mut origin = ReadOrigin::from_source(Arc::clone(source), accounts);
            origin.require(required);
            origin
        } else {
            reference.open(accounts)?
        };
        let observed = origin.read()?;
        for (account, writer) in &observed.writers {
            if (writer.closed || required)
                && covered
                    .get(account)
                    .is_some_and(|covered| *covered > writer.own_total)
            {
                return Err(Error::session("closed model cost counter changed"));
            }
        }
        journal.merge(&observed)?;
        origins.insert(key, origin);
    }
    if let Some(source) = source {
        let mut accounts: BTreeSet<_> = root_accounts
            .into_iter()
            .filter(|account| !closed.contains(account))
            .collect();
        // Only the current native source can gain newly registered writers.
        // Serialized ancestry remains scoped to writers actually observed.
        let origin = if let Some(mut previous) = origins.remove(&source.key) {
            previous.accounts.extend(accounts);
            previous.all_accounts = true;
            previous
        } else {
            let mut origin = ReadOrigin::watch_source(Arc::clone(&source), accounts.clone());
            // Root writers read before grouping are already observed facts,
            // even when the live account omitted its own serialized ref.
            origin.require(std::mem::take(&mut accounts));
            origin
        };
        origins.insert(source.key.clone(), origin);
    }
    Ok((origins, journal, closed))
}

/// Account for every retained branch, plus auxiliary/child spend checkpoints.
pub fn recorded_spend(session: &Session) -> Result<f64> {
    let (_, journal, _) = load_session_ledgers(session, None, None)?;
    let (spent, _, unavailable) = recorded_account(session, Some(&journal))?;
    if unavailable {
        return Err(Error::session(RECORDED_BILLING_FAILURE));
    }
    Ok(spent)
}

fn recorded_account(session: &Session, journal: Option<&Journal>) -> Result<(f64, Coverage, bool)> {
    if session.has_partial_v2_hydration() {
        let mut complete = session.clone();
        complete.ensure_full_v2_hydration_before_save()?;
        return recorded_account(&complete, journal);
    }
    // A finite retained amount is only the known lower bound when the
    // provider or a prior live account reported unavailable billing.
    validate_recorded_billing(session, journal)?;
    let main_spent = main_spend(session);
    if let Some(frontier) = latest_checkpoint(session)
        .filter(|data| data.get("lateVersion").and_then(serde_json::Value::as_u64) == Some(1))
    {
        return recorded_frontier(session, frontier, journal, main_spent);
    }
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
    Ok((spent, observed, false))
}

/// A complete modern checkpoint supersedes historical main/ledger deltas.
/// Reapplying those deltas to older checkpoints would count the same primary
/// bill twice. Historical finalized totals still establish a nonrefundable floor.
fn recorded_frontier(
    session: &Session,
    frontier: &serde_json::Value,
    journal: Option<&Journal>,
    main_spent: f64,
) -> Result<(f64, Coverage, bool)> {
    let amount = |key: &str| {
        frontier
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .filter(|amount| amount.is_finite() && *amount >= 0.0)
            .ok_or_else(|| Error::session("invalid model cost accounting frontier"))
    };
    let recorded = amount("spent")?;
    let main_covered = amount("mainSpent")?;
    let account = frontier
        .get("account")
        .and_then(serde_json::Value::as_str)
        .filter(|account| uuid::Uuid::parse_str(account).is_ok())
        .ok_or_else(|| Error::session("invalid model cost accounting frontier"))?;
    let coverage: Coverage = frontier
        .get("lateCovered")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    durability::validate_coverage(&coverage)?;
    if !coverage.contains_key(account) {
        return Err(Error::session("invalid model cost accounting frontier"));
    }
    let historical_floor = session
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Custom(entry) if entry.custom_type == CHECKPOINT_TYPE => entry
                .data
                .as_ref()
                .and_then(|data| data.get("spent"))
                .and_then(serde_json::Value::as_f64)
                .filter(|spent| spent.is_finite() && *spent >= 0.0),
            _ => None,
        })
        .fold(main_spent, f64::max);
    let ledger_late = journal.map_or(0.0, |journal| journal.uncovered_since(&coverage));
    let primary_late = (main_spent - main_covered).max(0.0);
    let unavailable = primary_late > 0.0 && ledger_late > 0.0;
    // Without a newer receipt, primary rows cannot identify whether this
    // ledger delta is their bill or separate background work. This applies to
    // foreign writers too: a shared transcript has no per-message account ID.
    // Preserve the lower bound and fence admission instead of guessing overlap.
    let candidate = add_cost(recorded, primary_late.max(ledger_late));
    Ok((historical_floor.max(candidate), coverage, unavailable))
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
        return inherited.for_replacement(session, true).map(Some);
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
    #[allow(clippy::too_many_lines)] // Exercise the complete retirement and reopen lifecycle together.
    fn late_billing_failure_quarantines_live_and_reopened_accounts() {
        fn assert_unavailable<T: std::fmt::Debug>(result: Result<T>) {
            assert!(matches!(
                result.unwrap_err(),
                Error::Session(message) if message == RECORDED_BILLING_FAILURE
            ));
        }

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for with_v2_sidecar in [false, true] {
            for through_attempt in [false, true] {
                for invalid in [-1.0, f64::NAN, f64::INFINITY] {
                    let directory = tempfile::tempdir().unwrap();
                    let path = directory.path().join("late-unavailable-billing.jsonl");
                    let mut session = Session::create();
                    session.path = Some(path.clone());
                    session.append_message(assistant(1.0));
                    let owner = crate::agent_cx::AgentCx::for_testing_with_io();
                    let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
                    budget.record(1.0).unwrap();
                    let mut failed = budget.start_attempt().unwrap();
                    failed.observe(0.25).unwrap();
                    let mut surviving = budget.start_attempt().unwrap();
                    surviving.observe(0.5).unwrap();
                    budget.checkpoint(&mut session).unwrap();
                    runtime.block_on(session.save()).unwrap();
                    let transcript = std::fs::read(&path).unwrap();
                    budget.retire_session(&session, owner.cx()).unwrap();
                    let resumed = CostBudget::from_session(&session, None, Some(10.0)).unwrap();
                    resumed.admit().unwrap();
                    let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
                    resumed.checkpoint(&mut fork).unwrap();
                    runtime.block_on(fork.save()).unwrap();
                    resumed.retire_session(&fork, owner.cx()).unwrap();

                    if through_attempt {
                        assert!(failed.finish(invalid).is_err());
                        assert_eq!(budget.spent(), 1.25);
                    } else {
                        assert!(budget.record(invalid).is_err());
                        assert_eq!(budget.spent(), 1.0, "invalid records add no charge");
                    }
                    // In the direct-record case this precedes any finalization:
                    // quarantine itself must write the durable refusal.
                    assert_unavailable(recorded_spend(&session));
                    assert!(budget.admit().is_err());
                    if !through_attempt {
                        failed.finish(0.25).unwrap();
                    }
                    budget.quarantine("private transport diagnostic");
                    // Work admitted before quarantine still owns its final bill.
                    surviving.finish(0.5).unwrap();
                    assert_eq!(budget.spent(), 1.75);
                    assert_eq!(std::fs::read(&path).unwrap(), transcript);

                    let origin = Origin::for_session(&session).unwrap().unwrap();
                    let account = budget
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .account
                        .clone();
                    let journal = origin.read().unwrap();
                    assert_eq!(journal.writers[&account].own_total, 1.75);
                    assert!(journal.writers[&account].billing_unavailable);
                    // A stale healthy acknowledgement must never erase a
                    // refusal already written by this account.
                    let stale =
                        Retirement::prepare(Arc::clone(&origin), 1.0, Coverage::new(), owner.cx())
                            .unwrap();
                    stale
                        .persist(
                            &account,
                            1.75,
                            false,
                            &Writers::new(),
                            durability::settlement_deadline(None),
                        )
                        .unwrap();
                    assert!(origin.read().unwrap().writers[&account].billing_unavailable);
                    let encoded = std::fs::read_to_string(&origin.key).unwrap();
                    assert!(!encoded.contains("private transport diagnostic"));
                    assert!(!encoded.contains("NaN"));

                    assert!(resumed.refresh().is_err());
                    assert_eq!(resumed.spent(), 1.75, "retain the known lower bound");
                    assert!(resumed.admit().is_err());
                    assert_unavailable(recorded_spend(&fork));
                    // Refusing new requests must not prevent saving the
                    // known charges and refusal during shutdown or replacement.
                    resumed.checkpoint(&mut fork).unwrap();
                    runtime.block_on(fork.save()).unwrap();
                    let SessionEntry::Custom(checkpoint) = fork.entries.last().unwrap() else {
                        panic!("cost checkpoint");
                    };
                    assert_eq!(
                        checkpoint.data.as_ref().unwrap()["failure"],
                        RECORDED_BILLING_FAILURE
                    );

                    let reopened = if with_v2_sidecar {
                        let store = crate::session::create_v2_sidecar_from_jsonl(&path).unwrap();
                        let (mut reopened, diagnostics) = Session::open_from_v2(
                            &store,
                            session.header.clone(),
                            crate::session::V2OpenMode::ActivePath,
                        )
                        .unwrap();
                        assert!(diagnostics.skipped_entries.is_empty());
                        reopened.path = Some(path.clone());
                        reopened
                    } else {
                        runtime
                            .block_on(Session::open(path.to_str().unwrap()))
                            .unwrap()
                    };
                    assert_unavailable(recorded_spend(&reopened));
                    assert_unavailable(CostBudget::from_session(&reopened, None, Some(10.0)));
                    let current = CostBudget::new(None, Some(10.0), 2.0).unwrap();
                    for preserve in [false, true] {
                        assert_unavailable(current.for_replacement(&reopened, preserve));
                        current.admit().unwrap();
                        assert_eq!(
                            current.spent(),
                            2.0,
                            "a refused switch leaves its source intact"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_zero_usage_observation_quarantines_a_legacy_retired_journal() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let mut outstanding = budget.start_attempt().unwrap();
        budget.checkpoint(&mut session).unwrap();
        runtime.block_on(session.save()).unwrap();
        budget.retire_session(&session, owner.cx()).unwrap();
        let origin = Origin::for_session(&session).unwrap().unwrap();

        // Journals written before this field existed remain readable. Use the
        // real journal reader so the schema check and file admission both run.
        let mut legacy = serde_json::to_value(origin.read().unwrap()).unwrap();
        legacy["version"] = serde_json::json!(1);
        for writer in legacy["writers"].as_object_mut().unwrap().values_mut() {
            let _ = writer.as_object_mut().unwrap().remove("billingUnavailable");
            let _ = writer.as_object_mut().unwrap().remove("closed");
        }
        std::fs::write(&origin.key, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(recorded_spend(&session).unwrap(), 0.0);
        assert!(
            origin
                .read()
                .unwrap()
                .writers
                .values()
                .all(|writer| !writer.billing_unavailable && !writer.closed)
        );

        assert!(outstanding.observe(f64::NAN).is_err());
        assert!(matches!(
            recorded_spend(&session),
            Err(Error::Session(message)) if message == RECORDED_BILLING_FAILURE
        ));
        assert_eq!(budget.spent(), 0.0);
        drop(outstanding);
        assert_eq!(budget.snapshot().pending, 0.0);
        assert!(
            origin
                .read()
                .unwrap()
                .writers
                .values()
                .all(|writer| writer.billing_unavailable && writer.own_total == 0.0)
        );
        assert!(matches!(
            CostBudget::from_session(&session, None, Some(1.0)),
            Err(Error::Session(message)) if message == RECORDED_BILLING_FAILURE
        ));
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
    #[allow(clippy::too_many_lines)] // Follow two independent accounts through durable forks and final closure.
    fn cold_forks_recover_ancestor_bills_after_every_observer_has_closed() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let stores = [
            crate::session::SessionStoreKind::Jsonl,
            #[cfg(feature = "sqlite-sessions")]
            crate::session::SessionStoreKind::Sqlite,
        ];
        for store in stores {
            let directory = tempfile::tempdir().unwrap();
            let owner = crate::agent_cx::AgentCx::for_testing_with_io();
            let original = CostBudget::new(None, Some(5.0), 0.0).unwrap();
            original.record(2.0).unwrap();
            let mut outstanding = original.start_attempt().unwrap();
            outstanding.observe(1.0).unwrap();
            let mut source =
                Session::create_with_dir_and_store(Some(directory.path().to_path_buf()), store);
            original.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            let prepared = original.prepare_handoff(&source).unwrap();
            prepared.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            prepared.retire_session(&source, owner.cx()).unwrap();
            drop(prepared);

            let mut observer = CostBudget::from_session(&source, None, Some(5.0)).unwrap();
            observer.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            observer = observer.prepare_handoff(&source).unwrap();
            observer.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            let mut fork =
                Session::create_with_dir_and_store(Some(directory.path().to_path_buf()), store);
            observer.checkpoint(&mut fork).unwrap();
            runtime.block_on(fork.save()).unwrap();
            observer.retire_session(&source, owner.cx()).unwrap();
            let mut descendant =
                Session::create_with_dir_and_store(Some(directory.path().to_path_buf()), store);
            observer.checkpoint(&mut descendant).unwrap();
            runtime.block_on(descendant.save()).unwrap();
            observer.retire_session(&fork, owner.cx()).unwrap();
            let descendant_path = descendant.path.clone().unwrap();
            let descendant_bytes = std::fs::read(&descendant_path).unwrap();
            let source_ledger = Origin::for_session(&source).unwrap().unwrap();
            let original_id = original.state.lock().unwrap().account.clone();
            let observer_id = observer.state.lock().unwrap().account.clone();
            drop(observer);
            assert!(source_ledger.read().unwrap().writers[&observer_id].closed);

            // No observer remains to relay this bill into either descendant.
            outstanding.finish(3.0).unwrap();
            drop(outstanding);
            drop(original);
            let terminal = source_ledger.read().unwrap();
            assert_eq!(terminal.writers[&original_id].own_total, 5.0);
            assert!(terminal.writers[&original_id].closed);
            assert_eq!(std::fs::read(&descendant_path).unwrap(), descendant_bytes);
            // A cold descendant needs the small ledger, not its old transcript
            // or SQLite database. Moving that source must not trigger a probe.
            std::fs::rename(
                source.path.as_ref().unwrap(),
                directory.path().join("retired-transcript"),
            )
            .unwrap();
            let mut reopened = runtime
                .block_on(Session::open(descendant_path.to_str().unwrap()))
                .unwrap();
            let restored = CostBudget::from_session(&reopened, None, Some(5.0)).unwrap();
            assert_eq!(restored.spent(), 5.0);
            assert!(matches!(restored.admit(), Err(Error::CostCap { .. })));
            restored.checkpoint(&mut reopened).unwrap();
            assert!(
                latest_checkpoint(&reopened)
                    .unwrap()
                    .get("lateSources")
                    .is_none()
            );
            runtime.block_on(reopened.save()).unwrap();
            drop(restored);

            // Once terminal counters have been incorporated, the checkpoint
            // is portable without retaining any ancestor path dependency.
            std::fs::rename(
                &source_ledger.key,
                source_ledger.key.with_extension("closed"),
            )
            .unwrap();
            if store == crate::session::SessionStoreKind::Jsonl {
                let portable = tempfile::tempdir().unwrap();
                let copy = portable.path().join(descendant_path.file_name().unwrap());
                std::fs::copy(&descendant_path, &copy).unwrap();
                let copied = runtime
                    .block_on(Session::open(copy.to_str().unwrap()))
                    .unwrap();
                assert_eq!(recorded_spend(&copied).unwrap(), 5.0);
            }
        }
    }

    #[test]
    fn prepared_handoffs_do_not_activate_abandoned_sources_or_tolerate_missing_foreign_ledgers() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        budget.record(1.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let prepared = budget.prepare_handoff(&source).unwrap();
        prepared.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        prepared.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let source_origin = Origin::for_session(&source).unwrap().unwrap();
        assert!(!source_origin.key.exists());
        assert!(budget.state.lock().unwrap().retired.is_empty());
        let candidate = prepared.for_replacement(&fork, true).unwrap();
        drop(candidate);
        drop(prepared);
        assert!(!source_origin.key.exists());
        budget.admit().unwrap();
        assert_eq!(
            CostBudget::from_session(&source, None, Some(10.0))
                .unwrap()
                .spent(),
            1.0
        );
        assert!(CostBudget::from_session(&fork, None, Some(10.0)).is_err());
        let prepared = budget.prepare_handoff(&source).unwrap();
        prepared.retire_session(&source, owner.cx()).unwrap();
        budget.record(2.0).unwrap();
        assert_eq!(recorded_spend(&fork).unwrap(), 3.0);
        std::fs::rename(
            &source_origin.key,
            source_origin.key.with_extension("unavailable"),
        )
        .unwrap();
        assert!(CostBudget::from_session(&fork, None, Some(10.0)).is_err());
    }

    #[test]
    fn repeated_forks_keep_one_canonical_sink_instead_of_consuming_the_origin_bound() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let mut current = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut current).unwrap();
        runtime.block_on(current.save()).unwrap();
        let mut budget = budget.prepare_handoff(&current).unwrap();
        budget.checkpoint(&mut current).unwrap();
        runtime.block_on(current.save()).unwrap();
        budget.retire_session(&current, owner.cx()).unwrap();
        for _ in 0..durability::MAX_TARGETS + 4 {
            let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
            budget.checkpoint(&mut fork).unwrap();
            runtime.block_on(fork.save()).unwrap();
            let replacement = budget.for_replacement(&fork, true).unwrap();
            budget.retire_session(&current, owner.cx()).unwrap();
            budget = replacement;
            current = fork;
        }
        assert_eq!(budget.state.lock().unwrap().retired.len(), 1);
        assert_eq!(
            budget.retained_origins().unwrap().len(),
            2,
            "canonical sink plus current native source"
        );
        budget.record(0.5).unwrap();
        assert_eq!(recorded_spend(&current).unwrap(), 0.5);
    }

    #[test]
    fn a_failed_bounded_closure_remains_an_unresolved_writer() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let mut attempt = budget.start_attempt().unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        budget.retire_session(&source, owner.cx()).unwrap();
        let account = budget.state.lock().unwrap().account.clone();
        let origin = Origin::for_session(&source).unwrap().unwrap();
        assert!(attempt.finish_known_before(Instant::now()).is_err());
        drop(attempt);
        drop(budget);
        assert!(!origin.read().unwrap().writers[&account].closed);
    }

    #[test]
    fn modern_frontiers_deduplicate_primary_bills_and_quarantine_uncheckpointed_overlap() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        budget.record(1.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        source.append_message(assistant(1.0));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let budget = budget.prepare_handoff(&source).unwrap();
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        budget.retire_session(&source, owner.cx()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        budget.record(2.0).unwrap();
        fork.append_message(assistant(2.0));
        budget.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        assert_eq!(
            recorded_spend(&fork).unwrap(),
            3.0,
            "do not reapply the first checkpoint's deltas"
        );
        let independent = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();

        budget.record(1.0).unwrap();
        fork.append_message(assistant(1.0));
        runtime.block_on(fork.save()).unwrap();
        let ambiguous = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();
        assert_eq!(ambiguous.spent(), 4.0, "retain the known lower bound");
        assert!(
            matches!(ambiguous.admit(), Err(Error::Session(message)) if message.contains(RECORDED_BILLING_FAILURE))
        );
        assert!(
            matches!(recorded_spend(&fork), Err(Error::Session(message)) if message == RECORDED_BILLING_FAILURE)
        );
        assert!(independent.for_replacement(&fork, true).is_err());
        independent.admit().unwrap();
        budget.admit().unwrap();
        // The still-live owning account has the exact receipt and can complete
        // its interrupted checkpoint without poisoning itself on a refusal.
        budget.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        assert_eq!(recorded_spend(&fork).unwrap(), 4.0);

        let observer = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();
        observer.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        budget.record(0.5).unwrap();
        observer.record(1.0).unwrap();
        fork.append_message(assistant(1.0));
        runtime.block_on(fork.save()).unwrap();
        let foreign_overlap = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();
        assert_eq!(foreign_overlap.spent(), 5.0);
        assert!(
            matches!(foreign_overlap.admit(), Err(Error::Session(message)) if message.contains(RECORDED_BILLING_FAILURE))
        );
        observer.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        assert_eq!(recorded_spend(&fork).unwrap(), 5.5);
    }

    #[test]
    fn a_newer_retirement_baseline_cannot_hide_a_saved_descendants_intervening_bill() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        budget.record(1.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let prepared = budget.prepare_handoff(&source).unwrap();
        prepared.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        prepared.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        // A later preflight acknowledges more usage on the source, while an
        // already saved descendant still covers the original counter of one.
        budget.record(2.0).unwrap();
        prepared.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        prepared.retire_session(&source, owner.cx()).unwrap();
        assert_eq!(recorded_spend(&fork).unwrap(), 3.0);
        budget.record(0.5).unwrap();
        assert_eq!(recorded_spend(&fork).unwrap(), 3.5);
    }

    #[test]
    fn current_forks_watch_new_native_writers_then_retain_their_unresolved_bills() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(2.0), 0.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let prepared = budget.prepare_handoff(&source).unwrap();
        prepared.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        prepared.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let active = prepared.for_replacement(&fork, true).unwrap();
        prepared.retire_session(&source, owner.cx()).unwrap();
        assert!(!Origin::for_session(&fork).unwrap().unwrap().key.exists());

        let foreign = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();
        foreign.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let foreign = foreign.prepare_handoff(&fork).unwrap();
        foreign.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        foreign.retire_session(&fork, owner.cx()).unwrap();
        foreign.record(2.0).unwrap();
        assert!(matches!(active.admit(), Err(Error::CostCap { spent, .. }) if spent == 2.0));

        active.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let mut descendant = Session::create_with_dir(Some(directory.path().to_path_buf()));
        active.checkpoint(&mut descendant).unwrap();
        runtime.block_on(descendant.save()).unwrap();
        let descendant_budget = active.for_replacement(&descendant, true).unwrap();
        active.retire_session(&fork, owner.cx()).unwrap();
        drop(active);
        foreign.record(1.0).unwrap();
        assert_eq!(descendant_budget.refresh().unwrap().spent, 3.0);
        assert_eq!(recorded_spend(&descendant).unwrap(), 3.0);
    }

    #[test]
    fn departing_native_sources_import_unseen_terminal_bills_before_pruning_their_refs() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let prepared = budget.prepare_handoff(&source).unwrap();
        prepared.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        prepared.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let active = prepared.for_replacement(&fork, true).unwrap();
        prepared.retire_session(&source, owner.cx()).unwrap();

        let mut descendant = Session::create_with_dir(Some(directory.path().to_path_buf()));
        active.checkpoint(&mut descendant).unwrap();
        runtime.block_on(descendant.save()).unwrap();
        assert_eq!(recorded_spend(&descendant).unwrap(), 0.0);

        // This writer appears and closes after the candidate's first save.
        // The departing handle has never imported its registration or bill.
        let mut foreign = CostBudget::from_session(&fork, None, Some(10.0)).unwrap();
        foreign.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        foreign = foreign.prepare_handoff(&fork).unwrap();
        foreign.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        foreign.retire_session(&fork, owner.cx()).unwrap();
        foreign.record(1.0).unwrap();
        let foreign_account = foreign.state.lock().unwrap().account.clone();
        drop(foreign);
        let native = Origin::for_session(&fork).unwrap().unwrap();
        assert!(native.read().unwrap().writers[&foreign_account].closed);

        let replacement = active.for_replacement(&descendant, true).unwrap();
        assert_eq!(replacement.spent(), 1.0);
        replacement.checkpoint(&mut descendant).unwrap();
        runtime.block_on(descendant.save()).unwrap();
        let receipt = latest_checkpoint(&descendant).unwrap();
        assert_eq!(receipt["lateCovered"][&foreign_account], 1.0);
        assert!(
            receipt["lateClosed"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(foreign_account))
        );
        assert!(receipt["lateSources"].get(&foreign_account).is_none());

        active.retire_session(&fork, owner.cx()).unwrap();
        drop(replacement);
        drop(active);
        drop(prepared);
        drop(budget);
        std::fs::rename(&native.key, native.key.with_extension("closed")).unwrap();
        let reopened = runtime
            .block_on(Session::open(
                descendant.path.as_ref().unwrap().to_str().unwrap(),
            ))
            .unwrap();
        assert_eq!(recorded_spend(&reopened).unwrap(), 1.0);
    }

    #[test]
    fn a_prospective_native_self_ref_cannot_make_known_foreign_writers_optional() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let original = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        original.record(1.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        original.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        original.retire_session(&source, owner.cx()).unwrap();
        let mut observer = CostBudget::from_session(&source, None, Some(10.0)).unwrap();
        observer.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        observer = observer.prepare_handoff(&source).unwrap();
        observer.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let native = Origin::for_session(&source).unwrap().unwrap();
        let before = std::fs::read(&native.key).unwrap();
        assert_eq!(
            load_session_ledgers(&source, None, None)
                .unwrap()
                .1
                .writers
                .len(),
            1
        );
        let mut missing_writer = serde_json::to_value(native.read().unwrap()).unwrap();
        missing_writer["writers"] = serde_json::json!({});
        std::fs::write(&native.key, serde_json::to_vec(&missing_writer).unwrap()).unwrap();
        assert!(load_session_ledgers(&source, None, None).is_err());
        std::fs::write(&native.key, &before).unwrap();
        let backup = native.key.with_extension("unavailable");
        std::fs::rename(&native.key, &backup).unwrap();
        assert!(load_session_ledgers(&source, None, None).is_err());
        std::fs::rename(&backup, &native.key).unwrap();
        observer.admit().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_ledger_paths_fail_encoding_without_lossy_references() {
        use std::os::unix::ffi::OsStringExt as _;

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let native_directory = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"ledger-\xff".to_vec()));
        std::fs::create_dir(&native_directory).unwrap();
        let mut source = Session::create();
        source.path = Some(native_directory.join("source.jsonl"));
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        budget.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        budget.retire_session(&source, owner.cx()).unwrap();
        let reference = Origin::for_session(&source).unwrap().unwrap().reference();
        assert!(serde_json::to_value(&reference).is_err());
        assert!(reference.validate().is_err());
        assert!(load_session_ledgers(&source, None, None).is_err());
        budget.admit().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keep both live handoffs and the independent cold resume together.
    fn resumed_forks_relay_late_ancestor_charges_without_refunding_the_cap() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let original = CostBudget::new(None, Some(5.0), 0.0).unwrap();
        original.record(2.0).unwrap();
        let mut first = original.start_attempt().unwrap();
        first.observe(1.0).unwrap();
        let mut second = original.start_attempt().unwrap();
        second.observe(0.25).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        original.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        original.retire_session(&source, owner.cx()).unwrap();
        let source_path = source.path.clone().unwrap();
        let mut source = runtime
            .block_on(Session::open(source_path.to_str().unwrap()))
            .unwrap();
        let resumed = CostBudget::from_session(&source, None, Some(5.0)).unwrap();
        assert!(!original.same_tree(&resumed));

        // Match the saved outgoing checkpoint and preserved-budget handoff
        // used by interactive forks. Each replacement outlives its old handle.
        resumed.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let resumed = resumed.prepare_handoff(&source).unwrap();
        resumed.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        let mut fork = Session::create_with_dir(Some(directory.path().to_path_buf()));
        resumed.checkpoint(&mut fork).unwrap();
        runtime.block_on(fork.save()).unwrap();
        let fork_budget = resumed.for_replacement(&fork, true).unwrap();
        resumed.retire_session(&source, owner.cx()).unwrap();
        drop(resumed);

        let mut descendant = Session::create_with_dir(Some(directory.path().to_path_buf()));
        fork_budget.checkpoint(&mut descendant).unwrap();
        runtime.block_on(descendant.save()).unwrap();
        let descendant_budget = fork_budget.for_replacement(&descendant, true).unwrap();
        fork_budget.retire_session(&fork, owner.cx()).unwrap();
        drop(fork_budget);
        let fork_path = fork.path.clone().unwrap();
        let source_bytes = std::fs::read(&source_path).unwrap();
        let fork_bytes = std::fs::read(&fork_path).unwrap();
        let unrelated = descendant_budget
            .for_replacement(&Session::in_memory(), false)
            .unwrap();
        assert!(!descendant_budget.same_tree(&unrelated));

        // Only the original source receives this writer's final bill directly.
        // Every fork can now observe that source directly, even after its
        // previous in-process observers have been destroyed.
        first.finish(3.0).unwrap();
        assert!(matches!(
            descendant_budget.admit(),
            Err(Error::CostCap { spent, cap }) if spent == 5.0 && cap == 5.0
        ));
        assert_eq!(descendant_budget.refresh().unwrap().spent, 5.0);
        assert_eq!(recorded_spend(&fork).unwrap(), 5.0);
        let cold_fork = runtime
            .block_on(Session::open(fork_path.to_str().unwrap()))
            .unwrap();
        let cold = CostBudget::from_session(&cold_fork, None, Some(5.0)).unwrap();
        assert!(matches!(cold.admit(), Err(Error::CostCap { .. })));

        second.finish(0.5).unwrap();
        assert_eq!(descendant_budget.refresh().unwrap().spent, 5.5);
        assert_eq!(descendant_budget.refresh().unwrap().spent, 5.5);
        assert_eq!(cold.refresh().unwrap().spent, 5.5);
        assert_eq!(recorded_spend(&fork).unwrap(), 5.5);
        assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&fork_path).unwrap(), fork_bytes);
        unrelated.admit().unwrap();
        assert_eq!(unrelated.spent(), 0.0);

        let original_id = original.state.lock().unwrap().account.clone();
        let resumed_id = descendant_budget.state.lock().unwrap().account.clone();
        assert_eq!(descendant_budget.state.lock().unwrap().own_finalized, 0.0);
        let journal = Origin::for_session(&source)
            .unwrap()
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(journal.writers[&original_id].own_total, 5.5);
        assert_eq!(journal.writers[&original_id].baseline_own, 2.0);
        assert_eq!(journal.writers[&resumed_id].own_total, 0.0);

        descendant_budget.checkpoint(&mut descendant).unwrap();
        runtime.block_on(descendant.save()).unwrap();
        descendant_budget
            .retire_session(&descendant, owner.cx())
            .unwrap();
        let descendant_path = descendant.path.clone().unwrap();
        let cold_descendant = runtime
            .block_on(Session::open(descendant_path.to_str().unwrap()))
            .unwrap();
        let restored = CostBudget::from_session(&cold_descendant, None, Some(5.0)).unwrap();
        assert_eq!(restored.spent(), 5.5);
        assert!(matches!(restored.admit(), Err(Error::CostCap { .. })));
    }

    #[test]
    fn preserved_unsaved_replacement_keeps_ancestry_without_retaining_abandoned_targets() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        let original = CostBudget::new(None, Some(100.0), 0.0).unwrap();
        let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
        original.checkpoint(&mut source).unwrap();
        runtime.block_on(source.save()).unwrap();
        original.retire_session(&source, owner.cx()).unwrap();
        let resumed = CostBudget::from_session(&source, None, Some(100.0)).unwrap();

        // SDK callers can share a budget with an unsaved replacement without
        // retiring the old handle. Its read ancestry must survive that drop.
        let replacement = resumed
            .for_replacement(&Session::in_memory(), true)
            .unwrap();
        drop(resumed);
        original.record(1.0).unwrap();
        assert_eq!(replacement.refresh().unwrap().spent, 1.0);

        let foreign = CostBudget::new(None, Some(100.0), 0.0).unwrap();
        let mut candidate = Session::create_with_dir(Some(directory.path().to_path_buf()));
        foreign.checkpoint(&mut candidate).unwrap();
        runtime.block_on(candidate.save()).unwrap();
        foreign.retire_session(&candidate, owner.cx()).unwrap();
        let abandoned = replacement.for_replacement(&candidate, true).unwrap();
        drop(abandoned);
        foreign.record(2.0).unwrap();
        original.record(0.5).unwrap();
        assert_eq!(replacement.refresh().unwrap().spent, 1.5);
        assert_eq!(recorded_spend(&candidate).unwrap(), 2.0);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Both source orderings must use the same original writer identity.
    fn foreign_relay_keeps_its_first_floor_across_newer_fork_baselines() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for higher_baseline_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner = crate::agent_cx::AgentCx::for_testing_with_io();
            let original = CostBudget::new(None, Some(100.0), 0.0).unwrap();
            original.record(50.0).unwrap();
            let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
            original.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            original.retire_session(&source, owner.cx()).unwrap();
            original.record(5.0).unwrap();

            let observer = CostBudget::new(None, Some(100.0), 0.0).unwrap();
            observer.record(10.0).unwrap();
            let mut destination = Session::create_with_dir(Some(directory.path().to_path_buf()));
            observer.checkpoint(&mut destination).unwrap();
            runtime.block_on(destination.save()).unwrap();
            observer.retire_session(&destination, owner.cx()).unwrap();
            let observer = observer.for_replacement(&source, true).unwrap();
            assert_eq!(observer.spent(), 15.0);
            assert_eq!(recorded_spend(&destination).unwrap(), 15.0);
            let (observer_id, stale_imports) = {
                let state = observer.state.lock().unwrap();
                (state.account.clone(), state.imported.clone())
            };

            original.record(5.0).unwrap();
            let mut newer = Session::create_with_dir(Some(directory.path().to_path_buf()));
            original.checkpoint(&mut newer).unwrap();
            runtime.block_on(newer.save()).unwrap();
            original.retire_session(&newer, owner.cx()).unwrap();
            original.record(5.0).unwrap();
            if !higher_baseline_first {
                assert_eq!(observer.refresh().unwrap().spent, 25.0);
            }
            // The newer checkpoint covers 60 and the writer has reached 65.
            // This tree already observed 55, so all ten new dollars remain.
            let observer = observer.for_replacement(&newer, true).unwrap();
            assert_eq!(observer.spent(), 25.0);
            assert_eq!(observer.refresh().unwrap().spent, 25.0);
            assert_eq!(observer.refresh().unwrap().spent, 25.0);
            assert_eq!(observer.state.lock().unwrap().own_finalized, 10.0);

            let target = Origin::for_session(&destination).unwrap().unwrap();
            let stale = Retirement::prepare(Arc::clone(&target), 10.0, Coverage::new(), owner.cx())
                .unwrap();
            stale
                .persist(
                    &observer_id,
                    10.0,
                    false,
                    &stale_imports,
                    durability::settlement_deadline(None),
                )
                .unwrap();
            let original_id = original.state.lock().unwrap().account.clone();
            let journal = target.read().unwrap();
            assert_eq!(journal.writers[&original_id].baseline_own, 50.0);
            assert_eq!(journal.writers[&original_id].own_total, 65.0);
            assert_eq!(journal.writers[&observer_id].own_total, 10.0);
            assert!(
                stale
                    .persist(
                        &observer_id,
                        9.0,
                        false,
                        &stale_imports,
                        durability::settlement_deadline(None),
                    )
                    .is_err(),
                "the owning writer still cannot move its counter backwards"
            );
            let destination_path = destination.path.as_ref().unwrap();
            let reopened = runtime
                .block_on(Session::open(destination_path.to_str().unwrap()))
                .unwrap();
            let cold = CostBudget::from_session(&reopened, None, Some(25.0)).unwrap();
            assert_eq!(
                cold.spent(),
                25.0,
                "do not import the original fifty dollars"
            );
            assert!(matches!(cold.admit(), Err(Error::CostCap { .. })));
            assert_eq!(original.spent(), 65.0);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Exercise both bounded resources through the public replacement seam.
    fn rejected_cost_replacements_leave_the_source_and_journals_unchanged() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for exhaust_origins in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner = crate::agent_cx::AgentCx::for_testing_with_io();
            let mut budget = CostBudget::new(None, Some(100.0), 0.0).unwrap();
            budget.record(1.0).unwrap();
            let mut source = Session::create_with_dir(Some(directory.path().to_path_buf()));
            budget.checkpoint(&mut source).unwrap();
            runtime.block_on(source.save()).unwrap();
            budget.retire_session(&source, owner.cx()).unwrap();

            let other = CostBudget::new(None, Some(100.0), 0.0).unwrap();
            let mut candidate = Session::create_with_dir(Some(directory.path().to_path_buf()));
            other.checkpoint(&mut candidate).unwrap();
            runtime.block_on(candidate.save()).unwrap();
            other.retire_session(&candidate, owner.cx()).unwrap();
            other.record(2.0).unwrap();
            let candidate_origin = Origin::for_session(&candidate).unwrap().unwrap();
            let mut foreign_owners = Vec::new();
            if exhaust_origins {
                // One committed retirement plus MAX_TARGETS-1 read ancestors
                // fills the combined bound, although neither map alone is full.
                for _ in 1..durability::MAX_TARGETS {
                    let foreign = CostBudget::new(None, Some(100.0), 0.0).unwrap();
                    let mut ancestor =
                        Session::create_with_dir(Some(directory.path().to_path_buf()));
                    foreign.checkpoint(&mut ancestor).unwrap();
                    runtime.block_on(ancestor.save()).unwrap();
                    foreign.retire_session(&ancestor, owner.cx()).unwrap();
                    budget = budget.for_replacement(&ancestor, true).unwrap();
                    foreign_owners.push(foreign);
                }
            } else {
                // A valid journal can contain MAX_ACCOUNTS writers, but an
                // observer also needs a slot for its own new account UUID.
                let mut journal = serde_json::to_value(candidate_origin.read().unwrap()).unwrap();
                let writers = journal["writers"].as_object_mut().unwrap();
                while writers.len() < durability::MAX_ACCOUNTS {
                    writers.insert(
                        uuid::Uuid::new_v4().to_string(),
                        serde_json::json!({"baselineOwn": 0.0, "ownTotal": 0.0}),
                    );
                }
                std::fs::write(&candidate_origin.key, serde_json::to_vec(&journal).unwrap())
                    .unwrap();
                assert!(CostBudget::from_session(&candidate, None, Some(100.0)).is_err());
            }
            let source_origin = Origin::for_session(&source).unwrap().unwrap();
            let source_journal = std::fs::read(&source_origin.key).unwrap();
            let candidate_journal = std::fs::read(&candidate_origin.key).unwrap();
            let previous_revision = budget.snapshot().revision;

            assert!(budget.for_replacement(&candidate, true).is_err());
            assert_eq!(budget.spent(), 1.0);
            assert_eq!(budget.snapshot().revision, previous_revision);
            assert_eq!(
                budget.state.lock().unwrap().imported.len(),
                foreign_owners.len()
            );
            budget.admit().unwrap();
            assert_eq!(std::fs::read(&source_origin.key).unwrap(), source_journal);
            assert_eq!(
                std::fs::read(&candidate_origin.key).unwrap(),
                candidate_journal
            );
            if exhaust_origins {
                let independent = budget.for_replacement(&candidate, false).unwrap();
                assert!(!budget.same_tree(&independent));
                assert_eq!(independent.spent(), 2.0);
                independent.admit().unwrap();
            }
        }
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
    fn unavailable_billing_survives_jsonl_sqlite_and_retained_branch_replacement() {
        fn assert_unavailable<T: std::fmt::Debug>(result: Result<T>) {
            assert!(matches!(
                result.unwrap_err(),
                Error::Session(message) if message == RECORDED_BILLING_FAILURE
            ));
        }

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for checkpoint_only in [false, true] {
            for with_v2_sidecar in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("unavailable-billing.jsonl");
                let mut session = Session::create();
                session.path = Some(path.clone());
                let completed = session.append_message(assistant(1.0));
                let budget = CostBudget::new(None, Some(10.0), 1.0).unwrap();
                if checkpoint_only {
                    let mut attempt = budget.start_attempt().unwrap();
                    attempt.observe(0.25).unwrap();
                    assert!(attempt.finish(f64::NAN).is_err());
                    budget.quarantine("private transport diagnostic");
                } else {
                    let mut failed = assistant(0.25);
                    if let SessionMessage::Assistant { message } = &mut failed {
                        message.usage.billing = Some(crate::model::BillingUsage::Unavailable {
                            reason: "missing audio usage".to_string(),
                        });
                    }
                    session.append_message(failed);
                    budget.record(0.25).unwrap();
                }

                // A quarantined account must still save its known charge and
                // a stable failure marker, even without a public usage event.
                let mut checkpointed = session.clone();
                budget.checkpoint(&mut checkpointed).unwrap();
                let SessionEntry::Custom(checkpoint) = checkpointed.entries.last().unwrap() else {
                    panic!("cost checkpoint");
                };
                let data = checkpoint.data.as_ref().unwrap();
                assert_eq!(data["spent"], 1.25);
                assert_eq!(data["failure"], RECORDED_BILLING_FAILURE);
                let encoded = serde_json::to_string(data).unwrap();
                assert!(!encoded.contains("private transport diagnostic"));
                assert!(!encoded.contains("NaN"));
                assert!(budget.admit().is_err());
                if checkpoint_only {
                    session = checkpointed;
                }
                // Keep the other case free of any failed checkpoint, proving
                // Usage::Unavailable itself blocks restoration as well.
                assert_unavailable(recorded_spend(&session));
                assert!(session.create_branch_from(&completed));
                let active = session.append_message(assistant(0.0));
                let main_spent = main_spend(&session);
                session.append_custom_entry(
                    CHECKPOINT_TYPE.to_string(),
                    Some(serde_json::json!({"spent": 1.25, "mainSpent": main_spent})),
                );
                assert!(session.navigate_to(&active));
                runtime.block_on(session.save()).unwrap();

                let reopened = if with_v2_sidecar {
                    let store = crate::session::create_v2_sidecar_from_jsonl(&path).unwrap();
                    let (mut reopened, diagnostics) = Session::open_from_v2(
                        &store,
                        session.header.clone(),
                        crate::session::V2OpenMode::ActivePath,
                    )
                    .unwrap();
                    assert!(diagnostics.skipped_entries.is_empty());
                    assert!(reopened.has_partial_v2_hydration());
                    reopened.path = Some(path.clone());
                    reopened
                } else {
                    runtime
                        .block_on(Session::open(path.to_str().unwrap()))
                        .unwrap()
                };
                assert_unavailable(recorded_spend(&reopened));
                assert_unavailable(CostBudget::from_session(&reopened, None, Some(10.0)));
                let current = CostBudget::new(None, Some(10.0), 2.0).unwrap();
                for preserve in [false, true] {
                    assert_unavailable(current.for_replacement(&reopened, preserve));
                }
                current.admit().unwrap();
                assert_eq!(
                    current.spent(),
                    2.0,
                    "a refused switch leaves its source intact"
                );
                let preserved = budget.for_replacement(&Session::in_memory(), true).unwrap();
                assert!(
                    preserved.admit().is_err(),
                    "preserving an account cannot clear failure"
                );
            }
        }
    }

    #[test]
    fn explicit_zero_bills_remain_valid_after_durable_restore() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for billing in [
            crate::model::BillingUsage::ReportedTotal { total: 0.0 },
            crate::model::BillingUsage::Tokens {
                text: crate::model::BillingTokenCounts {
                    input: 20,
                    ..Default::default()
                },
                audio: crate::model::BillingTokenCounts {
                    input: 5,
                    ..Default::default()
                },
                reported_total: Some(0.0),
            },
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("zero-bill.jsonl");
            let mut session = Session::create();
            session.path = Some(path.clone());
            session.append_message(assistant(0.25));
            let mut free = assistant(0.0);
            if let SessionMessage::Assistant { message } = &mut free {
                message.usage.billing = Some(billing);
            }
            session.append_message(free);
            let budget = CostBudget::new(None, Some(1.0), 0.25).unwrap();
            budget.checkpoint(&mut session).unwrap();
            let SessionEntry::Custom(checkpoint) = session.entries.last().unwrap() else {
                panic!("cost checkpoint");
            };
            assert!(checkpoint.data.as_ref().unwrap().get("failure").is_none());
            runtime.block_on(session.save()).unwrap();
            let reopened = runtime
                .block_on(Session::open(path.to_str().unwrap()))
                .unwrap();
            assert_eq!(recorded_spend(&reopened).unwrap(), 0.25);
            let resumed = CostBudget::from_session(&reopened, None, Some(1.0)).unwrap();
            assert_eq!(resumed.spent(), 0.25);
            resumed.admit().unwrap();
            resumed
                .for_replacement(&reopened, true)
                .unwrap()
                .admit()
                .unwrap();
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
