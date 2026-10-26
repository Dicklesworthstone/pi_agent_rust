//! Default-FTUI access to the SDK's session-bound plan lifecycle.
//!
//! The driver owns the review capability. Commands contain only a display ID;
//! approving never substitutes a freshly fetched proposal for the one shown.
//! These controls do not start provider turns or relax tool-approval policy.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::interactive::PiMsg;
use crate::model::ThinkingLevel;
use crate::plan::{PlanChange, PlanMode, PlanPersistence, SessionPlanReview};
use crate::sdk::AgentSessionHandle;
use std::fmt::Write as _;
use std::sync::mpsc::Sender;
use uuid::Uuid;

pub(super) const USAGE: &str =
    "usage: /plan [enter|status|review|approve <review-id>|reject <review-id>|off|save|restore]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanCommand {
    Enter,
    Status,
    Review,
    Approve(Uuid),
    Reject(Uuid),
    Exit,
    Save,
    Restore,
}

/// Token-exact admission shared by UI routing and the driver. In particular,
/// a bare approval or a pasted command with extra words cannot authorize work.
pub(super) fn parse(args: &str) -> Result<PlanCommand> {
    let mut words = args.split_whitespace();
    let action = words.next().unwrap_or("enter");
    let command = match action.to_ascii_lowercase().as_str() {
        "enter" => PlanCommand::Enter,
        "status" => PlanCommand::Status,
        "review" => PlanCommand::Review,
        "off" | "exit" => PlanCommand::Exit,
        "save" => PlanCommand::Save,
        "restore" => PlanCommand::Restore,
        "approve" | "reject" => {
            let id = words
                .next()
                .and_then(|word| Uuid::parse_str(word).ok())
                .ok_or_else(|| Error::validation(USAGE.to_string()))?;
            if action.eq_ignore_ascii_case("approve") {
                PlanCommand::Approve(id)
            } else {
                PlanCommand::Reject(id)
            }
        }
        _ => return Err(Error::validation(USAGE.to_string())),
    };
    if words.next().is_some() {
        return Err(Error::validation(USAGE.to_string()));
    }
    Ok(command)
}

struct DisplayedPlan {
    id: Uuid,
    review: SessionPlanReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelSelection {
    provider: String,
    model: String,
    thinking: ThinkingLevel,
}

impl ModelSelection {
    fn current(handle: &AgentSessionHandle) -> Self {
        let (provider, model) = handle.model();
        Self {
            provider,
            model,
            thinking: handle.thinking_level().unwrap_or_default(),
        }
    }

    fn label(&self) -> String {
        format!("{}/{}:{}", self.provider, self.model, self.thinking)
    }

    fn is_current(
        &self,
        handle: &AgentSessionHandle,
        origin: &crate::extensions::SessionActionOrigin,
    ) -> bool {
        handle
            .session()
            .session_action_admission_gate()
            .origin_source()
            .accepts(origin)
            && Self::current(handle) == *self
    }
}

struct PlanModelRestore {
    origin: crate::extensions::SessionActionOrigin,
    previous: ModelSelection,
    planner: ModelSelection,
}

impl PlanModelRestore {
    async fn still_owned(&self, handle: &AgentSessionHandle) -> Result<bool> {
        if !handle
            .session()
            .session_action_admission_gate()
            .origin_source()
            .accepts(&self.origin)
        {
            return Ok(false);
        }
        let current = ModelSelection::current(handle);
        if current == self.planner {
            return Ok(true);
        }
        // A fallback remains part of the planning role. Its durable primary
        // identifies the planner; an explicit model selection clears this
        // provenance. Wait for the store rather than treating contention as a
        // newer selection and losing the execution model permanently.
        let store = handle.session_store();
        let owner = AgentCx::for_current_or_request();
        let session = store.lock(owner.cx()).await.map_err(|error| {
            Error::session(format!("Unable to confirm plan model ownership: {error}"))
        })?;
        let fallback_thinking = handle.session().clamp_thinking_level_for_model(
            &current.provider,
            &current.model,
            self.planner.thinking,
        );
        Ok(current.thinking == fallback_thinking
            && session
                .active_failover_provenance_for_current_path()
                .is_some_and(|provenance| {
                    provenance.primary_provider == self.planner.provider
                        && provenance.primary_model_id == self.planner.model
                        && provenance
                            .primary_thinking_level
                            .as_deref()
                            .and_then(|level| level.parse::<ThinkingLevel>().ok())
                            == Some(self.planner.thinking)
                        && provenance.fallback_provider == current.provider
                        && provenance.fallback_model_id == current.model
                }))
    }
}

#[derive(Default)]
pub(super) struct PlanController {
    displayed: Option<DisplayedPlan>,
    model_spec: Option<String>,
    disabled_providers: Vec<String>,
    scope_overrides: Vec<crate::config::ModelScopeOverride>,
    model_restore: Option<PlanModelRestore>,
    pending_model_selection: Option<(crate::extensions::SessionActionOrigin, ModelSelection)>,
}

impl PlanController {
    pub(super) fn new(model_spec: Option<String>) -> Self {
        Self {
            model_spec: model_spec.filter(|spec| !spec.trim().is_empty()),
            ..Self::default()
        }
    }

    pub(super) fn with_provider_policy(
        mut self,
        disabled_providers: Vec<String>,
        scope_overrides: Vec<crate::config::ModelScopeOverride>,
    ) -> Self {
        self.disabled_providers = disabled_providers;
        self.scope_overrides = scope_overrides;
        self
    }

    /// Successful explicit model/thinking selection owns the resulting model,
    /// even when the user selected the planner that was already active.
    pub(super) fn forget_model_restore(&mut self) {
        self.model_restore = None;
        self.pending_model_selection = None;
    }

    /// Session replacement and a disconnected UI retire the display capability.
    /// SDK session-incarnation checks independently reject foreign reviews.
    pub(super) fn clear_review(&mut self) {
        self.displayed = None;
    }

    pub(super) async fn run(
        &mut self,
        handle: &mut AgentSessionHandle,
        args: &str,
        sender: &Sender<PiMsg>,
    ) {
        let message = match self.execute(handle, args).await {
            Ok(text) => PiMsg::SystemNote(text),
            Err(error) => PiMsg::AgentError(format!("plan: {error}")),
        };
        if sender.send(message).is_err() {
            self.clear_review();
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&mut self, handle: &mut AgentSessionHandle, args: &str) -> Result<String> {
        // Invalid commands neither change live state nor replace the review.
        let command = parse(args)?;
        let restore_stale = match self.model_restore.as_ref() {
            Some(restore) => !restore.still_owned(handle).await?,
            None => false,
        };
        if restore_stale
            || self
                .pending_model_selection
                .as_ref()
                .is_some_and(|(origin, current)| !current.is_current(handle, origin))
        {
            self.forget_model_restore();
        }
        let before = handle.session().agent.plan_state().mode();
        let entering = matches!(command, PlanCommand::Enter)
            && matches!(before, PlanMode::Off | PlanMode::Approved);
        let owner = AgentCx::for_current_or_request();
        let change = match command {
            PlanCommand::Status => {
                let mode = handle.session().agent.plan_state().mode();
                let hint = if handle.session().ensure_provider_reentry_allowed().is_err() {
                    PROVIDER_BLOCKED_HINT
                } else if self.pending_model_selection.is_some() {
                    "Planning model selection is pending. Use /plan save to confirm the checkpoint and finish selecting the configured model."
                } else {
                    mode_hint(mode, self.model_restore.is_some())
                };
                return Ok(format!("Plan mode: {}. {}\n{USAGE}", mode.as_str(), hint,));
            }
            PlanCommand::Review => {
                // A failed refresh must not leave an earlier display authorizing
                // a later command. The full text is literal system output, not
                // markdown; escapes make control/bidi/zero-width text visible.
                self.clear_review();
                let review = handle.pending_plan_review()?.ok_or_else(|| {
                    Error::validation(
                        "No pending proposal. Enter /plan, request a plan ending with submit_plan, then use /plan review."
                            .to_string(),
                    )
                })?;
                let id = Uuid::new_v4();
                let text = render_review(&review, id);
                self.displayed = Some(DisplayedPlan { id, review });
                return Ok(text);
            }
            PlanCommand::Save => {
                let persistence = handle.checkpoint_plan(&owner).await?;
                let mut text = format!("Plan checkpoint. {}", render_persistence(&persistence));
                if !matches!(persistence, PlanPersistence::Unconfirmed { .. }) {
                    if self.pending_model_selection.take().is_some()
                        && matches!(
                            handle.session().agent.plan_state().mode(),
                            PlanMode::Planning | PlanMode::PendingApproval
                        )
                    {
                        self.select_plan_model(handle, &mut text).await;
                    }
                    self.restore_model(handle, &mut text).await;
                }
                return Ok(text);
            }
            PlanCommand::Enter => handle.enter_plan_mode(&owner).await?,
            PlanCommand::Exit => handle.exit_plan_mode(&owner).await?,
            PlanCommand::Restore => handle.restore_plan_checkpoint(&owner).await?,
            PlanCommand::Approve(id) | PlanCommand::Reject(id) => {
                let displayed = self.displayed.as_ref().ok_or_else(|| {
                    Error::validation("No displayed proposal; run /plan review first.".to_string())
                })?;
                if displayed.id != id {
                    return Err(Error::validation(
                        "Review ID does not match the displayed proposal. Run /plan review again; nothing was changed."
                            .to_string(),
                    ));
                }
                // Consume the capability before awaiting. Cancellation during
                // persistence must not make an accepted decision replayable.
                let displayed = self.displayed.take().ok_or_else(|| {
                    Error::validation("Review unavailable; run /plan review again.".to_string())
                })?;
                if matches!(command, PlanCommand::Approve(_)) {
                    handle
                        .approve_plan_review(&owner, &displayed.review)
                        .await?
                } else {
                    handle.reject_plan_review(&owner, &displayed.review).await?
                }
            }
        };
        self.clear_review();
        if matches!(command, PlanCommand::Approve(_) | PlanCommand::Exit) {
            self.pending_model_selection = None;
        }
        let mut model_message = String::new();
        // A failed checkpoint never authorizes the automatic model transition.
        // The live gate remains as reported; save it before starting more work.
        if matches!(change.persistence, PlanPersistence::Unconfirmed { .. }) {
            if entering && self.model_spec.is_some() {
                self.pending_model_selection = Some((
                    handle
                        .session()
                        .session_action_admission_gate()
                        .capture_origin(),
                    ModelSelection::current(handle),
                ));
            }
        } else if entering {
            self.pending_model_selection = None;
            self.select_plan_model(handle, &mut model_message).await;
        } else if matches!(command, PlanCommand::Approve(_) | PlanCommand::Exit) {
            self.restore_model(handle, &mut model_message).await;
        }
        let mut text = render_change(
            &change,
            self.model_restore.is_some(),
            handle.session().ensure_provider_reentry_allowed().is_err(),
        );
        text.push_str(&model_message);
        Ok(text)
    }

    async fn select_plan_model(&mut self, handle: &mut AgentSessionHandle, text: &mut String) {
        let Some(spec) = self.model_spec.as_deref() else {
            return;
        };
        let previous = self.model_restore.as_ref().map_or_else(
            || ModelSelection::current(handle),
            |restore| restore.previous.clone(),
        );
        let origin = handle
            .session()
            .session_action_admission_gate()
            .capture_origin();
        let selection = resolve_plan_model(
            handle,
            spec,
            &self.disabled_providers,
            &self.scope_overrides,
        )
        .await;
        let result = match selection {
            Ok(selection) => {
                handle
                    .set_plan_role_model(
                        &selection.provider,
                        &selection.model,
                        selection.thinking,
                        &origin,
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(true) => {
                let planner = ModelSelection::current(handle);
                let _ = write!(text, "\nPlanning model: {}.", planner.label());
                self.model_restore = (planner != previous).then_some(PlanModelRestore {
                    origin,
                    previous,
                    planner,
                });
            }
            Ok(false) => {
                self.forget_model_restore();
                text.push_str("\nPlanning model was not changed: a newer model selection won.");
            }
            Err(error) => {
                let recovery = if handle.session().ensure_provider_reentry_allowed().is_err() {
                    PROVIDER_BLOCKED_HINT
                } else {
                    "Use /model to select a usable model explicitly."
                };
                let _ = write!(
                    text,
                    "\nPlanning remains read-only, but the plan model could not be selected: {}. \
                     Current model: {}. {}",
                    escape_review_text(&error.to_string()),
                    ModelSelection::current(handle).label(),
                    recovery,
                );
            }
        }
    }

    async fn restore_model(&mut self, handle: &mut AgentSessionHandle, text: &mut String) {
        if !matches!(
            handle.session().agent.plan_state().mode(),
            PlanMode::Off | PlanMode::Approved
        ) {
            return;
        }
        let Some(restore) = self.model_restore.as_ref() else {
            return;
        };
        match restore.still_owned(handle).await {
            Ok(true) => {}
            Ok(false) => {
                self.forget_model_restore();
                return;
            }
            Err(error) => {
                let _ = write!(
                    text,
                    "\nExecution model ownership could not be confirmed: {}. Use /plan save to retry; no model was changed.",
                    escape_review_text(&error.to_string()),
                );
                return;
            }
        }
        let previous = restore.previous.clone();
        match handle
            .set_plan_role_model(
                &previous.provider,
                &previous.model,
                previous.thinking,
                &restore.origin,
            )
            .await
        {
            Ok(true) => {
                self.forget_model_restore();
                let _ = write!(
                    text,
                    "\nRestored execution model: {}.",
                    ModelSelection::current(handle).label(),
                );
            }
            Ok(false) => {
                self.forget_model_restore();
                text.push_str(
                    "\nKept the newer model selection; no automatic restoration applied.",
                );
            }
            Err(error) => {
                let recovery = if handle.session().ensure_provider_reentry_allowed().is_err() {
                    PROVIDER_BLOCKED_HINT
                } else {
                    "Use /plan save to retry after resolving the error, or choose /model explicitly."
                };
                let _ = write!(
                    text,
                    "\nExecution model restoration was NOT confirmed: {}. \
                     No execution was started. {}",
                    escape_review_text(&error.to_string()),
                    recovery,
                );
            }
        }
    }
}

async fn resolve_plan_model(
    handle: &AgentSessionHandle,
    spec: &str,
    disabled_providers: &[String],
    scope_overrides: &[crate::config::ModelScopeOverride],
) -> Result<ModelSelection> {
    let registry = handle
        .session()
        .model_registry()
        .ok_or_else(|| Error::validation("Plan model selection requires a model registry"))?;
    let catalog = registry
        .models()
        .iter()
        .map(|entry| format!("{}/{}", entry.model.provider, entry.model.id))
        .collect::<Vec<_>>();
    let (provider, model, thinking) =
        super::resolve_model_selector(&catalog, spec).map_err(Error::validation)?;
    let entry = registry
        .find(&provider, &model)
        .ok_or_else(|| Error::validation(format!("Plan model {spec:?} is not registered")))?;
    // Keep the full registry for exact identity and CLI-key admission. A role
    // must still respect disabled providers, independently of picker scopes.
    // Resolve the current session's attachment, not the launch process cwd.
    let cwd = if scope_overrides.is_empty() {
        None
    } else {
        Some(
            handle
                .with_session(crate::session_workdir::require_session_workdir)
                .await??,
        )
    };
    let scope = cwd
        .as_deref()
        .and_then(|cwd| crate::failover::best_scope_override(scope_overrides, cwd));
    if crate::failover::provider_is_disabled(disabled_providers, scope, &entry.model.provider) {
        return Err(Error::validation(format!(
            "Plan destination provider is disabled: {}",
            entry.model.provider
        )));
    }
    Ok(ModelSelection {
        provider: entry.model.provider,
        model: entry.model.id,
        thinking: thinking.unwrap_or_else(|| handle.thinking_level().unwrap_or_default()),
    })
}

const PROVIDER_BLOCKED_HINT: &str = "Provider access is blocked after an indeterminate transition. Resolve the storage error, reopen the saved session, and explicitly select the intended model; /plan save cannot clear this block.";

const fn mode_hint(mode: PlanMode, restore_pending: bool) -> &'static str {
    if restore_pending && matches!(mode, PlanMode::Off | PlanMode::Approved) {
        return "Execution model restoration is pending. Use /plan save to retry, or choose /model explicitly before continuing.";
    }
    match mode {
        PlanMode::Off => "Normal tool policy remains in effect.",
        PlanMode::Planning => {
            "Planning is read-only. Send your planning task and ask the agent to finish with submit_plan; then run /plan review."
        }
        PlanMode::PendingApproval => {
            "The proposal is awaiting review; mutations remain blocked. Use /plan review to display it."
        }
        PlanMode::Approved => {
            "The reviewed proposal is approved in the live session. No execution turn was started and tool permissions were not changed."
        }
    }
}

fn render_persistence(persistence: &PlanPersistence) -> String {
    match persistence {
        PlanPersistence::Saved => "Saved to the session.".to_string(),
        PlanPersistence::MemoryOnly => "Memory-only session; this state was not saved.".to_string(),
        PlanPersistence::Unchanged => "No live transition or save was needed.".to_string(),
        PlanPersistence::Unconfirmed { reason } => format!(
            "Saving was NOT confirmed: {}. Live plan state was not rolled back. \
             Use /plan save to retry persistence; do not repeat the approval decision.",
            escape_review_text(reason),
        ),
    }
}

fn render_change(change: &PlanChange, restore_pending: bool, provider_blocked: bool) -> String {
    let hint = if provider_blocked {
        PROVIDER_BLOCKED_HINT
    } else if matches!(change.persistence, PlanPersistence::Unconfirmed { .. }) {
        "Persistence is unconfirmed. Save the plan state before starting another turn."
    } else {
        mode_hint(change.mode, restore_pending)
    };
    format!(
        "Plan mode: {}. {}\n{}",
        change.mode.as_str(),
        hint,
        render_persistence(&change.persistence),
    )
}

/// Reversible display encoding: printable ASCII remains literal, backslashes
/// and every other scalar are escaped except LF. In particular a CR cannot
/// overwrite a line, and bidi controls cannot reorder a reviewed file scope.
fn escape_review_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n'
            || (character.is_ascii_graphic() && character != '\\')
            || character == ' '
        {
            escaped.push(character);
        } else {
            escaped.extend(character.escape_default());
        }
    }
    escaped
}

fn render_review(review: &SessionPlanReview, id: Uuid) -> String {
    let mut text = String::from(
        "Pending plan (complete text; non-ASCII, control characters and backslashes are escaped):\n",
    );
    // Every line is visibly quoted; proposal-supplied instructions cannot be
    // mistaken for the host's decision command following the end marker.
    for line in escape_review_text(review.text()).split('\n') {
        let _ = writeln!(text, "| {line}");
    }
    let _ = write!(
        text,
        "End of proposal.\nReview ID: {id}\n\
         Approve exactly this submission: /plan approve {id}\n\
         Reject exactly this submission: /plan reject {id}\n\
         Approval pins the plan only; no execution turn starts and existing tool permissions still apply."
    );
    text
}

#[cfg(test)]
mod tests;
