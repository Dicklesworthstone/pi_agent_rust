//! RPC projection and lifecycle adapter for persistent goals.
//!
//! This exposes host lifecycle operations only. It does not create a
//! model-visible goal tool or submit provider turns. Mutations are admitted by
//! the RPC loop only while idle; get is read-only. Budget revision is an atomic
//! pause operation, retaining the outer loop's existing input-recovery preflight.
//! Reviewed completion and save-only resolution use that same guarded route.

use super::{
    GoalBudget, GoalPersistence, GoalStatus, Record, Transition, append, check_owner, failure,
    latest, persist, transition,
};
use crate::agent::AgentSession;
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::session::Session;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::sync::Arc;

mod resolution;
#[cfg(test)]
mod resolution_tests;

fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::BudgetLimited => "budget-limited",
        GoalStatus::Complete => "complete",
        GoalStatus::Dropped => "dropped",
    }
}

fn result_value(record: Option<&Record>) -> Value {
    let Some(record) = record else {
        return json!({"goal": null, "state": null});
    };
    let status = record.effective_status();
    let goal = json!({
        "id": record.id,
        "objective": record.objective,
        "status": status_name(status),
        "tokenBudget": record.token_budget,
        "tokensUsed": record.tokens_spent,
        "timeUsedSeconds": 0,
        "createdAt": 0,
        "updatedAt": 0,
        "maxPrompts": record.max_prompts,
        "promptsStarted": record.prompts_started,
        "interrupted": record.in_flight,
        "note": record.note,
    });
    json!({
        "goal": goal,
        "state": {
            "enabled": status == GoalStatus::Active,
            "mode": "active",
            "goal": goal,
        }
    })
}

// A logical revision receipt, not a bearer capability. Bind dynamic recorded
// usage as well as the journal leaf, so neither queued input nor another
// branch/goal can inherit a budget decision. Unchanged saved state has the
// same receipt after reopening; SDK incarnation checks are intentionally
// stronger. Live tool authority and execution deadlines are not changed here.
fn revision(session: &Session, current: &Option<(String, Record)>) -> Result<String> {
    if session.header.id.is_empty()
        || session.header.id.len() > 1024
        || session.leaf_id().is_some_and(|id| id.len() > 1024)
    {
        return Err(failure(
            "GOAL_REVIEW",
            "session identity is missing or oversized",
        ));
    }
    let bytes = serde_json::to_vec(&(
        "pi.goal.rpc-review.v1",
        &session.header.id,
        session.leaf_id(),
        current,
        crate::plan::replayed_plan_mode(session).as_str(),
    ))?;
    let mut receipt = String::from("gr1-");
    for byte in Sha256::digest(&bytes) {
        let _ = write!(receipt, "{byte:02x}");
    }
    Ok(receipt)
}

pub(crate) fn rpc_goal_state(session: &Session) -> Result<Value> {
    let current = latest(session)?;
    let mut value = result_value(current.as_ref().map(|(_, record)| record));
    value["revision"] = Value::String(revision(session, &current)?);
    Ok(value)
}

fn requested_budget(request: &Value) -> Result<GoalBudget> {
    let defaults = GoalBudget::default();
    let token_budget = match request.get("token_budget") {
        None | Some(Value::Null) => defaults.token_budget,
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| failure("GOAL_RPC", "token_budget must be a positive integer"))?,
    };
    let max_prompts = match request.get("max_prompts") {
        None | Some(Value::Null) => defaults.max_prompts,
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| (1..=1024).contains(value))
            .ok_or_else(|| failure("GOAL_RPC", "max_prompts must be an integer from 1 to 1024"))?,
    };
    Ok(GoalBudget {
        max_prompts,
        token_budget,
    })
}

fn revised_budget(session: &Session, request: &Value) -> Result<Record> {
    let budget = &request["budget"];
    if budget.as_object().is_none_or(|object| object.len() != 2)
        || budget.get("token_budget").is_none_or(Value::is_null)
        || budget.get("max_prompts").is_none_or(Value::is_null)
    {
        return Err(failure(
            "GOAL_RPC",
            "budget must contain exactly the new token_budget and max_prompts totals",
        ));
    }
    let budget = requested_budget(budget)?;
    let reason = request
        .get("reason")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("GOAL_RPC", "budget revision requires an explicit reason"))?;
    let reviewed = request
        .get("revision")
        .and_then(Value::as_str)
        .filter(|value| value.len() == 68 && value.starts_with("gr1-"))
        .ok_or_else(|| {
            failure(
                "GOAL_RPC",
                "budget revision requires the receipt from goal get",
            )
        })?;
    let current = latest(session)?;
    if reviewed != revision(session, &current)? {
        return Err(failure(
            "GOAL_STALE",
            "session, branch or goal changed; get and review it again. Nothing was changed.",
        ));
    }
    let (_, record) = current.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
    transition(record, Transition::Budget { budget, reason })
}

fn require_no_plan_mode(session: &Session) -> Result<()> {
    if crate::plan::replayed_plan_mode(session) != crate::plan::PlanMode::Off {
        return Err(failure(
            "GOAL_PLAN_MODE",
            "exit plan mode before creating or resuming a goal",
        ));
    }
    Ok(())
}

pub(crate) async fn rpc_goal_command(
    agent_session: &mut AgentSession,
    owner: &AgentCx,
    request: &Value,
) -> Result<Value> {
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("GOAL_RPC", "goal op is required"))?;
    let resolution = resolution::parse(request, op)?;
    // Refuse misplaced budget edits rather than silently resuming with an
    // ignored allowance. A plain pause keeps its existing wire contract.
    if request.get("budget").is_some() && op != "pause" {
        return Err(failure("GOAL_RPC", "a budget revision requires op=pause"));
    }
    if op == "get" {
        let store = Arc::clone(&agent_session.session);
        let session = store
            .try_lock()
            .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;
        return rpc_goal_state(&session);
    }
    if !matches!(op, "create" | "resume" | "pause" | "drop") {
        return Err(failure(
            "GOAL_RPC",
            "goal op must be get, create, resume, pause, or drop",
        ));
    }

    let enabled = agent_session.save_enabled();
    check_owner(owner, enabled)?;
    let store = Arc::clone(&agent_session.session);
    let mut session = store
        .try_lock_owned()
        .map_err(|_| failure("GOAL_BUSY", "session is busy"))?;

    let next = if let Some(resolution) = resolution {
        resolution.apply(&session)?
    } else {
        Some(match op {
            "create" => {
                require_no_plan_mode(&session)?;
                let objective = request
                    .get("objective")
                    .and_then(Value::as_str)
                    .ok_or_else(|| failure("GOAL_RPC", "objective is required when op=create"))?;
                if latest(&session)?.is_some_and(|(_, record)| !record.status.terminal()) {
                    return Err(failure(
                        "GOAL_EXISTS",
                        "resume or drop the existing goal before creating another",
                    ));
                }
                Record::new(objective.trim(), requested_budget(request)?)?
            }
            "resume" => {
                require_no_plan_mode(&session)?;
                let (_, record) = latest(&session)?
                    .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
                transition(record, Transition::Resume)?
            }
            "pause" if request.get("budget").is_some() => revised_budget(&session, request)?,
            "pause" => {
                let (_, record) = latest(&session)?
                    .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
                transition(record, Transition::Pause("paused by RPC host"))?
            }
            "drop" => {
                let (_, record) = latest(&session)?
                    .ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
                transition(record, Transition::Drop("dropped by RPC host"))?
            }
            _ => unreachable!(),
        })
    };

    check_owner(owner, enabled)?;
    let journal_changed = next.is_some();
    if let Some(next) = next {
        append(&mut session, &next)?;
        agent_session.invalidate_background_compaction();
    }
    let persistence = persist(&mut session, owner, enabled).await;
    let mut value = rpc_goal_state(&session)?;
    if let Some(resolution) = resolution {
        value["resolution"] = json!({
            "kind": resolution.name(), "journalChanged": journal_changed,
        });
    }
    value["persistence"] = Value::String(
        match &persistence {
            GoalPersistence::MemoryOnly => "memory_only",
            GoalPersistence::Saved => "saved",
            GoalPersistence::Unconfirmed { .. } => "unconfirmed",
        }
        .to_string(),
    );
    if let GoalPersistence::Unconfirmed { reason } = persistence {
        value["persistenceError"] = Value::String(reason);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_uses_omp_status_spelling_and_additive_native_fields() {
        let mut record = Record::new(
            "Ship the feature",
            GoalBudget {
                max_prompts: 3,
                token_budget: 90,
            },
        )
        .unwrap();
        record.prompts_started = 3;
        record.tokens_spent = 25;
        let value = result_value(Some(&record));
        assert_eq!(value["goal"]["status"], "budget-limited");
        assert_eq!(value["goal"]["tokenBudget"], 90);
        assert_eq!(value["goal"]["tokensUsed"], 25);
        assert_eq!(value["goal"]["maxPrompts"], 3);
        assert_eq!(value["goal"]["promptsStarted"], 3);
        assert_eq!(value["state"]["enabled"], false);
    }

    #[test]
    fn rpc_budget_is_bounded_and_defaults_match_native_goal_budget() {
        assert_eq!(requested_budget(&json!({})).unwrap(), GoalBudget::default());
        assert!(requested_budget(&json!({"token_budget":0})).is_err());
        assert!(requested_budget(&json!({"token_budget":1.5})).is_err());
        assert!(requested_budget(&json!({"max_prompts":0})).is_err());
        assert!(requested_budget(&json!({"max_prompts":1025})).is_err());
        assert_eq!(
            requested_budget(&json!({"token_budget":17,"max_prompts":2})).unwrap(),
            GoalBudget {
                max_prompts: 2,
                token_budget: 17
            }
        );
    }

    #[test]
    fn read_only_projection_follows_current_branch() {
        let mut session = Session::in_memory();
        let root = session.append_custom_entry("root".into(), None);
        let first = Record::new("first", GoalBudget::default()).unwrap();
        let first_entry = append(&mut session, &first).unwrap();
        assert!(session.navigate_to(&root));
        assert_eq!(rpc_goal_state(&session).unwrap()["goal"], Value::Null);
        let second = Record::new("second", GoalBudget::default()).unwrap();
        append(&mut session, &second).unwrap();
        assert_eq!(rpc_goal_state(&session).unwrap()["goal"]["id"], second.id);
        assert!(session.navigate_to(&first_entry));
        assert_eq!(rpc_goal_state(&session).unwrap()["goal"]["id"], first.id);
    }
}
