//! Revocable ownership of synchronous source work dispatched to a blocking
//! worker. The async request owns revocation; worker clones cannot extend it.
//!
//! The thread-local is installed only within Worker::run's synchronous scope.
//! No async task or await holds that scope. Nested calls restore their previous
//! owner on success, error and unwinding.

use super::{Result, scan_error};
use crate::agent_cx::AgentCx;
use std::cell::RefCell;
use std::sync::{Arc, Mutex, MutexGuard};

struct State {
    owner: AgentCx,
    live: Mutex<bool>,
}

fn authority(owner: &AgentCx) -> Result<()> {
    if !owner.capabilities().io {
        return Err(scan_error("source operation requires I/O capability"));
    }
    owner
        .checkpoint()
        .map_err(|_| scan_error("source operation cancelled"))
}

impl State {
    fn permit(&self) -> Result<MutexGuard<'_, bool>> {
        let permit = self.live.lock()
            .map_err(|_| scan_error("source operation publication gate is poisoned"))?;
        if !*permit {
            return Err(scan_error("source request was dropped; operation revoked"));
        }
        authority(&self.owner)?;
        Ok(permit)
    }
}

/// Intentionally not Clone: dropping the single awaiting request revokes its
/// worker, not the shared session owner or any sibling source operation.
struct Request(Arc<State>);

impl Request {
    fn new() -> Result<Self> {
        let owner = AgentCx::for_current_or_request();
        authority(&owner)?;
        Ok(Self(Arc::new(State { owner, live: Mutex::new(true) })))
    }

    fn worker(&self) -> Worker {
        Worker(Arc::clone(&self.0))
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // Serialize with the final rename/link only, never a scan or file
        // serialization. An already admitted OS commit may finish first; no
        // later publication can be admitted after this Drop returns. This
        // can wait for that foreign syscall and is not a hard-time guarantee.
        let mut live = self.0.live.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live = false;
    }
}

struct Worker(Arc<State>);

thread_local! {
    static CURRENT: RefCell<Option<Arc<State>>> = const { RefCell::new(None) };
}

struct Installed(Option<Arc<State>>);
impl Drop for Installed {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

impl Worker {
    fn run<T>(self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let _authority = self.0.owner.cx().clone().set_current_restricted();
        let _installed = Installed(CURRENT.with(|current| current.replace(Some(self.0))));
        checkpoint()?;
        let result = work();
        checkpoint()?;
        result
    }
}

/// The request guard stays in this future, never in the blocking closure.
/// Dropping a queued/running invocation therefore revokes that closure even
/// when the runtime cannot interrupt its currently executing OS operation.
pub(super) async fn run<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let request = Request::new()?;
    let worker = request.worker();
    let result = asupersync::runtime::spawn_blocking(move || worker.run(work)).await;
    drop(request);
    result
}

fn current() -> Option<Arc<State>> {
    CURRENT.with(|current| current.borrow().clone())
}

pub(super) fn checkpoint() -> Result<()> {
    if let Some(state) = current() {
        drop(state.permit()?);
        Ok(())
    } else {
        // Direct synchronous SDK calls have no async request to drop but
        // still honor the caller's restricted capabilities and cancellation.
        authority(&AgentCx::for_current_or_request())
    }
}

/// Keep only the final atomic filesystem commit inside the revocation gate.
/// The closure must not call checkpoint() or recursively publish.
pub(super) fn publish<T>(write: impl FnOnce() -> Result<T>) -> Result<T> {
    let state = current();
    let _permit = state.as_ref().map(|state| state.permit()).transpose()?;
    if state.is_none() {
        authority(&AgentCx::for_current_or_request())?;
    }
    write()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn dropped_request_revokes_queued_work_even_with_a_fresh_callers_authority() {
        let request = Request::new().unwrap();
        let worker = request.worker();
        drop(request);
        let called = AtomicBool::new(false);
        let fresh = asupersync::Cx::for_request();
        let _current = fresh.set_current_restricted();
        let result = worker.run(|| { called.store(true, Ordering::SeqCst); Ok(()) });
        assert!(result.is_err());
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn revocation_does_not_cancel_the_session_or_sibling_requests() {
        let owner = AgentCx::for_request();
        let _current = owner.cx().clone().set_current_restricted();
        let first = Request::new().unwrap();
        let second = Request::new().unwrap();
        let first_worker = first.worker();
        drop(first);
        assert!(first_worker.run(|| Ok(())).is_err());
        assert!(owner.checkpoint().is_ok());
        assert_eq!(second.worker().run(|| Ok(42)).unwrap(), 42);
    }

    #[test]
    fn explicit_owner_cancellation_prevents_dispatch_under_a_live_request() {
        let owner = AgentCx::for_request();
        let _current = owner.cx().clone().set_current_restricted();
        let request = Request::new().unwrap();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("fixture cancellation"));
        assert!(request.worker().run(|| Ok(())).is_err());
    }

    #[test]
    fn synchronous_worker_scope_restores_nested_owners_after_unwinding() {
        let outer = Request::new().unwrap();
        let inner = Request::new().unwrap();
        outer.worker().run(|| {
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _: Result<()> = inner.worker().run(|| panic!("fixture unwind"));
            }));
            assert!(failed.is_err());
            drop(inner);
            // Inner revocation must not remain installed on a reused thread.
            checkpoint()?;
            publish(|| Ok(()))
        }).unwrap();
        drop(outer);
        checkpoint().unwrap();
    }

    #[test]
    fn denied_request_has_no_worker_and_does_not_borrow_a_future_callers_capabilities() {
        let owner = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _current = owner.set_current_restricted();
        assert!(Request::new().is_err());
        assert!(publish(|| Ok(())).is_err());
    }

    #[test]
    fn a_completed_request_cannot_authorize_a_retained_worker() {
        let request = Request::new().unwrap();
        let retained = request.worker();
        assert_eq!(request.worker().run(|| Ok(7)).unwrap(), 7);
        drop(request);
        assert!(retained.run(|| publish(|| Ok(()))).is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn dropped_running_request_cannot_publish_reports_or_review_decisions() {
        use super::super::storage::{DEFAULT_REPORT, Storage};
        use std::sync::mpsc;
        for operation in ["default", "export", "disposition"] {
            let dir = tempfile::tempdir().unwrap();
            let storage = Storage::open(dir.path()).unwrap();
            storage.write_report(None, b"previous complete report").unwrap();
            let request = Request::new().unwrap();
            let worker = request.worker();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let thread = std::thread::spawn(move || worker.run(|| {
                ready_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                match operation {
                    "default" => storage.write_report(None, b"late report"),
                    "export" => storage.write_report(Some("late.sarif"), b"late report"),
                    _ => storage.update("0123456789abcdef".into(), super::super::Disposition {
                        status: "accepted".into(), reason: "late decision".into(), at_ms: 1,
                    }).map(|_| ()),
                }
            }));
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            // Exact ordering, no sleeps: abandonment completes before the
            // blocked worker is allowed to attempt real persistence.
            drop(request);
            resume_tx.send(()).unwrap();
            assert!(thread.join().unwrap().is_err());
            assert_eq!(std::fs::read(dir.path().join(DEFAULT_REPORT)).unwrap(), b"previous complete report");
            assert!(!dir.path().join("late.sarif").exists());
            assert!(!dir.path().join(".pi/security-dispositions.json").exists());
        }
    }

    #[test]
    fn admitted_publication_finishes_before_drop_returns_and_later_commits_are_denied() {
        use std::sync::mpsc;
        let request = Request::new().unwrap();
        let worker = request.worker();
        let retained = request.worker();
        let committed = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&committed);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let publishing = std::thread::spawn(move || worker.run(|| publish(|| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })));
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let dropping = std::thread::spawn(move || {
            drop(request);
            assert!(committed.load(Ordering::SeqCst));
        });
        release_tx.send(()).unwrap();
        // The post-work checkpoint may observe either completion or drop;
        // neither result permits publishing after the revocation fence.
        let _ = publishing.join().unwrap();
        dropping.join().unwrap();
        assert!(retained.run(|| publish(|| Ok(()))).is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn dropping_the_real_blocking_future_revokes_its_already_running_publisher() {
        use super::super::storage::{DEFAULT_REPORT, Storage};
        use std::future::Future as _;
        use std::sync::mpsc;
        use std::task::Poll;
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        storage.write_report(None, b"completed baseline").unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(2).blocking_threads(1, 8).build().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        runtime.block_on(async {
            let mut future = Box::pin(run(move || {
                started_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                let result = storage.write_report(None, b"abandoned scan");
                finished_tx.send(result.is_err()).unwrap();
                result
            }));
            std::future::poll_fn(|cx| {
                assert!(matches!(future.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            }).await;
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(future);
            resume_tx.send(()).unwrap();
            assert!(finished_rx.recv_timeout(Duration::from_secs(10)).unwrap());
        });
        assert_eq!(std::fs::read(dir.path().join(DEFAULT_REPORT)).unwrap(), b"completed baseline");
    }
}
