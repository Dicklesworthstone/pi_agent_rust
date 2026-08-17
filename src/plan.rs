//! Plan mode (bd-cv653.3.5): a read-only planning state with an approval gate.
//!
//! While `Planning`, the tool executor rejects any tool whose effects
//! intersect the mutation/process BARRIER set with a structured, model-readable
//! error — reads, searches, and analysis flow freely.
//!
//! The agent ends planning by submitting a structured plan via `submit_plan`;
//! the plan is reviewed (TUI card / `/plan approve|reject` / RPC
//! `approve_plan`), and on approval it becomes a pinned context document for
//! execution turns. `--plan-yolo` / `plan.autoApprove` auto-approves for
//! unattended runs. Every transition is logged as session entries
//! (replay-safe).

use crate::tools::ToolEffects;
use std::sync::{Arc, RwLock};

/// The `submit_plan` tool (bd-cv653.3.5).
///
/// The agent calls this with the full plan to end planning and request
/// review. Session-host-coupled: the shared [`PlanState`] is created by the
/// agent and handed here at registry extension time (like ask/todo).
pub struct SubmitPlanTool {
    state: PlanState,
    auto_approve: bool,
}

impl SubmitPlanTool {
    #[must_use]
    pub const fn new(state: PlanState, auto_approve: bool) -> Self {
        Self {
            state,
            auto_approve,
        }
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::tools::Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        "submit_plan"
    }

    fn label(&self) -> &str {
        "submit_plan"
    }

    fn description(&self) -> &str {
        "Submit a completed plan for user review and exit read-only planning. \
         Call ONLY when the plan is complete: goal, ordered steps, files to \
         touch, and verification. The user approves (execution resumes with \
         the plan pinned as context) or rejects with edits (planning \
         continues). Fails when plan mode is not active."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full plan: goal, ordered steps, files to touch, and how to verify"
                }
            },
            "required": ["plan"]
        })
    }

    fn effects(&self) -> ToolEffects {
        // Records session state only; mutates no files — must pass the
        // plan-mode gate (which it is called under by definition).
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<crate::tools::ToolOutput> {
        let plan = input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if plan.len() < 20 {
            return Ok(crate::tools::ToolOutput {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(
                        "Plan is too short to review — include goal, ordered steps, files to touch, and verification.",
                    ),
                )],
                details: None,
                is_error: true,
            });
        }
        if !self.state.submit_plan(plan.clone()) {
            return Ok(crate::tools::ToolOutput {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(
                        "submit_plan called outside of plan mode. Enter plan mode first (/plan).",
                    ),
                )],
                details: None,
                is_error: true,
            });
        }
        if self.auto_approve {
            // --plan-yolo / plan.autoApprove (bd-cv653.3.5): skip review; the
            // plan rides back in the tool result so execution continues with
            // it in context immediately.
            let _ = self.state.approve();
            return Ok(crate::tools::ToolOutput {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(format!(
                        "Plan auto-approved (plan yolo). Execute it now:\n\n{plan}"
                    )),
                )],
                details: Some(serde_json::json!({"planReview": "auto_approved"})),
                is_error: false,
            });
        }
        Ok(crate::tools::ToolOutput {
            content: vec![crate::model::ContentBlock::Text(
                crate::model::TextContent::new(
                    "Plan submitted for review. Wait for the user's decision: on approval, execute the plan; on rejection, revise it from their feedback.",
                ),
            )],
            details: Some(serde_json::json!({"planReview": "pending"})),
            is_error: false,
        })
    }
}

/// Plan-mode state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode {
    /// Normal operation.
    #[default]
    Off,
    /// Read-only planning; mutations are blocked.
    Planning,
    /// A plan has been submitted and awaits review.
    PendingApproval,
    /// The plan was approved; execution proceeds with the plan pinned.
    Approved,
}

impl PlanMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Planning => "planning",
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
        }
    }
}

/// Shared plan-mode state, held by the agent (for the executor gate) and the
/// `submit_plan` tool (for plan capture).
#[derive(Debug, Clone, Default)]
pub struct PlanState {
    inner: Arc<RwLock<PlanStateInner>>,
}

#[derive(Debug, Default)]
struct PlanStateInner {
    mode: PlanMode,
    plan: Option<String>,
    /// The model the session ran before plan mode took over (restored on
    /// approval when the plan role was active).
    previous_model: Option<(String, String)>,
}

impl PlanState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn mode(&self) -> PlanMode {
        self.inner.read().map_or(PlanMode::Off, |inner| inner.mode)
    }

    /// Enter planning. Returns the previous mode.
    pub fn enter_planning(&self) -> PlanMode {
        let mut inner = self.inner.write().expect("plan state lock");
        let previous = inner.mode;
        inner.mode = PlanMode::Planning;
        previous
    }

    /// Submit a plan for review (called by the submit_plan tool). Returns
    /// false when not planning (the tool reports a usage error).
    pub fn submit_plan(&self, plan: String) -> bool {
        let mut inner = self.inner.write().expect("plan state lock");
        if inner.mode != PlanMode::Planning {
            return false;
        }
        inner.plan = Some(plan);
        inner.mode = PlanMode::PendingApproval;
        true
    }

    /// Approve the pending plan. Returns the plan text on success.
    pub fn approve(&self) -> Option<String> {
        let mut inner = self.inner.write().expect("plan state lock");
        if inner.mode != PlanMode::PendingApproval {
            return None;
        }
        inner.mode = PlanMode::Approved;
        inner.plan.clone()
    }

    /// Reject the pending plan (back to Planning for the edit loop).
    pub fn reject(&self) -> bool {
        let mut inner = self.inner.write().expect("plan state lock");
        if inner.mode != PlanMode::PendingApproval {
            return false;
        }
        inner.mode = PlanMode::Planning;
        true
    }

    /// Leave plan mode entirely (plan text dropped).
    pub fn exit(&self) {
        let mut inner = self.inner.write().expect("plan state lock");
        inner.mode = PlanMode::Off;
        inner.plan = None;
        inner.previous_model = None;
    }

    /// The submitted plan text, if any.
    #[must_use]
    pub fn plan(&self) -> Option<String> {
        self.inner.read().ok().and_then(|inner| inner.plan.clone())
    }

    /// Record the pre-plan-mode model (for restore on approval).
    pub fn stash_previous_model(&self, provider: &str, model_id: &str) {
        let mut inner = self.inner.write().expect("plan state lock");
        inner.previous_model = Some((provider.to_string(), model_id.to_string()));
    }

    /// Take the stashed previous model (on approval).
    pub fn take_previous_model(&self) -> Option<(String, String)> {
        let mut inner = self.inner.write().expect("plan state lock");
        inner.previous_model.take()
    }

    /// The executor gate: whether a tool with these effects may run in the
    /// current mode. Planning/PendingApproval block the mutation/process
    /// BARRIER set (write|append|process); everything else flows.
    #[must_use]
    pub fn allows_effects(&self, effects: ToolEffects) -> bool {
        match self.mode() {
            PlanMode::Off | PlanMode::Approved => true,
            PlanMode::Planning | PlanMode::PendingApproval => {
                !(effects.writes() || effects.appends() || effects.processes())
            }
        }
    }

    /// The structured, model-readable block error for the gate.
    #[must_use]
    pub fn block_message(tool_name: &str) -> String {
        format!(
            "[PLAN_MODE_BLOCKED] Tool {tool_name:?} is unavailable while planning: plan mode is \
             read-only. Use read/grep/find/ls (and xdev run on read-only tools) to inspect; \
             finish by calling submit_plan with the full plan. The user reviews it and execution \
             resumes on approval."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_transitions() {
        let state = PlanState::new();
        assert_eq!(state.mode(), PlanMode::Off);
        state.enter_planning();
        assert_eq!(state.mode(), PlanMode::Planning);

        // Cannot approve before submitting.
        assert!(state.approve().is_none());
        assert!(state.submit_plan("plan A".to_string()));
        assert_eq!(state.mode(), PlanMode::PendingApproval);

        // Reject loops back to Planning; submit again; approve yields the plan.
        assert!(state.reject());
        assert_eq!(state.mode(), PlanMode::Planning);
        assert!(state.submit_plan("plan B".to_string()));
        assert_eq!(state.approve().as_deref(), Some("plan B"));
        assert_eq!(state.mode(), PlanMode::Approved);

        state.exit();
        assert_eq!(state.mode(), PlanMode::Off);
        assert!(state.plan().is_none());
    }

    #[test]
    fn submit_only_works_while_planning() {
        let state = PlanState::new();
        assert!(!state.submit_plan("nope".to_string()));
        state.enter_planning();
        assert!(state.submit_plan("ok".to_string()));
        assert!(!state.submit_plan("twice".to_string())); // pending approval now
    }

    #[test]
    fn gate_blocks_barrier_effects_only_while_planning() {
        let state = PlanState::new();
        assert!(state.allows_effects(ToolEffects::write()));
        state.enter_planning();
        assert!(!state.allows_effects(ToolEffects::write()));
        assert!(!state.allows_effects(ToolEffects::process()));
        assert!(!state.allows_effects(ToolEffects::append()));
        assert!(state.allows_effects(ToolEffects::read()));
        assert!(state.allows_effects(ToolEffects::network()));
        state.submit_plan("p".to_string());
        assert!(!state.allows_effects(ToolEffects::write()));
        state.approve();
        assert!(state.allows_effects(ToolEffects::write()));
    }

    #[test]
    fn previous_model_round_trip() {
        let state = PlanState::new();
        assert!(state.take_previous_model().is_none());
        state.stash_previous_model("anthropic", "claude-opus-4-7");
        assert_eq!(
            state.take_previous_model(),
            Some(("anthropic".to_string(), "claude-opus-4-7".to_string()))
        );
        assert!(state.take_previous_model().is_none());
    }

    #[test]
    fn block_message_is_model_readable() {
        let message = PlanState::block_message("write");
        assert!(message.contains("PLAN_MODE_BLOCKED"));
        assert!(message.contains("submit_plan"));
        assert!(message.contains("\"write\""));
    }
}
