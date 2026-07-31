//! The five pinned Devin process tools, backed by one session-owned
//! [`ProcessSupervisor`].
//!
//! Each tool is a thin adapter: it validates its arguments, runs the shared
//! policy gate, drives the supervisor, and closes the audit record for the
//! call. There is no second subprocess stack and no second permission path.
//!
//! Every adapter calls [`ToolPolicyEngine::evaluate`] itself rather than
//! relying on the agent loop, because the tool registry is also driven by the
//! ACP and RPC surfaces. Re-evaluating an in-flight call is safe: the audit log
//! keys records by `call_id` and upserts, so a call never produces two rows.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::audit::{AuditLog, AuditStatus, redact_error};
use super::policy::{
    PolicyAction, PolicyDecision, ToolPolicyEngine, ToolRequest, ToolRequestOrigin,
};
use super::process::{
    PROCESS_DEFAULT_TIMEOUT, ProcessOutcome, ProcessStatus, SharedProcessSupervisor, SpawnRequest,
};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolRegistry, ToolUpdate};

/// Stable schema id for the structured details every process tool returns.
pub const PROCESS_TOOL_DETAILS_SCHEMA_V1: &str = "pi.devin.process.result.v1";

/// The pinned Devin process tool names this module implements.
pub const DEVIN_PROCESS_TOOL_NAMES: [&str; 5] = [
    "exec",
    "shell_command",
    "get_output",
    "write_to_process",
    "kill_shell",
];

/// Shared wiring handed to each adapter.
#[derive(Clone)]
struct ProcessToolContext {
    supervisor: SharedProcessSupervisor,
    policy: Arc<ToolPolicyEngine>,
    audit: Option<Arc<AuditLog>>,
}

impl ProcessToolContext {
    /// Run the shared policy gate and open/refresh the audit record.
    ///
    /// Returns `Err(output)` when the call must not execute.
    fn admit(
        &self,
        tool_name: &str,
        call_id: &str,
        arguments: &Value,
    ) -> std::result::Result<PolicyDecision, ToolOutput> {
        let decision = self.policy.evaluate(&ToolRequest {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments: arguments.clone(),
            origin: ToolRequestOrigin::Native,
        });

        match decision.action {
            PolicyAction::Allow => {
                if let Some(audit) = &self.audit {
                    audit.mark_allowed(call_id, Some("policy"));
                }
                Ok(decision)
            }
            PolicyAction::Deny => Err(self.close_denied(call_id, &decision.reason)),
            // The agent loop resolves `Ask` before dispatch. Reaching a tool
            // with an unresolved `Ask` means no approval surface answered, so
            // the call fails closed instead of silently executing.
            PolicyAction::Ask => Err(self.close_denied(
                call_id,
                &format!(
                    "`{tool_name}` requires approval and no approval was recorded: {}",
                    decision.reason
                ),
            )),
            PolicyAction::Sandbox => Err(self.close_denied(
                call_id,
                "no sandbox execution adapter is configured; refusing to run unsandboxed",
            )),
        }
    }

    fn close_denied(&self, call_id: &str, reason: &str) -> ToolOutput {
        if let Some(audit) = &self.audit {
            audit.complete(
                call_id,
                AuditStatus::Denied,
                &[],
                Some(&redact_error(reason)),
            );
        }
        error_output(reason)
    }

    fn close_failed(&self, call_id: &str, error: &Error) -> ToolOutput {
        let message = error.to_string();
        if let Some(audit) = &self.audit {
            audit.complete(
                call_id,
                AuditStatus::Failed,
                &[],
                Some(&redact_error(&message)),
            );
        }
        error_output(&message)
    }

    fn close_outcome(&self, call_id: &str, outcome: &ProcessOutcome) {
        let Some(audit) = &self.audit else {
            return;
        };
        let status = match outcome.record.status {
            ProcessStatus::Running => AuditStatus::Allowed,
            ProcessStatus::TimedOut => AuditStatus::TimedOut,
            ProcessStatus::Cancelled | ProcessStatus::Killed => AuditStatus::Cancelled,
            ProcessStatus::Exited if outcome.record.exit_code == Some(0) => AuditStatus::Succeeded,
            ProcessStatus::Exited => AuditStatus::Failed,
        };
        if status == AuditStatus::Allowed {
            audit.mark_allowed(call_id, Some("policy"));
            return;
        }
        audit.complete(call_id, status, &outcome.artifact_refs(), None);
    }

    fn close_succeeded(&self, call_id: &str, artifact_refs: &[String]) {
        if let Some(audit) = &self.audit {
            audit.complete(call_id, AuditStatus::Succeeded, artifact_refs, None);
        }
    }

    /// Workspace root used when a call omits an explicit working directory.
    fn default_cwd(&self) -> PathBuf {
        self.policy
            .state()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .workspace
            .clone()
    }
}

fn error_output(message: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(message.to_string()))],
        details: Some(json!({
            "schema": PROCESS_TOOL_DETAILS_SCHEMA_V1,
            "status": "rejected",
        })),
        is_error: true,
    }
}

fn outcome_output(outcome: &ProcessOutcome) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(outcome.output.clone()))],
        details: Some(json!({
            "schema": PROCESS_TOOL_DETAILS_SCHEMA_V1,
            "process": outcome.record,
            "truncated": outcome.truncated,
        })),
        is_error: outcome.is_error(),
    }
}

// ============================================================================
// Argument shapes
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    detached: bool,
    #[serde(default)]
    interactive: Option<bool>,
    #[serde(default)]
    shell: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetOutputInput {
    process_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteToProcessInput {
    process_id: String,
    data: String,
    #[serde(default)]
    append_newline: bool,
    #[serde(default)]
    close_stdin: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KillShellInput {
    process_id: String,
    #[serde(default)]
    grace_ms: Option<u64>,
}

fn run_schema(with_shell: bool) -> Value {
    let mut properties = json!({
        "command": {
            "type": "string",
            "description": "Command line to execute."
        },
        "cwd": {
            "type": "string",
            "description": "Working directory. Must resolve inside the session workspace or a granted scope. Defaults to the workspace root."
        },
        "timeout_ms": {
            "type": "integer",
            "minimum": 0,
            "description": "Foreground timeout in milliseconds. 0 disables the timeout. Ignored for background processes. Defaults to 120000."
        },
        "background": {
            "type": "boolean",
            "description": "Return immediately with a process id instead of waiting. Read output with get_output."
        },
        "detached": {
            "type": "boolean",
            "description": "Exempt this process from session cleanup. Explicit opt-in; recorded on the process registry entry."
        },
        "interactive": {
            "type": "boolean",
            "description": "Keep stdin open so write_to_process can write to it. When false stdin is closed immediately after spawn. Defaults to true for background processes and false in the foreground."
        }
    });
    if with_shell && let Some(map) = properties.as_object_mut() {
        map.insert(
            "shell".to_string(),
            json!({
                "type": "string",
                "description": "Absolute path to the shell used to interpret the command. Defaults to bash, falling back to sh."
            }),
        );
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["command"],
        "additionalProperties": false
    })
}

// ============================================================================
// exec / shell_command
// ============================================================================

/// `exec` and `shell_command` share one implementation; they differ only in the
/// tool name reported to the model and in whether an explicit shell may be
/// selected.
pub struct RunProcessTool {
    name: &'static str,
    description: &'static str,
    allow_shell_override: bool,
    context: ProcessToolContext,
}

impl RunProcessTool {
    fn parse(&self, input: Value) -> Result<(RunInput, SpawnRequest)> {
        let parsed: RunInput =
            serde_json::from_value(input).map_err(|err| Error::validation(err.to_string()))?;
        if parsed.command.trim().is_empty() {
            return Err(Error::validation("`command` must not be empty"));
        }
        if parsed.shell.is_some() && !self.allow_shell_override {
            return Err(Error::validation(format!(
                "`{}` does not accept a `shell` override; use `shell_command`",
                self.name
            )));
        }

        let cwd = parsed.cwd.as_ref().map_or_else(
            || self.context.default_cwd(),
            |raw| {
                let candidate = PathBuf::from(raw);
                if candidate.is_absolute() {
                    candidate
                } else {
                    self.context.default_cwd().join(candidate)
                }
            },
        );

        let timeout = match parsed.timeout_ms {
            None => Some(PROCESS_DEFAULT_TIMEOUT),
            Some(0) => None,
            Some(millis) => Some(Duration::from_millis(millis)),
        };

        let request = SpawnRequest {
            command: parsed.command.clone(),
            cwd,
            shell: parsed.shell.clone(),
            timeout,
            background: parsed.background,
            detached: parsed.detached,
            // A background process is usually meant to be fed later, so stdin
            // defaults to open there and closed in the foreground. An explicit
            // `interactive` always wins over that default.
            interactive: parsed.interactive.unwrap_or(parsed.background),
            tool_name: self.name.to_string(),
        };
        Ok((parsed, request))
    }
}

#[async_trait]
impl Tool for RunProcessTool {
    fn name(&self) -> &str {
        self.name
    }

    fn label(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> Value {
        run_schema(self.allow_shell_override)
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process().union(ToolEffects::write())
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let arguments = input.clone();
        let (parsed, request) = match self.parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
        };

        // Policy sees the resolved working directory so containment is checked
        // against what will actually be executed, not against what was typed.
        let mut policy_arguments = arguments;
        if let Some(map) = policy_arguments.as_object_mut() {
            map.insert(
                "cwd".to_string(),
                Value::String(request.cwd.display().to_string()),
            );
        }
        let decision = match self
            .context
            .admit(self.name, tool_call_id, &policy_arguments)
        {
            Ok(decision) => decision,
            Err(output) => return Ok(output),
        };
        debug_assert_eq!(decision.action, PolicyAction::Allow);

        let outcome = if parsed.background {
            match self.context.supervisor.start_background(request) {
                Ok(outcome) => outcome,
                Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
            }
        } else {
            match self
                .context
                .supervisor
                .run_foreground(request, on_update.as_deref())
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
            }
        };

        self.context.close_outcome(tool_call_id, &outcome);
        Ok(outcome_output(&outcome))
    }
}

// ============================================================================
// get_output
// ============================================================================

pub struct GetOutputTool {
    context: ProcessToolContext,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for GetOutputTool {
    fn name(&self) -> &str {
        "get_output"
    }

    fn label(&self) -> &str {
        "get_output"
    }

    fn description(&self) -> &str {
        "Read output produced by a supervised process since the previous call. Returns the process status, exit code, and whether any buffered output was dropped."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "Process id returned by exec or shell_command."
                }
            },
            "required": ["process_id"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let arguments = input.clone();
        let parsed: GetOutputInput = match serde_json::from_value(input) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Ok(self
                    .context
                    .close_failed(tool_call_id, &Error::validation(err.to_string())));
            }
        };
        if let Err(output) = self.context.admit("get_output", tool_call_id, &arguments) {
            return Ok(output);
        }

        let (record, text, missed) = match self.context.supervisor.get_output(&parsed.process_id) {
            Ok(result) => result,
            Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
        };

        let mut body = if text.is_empty() {
            "(no new output)".to_string()
        } else {
            text
        };
        if missed > 0 {
            let artifact = record.artifact_path.as_ref().map_or_else(
                || " No artifact was written.".to_string(),
                |path| format!(" Full output artifact: {}", path.display()),
            );
            let _ = write!(
                body,
                "\n\n[{missed} bytes were dropped from the in-memory buffer before this read.{artifact}]"
            );
        }

        let artifact_refs = record
            .artifact_path
            .as_ref()
            .map_or_else(Vec::new, |path| vec![format!("file://{}", path.display())]);
        self.context.close_succeeded(tool_call_id, &artifact_refs);

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(body))],
            details: Some(json!({
                "schema": PROCESS_TOOL_DETAILS_SCHEMA_V1,
                "process": record,
                "droppedBytes": missed,
            })),
            is_error: false,
        })
    }
}

// ============================================================================
// write_to_process
// ============================================================================

pub struct WriteToProcessTool {
    context: ProcessToolContext,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for WriteToProcessTool {
    fn name(&self) -> &str {
        "write_to_process"
    }

    fn label(&self) -> &str {
        "write_to_process"
    }

    fn description(&self) -> &str {
        "Write to the stdin of a running supervised process. Fails when the process id is unknown, the process has exited, or its stdin is closed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "Process id returned by exec or shell_command."
                },
                "data": {
                    "type": "string",
                    "description": "Bytes to write to stdin."
                },
                "append_newline": {
                    "type": "boolean",
                    "description": "Append a trailing newline, which most line-oriented programs require before they act."
                },
                "close_stdin": {
                    "type": "boolean",
                    "description": "Close stdin after writing so the process observes EOF."
                }
            },
            "required": ["process_id", "data"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let arguments = input.clone();
        let parsed: WriteToProcessInput = match serde_json::from_value(input) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Ok(self
                    .context
                    .close_failed(tool_call_id, &Error::validation(err.to_string())));
            }
        };
        if let Err(output) = self
            .context
            .admit("write_to_process", tool_call_id, &arguments)
        {
            return Ok(output);
        }

        let mut data = parsed.data;
        if parsed.append_newline && !data.ends_with('\n') {
            data.push('\n');
        }

        let record = match self.context.supervisor.write_to_process(
            &parsed.process_id,
            &data,
            parsed.close_stdin,
        ) {
            Ok(record) => record,
            Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
        };

        self.context.close_succeeded(tool_call_id, &[]);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Wrote {} bytes to `{}` stdin.{}",
                data.len(),
                parsed.process_id,
                if parsed.close_stdin {
                    " stdin is now closed."
                } else {
                    ""
                }
            )))],
            details: Some(json!({
                "schema": PROCESS_TOOL_DETAILS_SCHEMA_V1,
                "process": record,
                "bytesWritten": data.len(),
            })),
            is_error: false,
        })
    }
}

// ============================================================================
// kill_shell
// ============================================================================

pub struct KillShellTool {
    context: ProcessToolContext,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "kill_shell"
    }

    fn label(&self) -> &str {
        "kill_shell"
    }

    fn description(&self) -> &str {
        "Terminate a supervised process and its entire process group: SIGTERM first, then SIGKILL after a short grace period."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "Process id returned by exec or shell_command."
                },
                "grace_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Milliseconds to wait after SIGTERM before SIGKILL. Defaults to 1500."
                }
            },
            "required": ["process_id"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let arguments = input.clone();
        let parsed: KillShellInput = match serde_json::from_value(input) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Ok(self
                    .context
                    .close_failed(tool_call_id, &Error::validation(err.to_string())));
            }
        };
        if let Err(output) = self.context.admit("kill_shell", tool_call_id, &arguments) {
            return Ok(output);
        }

        let record = match self
            .context
            .supervisor
            .kill(
                &parsed.process_id,
                parsed.grace_ms.map(Duration::from_millis),
            )
            .await
        {
            Ok(record) => record,
            Err(err) => return Ok(self.context.close_failed(tool_call_id, &err)),
        };

        self.context.close_succeeded(tool_call_id, &[]);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Process `{}` is {} (exit code {}). Its process group was terminated.",
                record.id,
                record.status.as_str(),
                record
                    .exit_code
                    .map_or_else(|| "unknown".to_string(), |code| code.to_string())
            )))],
            details: Some(json!({
                "schema": PROCESS_TOOL_DETAILS_SCHEMA_V1,
                "process": record,
            })),
            is_error: false,
        })
    }
}

// ============================================================================
// Registration
// ============================================================================

/// Build the five pinned Devin process tools over one shared supervisor.
#[must_use]
pub fn process_tools(
    supervisor: SharedProcessSupervisor,
    policy: Arc<ToolPolicyEngine>,
    audit: Option<Arc<AuditLog>>,
) -> Vec<Box<dyn Tool>> {
    let context = ProcessToolContext {
        supervisor,
        policy,
        audit,
    };
    vec![
        Box::new(RunProcessTool {
            name: "exec",
            description: "Execute a command in the session workspace under the process supervisor. Streams output while it runs, or returns a process id immediately when `background` is set.",
            allow_shell_override: false,
            context: context.clone(),
        }),
        Box::new(RunProcessTool {
            name: "shell_command",
            description: "Execute a command through an explicit shell under the process supervisor. Streams output while it runs, or returns a process id immediately when `background` is set.",
            allow_shell_override: true,
            context: context.clone(),
        }),
        Box::new(GetOutputTool {
            context: context.clone(),
        }),
        Box::new(WriteToProcessTool {
            context: context.clone(),
        }),
        Box::new(KillShellTool { context }),
    ]
}

/// Register the five pinned Devin process tools on an existing registry.
pub fn register_process_tools(
    registry: &mut ToolRegistry,
    supervisor: SharedProcessSupervisor,
    policy: Arc<ToolPolicyEngine>,
    audit: Option<Arc<AuditLog>>,
) {
    registry.extend(process_tools(supervisor, policy, audit));
}
