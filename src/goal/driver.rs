//! Bounded goal execution through the existing controlled SDK prompt path.
//!
//! A durable intent precedes each prompt. An abandoned run leaves that intent
//! interrupted, never permission to replay it. A host verdict, not model prose,
//! decides completion. All prompts share one absolute deadline; abort requests
//! drain the native future rather than spawning detached cleanup work.

use super::{
    GoalChange, GoalPersistence, GoalSnapshot, GoalStatus, Record, append, check_owner,
    check_snapshot, failure, latest, persist, snapshot, validate_text,
};
use crate::agent::AgentEvent;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{AssistantMessage, ContentBlock, StopReason};
use crate::sdk::AgentSessionHandle;
use crate::session_control::{
    ControllableSession, InputId, PendingInput, SessionControlHandle, TurnDeadline, TurnDeadlineError,
};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(test)]
mod tests;

/// An explicit host decision after a successful native prompt. Evidence text
/// is an attestation by the host, not proof verified by Pi. A continuation is
/// bounded and journaled; none is inferred from a completion marker in prose.
#[derive(Clone)]
pub enum GoalDecision {
    Continue { instruction: String },
    Pause { reason: String },
    Complete { evidence: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalRunStop {
    Complete,
    Paused,
    BudgetLimited,
    Deadline,
    Cancelled,
    NativeFailure,
    PersistenceUnconfirmed,
    Failed,
}

/// The last native outcome survives independently of any goal-journal error.
/// In particular, neither a save failure nor expiry discards a typed provider
/// error or a partial result. Pending inputs were NOT handed to the agent and
/// are returned in admission order; this driver never silently replays them.
#[must_use = "inspect stop, completion, error, persistence and pending input"]
pub struct GoalRunReport {
    pub stop: GoalRunStop,
    /// Latest observed snapshot. On a journal/read failure this may be stale;
    /// inspect `error` and obtain a fresh snapshot before another decision.
    pub goal: GoalSnapshot,
    pub completion: Option<std::result::Result<AssistantMessage, TurnDeadlineError>>,
    pub persistence: Option<GoalPersistence>,
    pub error: Option<Error>,
    pub pending: Vec<PendingInput>,
}

impl fmt::Debug for GoalRunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoalRunReport")
            .field("stop", &self.stop)
            .field("goal", &self.goal)
            .field("has_completion", &self.completion.is_some())
            .field("has_error", &self.error.is_some())
            .field("pending_inputs", &self.pending.len())
            .finish_non_exhaustive()
    }
}

struct ControlState {
    paused: bool,
    finished: bool,
    turn: Option<SessionControlHandle>,
    pending: Vec<PendingInput>,
}

/// A handle for ONE run, including the gaps between its prompts. `pause`
/// latches across those gaps and requests native abort when a prompt is live.
/// A handle from a finished/dropped run cannot control a later run.
#[derive(Clone)]
pub struct GoalControl {
    state: Arc<Mutex<ControlState>>,
}

impl GoalControl {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ControlState {
                paused: false,
                finished: false,
                turn: None,
                pending: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Seal continuation even between prompts. Await the run to drain native
    /// cancellation. Dropping does not promise completed async cleanup.
    pub fn pause(&self) -> bool {
        let turn = {
            let mut state = self.lock();
            if state.finished || state.paused {
                return false;
            }
            state.paused = true;
            state.turn.clone()
        };
        if let Some(turn) = turn {
            turn.abort();
        }
        true
    }

    pub fn is_finished(&self) -> bool {
        self.lock().finished
    }

    fn paused(&self) -> bool {
        let state = self.lock();
        state.paused || state.finished
    }

    fn current_turn(&self) -> Result<SessionControlHandle> {
        let state = self.lock();
        if state.paused || state.finished {
            return Err(failure("GOAL_INPUT_CLOSED", "goal run is paused or finished"));
        }
        state.turn.clone().ok_or_else(|| {
            failure("GOAL_INPUT_IDLE", "no prompt is currently accepting live input")
        })
    }

    pub fn steer(&self, text: &str) -> Result<InputId> {
        self.current_turn()?.steer(text)
    }

    pub fn follow_up(&self, text: &str) -> Result<InputId> {
        self.current_turn()?.follow_up(text)
    }

    /// Reclaim input not yet handed to the native agent, including after a
    /// dropped run. This is also a live retraction operation. Inputs returned
    /// here cannot later appear in the run report; ownership transfers once.
    pub fn take_pending(&self) -> Vec<PendingInput> {
        let mut state = self.lock();
        let mut pending = std::mem::take(&mut state.pending);
        if let Some(turn) = &state.turn {
            pending.extend(turn.take_pending());
        }
        pending
    }

    /// Install before polling, then honor any pause racing this publication.
    fn install(&self, turn: SessionControlHandle) {
        let paused = {
            let mut state = self.lock();
            state.turn = Some(turn.clone());
            state.paused || state.finished
        };
        if paused {
            turn.abort();
        }
    }

    fn retire_turn(&self) {
        self.lock().turn = None;
    }

    fn finish(&self) {
        let mut state = self.lock();
        state.finished = true;
        if let Some(turn) = state.turn.take() {
            turn.abort();
            state.pending.extend(turn.take_pending());
        }
    }
}

impl fmt::Debug for GoalControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("GoalControl")
            .field("paused", &state.paused)
            .field("finished", &state.finished)
            .field("prompt_live", &state.turn.is_some())
            .finish()
    }
}

struct RunGuard(GoalControl);

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[must_use = "await the goal run, or drop it to retire its control handle"]
pub struct GoalRun<F> {
    future: Pin<Box<F>>,
    control: GoalControl,
}

impl<F> GoalRun<F> {
    pub fn control(&self) -> GoalControl {
        self.control.clone()
    }
}

impl<F: Future<Output = GoalRunReport>> Future for GoalRun<F> {
    type Output = GoalRunReport;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// Reuses the controlled session's queue fetchers rather than registering
/// another pair per continuation. The session remains exclusively borrowed
/// throughout a run; ordinary lifecycle operations are available while idle.
pub struct GoalSession {
    session: ControllableSession,
}

impl AgentSessionHandle {
    pub fn into_goal_session(self) -> GoalSession {
        GoalSession {
            session: self.into_controllable(),
        }
    }
}

impl GoalSession {
    pub fn session_mut(&mut self) -> &mut AgentSessionHandle {
        self.session.session_mut()
    }

    /// Run the exact reviewed goal until explicit completion/pause or a limit.
    /// Construct in the runtime: the absolute deadline starts NOW, not on first
    /// poll, and includes all prompts, retries and tool loops. The host verdict
    /// callback is synchronous and cannot be preempted; expiry/cancellation is
    /// rechecked after it. Tools, approvals, hooks, compaction, native recovery
    /// and persistence continue through the existing SDK path.
    ///
    /// `Err` here is preflight (no run created). Once created, inspect the
    /// report even on failure: its native completion and unclaimed input are
    /// retained. An unused dropped run performs no goal/session/provider work.
    pub fn run_goal<'a>(
        &'a mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        timeout: Duration,
        on_event: impl Fn(&GoalControl, AgentEvent) + Send + Sync + 'static,
        decide: impl FnMut(&GoalSnapshot, &AssistantMessage) -> GoalDecision + Send + 'a,
    ) -> Result<GoalRun<impl Future<Output = GoalRunReport> + 'a>> {
        let deadline = TurnDeadline::after(owner, timeout)?;
        let owner = owner.clone();
        let expected = expected.clone();
        let control = GoalControl::new();
        // Capture the guard before creating the async body: dropping an
        // UNPOLLED run still seals any handle already given to another thread.
        let guard = RunGuard(control.clone());
        let run_control = control.clone();
        let future = async move {
            let _guard = guard;
            owner
                .with_current(self.drive(&owner, expected, deadline, run_control, on_event, decide))
                .await
        };
        Ok(GoalRun {
            future: Box::pin(future),
            control,
        })
    }

    async fn drive(
        &mut self,
        owner: &AgentCx,
        expected: GoalSnapshot,
        deadline: TurnDeadline,
        control: GoalControl,
        on_event: impl Fn(&GoalControl, AgentEvent) + Send + Sync + 'static,
        mut decide: impl FnMut(&GoalSnapshot, &AssistantMessage) -> GoalDecision + Send,
    ) -> GoalRunReport {
        let mut report = GoalRunReport {
            stop: GoalRunStop::Paused,
            goal: expected,
            completion: None,
            persistence: None,
            error: None,
            pending: Vec::new(),
        };
        let on_event = Arc::new(on_event);
        if let Err(error) = verify_current(self.session_mut(), &report.goal) {
            return failed(report, error);
        }
        loop {
            if let Some(stop) = interruption(owner, &deadline, &control) {
                report.stop = stop;
                return report;
            }
            if report.goal.status() != GoalStatus::Active {
                report.stop = if report.goal.status() == GoalStatus::BudgetLimited {
                    GoalRunStop::BudgetLimited
                } else {
                    GoalRunStop::Paused
                };
                return report;
            }
            let intent = match start_attempt(self.session_mut(), owner, &report.goal).await {
                Ok(change) => change,
                Err(error) => return failed(report, error),
            };
            report.goal = intent.goal;
            report.persistence = Some(intent.persistence.clone());
            if !intent.persistence.allows_continuation() {
                report.stop = GoalRunStop::PersistenceUnconfirmed;
                return report;
            }
            let ticket = report.goal.clone();
            let prompt = prompt_for(&ticket.record);
            let callback = Arc::clone(&on_event);
            let callback_control = control.clone();
            let turn = match self.session.prompt_with_control(prompt, move |_, event| {
                callback(&callback_control, event);
            }) {
                Ok(turn) => turn,
                Err(error) => return failed(report, error),
            };
            let native_control = turn.control();
            control.install(native_control.clone());
            report.completion = Some(turn.with_deadline(deadline.clone()).await);
            control.retire_turn();
            report.pending = native_control.take_pending();

            let observed = match self.session_mut().current_goal() {
                Ok(Some(goal)) => goal,
                Ok(None) => return failed(report, failure("GOAL_MISSING", "goal disappeared during execution")),
                Err(error) => return failed(report, error),
            };
            if let Err(error) = check_intent(&ticket, &observed) {
                return failed(report, error);
            }
            report.goal = observed;
            let (decision, stop) = decide_next(&mut report, owner, &deadline, &control, &mut decide);
            let (settled, late_stop) = match settle_attempt(
                self.session_mut(), owner, &report.goal, decision, &deadline, &control,
            ).await {
                Ok(settled) => settled,
                Err(error) => {
                    report.stop = match interruption(owner, &deadline, &control).or(stop) {
                        Some(GoalRunStop::Cancelled) => GoalRunStop::Cancelled,
                        Some(GoalRunStop::Deadline) => GoalRunStop::Deadline,
                        Some(GoalRunStop::NativeFailure) => GoalRunStop::NativeFailure,
                        _ => GoalRunStop::Failed,
                    };
                    report.error = Some(error);
                    return report;
                }
            };
            report.goal = settled.goal;
            report.persistence = Some(settled.persistence.clone());
            if !settled.persistence.allows_continuation() {
                report.stop = GoalRunStop::PersistenceUnconfirmed;
                return report;
            }
            if let Some(stop) = late_stop.or(stop) {
                report.stop = stop;
                return report;
            }
            if report.goal.status() == GoalStatus::BudgetLimited {
                report.stop = GoalRunStop::BudgetLimited;
                return report;
            }
        }
    }
}

fn failed(mut report: GoalRunReport, error: Error) -> GoalRunReport {
    report.stop = GoalRunStop::Failed;
    report.error = Some(error);
    report
}

fn interruption(owner: &AgentCx, deadline: &TurnDeadline, control: &GoalControl) -> Option<GoalRunStop> {
    if owner.checkpoint().is_err() {
        Some(GoalRunStop::Cancelled)
    } else if control.paused() {
        Some(GoalRunStop::Paused)
    } else if deadline.is_elapsed() {
        Some(GoalRunStop::Deadline)
    } else {
        None
    }
}

fn pause_decision(reason: &str) -> GoalDecision {
    GoalDecision::Pause { reason: reason.to_string() }
}

fn successful(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Stop
        && message.error_message.is_none()
        && !message.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(_)))
        && message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text(text) if !text.text.trim().is_empty())
        })
}

fn decide_next(
    report: &mut GoalRunReport,
    owner: &AgentCx,
    deadline: &TurnDeadline,
    control: &GoalControl,
    decide: &mut impl FnMut(&GoalSnapshot, &AssistantMessage) -> GoalDecision,
) -> (GoalDecision, Option<GoalRunStop>) {
    if let Some(stop) = interruption(owner, deadline, control) {
        return (pause_decision("run interrupted; inspect native completion before resuming"), Some(stop));
    }
    let Some(Ok(message)) = report.completion.as_ref() else {
        let stop = match report.completion.as_ref() {
            Some(Err(TurnDeadlineError::Elapsed { .. })) => GoalRunStop::Deadline,
            Some(Err(TurnDeadlineError::OwnerCancelled { .. })) => GoalRunStop::Cancelled,
            _ => GoalRunStop::NativeFailure,
        };
        return (
            pause_decision("native prompt failed; inspect completion before resuming"),
            Some(stop),
        );
    };
    if !successful(message) {
        return (pause_decision("native prompt did not produce a successful final answer"), Some(GoalRunStop::NativeFailure));
    }
    if !report.pending.is_empty() {
        return (
            pause_decision("unclaimed user input returned to host"),
            Some(GoalRunStop::Paused),
        );
    }
    let decision = decide(&report.goal, message);
    // A host callback may request pause or take time. Its late verdict must
    // not turn an interrupted run into success or authorize another prompt.
    if let Some(stop) = interruption(owner, deadline, control) {
        return (pause_decision("run interrupted while host reviewed the result"), Some(stop));
    }
    let (text, stop) = match &decision {
        GoalDecision::Continue { instruction } => (instruction, None),
        GoalDecision::Pause { reason } => (reason, Some(GoalRunStop::Paused)),
        GoalDecision::Complete { evidence } => (evidence, Some(GoalRunStop::Complete)),
    };
    if let Err(error) = validate_text(text, "host verdict") {
        report.error = Some(error);
        return (pause_decision("host verdict was empty or oversized"), Some(GoalRunStop::Failed));
    }
    (decision, stop)
}

fn prepare_intent(mut record: Record) -> Result<Record> {
    if record.effective_status() != GoalStatus::Active || record.in_flight {
        return Err(failure("GOAL_NOT_ACTIVE", "explicitly resume an interrupted goal before execution"));
    }
    record.prompts_started = record.prompts_started.checked_add(1)
        .ok_or_else(|| failure("GOAL_BUDGET", "prompt counter overflow"))?;
    record.in_flight = true;
    Ok(record)
}

fn prompt_for(record: &Record) -> String {
    let instruction = if !record.note.is_empty() {
        record.note.as_str()
    } else if record.prompts_started <= 1 {
        "Work on the objective and report concrete progress and verification."
    } else {
        "Continue from the existing conversation. Do not replay completed tool operations. Verify the remaining deliverables."
    };
    format!("Goal {}:\n{}\n\n{}", record.id, record.objective, instruction)
}

fn check_intent(ticket: &GoalSnapshot, observed: &GoalSnapshot) -> Result<()> {
    let mut record = observed.record.clone();
    record.tokens_spent = ticket.record.tokens_spent;
    if ticket.store.as_ptr() != observed.store.as_ptr()
        || ticket.session_id != observed.session_id
        || ticket.checkpoint_id != observed.checkpoint_id
        || ticket.record != record
        || observed.tokens_spent() < ticket.tokens_spent()
    {
        return Err(failure("GOAL_STALE", "goal intent or session changed during the prompt"));
    }
    Ok(())
}

fn verify_current(handle: &AgentSessionHandle, expected: &GoalSnapshot) -> Result<()> {
    let store = handle.session_store();
    let session = store.try_lock().map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
    let (id, record) = latest(&session)?.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
    check_snapshot(expected, &store, &session, &id, &record)
}

fn settle_record(mut record: Record, decision: GoalDecision) -> Result<Record> {
    if !record.in_flight || record.status != GoalStatus::Active {
        return Err(failure("GOAL_INTENT", "no matching unfinished goal prompt"));
    }
    let (note, status) = match decision {
        GoalDecision::Continue { instruction } => {
            let status = if record.exhausted() { GoalStatus::BudgetLimited } else { GoalStatus::Active };
            (instruction, status)
        }
        GoalDecision::Pause { reason } => (reason, GoalStatus::Paused),
        GoalDecision::Complete { evidence } => (evidence, GoalStatus::Complete),
    };
    validate_text(&note, "host verdict")?;
    record.note = note;
    record.status = status;
    record.in_flight = false;
    Ok(record)
}

async fn write_attempt(
    handle: &mut AgentSessionHandle,
    owner: &AgentCx,
    expected: &GoalSnapshot,
    edit: impl FnOnce(Record) -> Result<Record>,
) -> Result<GoalChange> {
    let enabled = handle.session().save_enabled();
    check_owner(owner, enabled)?;
    let store = handle.session_store();
    let mut session = store.try_lock_owned().map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
    let (id, record) = latest(&session)?.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
    check_snapshot(expected, &store, &session, &id, &record)?;
    let record = edit(record)?;
    check_owner(owner, enabled)?;
    let id = append(&mut session, &record)?;
    handle.session_mut().invalidate_background_compaction();
    let goal = snapshot(&store, &session, id, record);
    let persistence = persist(&mut session, owner, enabled).await;
    Ok(GoalChange { goal, persistence })
}

async fn start_attempt(handle: &mut AgentSessionHandle, owner: &AgentCx, expected: &GoalSnapshot) -> Result<GoalChange> {
    write_attempt(handle, owner, expected, prepare_intent).await
}

async fn settle_attempt(
    handle: &mut AgentSessionHandle,
    owner: &AgentCx,
    expected: &GoalSnapshot,
    decision: GoalDecision,
    deadline: &TurnDeadline,
    control: &GoalControl,
) -> Result<(GoalChange, Option<GoalRunStop>)> {
    let enabled = handle.session().save_enabled();
    check_owner(owner, enabled)?;
    let store = handle.session_store();
    let mut session = store.try_lock_owned().map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
    let (goal, late_stop) = {
        // Pause and completion have one linearization boundary. A pause that
        // wins this lock cannot be overwritten by an already computed verdict.
        // The synchronous guard is gone before persistence can suspend.
        let state = control.lock();
        let (id, record) = latest(&session)?.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
        check_snapshot(expected, &store, &session, &id, &record)?;
        let late_stop = if state.paused || state.finished {
            Some(GoalRunStop::Paused)
        } else if deadline.is_elapsed() {
            Some(GoalRunStop::Deadline)
        } else {
            None
        };
        let decision = if late_stop.is_some() {
            pause_decision("run interrupted before the host verdict was committed")
        } else {
            decision
        };
        let record = settle_record(record, decision)?;
        check_owner(owner, enabled)?;
        let id = append(&mut session, &record)?;
        (snapshot(&store, &session, id, record), late_stop)
    };
    handle.session_mut().invalidate_background_compaction();
    let persistence = persist(&mut session, owner, enabled).await;
    Ok((GoalChange { goal, persistence }, late_stop))
}
