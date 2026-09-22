//! Deterministic scheduler probes for the production deadline wrapper.
//! These futures model blocked cleanup; they do not replace a Provider or Tool.

use super::*;
use crate::model::{ContentBlock, TextContent};
use crate::session_control::TurnGuard;
use asupersync::time::VirtualClock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Wake, Waker};

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

struct DrainProbe {
    guard: Option<TurnGuard>,
    release: Arc<AtomicBool>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    completion: Option<Result<AssistantMessage>>,
    cross_deadline: Option<Arc<VirtualClock>>,
}

impl Future for DrainProbe {
    type Output = Result<AssistantMessage>;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if let Some(clock) = self.cross_deadline.take() {
            clock.advance_to(Time::from_secs(20));
        }
        if !self.release.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        drop(self.guard.take());
        Poll::Ready(self.completion.take().unwrap())
    }
}

impl Drop for DrainProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn clock_deadline() -> (Arc<VirtualClock>, TurnDeadline) {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
    let deadline =
        TurnDeadline::from_timer(&AgentCx::for_request(), timer, Duration::from_secs(10)).unwrap();
    (clock, deadline)
}

fn turn(completion: Result<AssistantMessage>) -> ControlledTurn<DrainProbe> {
    let (control, guard, _) = crate::session_control::tests::live();
    ControlledTurn {
        control,
        polled: false,
        future: Box::pin(DrainProbe {
            guard: Some(guard),
            release: Arc::new(AtomicBool::new(false)),
            polls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            completion: Some(completion),
            cross_deadline: None,
        }),
    }
}

fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn deadline_is_absolute_and_clones_share_remaining_time() {
    let (clock, deadline) = clock_deadline();
    let shared = deadline.clone();
    clock.advance_to(Time::from_secs(7));
    assert_eq!(deadline.remaining(), Duration::from_secs(3));
    assert_eq!(shared.at(), deadline.at());
    assert_eq!(shared.remaining(), deadline.remaining());
    clock.advance_to(Time::from_secs(10));
    assert!(shared.is_elapsed());
    assert_eq!(shared.remaining(), Duration::ZERO);
}

#[test]
fn inherited_deadline_is_a_ceiling_not_a_fresh_budget() {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(clock);
    let owner = AgentCx::for_request_with_budget(
        asupersync::Budget::new().with_deadline(Time::from_secs(2)),
    );
    let deadline = TurnDeadline::from_timer(&owner, timer, Duration::from_secs(10)).unwrap();
    assert_eq!(deadline.at(), Time::from_secs(2));
}

#[test]
fn invalid_durations_and_clock_overflow_are_rejected() {
    let (clock, deadline) = clock_deadline();
    for duration in [Duration::ZERO, Duration::from_secs(86_401), Duration::MAX] {
        assert!(
            TurnDeadline::from_timer(&deadline.owner, deadline.timer.clone(), duration).is_err()
        );
    }
    clock.advance_to(Time::from_nanos(u64::MAX - 1));
    assert!(
        TurnDeadline::from_timer(&deadline.owner, deadline.timer, Duration::from_nanos(2)).is_err()
    );
}

#[test]
fn public_constructor_does_not_borrow_ambient_timer_authority() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        assert!(TurnDeadline::after(&owner, Duration::from_secs(1)).is_ok());
        let restricted = {
            let _guard = owner
                .cx()
                .clone()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        let error = TurnDeadline::after(&restricted, Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("SESSION_DEADLINE_CAPABILITY"));
    });
}

#[test]
fn expiry_wakes_a_dormant_turn_then_waits_for_native_cleanup() {
    let (clock, deadline) = clock_deadline();
    let timer = deadline.timer.clone();
    let native = turn(Err(Error::session("typed native cleanup failure")));
    let release = Arc::clone(&native.future.release);
    let drops = Arc::clone(&native.future.drops);
    let control = native.control();
    let id = control.follow_up("unclaimed-private-input").unwrap();
    let mut limited = native.with_deadline(deadline);
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut limited).poll(&mut cx).is_pending());
    clock.advance_to(Time::from_secs(10));
    let _ = timer.process_timers();
    assert!(
        wakes.0.load(Ordering::SeqCst) > 0,
        "deadline must wake a silent provider"
    );
    assert!(Pin::new(&mut limited).poll(&mut cx).is_pending());
    assert!(!control.snapshot().accepting_input);
    assert!(!control.snapshot().finished, "cleanup has not returned yet");
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "never drop the losing native future"
    );
    assert!(control.steer("too late").is_err());
    assert_eq!(control.take_pending()[0].id, id);
    release.store(true, Ordering::SeqCst);
    let Poll::Ready(Err(error)) = poll(&mut limited) else {
        panic!("timeout after cleanup")
    };
    assert!(error.is_elapsed());
    assert!(matches!(error.completion(), Some(Err(Error::Session(_)))));
    assert!(std::error::Error::source(&error).is_some());
    assert!(control.snapshot().finished);
    drop(limited);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn late_native_success_is_preserved_but_is_not_accepted_as_timely_success() {
    let (clock, deadline) = clock_deadline();
    let native = turn(Ok(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new("private-result"))],
        ..AssistantMessage::default()
    }));
    let release = Arc::clone(&native.future.release);
    let mut limited = native.with_deadline(deadline);
    assert!(poll(&mut limited).is_pending());
    clock.advance_to(Time::from_secs(11));
    assert!(poll(&mut limited).is_pending());
    release.store(true, Ordering::SeqCst);
    let Poll::Ready(Err(error)) = poll(&mut limited) else {
        panic!("timeout")
    };
    assert!(error.is_elapsed());
    assert!(matches!(error.completion(), Some(Ok(_))));
    assert!(!format!("{error:?}").contains("private-result"));
}

#[test]
fn synchronous_poll_crossing_the_deadline_cannot_report_timely_success() {
    let (clock, deadline) = clock_deadline();
    let mut native = turn(Ok(AssistantMessage::default()));
    native.future.release.store(true, Ordering::SeqCst);
    native.future.cross_deadline = Some(clock);
    let Poll::Ready(Err(error)) = poll(&mut native.with_deadline(deadline)) else {
        panic!("late completion must be a deadline error")
    };
    assert!(error.is_elapsed());
    assert!(matches!(error.completion(), Some(Ok(_))));
}

#[test]
fn prior_explicit_abort_is_not_reclassified_while_cleanup_drains() {
    let (clock, deadline) = clock_deadline();
    let native = turn(Err(Error::session("native abort")));
    let release = Arc::clone(&native.future.release);
    let control = native.control();
    let mut limited = native.with_deadline(deadline);
    assert!(poll(&mut limited).is_pending());
    assert!(control.abort());
    clock.advance_to(Time::from_secs(20));
    release.store(true, Ordering::SeqCst);
    assert!(matches!(
        poll(&mut limited),
        Poll::Ready(Err(TurnDeadlineError::Turn(_)))
    ));
}

#[test]
fn owner_cancellation_drains_without_claiming_deadline_expiry() {
    let (_, deadline) = clock_deadline();
    let owner = deadline.owner.clone();
    let native = turn(Err(Error::session("native cancellation")));
    let release = Arc::clone(&native.future.release);
    let mut limited = native.with_deadline(deadline);
    assert!(poll(&mut limited).is_pending());
    owner.cancel_with(asupersync::types::CancelKind::User, Some("test"));
    assert!(poll(&mut limited).is_pending());
    release.store(true, Ordering::SeqCst);
    assert!(matches!(
        poll(&mut limited),
        Poll::Ready(Err(TurnDeadlineError::OwnerCancelled { .. }))
    ));
}

#[test]
fn normal_completion_preserves_native_success_and_typed_error() {
    for completion in [
        Ok(AssistantMessage::default()),
        Err(Error::session("expected")),
    ] {
        let (_, deadline) = clock_deadline();
        let expected_error = completion.is_err();
        let native = turn(completion);
        native.future.release.store(true, Ordering::SeqCst);
        let control = native.control();
        let Poll::Ready(result) = poll(&mut native.with_deadline(deadline)) else {
            panic!("ready")
        };
        assert_eq!(result.is_err(), expected_error);
        if let Err(error) = result {
            assert!(matches!(error, TurnDeadlineError::Turn(Error::Session(_))));
        }
        assert!(control.snapshot().finished);
    }
}

#[test]
fn unpolled_drop_retires_control_without_consuming_queued_input() {
    let (_, deadline) = clock_deadline();
    let native = turn(Ok(AssistantMessage::default()));
    let polls = Arc::clone(&native.future.polls);
    let control = native.control();
    control.steer("recover on drop").unwrap();
    drop(native.with_deadline(deadline));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(control.snapshot().finished);
    assert_eq!(control.take_pending()[0].text, "recover on drop");
}

#[test]
fn expired_unpolled_turn_is_retired_without_dispatch_or_fake_completion() {
    let (clock, deadline) = clock_deadline();
    let native = turn(Ok(AssistantMessage::default()));
    let polls = Arc::clone(&native.future.polls);
    let drops = Arc::clone(&native.future.drops);
    let control = native.control();
    control.steer("never dispatched").unwrap();
    clock.advance_to(Time::from_secs(10));
    let Poll::Ready(Err(error)) = poll(&mut native.with_deadline(deadline)) else {
        panic!("expired before execution")
    };
    assert!(error.is_elapsed());
    assert!(error.completion().is_none());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(control.snapshot().finished);
    assert_eq!(control.take_pending()[0].text, "never dispatched");
}

#[test]
fn wrapping_an_already_polled_turn_still_drains_its_native_cleanup() {
    let (clock, deadline) = clock_deadline();
    let mut native = turn(Err(Error::session("actual cleanup")));
    let release = Arc::clone(&native.future.release);
    let drops = Arc::clone(&native.future.drops);
    assert!(poll(&mut native).is_pending());
    clock.advance_to(Time::from_secs(10));
    let mut limited = native.with_deadline(deadline);
    assert!(poll(&mut limited).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    release.store(true, Ordering::SeqCst);
    let Poll::Ready(Err(error)) = poll(&mut limited) else {
        panic!("drained expired turn")
    };
    assert!(error.is_elapsed());
    assert!(matches!(error.completion(), Some(Err(Error::Session(_)))));
}

#[test]
fn cancelled_unpolled_turn_never_starts_execution() {
    let (_, deadline) = clock_deadline();
    deadline
        .owner
        .cancel_with(asupersync::types::CancelKind::User, Some("before dispatch"));
    let native = turn(Ok(AssistantMessage::default()));
    let polls = Arc::clone(&native.future.polls);
    let control = native.control();
    let Poll::Ready(Err(TurnDeadlineError::OwnerCancelled { completion })) =
        poll(&mut native.with_deadline(deadline))
    else {
        panic!("unstarted cancellation")
    };
    assert!(completion.is_none());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(control.snapshot().finished);
}

#[test]
fn a_fired_timer_is_not_polled_again_after_a_test_clock_rewind() {
    let (clock, deadline) = clock_deadline();
    let timer = deadline.timer.clone();
    let native = turn(Err(Error::session("drained")));
    let release = Arc::clone(&native.future.release);
    let mut limited = native.with_deadline(deadline);
    assert!(poll(&mut limited).is_pending());
    clock.advance_to(Time::from_secs(10));
    let _ = timer.process_timers();
    clock.set(Time::ZERO);
    assert!(poll(&mut limited).is_pending());
    release.store(true, Ordering::SeqCst);
    let Poll::Ready(Err(error)) = poll(&mut limited) else {
        panic!("latched timeout")
    };
    assert!(error.is_elapsed());
}
