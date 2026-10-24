//! Per-conversation ownership of retained native debugger sessions.
//!
//! The operation lease serializes selection with the entire DAP workflow.
//! Switching conversations parks the old adapter; it does not terminate it.
//! A full registry refuses a new target rather than evicting somebody else's.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{DapSession, DebugTool, lock, tool_err};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::jobs::JobSessionScope;

mod identity;

pub(super) const MAX_DEBUG_SESSIONS: usize = 4;
const MAX_OWNER_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct SessionOwners {
    active: Option<String>,
    /// Only owners with a concrete adapter consume a retained slot.
    parked: BTreeMap<String, Arc<DapSession>>,
}

impl DebugTool {
    /// Registry construction/mounting transfers an owned tool to a new scope.
    /// Shallow snapshots preserve the same tool and must not rebind it here.
    pub(super) fn bind_debug_scope(&mut self, scope: JobSessionScope) {
        self.session_scope = scope;
        let session = self
            .session
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let owners = std::mem::take(
            self.session_owners
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // DAP transport/process cleanup runs without a slot/map mutex held.
        drop(session);
        drop(owners);
    }

    pub(super) async fn debug_owner(&self, owner: &AgentCx) -> Result<String> {
        let id = identity::resolve(&self.session_scope, owner).await?;
        if id.len() > MAX_OWNER_BYTES {
            return Err(tool_err(
                "DAP_SESSION_UNAVAILABLE",
                "agent session identity exceeds 1024 bytes",
            ));
        }
        Ok(id)
    }

    pub(super) async fn require_debug_owner(&self, expected: &str, owner: &AgentCx) -> Result<()> {
        if self.debug_owner(owner).await? != expected {
            return Err(tool_err(
                "DAP_SESSION_CHANGED",
                "agent session changed; request was not retargeted and any admitted effects belong to its original session",
            ));
        }
        Ok(())
    }

    /// Called only while holding the tool's whole-operation lease. The live
    /// slot and its parked siblings always retain their original DapSession,
    /// including breakpoint sets, output and suspension-scoped handle tables.
    pub(super) fn activate_debug_owner(&self, id: &str) {
        let retired = {
            let mut owners = lock(&self.session_owners);
            if owners.active.as_deref() == Some(id) {
                return;
            }
            let mut session = lock(&self.session);
            let previous = session.take();
            let retired = match (owners.active.take(), previous) {
                (Some(previous_id), Some(previous)) => owners.parked.insert(previous_id, previous),
                // An unowned slot is never adopted by a new conversation.
                (None, previous) => previous,
                (Some(_), None) => None,
            };
            *session = owners.parked.remove(id);
            owners.active = Some(id.to_string());
            retired
        };
        drop(retired);
    }

    /// Admission never destroys an adapter to make room. The operation lease
    /// is held until a successful launch has populated the active slot, so
    /// concurrent callers cannot over-reserve the final capacity position.
    pub(super) fn require_debug_capacity(&self) -> Result<()> {
        let owners = lock(&self.session_owners);
        let count = owners.parked.len() + usize::from(lock(&self.session).is_some());
        if count >= MAX_DEBUG_SESSIONS {
            return Err(tool_err(
                "DAP_SESSION_CAPACITY",
                "four debugger sessions are retained by this tool; disconnect or terminate a target from its owning conversation before starting another",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
