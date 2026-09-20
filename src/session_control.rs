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

mod attachments;
#[cfg(test)]
mod agent_tests;

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
}

struct Run {
    data: Mutex<RunData>,
    abort: AbortHandle,
}

type ActiveRun = Arc<Mutex<Weak<Run>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let Some(index) = data.pending.iter().position(|input| input.kind == kind) else {
            return Vec::new();
        };
        let Some(input) = data.pending.remove(index) else {
            return Vec::new();
        };
        data.pending_bytes -= input.bytes;
        data.handed_to_agent = data.handed_to_agent.saturating_add(1);
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
        }
        self.run.abort.abort();
        true
    }

    /// Reclaim all still-unclaimed inputs in their original admission order.
    /// This is linearized against fetches: an item is either returned here OR
    /// handed to the agent, never both. It may also be used to retract input
    /// while a turn is live. Already handed-off input cannot be retracted.
    pub fn take_pending(&self) -> Vec<PendingInput> {
        let mut data = lock(&self.run.data);
        data.pending_bytes = 0;
        data.pending.drain(..).collect()
    }

    /// Retract exactly one unclaimed input. `None` means it was already
    /// handed off, reclaimed, or never admitted by this turn's handle.
    pub fn retract(&self, id: InputId) -> Option<PendingInput> {
        let mut data = lock(&self.run.data);
        let index = data.pending.iter().position(|input| input.id == id)?;
        let input = data.pending.remove(index)?;
        data.pending_bytes -= input.bytes;
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
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// In-process SDK driver with live input and cancellation. The two additive
/// fetchers are installed once, not once per prompt. Idle session management
/// stays available through `session_mut`; Rust prevents access during a turn.
pub struct ControllableSession {
    session: AgentSessionHandle,
    active: ActiveRun,
}

impl AgentSessionHandle {
    /// Add live, bounded, turn-scoped control without changing providers,
    /// listeners, retry/failover policy, tools, or session persistence.
    #[must_use]
    pub fn into_controllable(mut self) -> ControllableSession {
        let active = Arc::new(Mutex::new(Weak::new()));
        self.session_mut().agent.register_message_fetchers(
            Some(fetcher(&active, InputKind::Steering)),
            Some(fetcher(&active, InputKind::FollowUp)),
        );
        ControllableSession {
            session: self,
            active,
        }
    }
}

impl ControllableSession {
    pub fn session_mut(&mut self) -> &mut AgentSessionHandle {
        &mut self.session
    }

    fn begin(&self) -> Result<(TurnGuard, AbortSignal)> {
        let mut active = lock(&self.active);
        if active.upgrade().is_some() {
            return Err(control_error(
                "SESSION_CONTROL_BUSY",
                "a controlled turn is still active",
            ));
        }
        let (run, signal) = Run::new();
        *active = Arc::downgrade(&run);
        Ok((
            TurnGuard {
                active: Arc::clone(&self.active),
                run,
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
            session
                .prompt_with_abort(input, signal, move |event| {
                    on_event(&callback_control, event);
                })
                .await
        };
        Ok(ControlledTurn {
            future: Box::pin(future),
            control,
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
            future: Box::pin(future),
            control,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn live() -> (SessionControlHandle, TurnGuard, AbortSignal) {
        let (run, signal) = Run::new();
        let active = Arc::new(Mutex::new(Arc::downgrade(&run)));
        (SessionControlHandle { run: Arc::clone(&run) }, TurnGuard { active, run }, signal)
    }

    #[test]
    fn queues_preserve_kind_order_and_exact_authored_text() {
        let (control, _guard, _) = live();
        control.follow_up("next").unwrap();
        control.steer("  change course\n日本語  ").unwrap();
        control.steer("then inspect").unwrap();
        let first = control.run.fetch(InputKind::Steering);
        assert_eq!(first[0].keyword_scan_source(), Some("  change course\n日本語  "));
        assert_eq!(control.run.fetch(InputKind::Steering)[0].text_for_display(), Some("then inspect"));
        assert_eq!(control.run.fetch(InputKind::FollowUp)[0].text_for_display(), Some("next"));
        assert_eq!(control.snapshot().handed_to_agent, 3);
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn full_queue_rejects_new_input_without_losing_oldest() {
        let (control, _guard, _) = live();
        for index in 0..MAX_PENDING_INPUTS {
            control.steer(&index.to_string()).unwrap();
        }
        assert!(control.follow_up("overflow").unwrap_err().to_string().contains("SESSION_CONTROL_FULL"));
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
        let turn = ControlledTurn { future: Box::pin(future), control: control.clone() };
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
    fn debugging_never_prints_queued_text() {
        let (control, _guard, _) = live();
        control.steer("private-secret-text").unwrap();
        assert!(!format!("{control:?}").contains("private-secret-text"));
        assert!(!format!("{:?}", control.take_pending()).contains("private-secret-text"));
    }
}
