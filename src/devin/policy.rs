//! Central policy evaluation for every Devin-compatible tool call.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::audit::{AuditLog, AuditRecord, AuditStatus, ToolEffect, argument_hash};
use super::state::{
    AgentMode, PermissionMode, SandboxStatus, ScopeAccess, SharedDevinSessionState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRequestOrigin {
    Native,
    Acp,
    Rpc,
    CloudXml,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub origin: ToolRequestOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Read,
    FileMutation,
    Process,
    Network,
    Planning,
    SessionState,
    External,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
    Sandbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub action: PolicyAction,
    pub category: ToolCategory,
    pub risk: RiskClass,
    pub reason: String,
    pub scoped_paths: Vec<PathBuf>,
}

impl PolicyDecision {
    fn deny(category: ToolCategory, risk: RiskClass, reason: impl Into<String>) -> Self {
        Self {
            action: PolicyAction::Deny,
            category,
            risk,
            reason: reason.into(),
            scoped_paths: Vec::new(),
        }
    }
}

/// Session-bound policy engine shared by TUI, ACP, RPC, and native tool calls.
#[derive(Debug, Clone)]
pub struct ToolPolicyEngine {
    state: SharedDevinSessionState,
    audit: Option<Arc<AuditLog>>,
}

impl ToolPolicyEngine {
    #[must_use]
    pub fn new(state: SharedDevinSessionState) -> Self {
        Self { state, audit: None }
    }

    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    #[must_use]
    pub fn state(&self) -> &SharedDevinSessionState {
        &self.state
    }

    #[must_use]
    pub fn evaluate(&self, request: &ToolRequest) -> PolicyDecision {
        let state = self.state.read().unwrap_or_else(|err| err.into_inner());
        let category = classify_tool(&request.tool_name);
        let risk = classify_risk(category, &request.tool_name);

        let mut decision = if !request.arguments.is_object() {
            PolicyDecision::deny(category, risk, "tool arguments must be a JSON object")
        } else if request.tool_name.trim().is_empty() {
            PolicyDecision::deny(category, risk, "tool name must not be empty")
        } else if !agent_mode_allows(state.agent_mode, &request.tool_name, category) {
            PolicyDecision::deny(
                category,
                risk,
                format!(
                    "{} mode does not permit `{}`",
                    agent_mode_name(state.agent_mode),
                    request.tool_name
                ),
            )
        } else {
            match validate_paths(&state, request, category) {
                Ok(scoped_paths) => {
                    let (action, reason) = permission_decision(
                        state.permission_mode,
                        state.sandbox_status,
                        category,
                        &request.tool_name,
                    );
                    PolicyDecision {
                        action,
                        category,
                        risk,
                        reason,
                        scoped_paths,
                    }
                }
                Err(reason) => PolicyDecision::deny(category, risk, reason),
            }
        };

        if request.origin == ToolRequestOrigin::CloudXml {
            decision.reason = format!("cloud XML compatibility: {}", decision.reason);
        }
        self.audit_decision(&state, request, &decision);
        decision
    }

    fn audit_decision(
        &self,
        state: &super::state::DevinSessionState,
        request: &ToolRequest,
        decision: &PolicyDecision,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let now = Utc::now();
        audit.push(AuditRecord {
            call_id: request.call_id.clone(),
            session_id: state.session_id.clone(),
            parent_agent: state.parent_agent.clone(),
            tool_name: request.tool_name.clone(),
            argument_hash: argument_hash(&request.arguments),
            effects: effects_for(decision.category),
            risk: decision.risk,
            policy_action: decision.action,
            approval_source: None,
            started_at: now,
            ended_at: Some(now),
            status: match decision.action {
                PolicyAction::Allow => AuditStatus::Allowed,
                PolicyAction::Deny => AuditStatus::Denied,
                PolicyAction::Ask | PolicyAction::Sandbox => AuditStatus::Pending,
            },
            artifact_refs: Vec::new(),
            redacted_error: None,
        });
    }
}

#[must_use]
pub fn classify_tool(name: &str) -> ToolCategory {
    match name {
        "read" | "grep" | "find" | "find_file_by_name" | "ls" | "notebook_read"
        | "get_output" | "read_subagent" | "mcp_list_servers" | "mcp_list_tools" => {
            ToolCategory::Read
        }
        "write" | "edit" | "apply_patch" | "hashline_edit" | "notebook_edit" => {
            ToolCategory::FileMutation
        }
        "bash" | "exec" | "shell_command" | "kill_shell" | "write_to_process" => {
            ToolCategory::Process
        }
        "web_search" | "webfetch" | "mcp_call_tool" | "mcp_read_resource" => {
            ToolCategory::Network
        }
        "update_plan" | "todo_write" | "exit_plan_mode" => ToolCategory::Planning,
        "ask_user_question" | "request_scope" => ToolCategory::SessionState,
        "run_subagent" | "skill" | "cloud_handoff" => ToolCategory::External,
        _ => ToolCategory::Unknown,
    }
}

fn classify_risk(category: ToolCategory, name: &str) -> RiskClass {
    match category {
        ToolCategory::Read | ToolCategory::Planning => RiskClass::Low,
        ToolCategory::SessionState => RiskClass::Medium,
        ToolCategory::FileMutation | ToolCategory::Network => RiskClass::High,
        ToolCategory::Process | ToolCategory::External | ToolCategory::Unknown => {
            if matches!(name, "cloud_handoff" | "run_subagent") {
                RiskClass::High
            } else {
                RiskClass::Critical
            }
        }
    }
}

fn agent_mode_allows(mode: AgentMode, name: &str, category: ToolCategory) -> bool {
    match mode {
        AgentMode::Normal => true,
        AgentMode::Plan => matches!(
            name,
            "read"
                | "grep"
                | "find"
                | "find_file_by_name"
                | "ls"
                | "notebook_read"
                | "update_plan"
                | "todo_write"
                | "ask_user_question"
                | "exit_plan_mode"
        ),
        AgentMode::Ask => {
            category == ToolCategory::Read
                || matches!(name, "todo_write" | "ask_user_question")
        }
    }
}

const fn agent_mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Normal => "normal",
        AgentMode::Plan => "plan",
        AgentMode::Ask => "ask",
    }
}

fn permission_decision(
    mode: PermissionMode,
    sandbox: SandboxStatus,
    category: ToolCategory,
    name: &str,
) -> (PolicyAction, String) {
    if category == ToolCategory::Unknown {
        return (
            PolicyAction::Ask,
            "unknown tools require explicit approval".to_string(),
        );
    }
    if name == "request_scope" {
        return (
            PolicyAction::Ask,
            "expanding session scope requires explicit approval".to_string(),
        );
    }
    if matches!(
        category,
        ToolCategory::Read | ToolCategory::Planning | ToolCategory::SessionState
    ) {
        return (
            PolicyAction::Allow,
            "read-only or session-local operation".to_string(),
        );
    }

    match mode {
        PermissionMode::Normal => (
            PolicyAction::Ask,
            "normal mode requires approval for side effects".to_string(),
        ),
        PermissionMode::AcceptEdits if category == ToolCategory::FileMutation => (
            PolicyAction::Allow,
            "workspace edit allowed by accept-edits mode".to_string(),
        ),
        PermissionMode::AcceptEdits => (
            PolicyAction::Ask,
            "accept-edits mode still requires approval for non-file effects".to_string(),
        ),
        PermissionMode::Smart => (
            PolicyAction::Ask,
            "smart mode requires a risk-aware approval".to_string(),
        ),
        PermissionMode::Bypass => (
            PolicyAction::Allow,
            "bypass mode allows calls inside enforced scopes".to_string(),
        ),
        PermissionMode::Autonomous if sandbox != SandboxStatus::Active => (
            PolicyAction::Deny,
            "autonomous mode requires an active OS sandbox".to_string(),
        ),
        PermissionMode::Autonomous if category == ToolCategory::FileMutation => (
            PolicyAction::Ask,
            "direct file tools execute outside the sandbox and require approval".to_string(),
        ),
        PermissionMode::Autonomous
            if matches!(category, ToolCategory::Process | ToolCategory::Network) =>
        {
            (
                PolicyAction::Sandbox,
                "operation must execute through the active OS sandbox".to_string(),
            )
        }
        PermissionMode::Autonomous => (
            PolicyAction::Ask,
            format!("autonomous mode requires approval for `{name}`"),
        ),
    }
}

fn validate_paths(
    state: &super::state::DevinSessionState,
    request: &ToolRequest,
    category: ToolCategory,
) -> Result<Vec<PathBuf>, String> {
    if request.tool_name == "request_scope" {
        return Ok(Vec::new());
    }
    let access = match category {
        ToolCategory::FileMutation => ScopeAccess::Write,
        ToolCategory::Read => ScopeAccess::Read,
        _ => return Ok(Vec::new()),
    };
    let Some(arguments) = request.arguments.as_object() else {
        return Err("tool arguments must be a JSON object".to_string());
    };

    let mut scoped = Vec::new();
    for key in ["file_path", "path", "notebook_path"] {
        let Some(raw_path) = arguments.get(key).and_then(Value::as_str) else {
            continue;
        };
        let path = resolve_scoped_path(state, raw_path, access)?;
        scoped.push(path);
    }
    Ok(scoped)
}

fn resolve_scoped_path(
    state: &super::state::DevinSessionState,
    raw_path: &str,
    access: ScopeAccess,
) -> Result<PathBuf, String> {
    let supplied = Path::new(raw_path);
    if supplied
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(format!("path traversal is not allowed: `{raw_path}`"));
    }

    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        state.workspace.join(supplied)
    };
    let resolved = canonicalize_allow_missing(&candidate)?;
    let workspace = canonicalize_allow_missing(&state.workspace)?;
    if resolved.starts_with(&workspace) {
        return Ok(resolved);
    }

    for scope in &state.scopes {
        let scope_root = canonicalize_allow_missing(&scope.root)?;
        if resolved.starts_with(scope_root) && scope.access.permits(access) {
            return Ok(resolved);
        }
    }
    Err(format!(
        "path `{}` is outside the allowed workspace and scopes",
        candidate.display()
    ))
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        if !existing.pop() {
            break;
        }
    }

    let mut resolved = existing
        .canonicalize()
        .map_err(|err| format!("cannot resolve scoped path `{}`: {err}", path.display()))?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn effects_for(category: ToolCategory) -> Vec<ToolEffect> {
    match category {
        ToolCategory::Read => vec![ToolEffect::Read],
        ToolCategory::FileMutation => vec![ToolEffect::Write],
        ToolCategory::Process => vec![ToolEffect::Process],
        ToolCategory::Network => vec![ToolEffect::Network],
        ToolCategory::Planning | ToolCategory::SessionState => vec![ToolEffect::SessionState],
        ToolCategory::External | ToolCategory::Unknown => vec![ToolEffect::External],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::state::DevinSessionState;
    use serde_json::json;
    use std::fs;
    use std::sync::RwLock;

    fn engine(
        workspace: &Path,
        agent_mode: AgentMode,
        permission_mode: PermissionMode,
    ) -> ToolPolicyEngine {
        let mut state = DevinSessionState::new("session", workspace);
        state.agent_mode = agent_mode;
        state.permission_mode = permission_mode;
        ToolPolicyEngine::new(Arc::new(RwLock::new(state)))
    }

    fn request(name: &str, arguments: Value) -> ToolRequest {
        ToolRequest {
            call_id: "call-1".to_string(),
            tool_name: name.to_string(),
            arguments,
            origin: ToolRequestOrigin::Native,
        }
    }

    #[test]
    fn plan_mode_blocks_writes_and_processes() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = engine(
            workspace.path(),
            AgentMode::Plan,
            PermissionMode::Bypass,
        );
        assert_eq!(
            policy
                .evaluate(&request(
                    "write",
                    json!({"file_path": workspace.path().join("x").display().to_string()})
                ))
                .action,
            PolicyAction::Deny
        );
        assert_eq!(
            policy
                .evaluate(&request("exec", json!({"command": "true"})))
                .action,
            PolicyAction::Deny
        );
        assert_eq!(
            policy
                .evaluate(&request("exit_plan_mode", json!({})))
                .action,
            PolicyAction::Allow
        );
    }

    #[test]
    fn traversal_and_symlink_escape_are_denied() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy = engine(
            workspace.path(),
            AgentMode::Normal,
            PermissionMode::AcceptEdits,
        );

        assert_eq!(
            policy
                .evaluate(&request("write", json!({"file_path": "../escape"})))
                .action,
            PolicyAction::Deny
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
            assert_eq!(
                policy
                    .evaluate(&request(
                        "write",
                        json!({"file_path": workspace.path().join("link/file").display().to_string()})
                    ))
                    .action,
                PolicyAction::Deny
            );
        }
    }

    #[test]
    fn explicit_write_scope_allows_external_path() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut state = DevinSessionState::new("session", workspace.path());
        state.permission_mode = PermissionMode::AcceptEdits;
        state.grant_scope(outside.path(), ScopeAccess::Write);
        let policy = ToolPolicyEngine::new(Arc::new(RwLock::new(state)));
        let target = outside.path().join("new.txt");
        assert_eq!(
            policy
                .evaluate(&request(
                    "write",
                    json!({"file_path": target.display().to_string()})
                ))
                .action,
            PolicyAction::Allow
        );
    }

    #[test]
    fn autonomous_processes_fail_closed_without_sandbox() {
        let workspace = tempfile::tempdir().unwrap();
        let mut state = DevinSessionState::new("session", workspace.path());
        state.permission_mode = PermissionMode::Autonomous;
        let policy = ToolPolicyEngine::new(Arc::new(RwLock::new(state)));
        assert_eq!(
            policy
                .evaluate(&request("exec", json!({"command": "true"})))
                .action,
            PolicyAction::Deny
        );
    }

    #[test]
    fn audit_records_hash_but_not_arguments() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "ok").unwrap();
        let state = Arc::new(RwLock::new(DevinSessionState::new(
            "session",
            workspace.path(),
        )));
        let audit = Arc::new(AuditLog::new(8));
        let policy = ToolPolicyEngine::new(state).with_audit(Arc::clone(&audit));
        policy.evaluate(&request(
            "read",
            json!({"file_path": workspace.path().join("file").display().to_string()}),
        ));

        let records = audit.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].argument_hash.len(), 64);
        assert!(!serde_json::to_string(&records[0]).unwrap().contains("file_path"));
    }
}
