//! Session-scoped retained eval namespaces over the registry's live identity.
//!
//! The execution lease serializes activation with cell execution. A registry
//! snapshot is not a new namespace: snapshots share the same EvalTool and scope.
//! Cache only a bounded number of idle namespaces, never arbitrary session IDs.

use super::{EvalTool, PyKernel, js_kernel};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::jobs::JobSessionScope;
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Includes the active namespace. Each namespace may own one of each kernel.
const MAX_RETAINED_SESSIONS: usize = 4;
const MAX_SESSION_ID_BYTES: usize = 1024;

struct StoredKernels {
    session_id: String,
    python: Option<PyKernel>,
    javascript: Option<js_kernel::JsKernel>,
}

#[derive(Default)]
pub(super) struct RetainedSessions {
    active: Option<String>,
    /// Least recently active first. Empty namespaces consume no cache entry.
    dormant: VecDeque<StoredKernels>,
}

/// A live identity failure is distinct from cancellation and cell timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwnershipStop {
    Changed,
    Unavailable,
}

impl OwnershipStop {
    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::Changed => "EVAL_SESSION_CHANGED",
            Self::Unavailable => "EVAL_SESSION_UNAVAILABLE",
        }
    }

    pub(super) const fn kind(self) -> &'static str {
        match self {
            Self::Changed => "sessionChanged",
            Self::Unavailable => "sessionUnavailable",
        }
    }

    pub(super) fn tool_error(self) -> Error {
        Error::tool(
            "eval",
            format!(
                "{}: active cell no longer has its original session owner; EVAL_STATE_RESET: selected kernel discarded",
                self.code()
            ),
        )
    }
}

/// Frozen ownership for one active invocation. A detected mismatch is sticky:
/// switching back cannot turn already-rejected work into a successful result.
/// This detects observed identities, not unobserved A-to-B-to-A transitions.
/// No resolver, guest code, callback or await runs with the stop lock held.
pub(super) struct CellSession {
    expected: String,
    scope: JobSessionScope,
    stop: Mutex<Option<OwnershipStop>>,
}

impl CellSession {
    pub(super) fn new(scope: JobSessionScope, expected: String) -> Self {
        Self {
            expected,
            scope,
            stop: Mutex::new(None),
        }
    }

    fn stopped(&self) -> Option<OwnershipStop> {
        *self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) async fn check(&self) -> std::result::Result<(), OwnershipStop> {
        if let Some(stop) = self.stopped() {
            return Err(stop);
        }
        let failure = match self.scope.session_id().await {
            Ok(id) if id.len() > MAX_SESSION_ID_BYTES => Some(OwnershipStop::Unavailable),
            Ok(id) if id != self.expected => Some(OwnershipStop::Changed),
            Ok(_) => None,
            Err(_) => Some(OwnershipStop::Unavailable),
        };
        let mut stop = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(failure) = failure {
            let _ = stop.get_or_insert(failure);
        }
        match *stop {
            Some(stop) => Err(stop),
            None => Ok(()),
        }
    }

    pub(super) async fn check_tool(&self) -> Result<()> {
        self.check().await.map_err(OwnershipStop::tool_error)
    }
}

pub(super) struct Activation {
    changed: bool,
    restored: bool,
    evicted: bool,
}

impl Activation {
    pub(super) fn annotate(&self, output: &mut ToolOutput) {
        if let Some(Value::Object(details)) = output.details.as_mut() {
            // Do not disclose another session's ID or kernel contents.
            details.insert(
                "kernelSession".into(),
                json!({
                    "scope": "session",
                    "changed": self.changed,
                    "namespaceRestored": self.restored,
                    "idleNamespaceEvicted": self.evicted,
                    "capacity": MAX_RETAINED_SESSIONS,
                }),
            );
        }
    }
}

impl EvalTool {
    /// The existing registry hook is also the live session-identity source for
    /// bash/jobs/hub. Reuse it rather than maintaining a second frontend binding.
    /// Moving a tool into another registry retires its old storage incarnation;
    /// ordinary session switches rebind the shared resolver, not this hook.
    pub(super) fn bind_eval_session_scope(&mut self, scope: JobSessionScope) {
        self.session_scope = scope;
        let retired = std::mem::take(
            self.retained_sessions
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let python = self
            .kernel
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let javascript = self
            .js
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(python);
        drop(javascript);
        drop(retired);
    }

    /// Capture an exact identity before queueing. An unavailable live session
    /// must not fall back to the preceding session or a process-global kernel.
    pub(super) async fn eval_session_id(&self) -> Result<String> {
        let id = self.session_scope.session_id().await.map_err(|_| {
            Error::tool(
                "eval",
                "EVAL_SESSION_UNAVAILABLE: current session identity is unavailable",
            )
        })?;
        if id.len() > MAX_SESSION_ID_BYTES {
            return Err(Error::tool(
                "eval",
                "EVAL_SESSION_UNAVAILABLE: session identity exceeds 1024 bytes",
            ));
        }
        Ok(id)
    }

    pub(super) async fn require_eval_session(&self, expected: &str) -> Result<()> {
        if self.eval_session_id().await? != expected {
            return Err(Error::tool(
                "eval",
                "EVAL_SESSION_CHANGED: session changed before execution; code and reset were not applied",
            ));
        }
        Ok(())
    }

    /// Activate only after input/source/archive preflight and while owning the
    /// same execution lease the runners hold until their kernel is returned.
    /// No await, callback, or process cleanup occurs with a slot/cache lock held.
    pub(super) fn activate_eval_session(
        &self,
        session_id: &str,
        _execution: &futures::lock::MutexGuard<'_, ()>,
        owner: &AgentCx,
    ) -> Activation {
        let (activation, evicted) = {
            let mut retained = self
                .retained_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retained.active.as_deref() == Some(session_id) {
                return Activation {
                    changed: false,
                    restored: false,
                    evicted: false,
                };
            }
            let changed = retained.active.is_some();
            let index = retained
                .dormant
                .iter()
                .position(|entry| entry.session_id == session_id);
            let mut incoming = index.and_then(|index| retained.dormant.remove(index));
            let restored = incoming.is_some();
            let mut python = self
                .kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut javascript = self
                .js
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_python = std::mem::replace(
                &mut *python,
                incoming.as_mut().and_then(|entry| entry.python.take()),
            );
            let previous_javascript = std::mem::replace(
                &mut *javascript,
                incoming.as_mut().and_then(|entry| entry.javascript.take()),
            );
            let previous = retained.active.replace(session_id.to_string());
            // All production runners activate before spawning. Keep an orphan
            // pair separate so even an unowned slot is cleaned up outside locks.
            let previous = StoredKernels {
                session_id: previous.unwrap_or_default(),
                python: previous_python,
                javascript: previous_javascript,
            };
            let evicted = if previous.session_id.is_empty() {
                Some(previous)
            } else {
                if previous.python.is_some() || previous.javascript.is_some() {
                    retained.dormant.push_back(previous);
                }
                if retained.dormant.len() >= MAX_RETAINED_SESSIONS {
                    retained.dormant.pop_front()
                } else {
                    None
                }
            };
            let did_evict = evicted
                .as_ref()
                .is_some_and(|entry| entry.python.is_some() || entry.javascript.is_some());
            (
                Activation {
                    changed,
                    restored,
                    evicted: did_evict,
                },
                evicted,
            )
        };
        // PyKernel's ProcessGuard reaps its owned process tree. JsKernel Drop
        // revokes its worker. Never run either cleanup under a cache/slot lock.
        let _owner = owner.cx().clone().set_current_restricted();
        drop(evicted);
        activation
    }
}

#[cfg(test)]
mod tests;
