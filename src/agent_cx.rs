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
use std::path::Path;
use std::time::Duration;

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

    /// Filesystem capability accessor.
    #[must_use]
    pub const fn fs(&self) -> AgentFs<'_> {
        AgentFs { _cx: self }
    }

    /// Time capability accessor.
    #[must_use]
    pub const fn time(&self) -> AgentTime<'_> {
        AgentTime { _cx: self }
    }

    /// HTTP capability accessor.
    #[must_use]
    pub const fn http(&self) -> AgentHttp<'_> {
        AgentHttp { _cx: self }
    }

    /// Process capability accessor.
    #[must_use]
    pub const fn process(&self) -> AgentProcess<'_> {
        AgentProcess { _cx: self }
    }
}

/// Filesystem-related operations.
pub struct AgentFs<'a> {
    _cx: &'a AgentCx,
}

impl AgentFs<'_> {
    pub async fn read(&self, path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
        asupersync::fs::read(path).await
    }

    pub async fn write(
        &self,
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> std::io::Result<()> {
        asupersync::fs::write(path, contents).await
    }

    pub async fn create_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        asupersync::fs::create_dir_all(path).await
    }
}

/// Time-related operations.
pub struct AgentTime<'a> {
    _cx: &'a AgentCx,
}

impl AgentTime<'_> {
    pub async fn sleep(&self, duration: Duration) {
        asupersync::time::sleep(asupersync::time::wall_now(), duration).await;
    }
}

/// HTTP-related operations.
pub struct AgentHttp<'a> {
    _cx: &'a AgentCx,
}

impl AgentHttp<'_> {
    #[must_use]
    pub fn client(&self) -> crate::http::client::Client {
        crate::http::client::Client::new()
    }
}

/// Process-related operations.
pub struct AgentProcess<'a> {
    _cx: &'a AgentCx,
}

impl AgentProcess<'_> {
    #[must_use]
    pub fn command(&self, program: &str) -> std::process::Command {
        std::process::Command::new(program)
    }
}
