//! Idle terminal controls over the native branch-scoped goal journal.
//!
//! A revision receipt binds even creation to the displayed session/branch.
//! Receipt verification and journal mutation share the same owned store lock.
//! These are lifecycle controls only: no provider turn, polling task, tool
//! permission, or implicit continuation is introduced by a slash command.

use super::{
    GoalBudget, GoalDecision, GoalPersistence, GoalStatus, MAX_TEXT_BYTES, Record,
    Transition, append, check_owner, failure, latest, persist, snapshot, transition, validate_text,
};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::plan::PlanMode;
use crate::model::ContentBlock;
use crate::sdk::AgentSessionHandle;
use crate::session::Session;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::time::Duration;

const USAGE: &str = "usage: /goal [status | create <review> [--tokens N] [--prompts N] -- <objective> | budget <review> --tokens N --prompts N -- <reason> | step <review> | run <review> --steps N | pause <review> | resume <review> | drop <review> -- <reason> | complete <review> -- <evidence> | save <review>]";
const RECEIPT_PREFIX: &str = "g1-";

#[derive(Clone, Copy)]
enum Action<'a> {
    Create { objective: &'a str, budget: GoalBudget },
    Budget { reason: &'a str, budget: GoalBudget },
    Step,
    Run { steps: u32 },
    Pause,
    Resume,
    Drop(&'a str),
    Complete(&'a str),
    Save,
}

// Do not derive Debug: objectives and completion evidence may be private.
enum Command<'a> {
    Status,
    Change { review: &'a str, action: Action<'a> },
}

fn word<'a>(remaining: &mut &'a str) -> Option<&'a str> {
    let input = remaining.trim_start();
    if input.is_empty() {
        *remaining = input;
        return None;
    }
    let (head, tail) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    *remaining = tail;
    Some(head)
}

fn usage() -> crate::error::Error {
    failure("GOAL_COMMAND", USAGE)
}

fn text_tail<'a>(remaining: &mut &'a str) -> Result<&'a str> {
    if word(remaining) != Some("--") {
        return Err(usage());
    }
    let text = remaining.trim();
    validate_text(text, "objective, reason or evidence")?;
    Ok(text)
}

fn parse(args: &str) -> Result<Command<'_>> {
    // Bound input before token scanning or allocation, independently of the
    // journal's 16 KiB text limit. No shell parser or file/template expansion.
    if args.len() > MAX_TEXT_BYTES + 512 || args.contains('\0') {
        return Err(usage());
    }
    let mut remaining = args;
    let verb = word(&mut remaining).unwrap_or("status");
    if verb.eq_ignore_ascii_case("status") {
        return if word(&mut remaining).is_none() { Ok(Command::Status) } else { Err(usage()) };
    }
    let review = word(&mut remaining).ok_or_else(usage)?;
    if review.len() != RECEIPT_PREFIX.len() + 64
        || !review.starts_with(RECEIPT_PREFIX)
        || !review[RECEIPT_PREFIX.len()..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(usage());
    }
    let action = match verb.to_ascii_lowercase().as_str() {
        "create" | "budget" => {
            let revising = verb.eq_ignore_ascii_case("budget");
            let mut budget = GoalBudget::default();
            let (mut tokens_seen, mut prompts_seen) = (false, false);
            loop {
                let saved = remaining;
                match word(&mut remaining) {
                    Some("--tokens") if !tokens_seen => {
                        tokens_seen = true;
                        budget.token_budget = word(&mut remaining)
                            .and_then(|value| value.parse().ok()).ok_or_else(usage)?;
                    }
                    Some("--prompts") if !prompts_seen => {
                        prompts_seen = true;
                        budget.max_prompts = word(&mut remaining)
                            .and_then(|value| value.parse().ok()).ok_or_else(usage)?;
                    }
                    Some("--") => {
                        remaining = saved;
                        break;
                    }
                    _ => return Err(usage()),
                }
            }
            // Existing creation defaults remain useful, but revising one
            // total must not silently reset the other to a default allowance.
            if revising && !(tokens_seen && prompts_seen) { return Err(usage()); }
            budget.validate()?;
            let text = text_tail(&mut remaining)?;
            if revising {
                Action::Budget { reason: text, budget }
            } else {
                Action::Create { objective: text, budget }
            }
        }
        "drop" => Action::Drop(text_tail(&mut remaining)?),
        "complete" => Action::Complete(text_tail(&mut remaining)?),
        "run" => {
            if word(&mut remaining) != Some("--steps") {
                return Err(usage());
            }
            let steps = word(&mut remaining)
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|steps| (1..=32).contains(steps))
                .ok_or_else(usage)?;
            if word(&mut remaining).is_some() {
                return Err(usage());
            }
            Action::Run { steps }
        }
        "step" | "pause" | "resume" | "save" => {
            if word(&mut remaining).is_some() { return Err(usage()); }
            match verb.to_ascii_lowercase().as_str() {
                "step" => Action::Step,
                "pause" => Action::Pause,
                "resume" => Action::Resume,
                _ => Action::Save,
            }
        }
        _ => return Err(usage()),
    };
    Ok(Command::Change { review, action })
}

/// A logical revision identifier, NOT a bearer capability or authentication.
/// Length-delimited serialization binds absence as well as a complete record.
/// Unchanged saved state can be reviewed across reopening; native SDK snapshot
/// incarnation checks are a different API and remain unchanged.
fn receipt(
    session: &Session,
    current: &Option<(String, Record)>,
    live_plan: PlanMode,
    persistent: bool,
) -> Result<String> {
    if session.header.id.is_empty() || session.header.id.len() > 1024
        || session.leaf_id().is_some_and(|id| id.len() > 1024)
    {
        return Err(failure("GOAL_REVIEW", "session identity is missing or oversized"));
    }
    let state = serde_json::to_vec(&(
        "pi.goal.terminal-review.v1",
        &session.header.id,
        session.leaf_id(),
        current,
        live_plan.as_str(),
        crate::plan::replayed_plan_mode(session).as_str(),
        persistent,
    ))?;
    let mut result = String::from(RECEIPT_PREFIX);
    let digest = Sha256::digest(&state);
    for byte in &digest {
        let _ = write!(result, "{byte:02x}");
    }
    Ok(result)
}

fn require_no_plan(session: &Session, live: PlanMode) -> Result<()> {
    if live != PlanMode::Off || crate::plan::replayed_plan_mode(session) != PlanMode::Off {
        return Err(failure("GOAL_PLAN_MODE", "exit plan mode before creating or resuming a goal"));
    }
    Ok(())
}

pub(crate) async fn run(handle: &mut AgentSessionHandle, args: &str) -> Result<String> {
    let command = parse(args)?;
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner, false)?;
    let persistent = handle.session().save_enabled();
    let live_plan = handle.session().agent.plan_state().mode();
    let store = handle.session_store();
    let mut session = store.try_lock_owned()
        .map_err(|_| failure("GOAL_BUSY", "session is busy; retry /goal status when idle"))?;
    let mut current = latest(&session)?;
    let expected = receipt(&session, &current, live_plan, persistent)?;
    let Command::Change { review, action } = command else {
        return Ok(render(current.as_ref().map(|(_, record)| record), &expected));
    };
    if review != expected {
        return Err(failure("GOAL_STALE", "session, branch or goal changed; run /goal status and review it again. Nothing was changed."));
    }
    if matches!(action, Action::Step | Action::Run { .. }) {
        require_no_plan(&session, live_plan)?;
        let (id, record) = current.as_ref()
            .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
        let reviewed = snapshot(&store, &session, id.clone(), record.clone());
        let (requested_steps, label) = match action {
            Action::Step => (1, "step"),
            Action::Run { steps } => (steps, "run"),
            _ => unreachable!(),
        };
        drop(session);
        let mut completed_steps = 0u32;
        let run = handle.run_goal(
            &owner,
            &reviewed,
            Duration::from_secs(15 * 60),
            |_control, _event| {},
            move |_goal, _message| {
                completed_steps = completed_steps.saturating_add(1);
                if completed_steps >= requested_steps {
                    GoalDecision::Pause {
                        reason: format!(
                            "Terminal run completed {completed_steps} reviewed prompt(s); inspect the result before continuing."
                        ),
                    }
                } else {
                    GoalDecision::Continue {
                        instruction: format!(
                            "Continue working on the same objective. This is prompt {} of the user's bounded terminal run; do not claim completion on the user's behalf.",
                            completed_steps.saturating_add(1)
                        ),
                    }
                }
            },
        )?;
        let report = run.await;
        let mut output = String::new();
        let _ = writeln!(output, "Goal {label} stopped: {:?}", report.stop);
        if let Some(Ok(message)) = &report.completion {
            let mut wrote = false;
            for block in &message.content {
                if let ContentBlock::Text(text) = block
                    && !text.text.trim().is_empty()
                {
                    if !wrote {
                        output.push_str("Assistant result:\n");
                        wrote = true;
                    }
                    quoted(&mut output, &text.text);
                }
            }
        }
        if let Some(error) = &report.error {
            output.push_str("Step error:\n");
            quoted(&mut output, &error.to_string());
        }
        if let Some(persistence) = &report.persistence {
            let _ = writeln!(output, "{}", persistence_text(persistence));
        }
        let store = handle.session_store();
        let session = store.try_lock_owned()
            .map_err(|_| failure("GOAL_BUSY", "session is busy after goal step"))?;
        let current = latest(&session)?;
        let review = receipt(&session, &current, live_plan, persistent)?;
        let _ = write!(output, "{}", render(current.as_ref().map(|(_, record)| record), &review));
        return Ok(output);
    }
    check_owner(&owner, persistent)?;
    let next = match action {
        Action::Create { objective, budget } => {
            require_no_plan(&session, live_plan)?;
            if current.as_ref().is_some_and(|(_, record)| !record.status.terminal()) {
                return Err(failure("GOAL_EXISTS", "resume or explicitly drop the existing goal before replacing it"));
            }
            Some(Record::new(objective, budget)?)
        }
        Action::Budget { budget, reason } => {
            let (_, record) = current.as_ref()
                .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
            Some(transition(record.clone(), Transition::Budget { budget, reason })?)
        }
        Action::Save => {
            if current.is_none() { return Err(failure("GOAL_MISSING", "this branch has no goal to save")); }
            None
        }
        Action::Step | Action::Run { .. } => unreachable!("execution handled before journal mutation"),
        Action::Pause | Action::Resume | Action::Drop(_) | Action::Complete(_) => {
            if matches!(action, Action::Resume) { require_no_plan(&session, live_plan)?; }
            let (_, record) = current.as_ref()
                .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
            if matches!(action, Action::Pause) && record.in_flight {
                return Err(failure("GOAL_INTERRUPTED", "an unfinished attempt is already paused; inspect it before explicitly resuming or dropping it"));
            }
            let change = match action {
                Action::Pause => Transition::Pause("Paused by terminal user"),
                Action::Resume => Transition::Resume,
                Action::Drop(reason) => Transition::Drop(reason),
                Action::Complete(evidence) => Transition::Complete(evidence),
                _ => unreachable!(),
            };
            Some(transition(record.clone(), change)?)
        }
    };
    // No await or unlock separates the receipt check from this append. A
    // queued command cannot be redirected by a session/branch replacement.
    check_owner(&owner, persistent)?;
    if let Some(record) = next {
        let id = append(&mut session, &record)?;
        handle.session_mut().invalidate_background_compaction();
        current = Some((id, record));
    }
    let persistence = persist(&mut session, &owner, persistent).await;
    let review = receipt(&session, &current, live_plan, persistent)?;
    Ok(format!("{}\n{}", persistence_text(&persistence), render(current.as_ref().map(|(_, record)| record), &review)))
}

fn escaped(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if character == '\n' || character == ' ' || (character.is_ascii_graphic() && character != '\\') {
            output.push(character);
        } else {
            output.extend(character.escape_default());
        }
    }
    output
}

fn quoted(output: &mut String, text: &str) {
    for line in escaped(text).split('\n') { let _ = writeln!(output, "| {line}"); }
}

fn persistence_text(persistence: &GoalPersistence) -> String {
    match persistence {
        GoalPersistence::Saved => "Goal journal saved to the session.".into(),
        GoalPersistence::MemoryOnly => "Memory-only session: goal journal was NOT saved to disk.".into(),
        GoalPersistence::Unconfirmed { reason } => {
            let bounded: String = reason.chars().take(1024).collect();
            let mut text = String::from("Saving was NOT confirmed; the live decision was not rolled back.\n");
            quoted(&mut text, &bounded);
            if bounded.len() < reason.len() { text.push_str("[persistence diagnostic truncated]\n"); }
            text.push_str("Use /goal save with the new review receipt; do not repeat the decision.");
            text
        }
    }
}

fn render(record: Option<&Record>, review: &str) -> String {
    let mut text = String::new();
    if let Some(record) = record {
        let status = match record.effective_status() {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::BudgetLimited => "budget-limited",
            GoalStatus::Complete => "complete (user/host attestation)",
            GoalStatus::Dropped => "dropped",
        };
        let _ = writeln!(text, "Goal {}: {status}\nPrompts: {}/{}; recorded tokens: {}/{}; interrupted: {}",
            record.id, record.prompts_started, record.max_prompts,
            record.tokens_spent, record.token_budget, record.in_flight);
        text.push_str("Objective (complete; non-ASCII, controls and backslashes escaped):\n");
        quoted(&mut text, &record.objective);
        text.push_str("End of objective.\n");
        if !record.note.is_empty() {
            text.push_str("Recorded note/evidence:\n");
            quoted(&mut text, &record.note);
            text.push_str("End of note/evidence.\n");
        }
        if record.in_flight {
            text.push_str("Interrupted work: inspect history and side effects before explicitly resuming or dropping it.\n");
        }
    } else {
        text.push_str("No goal on the current session branch.\n");
    }
    let _ = writeln!(text, "Review: {review}");
    if record.is_none_or(|record| record.status.terminal()) {
        let _ = writeln!(text, "Create: /goal create {review} --tokens 100000 --prompts 16 -- <objective>");
    } else {
        let _ = writeln!(text, "Pause: /goal pause {review}\nResume: /goal resume {review}\nDrop: /goal drop {review} -- <reason>\nAttest completion: /goal complete {review} -- <evidence>\nRetry saving: /goal save {review}");
        let _ = writeln!(text, "Revise total budgets: /goal budget {review} --tokens N --prompts N -- <reason>\nBudget revision preserves spent counters and interruption; it does not resume work.");
    }
    text.push_str("Lifecycle controls only: no model request was started or resumed. Bounded execution uses the SDK run_goal APIs; ordinary terminal prompts do not run that driver.\nToken accounting is reported usage, not a hard billing ceiling. Completion evidence is an attestation, not an independently verified test result.");
    text
}

#[cfg(test)]
mod tests;
