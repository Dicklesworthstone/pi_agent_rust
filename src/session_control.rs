//! Live, turn-scoped control for in-process SDK sessions.
//!
//! A prompt borrows its session mutably until it finishes. The control handle
//! does not: another thread can steer, queue a follow-up, or abort while the
//! provider is streaming. Inputs use the agent's existing message fetchers,
//! not a command loop that is blocked waiting for the prompt to finish.
//!
//! Acceptance means in-memory queue admission, NOT persistence or provider
//! acknowledgement. Unclaimed input remains recoverable after failure or
//! cancellation. Input already handed to the agent follows its normal history
//! and persistence rules and must not be blindly replayed by the caller.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use crate::agent::{AbortHandle, AbortSignal, AgentEvent, MessageFetcher, QueuedAgentMessage};
use crate::error::{Error, Result};
use crate::model::{AssistantMessage, Message, UserContent, UserMessage};
use crate::sdk::AgentSessionHandle;

#[cfg(test)]
mod agent_tests;
mod attachments;
mod deadline;
mod execution;
mod recovery;
pub use deadline::{DeadlineTurn, TurnDeadline, TurnDeadlineError};
pub use recovery::TransferredInput;

const MAX_PENDING_INPUTS: usize = 100;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// The existing agent loop decides the exact safe delivery boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Delivered at a steering boundary, without restarting completed tools.
    Steering,
    /// Delivered after the current model turn finishes.
    FollowUp,
}

/// An opaque input identity, not a durable receipt. Identities do not repeat
/// between turns, so a stale retraction cannot select a new turn's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputId(uuid::Uuid);

/// An ordered yield point in one turn's mailbox. A stale identity cannot
/// release a later barrier, even when the same session starts another turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YieldBarrierId(uuid::Uuid);

/// An input which has not been handed to the agent and is safe to reclaim.
/// Debug output deliberately excludes its potentially private text.
#[derive(Clone)]
pub struct PendingInput {
    pub id: InputId,
    pub kind: InputKind,
    /// Original user-authored prose, before template/attachment expansion.
    pub text: String,
    /// Exact provider-visible text/image/media payload, never flattened.
    pub content: UserContent,
    bytes: usize,
}

impl fmt::Debug for PendingInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingInput")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// Mailbox state only. `handed_to_agent` does not mean model acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlSnapshot {
    pub accepting_input: bool,
    pub finished: bool,
    pub pending_steering: usize,
    pub pending_follow_up: usize,
    pub pending_bytes: usize,
    pub handed_to_agent: u64,
}

struct RunData {
    accepting_input: bool,
    finished: bool,
    pending: VecDeque<PendingInput>,
    pending_bytes: usize,
    handed_to_agent: u64,
    yield_barrier: Option<YieldBarrier>,
    cancelled: bool,
    completed_successfully: bool,
}

struct YieldBarrier {
    id: YieldBarrierId,
    /// Number of still-unclaimed inputs ahead of the barrier. Corrections
    /// retain their queue position; only removal from this prefix changes it.
    preceding_inputs: usize,
    refused: bool,
}

struct Run {
    data: Mutex<RunData>,
    abort: AbortHandle,
}

type ActiveRun = Arc<Mutex<Weak<Run>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn control_error(code: &str, message: &str) -> Error {
    Error::validation(format!("[{code}] {message}"))
}

impl Run {
    fn new() -> (Arc<Self>, AbortSignal) {
        let (abort, signal) = AbortHandle::new();
        let run = Arc::new(Self {
            data: Mutex::new(RunData {
                accepting_input: true,
                finished: false,
                pending: VecDeque::new(),
                pending_bytes: 0,
                handed_to_agent: 0,
                yield_barrier: None,
                cancelled: false,
                completed_successfully: false,
            }),
            abort,
        });
        (run, signal)
    }

    fn fetch(&self, kind: InputKind) -> Vec<QueuedAgentMessage> {
        let mut data = lock(&self.data);
        if !data.accepting_input {
            return Vec::new();
        }
        let available = data
            .yield_barrier
            .as_ref()
            .map_or(data.pending.len(), |barrier| barrier.preceding_inputs);
        let Some(index) = data
            .pending
            .iter()
            .take(available)
            .position(|input| input.kind == kind)
        else {
            return Vec::new();
        };
        let Some(input) = data.pending.remove(index) else {
            return Vec::new();
        };
        data.pending_bytes -= input.bytes;
        if let Some(barrier) = &mut data.yield_barrier {
            barrier.preceding_inputs -= 1;
        }
        data.handed_to_agent = data.handed_to_agent.saturating_add(1);
        drop(data);
        // One item per fetch avoids pre-draining a follow-up backlog into the
        // agent. Preserve exactly the authored text for keyword scanning.
        vec![QueuedAgentMessage::authored(
            Message::User(UserMessage {
                content: input.content,
                timestamp: chrono::Utc::now().timestamp_millis(),
            }),
            input.text,
        )]
    }

    fn yield_ready(&self) -> bool {
        lock(&self.data)
            .yield_barrier
            .as_ref()
            .is_some_and(|barrier| barrier.preceding_inputs == 0)
    }
}

fn fetcher(active: &ActiveRun, kind: InputKind) -> MessageFetcher {
    let active = Arc::clone(active);
    Arc::new(move || {
        // Bind the source at construction, but dequeue only when polled. A
        // delayed fetch from an old turn must never read the next turn's input.
        let run = lock(&active).upgrade();
        Box::pin(async move {
            // Constructing an unpolled fetch future must not consume input.
            if crate::agent_cx::AgentCx::for_current_or_request()
                .checkpoint()
                .is_err()
            {
                return Vec::new();
            }
            run.map_or_else(Vec::new, |run| run.fetch(kind))
        })
    })
}

/// Clone before awaiting the turn; use from another thread or event callback.
/// A handle is permanently tied to ONE turn. A late abort or input cannot
/// control a subsequent turn, even on the same session.
#[derive(Clone)]
pub struct SessionControlHandle {
    run: Arc<Run>,
}

impl fmt::Debug for SessionControlHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionControlHandle")
            .field("state", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl SessionControlHandle {
    /// Whether an ordered host operation still owns this turn's held input.
    #[must_use]
    pub fn has_yield_barrier(&self) -> bool {
        lock(&self.run.data).yield_barrier.is_some()
    }

    /// Insert one ordered pause after input already accepted by this turn.
    /// Existing input retains its lane and runs first; input admitted later
    /// remains recoverable in the mailbox. The agent finishes its current
    /// assistant/tool work and the SDK completes its normal persistence before
    /// returning. This never aborts a tool or submits a synthetic prompt.
    ///
    /// The host owns the operation to run after yielding, such as manual
    /// compaction. Await this turn and inspect its result before performing
    /// that operation; a ready barrier alone is not a completion receipt.
    pub fn insert_yield_barrier(&self) -> Result<YieldBarrierId> {
        let mut data = lock(&self.run.data);
        if !data.accepting_input || data.finished {
            return Err(control_error(
                "SESSION_CONTROL_CLOSED",
                "this turn no longer accepts input",
            ));
        }
        if data.yield_barrier.is_some() {
            return Err(control_error(
                "SESSION_CONTROL_BARRIER",
                "this turn already has a pending yield barrier",
            ));
        }
        let id = YieldBarrierId(uuid::Uuid::new_v4());
        data.yield_barrier = Some(YieldBarrier {
            id,
            preceding_inputs: data.pending.len(),
            refused: false,
        });
        Ok(id)
    }

    /// Whether all inputs preceding this barrier have been handed off or
    /// reclaimed. The currently running provider/tools may still be active.
    #[must_use]
    pub fn yield_barrier_ready(&self, id: YieldBarrierId) -> bool {
        lock(&self.run.data)
            .yield_barrier
            .as_ref()
            .is_some_and(|barrier| barrier.id == id && barrier.preceding_inputs == 0)
    }

    /// True only after this controlled turn returned a successful, durably
    /// completed normal stop with the barrier's preceding input drained.
    /// Abort, failure, a run-time cap, and dropping the future never admit the
    /// host operation. The host must still check its session/branch identity.
    #[must_use]
    pub fn yield_barrier_can_run(&self, id: YieldBarrierId) -> bool {
        let data = lock(&self.run.data);
        data.finished
            && data.completed_successfully
            && !data.cancelled
            && data.yield_barrier.as_ref().is_some_and(|barrier| {
                barrier.id == id && barrier.preceding_inputs == 0 && !barrier.refused
            })
    }

    /// Refuse the current host operation without losing its restoration
    /// boundary or held input. Used when the owning session/branch changed.
    /// A later, explicitly inserted barrier starts with its own admission.
    pub(crate) fn invalidate_yield_barrier(&self) {
        if let Some(barrier) = &mut lock(&self.run.data).yield_barrier {
            barrier.refused = true;
        }
    }

    /// Cancel only this barrier, leaving every queued input in place. If the
    /// turn is still running, its ordinary input delivery can resume.
    pub fn remove_yield_barrier(&self, id: YieldBarrierId) -> bool {
        let mut data = lock(&self.run.data);
        if data
            .yield_barrier
            .as_ref()
            .is_none_or(|barrier| barrier.id != id)
        {
            return false;
        }
        data.yield_barrier = None;
        true
    }

    /// Reclaim this barrier and all unclaimed inputs atomically. The two
    /// vectors contain input before and after the host's queued operation,
    /// preserving admission order and exact native content within each part.
    /// Useful both after a successful yield and for ordered editor restoration
    /// after cancellation. A stale identity changes nothing and returns None.
    pub fn take_pending_at_barrier(
        &self,
        id: YieldBarrierId,
    ) -> Option<(Vec<PendingInput>, Vec<PendingInput>)> {
        let mut data = lock(&self.run.data);
        let barrier = data
            .yield_barrier
            .as_ref()
            .filter(|barrier| barrier.id == id)?;
        let preceding = barrier.preceding_inputs;
        let before = data.pending.drain(..preceding).collect();
        let after = data.pending.drain(..).collect();
        data.pending_bytes = 0;
        data.yield_barrier = None;
        Some((before, after))
    }

    /// Queue exact user-authored text for the next steering boundary.
    pub fn steer(&self, text: &str) -> Result<InputId> {
        self.enqueue(InputKind::Steering, text)
    }

    /// Queue exact user-authored text for a subsequent model turn.
    pub fn follow_up(&self, text: &str) -> Result<InputId> {
        self.enqueue(InputKind::FollowUp, text)
    }

    fn enqueue(&self, kind: InputKind, text: &str) -> Result<InputId> {
        if text.len() > MAX_INPUT_BYTES || text.trim().is_empty() {
            return Err(control_error(
                "SESSION_CONTROL_INPUT",
                "input must be nonblank and at most 256 KiB",
            ));
        }
        self.enqueue_content(kind, &UserContent::Text(text.to_string()), text)
    }

    /// Request cancellation of this turn only and stop dequeuing new input.
    /// Await the turn for normal cancellation cleanup; dropping its future
    /// does not claim that async cleanup or persistence completed.
    /// Returns false once the turn is finished or already stopped accepting.
    pub fn abort(&self) -> bool {
        {
            let mut data = lock(&self.run.data);
            if !data.accepting_input {
                return false;
            }
            data.accepting_input = false;
            data.cancelled = true;
        }
        self.run.abort.abort();
        true
    }

    /// Reclaim all still-unclaimed inputs in their original admission order.
    /// This is linearized against fetches: an item is either returned here OR
    /// handed to the agent, never both. It may also be used to retract input
    /// while a turn is live. Already handed-off input cannot be retracted.
    /// Reclaiming all input also invalidates any pending yield barrier.
    pub fn take_pending(&self) -> Vec<PendingInput> {
        let mut data = lock(&self.run.data);
        data.pending_bytes = 0;
        data.yield_barrier = None;
        data.pending.drain(..).collect()
    }

    /// Retract exactly one unclaimed input. `None` means it was already
    /// handed off, reclaimed, or never admitted by this turn's handle.
    pub fn retract(&self, id: InputId) -> Option<PendingInput> {
        let mut data = lock(&self.run.data);
        let index = data.pending.iter().position(|input| input.id == id)?;
        let input = data.pending.remove(index)?;
        data.pending_bytes -= input.bytes;
        if let Some(barrier) = &mut data.yield_barrier
            && index < barrier.preceding_inputs
        {
            barrier.preceding_inputs -= 1;
        }
        drop(data);
        Some(input)
    }

    #[must_use]
    pub fn snapshot(&self) -> ControlSnapshot {
        let data = lock(&self.run.data);
        let steering = data
            .pending
            .iter()
            .filter(|input| input.kind == InputKind::Steering)
            .count();
        ControlSnapshot {
            accepting_input: data.accepting_input,
            finished: data.finished,
            pending_steering: steering,
            pending_follow_up: data.pending.len() - steering,
            pending_bytes: data.pending_bytes,
            handed_to_agent: data.handed_to_agent,
        }
    }
}

struct TurnGuard {
    active: ActiveRun,
    run: Arc<Run>,
    /// Keeps this turn's Agent-side message readers registered. Dropping the
    /// guard retires the lease; the Agent prunes the readers before polling.
    fetcher_lifetime: Arc<()>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        {
            let mut data = lock(&self.run.data);
            data.accepting_input = false;
            data.finished = true;
        }
        self.run.abort.abort();
        let mut active = lock(&self.active);
        if active
            .upgrade()
            .is_some_and(|run| Arc::ptr_eq(&run, &self.run))
        {
            *active = Weak::new();
        }
        // Unclaimed input is retained by the caller's control handle. Never
        // silently feed it to the next prompt or discard it on an error path.
    }
}

/// A normal agent turn future with a separately cloneable live control lane.
/// Dropping even an UNPOLLED turn closes its control handle.
#[must_use = "await the turn, or drop it to retire its control handle"]
pub struct ControlledTurn<F> {
    future: Pin<Box<F>>,
    control: SessionControlHandle,
    // Deadline preflight must distinguish unused input from a turn which was
    // already polled before the host attached a deadline wrapper.
    polled: bool,
}

impl<F> ControlledTurn<F> {
    #[must_use]
    pub fn control(&self) -> SessionControlHandle {
        self.control.clone()
    }
}

impl<F: Future<Output = Result<AssistantMessage>>> Future for ControlledTurn<F> {
    type Output = Result<AssistantMessage>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.polled = true;
        let result = this.future.as_mut().poll(cx);
        if let Poll::Ready(completion) = &result {
            let mut data = lock(&this.control.run.data);
            data.completed_successfully = !data.cancelled
                && matches!(completion, Ok(message)
                    if message.stop_reason == crate::model::StopReason::Stop
                        && crate::agent::time_cap_marker(message).is_none());
        }
        result
    }
}

/// In-process SDK driver with live input and cancellation.
///
/// Each turn registers scoped steering/follow-up readers on the current agent.
/// Idle session management stays available through `session_mut`; Rust prevents
/// access during a turn. Text and native-content prompts share the SDK's
/// validation, retry, failover, hooks and persistence boundaries.
/// Each turn retains its initiating scoped context across polls. With no scoped
/// context at construction it binds once on first poll, allowing construction
/// outside a runtime. Owner cancellation wakes the turn, seals live input and
/// requests native abort; await completion to drain cleanup and inspect errors.
pub struct ControllableSession {
    session: AgentSessionHandle,
    active: ActiveRun,
}

impl AgentSessionHandle {
    /// Add live, bounded, turn-scoped control without changing providers,
    /// listeners, retry/failover policy, tools, or session persistence.
    #[must_use]
    pub fn into_controllable(self) -> ControllableSession {
        ControllableSession {
            session: self,
            active: Arc::new(Mutex::new(Weak::new())),
        }
    }
}

impl AgentSessionHandle {
    /// One controlled prompt turn on a borrowed handle, for hosts that keep
    /// the handle between turns (and may replace it, e.g. `/resume`) rather
    /// than converting it once with [`Self::into_controllable`]. This turn's
    /// steering and follow-up fetchers are installed on the current agent, so
    /// a replaced session is never left without them. Same queue, bounds,
    /// cancellation and unclaimed-input guarantees as
    /// [`ControllableSession::prompt`].
    pub fn prompt_controlled(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_> {
        self.prompt_controlled_with_images(input, Vec::new(), on_event)
    }

    /// [`Self::prompt_controlled`] with image attachments (`@file` images, a
    /// pasted screenshot). With no images it is exactly the text turn.
    pub fn prompt_controlled_with_images(
        &mut self,
        input: String,
        images: Vec<crate::model::ImageContent>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_> {
        let content = if images.is_empty() {
            UserContent::Text(input)
        } else {
            let mut blocks = Vec::with_capacity(images.len() + 1);
            if !input.is_empty() {
                blocks.push(crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(input),
                ));
            }
            blocks.extend(images.into_iter().map(crate::model::ContentBlock::Image));
            UserContent::Blocks(blocks)
        };
        self.prompt_controlled_input(content, on_event)
    }

    /// Run ordered native text, image, audio and video input with the same
    /// turn-scoped steering, cancellation, persistence and retry behavior.
    pub fn prompt_controlled_with_content(
        &mut self,
        content: Vec<crate::model::ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_> {
        self.prompt_controlled_input(UserContent::Blocks(content), on_event)
    }

    pub(crate) fn prompt_controlled_input(
        &mut self,
        content: UserContent,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_> {
        let active: ActiveRun = Arc::new(Mutex::new(Weak::new()));
        let (run, signal) = Run::new();
        *lock(&active) = Arc::downgrade(&run);
        let fetcher_lifetime = Arc::new(());
        self.session_mut().agent.register_scoped_message_fetchers(
            &fetcher_lifetime,
            Some(fetcher(&active, InputKind::Steering)),
            Some(fetcher(&active, InputKind::FollowUp)),
        );
        let yield_run = Arc::downgrade(&run);
        self.session_mut().agent.register_scoped_yield_check(
            &fetcher_lifetime,
            Arc::new(move || yield_run.upgrade().is_some_and(|run| run.yield_ready())),
        );
        let guard = TurnGuard {
            active,
            run,
            fetcher_lifetime,
        };
        let control = SessionControlHandle {
            run: Arc::clone(&guard.run),
        };
        let session = self;
        let future = async move {
            let _guard = guard;
            match content {
                UserContent::Text(text) => session.prompt_with_abort(text, signal, on_event).await,
                UserContent::Blocks(blocks) => {
                    session
                        .prompt_with_content_with_abort(blocks, signal, on_event)
                        .await
                }
            }
        };
        ControlledTurn {
            future: Box::pin(execution::OwnedTurn::new(future, control.clone())),
            control,
            polled: false,
        }
    }
}

impl ControllableSession {
    pub const fn session_mut(&mut self) -> &mut AgentSessionHandle {
        &mut self.session
    }

    fn begin(&mut self) -> Result<(TurnGuard, AbortSignal)> {
        let mut active = lock(&self.active);
        if active.upgrade().is_some() {
            return Err(control_error(
                "SESSION_CONTROL_BUSY",
                "a controlled turn is still active",
            ));
        }
        let (run, signal) = Run::new();
        *active = Arc::downgrade(&run);
        drop(active);
        let fetcher_lifetime = Arc::new(());
        self.session
            .session_mut()
            .agent
            .register_scoped_message_fetchers(
                &fetcher_lifetime,
                Some(fetcher(&self.active, InputKind::Steering)),
                Some(fetcher(&self.active, InputKind::FollowUp)),
            );
        let yield_run = Arc::downgrade(&run);
        self.session
            .session_mut()
            .agent
            .register_scoped_yield_check(
                &fetcher_lifetime,
                Arc::new(move || yield_run.upgrade().is_some_and(|run| run.yield_ready())),
            );
        Ok((
            TurnGuard {
                active: Arc::clone(&self.active),
                run,
                fetcher_lifetime,
            },
            signal,
        ))
    }

    /// Build a prompt turn. Obtain `turn.control()` before awaiting it.
    /// The original SDK path owns retries, failover, hooks and persistence.
    pub fn prompt(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        self.prompt_with_control(input, move |_, event| on_event(event))
    }

    /// Like `prompt`, but each event callback also receives this turn's
    /// control lane. Hosts can react to streaming/tool events without a
    /// shared mutable session, a command-loop deadlock, or a setup race.
    pub fn prompt_with_control(
        &mut self,
        input: String,
        on_event: impl Fn(&SessionControlHandle, AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        self.prompt_input_with_control(UserContent::Text(input), on_event)
    }

    /// Build a live-controlled prompt with ordered native text, image, audio
    /// and video blocks. Obtain `turn.control()` before awaiting the result.
    ///
    /// Payloads are moved intact into the ordinary SDK content path, not
    /// flattened or queued as follow-ups. The SDK validates user blocks before
    /// provider admission; model capability filtering, input hooks, recovery
    /// and persistence remain authoritative. This API does not read files or
    /// fetch URLs, and makes no new claim about a provider's media support.
    ///
    /// As with text turns, cancellation before the first poll dispatches no
    /// prompt. Await an admitted turn to finish native cleanup; dropping it
    /// cannot promise completed persistence. Unclaimed control input remains
    /// recoverable from the turn's control handle.
    pub fn prompt_with_content(
        &mut self,
        content: Vec<crate::model::ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        self.prompt_with_content_with_control(content, move |_, event| on_event(event))
    }

    /// Native-content counterpart of [`Self::prompt_with_control`]. Each
    /// callback receives this turn's control handle, allowing a host to steer,
    /// queue a follow-up or abort in response to provider/tool events without
    /// borrowing the session or publishing the handle through a separate slot.
    pub fn prompt_with_content_with_control(
        &mut self,
        content: Vec<crate::model::ContentBlock>,
        on_event: impl Fn(&SessionControlHandle, AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        self.prompt_input_with_control(UserContent::Blocks(content), on_event)
    }

    fn prompt_input_with_control(
        &mut self,
        content: UserContent,
        on_event: impl Fn(&SessionControlHandle, AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        // Text, native blocks and image helpers share this one constructor.
        // begin registers scoped fetchers on the current agent for this turn;
        // the guard retires them when the turn finishes or is dropped.
        let (guard, signal) = self.begin()?;
        let control = SessionControlHandle {
            run: Arc::clone(&guard.run),
        };
        let callback_control = control.clone();
        let session = &mut self.session;
        let future = async move {
            // Capture the guard in the future so cancellation before its first
            // poll is just as safe as dropping it during a provider request.
            let _guard = guard;
            let callback = move |event| on_event(&callback_control, event);
            match content {
                UserContent::Text(text) => session.prompt_with_abort(text, signal, callback).await,
                UserContent::Blocks(blocks) => {
                    session
                        .prompt_with_content_with_abort(blocks, signal, callback)
                        .await
                }
            }
        };
        Ok(ControlledTurn {
            future: Box::pin(execution::OwnedTurn::new(future, control.clone())),
            control,
            polled: false,
        })
    }

    /// Resume through the SDK's existing continuation/retry path. This does
    /// not add or replay the user's original prompt or completed tool calls.
    pub fn continue_turn(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        let (guard, signal) = self.begin()?;
        let control = SessionControlHandle {
            run: Arc::clone(&guard.run),
        };
        let session = &mut self.session;
        let future = async move {
            let _guard = guard;
            session.continue_turn_with_abort(signal, on_event).await
        };
        Ok(ControlledTurn {
            future: Box::pin(execution::OwnedTurn::new(future, control.clone())),
            control,
            polled: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn live() -> (SessionControlHandle, TurnGuard, AbortSignal) {
        let (run, signal) = Run::new();
        let active = Arc::new(Mutex::new(Arc::downgrade(&run)));
        (
            SessionControlHandle {
                run: Arc::clone(&run),
            },
            TurnGuard {
                active,
                run,
                fetcher_lifetime: Arc::new(()),
            },
            signal,
        )
    }

    #[test]
    fn queues_preserve_kind_order_and_exact_authored_text() {
        let (control, _guard, _) = live();
        control.follow_up("next").unwrap();
        control.steer("  change course\n日本語  ").unwrap();
        control.steer("then inspect").unwrap();
        let first = control.run.fetch(InputKind::Steering);
        assert_eq!(
            first[0].keyword_scan_source(),
            Some("  change course\n日本語  ")
        );
        assert_eq!(
            control.run.fetch(InputKind::Steering)[0].text_for_display(),
            Some("then inspect")
        );
        assert_eq!(
            control.run.fetch(InputKind::FollowUp)[0].text_for_display(),
            Some("next")
        );
        assert_eq!(control.snapshot().handed_to_agent, 3);
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn full_queue_rejects_new_input_without_losing_oldest() {
        let (control, _guard, _) = live();
        for index in 0..MAX_PENDING_INPUTS {
            control.steer(&index.to_string()).unwrap();
        }
        assert!(
            control
                .follow_up("overflow")
                .unwrap_err()
                .to_string()
                .contains("SESSION_CONTROL_FULL")
        );
        let pending = control.take_pending();
        assert_eq!(pending.len(), MAX_PENDING_INPUTS);
        assert_eq!(pending[0].text, "0");
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn byte_limits_are_shared_by_both_lanes_and_refunded() {
        let (control, _guard, _) = live();
        let text = "x".repeat(MAX_INPUT_BYTES);
        while control.follow_up(&text).is_ok() {}
        assert!(control.snapshot().pending_bytes <= MAX_PENDING_BYTES);
        assert!(control.steer(&text).is_err());
        control.run.fetch(InputKind::FollowUp);
        assert!(control.steer(&text).is_ok());
        assert!(control.steer(&"x".repeat(MAX_INPUT_BYTES + 1)).is_err());
        assert!(control.steer(" \n\t ").is_err());
    }

    #[test]
    fn abort_seals_input_and_preserves_unclaimed_work() {
        let (control, _guard, signal) = live();
        control.follow_up("recover me").unwrap();
        assert!(control.abort());
        assert!(signal.is_aborted());
        assert!(!control.abort());
        assert!(control.steer("too late").is_err());
        assert!(control.run.fetch(InputKind::FollowUp).is_empty());
        assert_eq!(control.take_pending()[0].text, "recover me");
    }

    #[test]
    fn guard_drop_closes_handles_without_discarding_input() {
        let (control, guard, _) = live();
        control.steer("not yet claimed").unwrap();
        let active = Arc::clone(&guard.active);
        drop(guard);
        assert!(control.snapshot().finished);
        assert!(!control.snapshot().accepting_input);
        assert!(lock(&active).upgrade().is_none());
        assert_eq!(control.take_pending()[0].text, "not yet claimed");
    }

    #[test]
    fn unpolled_turn_drop_retires_its_guard() {
        let (control, guard, _) = live();
        let future = async move {
            let _guard = guard;
            std::future::pending::<Result<AssistantMessage>>().await
        };
        let turn = ControlledTurn {
            future: Box::pin(future),
            control: control.clone(),
            polled: false,
        };
        drop(turn);
        assert!(control.snapshot().finished);
        assert!(control.steer("late").is_err());
    }

    #[test]
    fn old_abort_does_not_reach_a_later_turn() {
        let (old, guard, _) = live();
        drop(guard);
        let (new, _guard, signal) = live();
        assert!(!old.abort());
        assert!(!signal.is_aborted());
        assert!(new.steer("new turn").is_ok());
    }

    #[test]
    fn reclaim_and_fetch_cannot_return_the_same_input() {
        let (control, _guard, _) = live();
        control.steer("first").unwrap();
        control.follow_up("second").unwrap();
        assert_eq!(control.run.fetch(InputKind::Steering).len(), 1);
        let pending = control.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "second");
        assert!(control.run.fetch(InputKind::FollowUp).is_empty());
    }

    #[test]
    fn ordered_barrier_holds_later_inputs_and_preserves_corrected_prefix() {
        let (control, _guard, _) = live();
        let early_follow_up = control.follow_up("before compaction").unwrap();
        let early_steer = control.steer("earlier steering").unwrap();
        let barrier = control.insert_yield_barrier().unwrap();
        control.follow_up("after compaction").unwrap();
        let late_steer = control.steer("held steering").unwrap();
        let correction = control
            .replace_pending(early_follow_up, "corrected before compaction")
            .unwrap()
            .unwrap();
        assert_ne!(correction, early_follow_up);
        assert!(!control.yield_barrier_ready(barrier));
        assert_eq!(control.retract(late_steer).unwrap().text, "held steering");
        assert!(!control.yield_barrier_ready(barrier));
        assert_eq!(
            control.retract(early_steer).unwrap().text,
            "earlier steering"
        );
        assert!(!control.yield_barrier_ready(barrier));
        assert_eq!(
            control.run.fetch(InputKind::FollowUp)[0].text_for_display(),
            Some("corrected before compaction")
        );
        assert!(control.yield_barrier_ready(barrier));
        assert!(!control.yield_barrier_can_run(barrier));
        assert!(control.run.fetch(InputKind::FollowUp).is_empty());
        assert!(control.run.fetch(InputKind::Steering).is_empty());
        let (before, after) = control.take_pending_at_barrier(barrier).unwrap();
        assert!(before.is_empty());
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].text, "after compaction");
        assert_eq!(control.snapshot().pending_bytes, 0);
        assert!(!control.has_yield_barrier());
    }

    #[test]
    fn barrier_reclamation_restores_exact_order_and_native_content_after_abort() {
        let (control, _guard, _) = live();
        control.follow_up("  before\n ").unwrap();
        let barrier = control.insert_yield_barrier().unwrap();
        let content = UserContent::Blocks(vec![
            crate::model::ContentBlock::Image(crate::model::ImageContent {
                data: "YQ==".to_string(),
                mime_type: "image/png".to_string(),
            }),
            crate::model::ContentBlock::Text(crate::model::TextContent::new("expanded")),
        ]);
        control
            .follow_up_with_content(&content, "  @image.png explain  ")
            .unwrap();
        assert!(control.abort());
        let (before, after) = control.take_pending_at_barrier(barrier).unwrap();
        assert_eq!(before[0].text, "  before\n ");
        assert_eq!(after[0].text, "  @image.png explain  ");
        assert_eq!(
            serde_json::to_value(&after[0].content).unwrap(),
            serde_json::to_value(content).unwrap()
        );
        assert!(!control.yield_barrier_can_run(barrier));
        assert!(control.take_pending_at_barrier(barrier).is_none());
    }

    #[test]
    fn stale_barriers_cannot_release_or_claim_a_newer_operation() {
        let (control, _guard, _) = live();
        let old = control.insert_yield_barrier().unwrap();
        assert!(control.insert_yield_barrier().is_err());
        control.follow_up("held").unwrap();
        assert!(control.remove_yield_barrier(old));
        let current = control.insert_yield_barrier().unwrap();
        assert_ne!(old, current);
        assert!(!control.remove_yield_barrier(old));
        assert!(control.take_pending_at_barrier(old).is_none());
        assert!(control.has_yield_barrier());
        assert_eq!(control.take_pending()[0].text, "held");
        assert!(!control.has_yield_barrier());
        assert!(control.take_pending_at_barrier(current).is_none());
        assert!(!control.yield_barrier_ready(current));
    }

    #[test]
    fn barrier_admission_requires_an_observed_successful_normal_completion() {
        use crate::model::{ContentBlock, StopReason, TextContent};

        for outcome in [
            "stop",
            "error",
            "aborted",
            "length",
            "cap",
            "failed",
            "cancelled",
        ] {
            let (control, guard, _) = live();
            let barrier = control.insert_yield_barrier().unwrap();
            let mut message = AssistantMessage {
                stop_reason: match outcome {
                    "error" => StopReason::Error,
                    "aborted" => StopReason::Aborted,
                    "length" => StopReason::Length,
                    _ => StopReason::Stop,
                },
                ..AssistantMessage::default()
            };
            if outcome == "cap" {
                message.content = vec![ContentBlock::Text(TextContent::new(
                    "[time cap reached] time cap reached after 1s (--max-time); stopping at the turn boundary",
                ))];
            }
            if outcome == "cancelled" {
                assert!(control.abort());
            }
            let future = async move {
                drop(guard);
                if outcome == "failed" {
                    Err(Error::session_persistence("fixture save failure"))
                } else {
                    Ok(message)
                }
            };
            let turn = ControlledTurn {
                future: Box::pin(future),
                control: control.clone(),
                polled: false,
            };
            assert!(!control.yield_barrier_can_run(barrier));
            let _ = futures::executor::block_on(turn);
            assert_eq!(control.yield_barrier_can_run(barrier), outcome == "stop");
            control.invalidate_yield_barrier();
            assert!(!control.yield_barrier_can_run(barrier));
            assert!(control.take_pending_at_barrier(barrier).is_some());
        }

        let (control, guard, _) = live();
        let barrier = control.insert_yield_barrier().unwrap();
        drop(guard);
        assert!(!control.yield_barrier_can_run(barrier));
        assert!(control.insert_yield_barrier().is_err());
    }

    #[test]
    fn debugging_never_prints_queued_text() {
        let (control, _guard, _) = live();
        control.steer("private-secret-text").unwrap();
        assert!(!format!("{control:?}").contains("private-secret-text"));
        assert!(!format!("{:?}", control.take_pending()).contains("private-secret-text"));
    }
}
