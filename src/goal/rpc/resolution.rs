//! Explicit host decisions on the RPC loop's existing guarded stop route.
//! Evidence is an attestation, not a command, verifier or provider response.

use super::{Record, Result, Session, Transition, failure, latest, revision, transition};
use crate::goal::GoalReconciliation;
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) enum Resolution<'a> {
    Complete {
        evidence: &'a str,
        reviewed: &'a str,
    },
    Save {
        reviewed: &'a str,
    },
    Reconcile {
        outcome: GoalReconciliation,
        evidence: &'a str,
        reviewed: &'a str,
    },
}

impl Resolution<'_> {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Complete { .. } => "complete",
            Self::Save { .. } => "save",
            Self::Reconcile { .. } => "reconcile",
        }
    }

    /// Verification and the caller's append/save share one owned session lock.
    /// Saving retains even terminal/interrupted state without acknowledging it.
    pub(super) fn apply(self, session: &Session) -> Result<Option<Record>> {
        let reviewed = match self {
            Self::Complete { reviewed, .. }
            | Self::Save { reviewed }
            | Self::Reconcile { reviewed, .. } => reviewed,
        };
        let current = latest(session)?;
        if reviewed != revision(session, &current)? {
            return Err(failure(
                "GOAL_STALE",
                "session, branch or goal changed; get and review it again. Nothing was changed.",
            ));
        }
        let (_, record) =
            current.ok_or_else(|| failure("GOAL_MISSING", "this branch has no goal"))?;
        match self {
            Self::Complete { evidence, .. } => {
                transition(record, Transition::Complete(evidence)).map(Some)
            }
            Self::Save { .. } => Ok(None),
            Self::Reconcile {
                outcome, evidence, ..
            } => transition(record, Transition::Reconcile { outcome, evidence }).map(Some),
        }
    }
}

/// Keep the established pause envelope so these operations receive the same
/// idle admission and returned-input preflight as all other goal mutations.
/// Absence preserves the original pause/budget behavior; null is not absence.
pub(super) fn parse<'a>(request: &'a Value, op: &str) -> Result<Option<Resolution<'a>>> {
    let Some(raw) = request.get("resolution") else {
        return Ok(None);
    };
    if op != "pause" {
        return Err(failure("GOAL_RPC", "resolution requires op=pause"));
    }
    if [
        "budget",
        "objective",
        "token_budget",
        "max_prompts",
        "reason",
    ]
    .iter()
    .any(|field| request.get(*field).is_some())
    {
        return Err(failure(
            "GOAL_RPC",
            "resolution cannot be combined with goal creation or budget fields",
        ));
    }
    let reviewed = request
        .get("revision")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 68
                && value.starts_with("gr1-")
                && value[4..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| failure("GOAL_RPC", "resolution requires the revision from goal get"))?;
    let fields = raw.as_object().ok_or_else(invalid)?;
    let resolution = match fields.get("kind").and_then(Value::as_str) {
        Some("complete") if fields.len() == 2 && fields.contains_key("evidence") => {
            let evidence = fields["evidence"].as_str().ok_or_else(invalid)?;
            super::super::validate_text(evidence, "completion evidence")?;
            Resolution::Complete { evidence, reviewed }
        }
        Some("save") if fields.len() == 1 => Resolution::Save { reviewed },
        Some("reconcile")
            if fields.len() == 3
                && fields.contains_key("outcome")
                && fields.contains_key("evidence") =>
        {
            let outcome = match fields["outcome"].as_str() {
                Some("pause") => GoalReconciliation::Pause,
                Some("complete") => GoalReconciliation::Complete,
                Some("drop") => GoalReconciliation::Drop,
                _ => return Err(invalid()),
            };
            let evidence = fields["evidence"].as_str().ok_or_else(invalid)?;
            super::super::validate_text(evidence, "interruption reconciliation evidence")?;
            Resolution::Reconcile {
                outcome,
                evidence,
                reviewed,
            }
        }
        _ => return Err(invalid()),
    };
    Ok(Some(resolution))
}

fn invalid() -> crate::error::Error {
    failure(
        "GOAL_RPC",
        "resolution must be exactly {kind:complete,evidence:<nonblank text>}, {kind:save}, or {kind:reconcile,outcome:pause|complete|drop,evidence:<nonblank text>}",
    )
}

#[cfg(test)]
mod reconciliation_tests;
