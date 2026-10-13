//! Persistent, branch-scoped objectives for SDK hosts.
//!
//! Goal state is journaled through the existing session store. It does not
//! authorize tools, change approval policy, or start provider work by itself.
//! Lifecycle decisions use a snapshot of the exact store, branch leaf and goal
//! revision, so an old UI action cannot complete or drop a replacement goal.
//! Tokens are a continuation budget over recorded assistant usage, not a hard
//! billing ceiling: an in-flight prompt can overshoot and unreported usage is
//! unknowable. Prompt and wall-clock limits bound work independently.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::Usage;
use crate::sdk::AgentSessionHandle;
use crate::session::{Session, SessionEntry, SessionMessage};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Arc, Weak};

#[cfg(test)]
mod tests;

const CHECKPOINT_TYPE: &str = "goal_checkpoint";
const SCHEMA: &str = "pi.goal.checkpoint.v1";
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_ANCESTORS: usize = 100_000;
type Store = asupersync::sync::Mutex<Session>;

/// Both limits are required. A prompt includes its native tool-iteration loop;
/// this counter does not replace the agent's own iteration/time limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalBudget {
    pub max_prompts: u32,
    pub token_budget: u64,
}

impl Default for GoalBudget {
    fn default() -> Self {
        Self {
            max_prompts: 16,
            token_budget: 100_000,
        }
    }
}

impl GoalBudget {
    fn validate(self) -> Result<()> {
        if !(1..=1024).contains(&self.max_prompts) || self.token_budget == 0 {
            return Err(failure(
                "GOAL_BUDGET",
                "max_prompts must be between 1 and 1024 and token_budget must be positive",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    BudgetLimited,
    Complete,
    Dropped,
}

impl GoalStatus {
    const fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Dropped)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    schema: String,
    id: String,
    objective: String,
    status: GoalStatus,
    max_prompts: u32,
    token_budget: u64,
    prompts_started: u32,
    tokens_spent: u64,
    in_flight: bool,
    note: String,
}

impl Record {
    fn new(objective: &str, budget: GoalBudget) -> Result<Self> {
        budget.validate()?;
        validate_text(objective, "objective")?;
        Ok(Self {
            schema: SCHEMA.to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            objective: objective.to_string(),
            status: GoalStatus::Active,
            max_prompts: budget.max_prompts,
            token_budget: budget.token_budget,
            prompts_started: 0,
            tokens_spent: 0,
            in_flight: false,
            note: String::new(),
        })
    }

    const fn exhausted(&self) -> bool {
        self.prompts_started >= self.max_prompts || self.tokens_spent >= self.token_budget
    }

    const fn effective_status(&self) -> GoalStatus {
        if self.status.terminal() {
            self.status
        } else if self.exhausted() {
            GoalStatus::BudgetLimited
        } else if self.in_flight {
            // No automatic replay after dropping a future, crash, or reopening.
            GoalStatus::Paused
        } else {
            self.status
        }
    }
}

/// A read-only view and optimistic-concurrency token. Not a durable authority
/// token; obtain a new snapshot after reopening/replacing a session. Objective
/// and note are intentionally excluded from Debug output.
#[derive(Clone)]
pub struct GoalSnapshot {
    record: Record,
    checkpoint_id: String,
    leaf_id: Option<String>,
    session_id: String,
    store: Weak<Store>,
}

impl GoalSnapshot {
    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn objective(&self) -> &str {
        &self.record.objective
    }

    pub const fn status(&self) -> GoalStatus {
        self.record.effective_status()
    }

    pub const fn budget(&self) -> GoalBudget {
        GoalBudget {
            max_prompts: self.record.max_prompts,
            token_budget: self.record.token_budget,
        }
    }

    pub const fn prompts_started(&self) -> u32 {
        self.record.prompts_started
    }

    pub const fn tokens_spent(&self) -> u64 {
        self.record.tokens_spent
    }

    pub const fn remaining_tokens(&self) -> u64 {
        self.record.token_budget.saturating_sub(self.record.tokens_spent)
    }

    /// True when the saved intent has no settled result. Explicit resume or
    /// drop is required; starting a new driver never replays it automatically.
    pub const fn interrupted(&self) -> bool {
        self.record.in_flight
    }

    pub fn note(&self) -> &str {
        &self.record.note
    }
}

impl fmt::Debug for GoalSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoalSnapshot")
            .field("id", &self.id())
            .field("status", &self.status())
            .field("prompts_started", &self.prompts_started())
            .field("tokens_spent", &self.tokens_spent())
            .field("interrupted", &self.interrupted())
            .finish_non_exhaustive()
    }
}

/// Live transition and durable acknowledgement are separate. An unconfirmed
/// save leaves the journal change in memory; retry flushing, not the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalPersistence {
    MemoryOnly,
    Saved,
    Unconfirmed { reason: String },
}

impl GoalPersistence {
    /// Memory-only sessions may continue without a durable acknowledgement;
    /// persistent sessions must stop on an unconfirmed save.
    pub const fn allows_continuation(&self) -> bool {
        !matches!(self, Self::Unconfirmed { .. })
    }
}

#[must_use = "inspect persistence before starting or continuing goal work"]
#[derive(Debug, Clone)]
pub struct GoalChange {
    pub goal: GoalSnapshot,
    pub persistence: GoalPersistence,
}

fn failure(code: &str, message: &str) -> Error {
    Error::validation(format!("[{code}] {message}"))
}

fn validate_text(text: &str, kind: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES || text.trim().is_empty() {
        return Err(failure(
            "GOAL_TEXT",
            &format!("{kind} must be nonblank and at most 16 KiB"),
        ));
    }
    Ok(())
}

fn invalid_checkpoint() -> Error {
    failure("GOAL_CHECKPOINT", "invalid or unsupported goal checkpoint")
}

// All known values are flat and bounded before cloning the JSON object.
// Reject a malformed latest checkpoint instead of resurrecting an older goal.
fn decode(data: &serde_json::Value) -> Result<Record> {
    let map = data.as_object().ok_or_else(invalid_checkpoint)?;
    if map.len() != 10 {
        return Err(invalid_checkpoint());
    }
    for (key, value) in map {
        let valid = match key.as_str() {
            "schema" | "id" | "status" => value.as_str().is_some_and(|s| s.len() <= 64),
            "objective" | "note" => value.as_str().is_some_and(|s| s.len() <= MAX_TEXT_BYTES),
            "maxPrompts" | "tokenBudget" | "promptsStarted" | "tokensSpent" => value.is_u64(),
            "inFlight" => value.is_boolean(),
            _ => false,
        };
        if !valid {
            return Err(invalid_checkpoint());
        }
    }
    let record: Record = serde_json::from_value(data.clone()).map_err(|_| invalid_checkpoint())?;
    if record.schema != SCHEMA
        || uuid::Uuid::parse_str(&record.id).is_err()
        || record.objective.trim().is_empty()
        || !(1..=1024).contains(&record.max_prompts)
        || record.token_budget == 0
        || record.prompts_started > record.max_prompts
        || (record.in_flight
            && (record.status != GoalStatus::Active || record.prompts_started == 0))
    {
        return Err(invalid_checkpoint());
    }
    Ok(record)
}

fn usage_tokens(usage: &Usage) -> u64 {
    // Providers normalize cache buckets differently. Count the larger of the
    // supplied total and the component sum instead of trusting a missing total.
    usage.total_tokens.max(
        usage.input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write),
    )
}

fn latest(session: &Session) -> Result<Option<(String, Record)>> {
    let mut cursor = session.leaf_id();
    let mut usage = 0u64;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MAX_ANCESTORS {
        let Some(id) = cursor else { return Ok(None) };
        if !seen.insert(id) {
            return Err(failure("GOAL_HISTORY", "goal ancestry contains a cycle"));
        }
        let entry = session
            .get_entry(id)
            .ok_or_else(|| failure("GOAL_HISTORY", "goal ancestry contains a missing entry"))?;
        if let SessionEntry::Custom(custom) = entry
            && custom.custom_type == CHECKPOINT_TYPE
        {
            let mut record = decode(custom.data.as_ref().ok_or_else(invalid_checkpoint)?)?;
            if record.status == GoalStatus::Active {
                record.tokens_spent = record.tokens_spent.saturating_add(usage);
            }
            return Ok(Some((id.to_string(), record)));
        }
        if let SessionEntry::Message(entry) = entry
            && let SessionMessage::Assistant { message } = &entry.message
        {
            usage = usage.saturating_add(usage_tokens(&message.usage));
        }
        cursor = entry.base().parent_id.as_deref();
    }
    Err(failure(
        "GOAL_HISTORY",
        "goal ancestry exceeds 100,000 entries",
    ))
}

fn snapshot(store: &Arc<Store>, session: &Session, id: String, record: Record) -> GoalSnapshot {
    GoalSnapshot {
        record,
        checkpoint_id: id,
        leaf_id: session.leaf_id().map(str::to_string),
        session_id: session.header.id.clone(),
        store: Arc::downgrade(store),
    }
}

fn check_snapshot(
    expected: &GoalSnapshot,
    store: &Arc<Store>,
    session: &Session,
    id: &str,
    record: &Record,
) -> Result<()> {
    if expected.store.as_ptr() != Arc::as_ptr(store)
        || expected.session_id != session.header.id
        || expected.leaf_id.as_deref() != session.leaf_id()
        || expected.checkpoint_id != id
        || expected.record != *record
    {
        return Err(failure(
            "GOAL_STALE",
            "goal or session changed; obtain a fresh snapshot",
        ));
    }
    Ok(())
}

fn check_owner(owner: &AgentCx, persistent: bool) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| failure("GOAL_CANCELLED", "goal operation cancelled"))?;
    if persistent && (!owner.capabilities().io || !owner.capabilities().time) {
        return Err(failure(
            "GOAL_CAPABILITY",
            "goal persistence requires I/O and timers",
        ));
    }
    Ok(())
}

async fn persist(session: &mut Session, owner: &AgentCx, enabled: bool) -> GoalPersistence {
    if !enabled {
        return GoalPersistence::MemoryOnly;
    }
    if let Err(error) = check_owner(owner, true) {
        return GoalPersistence::Unconfirmed {
            reason: error.to_string(),
        };
    }
    match owner.with_current(session.save()).await {
        Ok(()) => GoalPersistence::Saved,
        Err(error) => GoalPersistence::Unconfirmed {
            reason: error.to_string(),
        },
    }
}

fn append(session: &mut Session, record: &Record) -> Result<String> {
    let data = serde_json::to_value(record)?;
    Ok(session.append_custom_entry(CHECKPOINT_TYPE.to_string(), Some(data)))
}

#[derive(Clone, Copy)]
enum Transition<'a> {
    Pause(&'a str),
    Resume,
    Complete(&'a str),
    Drop(&'a str),
}

fn transition(mut record: Record, action: Transition<'_>) -> Result<Record> {
    if record.status.terminal() {
        return Err(failure(
            "GOAL_TERMINAL",
            "the goal is already complete or dropped",
        ));
    }
    match action {
        Transition::Resume => {
            if record.exhausted() {
                return Err(failure(
                    "GOAL_BUDGET",
                    "goal budget exhausted; resume cannot reset it",
                ));
            }
            if record.effective_status() != GoalStatus::Paused {
                return Err(failure(
                    "GOAL_STATE",
                    "only a paused or interrupted goal can resume",
                ));
            }
            record.status = GoalStatus::Active;
            record.note.clear();
        }
        Transition::Pause(reason) | Transition::Drop(reason) | Transition::Complete(reason) => {
            validate_text(reason, "reason or completion evidence")?;
            if matches!(action, Transition::Complete(_)) && record.in_flight {
                return Err(failure(
                    "GOAL_INTERRUPTED",
                    "reconcile interrupted work before completing it",
                ));
            }
            record.status = match action {
                Transition::Pause(_) => GoalStatus::Paused,
                Transition::Drop(_) => GoalStatus::Dropped,
                Transition::Complete(_) => GoalStatus::Complete,
                Transition::Resume => unreachable!(),
            };
            record.note = reason.to_string();
        }
    }
    record.in_flight = false;
    Ok(record)
}

impl AgentSessionHandle {
    /// Inspect only the current branch. No I/O or provider work starts. A saved
    /// unfinished intent reports interrupted/paused, never implicit permission
    /// to replay an input. Imported/forked history retains its goal counters;
    /// a new snapshot binds any subsequent decision to the current store.
    pub fn current_goal(&self) -> Result<Option<GoalSnapshot>> {
        let store = self.session_store();
        let session = store
            .try_lock()
            .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
        Ok(latest(&session)?.map(|(id, record)| snapshot(&store, &session, id, record)))
    }

    /// Create an objective without executing it. An active, paused or exhausted
    /// goal must first be explicitly dropped; terminal goals may be replaced.
    pub async fn create_goal(
        &mut self,
        owner: &AgentCx,
        objective: &str,
        budget: GoalBudget,
    ) -> Result<GoalChange> {
        let record = Record::new(objective, budget)?;
        let enabled = self.session().save_enabled();
        check_owner(owner, enabled)?;
        let store = self.session_store();
        let mut session = store
            .try_lock_owned()
            .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
        if latest(&session)?.is_some_and(|(_, record)| !record.status.terminal()) {
            return Err(failure(
                "GOAL_EXISTS",
                "drop the existing goal before replacing it",
            ));
        }
        check_owner(owner, enabled)?;
        let id = append(&mut session, &record)?;
        self.session_mut().invalidate_background_compaction();
        let goal = snapshot(&store, &session, id, record);
        let persistence = persist(&mut session, owner, enabled).await;
        Ok(GoalChange { goal, persistence })
    }

    pub async fn pause_goal(
        &mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        reason: &str,
    ) -> Result<GoalChange> {
        change(self, owner, expected, Transition::Pause(reason)).await
    }

    /// Resume explicitly, keeping all already consumed prompt/token budgets.
    pub async fn resume_goal(
        &mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
    ) -> Result<GoalChange> {
        change(self, owner, expected, Transition::Resume).await
    }

    /// Host-attested completion. Evidence is an authored explanation, not an
    /// automatically verified proof and never inferred from assistant prose.
    pub async fn complete_goal(
        &mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        evidence: &str,
    ) -> Result<GoalChange> {
        change(self, owner, expected, Transition::Complete(evidence)).await
    }

    pub async fn drop_goal(
        &mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        reason: &str,
    ) -> Result<GoalChange> {
        change(self, owner, expected, Transition::Drop(reason)).await
    }

    /// Retry session persistence without replaying a lifecycle decision.
    pub async fn flush_goal(&mut self, owner: &AgentCx) -> Result<GoalPersistence> {
        let enabled = self.session().save_enabled();
        check_owner(owner, enabled)?;
        let store = self.session_store();
        let mut session = store
            .try_lock_owned()
            .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
        latest(&session)?.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
        Ok(persist(&mut session, owner, enabled).await)
    }
}

async fn change(
    handle: &mut AgentSessionHandle,
    owner: &AgentCx,
    expected: &GoalSnapshot,
    action: Transition<'_>,
) -> Result<GoalChange> {
    let enabled = handle.session().save_enabled();
    check_owner(owner, enabled)?;
    let store = handle.session_store();
    let mut session = store
        .try_lock_owned()
        .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
    let (id, record) =
        latest(&session)?.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
    check_snapshot(expected, &store, &session, &id, &record)?;
    let record = transition(record, action)?;
    check_owner(owner, enabled)?;
    let id = append(&mut session, &record)?;
    handle.session_mut().invalidate_background_compaction();
    let goal = snapshot(&store, &session, id, record);
    let persistence = persist(&mut session, owner, enabled).await;
    Ok(GoalChange { goal, persistence })
}
