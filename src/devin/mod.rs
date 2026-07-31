//! Devin-compatible session state, policy, and audit primitives.
//!
//! This module is intentionally independent from the TUI, ACP, and RPC
//! frontends. Those surfaces must share these core decisions instead of
//! implementing their own permission logic.

pub mod audit;
pub mod policy;
pub mod process;
pub mod process_tools;
pub mod state;

pub use audit::{AuditLog, AuditRecord, AuditStatus, ToolEffect, redact_error};
pub use policy::{
    PROCESS_WORKING_DIRECTORY_KEYS, PolicyAction, PolicyDecision, RiskClass, ToolCategory,
    ToolPolicyEngine, ToolRequest, ToolRequestOrigin,
};
pub use process::{
    PROCESS_ARTIFACT_FILE_PREFIX, PROCESS_DEFAULT_TIMEOUT, PROCESS_TERMINATE_GRACE, ProcessOutcome,
    ProcessRecord, ProcessStatus, ProcessSupervisor, SharedProcessSupervisor, SpawnRequest,
};
pub use process_tools::{DEVIN_PROCESS_TOOL_NAMES, process_tools, register_process_tools};
pub use state::{
    AgentMode, DEVIN_SESSION_STATE_CUSTOM_TYPE, DevinSessionState, PermissionMode, SandboxStatus,
    ScopeAccess, ScopeGrant, SharedDevinSessionState,
};
