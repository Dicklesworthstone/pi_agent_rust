//! Await host evidence under the goal run's original clock and cancellation.
//! No executor task or polling loop is spawned. The host owns cancellation-safe
//! resources inside its verifier; abandoning it cannot undo completed effects.

use super::{
    GoalControl, GoalDecision, GoalRunReport, GoalRunStop, GoalSnapshot, interruption,
    pause_decision, successful,
};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::model::AssistantMessage;
use crate::session_control::{TurnDeadline, TurnDeadlineError};
use asupersync::time::{TimerDriverHandle, TimerHandle};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Wake, Waker};

type CancelWait = Pin<Box<dyn Future<Output = ()> + Send>>;

fn cancellation(owner: AgentCx) -> CancelWait {
    Box::pin(async move {
        // The sender remains live without sending: only owner cancellation
        // completes this receive. Dropping the wait unregisters its waiter.
        let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        let _ = receiver.recv(owner.cx()).await;
        drop(sender);
    })
}

struct DeadlineWaker {
    target: Waker,
    fired: Arc<AtomicBool>,
    deadline: Weak<TurnDeadline>,
}

impl Wake for DeadlineWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let Some(deadline) = self.deadline.upgrade() else {
            return;
        };
        // A timer can wake early. Only observed expiry latches; the next
        // poll rearms an early wake. A weak reference avoids a clock cycle.
        let elapsed = deadline.is_elapsed();
        drop(deadline);
        if elapsed {
            self.fired.store(true, Ordering::SeqCst);
        }
        self.target.wake_by_ref();
    }
}

struct VerificationWait<F> {
    owner: AgentCx,
    control: GoalControl,
    deadline: Arc<TurnDeadline>,
    timer: TimerDriverHandle,
    timer_handle: Option<TimerHandle>,
    timer_fired: Arc<AtomicBool>,
    cancellation: Option<CancelWait>,
    future: Option<Pin<Box<F>>>,
}

impl<F> VerificationWait<F> {
    fn new(
        owner: &AgentCx,
        deadline: &TurnDeadline,
        control: &GoalControl,
        future: F,
    ) -> Result<Self> {
        let timer = owner.timer_driver().ok_or_else(|| {
            super::failure("GOAL_VERIFIER_TIMER", "goal verification requires its owner's timer")
        })?;
        Ok(Self {
            owner: owner.clone(),
            control: control.clone(),
            deadline: Arc::new(deadline.clone()),
            timer,
            timer_handle: None,
            timer_fired: Arc::new(AtomicBool::new(false)),
            cancellation: Some(cancellation(owner.clone())),
            future: Some(Box::pin(future)),
        })
    }

    fn stop(&self) -> Option<GoalRunStop> {
        interruption(&self.owner, &self.deadline, &self.control).or_else(|| {
            self.timer_fired.load(Ordering::SeqCst).then_some(GoalRunStop::Deadline)
        })
    }

    fn retire(&mut self) {
        // Remove all registrations, even on early return or an unpolled drop.
        // Never drop a task waker while holding the control mutex.
        let waker = self.control.lock().verification_waker.take();
        drop(waker);
        if let Some(handle) = self.timer_handle.take() {
            let _ = self.timer.cancel(&handle);
        }
        drop(self.cancellation.take());
        drop(self.future.take());
    }
}

impl<F: Future<Output = Result<GoalDecision>>> Future for VerificationWait<F> {
    type Output = std::result::Result<Result<GoalDecision>, GoalRunStop>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let owner = this.owner.clone();
        let _current = owner.cx().clone().set_current_restricted();

        // Register before checking pause, closing the check/subscribe race.
        // Only one verifier exists per exclusively borrowed goal run.
        let registered = cx.waker().clone();
        let old = this.control.lock().verification_waker.replace(registered);
        drop(old);
        let cancelled = this.cancellation.as_mut()
            .is_some_and(|wait| wait.as_mut().poll(cx).is_ready());
        if let Some(stop) = cancelled.then_some(GoalRunStop::Cancelled).or_else(|| this.stop()) {
            this.retire();
            return Poll::Ready(Err(stop));
        }

        if let Some(handle) = this.timer_handle.take() {
            // A fired early timer may have consumed the old handle. Always
            // register again, rather than updating a no-longer-live timer.
            let _ = this.timer.cancel(&handle);
        }
        let waker = Waker::from(Arc::new(DeadlineWaker {
            target: cx.waker().clone(),
            fired: Arc::clone(&this.timer_fired),
            deadline: Arc::downgrade(&this.deadline),
        }));
        this.timer_handle = Some(this.timer.register(this.deadline.at(), waker));
        if let Some(stop) = this.stop() {
            this.retire();
            return Poll::Ready(Err(stop));
        }

        let outcome = this.future.as_mut().expect("unfinished verifier").as_mut().poll(cx);
        // A non-preemptible poll or callback can pause or cross the deadline.
        // Even a Ready(Complete) must lose to that observed interruption.
        if let Some(stop) = this.stop() {
            this.retire();
            return Poll::Ready(Err(stop));
        }
        if outcome.is_ready() {
            this.retire();
        }
        outcome.map(Ok)
    }
}

impl<F> Drop for VerificationWait<F> {
    fn drop(&mut self) {
        // A run can be dropped by a different executor task. Host-resource
        // destructors still execute with the initiating authority installed.
        let _current = self.owner.cx().clone().set_current_restricted();
        self.retire();
    }
}

pub(super) async fn decide_next<F>(
    report: &mut GoalRunReport,
    owner: &AgentCx,
    deadline: &TurnDeadline,
    control: &GoalControl,
    verify: &mut impl FnMut(GoalSnapshot, AssistantMessage) -> F,
) -> (GoalDecision, Option<GoalRunStop>)
where
    F: Future<Output = Result<GoalDecision>> + Send,
{
    if let Some(stop) = interruption(owner, deadline, control) {
        return (pause_decision("run interrupted; inspect native completion before resuming"), Some(stop));
    }
    let Some(Ok(message)) = report.completion.as_ref() else {
        let stop = match report.completion.as_ref() {
            Some(Err(TurnDeadlineError::Elapsed { .. })) => GoalRunStop::Deadline,
            Some(Err(TurnDeadlineError::OwnerCancelled { .. })) => GoalRunStop::Cancelled,
            _ => GoalRunStop::NativeFailure,
        };
        return (pause_decision("native prompt failed; inspect completion before resuming"), Some(stop));
    };
    if crate::agent::time_cap_marker(message).is_some() {
        // A native cap can return a successful synthetic boundary message.
        // Starting another goal prompt would reset the cap and resume work.
        return (pause_decision("native run-time cap reached; explicit review required"), Some(GoalRunStop::Paused));
    }
    if !successful(message) {
        return (pause_decision("native prompt did not produce a successful final answer"), Some(GoalRunStop::NativeFailure));
    }
    if !report.pending.is_empty() {
        return (pause_decision("unclaimed user input returned to host"), Some(GoalRunStop::Paused));
    }

    // Own only the last successful response, never a clone of the whole
    // session. The callback cannot borrow the exclusively running session.
    let future = verify(report.goal.clone(), message.clone());
    let outcome = match VerificationWait::new(owner, deadline, control, future) {
        Ok(wait) => wait.await,
        Err(error) => Ok(Err(error)),
    };
    let decision = match outcome {
        Err(stop) => return (pause_decision("run interrupted during host verification"), Some(stop)),
        Ok(Err(error)) => {
            report.error = Some(error);
            return (pause_decision("host verification failed; inspect the run report"), Some(GoalRunStop::Failed));
        }
        Ok(Ok(decision)) => decision,
    };
    if let Some(stop) = interruption(owner, deadline, control) {
        return (pause_decision("run interrupted after host verification"), Some(stop));
    }
    let (text, stop) = match &decision {
        GoalDecision::Continue { instruction } => (instruction, None),
        GoalDecision::Pause { reason } => (reason, Some(GoalRunStop::Paused)),
        GoalDecision::Complete { evidence } => (evidence, Some(GoalRunStop::Complete)),
    };
    if let Err(error) = super::validate_text(text, "host verdict") {
        report.error = Some(error);
        return (pause_decision("host verdict was empty or oversized"), Some(GoalRunStop::Failed));
    }
    (decision, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Dropped(Arc<AtomicUsize>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn run<F: Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build().expect("runtime").block_on(future)
    }

    // Scheduler probes use the production wait, real timer driver and owner
    // cancellation. A pending verifier deliberately supplies no wake of its own.
    #[test]
    fn pause_wakes_the_latest_waiter_and_retires_all_registrations() {
        run(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
            let control = GoalControl::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let guard = Dropped(Arc::clone(&drops));
            let future = async move {
                let _guard = guard;
                std::future::pending::<Result<GoalDecision>>().await
            };
            let timer = owner.timer_driver().unwrap();
            let before = timer.pending_count();
            let mut wait = Box::pin(VerificationWait::new(&owner, &deadline, &control, future).unwrap());
            let old = Arc::new(WakeCount::default());
            let new = Arc::new(WakeCount::default());
            let old_waker = Waker::from(Arc::clone(&old));
            let new_waker = Waker::from(Arc::clone(&new));
            assert!(wait.as_mut().poll(&mut Context::from_waker(&old_waker)).is_pending());
            assert!(wait.as_mut().poll(&mut Context::from_waker(&new_waker)).is_pending());
            let old_before = old.0.load(Ordering::SeqCst);
            let new_before = new.0.load(Ordering::SeqCst);
            assert!(control.pause());
            assert!(new.0.load(Ordering::SeqCst) > new_before);
            assert_eq!(old.0.load(Ordering::SeqCst), old_before);
            assert!(matches!(wait.as_mut().poll(&mut Context::from_waker(&new_waker)),
                Poll::Ready(Err(GoalRunStop::Paused))));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(control.lock().verification_waker.is_none());
            assert_eq!(timer.pending_count(), before);
            assert!(!control.pause());
        });
    }

    #[test]
    fn owner_cancellation_wakes_a_silent_verifier_without_waiting_for_timeout() {
        run(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
            let control = GoalControl::new();
            let mut wait = Box::pin(VerificationWait::new(&owner, &deadline, &control,
                std::future::pending::<Result<GoalDecision>>()).unwrap());
            let wakes = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&wakes));
            assert!(wait.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
            let before = wakes.0.load(Ordering::SeqCst);
            owner.cancel_with(asupersync::types::CancelKind::User, Some("stop verification"));
            assert!(wakes.0.load(Ordering::SeqCst) > before);
            assert!(matches!(wait.as_mut().poll(&mut Context::from_waker(&waker)),
                Poll::Ready(Err(GoalRunStop::Cancelled))));
            assert!(wait.cancellation.is_none());
            assert!(wait.timer_handle.is_none());
            assert!(control.lock().verification_waker.is_none());
        });
    }

    #[test]
    fn dropping_a_verifier_unregisters_its_pause_and_cancellation_wakers() {
        run(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
            let control = GoalControl::new();
            let mut wait = Box::pin(VerificationWait::new(&owner, &deadline, &control,
                std::future::pending::<Result<GoalDecision>>()).unwrap());
            let wakes = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&wakes));
            assert!(wait.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
            drop(wait);
            let before = wakes.0.load(Ordering::SeqCst);
            assert!(control.pause());
            owner.cancel_with(asupersync::types::CancelKind::User, Some("after verifier drop"));
            assert_eq!(wakes.0.load(Ordering::SeqCst), before);
            assert!(control.lock().verification_waker.is_none());
        });
    }

    #[test]
    fn verifier_destructors_keep_their_initiating_authority() {
        struct ObserveTime(Arc<AtomicBool>);
        impl Drop for ObserveTime {
            fn drop(&mut self) {
                self.0.store(AgentCx::for_current_or_request().capabilities().time, Ordering::SeqCst);
            }
        }
        run(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
            let seen = Arc::new(AtomicBool::new(false));
            let guard = ObserveTime(Arc::clone(&seen));
            let future = async move {
                let _guard = guard;
                std::future::pending::<Result<GoalDecision>>().await
            };
            let wait = VerificationWait::new(&owner, &deadline, &GoalControl::new(), future).unwrap();
            let _restricted = owner.cx().clone()
                .restrict::<asupersync::cx::cap::None>().set_current_restricted();
            assert!(!AgentCx::for_current_or_request().capabilities().time);
            drop(wait);
            assert!(seen.load(Ordering::SeqCst), "drop must install the verifier's original owner");
            assert!(!AgentCx::for_current_or_request().capabilities().time,
                "the caller's restriction must be restored after drop");
        });
    }
}
