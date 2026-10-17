//! Request waits retain their owner and retire abandoned requests on drop.

use std::future::Future;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use crate::agent_cx::AgentCx;

struct Abandon<F: FnOnce()> {
    callback: Option<F>,
    owner: AgentCx,
}

impl<F: FnOnce()> Drop for Abandon<F> {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            // Destruction may happen before the first poll, on another task,
            // or during unwinding. Cleanup must retain the request's owner.
            let _owner = self.owner.cx().clone().set_current_restricted();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).is_err() {
                tracing::warn!("request abandonment callback panicked during cleanup");
            }
        }
    }
}

/// Wait for a completion while retaining the call-time owner and deadline.
///
/// Timeout, owner cancellation, a disconnected sender, and dropping the
/// returned future all call `on_abandon` exactly once. Even an unpolled future
/// owns cleanup: callers have already sent or queued a request before this
/// function is called. Consuming a successful completion disarms cleanup.
/// Cancellation and expiry take precedence over a buffered response.
///
/// `on_abandon` must be synchronous and non-blocking. The LSP transport queues
/// its cancellation frame instead of writing to a possibly stalled pipe here.
/// Shared with the MCP stdio transport; this does not retry requests or promise
/// to undo work the remote process already accepted.
pub fn await_completion<T>(
    rx: Receiver<T>,
    timeout: Duration,
    on_abandon: impl FnOnce(),
) -> impl Future<Output = std::result::Result<T, CompletionWaitError>> {
    let owner = AgentCx::for_current_or_request();
    let mut abandonment = Abandon {
        callback: Some(on_abandon),
        owner: owner.clone(),
    };
    let start = owner
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let poll_owner = owner.clone();
    async move {
        poll_owner
            .with_current(async move {
                loop {
                    if owner.checkpoint().is_err() {
                        return Err(CompletionWaitError::Cancelled);
                    }
                    let now = owner
                        .cx()
                        .timer_driver()
                        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                    let elapsed = Duration::from_nanos(now.duration_since(start));
                    if elapsed >= timeout {
                        return Err(CompletionWaitError::Timeout);
                    }
                    match rx.try_recv() {
                        Ok(value) => {
                            // Never publish an already buffered result for an
                            // owner that was cancelled while receiving it.
                            if owner.checkpoint().is_err() {
                                return Err(CompletionWaitError::Cancelled);
                            }
                            let finished = owner
                                .cx()
                                .timer_driver()
                                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                            if Duration::from_nanos(finished.duration_since(start)) >= timeout {
                                return Err(CompletionWaitError::Timeout);
                            }
                            drop(abandonment.callback.take());
                            return Ok(value);
                        }
                        Err(TryRecvError::Disconnected) => return Err(CompletionWaitError::Closed),
                        Err(TryRecvError::Empty) => {}
                    }
                    let delay = timeout
                        .saturating_sub(elapsed)
                        .min(Duration::from_millis(10));
                    asupersync::time::sleep(now, delay).await;
                }
            })
            .await
    }
}

/// Why a completion wait ended without a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionWaitError {
    /// Deadline exceeded.
    Timeout,
    /// The captured owner cancelled.
    Cancelled,
    /// The sender dropped without sending.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    fn once<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("test completion must be immediately ready"),
        }
    }

    #[test]
    fn dropping_an_unpolled_wait_retires_the_request_once() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel::<()>(1);
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&count);
        let future = await_completion(receiver, Duration::from_secs(60), move || {
            callback_count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(future);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_completion_disarms_abandonment() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send("completed").expect("send");
        let count = AtomicUsize::new(0);
        let result = once(await_completion(receiver, Duration::from_secs(60), || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(result, Ok("completed"));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn deadline_wins_over_a_buffered_response_and_cleans_up_once() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send("stale").expect("send");
        let count = AtomicUsize::new(0);
        let result = once(await_completion(receiver, Duration::ZERO, || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(result, Err(CompletionWaitError::Timeout));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disconnected_sender_still_retires_pending_state() {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<()>(1);
        drop(sender);
        let count = AtomicUsize::new(0);
        assert_eq!(
            once(await_completion(receiver, Duration::from_secs(60), || {
                count.fetch_add(1, Ordering::SeqCst);
            })),
            Err(CompletionWaitError::Closed)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancellation_and_cleanup_retain_the_construction_owner() {
        let parent =
            AgentCx::for_request_with_budget(asupersync::Budget::new().with_poll_quota(23));
        let _parent = parent.cx().clone().set_current_restricted();
        let owner = AgentCx::for_request_with_budget(asupersync::Budget::new().with_poll_quota(7));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send("stale").expect("send");
        let count = AtomicUsize::new(0);
        let future = {
            let _current = owner.cx().clone().set_current_restricted();
            await_completion(receiver, Duration::from_secs(60), || {
                assert_eq!(
                    asupersync::Cx::current().expect("cleanup owner").budget(),
                    owner.budget()
                );
                count.fetch_add(1, Ordering::SeqCst);
            })
        };
        owner.cancel_with(asupersync::types::CancelKind::User, Some("owner cancelled"));
        assert_eq!(once(future), Err(CompletionWaitError::Cancelled));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            asupersync::Cx::current().expect("parent restored").budget(),
            parent.budget()
        );
        assert!(!parent.is_cancel_requested());
    }

    #[test]
    fn dropping_a_suspended_wait_retires_it_without_another_poll() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime");
        runtime.block_on(async {
            let (_sender, receiver) = std::sync::mpsc::sync_channel::<()>(1);
            let count = AtomicUsize::new(0);
            let mut future = Box::pin(await_completion(receiver, Duration::from_secs(60), || {
                count.fetch_add(1, Ordering::SeqCst);
            }));
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            assert_eq!(count.load(Ordering::SeqCst), 0);
            drop(future);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        });
    }
}
