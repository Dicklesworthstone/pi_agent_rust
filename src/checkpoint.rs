//! Checkpoint / rewind / fresh / retry session operations (bd-cv653.3.7).
//!
//! `checkpoint` marks the current leaf with a Custom entry {name,
//! token_estimate, note, message_count} — cheap, no summarization.
//! `rewind` collapses the span from a checkpoint to now into a concise
//! report (the compaction summarizer, budget-capped with the local
//! fallback), replacing that span in the ACTIVE context while the full
//! span stays in the tree (append-only, non-destructive by construction).
//! `fresh` resets provider stream state (new session id) with the
//! transcript untouched. `retry` re-issues the last user turn from the
//! active context (the tree keeps the original path).

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{Message, UserContent, UserMessage};
use crate::session::{CustomEntry, Session, SessionEntry, SessionMessage};

/// Tool-result schema tag for checkpoint/rewind operations.
pub const CHECKPOINT_SCHEMA: &str = "pi.checkpoint.v1";

/// A checkpoint marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub schema: String,
    pub name: String,
    pub note: Option<String>,
    pub token_estimate: u64,
    /// Active message count at mark time: the rewind span boundary.
    pub message_count: usize,
    pub at_ms: i64,
    /// Session-tree entry id of the checkpoint marker itself. Derived from
    /// the tree at mark/find time (never stored inside the entry data);
    /// rewind entries reference it so context rebuilds can replay the
    /// collapse durably.
    #[serde(skip_serializing, default)]
    pub entry_id: Option<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Estimate tokens for active messages (chars/4 heuristic, matching the
/// compaction estimator's spirit).
#[must_use]
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|message| match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => text.len(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        crate::model::ContentBlock::Text(text) => text.text.len(),
                        crate::model::ContentBlock::Thinking(thinking) => thinking.thinking.len(),
                        crate::model::ContentBlock::RedactedThinking(_)
                        | crate::model::ContentBlock::Image(_)
                        | crate::model::ContentBlock::ToolCall(_) => 0,
                    })
                    .sum(),
            },
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .map(|block| match block {
                    crate::model::ContentBlock::Text(text) => text.text.len(),
                    crate::model::ContentBlock::Thinking(thinking) => thinking.thinking.len(),
                    crate::model::ContentBlock::RedactedThinking(_)
                    | crate::model::ContentBlock::Image(_)
                    | crate::model::ContentBlock::ToolCall(_) => 0,
                })
                .sum(),
            Message::ToolResult(result) => result
                .content
                .iter()
                .map(|block| match block {
                    crate::model::ContentBlock::Text(text) => text.text.len(),
                    crate::model::ContentBlock::Thinking(thinking) => thinking.thinking.len(),
                    crate::model::ContentBlock::RedactedThinking(_)
                    | crate::model::ContentBlock::Image(_)
                    | crate::model::ContentBlock::ToolCall(_) => 0,
                })
                .sum(),
            Message::Custom(_) => 0,
        })
        .sum();
    (chars / 4) as u64
}

/// Mark a checkpoint at the current leaf.
pub fn mark_checkpoint(
    session: &mut Session,
    name: &str,
    note: Option<&str>,
    active_messages: &[Message],
) -> Checkpoint {
    let checkpoint = Checkpoint {
        schema: CHECKPOINT_SCHEMA.to_string(),
        name: if name.trim().is_empty() {
            "checkpoint".to_string()
        } else {
            name.trim().to_string()
        },
        note: note
            .filter(|note| !note.trim().is_empty())
            .map(str::to_string),
        token_estimate: estimate_tokens(active_messages),
        message_count: active_messages.len(),
        at_ms: now_ms(),
        entry_id: None,
    };
    let entry_id = session.append_custom_entry(
        "checkpoint".to_string(),
        Some(serde_json::to_value(&checkpoint).unwrap_or_default()),
    );
    Checkpoint {
        entry_id: Some(entry_id),
        ..checkpoint
    }
}

/// Find a checkpoint by name, or the latest when name is None.
#[must_use]
pub fn find_checkpoint(session: &Session, name: Option<&str>) -> Option<Checkpoint> {
    session
        .entries_for_current_path()
        .iter()
        .rev()
        .filter_map(|entry| {
            let SessionEntry::Custom(custom) = entry else {
                return None;
            };
            if custom.custom_type != "checkpoint" {
                return None;
            }
            let mut checkpoint: Checkpoint =
                serde_json::from_value(custom.data.clone().unwrap_or_default()).ok()?;
            checkpoint.entry_id.clone_from(&custom.base.id);
            Some(checkpoint)
        })
        .find(|checkpoint| name.is_none_or(|name| checkpoint.name == name))
}

/// The outcome of a rewind.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindOutcome {
    pub schema: String,
    pub checkpoint: String,
    /// Tree entry id of the checkpoint this rewind collapsed to. Context
    /// rebuilds (compaction apply, resume, per-prompt SDK rebuilds) use it
    /// to replay the collapse; absent on legacy entries (no replay).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub checkpoint_entry_id: Option<String>,
    /// Messages collapsed out of the active context.
    pub collapsed_messages: usize,
    /// The report now standing in for the span.
    pub summary: String,
    pub summary_tokens_estimate: u64,
    /// The tree retained everything (always true here).
    pub tree_preserved: bool,
}

/// Summarize the span from a checkpoint to now via the compaction
/// summarizer (budget-capped, local fallback).
///
/// # Errors
/// Propagates compaction summarization errors.
pub async fn summarize_span(
    span: &[Message],
    provider: std::sync::Arc<dyn crate::provider::Provider>,
    api_key: &str,
    settings: &crate::compaction::ResolvedCompactionSettings,
) -> Result<String> {
    if span.is_empty() {
        return Ok(String::new());
    }
    let session_messages: Vec<SessionMessage> =
        span.iter().cloned().map(SessionMessage::from).collect();
    let tokens_before = estimate_tokens(span);
    let preparation = crate::compaction::CompactionPreparation {
        first_kept_entry_id: "rewind-span".to_string(),
        messages_to_summarize: session_messages,
        turn_prefix_messages: Vec::new(),
        is_split_turn: false,
        tokens_before,
        previous_summary: None,
        file_ops: crate::compaction::FileOperations::default(),
        settings: settings.clone(),
    };
    let result = crate::compaction::compact(
        preparation,
        provider,
        api_key,
        Some(
            "Summarize this span as a concise rewind report: what was explored, \
             what was decided, what remains open. Preserve file paths, \
             decisions, and constraints.",
        ),
    )
    .await?;
    Ok(result.summary)
}

/// Apply a rewind to the agent's active context: collapse the span into a
/// single report message. The session tree keeps every original entry.
pub fn apply_rewind_to_active(
    agent: &mut crate::agent::Agent,
    checkpoint: &Checkpoint,
    summary: String,
) -> RewindOutcome {
    let total = agent.messages().len();
    let mut boundary = checkpoint.message_count.min(total);
    // The recorded count is positional and the active list is renumbered by
    // compaction/resume, so it can land mid-turn. Truncating there would
    // leave a trailing assistant `tool_use` without its `tool_result` — the
    // next request then fails provider validation. Walk back to a user-turn
    // start so the kept prefix always ends with a completed turn.
    while boundary > 0
        && boundary < total
        && !matches!(agent.messages()[boundary], Message::User(_))
    {
        boundary -= 1;
    }
    let collapsed = total - boundary;
    agent.truncate_messages(boundary);
    if !summary.is_empty() {
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Text(rewind_report_text(&checkpoint.name, &summary)),
            timestamp: now_ms(),
        }));
    }
    RewindOutcome {
        schema: CHECKPOINT_SCHEMA.to_string(),
        checkpoint: checkpoint.name.clone(),
        checkpoint_entry_id: checkpoint.entry_id.clone(),
        collapsed_messages: collapsed,
        summary_tokens_estimate: (summary.len() / 4) as u64,
        summary,
        tree_preserved: true,
    }
}

/// The user-visible rewind report message. One definition shared by the
/// live rewind paths and the session-tree rebuild so the replayed context
/// is byte-identical to what the user saw.
#[must_use]
pub fn rewind_report_text(checkpoint_name: &str, summary: &str) -> String {
    format!(
        "[REWIND REPORT: {checkpoint_name}]\nThe span since this checkpoint was collapsed into \
         this report. The full span remains in the session tree.\n\n{summary}"
    )
}

/// Reset provider stream state (new session id) with the transcript
/// untouched. Returns the new session id.
pub fn fresh_stream_state(agent: &mut crate::agent::Agent, session: &mut Session) -> String {
    // A millisecond stamp alone can collide across rapid calls; the uuid
    // suffix keeps every /fresh a genuinely new provider session id.
    let new_id = format!("fresh-{}-{}", now_ms(), uuid::Uuid::new_v4().simple());
    agent.stream_options_mut().session_id = Some(new_id.clone());
    session.append_custom_entry(
        "fresh".to_string(),
        Some(serde_json::json!({
            "schema": "pi.fresh.v1",
            "newSessionId": new_id,
            "reason": "operator /fresh: provider cache + stream bookkeeping reset",
        })),
    );
    new_id
}

/// A prepared `/retry`: the abandoned turn's text plus its tree entry id.
pub struct RetryPreparation {
    /// Text of the abandoned user turn to re-issue.
    pub text: String,
    /// Tree entry id of the abandoned user entry. After preparation it is
    /// OFF the active path (its subtree stays in the file for `/tree`).
    pub abandoned_entry_id: String,
}

/// Read-only plan for a classic `/retry` sibling branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPlan {
    /// Text of the abandoned user turn to re-issue.
    pub text: String,
    /// Tree entry id of the abandoned user entry.
    pub abandoned_entry_id: String,
    /// Leaf id observed while planning. Apply refuses if the live leaf moved.
    pub original_leaf_id: Option<String>,
    /// Parent of the abandoned user entry. The retried turn lands here.
    pub expected_parent_id: Option<String>,
}

/// Why [`apply_retry_plan`] refused to move the leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryApplyError {
    /// The live leaf is no longer the leaf the plan was computed against.
    LeafChanged,
    /// The abandoned user entry disappeared from the tree.
    AbandonedMissing,
    /// The abandoned entry's parent no longer matches the plan.
    ParentMismatch,
    /// Navigation to the expected parent failed.
    NavigateFailed,
}

impl fmt::Display for RetryApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::LeafChanged => "session leaf changed before retry could be applied",
            Self::AbandonedMissing => "retry candidate disappeared before it could be applied",
            Self::ParentMismatch => "retry candidate parent changed before it could be applied",
            Self::NavigateFailed => "retry could not move the leaf to the candidate parent",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDisposition {
    Candidate,
    Barrier,
    Skip,
}

#[derive(Debug, Clone)]
struct ProjectedPathMessage {
    disposition: RetryDisposition,
    text: Option<String>,
    entry_id: Option<String>,
    parent_id: Option<String>,
}

impl ProjectedPathMessage {
    const fn candidate(text: String, entry_id: String, parent_id: Option<String>) -> Self {
        Self {
            disposition: RetryDisposition::Candidate,
            text: Some(text),
            entry_id: Some(entry_id),
            parent_id,
        }
    }

    const fn barrier() -> Self {
        Self {
            disposition: RetryDisposition::Barrier,
            text: None,
            entry_id: None,
            parent_id: None,
        }
    }

    const fn skip() -> Self {
        Self {
            disposition: RetryDisposition::Skip,
            text: None,
            entry_id: None,
            parent_id: None,
        }
    }
}

struct RetryPathProjection {
    projected: Vec<ProjectedPathMessage>,
    checkpoint_positions: HashMap<String, usize>,
}

impl RetryPathProjection {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            projected: Vec::with_capacity(capacity),
            checkpoint_positions: HashMap::new(),
        }
    }

    fn append(&mut self, entry: &SessionEntry) {
        match entry {
            SessionEntry::Message(message_entry) => match &message_entry.message {
                SessionMessage::User {
                    content: UserContent::Text(text),
                    ..
                } => match message_entry.base.id.clone() {
                    Some(entry_id) => self.projected.push(ProjectedPathMessage::candidate(
                        text.clone(),
                        entry_id,
                        message_entry.base.parent_id.clone(),
                    )),
                    None => self.projected.push(ProjectedPathMessage::barrier()),
                },
                SessionMessage::User {
                    content: UserContent::Blocks(_),
                    ..
                } => self.projected.push(ProjectedPathMessage::barrier()),
                SessionMessage::BashExecution { extra, .. } => {
                    let excluded = extra
                        .get("excludeFromContext")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if !excluded {
                        self.projected.push(ProjectedPathMessage::barrier());
                    }
                }
                _ => {}
            },
            SessionEntry::BranchSummary(_) => {
                self.projected.push(ProjectedPathMessage::barrier());
            }
            SessionEntry::Custom(custom) if custom.custom_type == "checkpoint" => {
                if let Some(id) = &custom.base.id {
                    self.checkpoint_positions
                        .insert(id.clone(), self.projected.len());
                }
            }
            SessionEntry::Custom(custom) if custom.custom_type == "rewind" => {
                self.apply_rewind(custom);
            }
            _ => {}
        }
    }

    fn apply_rewind(&mut self, custom: &CustomEntry) {
        let checkpoint_entry_id = custom
            .data
            .as_ref()
            .and_then(|data| data.get("checkpointEntryId"))
            .and_then(serde_json::Value::as_str);
        let Some(boundary) = checkpoint_entry_id
            .and_then(|id| self.checkpoint_positions.get(id))
            .copied()
        else {
            return;
        };
        self.projected.truncate(boundary);
        self.checkpoint_positions
            .retain(|_, position| *position <= boundary);
        let has_report = custom
            .data
            .as_ref()
            .and_then(|data| data.get("summary"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|summary| !summary.is_empty());
        if has_report {
            self.projected.push(ProjectedPathMessage::skip());
        }
    }
}

fn project_retry_path(session: &Session) -> Vec<ProjectedPathMessage> {
    let path = session.entries_for_current_path();
    let last_compaction = path
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::Compaction(_)));
    let mut projection = RetryPathProjection::with_capacity(path.len().saturating_add(1));

    if let Some(compaction_index) = last_compaction {
        let Some(SessionEntry::Compaction(compaction)) = path.get(compaction_index).copied() else {
            return Vec::new();
        };
        projection.projected.push(ProjectedPathMessage::barrier());
        let first_kept_entry_id = compaction.first_kept_entry_id.clone();
        let has_kept_entry = path
            .iter()
            .any(|entry| entry.base_id().is_some_and(|id| id == &first_kept_entry_id));
        let mut keep = false;
        let mut past_compaction = false;
        for (index, entry) in path.iter().enumerate() {
            if index == compaction_index {
                past_compaction = true;
            }
            if !keep {
                if has_kept_entry {
                    if entry.base_id().is_some_and(|id| id == &first_kept_entry_id) {
                        keep = true;
                    } else {
                        continue;
                    }
                } else if past_compaction {
                    keep = true;
                } else {
                    continue;
                }
            }
            projection.append(entry);
        }
    } else {
        for entry in path {
            projection.append(entry);
        }
    }

    projection.projected
}

/// Read-only retry selection over the compaction/rewind-aware Session path.
///
/// Walking from the newest projected item: `Skip` (synthetic rewind reports)
/// is ignored, `Barrier` (missing-ID text, user blocks, bash, branch/compaction
/// summaries) refuses the whole retry, and `Candidate` is the durable text
/// user to re-issue. Genuine user text starting with `[REWIND REPORT:` remains
/// a candidate because it is stored as a real user entry, not a rewind Custom.
#[must_use]
pub fn plan_retry(session: &Session) -> Option<RetryPlan> {
    let projected = project_retry_path(session);
    for item in projected.into_iter().rev() {
        match item.disposition {
            RetryDisposition::Skip => {}
            RetryDisposition::Barrier => return None,
            RetryDisposition::Candidate => {
                return Some(RetryPlan {
                    text: item.text?,
                    abandoned_entry_id: item.entry_id?,
                    original_leaf_id: session.leaf_id().map(str::to_string),
                    expected_parent_id: item.parent_id,
                });
            }
        }
    }
    None
}

/// Move the leaf to the planned parent after revalidating the original leaf,
/// abandoned entry, and expected parent. Does not persist.
pub fn apply_retry_plan(
    session: &mut Session,
    plan: &RetryPlan,
) -> std::result::Result<(), RetryApplyError> {
    if session.leaf_id() != plan.original_leaf_id.as_deref() {
        return Err(RetryApplyError::LeafChanged);
    }
    let abandoned = session
        .get_entry(&plan.abandoned_entry_id)
        .ok_or(RetryApplyError::AbandonedMissing)?;
    if abandoned.base().parent_id != plan.expected_parent_id {
        return Err(RetryApplyError::ParentMismatch);
    }
    let navigated = if let Some(parent) = &plan.expected_parent_id {
        session.navigate_to(parent)
    } else {
        session.reset_leaf();
        true
    };
    if navigated {
        Ok(())
    } else {
        Err(RetryApplyError::NavigateFailed)
    }
}

/// Prepare a `/retry` against the DURABLE session tree.
///
/// Moves the leaf to the parent of the last projected retryable user turn, so
/// the retried turn lands as a SIBLING branch instead of appending after the
/// abandoned turn and its response. The abandoned span stays in the tree but
/// leaves `entries_for_current_path`, which is exactly what a restart
/// rehydrates.
#[must_use]
pub fn prepare_retry_branch(session: &mut Session) -> Option<RetryPreparation> {
    let plan = plan_retry(session)?;
    apply_retry_plan(session, &plan).ok()?;
    Some(RetryPreparation {
        text: plan.text,
        abandoned_entry_id: plan.abandoned_entry_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[test]
    fn mark_and_find_checkpoint_roundtrip() {
        let mut session = Session::in_memory();
        let messages = vec![user_text("hello"), user_text("world")];
        let checkpoint = mark_checkpoint(&mut session, "alpha", Some("before refactor"), &messages);
        assert_eq!(checkpoint.name, "alpha");
        assert_eq!(checkpoint.message_count, 2);
        assert!(checkpoint.token_estimate > 0);

        let found = find_checkpoint(&session, Some("alpha")).expect("find by name");
        assert_eq!(found.name, "alpha");
        let latest = find_checkpoint(&session, None).expect("latest");
        assert_eq!(latest.name, "alpha");
        assert!(find_checkpoint(&session, Some("missing")).is_none());
    }

    #[test]
    fn estimate_tokens_scales_with_content() {
        let small = estimate_tokens(&[user_text("hi")]);
        let big = estimate_tokens(&[user_text(&"x".repeat(4000))]);
        assert!(big > small);
    }

    fn session_user(text: &str) -> SessionMessage {
        SessionMessage::from(user_text(text))
    }

    fn session_assistant(text: &str) -> SessionMessage {
        SessionMessage::from(Message::Assistant(std::sync::Arc::new(
            crate::model::AssistantMessage {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(text),
                )],
                ..Default::default()
            },
        )))
    }

    #[test]
    fn plan_retry_selects_last_text_user_and_apply_moves_leaf_to_parent() {
        let mut session = Session::in_memory();
        let first_user = session.append_message(session_user("first question"));
        let first_answer = session.append_message(session_assistant("first answer"));
        let abandoned = session.append_message(session_user("second question"));
        let _abandoned_answer = session.append_message(session_assistant("second answer"));
        let original_leaf = session.leaf_id().map(str::to_string);

        let plan = plan_retry(&session).expect("plan");
        assert_eq!(plan.text, "second question");
        assert_eq!(plan.abandoned_entry_id, abandoned);
        assert_eq!(plan.original_leaf_id, original_leaf);
        assert_eq!(
            plan.expected_parent_id.as_deref(),
            Some(first_answer.as_str())
        );

        apply_retry_plan(&mut session, &plan).expect("apply");
        assert_eq!(session.leaf_id(), Some(first_answer.as_str()));
        assert!(session.get_entry(&first_user).is_some());
        assert!(session.get_entry(&abandoned).is_some());
    }

    #[test]
    fn plan_retry_treats_literal_rewind_report_user_text_as_candidate() {
        let mut session = Session::in_memory();
        session.append_message(session_user("earlier"));
        let report = session.append_message(session_user(
            "[REWIND REPORT: alpha]\nthis is a genuine user prompt",
        ));
        let plan = plan_retry(&session).expect("literal rewind-report text is retryable");
        assert_eq!(plan.abandoned_entry_id, report);
        assert!(plan.text.starts_with("[REWIND REPORT: alpha]"));
    }

    #[test]
    fn plan_retry_skips_synthetic_rewind_and_does_not_retry_hidden_turn() {
        let mut session = Session::in_memory();
        let foundation = session.append_message(session_user("foundation"));
        session.append_message(session_assistant("foundation answer"));
        let checkpoint = mark_checkpoint(&mut session, "alpha", None, &[user_text("foundation")]);
        let hidden = session.append_message(session_user("hidden later turn"));
        session.append_message(session_assistant("hidden answer"));
        let outcome = RewindOutcome {
            schema: CHECKPOINT_SCHEMA.to_string(),
            checkpoint: "alpha".to_string(),
            checkpoint_entry_id: checkpoint.entry_id,
            collapsed_messages: 2,
            summary: "collapsed the hidden span".to_string(),
            summary_tokens_estimate: 1,
            tree_preserved: true,
        };
        session.append_custom_entry(
            "rewind".to_string(),
            Some(serde_json::to_value(&outcome).expect("serialize")),
        );

        let plan = plan_retry(&session).expect("foundation remains retryable");
        assert_eq!(plan.abandoned_entry_id, foundation);
        assert_ne!(plan.abandoned_entry_id, hidden);
        assert_eq!(plan.text, "foundation");
    }

    #[test]
    fn plan_retry_refuses_when_only_hidden_rewind_span_exists() {
        let mut session = Session::in_memory();
        let checkpoint = mark_checkpoint(&mut session, "alpha", None, &[]);
        session.append_message(session_user("hidden"));
        session.append_message(session_assistant("hidden answer"));
        let outcome = RewindOutcome {
            schema: CHECKPOINT_SCHEMA.to_string(),
            checkpoint: "alpha".to_string(),
            checkpoint_entry_id: checkpoint.entry_id,
            collapsed_messages: 2,
            summary: "nothing visible left".to_string(),
            summary_tokens_estimate: 1,
            tree_preserved: true,
        };
        session.append_custom_entry(
            "rewind".to_string(),
            Some(serde_json::to_value(&outcome).expect("serialize")),
        );
        assert!(plan_retry(&session).is_none());
    }

    #[test]
    fn plan_retry_treats_missing_id_text_as_barrier() {
        let mut session = Session::in_memory();
        session.append_message(session_user("older prompt"));
        session.append_message(session_user("missing id prompt"));
        if let Some(entry) = session.entries.last_mut() {
            entry.base_mut().id = None;
        }
        assert!(
            plan_retry(&session).is_none(),
            "missing-ID text must not skip back to an older prompt"
        );
    }

    #[test]
    fn plan_retry_treats_user_blocks_as_barrier() {
        let mut session = Session::in_memory();
        session.append_message(session_user("older prompt"));
        session.append_message(SessionMessage::User {
            content: UserContent::Blocks(vec![crate::model::ContentBlock::Text(
                crate::model::TextContent::new("image prompt"),
            )]),
            timestamp: Some(0),
        });
        assert!(plan_retry(&session).is_none());
    }

    #[test]
    fn plan_retry_treats_bash_execution_as_barrier() {
        let mut session = Session::in_memory();
        session.append_message(session_user("older prompt"));
        session.append_bash_execution(
            "echo hi".to_string(),
            "hi".to_string(),
            0,
            false,
            false,
            None,
        );
        assert!(plan_retry(&session).is_none());
    }

    #[test]
    fn plan_retry_treats_branch_summary_as_barrier() {
        let mut session = Session::in_memory();
        let older = session.append_message(session_user("older prompt"));
        session.append_branch_summary(older, "left a branch".to_string(), None, None);
        assert!(plan_retry(&session).is_none());
    }

    #[test]
    fn plan_retry_treats_compaction_summary_without_later_user_as_barrier() {
        let mut session = Session::in_memory();
        session.append_message(session_user("compacted away"));
        session.append_compaction(
            "summary".to_string(),
            "missing-kept-entry".to_string(),
            10,
            None,
            None,
        );
        assert!(plan_retry(&session).is_none());
    }

    #[test]
    fn apply_retry_plan_refuses_when_leaf_changed() {
        let mut session = Session::in_memory();
        session.append_message(session_user("question"));
        session.append_message(session_assistant("answer"));
        let plan = plan_retry(&session).expect("plan");
        session.append_message(session_user("intervening"));
        assert_eq!(
            apply_retry_plan(&mut session, &plan),
            Err(RetryApplyError::LeafChanged)
        );
    }
}
