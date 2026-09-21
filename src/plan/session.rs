//! Plan lifecycle operations on the real SDK session, not just its gate.
//!
//! Mutations require exclusive access to the SDK handle. The session store is
//! acquired without waiting, then the plan lock protects the decision and
//! prompt installation together. Neither lock is held by a provider turn.
//! A transition is committed in memory and journaled before saving. Failure or
//! cancellation of that save is not a rollback and must not be reported as one.

use super::{PlanMode, PlanReview, PlanState, PlanStateInner};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::sdk::AgentSessionHandle;
use crate::session::Session;
use std::fmt;
use std::sync::{Arc, Weak};

type Store = asupersync::sync::Mutex<Session>;

/// The exact proposal and session incarnation presented to a human or host.
/// Keep this value while awaiting the decision; do not fetch a fresh review
/// when processing an old approval. Not serializable and not an execution lease.
#[derive(Clone)]
pub struct SessionPlanReview {
    proposal: PlanReview,
    store: Weak<Store>,
    session_id: String,
}

impl SessionPlanReview {
    #[must_use]
    pub fn text(&self) -> &str {
        self.proposal.text()
    }

    /// True only for the same submitted proposal in the same session storage.
    #[must_use]
    pub fn same_submission(&self, other: &Self) -> bool {
        self.store.ptr_eq(&other.store)
            && self.session_id == other.session_id
            && self.proposal.same_submission(&other.proposal)
    }

    fn belongs_to(&self, store: &Arc<Store>, session: &Session) -> bool {
        same_store(&self.store, store) && self.session_id == session.header.id
    }
}

impl fmt::Debug for SessionPlanReview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionPlanReview")
            .field("bytes", &self.text().len())
            .finish_non_exhaustive()
    }
}

/// Whether this operation's journal entry was saved. None of these states
/// claims that an approved plan's full text is reconstructed on session resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPersistence {
    /// No transition or journal entry was needed; no save was attempted.
    Unchanged,
    /// Saving is disabled for the session. The transition is memory-only.
    MemoryOnly,
    /// Session::save completed successfully after the transition.
    Saved,
    /// The transition and journal remain in memory, but saving was not confirmed.
    /// Retry flushing the session, not the already-committed approval decision.
    Unconfirmed { reason: String },
}

/// A completed live transition, including the independent persistence outcome.
/// An Err from a control method instead means that no transition was applied.
#[must_use = "inspect persistence; a live transition is not necessarily saved"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanChange {
    pub mode: PlanMode,
    pub changed: bool,
    pub persistence: PlanPersistence,
}

impl PlanChange {
    fn unchanged(mode: PlanMode) -> Self {
        Self {
            mode,
            changed: false,
            persistence: PlanPersistence::Unchanged,
        }
    }
}

/// Prompt ownership is shared with the plan state rather than a transient UI
/// controller. A second host cannot install another pin over the first one.
/// The storage incarnation prevents a replaced session from editing its old
/// owner's prompt even when a host deliberately reuses the same session ID.
pub(super) struct PlanPin {
    store: Weak<Store>,
    session_id: String,
    before: Option<String>,
    block: String,
}

impl fmt::Debug for PlanPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanPin")
            .field("bytes", &self.block.len())
            .finish_non_exhaustive()
    }
}

impl PlanPin {
    fn new(store: &Arc<Store>, id: &str, before: Option<String>, plan: &str) -> Self {
        let marker = uuid::Uuid::new_v4();
        Self {
            store: Arc::downgrade(store),
            session_id: id.to_string(),
            before,
            block: format!(
                "\n\n<!-- pi-approved-plan:{marker} -->\n\
                 ## Approved Plan (execute this)\n\n{plan}\n\
                 <!-- /pi-approved-plan:{marker} -->"
            ),
        }
    }

    fn applied(&self) -> String {
        format!("{}{}", self.before.as_deref().unwrap_or_default(), self.block)
    }

    fn remove(&self, current: Option<&str>) -> Result<Option<String>> {
        if current == Some(self.applied().as_str()) {
            return Ok(self.before.clone());
        }
        let current = current.ok_or_else(prompt_changed)?;
        let mut matches = current.match_indices(&self.block);
        let (start, block) = matches.next().ok_or_else(prompt_changed)?;
        if matches.next().is_some() {
            return Err(prompt_changed());
        }
        // Remove only this uniquely identified block. Preserve unrelated
        // additions before and after it, including changes to the base prompt.
        Ok(Some(format!(
            "{}{}",
            &current[..start],
            &current[start + block.len()..]
        )))
    }
}

fn same_store(previous: &Weak<Store>, current: &Arc<Store>) -> bool {
    previous
        .upgrade()
        .is_some_and(|previous| Arc::ptr_eq(&previous, current))
}

fn control_error(code: &str, message: &str) -> Error {
    Error::validation(format!("[{code}] {message}"))
}

fn prompt_changed() -> Error {
    control_error(
        "PLAN_CONTEXT_CHANGED",
        "the owned plan context was removed, rewritten or duplicated; nothing was changed",
    )
}

fn check_owner(owner: &AgentCx, save_enabled: bool) -> Result<()> {
    owner.checkpoint().map_err(|_| {
        control_error("PLAN_CANCELLED", "operation cancelled before its live transition")
    })?;
    if save_enabled && (!owner.capabilities().io || !owner.capabilities().time) {
        return Err(control_error(
            "PLAN_CAPABILITY",
            "a persistent plan transition requires I/O and timer capabilities",
        ));
    }
    Ok(())
}

fn pin_owner(inner: &PlanStateInner, store: &Arc<Store>, session: &Session) -> Result<()> {
    if let Some(pin) = &inner.session_pin
        && (!same_store(&pin.store, store) || pin.session_id != session.header.id)
    {
        return Err(control_error(
            "PLAN_SESSION_CHANGED",
            "the plan context belongs to a different session incarnation",
        ));
    }
    Ok(())
}

fn reviewed(inner: &PlanStateInner, review: &SessionPlanReview) -> Result<()> {
    if inner.mode != PlanMode::PendingApproval
        || !inner
            .plan
            .as_ref()
            .is_some_and(|plan| Arc::ptr_eq(plan, &review.proposal.plan))
    {
        return Err(control_error(
            "PLAN_REVIEW_STALE",
            "this submission is no longer pending; present the current proposal again",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Enter,
    Approve(&'a SessionPlanReview),
    Reject(&'a SessionPlanReview),
    Exit,
}

impl AgentSessionHandle {
    /// Capture the pending proposal and exact session incarnation together.
    /// This performs no I/O, does not mark a plan reviewed, and never waits for
    /// a busy session. Present text() in full before accepting a decision.
    pub fn pending_plan_review(&self) -> Result<Option<SessionPlanReview>> {
        let store = self.session_store();
        let session = store.try_lock().map_err(|_| {
            control_error("PLAN_SESSION_BUSY", "session busy; retry taking the review")
        })?;
        let state = self.session().agent.plan_state();
        let inner = state.inner.try_read().map_err(|_| {
            control_error("PLAN_STATE_UNAVAILABLE", "plan state busy or unavailable")
        })?;
        pin_owner(&inner, &store, &session)?;
        if inner.mode != PlanMode::PendingApproval {
            return Ok(None);
        }
        let plan = inner.plan.as_ref().ok_or_else(|| {
            control_error("PLAN_TEXT_UNAVAILABLE", "pending plan has no reviewable text")
        })?;
        Ok(Some(SessionPlanReview {
            proposal: PlanReview { plan: Arc::clone(plan) },
            store: Arc::downgrade(&store),
            session_id: session.header.id.clone(),
        }))
    }

    /// Enter read-only planning with the current model. A pending proposal
    /// must be explicitly rejected or exited; it is never silently discarded.
    /// Re-entering from Approved removes this API's pin before restricting tools.
    /// Installs the native manual-review submit_plan tool bound to this agent,
    /// replacing any tool with that reserved name. This also repairs an already
    /// Planning session's binding without adding a redundant mode journal entry.
    pub async fn enter_plan_mode(&mut self, owner: &AgentCx) -> Result<PlanChange> {
        change(self, owner, Change::Enter).await
    }

    /// Approve exactly the proposal that was presented, pin it into the live
    /// agent prompt and journal the transition. No provider turn starts and no
    /// tool-approval mode changes. The exclusive handle and plan lock cover the
    /// check and prompt installation, not subsequent model/tool execution.
    ///
    /// Saving happens AFTER the live transition. An unconfirmed save is returned
    /// in PlanChange, not as Err. Dropping the future during saving also does not
    /// roll back the decision or its in-memory journal; inspect/flush the session.
    pub async fn approve_plan_review(
        &mut self,
        owner: &AgentCx,
        review: &SessionPlanReview,
    ) -> Result<PlanChange> {
        change(self, owner, Change::Approve(review)).await
    }

    /// Reject only the displayed submission, keeping mutations blocked while
    /// the model revises it. Stale or foreign reviews leave the current plan alone.
    pub async fn reject_plan_review(
        &mut self,
        owner: &AgentCx,
        review: &SessionPlanReview,
    ) -> Result<PlanChange> {
        change(self, owner, Change::Reject(review)).await
    }

    /// Exit planning and remove only this API's uniquely identified prompt pin.
    /// Ambiguous prompt rewrites fail without changing the prompt or gate. The
    /// raw PlanState APIs operate only on the gate; use this method for cleanup
    /// after approving through the SDK, even if a raw caller already set Off.
    pub async fn exit_plan_mode(&mut self, owner: &AgentCx) -> Result<PlanChange> {
        change(self, owner, Change::Exit).await
    }
}

async fn change(
    handle: &mut AgentSessionHandle,
    owner: &AgentCx,
    action: Change<'_>,
) -> Result<PlanChange> {
    let save_enabled = handle.session().save_enabled();
    check_owner(owner, save_enabled)?;
    let store = handle.session_store();
    let mut session = store.try_lock().map_err(|_| {
        control_error("PLAN_SESSION_BUSY", "session busy; no plan transition was applied")
    })?;
    let state = handle.session().agent.plan_state();
    let (mode, journal_mode) = apply_change(handle, &state, &store, &session, owner, action)?;
    let Some(journal_mode) = journal_mode else {
        return Ok(PlanChange::unchanged(mode));
    };
    handle.session_mut().invalidate_background_compaction();
    session.append_custom_entry(
        "plan_mode".to_string(),
        Some(serde_json::json!({"mode": journal_mode})),
    );
    let persistence = persist(&mut session, owner, save_enabled).await;
    Ok(PlanChange { mode, changed: true, persistence })
}

// Deliberately synchronous: never retain a std::sync guard across an await.
fn apply_change(
    handle: &mut AgentSessionHandle,
    state: &PlanState,
    store: &Arc<Store>,
    session: &Session,
    owner: &AgentCx,
    action: Change<'_>,
) -> Result<(PlanMode, Option<&'static str>)> {
    let mut inner = state.inner.try_write().map_err(|_| {
        control_error("PLAN_STATE_UNAVAILABLE", "plan state busy or unavailable")
    })?;
    pin_owner(&inner, store, session)?;
    if let Change::Approve(review) | Change::Reject(review) = action {
        if !review.belongs_to(store, session) {
            return Err(control_error(
                "PLAN_SESSION_CHANGED",
                "the displayed proposal belongs to another session incarnation",
            ));
        }
        reviewed(&inner, review)?;
    }
    // Recheck after acquiring both locks. All fallible context preparation is
    // before the live transition; no error after it masquerades as a rollback.
    check_owner(owner, handle.session().save_enabled())?;
    let agent = &mut handle.session_mut().agent;
    match action {
        Change::Approve(review) => {
            if inner.session_pin.is_some() {
                return Err(control_error(
                    "PLAN_CONTEXT_PRESENT",
                    "an earlier plan context is still owned; exit plan mode first",
                ));
            }
            let pin = PlanPin::new(
                store, &session.header.id,
                agent.system_prompt().map(str::to_string), review.text(),
            );
            let prompt = pin.applied();
            agent.set_system_prompt(Some(prompt));
            inner.session_pin = Some(pin);
            inner.mode = PlanMode::Approved;
            Ok((PlanMode::Approved, Some("approved")))
        }
        Change::Reject(_) => {
            inner.mode = PlanMode::Planning;
            Ok((PlanMode::Planning, Some("rejected")))
        }
        Change::Enter => {
            if inner.mode == PlanMode::PendingApproval {
                return Err(control_error(
                    "PLAN_REVIEW_PENDING", "reject or exit the pending proposal first",
                ));
            }
            let changed = inner.mode != PlanMode::Planning || inner.session_pin.is_some();
            if let Some(pin) = &inner.session_pin {
                let prompt = pin.remove(agent.system_prompt())?;
                agent.set_system_prompt(prompt);
            }
            // Registry construction alone does not install session-coupled
            // tools. Bind the actual live helper and invalidate cached schemas
            // through the agent's normal registry publication path. Explicit
            // manual review must not inherit a foreign/auto-approving helper.
            agent.extend_tools(std::iter::once(
                Box::new(super::SubmitPlanTool::new(state.clone(), false))
                    as Box<dyn crate::tools::Tool>,
            ));
            inner.session_pin = None;
            inner.mode = PlanMode::Planning;
            Ok((PlanMode::Planning, changed.then_some("planning")))
        }
        Change::Exit => {
            if inner.mode == PlanMode::Off && inner.session_pin.is_none() {
                return Ok((PlanMode::Off, None));
            }
            if let Some(pin) = &inner.session_pin {
                let prompt = pin.remove(agent.system_prompt())?;
                agent.set_system_prompt(prompt);
            }
            inner.session_pin = None;
            inner.mode = PlanMode::Off;
            inner.plan = None;
            inner.previous_model = None;
            Ok((PlanMode::Off, Some("off")))
        }
    }
}

async fn persist(session: &mut Session, owner: &AgentCx, enabled: bool) -> PlanPersistence {
    if !enabled {
        return PlanPersistence::MemoryOnly;
    }
    if let Err(error) = check_owner(owner, true) {
        return PlanPersistence::Unconfirmed { reason: error.to_string() };
    }
    match owner.with_current(session.save()).await {
        Ok(()) => PlanPersistence::Saved,
        Err(error) => PlanPersistence::Unconfirmed { reason: error.to_string() },
    }
}

#[cfg(test)]
mod tests;
