//! Default-FTUI access to the SDK's session-bound plan lifecycle.
//!
//! The driver owns the review capability. Commands contain only a display ID;
//! approving never substitutes a freshly fetched proposal for the one shown.
//! These controls do not start provider turns or relax tool-approval policy.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::interactive::PiMsg;
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

#[derive(Default)]
pub(super) struct PlanController {
    displayed: Option<DisplayedPlan>,
}

impl PlanController {
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

    async fn execute(&mut self, handle: &mut AgentSessionHandle, args: &str) -> Result<String> {
        // Invalid commands neither change live state nor replace the review.
        let command = parse(args)?;
        let owner = AgentCx::for_current_or_request();
        let change = match command {
            PlanCommand::Status => {
                let mode = handle.session().agent.plan_state().mode();
                return Ok(format!(
                    "Plan mode: {}. {}\n{USAGE}",
                    mode.as_str(),
                    mode_hint(mode),
                ));
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
                return Ok(format!("Plan checkpoint. {}", render_persistence(&persistence)));
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
                    handle.approve_plan_review(&owner, &displayed.review).await?
                } else {
                    handle.reject_plan_review(&owner, &displayed.review).await?
                }
            }
        };
        self.clear_review();
        Ok(render_change(&change))
    }
}

fn mode_hint(mode: PlanMode) -> &'static str {
    match mode {
        PlanMode::Off => "Normal tool policy remains in effect.",
        PlanMode::Planning => {
            "Planning is read-only. Send your planning task and ask the agent to finish with submit_plan; then run /plan review."
        }
        PlanMode::PendingApproval => {
            "The proposal is awaiting review; mutations remain blocked. Use /plan review to display it."
        }
        PlanMode::Approved => {
            "The reviewed plan is pinned. No execution turn was started and tool permissions were not changed. Send a prompt to begin execution."
        }
    }
}

fn render_persistence(persistence: &PlanPersistence) -> String {
    match persistence {
        PlanPersistence::Saved => "Saved to the session.".to_string(),
        PlanPersistence::MemoryOnly => {
            "Memory-only session; this state was not saved.".to_string()
        }
        PlanPersistence::Unchanged => "No live transition or save was needed.".to_string(),
        PlanPersistence::Unconfirmed { reason } => format!(
            "Saving was NOT confirmed: {}. Live plan state was not rolled back. \
             Use /plan save to retry persistence; do not repeat the approval decision.",
            escape_review_text(reason),
        ),
    }
}

fn render_change(change: &PlanChange) -> String {
    format!(
        "Plan mode: {}. {}\n{}",
        change.mode.as_str(),
        mode_hint(change.mode),
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
