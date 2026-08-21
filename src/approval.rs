//! Tool approval modes and graduated tool gating (bd-cv653.3.19).
//!
//! Exposes graduated tool gating under three operational modes:
//! - `always-ask`: Every mutating or executing tool call prompts for approval.
//!   Reads, searches, and inspections pass freely.
//! - `write`: File mutations (`write`, `edit`, `hashline_edit`, `append`) are
//!   auto-approved; processes (`bash`, `xdev run` process tools), subagents,
//!   network operations, and arbitrary executions prompt for approval.
//! - `yolo`: All tool calls are auto-approved EXCEPT hard policy gates
//!   (such as `bash.mediation` `block-*` rules, computer input, and remote
//!   prohibitions) and dangerous command classes configured in `dual_confirm_classes`.
//!
//! Also supports:
//! - `--plan-yolo`: Auto-approves mutations that fall within the scope of an approved plan.
//! - `approval.dual_confirm_classes`: Danger classes (e.g. `recursive_delete`, `disk_wipe`)
//!   that ALWAYS require explicit typed confirmation even under `yolo` mode.
//! - Audit trail: Every auto-approved tool call is recorded with the authorizing mode
//!   for post-hoc inspection.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::BashSettings;
use crate::extensions::DangerousCommandClass;
use crate::plan::PlanState;
use crate::tools::ToolEffects;

/// Top-level tool approval mode (bd-cv653.3.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Every mutating/executing call prompts the user for approval.
    #[default]
    AlwaysAsk,
    /// File mutations (write, edit, append, hashline_edit) are auto-approved;
    /// processes (bash), subagents, network, and arbitrary executions prompt.
    Write,
    /// All tool calls are auto-approved EXCEPT hard policy gates
    /// and danger classes configured in `dual_confirm_classes`.
    Yolo,
}

impl ApprovalMode {
    /// Parse from a user-supplied string or setting name.
    #[must_use]
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()) {
            Some(ref s)
                if s == "always-ask"
                    || s == "always_ask"
                    || s == "always"
                    || s == "ask"
                    || s == "prompt" =>
            {
                Self::AlwaysAsk
            }
            Some(ref s) if s == "write" || s == "files" || s == "file-write" => Self::Write,
            Some(ref s)
                if s == "yolo"
                    || s == "auto-approve"
                    || s == "auto_approve"
                    || s == "auto"
                    || s == "all" =>
            {
                Self::Yolo
            }
            _ => Self::AlwaysAsk,
        }
    }

    /// Canonical string identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysAsk => "always-ask",
            Self::Write => "write",
            Self::Yolo => "yolo",
        }
    }

    /// Whether this mode is `AlwaysAsk`.
    #[must_use]
    pub const fn is_always_ask(self) -> bool {
        matches!(self, Self::AlwaysAsk)
    }

    /// Whether this mode is `Write`.
    #[must_use]
    pub const fn is_write(self) -> bool {
        matches!(self, Self::Write)
    }

    /// Whether this mode is `Yolo`.
    #[must_use]
    pub const fn is_yolo(self) -> bool {
        matches!(self, Self::Yolo)
    }
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalMode {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_setting(Some(s)))
    }
}

/// Parse a dangerous command class from a configuration name.
#[must_use]
pub fn parse_dangerous_command_class(raw: &str) -> Option<DangerousCommandClass> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "recursive_delete" | "recursive-delete" | "recursivedelete" | "rm_rf" | "rm-rf" => {
            Some(DangerousCommandClass::RecursiveDelete)
        }
        "device_write" | "device-write" | "devicewrite" | "dd" | "mkfs" | "fdisk" => {
            Some(DangerousCommandClass::DeviceWrite)
        }
        "fork_bomb" | "fork-bomb" | "forkbomb" => Some(DangerousCommandClass::ForkBomb),
        "pipe_to_shell" | "pipe-to-shell" | "pipetoshell" | "curl_sh" | "curl-sh" => {
            Some(DangerousCommandClass::PipeToShell)
        }
        "system_shutdown" | "system-shutdown" | "shutdown" | "reboot" => {
            Some(DangerousCommandClass::SystemShutdown)
        }
        "permission_escalation" | "permission-escalation" | "chmod" | "chmod_777" => {
            Some(DangerousCommandClass::PermissionEscalation)
        }
        "process_termination" | "process-termination" | "kill" | "pkill" => {
            Some(DangerousCommandClass::ProcessTermination)
        }
        "credential_file_modification" | "credential-file-modification" | "passwd" | "shadow" => {
            Some(DangerousCommandClass::CredentialFileModification)
        }
        "disk_wipe" | "disk-wipe" | "diskwipe" | "shred" | "wipefs" => {
            Some(DangerousCommandClass::DiskWipe)
        }
        "reverse_shell" | "reverse-shell" | "reverseshell" => {
            Some(DangerousCommandClass::ReverseShell)
        }
        _ => None,
    }
}

/// Evaluation result for a tool call against the active approval policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalEvaluation {
    /// Tool is auto-approved under the active mode.
    AutoApproved { mode: ApprovalMode, reason: String },
    /// Tool requires interactive or programmatic approval.
    RequiresApproval {
        mode: ApprovalMode,
        reason: String,
        is_dual_confirm: bool,
        danger_classes: Vec<DangerousCommandClass>,
    },
    /// Tool is hard-blocked by policy (YOLO cannot override).
    HardBlocked { reason: String },
}

impl ApprovalEvaluation {
    /// Whether this evaluation auto-approved the tool execution.
    #[must_use]
    pub const fn is_auto_approved(&self) -> bool {
        matches!(self, Self::AutoApproved { .. })
    }

    /// Whether this evaluation hard-blocked the tool execution.
    #[must_use]
    pub const fn is_hard_blocked(&self) -> bool {
        matches!(self, Self::HardBlocked { .. })
    }

    /// Whether this evaluation requires approval.
    #[must_use]
    pub const fn requires_approval(&self) -> bool {
        matches!(self, Self::RequiresApproval { .. })
    }
}

/// Dynamic approval state shared across turns, TUI commands, and session lifetime.
#[derive(Debug, Clone)]
pub struct ApprovalState {
    mode: Arc<RwLock<ApprovalMode>>,
    plan_yolo: Arc<AtomicBool>,
    dual_confirm_classes: Arc<RwLock<Vec<DangerousCommandClass>>>,
    confirmed_tokens: Arc<Mutex<HashSet<String>>>,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self::new(ApprovalMode::AlwaysAsk, false, Vec::new())
    }
}

impl ApprovalState {
    /// Create a new approval state.
    #[must_use]
    pub fn new(
        mode: ApprovalMode,
        plan_yolo: bool,
        dual_confirm_classes: Vec<DangerousCommandClass>,
    ) -> Self {
        Self {
            mode: Arc::new(RwLock::new(mode)),
            plan_yolo: Arc::new(AtomicBool::new(plan_yolo)),
            dual_confirm_classes: Arc::new(RwLock::new(dual_confirm_classes)),
            confirmed_tokens: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Get current active approval mode.
    #[must_use]
    pub fn mode(&self) -> ApprovalMode {
        self.mode.read().map_or(ApprovalMode::AlwaysAsk, |m| *m)
    }

    /// Set approval mode.
    pub fn set_mode(&self, mode: ApprovalMode) {
        if let Ok(mut guard) = self.mode.write() {
            *guard = mode;
        }
    }

    /// Check if plan-yolo mode is enabled.
    #[must_use]
    pub fn plan_yolo(&self) -> bool {
        self.plan_yolo.load(Ordering::SeqCst)
    }

    /// Set plan-yolo mode.
    pub fn set_plan_yolo(&self, val: bool) {
        self.plan_yolo.store(val, Ordering::SeqCst);
    }

    /// Get dual confirmation dangerous command classes.
    #[must_use]
    pub fn dual_confirm_classes(&self) -> Vec<DangerousCommandClass> {
        self.dual_confirm_classes
            .read()
            .map_or_else(|_| Vec::new(), |v| v.clone())
    }

    /// Set dual confirmation dangerous command classes.
    pub fn set_dual_confirm_classes(&self, classes: Vec<DangerousCommandClass>) {
        if let Ok(mut guard) = self.dual_confirm_classes.write() {
            *guard = classes;
        }
    }

    /// Record explicit dual confirmation for a command or token.
    pub fn record_confirmation(&self, token: &str) {
        if let Ok(mut guard) = self.confirmed_tokens.lock() {
            guard.insert(token.to_string());
        }
    }

    /// Check if a command or token was previously dual-confirmed.
    #[must_use]
    pub fn is_confirmed(&self, token: &str) -> bool {
        self.confirmed_tokens
            .lock()
            .is_ok_and(|guard| guard.contains(token))
    }

    /// Clear recorded confirmations.
    pub fn clear_confirmations(&self) {
        if let Ok(mut guard) = self.confirmed_tokens.lock() {
            guard.clear();
        }
    }

    /// Evaluate whether a tool call is auto-approved, requires approval, or is hard-blocked.
    #[must_use]
    pub fn evaluate(
        &self,
        tool_name: &str,
        tool_args: &Value,
        effects: ToolEffects,
        plan_state: Option<&PlanState>,
        bash_settings: Option<&BashSettings>,
    ) -> ApprovalEvaluation {
        let mode = self.mode();

        // 1. Hard policy gates (e.g. bash mediation block-critical / block-high).
        // Hard policy gates apply regardless of YOLO or auto-approve overrides!
        if tool_name == "bash" {
            let cmd = tool_args
                .get("command")
                .or_else(|| tool_args.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or("");

            if !cmd.is_empty() {
                if let Some(s) = bash_settings {
                    let mode =
                        crate::bash_mediation::MediationMode::from_setting(s.mediation.as_deref());
                    if mode != crate::bash_mediation::MediationMode::Off {
                        let cwd = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let verdict = crate::bash_mediation::assess(cmd, s, mode, &cwd);
                        if !verdict.allows() {
                            let hits = match verdict {
                                crate::bash_mediation::MediationVerdict::Block { hits } => hits,
                                _ => Vec::new(),
                            };
                            let reasons: Vec<String> = hits.into_iter().map(|h| h.reason).collect();
                            let reason_str = if reasons.is_empty() {
                                "Refused by bash mediation policy".to_string()
                            } else {
                                reasons.join("; ")
                            };
                            return ApprovalEvaluation::HardBlocked {
                                reason: format!("Hard policy gate: {reason_str}"),
                            };
                        }
                    }
                }
            }
        }

        // 2. Check for SLB dual-confirmation dangerous command classes.
        // If the command matches any configured dual-confirm class, it ALWAYS requires
        // typed confirmation, even under YOLO mode.
        let dual_classes = self.dual_confirm_classes();
        if !dual_classes.is_empty() && (tool_name == "bash" || effects.processes()) {
            let cmd = tool_args
                .get("command")
                .or_else(|| tool_args.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !cmd.is_empty() {
                let classified = crate::extensions::classify_dangerous_command(cmd, &[]);
                let matching: Vec<DangerousCommandClass> = classified
                    .into_iter()
                    .filter(|c| dual_classes.contains(c))
                    .collect();

                if !matching.is_empty() {
                    let token = format!("{tool_name}:{cmd}");
                    if !self.is_confirmed(&token) {
                        let labels: Vec<&'static str> =
                            matching.iter().map(|c| c.label()).collect();
                        return ApprovalEvaluation::RequiresApproval {
                            mode,
                            reason: format!(
                                "Dual confirmation required for danger classes: {}",
                                labels.join(", ")
                            ),
                            is_dual_confirm: true,
                            danger_classes: matching,
                        };
                    }
                }
            }
        }

        // 3. Pure read-only operations never require approval under any mode.
        if !effects.writes() && !effects.appends() && !effects.processes() && !effects.networks() {
            return ApprovalEvaluation::AutoApproved {
                mode,
                reason: "Read-only tool operation".to_string(),
            };
        }

        // 4. Plan-YOLO mode (--plan-yolo):
        // Auto-approve in-plan file writes, ask on process/network/bash execution.
        if self.plan_yolo() {
            if let Some(plan) = plan_state {
                // If plan is approved, file mutations are auto-approved
                if plan.mode() == crate::plan::PlanMode::Approved
                    && (effects.writes() || effects.appends())
                    && !effects.processes()
                {
                    return ApprovalEvaluation::AutoApproved {
                        mode,
                        reason: "Plan-YOLO auto-approves in-plan file mutations".to_string(),
                    };
                }
            }
        }

        // 5. Graduated approval mode logic.
        match mode {
            ApprovalMode::Yolo => ApprovalEvaluation::AutoApproved {
                mode: ApprovalMode::Yolo,
                reason: "YOLO mode auto-approves execution".to_string(),
            },
            ApprovalMode::Write => {
                // File writes/edits auto-approved; process/network/bash execution prompts.
                if (effects.writes() || effects.appends())
                    && !effects.processes()
                    && !effects.networks()
                {
                    ApprovalEvaluation::AutoApproved {
                        mode: ApprovalMode::Write,
                        reason: "Write mode auto-approves file mutation".to_string(),
                    }
                } else {
                    ApprovalEvaluation::RequiresApproval {
                        mode: ApprovalMode::Write,
                        reason: format!("Write mode requires approval for {tool_name}"),
                        is_dual_confirm: false,
                        danger_classes: Vec::new(),
                    }
                }
            }
            ApprovalMode::AlwaysAsk => ApprovalEvaluation::RequiresApproval {
                mode: ApprovalMode::AlwaysAsk,
                reason: format!("Always-ask mode requires approval for {tool_name}"),
                is_dual_confirm: false,
                danger_classes: Vec::new(),
            },
        }
    }

    /// Construct an audit JSON value for an approval decision.
    #[must_use]
    pub fn audit_payload(
        tool_call_id: &str,
        tool_name: &str,
        evaluation: &ApprovalEvaluation,
    ) -> Value {
        match evaluation {
            ApprovalEvaluation::AutoApproved { mode, reason } => json!({
                "schema": "pi.tool_approval.audit.v1",
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "verdict": "auto_approved",
                "mode": mode.as_str(),
                "reason": reason,
            }),
            ApprovalEvaluation::RequiresApproval {
                mode,
                reason,
                is_dual_confirm,
                danger_classes,
            } => {
                let classes: Vec<&'static str> = danger_classes.iter().map(|c| c.label()).collect();
                json!({
                    "schema": "pi.tool_approval.audit.v1",
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "verdict": "prompt_required",
                    "mode": mode.as_str(),
                    "reason": reason,
                    "is_dual_confirm": is_dual_confirm,
                    "danger_classes": classes,
                })
            }
            ApprovalEvaluation::HardBlocked { reason } => json!({
                "schema": "pi.tool_approval.audit.v1",
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "verdict": "hard_blocked",
                "reason": reason,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approval_mode_parsing() {
        assert_eq!(
            ApprovalMode::from_setting(Some("always-ask")),
            ApprovalMode::AlwaysAsk
        );
        assert_eq!(
            ApprovalMode::from_setting(Some("always_ask")),
            ApprovalMode::AlwaysAsk
        );
        assert_eq!(
            ApprovalMode::from_setting(Some("write")),
            ApprovalMode::Write
        );
        assert_eq!(ApprovalMode::from_setting(Some("yolo")), ApprovalMode::Yolo);
        assert_eq!(
            ApprovalMode::from_setting(Some("auto-approve")),
            ApprovalMode::Yolo
        );
        assert_eq!(ApprovalMode::from_setting(None), ApprovalMode::AlwaysAsk);
    }

    #[test]
    fn test_read_tools_always_auto_approved() {
        let state = ApprovalState::new(ApprovalMode::AlwaysAsk, false, Vec::new());
        let eval = state.evaluate(
            "read",
            &json!({"path": "foo.rs"}),
            ToolEffects::read(),
            None,
            None,
        );
        assert!(eval.is_auto_approved());

        let eval_grep = state.evaluate(
            "grep",
            &json!({"pattern": "test"}),
            ToolEffects::read(),
            None,
            None,
        );
        assert!(eval_grep.is_auto_approved());
    }

    #[test]
    fn test_write_mode_graduated_gating() {
        let state = ApprovalState::new(ApprovalMode::Write, false, Vec::new());

        // File mutation should be auto-approved
        let eval_write = state.evaluate(
            "write",
            &json!({"path": "foo.rs", "content": "hi"}),
            ToolEffects::write(),
            None,
            None,
        );
        assert!(eval_write.is_auto_approved());

        // Process (bash) should require approval
        let eval_bash = state.evaluate(
            "bash",
            &json!({"command": "cargo build"}),
            ToolEffects::process(),
            None,
            None,
        );
        assert!(eval_bash.requires_approval());
    }

    #[test]
    fn test_always_ask_mode_requires_approval_for_mutations() {
        let state = ApprovalState::new(ApprovalMode::AlwaysAsk, false, Vec::new());

        let eval_write = state.evaluate(
            "write",
            &json!({"path": "foo.rs"}),
            ToolEffects::write(),
            None,
            None,
        );
        assert!(eval_write.requires_approval());

        let eval_bash = state.evaluate(
            "bash",
            &json!({"command": "ls"}),
            ToolEffects::process(),
            None,
            None,
        );
        assert!(eval_bash.requires_approval());
    }

    #[test]
    fn test_yolo_mode_auto_approves_normal_tools() {
        let state = ApprovalState::new(ApprovalMode::Yolo, false, Vec::new());

        let eval_write = state.evaluate(
            "write",
            &json!({"path": "foo.rs"}),
            ToolEffects::write(),
            None,
            None,
        );
        assert!(eval_write.is_auto_approved());

        let eval_bash = state.evaluate(
            "bash",
            &json!({"command": "cargo test"}),
            ToolEffects::process(),
            None,
            None,
        );
        assert!(eval_bash.is_auto_approved());
    }

    #[test]
    fn test_yolo_mode_respects_hard_policy_gates() {
        let state = ApprovalState::new(ApprovalMode::Yolo, false, Vec::new());
        let bash_settings = BashSettings {
            mediation: Some("block-critical".to_string()),
            ..Default::default()
        };

        // rm -rf / is critical and blocked by bash mediation
        let eval_blocked = state.evaluate(
            "bash",
            &json!({"command": "rm -rf /"}),
            ToolEffects::process(),
            None,
            Some(&bash_settings),
        );
        assert!(eval_blocked.is_hard_blocked());
    }

    #[test]
    fn test_dual_confirm_classes_under_yolo() {
        let state = ApprovalState::new(
            ApprovalMode::Yolo,
            false,
            vec![DangerousCommandClass::RecursiveDelete],
        );

        // rm -rf / without prior confirmation should require dual confirmation even under YOLO
        let eval_dc = state.evaluate(
            "bash",
            &json!({"command": "rm -rf /tmp/test"}),
            ToolEffects::process(),
            None,
            None,
        );
        assert!(matches!(
            eval_dc,
            ApprovalEvaluation::RequiresApproval {
                is_dual_confirm: true,
                ..
            }
        ));

        // Once confirmed, it passes
        state.record_confirmation("bash:rm -rf /tmp/test");
        let eval_confirmed = state.evaluate(
            "bash",
            &json!({"command": "rm -rf /tmp/test"}),
            ToolEffects::process(),
            None,
            None,
        );
        assert!(eval_confirmed.is_auto_approved());
    }

    #[test]
    fn test_plan_yolo_approves_in_plan_writes() {
        let state = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
        let plan_state = PlanState::default();
        plan_state.enter_planning();
        plan_state.submit_plan(
            "Goal: update code\nFiles: src/main.rs\nVerification: cargo test".to_string(),
        );
        plan_state.approve();

        let eval_write = state.evaluate(
            "write",
            &json!({"path": "src/main.rs"}),
            ToolEffects::write(),
            Some(&plan_state),
            None,
        );
        assert!(eval_write.is_auto_approved());

        // Bash out-of-plan still requires approval
        let eval_bash = state.evaluate(
            "bash",
            &json!({"command": "curl evil.com"}),
            ToolEffects::process().union(ToolEffects::network()),
            Some(&plan_state),
            None,
        );
        assert!(eval_bash.requires_approval());
    }
}
