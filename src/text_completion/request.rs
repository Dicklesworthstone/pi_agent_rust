//! Owner-scoped deadlines for inline auxiliary provider requests.
//!
//! This boundary owns no detached tasks. Its loser is dropped on timeout or
//! cancellation; foreign futures that block inside poll remain cooperative,
//! not hard real-time operations.

use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

use crate::agent_cx::AgentCx;

const CANCELLATION_POLL: Duration = Duration::from_millis(25);

/// Keep owner cancellation separate from provider failure and deadline expiry.
/// In particular, cancelling a review must not disable a healthy advisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStop {
    TimedOut,
    Cancelled,
    TimeUnavailable,
}

fn now(owner: &AgentCx) -> asupersync::types::Time {
    owner
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now())
}

/// Capture the owner on first poll, then retain its cancellation state,
/// capabilities and clock across every subsequent poll. Both request setup
/// and stream consumption share one deadline; progress never resets it.
///
/// An owner checkpoint runs before admitting work and after polling it, so
/// cancellation racing a completed response wins before publication. The
/// timer-backed checkpoint tick also covers idle, cancellation-blind futures.
/// No thread-local owner guard survives a Pending return to the executor.
///
/// Missing time authority, an expired deadline and owner cancellation all
/// refuse provider admission. They are distinct outcomes, not provider errors.
pub async fn with_timeout<F>(
    timeout: Duration,
    future: F,
) -> std::result::Result<F::Output, RequestStop>
where
    F: Future,
{
    with_timeout_and_abort(timeout, None, future).await
}

/// Inline requests initiated by an agent turn also observe its explicit abort
/// signal. Interactive cancellation uses that signal without cancelling the
/// executor context, which must remain alive to persist and finish the turn.
pub(crate) async fn with_timeout_and_abort<F>(
    timeout: Duration,
    abort: Option<&crate::agent::AbortSignal>,
    future: F,
) -> std::result::Result<F::Output, RequestStop>
where
    F: Future,
{
    let owner = AgentCx::for_current_or_request();
    let cancelled =
        || abort.is_some_and(crate::agent::AbortSignal::is_aborted) || owner.checkpoint().is_err();
    if cancelled() {
        return Err(RequestStop::Cancelled);
    }
    if timeout.is_zero() {
        return Err(RequestStop::TimedOut);
    }
    if !owner.capabilities().time {
        return Err(RequestStop::TimeUnavailable);
    }

    owner
        .with_current(async {
            let started = now(&owner);
            let mut deadline = std::pin::pin!(asupersync::time::sleep(started, timeout));
            let mut tick = std::pin::pin!(asupersync::time::sleep(
                started,
                CANCELLATION_POLL.min(timeout),
            ));
            let mut future = std::pin::pin!(future);
            poll_fn(|task_cx| {
                if cancelled() {
                    return Poll::Ready(Err(RequestStop::Cancelled));
                }
                if deadline.as_mut().poll(task_cx).is_ready() {
                    return Poll::Ready(Err(RequestStop::TimedOut));
                }
                if tick.as_mut().poll(task_cx).is_ready() {
                    tick.set(asupersync::time::sleep(now(&owner), CANCELLATION_POLL));
                    // A newly constructed Sleep is not registered until polled.
                    // Without this poll an idle provider could lose its wakeup.
                    let _ = tick.as_mut().poll(task_cx);
                }

                let output = future.as_mut().poll(task_cx);
                if cancelled() {
                    return Poll::Ready(Err(RequestStop::Cancelled));
                }
                if deadline.as_mut().poll(task_cx).is_ready() {
                    return Poll::Ready(Err(RequestStop::TimedOut));
                }
                output.map(Ok)
            })
            .await
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Budget, Cx};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct OnDrop(Arc<AtomicBool>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn explicit_abort_refuses_provider_admission_without_cancelling_the_owner() {
        let owner = Cx::for_request();
        let (handle, signal) = crate::agent::AbortHandle::new();
        handle.abort();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        let polled = AtomicBool::new(false);
        let mut request = std::pin::pin!(with_timeout_and_abort(
            Duration::from_secs(30),
            Some(&signal),
            async {
                let _guard = guard;
                polled.store(true, Ordering::SeqCst);
            },
        ));
        let _owner_guard = owner.clone().set_current_restricted();
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            // ubs:ignore[rust.panic.assert-macros] -- The aborted request must finish without provider admission.
            request.as_mut().poll(&mut cx),
            Poll::Ready(Err(RequestStop::Cancelled))
        ));
        assert!(!polled.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- Provider future must remain untouched.
        assert!(dropped.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- The refused request releases its resources.
        assert!(!owner.is_cancel_requested()); // ubs:ignore[rust.panic.assert-macros] -- Turn persistence retains its live executor context.
    }

    #[test]
    fn explicit_abort_racing_a_ready_response_discards_it() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- Test runtime is required for the deadline boundary.
        let (handle, signal) = crate::agent::AbortHandle::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let result = runtime.block_on(with_timeout_and_abort(
            Duration::from_secs(30),
            Some(&signal),
            async {
                handle.abort();
                OnDrop(Arc::clone(&dropped))
            },
        ));
        assert!(matches!(result, Err(RequestStop::Cancelled))); // ubs:ignore[rust.panic.assert-macros] -- Cancellation wins publication of a response completed in the same poll.
        assert!(dropped.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- Discarded successful output releases its resources.
    }

    #[test]
    fn explicit_abort_wakes_and_drops_an_idle_cancellation_blind_request() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- The real timer wakes an otherwise idle request.
        let (handle, signal) = crate::agent::AbortHandle::new();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let cancel_thread = std::thread::spawn(move || {
            started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request admitted"); // ubs:ignore[rust.panic.expect] -- Provider setup must run before this cancellation probe.
            handle.abort();
        });
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        let result = runtime.block_on(with_timeout_and_abort(
            Duration::from_secs(5),
            Some(&signal),
            async move {
                let _guard = guard;
                started_tx.send(()).expect("signal provider admission"); // ubs:ignore[rust.panic.expect] -- The cancellation thread remains alive until admission.
                std::future::pending::<()>().await;
            },
        ));
        cancel_thread.join().expect("cancellation thread"); // ubs:ignore[rust.panic.expect] -- Propagate failures from the test thread.
        assert_eq!(result, Err(RequestStop::Cancelled)); // ubs:ignore[rust.panic.assert-macros] -- The explicit signal must win before the independent deadline.
        assert!(dropped.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- No provider future survives cancellation.
    }

    #[test]
    fn cancelled_owner_refuses_first_poll_and_drops_captured_request() {
        let owner = Cx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("test cancel"));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        let polled = AtomicBool::new(false);
        let mut request = std::pin::pin!(with_timeout(Duration::from_secs(30), async {
            let _guard = guard;
            polled.store(true, Ordering::SeqCst);
        }));
        let _owner_guard = owner.set_current_restricted();
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            request.as_mut().poll(&mut cx),
            Poll::Ready(Err(RequestStop::Cancelled))
        ));
        assert!(!polled.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn timerless_owner_refuses_without_polling_or_panicking_in_sleep() {
        let owner = Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _owner_guard = owner.set_current_restricted();
        let polled = AtomicBool::new(false);
        let mut request = std::pin::pin!(with_timeout(Duration::from_secs(30), async {
            polled.store(true, Ordering::SeqCst);
        }));
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            request.as_mut().poll(&mut cx),
            Poll::Ready(Err(RequestStop::TimeUnavailable))
        ));
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[test]
    fn later_polls_retain_owner_and_restore_the_pollers_context() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = runtime.request_cx_with_budget(Budget::new().with_poll_quota(100));
        runtime.block_on(async {
            let poller = Cx::current().unwrap();
            let polls = AtomicUsize::new(0);
            let operation = poll_fn(|_| {
                let current = Cx::current().unwrap();
                assert_eq!(current.budget(), owner.budget());
                assert_eq!(current.capabilities(), owner.capabilities());
                if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Poll::Pending
                } else {
                    assert!(!current.is_cancel_requested());
                    Poll::Ready(42)
                }
            });
            let mut request = std::pin::pin!(with_timeout(Duration::from_secs(30), operation));
            let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            {
                let _guard = owner.clone().set_current_restricted();
                assert!(request.as_mut().poll(&mut cx).is_pending());
            }
            assert_eq!(Cx::current().unwrap().budget(), poller.budget());
            let unrelated = Cx::for_request();
            unrelated.cancel_with(asupersync::types::CancelKind::User, Some("unrelated"));
            {
                let _guard = unrelated.set_current_restricted();
                assert!(matches!(
                    request.as_mut().poll(&mut cx),
                    Poll::Ready(Ok(42))
                ));
                assert!(Cx::current().unwrap().is_cancel_requested());
            }
            assert!(!poller.is_cancel_requested());
            assert_eq!(Cx::current().unwrap().budget(), poller.budget());
        });
    }

    #[test]
    fn owner_cancel_between_polls_drops_request_without_polling_it_again() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = runtime.request_cx_with_budget(Budget::new());
        runtime.block_on(async {
            let polls = AtomicUsize::new(0);
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = OnDrop(Arc::clone(&dropped));
            let operation = async {
                let _guard = guard;
                poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<()>::Pending
                })
                .await;
            };
            let mut request = std::pin::pin!(with_timeout(Duration::from_secs(30), operation));
            let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            {
                let _guard = owner.clone().set_current_restricted();
                assert!(request.as_mut().poll(&mut cx).is_pending());
            }
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel between polls"),
            );
            assert!(matches!(
                request.as_mut().poll(&mut cx),
                Poll::Ready(Err(RequestStop::Cancelled))
            ));
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(!Cx::current().unwrap().is_cancel_requested());
        });
    }

    #[test]
    fn cancellation_during_ready_poll_discards_the_late_output() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let dropped = Arc::new(AtomicBool::new(false));
        let result = runtime.block_on(owner.with_current(with_timeout(
            Duration::from_secs(30),
            async {
                owner.cancel_with(asupersync::types::CancelKind::User, Some("late cancel"));
                OnDrop(Arc::clone(&dropped))
            },
        )));
        assert!(matches!(result, Err(RequestStop::Cancelled)));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn idle_cancellation_blind_request_is_woken_and_retired() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let cancel_owner = owner.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let cancel_thread = std::thread::spawn(move || {
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            cancel_owner.cancel_with(asupersync::types::CancelKind::User, Some("idle cancel"));
        });
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        let result = runtime.block_on(owner.with_current(with_timeout(
            Duration::from_secs(5),
            async move {
                let _guard = guard;
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            },
        )));
        cancel_thread.join().unwrap();
        assert_eq!(result, Err(RequestStop::Cancelled));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn deadline_expiring_during_ready_poll_does_not_publish_late_success() {
        asupersync::test_utils::run_test(|| async {
            let result = with_timeout(Duration::from_millis(1), async {
                std::thread::sleep(Duration::from_millis(20));
                42
            })
            .await;
            assert_eq!(result, Err(RequestStop::TimedOut));
        });
    }

    #[test]
    fn dropping_pending_wrapper_drops_request_and_restores_ambient_context() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = runtime.request_cx_with_budget(Budget::new().with_poll_quota(100));
        runtime.block_on(async {
            let parent = Cx::current().unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = OnDrop(Arc::clone(&dropped));
            let mut request = Box::pin(with_timeout(Duration::from_secs(30), async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            }));
            {
                let _owner_guard = owner.clone().set_current_restricted();
                assert!(futures::poll!(&mut request).is_pending());
            }
            drop(request);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(Cx::current().unwrap().budget(), parent.budget());
            assert!(!parent.is_cancel_requested());
        });
    }
}
