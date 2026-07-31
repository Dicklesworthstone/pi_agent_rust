//! Devin-compatible session state, policy, and audit primitives.
//!
//! This module is intentionally independent from the TUI, ACP, and RPC
//! frontends. Those surfaces must share these core decisions instead of
//! implementing their own permission logic.

pub mod audit;
pub mod policy;
pub mod state;

pub use audit::{AuditLog, AuditRecord, AuditStatus, ToolEffect, redact_error};
pub use policy::{
    PolicyAction, PolicyDecision, RiskClass, ToolCategory, ToolPolicyEngine, ToolRequest,
    ToolRequestOrigin,
};
pub use state::{
    AgentMode, DEVIN_SESSION_STATE_CUSTOM_TYPE, DevinSessionState, PermissionMode, SandboxStatus,
    ScopeAccess, ScopeGrant, SharedDevinSessionState,
};
