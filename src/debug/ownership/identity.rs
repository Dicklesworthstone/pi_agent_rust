//! Cancellation-aware identity waits. Never invent a fallback owner because
//! its session mutex is contended, and never leave the DAP lease held forever.

use std::task::Poll;
use std::time::{Duration, Instant};

use crate::agent_cx::AgentCx;
use crate::debug::session::DEFAULT_DAP_TIMEOUT;
use crate::debug::tool_err;
use crate::error::Result;
use crate::jobs::JobSessionScope;

fn checkpoint(owner: &AgentCx, deadline: Instant) -> Result<()> {
    owner.checkpoint()
        .map_err(|_| tool_err("DAP_CANCELLED", "debug owner resolution cancelled"))?;
    if Instant::now() >= deadline {
        return Err(tool_err(
            "DAP_SESSION_TIMEOUT",
            "current agent session identity did not resolve within its budget; request was not retargeted",
        ));
    }
    Ok(())
}

pub(super) async fn resolve(scope: &JobSessionScope, owner: &AgentCx) -> Result<String> {
    resolve_before(scope, owner, Instant::now() + DEFAULT_DAP_TIMEOUT).await
}

async fn resolve_before(scope: &JobSessionScope, owner: &AgentCx, deadline: Instant) -> Result<String> {
    // Retire the resolver under its request's context even when a queued tool
    // is dropped on another worker. Never keep a thread-local guard over await.
    let mut pending = Box::pin(owner.with_current(scope.session_id()));
    let result = loop {
        checkpoint(owner, deadline)?;
        // Ready fixed/fast resolvers need no timer capability or timer allocation.
        if let Poll::Ready(result) = futures::poll!(pending.as_mut()) {
            break result;
        }
        if !owner.capabilities().time {
            return Err(tool_err(
                "DAP_PERMISSION",
                "waiting for the current agent session requires timer capability",
            ));
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20));
        let time = owner.time();
        let sleeper = Box::pin(time.sleep(wait));
        match futures::future::select(pending, sleeper).await {
            futures::future::Either::Left((result, _)) => break result,
            futures::future::Either::Right(((), operation)) => pending = operation,
        }
    };
    // Cancellation or a blocking Ready poll that overran its deadline cannot
    // become success just because identity resolution completed in that poll.
    checkpoint(owner, deadline)?;
    result.map_err(|_| tool_err(
        "DAP_SESSION_UNAVAILABLE",
        "current agent session identity is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8).build().unwrap().block_on(future)
    }

    #[test]
    fn expired_and_cancelled_owners_do_not_invoke_the_resolver() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let scope = JobSessionScope::fixed("unused");
        scope.bind(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Some("owner-a".into()) })
        }));
        let owner = AgentCx::for_request();
        run(async {
            let error = resolve_before(&scope, &owner, Instant::now()).await.unwrap_err();
            assert!(error.to_string().contains("DAP_SESSION_TIMEOUT"));
            owner.set_cancel_requested(true);
            let error = resolve(&scope, &owner).await.unwrap_err();
            assert!(error.to_string().contains("DAP_CANCELLED"));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_pending_resolver_is_dropped_at_its_deadline() {
        struct OnDrop(Arc<AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&dropped);
        let scope = JobSessionScope::fixed("unused");
        scope.bind(Arc::new(move || {
            let guard = OnDrop(Arc::clone(&observed));
            Box::pin(async move {
                let _resource = guard;
                std::future::pending::<Option<String>>().await
            })
        }));
        let owner = AgentCx::for_request();
        run(async {
            let start = Instant::now();
            let error = resolve_before(&scope, &owner, start + Duration::from_millis(30)).await.unwrap_err();
            assert!(error.to_string().contains("DAP_SESSION_TIMEOUT"));
            assert!(start.elapsed() < Duration::from_secs(5));
        });
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn resolving_after_contention_is_not_reported_as_unavailable() {
        let (sender, receiver) = futures::channel::oneshot::channel::<String>();
        let slot = Arc::new(std::sync::Mutex::new(Some(receiver)));
        let scope = JobSessionScope::fixed("unused");
        scope.bind(Arc::new(move || {
            let receiver = slot.lock().unwrap().take().unwrap();
            Box::pin(async move { receiver.await.ok() })
        }));
        let owner = AgentCx::for_request();
        run(async {
            let mut pending = Box::pin(resolve(&scope, &owner));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            sender.send("owner-a".into()).unwrap();
            assert_eq!(pending.await.unwrap(), "owner-a");
        });
    }

    #[test]
    fn a_blocking_ready_resolution_cannot_overrun_the_deadline_as_success() {
        let owner = AgentCx::for_request();
        let scope = JobSessionScope::fixed("unused");
        scope.bind(Arc::new(|| Box::pin(async {
            // Deliberately block one poll, not an asynchronous timer: the
            // result must still fail the post-poll deadline check.
            std::thread::sleep(Duration::from_millis(20));
            Some("owner-a".into())
        })));
        run(async {
            let error = resolve_before(&scope, &owner, Instant::now() + Duration::from_millis(5)).await.unwrap_err();
            assert!(error.to_string().contains("DAP_SESSION_TIMEOUT"));
        });
    }

    #[test]
    fn ready_resolution_does_not_hide_cancellation() {
        let owner = AgentCx::for_request();
        let canceller = owner.clone();
        let scope = JobSessionScope::fixed("unused");
        scope.bind(Arc::new(move || {
            canceller.set_cancel_requested(true);
            Box::pin(async { Some("owner-a".into()) })
        }));
        let error = run(resolve(&scope, &owner)).unwrap_err();
        assert!(error.to_string().contains("DAP_CANCELLED"));
    }
}
