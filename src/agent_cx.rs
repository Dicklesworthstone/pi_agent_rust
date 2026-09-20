//! Capability-scoped async context for Pi.
//!
//! Pi builds on `asupersync` which provides a capability-based [`asupersync::Cx`] for cancellation,
//! budgeting, and deterministic testing hooks. Historically this codebase has sometimes passed raw
//! `Cx` instances around ad-hoc (or constructed them at call sites), which makes it harder to audit
//! the *intended* capability boundary between subsystems.
//!
//! `AgentCx` is a thin, explicit wrapper used at API boundaries (agent loop ↔ tools ↔ sessions ↔
//! RPC). It is intentionally small: it does **not** try to introduce a new runtime or replace
//! `asupersync::Cx`; it just centralizes how Pi threads context through async code.

use asupersync::{Budget, Cx};
use std::ffi::OsStr;
use std::future::{Future, poll_fn};
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

mod http;
mod process;
pub use http::{AgentHttpClient, AgentHttpRequest, AgentHttpResponse};
pub use process::{AgentChild, AgentCommand};

/// A capability-scoped context for agent operations.
///
/// ## Construction
/// - **Production:** prefer constructing once per top-level request/run and passing `&AgentCx`
///   through.
/// - **Tests:** use [`Self::for_testing`] / [`Self::for_testing_with_io`] to avoid ambient
///   dependencies and to keep runs deterministic.
#[derive(Debug, Clone)]
pub struct AgentCx {
    cx: Cx,
}

impl AgentCx {
    /// Wrap an existing `asupersync::Cx`.
    #[must_use]
    pub const fn from_cx(cx: Cx) -> Self {
        Self { cx }
    }

    /// Use the ambient context when available, otherwise fall back to a request-scoped context.
    ///
    /// This is useful when helper code may run either inside an already-scoped async task
    /// (where inheriting the current cancellation/budget is desirable) or at a top-level entry
    /// point that needs to create a fresh request context.
    #[must_use]
    pub fn for_current_or_request() -> Self {
        Self {
            cx: Cx::current().unwrap_or_else(Cx::for_request),
        }
    }

    /// Create a request-scoped context (infinite budget).
    #[must_use]
    pub fn for_request() -> Self {
        Self {
            cx: Cx::for_request(),
        }
    }

    /// Create a request-scoped context with an explicit budget.
    #[must_use]
    pub fn for_request_with_budget(budget: Budget) -> Self {
        Self {
            cx: Cx::for_request_with_budget(budget),
        }
    }

    /// Create a test-only context (infinite budget).
    #[must_use]
    pub fn for_testing() -> Self {
        Self {
            cx: Cx::for_testing(),
        }
    }

    /// Create a test-only context with lab I/O capability.
    #[must_use]
    pub fn for_testing_with_io() -> Self {
        Self {
            cx: Cx::for_testing_with_io(),
        }
    }

    /// Borrow the underlying `asupersync` context.
    #[must_use]
    pub const fn cx(&self) -> &Cx {
        &self.cx
    }

    /// Poll ambient-context APIs under this owner, not under the caller's task.
    ///
    /// The thread-local guard must end before returning `Pending`: holding it
    /// across an await would leak authority into other tasks on the same worker
    /// and could restore the wrong thread's context after task migration.
    pub(crate) async fn with_current<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        poll_fn(|task_cx| {
            let _guard = self.cx.clone().set_current_restricted();
            future.as_mut().poll(task_cx)
        })
        .await
    }

    /// Filesystem capability accessor.
    #[must_use]
    pub const fn fs(&self) -> AgentFs<'_> {
        AgentFs { cx: self }
    }

    /// Time capability accessor.
    #[must_use]
    pub const fn time(&self) -> AgentTime<'_> {
        AgentTime { cx: self }
    }

    /// HTTP capability accessor. Requests and response bodies retain this owner.
    #[must_use]
    pub const fn http(&self) -> AgentHttp<'_> {
        AgentHttp { cx: self }
    }

    /// Process capability accessor. Owned commands check authority at spawn.
    #[must_use]
    pub const fn process(&self) -> AgentProcess<'_> {
        AgentProcess { cx: self }
    }
}

impl Deref for AgentCx {
    type Target = Cx;

    fn deref(&self) -> &Self::Target {
        self.cx()
    }
}

/// Filesystem-related operations.
///
/// Authority and cancellation are checked when an operation is first polled,
/// before dispatching any filesystem work. Once dispatched, a blocking syscall
/// may still complete after cancellation; these methods await its result rather
/// than claiming that cancellation rolls back a write.
pub struct AgentFs<'a> {
    cx: &'a AgentCx,
}

impl AgentFs<'_> {
    fn check_access(&self) -> std::io::Result<()> {
        if !self.cx.capabilities().io {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "agent context does not permit filesystem I/O",
            ));
        }
        self.cx.checkpoint().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "agent filesystem operation cancelled before dispatch",
            )
        })
    }

    pub async fn read(&self, path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
        self.check_access()?;
        self.cx.with_current(asupersync::fs::read(path)).await
    }

    pub async fn write(
        &self,
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> std::io::Result<()> {
        self.check_access()?;
        self.cx
            .with_current(asupersync::fs::write(path, contents))
            .await
    }

    pub async fn create_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.check_access()?;
        self.cx
            .with_current(asupersync::fs::create_dir_all(path))
            .await
    }
}

/// Time-related operations.
pub struct AgentTime<'a> {
    cx: &'a AgentCx,
}

impl AgentTime<'_> {
    pub async fn sleep(&self, duration: Duration) {
        self.cx
            .with_current(async {
                let now = self
                    .cx
                    .cx()
                    .timer_driver()
                    .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                // Sleep obtains its timer through the ambient Cx. Both the
                // clock origin and timer registration must belong to our owner.
                asupersync::time::sleep(now, duration).await;
            })
            .await;
    }
}

/// HTTP operations scoped through dispatch and response-body consumption.
/// A configured client can be bound without losing VCR or transport settings.
pub struct AgentHttp<'a> {
    cx: &'a AgentCx,
}

impl AgentHttp<'_> {
    #[must_use]
    pub fn client(&self) -> AgentHttpClient {
        AgentHttpClient::new(self.cx.clone(), crate::http::client::Client::new())
    }

    #[must_use]
    pub fn bind(&self, client: &crate::http::client::Client) -> AgentHttpClient {
        AgentHttpClient::new(self.cx.clone(), client.clone())
    }

    /// Attach this owner to an already-configured request. This preserves its
    /// endpoint, credentials, headers, body, recorder and timeout settings.
    #[must_use]
    pub fn request<'a>(
        &self,
        request: crate::http::client::RequestBuilder<'a>,
    ) -> AgentHttpRequest<'a> {
        AgentHttpRequest::new(self.cx.clone(), request)
    }
}

/// Process operations scoped through dispatch and owned child cleanup.
pub struct AgentProcess<'a> {
    cx: &'a AgentCx,
}

impl AgentProcess<'_> {
    #[must_use]
    pub fn command(&self, program: impl AsRef<OsStr>) -> AgentCommand {
        AgentCommand::new(self.cx.clone(), program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_request_creates_valid_context() {
        let cx = AgentCx::for_request();
        // Verify the inner Cx is accessible.
        let _ = cx.cx();
    }

    #[test]
    fn from_cx_wraps_existing_context() {
        let inner = Cx::for_request();
        let cx = AgentCx::from_cx(inner);
        let _ = cx.cx();
    }

    #[test]
    fn for_current_or_request_creates_valid_context() {
        let cx = AgentCx::for_current_or_request();
        let _ = cx.cx();
    }

    #[test]
    fn inherited_context_preserves_restrictions_budget_and_owner_cancellation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime");
        let budget = Budget::new().with_poll_quota(8);
        let owner = runtime.request_cx_with_budget(budget);

        runtime.block_on(async {
            let parent = Cx::current().expect("runtime installs its context");
            let parent_caps = parent.capabilities();
            let captured = {
                let restricted = owner.restrict::<asupersync::cx::cap::None>();
                let _guard = restricted.set_current_restricted();
                let ambient = Cx::current().expect("restricted context remains present");
                let cx = AgentCx::for_current_or_request();
                let caps = cx.capabilities();
                assert_eq!(caps, ambient.capabilities());
                assert!(!caps.spawn && !caps.time && !caps.entropy && !caps.io && !caps.remote);
                assert_eq!(cx.budget(), budget);

                // The explicit constructor must preserve the same attenuated context.
                let wrapped = AgentCx::from_cx(cx.cx().clone());
                assert_eq!(wrapped.capabilities(), caps);
                assert_eq!(wrapped.budget(), budget);
                wrapped
            };
            assert_eq!(
                Cx::current().expect("parent restored").capabilities(),
                parent_caps
            );

            // Exercise the real cancel-aware lock used at Pi's session boundaries.
            let state = asupersync::sync::Mutex::new(7_u32);
            {
                let mut guard = state
                    .lock(captured.cx())
                    .await
                    .expect("live owner admitted");
                *guard += 1;
            }
            let captured_caps = captured.capabilities();
            owner.cancel_with(asupersync::types::CancelKind::User, Some("owner cancelled"));
            assert!(matches!(
                state.lock(captured.cx()).await,
                Err(asupersync::sync::LockError::Cancelled)
            ));
            assert_eq!(*state.try_lock().expect("cancelled lock released"), 8);
            assert_eq!(captured.capabilities(), captured_caps);
            assert_eq!(captured.budget(), budget);
            assert!(!parent.is_cancel_requested());
            assert_eq!(
                Cx::current().expect("parent retained").capabilities(),
                parent_caps
            );
        });
    }

    #[test]
    fn scoped_poll_preserves_owner_and_restores_parent_between_polls() {
        let parent = Cx::for_request_with_budget(Budget::new().with_poll_quota(23));
        let _parent_guard = parent.clone().set_current_restricted();
        let owner = AgentCx::for_request_with_budget(Budget::new().with_poll_quota(7));
        let mut first_poll = true;
        let operation = poll_fn(|_| {
            let current = Cx::current().expect("scoped owner installed");
            assert_eq!(current.budget(), owner.budget());
            assert_eq!(current.capabilities(), owner.capabilities());
            if first_poll {
                first_poll = false;
                assert!(!current.is_cancel_requested());
                std::task::Poll::Pending
            } else {
                assert!(current.is_cancel_requested());
                std::task::Poll::Ready(())
            }
        });
        let mut scoped = std::pin::pin!(owner.with_current(operation));
        let mut task_cx = std::task::Context::from_waker(futures::task::noop_waker_ref());

        assert!(scoped.as_mut().poll(&mut task_cx).is_pending());
        assert_eq!(
            Cx::current().expect("parent restored").budget(),
            parent.budget()
        );
        assert!(!parent.is_cancel_requested());

        owner.cancel_with(asupersync::types::CancelKind::User, Some("owner cancelled"));
        assert!(scoped.as_mut().poll(&mut task_cx).is_ready());
        assert_eq!(
            Cx::current().expect("parent restored").budget(),
            parent.budget()
        );
        assert!(!parent.is_cancel_requested());
    }

    #[test]
    fn scoped_poll_restores_parent_after_unwind() {
        let parent = Cx::for_request_with_budget(Budget::new().with_poll_quota(23));
        let _parent_guard = parent.clone().set_current_restricted();
        let owner = AgentCx::for_request_with_budget(Budget::new().with_poll_quota(7));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // This panic is intentional; do not pollute another test's crash bundle.
            let _panic_guard = crate::crash::SuppressPanicHook::new();
            let mut scoped = std::pin::pin!(owner.with_current(async {
                assert_eq!(
                    Cx::current().expect("owner installed").budget(),
                    owner.budget()
                );
                panic!("scoped operation failed");
            }));
            let mut task_cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            let _ = scoped.as_mut().poll(&mut task_cx);
        }));
        assert!(result.is_err());
        assert_eq!(
            Cx::current().expect("parent restored").budget(),
            parent.budget()
        );
    }

    #[test]
    fn restricted_filesystem_operations_do_not_read_or_mutate_paths() {
        let dir = tempfile::tempdir().expect("test directory");
        let path = dir.path().join("existing.txt");
        let new_dir = dir.path().join("new/nested");
        std::fs::write(&path, b"original").expect("write fixture");
        let owner = Cx::for_request();
        let captured = {
            let _guard = owner
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime");

        runtime.block_on(async {
            let fs = captured.fs();
            assert_eq!(
                fs.read(&path).await.expect_err("read denied").kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs.write(&path, b"replacement")
                    .await
                    .expect_err("write denied")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs.create_dir_all(&new_dir)
                    .await
                    .expect_err("mkdir denied")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        });
        assert_eq!(std::fs::read(&path).expect("read original"), b"original");
        assert!(!dir.path().join("new").exists());
    }

    #[test]
    fn cancellation_after_future_creation_prevents_filesystem_dispatch() {
        let dir = tempfile::tempdir().expect("test directory");
        let path = dir.path().join("existing.txt");
        let new_dir = dir.path().join("new/nested");
        std::fs::write(&path, b"original").expect("write fixture");
        let owner = AgentCx::for_request();
        let fs = owner.fs();
        let read = fs.read(&path);
        let write = fs.write(&path, b"replacement");
        let mkdir = fs.create_dir_all(&new_dir);
        owner.cancel_with(asupersync::types::CancelKind::User, Some("owner cancelled"));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime");

        runtime.block_on(async {
            assert_eq!(
                read.await.expect_err("read cancelled").kind(),
                std::io::ErrorKind::Interrupted
            );
            assert_eq!(
                write.await.expect_err("write cancelled").kind(),
                std::io::ErrorKind::Interrupted
            );
            assert_eq!(
                mkdir.await.expect_err("mkdir cancelled").kind(),
                std::io::ErrorKind::Interrupted
            );
        });
        assert_eq!(std::fs::read(&path).expect("read original"), b"original");
        assert!(!dir.path().join("new").exists());
    }

    #[test]
    fn live_filesystem_owner_can_create_write_and_read() {
        let dir = tempfile::tempdir().expect("test directory");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime");
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let fs = owner.fs();
            let nested = dir.path().join("nested");
            let path = nested.join("file.txt");
            fs.create_dir_all(&nested).await.expect("create directory");
            fs.write(&path, b"content").await.expect("write file");
            assert_eq!(fs.read(&path).await.expect("read file"), b"content");
        });
    }

    #[test]
    fn for_testing_creates_valid_context() {
        let cx = AgentCx::for_testing();
        let _ = cx.cx();
    }

    #[test]
    fn for_testing_with_io_creates_valid_context() {
        let cx = AgentCx::for_testing_with_io();
        let _ = cx.cx();
    }

    #[test]
    fn for_request_with_budget_creates_valid_context() {
        let budget = Budget::new().with_poll_quota(100);
        let cx = AgentCx::for_request_with_budget(budget);
        let _ = cx.cx();
    }

    #[test]
    fn fs_accessor_returns_agent_fs() {
        let cx = AgentCx::for_testing();
        let _fs = cx.fs();
    }

    #[test]
    fn time_accessor_returns_agent_time() {
        let cx = AgentCx::for_testing();
        let _time = cx.time();
    }

    #[test]
    fn http_accessor_returns_agent_http() {
        let cx = AgentCx::for_testing();
        let _http = cx.http();
    }

    #[test]
    fn process_accessor_returns_agent_process() {
        let cx = AgentCx::for_testing();
        let _proc = cx.process();
    }

    #[test]
    fn process_command_creates_command() {
        let cx = AgentCx::for_testing();
        let cmd = cx.process().command("echo");
        assert_eq!(cmd.get_program(), "echo");
    }

    #[test]
    fn agent_cx_is_clone() {
        let cx = AgentCx::for_testing();
        let cx2 = cx.clone();
        let _ = cx.cx();
        let _ = cx2.cx();
    }
}
