//! Turn-wide SDK deadlines that request abort and then drain the native turn.
//!
//! Unlike dropping a timeout's losing future, expiry does not abandon provider,
//! tool, or persistence cleanup. The deadline is absolute in its owner's clock,
//! so waiting before the first poll, follow-ups and retries share one budget.
//! This is cooperative cancellation, not preemption of a blocking syscall.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use asupersync::time::{Sleep, TimerDriverHandle};
use asupersync::types::Time;

use super::{ControlledTurn, SessionControlHandle, control_error};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::AssistantMessage;

/// An absolute, cloneable deadline owned by one runtime clock.
///
/// Construct inside the runtime with `AgentCx::for_current_or_request()`.
/// Reuse a clone to share a budget across several turns; cloning never restarts
/// the clock. An inherited context deadline can only shorten the requested one.
#[derive(Clone)]
pub struct TurnDeadline {
    owner: AgentCx,
    timer: TimerDriverHandle,
    at: Time,
}

impl fmt::Debug for TurnDeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnDeadline")
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl TurnDeadline {
    /// Create a deadline from now, not from the turn's first poll.
    ///
    /// Requires the owner's timer capability and a bound timer driver. A
    /// detached request context is not allowed to silently borrow the polling
    /// task's clock. Zero, excessive and overflowing durations are rejected.
    pub fn after(owner: &AgentCx, timeout: Duration) -> Result<Self> {
        if !owner.capabilities().time {
            return Err(control_error(
                "SESSION_DEADLINE_CAPABILITY",
                "the deadline owner does not permit timers",
            ));
        }
        owner.checkpoint().map_err(|_| {
            control_error("SESSION_DEADLINE_CANCELLED", "the deadline owner is cancelled")
        })?;
        let timer = owner.timer_driver().ok_or_else(|| {
            control_error(
                "SESSION_DEADLINE_TIMER",
                "create a deadline with a context from the running SDK runtime",
            )
        })?;
        Self::from_timer(owner, timer, timeout)
    }

    pub(super) fn from_timer(
        owner: &AgentCx,
        timer: TimerDriverHandle,
        timeout: Duration,
    ) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(86_400) {
            return Err(control_error(
                "SESSION_DEADLINE_RANGE",
                "turn timeout must be positive and at most 24 hours",
            ));
        }
        let nanos = u64::try_from(timeout.as_nanos()).map_err(|_| {
            control_error("SESSION_DEADLINE_RANGE", "turn timeout is not representable")
        })?;
        let at = timer.now().as_nanos().checked_add(nanos).ok_or_else(|| {
            control_error(
                "SESSION_DEADLINE_RANGE",
                "turn deadline would overflow its clock",
            )
        })?;
        let requested = Time::from_nanos(at);
        let at = owner
            .budget()
            .deadline
            .map_or(requested, |inherited| requested.min(inherited));
        Ok(Self {
            owner: owner.clone(),
            timer,
            at,
        })
    }

    #[must_use]
    pub const fn at(&self) -> Time {
        self.at
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        Duration::from_nanos(self.at.as_nanos().saturating_sub(self.timer.now().as_nanos()))
    }

    #[must_use]
    pub fn is_elapsed(&self) -> bool {
        self.timer.now() >= self.at
    }
}

/// Why a deadline-governed turn failed. Native errors retain their exact type.
///
/// The completion inside an interruption is the native turn's result AFTER
/// cooperative cleanup, not permission to replay its input or completed tools.
/// No completion means expiry/cancellation prevented the first native poll:
/// no prompt, provider request or session mutation was dispatched by that turn.
/// A provider may return a partial/aborted message, or even a late success.
/// Inspect this result and the session history before deciding what to resume.
pub enum TurnDeadlineError {
    Elapsed {
        completion: Option<Box<Result<AssistantMessage>>>,
    },
    OwnerCancelled {
        completion: Option<Box<Result<AssistantMessage>>>,
    },
    Turn(Error),
}

impl TurnDeadlineError {
    #[must_use]
    pub const fn is_elapsed(&self) -> bool {
        matches!(self, Self::Elapsed { .. })
    }

    /// The native completion, including typed persistence/provider failures,
    /// after a deadline or owner cancellation requested cooperative abort.
    #[must_use]
    pub fn completion(&self) -> Option<&Result<AssistantMessage>> {
        match self {
            Self::Elapsed { completion } | Self::OwnerCancelled { completion } => {
                completion.as_deref()
            }
            Self::Turn(_) => None,
        }
    }
}

impl fmt::Debug for TurnDeadlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Assistant messages and provider errors can contain private content.
        // Merely logging the deadline result must not print that content.
        match self {
            Self::Elapsed { completion } => f
                .debug_struct("TurnDeadlineElapsed")
                .field("started", &completion.is_some())
                .field(
                    "native_result_is_error",
                    &completion.as_deref().map(Result::is_err),
                )
                .finish_non_exhaustive(),
            Self::OwnerCancelled { completion } => f
                .debug_struct("TurnDeadlineOwnerCancelled")
                .field("started", &completion.is_some())
                .field(
                    "native_result_is_error",
                    &completion.as_deref().map(Result::is_err),
                )
                .finish_non_exhaustive(),
            Self::Turn(_) => f.write_str("TurnDeadlineError::Turn(..)"),
        }
    }
}

impl fmt::Display for TurnDeadlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elapsed { .. } => f.write_str(
                "[SESSION_CONTROL_TIMEOUT] turn deadline elapsed; inspect completion() before replaying work (None means execution never started)",
            ),
            Self::OwnerCancelled { .. } => f.write_str(
                "[SESSION_CONTROL_CANCELLED] deadline owner cancelled; inspect completion() before replaying work (None means execution never started)",
            ),
            Self::Turn(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for TurnDeadlineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Turn(error) => Some(error),
            Self::Elapsed { .. } | Self::OwnerCancelled { .. } => self
                .completion()
                .and_then(|completion| completion.as_ref().err())
                .map(|error| error as &(dyn std::error::Error + 'static)),
        }
    }
}

#[derive(Clone, Copy)]
enum Interruption {
    Elapsed,
    OwnerCancelled,
}

/// A controlled turn with a deadline and its original live control lane.
///
/// Expiry seals input admission and requests abort exactly once. This future
/// then continues polling the SAME native turn until it returns: no detached
/// cleanup task, duplicate provider request, or replacement session is created.
/// Dropping this future still has the ordinary SDK cancellation limitations;
/// await it to observe native completion. Cleanup can outlive the deadline.
#[must_use = "await the turn to drain native cancellation and inspect its outcome"]
pub struct DeadlineTurn<F> {
    turn: Option<Pin<Box<ControlledTurn<F>>>>,
    control: SessionControlHandle,
    deadline: TurnDeadline,
    sleep: Option<Pin<Box<Sleep>>>,
    interruption: Option<Interruption>,
    started: bool,
    finished: bool,
}

impl<F> ControlledTurn<F> {
    /// Apply a turn-wide deadline while retaining steer/follow-up/retraction.
    /// Works on text, attachment-bearing and continuation turns alike.
    pub fn with_deadline(self, deadline: TurnDeadline) -> DeadlineTurn<F> {
        let control = self.control();
        let started = self.polled;
        let sleep = Sleep::with_timer_driver(deadline.at, deadline.timer.clone());
        DeadlineTurn {
            turn: Some(Box::pin(self)),
            control,
            deadline,
            sleep: Some(Box::pin(sleep)),
            interruption: None,
            started,
            finished: false,
        }
    }
}

impl<F> DeadlineTurn<F> {
    #[must_use]
    pub fn control(&self) -> SessionControlHandle {
        self.control.clone()
    }

    fn interrupt(&mut self, reason: Interruption) {
        if self.control.abort() {
            self.interruption = Some(reason);
        }
        self.sleep = None;
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>) {
        // A prior user abort owns cancellation. Its cleanup must not be relabeled
        // as a timeout just because it eventually crosses the former deadline.
        if !self.control.snapshot().accepting_input {
            self.sleep = None;
        }
        if self.sleep.is_none() {
            return;
        }
        if self.deadline.is_elapsed() {
            self.interrupt(Interruption::Elapsed);
            return;
        }
        if self.deadline.owner.checkpoint().is_err() {
            self.interrupt(Interruption::OwnerCancelled);
            return;
        }
        let timer_ready = if let Some(sleep) = &mut self.sleep {
            // Scope authority to the timer poll only. Do not change the native
            // turn's I/O authority, and never retain a TLS guard across Pending.
            let _guard = self.deadline.owner.cx().clone().set_current_restricted();
            sleep.as_mut().poll(cx).is_ready()
        } else {
            false
        };
        // Time/cancellation may have changed while registering the timer.
        if self.deadline.is_elapsed() {
            self.interrupt(Interruption::Elapsed);
        } else if self.deadline.owner.checkpoint().is_err() {
            self.interrupt(Interruption::OwnerCancelled);
        } else if timer_ready {
            // A latched timer firing remains authoritative even if a custom
            // clock is rewound afterwards. Never poll a completed Sleep again.
            self.interrupt(Interruption::Elapsed);
        }
    }
}

impl<F: Future<Output = Result<AssistantMessage>>> Future for DeadlineTurn<F> {
    type Output = std::result::Result<AssistantMessage, TurnDeadlineError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(
            !this.finished,
            "a completed deadline turn cannot be polled again"
        );
        this.poll_deadline(cx);
        if !this.started && this.interruption.is_some() {
            // The native async body has never run, so there is no async work
            // to drain. Dropping it releases its captured TurnGuard without
            // installing a prompt, invoking hooks or starting provider I/O.
            drop(this.turn.take());
            this.finished = true;
            return Poll::Ready(Err(match this.interruption {
                Some(Interruption::Elapsed) => TurnDeadlineError::Elapsed { completion: None },
                _ => TurnDeadlineError::OwnerCancelled { completion: None },
            }));
        }
        let monitored = this.sleep.is_some();
        this.started = true;
        let result = this
            .turn
            .as_mut()
            .expect("unfinished deadline turn retains its native future")
            .as_mut()
            .poll(cx);
        // A single non-preemptible poll can cross the deadline. Never report
        // its late completion as timely success; if it is still pending, abort
        // it now rather than waiting for another external wakeup.
        let interruption = if !monitored {
            None
        } else if this.deadline.is_elapsed() {
            Some(Interruption::Elapsed)
        } else if this.deadline.owner.checkpoint().is_err() {
            Some(Interruption::OwnerCancelled)
        } else {
            None
        };
        if let Some(reason) = interruption {
            if result.is_ready() {
                this.interruption = Some(reason);
                this.sleep = None;
            } else {
                this.interrupt(reason);
                cx.waker().wake_by_ref();
            }
        }
        let Poll::Ready(completion) = result else {
            return Poll::Pending;
        };
        this.finished = true;
        this.sleep = None;
        Poll::Ready(match this.interruption {
            Some(Interruption::Elapsed) => Err(TurnDeadlineError::Elapsed {
                completion: Some(Box::new(completion)),
            }),
            Some(Interruption::OwnerCancelled) => Err(TurnDeadlineError::OwnerCancelled {
                completion: Some(Box::new(completion)),
            }),
            None => completion.map_err(TurnDeadlineError::Turn),
        })
    }
}

#[cfg(test)]
mod tests;
