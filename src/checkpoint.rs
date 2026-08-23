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

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{Message, UserContent, UserMessage};
use crate::session::{Session, SessionEntry, SessionMessage};

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

/// Extract the last user turn's text for a retry (the original path stays
/// in the tree; the active context rewinds to just before it).
#[must_use]
pub fn take_last_user_turn(agent: &mut crate::agent::Agent) -> Option<String> {
    // Skip synthetic user messages (rewind reports) — retrying one of those
    // instead of the real prompt would replay bookkeeping text as a turn.
    let last_user_index = agent.messages().iter().rposition(|message| {
        matches!(
            message,
            Message::User(user)
                if !matches!(
                    &user.content,
                    UserContent::Text(text) if text.starts_with("[REWIND REPORT:")
                )
        )
    })?;
    let text = match &agent.messages()[last_user_index] {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(_) => return None,
        },
        _ => return None,
    };
    agent.truncate_messages(last_user_index);
    Some(text)
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
}
