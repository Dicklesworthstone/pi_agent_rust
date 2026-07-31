//! Session-scoped Devin modes and access grants.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub const DEVIN_SESSION_STATE_CUSTOM_TYPE: &str = "devin_session_state_v1";

/// Agent behavior profile for the current session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Full coding agent.
    #[default]
    Normal,
    /// Read-only planning agent until `exit_plan_mode` succeeds.
    Plan,
    /// Read-only question-answering agent.
    Ask,
}

/// Permission policy selected for the current session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Prompt for writes, processes, and network access.
    #[default]
    Normal,
    /// Auto-approve workspace edits while still prompting for processes.
    #[serde(rename = "accept-edits", alias = "accept_edits")]
    AcceptEdits,
    /// Risk-sensitive approval mode.
    Smart,
    /// Auto-approve calls that remain inside enforced scopes.
    #[serde(alias = "dangerous", alias = "yolo")]
    Bypass,
    /// Execute process and network calls only through an active OS sandbox.
    Autonomous,
}

/// Availability of the OS-level sandbox required by autonomous mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    #[default]
    Unavailable,
    Available,
    Active,
}

/// Access level granted for a path outside the primary workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeAccess {
    Read,
    Write,
}

impl ScopeAccess {
    #[must_use]
    pub const fn permits(self, requested: Self) -> bool {
        matches!(
            (self, requested),
            (Self::Write, _) | (Self::Read, Self::Read)
        )
    }
}

/// A session-local filesystem scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeGrant {
    pub root: PathBuf,
    pub access: ScopeAccess,
}

/// State shared by every frontend and tool call for one agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevinSessionState {
    pub session_id: String,
    pub parent_agent: Option<String>,
    pub workspace: PathBuf,
    pub agent_mode: AgentMode,
    pub permission_mode: PermissionMode,
    pub sandbox_status: SandboxStatus,
    pub scopes: Vec<ScopeGrant>,
}

impl DevinSessionState {
    #[must_use]
    pub fn new(session_id: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            session_id: session_id.into(),
            parent_agent: None,
            workspace: workspace.into(),
            agent_mode: AgentMode::Normal,
            permission_mode: PermissionMode::Normal,
            sandbox_status: SandboxStatus::Unavailable,
            scopes: Vec::new(),
        }
    }

    /// Select a permission mode, rejecting autonomous mode unless the sandbox
    /// is already active. This keeps the transition fail-closed.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
        if mode == PermissionMode::Autonomous && self.sandbox_status != SandboxStatus::Active {
            return Err("autonomous mode requires an active OS sandbox".to_string());
        }
        self.permission_mode = mode;
        Ok(())
    }

    pub fn grant_scope(&mut self, root: impl Into<PathBuf>, access: ScopeAccess) {
        self.scopes.push(ScopeGrant {
            root: root.into(),
            access,
        });
    }

    #[must_use]
    pub fn scope_permits(&self, path: &Path, access: ScopeAccess) -> bool {
        self.scopes
            .iter()
            .any(|scope| path.starts_with(&scope.root) && scope.access.permits(access))
    }
}

pub type SharedDevinSessionState = Arc<RwLock<DevinSessionState>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomous_mode_requires_active_sandbox() {
        let mut state = DevinSessionState::new("session", "/workspace");
        assert!(
            state
                .set_permission_mode(PermissionMode::Autonomous)
                .is_err()
        );

        state.sandbox_status = SandboxStatus::Active;
        assert!(
            state
                .set_permission_mode(PermissionMode::Autonomous)
                .is_ok()
        );
    }

    #[test]
    fn write_scope_includes_read_access() {
        assert!(ScopeAccess::Write.permits(ScopeAccess::Read));
        assert!(ScopeAccess::Write.permits(ScopeAccess::Write));
        assert!(!ScopeAccess::Read.permits(ScopeAccess::Write));
    }

    #[test]
    fn permission_modes_use_devin_cli_names_and_aliases() {
        assert_eq!(
            serde_json::to_string(&PermissionMode::AcceptEdits).unwrap(),
            "\"accept-edits\""
        );
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"dangerous\"").unwrap(),
            PermissionMode::Bypass
        );
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"yolo\"").unwrap(),
            PermissionMode::Bypass
        );
    }
}
