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

mod session;
pub use session::{PlanChange, PlanPersistence, SessionPlanReview};

/// Maximum UTF-8 bytes retained for one submitted plan. The tool checks this
/// before copying model input; direct state callers use the same bound.
pub const MAX_PLAN_BYTES: usize = 256 * 1024;

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
                    "maxLength": MAX_PLAN_BYTES,
                    "description": "The full plan (at most 256 KiB of UTF-8): goal, ordered steps, and verification. Specify files with the optional files array or one top-level Files: line, not both. Only scoped single-file writes can inherit --plan-yolo approval; other tool policies still apply."
                },
                "files": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 128,
                    "items": {"type": "string", "maxLength": 1024},
                    "description": "Relative file scopes appended visibly to the reviewed plan. Exact paths match only that file; a trailing / grants a directory tree; * matches within a component; a whole ** component is recursive. Use this array for names containing spaces or commas. Parent traversal, absolute paths, backslashes, unsupported globs and ambiguous names are not admitted. Paths are limited to 1024 UTF-8 bytes and 64 components."
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
            .trim();
        if plan.len() > MAX_PLAN_BYTES {
            return Ok(crate::tools::ToolOutput {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(format!(
                        "Plan exceeds the {MAX_PLAN_BYTES}-byte UTF-8 limit. Shorten it and submit again; no plan state was changed."
                    )),
                )],
                details: Some(serde_json::json!({"planReview": "too_large"})),
                is_error: true,
            });
        }
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
        let plan = match input.get("files") {
            Some(files) => match crate::approval::append_files_declaration(plan, files) {
                Ok(text) => std::borrow::Cow::Owned(text),
                Err(message) => {
                    return Ok(crate::tools::ToolOutput {
                        content: vec![crate::model::ContentBlock::Text(
                            crate::model::TextContent::new(format!(
                                "Invalid plan file scope: {message}. No plan state was changed."
                            )),
                        )],
                        details: Some(serde_json::json!({"planReview": "invalid_scope"})),
                        is_error: true,
                    });
                }
            },
            None => std::borrow::Cow::Borrowed(plan),
        };
        // Submission and configured auto-approval are one state transition.
        // A separate approve() could authorize another submitter's plan after
        // a concurrent rejection/re-entry, or report success after exit().
        if !self.state.submit(plan.to_string(), self.auto_approve) {
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

/// An immutable review of one specific submission, shared without copying its
/// text. A new submission gets a new identity even when its bytes are identical.
///
/// This handle is deliberately not serializable or constructible from text.
/// It survives cloning within a session, not rejection/resubmission, session
/// reset, or a different PlanState. It authorizes a state transition only;
/// executor policy and filesystem checks still apply to each tool call.
#[derive(Clone)]
pub struct PlanReview {
    plan: Arc<str>,
}

impl PlanReview {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.plan
    }

    #[must_use]
    pub fn same_submission(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.plan, &other.plan)
    }
}

impl std::fmt::Debug for PlanReview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlanReview")
            .field("bytes", &self.plan.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct PlanStateInner {
    mode: PlanMode,
    plan: Option<Arc<str>>,
    /// The SDK-owned prompt pin survives raw gate transitions so SDK exit can
    /// still remove it. Session reset retires it with the previous agent.
    session_pin: Option<session::PlanPin>,
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
        // A poisoned authorization state is not permission to mutate.
        self.inner
            .read()
            .map_or(PlanMode::Planning, |inner| inner.mode)
    }

    /// Enter planning. Returns the previous mode.
    pub fn enter_planning(&self) -> PlanMode {
        let mut inner = self.inner.write().expect("plan state lock");
        let previous = inner.mode;
        inner.mode = PlanMode::Planning;
        previous
    }

    /// Submit a plan for review (called by the submit_plan tool). Returns
    /// false when not planning, unavailable, empty, or over the byte limit.
    pub fn submit_plan(&self, plan: String) -> bool {
        self.submit(plan, false)
    }

    fn submit(&self, plan: String, auto_approve: bool) -> bool {
        if plan.trim().is_empty() || plan.len() > MAX_PLAN_BYTES {
            return false;
        }
        let Ok(mut inner) = self.inner.write() else {
            return false;
        };
        if inner.mode != PlanMode::Planning {
            return false;
        }
        // Fresh allocation is the submission identity. Keeping an old review
        // alive prevents that identity from being recycled beneath a reader.
        inner.plan = Some(Arc::from(plan));
        inner.mode = if auto_approve {
            PlanMode::Approved
        } else {
            PlanMode::PendingApproval
        };
        true
    }

    /// Approve the pending plan. Returns the plan text on success.
    pub fn approve(&self) -> Option<String> {
        let mut inner = self.inner.write().ok()?;
        if inner.mode != PlanMode::PendingApproval {
            return None;
        }
        let plan = inner.plan.as_deref()?.to_string();
        inner.mode = PlanMode::Approved;
        drop(inner);
        Some(plan)
    }

    /// Approve exactly the pending text presented by a review surface.
    /// Comparison and transition share one lock; mismatches keep the gate shut.
    /// The surface must also discard its review on rejection/session changes:
    /// this text comparison is not a submission-generation or execution lease.
    pub fn approve_reviewed(&self, reviewed: &str) -> Option<String> {
        let mut inner = self.inner.write().ok()?;
        if inner.mode != PlanMode::PendingApproval || inner.plan.as_deref() != Some(reviewed) {
            return None;
        }
        let plan = inner.plan.as_deref()?.to_string();
        inner.mode = PlanMode::Approved;
        drop(inner);
        Some(plan)
    }

    /// Capture pending state and its exact submission together under one lock.
    #[must_use]
    pub fn pending_review(&self) -> Option<PlanReview> {
        let inner = self.inner.read().ok()?;
        if inner.mode != PlanMode::PendingApproval {
            return None;
        }
        Some(PlanReview {
            plan: Arc::clone(inner.plan.as_ref()?),
        })
    }

    /// Approve the submission represented by this review, not merely matching
    /// text. An identical resubmission or another session cannot reuse it.
    pub fn approve_review(&self, review: &PlanReview) -> Option<String> {
        let mut inner = self.inner.write().ok()?;
        if inner.mode != PlanMode::PendingApproval
            || !Arc::ptr_eq(inner.plan.as_ref()?, &review.plan)
        {
            return None;
        }
        let plan = review.text().to_string();
        inner.mode = PlanMode::Approved;
        drop(inner);
        Some(plan)
    }

    /// Reject exactly the reviewed submission. A queued rejection must not
    /// discard a different proposal submitted while the user was deciding.
    pub fn reject_review(&self, review: &PlanReview) -> bool {
        let Ok(mut inner) = self.inner.write() else {
            return false;
        };
        if inner.mode != PlanMode::PendingApproval
            || !inner
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan, &review.plan))
        {
            return false;
        }
        inner.mode = PlanMode::Planning;
        true
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

    /// Install plan-mode state reconstructed for a newly active Session.
    ///
    /// Live proposal identity, prompt ownership and the pre-plan model must
    /// never cross a Session boundary. Neither PendingApproval nor Approved
    /// can be reconstructed from a mode label alone: both become read-only
    /// Planning. The host may restore a saved checkpoint for fresh review,
    /// request a new submission, or explicitly exit planning.
    pub fn reset_for_session(&self, mode: PlanMode) {
        let mut inner = self.inner.write().expect("plan state lock");
        inner.mode = match mode {
            PlanMode::PendingApproval | PlanMode::Approved => PlanMode::Planning,
            PlanMode::Off | PlanMode::Planning => mode,
        };
        inner.plan = None;
        inner.previous_model = None;
        inner.session_pin = None;
    }

    /// The submitted plan text, if any.
    #[must_use]
    pub fn plan(&self) -> Option<String> {
        self.inner
            .read()
            .ok()
            .and_then(|inner| inner.plan.as_deref().map(str::to_string))
    }

    /// Read approval state and its text together, never a mode from one plan
    /// and text from a later submission. This snapshot is not an execution
    /// lease: the executor still owns its normal plan/policy checks.
    #[must_use]
    pub fn approved_plan(&self) -> Option<String> {
        let inner = self.inner.read().ok()?;
        if inner.mode != PlanMode::Approved {
            return None;
        }
        inner.plan.as_deref().map(str::to_string)
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
    /// BARRIER set (write|append|process); everything else flows. Approval
    /// requires a live proposal, not merely a reconstructed mode label.
    #[must_use]
    pub fn allows_effects(&self, effects: ToolEffects) -> bool {
        // Observe the mode and its proposal under the same guard. Missing
        // approval context and poisoned state both retain the read-only gate.
        let unrestricted = self.inner.read().is_ok_and(|inner| match inner.mode {
            PlanMode::Off => true,
            PlanMode::Approved => inner.plan.is_some(),
            PlanMode::Planning | PlanMode::PendingApproval => false,
        });
        unrestricted || !(effects.writes() || effects.appends() || effects.processes())
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

    fn review_fixture() -> (PlanState, PlanReview) {
        let state = PlanState::new();
        state.enter_planning();
        assert!(state.submit_plan("reviewed proposal".to_string()));
        let review = state.pending_review().unwrap();
        (state, review)
    }

    #[test]
    fn review_handle_approves_exactly_once_through_a_cloned_owner() {
        let (state, review) = review_fixture();
        let another = state.pending_review().unwrap();
        assert!(review.same_submission(&another));
        assert_eq!(review.text(), "reviewed proposal");
        let cloned_state = state.clone();
        assert_eq!(
            cloned_state.approve_review(&review).as_deref(),
            Some(review.text())
        );
        assert!(state.approve_review(&another).is_none());
        assert!(state.pending_review().is_none());
    }

    #[test]
    fn identical_resubmission_requires_a_new_review_handle() {
        let (state, review) = review_fixture();
        assert!(state.reject());
        assert!(state.pending_review().is_none());
        assert!(state.approve_review(&review).is_none());
        assert!(state.submit_plan(review.text().to_string()));
        let replacement = state.pending_review().unwrap();
        assert_eq!(review.text(), replacement.text());
        assert!(!review.same_submission(&replacement));
        assert!(state.approve_review(&review).is_none());
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert!(!state.allows_effects(ToolEffects::write()));
        assert!(state.approve_review(&replacement).is_some());
    }

    #[test]
    fn review_cannot_cross_independent_plan_owners() {
        let (state, review) = review_fixture();
        let (other, other_review) = review_fixture();
        assert!(!review.same_submission(&other_review));
        assert!(other.approve_review(&review).is_none());
        assert!(state.approve_review(&other_review).is_none());
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert_eq!(other.mode(), PlanMode::PendingApproval);
    }

    #[test]
    fn stale_rejection_does_not_discard_an_identical_new_submission() {
        let (state, old) = review_fixture();
        assert!(state.reject_review(&old));
        assert!(!state.reject_review(&old));
        assert!(state.submit_plan(old.text().to_string()));
        let current = state.pending_review().unwrap();
        assert!(!state.reject_review(&old));
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert!(state.reject_review(&current));
        assert_eq!(state.mode(), PlanMode::Planning);
        assert!(!state.allows_effects(ToolEffects::write()));
    }

    #[test]
    fn rejecting_a_foreign_review_preserves_both_owners() {
        let (state, review) = review_fixture();
        let (other, other_review) = review_fixture();
        assert!(!state.reject_review(&other_review));
        assert!(!other.reject_review(&review));
        assert!(state.pending_review().unwrap().same_submission(&review));
        assert!(
            other
                .pending_review()
                .unwrap()
                .same_submission(&other_review)
        );
    }

    #[test]
    fn session_reset_and_reentry_retire_outstanding_reviews() {
        for reset in [false, true] {
            let (state, review) = review_fixture();
            if reset {
                state.reset_for_session(PlanMode::PendingApproval);
            } else {
                state.exit();
                state.enter_planning();
            }
            assert!(state.submit_plan(review.text().to_string()));
            assert!(state.approve_review(&review).is_none());
            assert_eq!(state.mode(), PlanMode::PendingApproval);
        }
    }

    #[test]
    fn invalid_submission_cannot_invalidate_a_live_review() {
        let (state, review) = review_fixture();
        assert!(!state.submit_plan("x".repeat(MAX_PLAN_BYTES + 1)));
        assert!(!state.submit_plan("another submission while pending".to_string()));
        assert!(review.same_submission(&state.pending_review().unwrap()));
        assert!(state.approve_review(&review).is_some());
    }

    #[test]
    fn review_text_is_immutable_and_not_disclosed_by_debug_output() {
        let (state, review) = review_fixture();
        state.exit();
        assert_eq!(review.text(), "reviewed proposal");
        assert!(!format!("{review:?}").contains(review.text()));
        assert!(state.approve_review(&review).is_none());
    }

    #[test]
    fn poisoned_review_owner_cannot_authorize_a_transition() {
        let (state, review) = review_fixture();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.inner.write().unwrap();
            panic!("poison plan owner");
        }));
        assert!(state.pending_review().is_none());
        assert!(state.approve_review(&review).is_none());
        assert!(!state.allows_effects(ToolEffects::process()));
    }

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
    fn session_reset_drops_memory_only_plan_state_and_fails_closed() {
        let state = PlanState::new();
        state.enter_planning();
        assert!(state.submit_plan("a complete plan that must not cross sessions".to_string()));
        state.stash_previous_model("provider-a", "model-a");

        state.reset_for_session(PlanMode::PendingApproval);

        assert_eq!(state.mode(), PlanMode::Planning);
        assert!(state.plan().is_none());
        assert!(state.take_previous_model().is_none());
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

    fn execute_plan(state: &PlanState, auto_approve: bool, plan: &str) -> crate::tools::ToolOutput {
        use crate::tools::Tool;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let tool = SubmitPlanTool::new(state.clone(), auto_approve);
        runtime
            .block_on(tool.execute("plan-test", serde_json::json!({"plan": plan}), None))
            .unwrap()
    }

    #[test]
    fn auto_approval_commits_the_submitted_text_in_one_transition() {
        let state = PlanState::new();
        state.enter_planning();
        let text = "Goal: fix code\nFiles: src/main.rs\nVerification: test";
        let output = execute_plan(&state, true, text);
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["planReview"], "auto_approved");
        assert_eq!(state.mode(), PlanMode::Approved);
        assert_eq!(state.approved_plan().as_deref(), Some(text));
        assert!(state.approve().is_none(), "there is no later approval step");
    }

    #[test]
    fn manual_submission_stays_read_only_until_exact_review() {
        let state = PlanState::new();
        state.enter_planning();
        let text = "Goal: fix code\nFiles: src/main.rs\nVerification: test";
        let output = execute_plan(&state, false, text);
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["planReview"], "pending");
        assert!(state.approved_plan().is_none());
        assert!(!state.allows_effects(ToolEffects::write()));
        assert_eq!(state.approve_reviewed(text).as_deref(), Some(text));
        assert!(state.allows_effects(ToolEffects::write()));
        assert!(state.approve_reviewed(text).is_none());
    }

    #[test]
    fn stale_review_cannot_approve_revised_text() {
        let state = PlanState::new();
        state.enter_planning();
        assert!(state.approve_reviewed("plan A").is_none());
        assert!(state.submit_plan("plan A".to_string()));
        assert!(state.reject());
        assert!(state.submit_plan("plan B".to_string()));
        for stale in ["plan A", "plan B ", "PLAN B"] {
            assert!(state.approve_reviewed(stale).is_none());
            assert_eq!(state.mode(), PlanMode::PendingApproval);
            assert!(!state.allows_effects(ToolEffects::write()));
        }
        assert_eq!(state.approve_reviewed("plan B").as_deref(), Some("plan B"));
    }

    #[test]
    fn approved_snapshot_never_exposes_pending_or_rejected_revisions() {
        let state = PlanState::new();
        state.enter_planning();
        assert!(state.submit("approved A".to_string(), true));
        assert_eq!(state.approved_plan().as_deref(), Some("approved A"));
        state.enter_planning();
        assert!(state.approved_plan().is_none());
        assert!(state.submit_plan("pending B".to_string()));
        assert!(state.approved_plan().is_none());
        assert!(state.reject());
        assert!(state.approved_plan().is_none());
        state.exit();
        assert!(state.approved_plan().is_none());
    }

    #[test]
    fn unavailable_plan_state_fails_closed() {
        let state = PlanState::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.inner.write().unwrap();
            panic!("poison the authorization state");
        }));
        assert!(result.is_err());
        assert_eq!(state.mode(), PlanMode::Planning);
        for effect in [
            ToolEffects::write(),
            ToolEffects::append(),
            ToolEffects::process(),
        ] {
            assert!(!state.allows_effects(effect));
        }
        assert!(state.allows_effects(ToolEffects::read()));
        assert!(state.approved_plan().is_none());
        assert!(state.approve().is_none());
        assert!(state.approve_reviewed("unavailable").is_none());
        assert!(!state.submit("cannot authorize this".to_string(), true));
    }

    #[test]
    fn missing_pending_text_cannot_open_the_mutation_gate() {
        let state = PlanState::new();
        state.inner.write().unwrap().mode = PlanMode::PendingApproval;
        assert!(state.approve().is_none());
        assert!(state.approve_reviewed("").is_none());
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert!(!state.allows_effects(ToolEffects::write()));
    }

    #[test]
    fn rejected_submission_preserves_existing_plan_and_mode() {
        let state = PlanState::new();
        state.enter_planning();
        assert!(state.submit_plan("retained proposal".to_string()));
        assert!(state.reject());
        for invalid in [
            String::new(),
            " \n\t".to_string(),
            "x".repeat(MAX_PLAN_BYTES + 1),
        ] {
            assert!(!state.submit(invalid, true));
            assert_eq!(state.mode(), PlanMode::Planning);
            assert_eq!(state.plan().as_deref(), Some("retained proposal"));
        }
    }

    #[test]
    fn tool_enforces_utf8_byte_budget_before_mutating_state() {
        for auto_approve in [false, true] {
            let state = PlanState::new();
            state.enter_planning();
            // Fewer than MAX_PLAN_BYTES characters, but more UTF-8 bytes.
            let oversized = "é".repeat(MAX_PLAN_BYTES / 2 + 1);
            let output = execute_plan(&state, auto_approve, &oversized);
            assert!(output.is_error);
            assert_eq!(output.details.unwrap()["planReview"], "too_large");
            assert_eq!(state.mode(), PlanMode::Planning);
            assert!(state.plan().is_none());
        }
    }

    #[test]
    fn exact_byte_budget_can_be_reviewed_and_approved() {
        let state = PlanState::new();
        state.enter_planning();
        let text = "é".repeat(MAX_PLAN_BYTES / 2);
        let output = execute_plan(&state, false, &text);
        assert!(!output.is_error);
        assert_eq!(state.plan().unwrap().len(), MAX_PLAN_BYTES);
        assert_eq!(state.approve_reviewed(&text), Some(text));
    }

    #[test]
    fn automatic_tool_call_outside_planning_never_reports_success() {
        let state = PlanState::new();
        let text = "Goal: fix code\nFiles: src/main.rs\nVerification: test";
        assert!(execute_plan(&state, true, text).is_error);
        assert_eq!(state.mode(), PlanMode::Off);
        state.enter_planning();
        assert!(!execute_plan(&state, false, text).is_error);
        assert!(execute_plan(&state, true, "another complete plan to substitute").is_error);
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert_eq!(state.plan().as_deref(), Some(text));
    }

    #[test]
    fn session_reset_preserves_only_non_authorizing_modes() {
        for (restored, expected) in [
            (PlanMode::Off, PlanMode::Off),
            (PlanMode::Planning, PlanMode::Planning),
            (PlanMode::PendingApproval, PlanMode::Planning),
            (PlanMode::Approved, PlanMode::Planning),
        ] {
            let (state, old) = review_fixture();
            assert!(state.approve_review(&old).is_some());
            state.stash_previous_model("old-provider", "old-model");
            state.reset_for_session(restored);
            assert_eq!(state.mode(), expected);
            assert!(state.plan().is_none());
            assert!(state.approved_plan().is_none());
            assert!(state.pending_review().is_none());
            assert!(state.take_previous_model().is_none());
            assert!(state.approve_review(&old).is_none());
            for effect in [
                ToolEffects::write(),
                ToolEffects::append(),
                ToolEffects::process(),
                ToolEffects::read().union(ToolEffects::write()),
            ] {
                assert_eq!(state.allows_effects(effect), expected == PlanMode::Off);
            }
            assert!(state.allows_effects(ToolEffects::read()));
            assert!(state.allows_effects(ToolEffects::network()));
        }
    }

    #[test]
    fn reset_approval_requires_a_fresh_submission_and_review() {
        let (state, old) = review_fixture();
        assert!(state.approve_review(&old).is_some());
        state.reset_for_session(PlanMode::Approved);
        assert!(!state.allows_effects(ToolEffects::write()));
        assert!(state.approve().is_none());
        assert!(state.approve_reviewed(old.text()).is_none());
        assert!(state.submit_plan(old.text().to_string()));
        let fresh = state.pending_review().unwrap();
        assert!(!old.same_submission(&fresh));
        assert!(state.approve_review(&old).is_none());
        assert!(!state.reject_review(&old));
        assert!(!state.allows_effects(ToolEffects::write()));
        assert!(state.approve_review(&fresh).is_some());
        assert!(state.allows_effects(ToolEffects::write()));
        assert_eq!(state.approved_plan().as_deref(), Some(fresh.text()));
    }

    #[test]
    fn orphaned_approved_mode_cannot_open_the_mutation_gate() {
        let state = PlanState::new();
        state.inner.write().unwrap().mode = PlanMode::Approved;
        assert!(state.approved_plan().is_none());
        for effect in [
            ToolEffects::write(),
            ToolEffects::append(),
            ToolEffects::process(),
            ToolEffects::read().union(ToolEffects::process()),
        ] {
            assert!(!state.allows_effects(effect));
        }
        assert!(state.allows_effects(ToolEffects::read()));
        assert!(state.allows_effects(ToolEffects::network()));
        state.exit();
        assert!(state.allows_effects(ToolEffects::write()));
    }
}
