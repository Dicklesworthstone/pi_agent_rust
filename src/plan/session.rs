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
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
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

/// Whether this operation's journal and plan checkpoint were saved. Recovery
/// is explicit and never reconstructs approval authority from persisted text.
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
        format!(
            "{}{}",
            self.before.as_deref().unwrap_or_default(),
            self.block
        )
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
        control_error(
            "PLAN_CANCELLED",
            "operation cancelled before its live transition",
        )
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
            control_error(
                "PLAN_TEXT_UNAVAILABLE",
                "pending plan has no reviewable text",
            )
        })?;
        Ok(Some(SessionPlanReview {
            proposal: PlanReview {
                plan: Arc::clone(plan),
            },
            store: Arc::downgrade(&store),
            session_id: session.header.id.clone(),
        }))
    }

    /// Enter read-only planning with the current model. A pending proposal
    /// must be explicitly rejected or exited; it is never silently discarded.
    /// Re-entering from Approved removes this API's pin before restricting tools.
    /// Installs a checkpointed native manual-review submit_plan tool bound to this agent,
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
        control_error(
            "PLAN_SESSION_BUSY",
            "session busy; no plan transition was applied",
        )
    })?;
    let state = handle.session().agent.plan_state();
    let (mode, journal_mode) = apply_change(handle, &state, &store, &mut session, owner, action)?;
    if journal_mode.is_none() {
        return Ok(PlanChange::unchanged(mode));
    }
    handle.session_mut().invalidate_background_compaction();
    // The mode and checkpoint were appended together under the decision lock.
    let persistence = persist(&mut session, owner, save_enabled).await;
    Ok(PlanChange {
        mode,
        changed: true,
        persistence,
    })
}

// Deliberately synchronous: never retain a std::sync guard across an await.
fn apply_change(
    handle: &mut AgentSessionHandle,
    state: &PlanState,
    store: &Arc<Store>,
    session: &mut Session,
    owner: &AgentCx,
    action: Change<'_>,
) -> Result<(PlanMode, Option<&'static str>)> {
    let mut inner = state
        .inner
        .try_write()
        .map_err(|_| control_error("PLAN_STATE_UNAVAILABLE", "plan state busy or unavailable"))?;
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
    let save_enabled = handle.session().save_enabled();
    let agent = &mut handle.session_mut().agent;
    let result = match action {
        Change::Approve(review) => {
            if inner.session_pin.is_some() {
                return Err(control_error(
                    "PLAN_CONTEXT_PRESENT",
                    "an earlier plan context is still owned; exit plan mode first",
                ));
            }
            let pin = PlanPin::new(
                store,
                &session.header.id,
                agent.system_prompt().map(str::to_string),
                review.text(),
            );
            let prompt = pin.applied();
            agent.set_system_prompt(Some(prompt));
            inner.session_pin = Some(pin);
            inner.mode = PlanMode::Approved;
            (PlanMode::Approved, Some("approved"))
        }
        Change::Reject(_) => {
            inner.mode = PlanMode::Planning;
            (PlanMode::Planning, Some("rejected"))
        }
        Change::Enter => {
            if inner.mode == PlanMode::PendingApproval {
                return Err(control_error(
                    "PLAN_REVIEW_PENDING",
                    "reject or exit the pending proposal first",
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
            install_checkpointed_submit(agent, state, store, session, save_enabled);
            inner.session_pin = None;
            inner.mode = PlanMode::Planning;
            (PlanMode::Planning, changed.then_some("planning"))
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
            (PlanMode::Off, Some("off"))
        }
    };
    if let Some(journal_mode) = result.1 {
        append_plan_transition(session, &inner, journal_mode);
    }
    Ok(result)
}

async fn persist(session: &mut Session, owner: &AgentCx, enabled: bool) -> PlanPersistence {
    if !enabled {
        return PlanPersistence::MemoryOnly;
    }
    if let Err(error) = check_owner(owner, true) {
        return PlanPersistence::Unconfirmed {
            reason: error.to_string(),
        };
    }
    match owner.with_current(session.save()).await {
        Ok(()) => PlanPersistence::Saved,
        Err(error) => PlanPersistence::Unconfirmed {
            reason: error.to_string(),
        },
    }
}

const PLAN_CHECKPOINT_TYPE: &str = "plan_checkpoint";
const PLAN_CHECKPOINT_SCHEMA: &str = "pi.plan.checkpoint.v1";
const MAX_CHECKPOINT_ANCESTORS: usize = 100_000;

// Durable content, NOT durable authority. In particular, no PlanReview, prompt
// ownership marker, previous model, or tool-approval override is serialized.
struct PlanCheckpoint {
    mode: PlanMode,
    plan: Option<Arc<str>>,
}

fn checkpoint_error(message: &str) -> Error {
    control_error("PLAN_CHECKPOINT_INVALID", message)
}

fn valid_checkpoint_plan(mode: PlanMode, plan: Option<&str>) -> bool {
    if let Some(text) = plan
        && (text.len() > super::MAX_PLAN_BYTES || text.trim().is_empty())
    {
        return false;
    }
    match mode {
        PlanMode::Off => plan.is_none(),
        PlanMode::Planning => true,
        PlanMode::PendingApproval | PlanMode::Approved => plan.is_some(),
    }
}

/// Only called while the session and plan locks cover the same transition.
fn append_plan_transition(session: &mut Session, inner: &PlanStateInner, label: &str) {
    session.append_custom_entry(
        "plan_mode".to_string(),
        Some(serde_json::json!({"mode": label})),
    );
    append_plan_checkpoint(session, inner);
}

fn append_plan_checkpoint(session: &mut Session, inner: &PlanStateInner) {
    session.append_custom_entry(
        PLAN_CHECKPOINT_TYPE.to_string(),
        Some(serde_json::json!({
            "schema": PLAN_CHECKPOINT_SCHEMA,
            "sessionId": session.header.id,
            "mode": inner.mode.as_str(),
            "plan": inner.plan.as_deref(),
        })),
    );
}

fn decode_plan_checkpoint(data: &serde_json::Value, session_id: &str) -> Result<PlanCheckpoint> {
    let object = data.as_object().ok_or_else(|| checkpoint_error("expected a checkpoint object"))?;
    if object.len() != 4
        || object.keys().any(|key| !matches!(key.as_str(), "schema" | "sessionId" | "mode" | "plan"))
        || object.get("schema").and_then(serde_json::Value::as_str) != Some(PLAN_CHECKPOINT_SCHEMA)
        || object.get("sessionId").and_then(serde_json::Value::as_str) != Some(session_id)
    {
        return Err(checkpoint_error("unsupported checkpoint schema, fields or session identity"));
    }
    let mode = match object.get("mode").and_then(serde_json::Value::as_str) {
        Some("off") => PlanMode::Off,
        Some("planning") => PlanMode::Planning,
        Some("pending_approval") => PlanMode::PendingApproval,
        Some("approved") => PlanMode::Approved,
        _ => return Err(checkpoint_error("unknown checkpoint mode")),
    };
    let plan = match object.get("plan") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => Some(text.as_str()),
        _ => return Err(checkpoint_error("checkpoint plan must be text or null")),
    };
    // Validate borrowed data BEFORE cloning. Malformed or oversized records
    // must not trigger an unbounded copy, or fall back to an older approval.
    if !valid_checkpoint_plan(mode, plan) {
        return Err(checkpoint_error("checkpoint plan is missing, empty, oversized or inconsistent with its mode"));
    }
    Ok(PlanCheckpoint { mode, plan: plan.map(Arc::from) })
}

fn latest_plan_checkpoint(session: &Session) -> Result<Option<PlanCheckpoint>> {
    let mut cursor = session.leaf_id();
    for _ in 0..MAX_CHECKPOINT_ANCESTORS {
        let Some(id) = cursor else { return Ok(None) };
        let entry = session.get_entry(id).ok_or_else(|| {
            checkpoint_error("current branch has a missing ancestor")
        })?;
        if let crate::session::SessionEntry::Custom(custom) = entry {
            if custom.custom_type == PLAN_CHECKPOINT_TYPE {
                let data = custom.data.as_ref().ok_or_else(|| checkpoint_error("checkpoint has no data"))?;
                return decode_plan_checkpoint(data, &session.header.id).map(Some);
            }
            // Older clients journal only mode changes. Such a newer change
            // supersedes a previous checkpoint; never resurrect its proposal.
            if custom.custom_type == "plan_mode" {
                return Ok(None);
            }
        }
        cursor = entry.base().parent_id.as_deref();
    }
    Err(checkpoint_error("current branch exceeds the checkpoint ancestry budget or contains a cycle"))
}

impl AgentSessionHandle {
    /// Save the live plan's bounded text and mode on the CURRENT session branch.
    /// Normal SDK plan transitions and the SDK-bound submit_plan tool already
    /// checkpoint automatically. This also supports raw PlanState clients and
    /// retrying a previously unconfirmed save without replaying an approval.
    /// Identical checkpoints are not appended again, but persistence is retried.
    pub async fn checkpoint_plan(&mut self, owner: &AgentCx) -> Result<PlanPersistence> {
        let save_enabled = self.session().save_enabled();
        check_owner(owner, save_enabled)?;
        let store = self.session_store();
        let mut session = store.try_lock().map_err(|_| {
            control_error("PLAN_SESSION_BUSY", "session busy; no checkpoint was appended")
        })?;
        let previous = latest_plan_checkpoint(&session)?;
        let state = self.session().agent.plan_state();
        {
            let inner = state.inner.try_read().map_err(|_| {
                control_error("PLAN_STATE_UNAVAILABLE", "plan state busy or unavailable")
            })?;
            pin_owner(&inner, &store, &session)?;
            if !valid_checkpoint_plan(inner.mode, inner.plan.as_deref()) {
                return Err(checkpoint_error("live plan state cannot be checkpointed"));
            }
            check_owner(owner, save_enabled)?;
            let same = previous.as_ref().is_some_and(|old| {
                old.mode == inner.mode && old.plan.as_deref() == inner.plan.as_deref()
            });
            if !same {
                append_plan_checkpoint(&mut session, &inner);
            }
        }
        Ok(persist(&mut session, owner, save_enabled).await)
    }

    /// Recover a saved proposal after opening a session. This is opt-in, not
    /// automatic SDK startup behavior. Pending AND previously approved plans
    /// become PendingApproval with a fresh review identity and no prompt pin.
    /// The caller must display the proposal and obtain a new decision. Rejected
    /// drafts remain Planning, and Off remains Off. No provider turn starts.
    ///
    /// Refuses to replace any live proposal or owned prompt. Malformed, foreign,
    /// superseded or off-branch checkpoints are not used. On Err no state was
    /// changed; callers must not continue execution as though recovery succeeded.
    pub async fn restore_plan_checkpoint(&mut self, owner: &AgentCx) -> Result<PlanChange> {
        let save_enabled = self.session().save_enabled();
        check_owner(owner, save_enabled)?;
        let store = self.session_store();
        let mut session = store.try_lock().map_err(|_| {
            control_error("PLAN_SESSION_BUSY", "session busy; no checkpoint was restored")
        })?;
        let checkpoint = latest_plan_checkpoint(&session)?.ok_or_else(|| {
            control_error("PLAN_CHECKPOINT_MISSING", "this branch has no current recoverable plan checkpoint")
        })?;
        let state = self.session().agent.plan_state();
        let mode = restore_checkpoint_in_memory(self, &state, &store, &mut session, owner, checkpoint)?;
        self.session_mut().invalidate_background_compaction();
        let persistence = persist(&mut session, owner, save_enabled).await;
        Ok(PlanChange { mode, changed: true, persistence })
    }
}

// The write guard stays in this synchronous helper, never across saving.
fn restore_checkpoint_in_memory(
    handle: &mut AgentSessionHandle,
    state: &PlanState,
    store: &Arc<Store>,
    session: &mut Session,
    owner: &AgentCx,
    checkpoint: PlanCheckpoint,
) -> Result<PlanMode> {
    let mut inner = state.inner.try_write().map_err(|_| {
        control_error("PLAN_STATE_UNAVAILABLE", "plan state busy or unavailable")
    })?;
    if inner.plan.is_some() || inner.session_pin.is_some() {
        return Err(control_error("PLAN_ALREADY_ACTIVE", "cannot restore over a live plan or its owned context"));
    }
    let save_enabled = handle.session().save_enabled();
    check_owner(owner, save_enabled)?;
    let mode = match checkpoint.mode {
        PlanMode::PendingApproval | PlanMode::Approved => PlanMode::PendingApproval,
        PlanMode::Off | PlanMode::Planning => checkpoint.mode,
    };
    install_checkpointed_submit(&mut handle.session_mut().agent, state, store, session, save_enabled);
    inner.mode = mode;
    // Decoding allocated new text; persisted data cannot reconstruct a live
    // PlanReview capability even when resuming through the very same store.
    inner.plan = checkpoint.plan;
    inner.previous_model = None;
    append_plan_transition(session, &inner, mode.as_str());
    Ok(mode)
}

fn install_checkpointed_submit(
    agent: &mut crate::agent::Agent,
    state: &PlanState,
    store: &Arc<Store>,
    session: &Session,
    save_enabled: bool,
) {
    let tool = CheckpointedSubmitPlan {
        native: super::SubmitPlanTool::new(state.clone(), false),
        state: state.clone(),
        store: Arc::downgrade(store),
        session_id: session.header.id.clone(),
        save_enabled,
    };
    agent.extend_tools(std::iter::once(Box::new(tool) as Box<dyn Tool>));
}

/// The ordinary native submission contract plus session-owned persistence.
/// Weak ownership avoids keeping closed sessions alive through a tool snapshot.
struct CheckpointedSubmitPlan {
    native: super::SubmitPlanTool,
    state: PlanState,
    store: Weak<Store>,
    session_id: String,
    save_enabled: bool,
}

#[async_trait::async_trait]
impl Tool for CheckpointedSubmitPlan {
    fn name(&self) -> &str { self.native.name() }
    fn label(&self) -> &str { self.native.label() }
    fn description(&self) -> &str { self.native.description() }
    fn parameters(&self) -> serde_json::Value { self.native.parameters() }
    fn effects(&self) -> ToolEffects { self.native.effects() }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner, false)?;
        let store = self.store.upgrade().ok_or_else(|| {
            control_error("PLAN_SESSION_CHANGED", "the submission's session is no longer available")
        })?;
        let mut session = store.try_lock().map_err(|_| {
            control_error("PLAN_SESSION_BUSY", "session busy; plan was not submitted")
        })?;
        if session.header.id != self.session_id {
            return Err(control_error("PLAN_SESSION_CHANGED", "the submission belongs to another session"));
        }
        let mut output = self.native.execute(tool_call_id, input, on_update).await?;
        if output.is_error {
            return Ok(output);
        }
        // Capture immediately after the native state transition, before the
        // first save await. Cancellation cannot remove its in-memory journal.
        let captured = record_submitted_checkpoint(&mut session, &store, &self.state);
        let persistence = match captured {
            Ok(()) => persist(&mut session, &owner, self.save_enabled).await,
            Err(error) => PlanPersistence::Unconfirmed { reason: error.to_string() },
        };
        let label = match &persistence {
            PlanPersistence::Saved => "saved",
            PlanPersistence::MemoryOnly => "memory_only",
            PlanPersistence::Unchanged => "unchanged",
            PlanPersistence::Unconfirmed { .. } => "unconfirmed",
        };
        if let Some(details) = output.details.as_mut().and_then(serde_json::Value::as_object_mut) {
            details.insert("checkpointPersistence".to_string(), serde_json::json!(label));
        }
        if matches!(persistence, PlanPersistence::Unconfirmed { .. }) {
            // Keep filesystem paths and other persistence diagnostics out of
            // the model-visible payload. The accepted proposal remains live.
            output.content.push(crate::model::ContentBlock::Text(
                crate::model::TextContent::new(
                    "The live plan was submitted, but saving its checkpoint was not confirmed. Do not resubmit or execute it to retry saving; the host can flush the checkpoint.",
                ),
            ));
        }
        Ok(output)
    }
}

fn record_submitted_checkpoint(session: &mut Session, store: &Arc<Store>, state: &PlanState) -> Result<()> {
    let inner = state.inner.try_read().map_err(|_| {
        control_error("PLAN_STATE_UNAVAILABLE", "plan submitted but checkpoint state is unavailable")
    })?;
    pin_owner(&inner, store, session)?;
    if !valid_checkpoint_plan(inner.mode, inner.plan.as_deref()) {
        return Err(checkpoint_error("submitted plan state cannot be checkpointed"));
    }
    append_plan_transition(session, &inner, inner.mode.as_str());
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::approval::{ApprovalMode, ApprovalState};
    use crate::session::SessionEntry;
    use serde_json::{Value, json};

    const TEXT: &str = "Goal: retain this proposal. Steps: edit the named file. Verification: inspect the result.";

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(future)
    }

    fn handle(session: Session, save: bool) -> AgentSessionHandle {
        let provider = Arc::new(crate::providers::openai::OpenAIProvider::new("checkpoint-fixture")
            .with_base_url("http://127.0.0.1:1/v1"));
        let agent = crate::agent::Agent::new(
            provider,
            crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None),
            crate::agent::AgentConfig {
                system_prompt: Some("fresh session instructions".to_string()),
                approval_state: Some(ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new())),
                ..crate::agent::AgentConfig::default()
            },
        );
        AgentSessionHandle::from_session_with_listeners(
            crate::agent::AgentSession::new(agent, Arc::new(Store::new(session)), save,
                crate::compaction::ResolvedCompactionSettings::default()),
            crate::sdk::EventListeners::default(),
        )
    }

    fn submission_tool(handle: &AgentSessionHandle) -> CheckpointedSubmitPlan {
        let state = handle.session().agent.plan_state();
        let store = handle.session_store();
        let session_id = store.try_lock().unwrap().header.id.clone();
        CheckpointedSubmitPlan {
            native: super::super::SubmitPlanTool::new(state.clone(), false),
            state,
            store: Arc::downgrade(&store),
            session_id,
            save_enabled: handle.session().save_enabled(),
        }
    }

    fn submit(handle: &AgentSessionHandle) -> ToolOutput {
        run(submission_tool(handle).execute("submit", json!({
            "plan": TEXT, "files": ["src/hello world.rs", "文档/a,b.md"]
        }), None)).unwrap()
    }

    fn checkpoint(session: &mut Session, mode: &str, text: Option<&str>) -> String {
        session.append_custom_entry(PLAN_CHECKPOINT_TYPE.to_string(), Some(json!({
            "schema": PLAN_CHECKPOINT_SCHEMA, "sessionId": session.header.id,
            "mode": mode, "plan": text,
        })))
    }

    fn count(handle: &AgentSessionHandle) -> usize {
        handle.session_store().try_lock().unwrap().entries.len()
    }

    fn persistent(path: &std::path::Path) -> AgentSessionHandle {
        let mut session = Session::in_memory();
        session.path = Some(path.to_path_buf());
        handle(session, true)
    }

    #[test]
    fn native_pending_submission_is_saved_before_another_provider_turn() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending.jsonl");
        let owner = AgentCx::for_request();
        let mut first = persistent(&path);
        let _ = run(first.enter_plan_mode(&owner)).unwrap();
        let output = submit(&first);
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["checkpointPersistence"], "saved");
        let text = first.pending_plan_review().unwrap().unwrap().text().to_string();
        assert!(text.contains("Files: [\"src/hello world.rs\",\"文档/a,b.md\"]"));
        drop(first);
        let opened = run(Session::open(path.to_str().unwrap())).unwrap();
        let mut resumed = handle(opened, true);
        let result = run(resumed.restore_plan_checkpoint(&owner)).unwrap();
        assert_eq!(result.mode, PlanMode::PendingApproval);
        assert_eq!(result.persistence, PlanPersistence::Saved);
        assert_eq!(resumed.pending_plan_review().unwrap().unwrap().text(), text);
        assert!(!resumed.session().agent.plan_state().allows_effects(ToolEffects::write()));
        assert_eq!(resumed.session().agent.system_prompt(), Some("fresh session instructions"));
    }

    #[test]
    fn approved_plan_reopens_for_fresh_review_without_inheriting_authority() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("approved.jsonl");
        let owner = AgentCx::for_request();
        let mut first = persistent(&path);
        let _ = run(first.enter_plan_mode(&owner)).unwrap();
        assert!(!submit(&first).is_error);
        let old = first.pending_plan_review().unwrap().unwrap();
        let _ = run(first.approve_plan_review(&owner, &old)).unwrap();
        drop(first);
        let mut resumed = handle(run(Session::open(path.to_str().unwrap())).unwrap(), true);
        let _ = run(resumed.restore_plan_checkpoint(&owner)).unwrap();
        let new = resumed.pending_plan_review().unwrap().unwrap();
        assert_eq!(old.text(), new.text());
        assert!(!old.same_submission(&new));
        assert!(run(resumed.approve_plan_review(&owner, &old)).is_err());
        let state = resumed.session().agent.plan_state();
        let policy = resumed.session().agent.approval_state().unwrap();
        assert_eq!(policy.mode(), ApprovalMode::AlwaysAsk);
        assert!(policy.evaluate("write", &json!({"path":"src/hello world.rs"}),
            ToolEffects::write(), Some(&state), None).requires_approval());
        let _ = run(resumed.approve_plan_review(&owner, &new)).unwrap();
        assert!(resumed.session().agent.system_prompt().unwrap().contains(new.text()));
        let _ = run(resumed.exit_plan_mode(&owner)).unwrap();
        drop(resumed);
        let mut closed = handle(run(Session::open(path.to_str().unwrap())).unwrap(), false);
        assert_eq!(run(closed.restore_plan_checkpoint(&owner)).unwrap().mode, PlanMode::Off);
        assert!(closed.session().agent.plan_state().plan().is_none());
    }

    #[test]
    fn rejected_draft_stays_read_only_and_cannot_be_approved_on_resume() {
        let mut stored = Session::in_memory();
        checkpoint(&mut stored, "planning", Some(TEXT));
        let mut resumed = handle(stored, false);
        let owner = AgentCx::for_request();
        assert_eq!(run(resumed.restore_plan_checkpoint(&owner)).unwrap().mode, PlanMode::Planning);
        assert!(resumed.pending_plan_review().unwrap().is_none());
        assert_eq!(resumed.session().agent.plan_state().plan().as_deref(), Some(TEXT));
        assert!(!resumed.session().agent.plan_state().allows_effects(ToolEffects::process()));
        assert!(!submit(&resumed).is_error);
        assert!(resumed.pending_plan_review().unwrap().is_some());
    }

    #[test]
    fn recovery_uses_the_selected_branch_not_the_latest_physical_entry() {
        let mut stored = Session::in_memory();
        let root = stored.append_custom_entry("root".to_string(), None);
        let branch_a = checkpoint(&mut stored, "approved", Some("proposal on branch A"));
        assert!(stored.navigate_to(&root));
        checkpoint(&mut stored, "pending_approval", Some("proposal on branch B"));
        assert!(stored.navigate_to(&branch_a));
        let mut resumed = handle(stored, false);
        let _ = run(resumed.restore_plan_checkpoint(&AgentCx::for_request())).unwrap();
        assert_eq!(resumed.pending_plan_review().unwrap().unwrap().text(), "proposal on branch A");
    }

    #[test]
    fn newer_legacy_mode_change_prevents_resurrecting_an_old_checkpoint() {
        for mode in ["off", "planning", "approved", "rejected"] {
            let mut stored = Session::in_memory();
            checkpoint(&mut stored, "approved", Some(TEXT));
            stored.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": mode})));
            let mut resumed = handle(stored, false);
            let before = count(&resumed);
            assert!(run(resumed.restore_plan_checkpoint(&AgentCx::for_request())).is_err());
            assert_eq!(count(&resumed), before);
            assert_eq!(resumed.session().agent.plan_state().mode(), PlanMode::Off);
        }
    }

    #[test]
    fn malformed_latest_checkpoint_never_falls_back_to_an_older_approval() {
        for data in [None, Some(Value::Null), Some(json!({"schema":"future"}))] {
            let mut stored = Session::in_memory();
            checkpoint(&mut stored, "approved", Some(TEXT));
            stored.append_custom_entry(PLAN_CHECKPOINT_TYPE.to_string(), data);
            let mut resumed = handle(stored, false);
            assert!(run(resumed.restore_plan_checkpoint(&AgentCx::for_request())).is_err());
            assert!(resumed.session().agent.plan_state().plan().is_none());
            assert_eq!(resumed.session().agent.system_prompt(), Some("fresh session instructions"));
        }
    }

    #[test]
    fn checkpoint_validation_is_bounded_typed_and_session_bound() {
        let valid = json!({"schema":PLAN_CHECKPOINT_SCHEMA, "sessionId":"session",
            "mode":"pending_approval", "plan":TEXT});
        assert!(decode_plan_checkpoint(&valid, "session").is_ok());
        assert!(decode_plan_checkpoint(&valid, "another").is_err());
        for (key, value) in [
            ("mode", json!("yolo")), ("mode", json!("off")),
            ("plan", Value::Null), ("plan", json!(7)), ("plan", json!("  \n")),
            ("plan", json!("é".repeat(super::super::MAX_PLAN_BYTES / 2 + 1))),
            ("schema", json!("pi.plan.checkpoint.v2")), ("extra", json!(true)),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(decode_plan_checkpoint(&invalid, "session").is_err(), "key={key}");
        }
        let mut exact = valid;
        exact["plan"] = json!("é".repeat(super::super::MAX_PLAN_BYTES / 2));
        assert_eq!(decode_plan_checkpoint(&exact, "session").unwrap().plan.unwrap().len(), super::super::MAX_PLAN_BYTES);
    }

    #[test]
    fn cyclic_and_missing_branch_ancestry_fail_with_a_bounded_error() {
        for parent in ["missing", "cycle"] {
            let mut stored = Session::in_memory();
            checkpoint(&mut stored, "approved", Some(TEXT));
            let leaf = stored.append_custom_entry("ordinary".to_string(), None);
            let target = if parent == "cycle" { leaf.clone() } else { parent.to_string() };
            let Some(SessionEntry::Custom(entry)) = stored.get_entry_mut(&leaf) else { panic!("custom") };
            entry.base.parent_id = Some(target);
            assert!(latest_plan_checkpoint(&stored).is_err());
        }
    }

    #[test]
    fn repeat_checkpoint_retries_saving_without_duplicating_history() {
        let owner = AgentCx::for_request();
        let mut live = handle(Session::in_memory(), false);
        let _ = run(live.enter_plan_mode(&owner)).unwrap();
        assert!(!submit(&live).is_error);
        let before = count(&live);
        for _ in 0..3 {
            assert_eq!(run(live.checkpoint_plan(&owner)).unwrap(), PlanPersistence::MemoryOnly);
            assert_eq!(count(&live), before);
        }
    }

    #[test]
    fn save_failure_preserves_the_proposal_and_can_be_flushed_without_resubmission() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("private-persistence-path");
        std::fs::write(&blocker, b"sentinel").unwrap();
        let mut live = persistent(&blocker.join("session.jsonl"));
        let owner = AgentCx::for_request();
        let _ = run(live.enter_plan_mode(&owner)).unwrap();
        let output = submit(&live);
        assert!(!output.is_error);
        assert_eq!(output.details.as_ref().unwrap()["checkpointPersistence"], "unconfirmed");
        assert!(!serde_json::to_string(&output.content).unwrap().contains("private-persistence-path"));
        let review = live.pending_plan_review().unwrap().unwrap();
        let before = count(&live);
        let path = temp.path().join("recovered.jsonl");
        live.session_store().try_lock().unwrap().path = Some(path.clone());
        assert_eq!(run(live.checkpoint_plan(&owner)).unwrap(), PlanPersistence::Saved);
        assert_eq!(count(&live), before);
        assert!(review.same_submission(&live.pending_plan_review().unwrap().unwrap()));
        assert!(path.is_file());
        assert_eq!(std::fs::read(blocker).unwrap(), b"sentinel");
    }

    #[test]
    fn invalid_tool_input_and_busy_or_replaced_session_do_not_submit_or_checkpoint() {
        let mut live = handle(Session::in_memory(), false);
        let _ = run(live.enter_plan_mode(&AgentCx::for_request())).unwrap();
        let tool = submission_tool(&live);
        let before = count(&live);
        assert!(run(tool.execute("bad", json!({"plan":"short"}), None)).unwrap().is_error);
        assert_eq!(count(&live), before);
        let store = live.session_store();
        let held = store.try_lock().unwrap();
        assert!(run(tool.execute("busy", json!({"plan":TEXT}), None)).is_err());
        drop(held);
        store.try_lock().unwrap().header.id = "replacement".to_string();
        assert!(run(tool.execute("foreign", json!({"plan":TEXT}), None)).is_err());
        assert_eq!(count(&live), before);
        assert_eq!(live.session().agent.plan_state().mode(), PlanMode::Planning);
    }

    #[test]
    fn no_session_submission_checkpoints_only_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("must-not-exist.jsonl");
        let mut stored = Session::in_memory();
        stored.path = Some(path.clone());
        let mut live = handle(stored, false);
        let _ = run(live.enter_plan_mode(&AgentCx::for_request())).unwrap();
        assert_eq!(submit(&live).details.unwrap()["checkpointPersistence"], "memory_only");
        assert!(latest_plan_checkpoint(&live.session_store().try_lock().unwrap()).unwrap().is_some());
        assert!(!path.exists());
    }

    #[test]
    fn restore_cannot_overwrite_a_live_proposal_or_owned_prompt() {
        let mut live = handle(Session::in_memory(), false);
        let owner = AgentCx::for_request();
        let _ = run(live.enter_plan_mode(&owner)).unwrap();
        assert!(!submit(&live).is_error);
        let review = live.pending_plan_review().unwrap().unwrap();
        let before = count(&live);
        assert!(run(live.restore_plan_checkpoint(&owner)).is_err());
        assert_eq!(count(&live), before);
        let _ = run(live.approve_plan_review(&owner, &review)).unwrap();
        let prompt = live.session().agent.system_prompt().unwrap().to_string();
        assert!(run(live.restore_plan_checkpoint(&owner)).is_err());
        assert_eq!(live.session().agent.system_prompt(), Some(prompt.as_str()));
    }

    #[test]
    fn recovery_in_the_same_store_still_retires_the_old_review_identity() {
        let mut live = handle(Session::in_memory(), false);
        let owner = AgentCx::for_request();
        let _ = run(live.enter_plan_mode(&owner)).unwrap();
        assert!(!submit(&live).is_error);
        let old = live.pending_plan_review().unwrap().unwrap();
        live.session().agent.plan_state().reset_for_session(PlanMode::Planning);
        let _ = run(live.restore_plan_checkpoint(&owner)).unwrap();
        let new = live.pending_plan_review().unwrap().unwrap();
        assert_eq!(old.text(), new.text());
        assert!(!old.same_submission(&new));
        assert!(run(live.approve_plan_review(&owner, &old)).is_err());
    }

    #[test]
    fn cancelled_and_unpolled_recovery_leave_the_live_session_unchanged() {
        let mut stored = Session::in_memory();
        checkpoint(&mut stored, "approved", Some(TEXT));
        let mut live = handle(stored, false);
        let before = count(&live);
        let owner = AgentCx::for_request();
        drop(live.restore_plan_checkpoint(&owner));
        owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel recovery"));
        assert!(run(live.restore_plan_checkpoint(&owner)).is_err());
        assert!(run(live.checkpoint_plan(&owner)).is_err());
        assert_eq!(count(&live), before);
        assert_eq!(live.session().agent.plan_state().mode(), PlanMode::Off);
    }
}
