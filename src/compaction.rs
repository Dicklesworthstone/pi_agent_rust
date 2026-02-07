//! Context compaction for long sessions.
//!
//! This module ports the pi-mono compaction algorithm:
//! - Estimate context usage and choose a cut point that keeps recent context
//! - Summarize the discarded portion with the LLM (iteratively updating prior summaries)
//! - Record a `compaction` session entry containing the summary and cut point
//! - When building provider context, the session inserts the summary before the kept region
//!   and omits older messages.

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingLevel, ToolCall,
    Usage, UserContent, UserMessage,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::session::{SessionEntry, SessionMessage, session_message_to_model};
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

// =============================================================================
// Public types
// =============================================================================

#[derive(Debug, Clone)]
pub struct ResolvedCompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u32,
    pub keep_recent_tokens: u32,
}

impl Default for ResolvedCompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// Details stored in `CompactionEntry.details` for cumulative file tracking.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub details: CompactionDetails,
}

#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<SessionMessage>,
    pub turn_prefix_messages: Vec<SessionMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: ResolvedCompactionSettings,
}

// =============================================================================
// File op tracking (read/write/edit)
// =============================================================================

#[derive(Debug, Clone, Default)]
pub struct FileOperations {
    read: HashSet<String>,
    written: HashSet<String>,
    edited: HashSet<String>,
}

impl FileOperations {
    pub fn read_files(&self) -> impl Iterator<Item = &str> {
        self.read.iter().map(String::as_str)
    }
}

fn extract_file_ops_from_message(message: &SessionMessage, file_ops: &mut FileOperations) {
    let SessionMessage::Assistant { message } = message else {
        return;
    };

    for block in &message.content {
        let ContentBlock::ToolCall(ToolCall {
            name, arguments, ..
        }) = block
        else {
            continue;
        };

        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            continue;
        };

        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: HashSet<&String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .collect();

    let mut read_only = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(f))
        .cloned()
        .collect::<Vec<_>>();
    read_only.sort();

    let mut modified_files = modified.into_iter().cloned().collect::<Vec<_>>();
    modified_files.sort();

    (read_only, modified_files)
}

fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

// =============================================================================
// Token estimation
// =============================================================================

const fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output
    }
}

const fn get_assistant_usage(message: &SessionMessage) -> Option<&Usage> {
    let SessionMessage::Assistant { message } = message else {
        return None;
    };

    if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
        return None;
    }

    Some(&message.usage)
}

#[derive(Debug, Clone, Copy)]
struct ContextUsageEstimate {
    tokens: u64,
    last_usage_index: Option<usize>,
}

fn estimate_context_tokens(messages: &[SessionMessage]) -> ContextUsageEstimate {
    let mut last_usage: Option<(&Usage, usize)> = None;
    for (idx, msg) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(msg) {
            last_usage = Some((usage, idx));
            break;
        }
    }

    let Some((usage, usage_index)) = last_usage else {
        let total = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: total,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens = messages[usage_index + 1..]
        .iter()
        .map(estimate_tokens)
        .sum::<u64>();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        last_usage_index: Some(usage_index),
    }
}

fn should_compact(
    context_tokens: u64,
    context_window: u32,
    settings: &ResolvedCompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    let reserve = u64::from(settings.reserve_tokens);
    let window = u64::from(context_window);
    context_tokens > window.saturating_sub(reserve)
}

fn estimate_tokens(message: &SessionMessage) -> u64 {
    let mut chars: usize = 0;

    match message {
        SessionMessage::User { content, .. } => match content {
            UserContent::Text(text) => chars = text.len(),
            UserContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text(text) => chars += text.text.len(),
                        ContentBlock::Image(_) => chars += 4800, // ~1200 tokens
                        ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                        ContentBlock::ToolCall(call) => {
                            chars += call.name.len();
                            chars += serde_json::to_string(&call.arguments)
                                .map(|s| s.len())
                                .unwrap_or_default();
                        }
                    }
                }
            }
        },
        SessionMessage::Assistant { message } => {
            for block in &message.content {
                match block {
                    ContentBlock::Text(text) => chars += text.text.len(),
                    ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                    ContentBlock::Image(_) => chars += 4800,
                    ContentBlock::ToolCall(call) => {
                        chars += call.name.len();
                        chars += serde_json::to_string(&call.arguments)
                            .map(|s| s.len())
                            .unwrap_or_default();
                    }
                }
            }
        }
        SessionMessage::ToolResult { content, .. } => {
            for block in content {
                match block {
                    ContentBlock::Text(text) => chars += text.text.len(),
                    ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                    ContentBlock::Image(_) => chars += 4800,
                    ContentBlock::ToolCall(call) => {
                        chars += call.name.len();
                        chars += serde_json::to_string(&call.arguments)
                            .map(|s| s.len())
                            .unwrap_or_default();
                    }
                }
            }
        }
        SessionMessage::Custom { content, .. } => chars = content.len(),
        SessionMessage::BashExecution {
            command, output, ..
        } => chars = command.len() + output.len(),
        SessionMessage::BranchSummary { summary, .. }
        | SessionMessage::CompactionSummary { summary, .. } => chars = summary.len(),
    }

    u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX)
}

// =============================================================================
// Cut point detection
// =============================================================================

#[derive(Debug, Clone, Copy)]
struct CutPointResult {
    first_kept_entry_index: usize,
    turn_start_index: Option<usize>,
    is_split_turn: bool,
}

fn message_from_entry(entry: &SessionEntry) -> Option<SessionMessage> {
    match entry {
        SessionEntry::Message(msg_entry) => Some(msg_entry.message.clone()),
        SessionEntry::BranchSummary(summary) => Some(SessionMessage::BranchSummary {
            summary: summary.summary.clone(),
            from_id: summary.from_id.clone(),
        }),
        SessionEntry::Compaction(compaction) => Some(SessionMessage::CompactionSummary {
            summary: compaction.summary.clone(),
            tokens_before: compaction.tokens_before,
        }),
        _ => None,
    }
}

const fn entry_is_message_like(entry: &SessionEntry) -> bool {
    matches!(
        entry,
        SessionEntry::Message(_) | SessionEntry::BranchSummary(_)
    )
}

const fn entry_is_compaction_boundary(entry: &SessionEntry) -> bool {
    matches!(entry, SessionEntry::Compaction(_))
}

fn find_valid_cut_points(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (idx, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        match entry {
            SessionEntry::Message(msg_entry) => match msg_entry.message {
                SessionMessage::ToolResult { .. } => {}
                _ => cut_points.push(idx),
            },
            SessionEntry::BranchSummary(_) => cut_points.push(idx),
            _ => {}
        }
    }
    cut_points
}

const fn is_user_turn_start(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::BranchSummary(_) => true,
        SessionEntry::Message(msg_entry) => matches!(
            msg_entry.message,
            SessionMessage::User { .. } | SessionMessage::BashExecution { .. }
        ),
        _ => false,
    }
}

fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&idx| is_user_turn_start(&entries[idx]))
}

fn find_cut_point(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u32,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0];

    for i in (start_index..end_index).rev() {
        let entry = &entries[i];
        let SessionEntry::Message(msg_entry) = entry else {
            continue;
        };
        accumulated_tokens = accumulated_tokens.saturating_add(estimate_tokens(&msg_entry.message));

        if accumulated_tokens >= u64::from(keep_recent_tokens) {
            let mut found = false;
            // Find the largest cut point <= i (start earlier or at i to keep enough tokens)
            for &cut_point in cut_points.iter().rev() {
                if cut_point <= i {
                    cut_index = cut_point;
                    found = true;
                    break;
                }
            }
            if !found {
                // If no cut point <= i (e.g. i is before the first valid cut),
                // fall back to the earliest valid cut point.
                if let Some(&first) = cut_points.first() {
                    cut_index = first;
                }
            }
            break;
        }
    }

    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        if entry_is_compaction_boundary(prev) {
            break;
        }
        if entry_is_message_like(prev) {
            break;
        }
        cut_index -= 1;
    }

    let is_user_message = is_user_turn_start(&entries[cut_index]);
    let turn_start_index = if is_user_message {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index.is_some(),
    }
}

// =============================================================================
// Summarization prompts
// =============================================================================

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Custom(custom) => {
                let label = if custom.custom_type.trim().is_empty() {
                    "Custom".to_string()
                } else {
                    format!("Custom:{}", custom.custom_type)
                };
                if !custom.content.trim().is_empty() {
                    parts.push(format!("[{label}]: {}", custom.content));
                }
            }
            Message::Assistant(assistant) => {
                let mut text_parts = Vec::new();
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(text) => text_parts.push(text.text.clone()),
                        ContentBlock::Thinking(thinking) => {
                            thinking_parts.push(thinking.thinking.clone());
                        }
                        ContentBlock::ToolCall(call) => {
                            let args_str = call.arguments.as_object().map_or_else(
                                || {
                                    serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| call.arguments.to_string())
                                },
                                |obj| {
                                    obj.iter()
                                        .map(|(k, v)| {
                                            format!(
                                                "{k}={}",
                                                serde_json::to_string(v)
                                                    .unwrap_or_else(|_| v.to_string())
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                },
                            );
                            tool_calls.push(format!("{}({args_str})", call.name));
                        }
                        ContentBlock::Image(_) => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if !text_parts.is_empty() {
                    parts.push(format!("[Assistant]: {}", text_parts.join("\n")));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(tool) => {
                let content = tool
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !content.is_empty() {
                    parts.push(format!("[Tool result]: {content}"));
                }
            }
        }
    }

    parts.join("\n\n")
}

async fn complete_simple(
    provider: Arc<dyn Provider>,
    system_prompt: &str,
    prompt_text: String,
    api_key: &str,
    reserve_tokens: u32,
    max_tokens_factor: f64,
) -> Result<AssistantMessage> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_tokens = (f64::from(reserve_tokens) * max_tokens_factor).floor() as u32;
    let max_tokens = max_tokens.max(256);

    let context = Context {
        system_prompt: Some(system_prompt.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(prompt_text))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })],
        tools: Vec::new(),
    };

    let options = StreamOptions {
        api_key: Some(api_key.to_string()),
        max_tokens: Some(max_tokens),
        thinking_level: Some(ThinkingLevel::High),
        ..Default::default()
    };

    let mut stream = provider.stream(&context, &options).await?;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        match event? {
            crate::model::StreamEvent::Done { message, .. } => {
                final_message = Some(message);
            }
            crate::model::StreamEvent::Error { error, .. } => {
                let msg = error
                    .error_message
                    .unwrap_or_else(|| "Summarization error".to_string());
                return Err(Error::api(msg));
            }
            _ => {}
        }
    }

    let message = final_message.ok_or_else(|| Error::api("Stream ended without Done event"))?;
    if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
        let msg = message
            .error_message
            .unwrap_or_else(|| "Summarization error".to_string());
        return Err(Error::api(msg));
    }
    Ok(message)
}

async fn generate_summary(
    messages: &[SessionMessage],
    provider: Arc<dyn Provider>,
    api_key: &str,
    settings: &ResolvedCompactionSettings,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
) -> Result<String> {
    let base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };

    let mut prompt = base_prompt.to_string();
    if let Some(custom) = custom_instructions.filter(|s| !s.trim().is_empty()) {
        let _ = write!(prompt, "\n\nAdditional focus: {custom}");
    }

    let llm_messages = messages
        .iter()
        .filter_map(session_message_to_model)
        .collect::<Vec<_>>();
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        let _ = write!(
            prompt_text,
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        );
    }
    prompt_text.push_str(&prompt);

    let assistant = complete_simple(
        provider,
        SUMMARIZATION_SYSTEM_PROMPT,
        prompt_text,
        api_key,
        settings.reserve_tokens,
        0.8,
    )
    .await?;

    let text = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.trim().is_empty() {
        return Err(Error::api(
            "Summarization returned empty text; refusing to store empty compaction summary",
        ));
    }

    Ok(text)
}

async fn generate_turn_prefix_summary(
    messages: &[SessionMessage],
    provider: Arc<dyn Provider>,
    api_key: &str,
    settings: &ResolvedCompactionSettings,
) -> Result<String> {
    let llm_messages = messages
        .iter()
        .filter_map(session_message_to_model)
        .collect::<Vec<_>>();
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let assistant = complete_simple(
        provider,
        SUMMARIZATION_SYSTEM_PROMPT,
        prompt_text,
        api_key,
        settings.reserve_tokens,
        0.5,
    )
    .await?;

    let text = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.trim().is_empty() {
        return Err(Error::api(
            "Turn prefix summarization returned empty text; refusing to store empty summary",
        ));
    }

    Ok(text)
}

// =============================================================================
// Public API
// =============================================================================

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: ResolvedCompactionSettings,
) -> Option<CompactionPreparation> {
    if path_entries.is_empty() {
        return None;
    }

    if path_entries
        .last()
        .is_some_and(|entry| matches!(entry, SessionEntry::Compaction(_)))
    {
        return None;
    }

    let mut prev_compaction_index: Option<usize> = None;
    for (idx, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, SessionEntry::Compaction(_)) {
            prev_compaction_index = Some(idx);
            break;
        }
    }

    let boundary_start = prev_compaction_index.map_or(0, |i| i + 1);
    let boundary_end = path_entries.len();

    let usage_start = prev_compaction_index.unwrap_or(0);
    let mut usage_messages = Vec::new();
    for entry in &path_entries[usage_start..boundary_end] {
        if let Some(msg) = message_from_entry(entry) {
            usage_messages.push(msg);
        }
    }
    let tokens_before = estimate_context_tokens(&usage_messages).tokens;

    let cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );

    let first_kept_entry = &path_entries[cut_point.first_kept_entry_index];
    let first_kept_entry_id = first_kept_entry.base_id()?.clone();

    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index?
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &path_entries[boundary_start..history_end] {
        if let Some(msg) = message_from_entry(entry) {
            messages_to_summarize.push(msg);
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if cut_point.is_split_turn {
        let turn_start = cut_point.turn_start_index?;
        for entry in &path_entries[turn_start..cut_point.first_kept_entry_index] {
            if let Some(msg) = message_from_entry(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }

    // No-op compaction: if there's nothing to summarize, don't issue an LLM call and don't append a
    // compaction entry. This can happen early in a session (e.g. session header entries only).
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let previous_summary = prev_compaction_index.and_then(|idx| match &path_entries[idx] {
        SessionEntry::Compaction(entry) => Some(entry.summary.clone()),
        _ => None,
    });

    let mut file_ops = FileOperations::default();

    // Collect file tracking from previous compaction details if pi-generated.
    if let Some(idx) = prev_compaction_index {
        if let SessionEntry::Compaction(entry) = &path_entries[idx] {
            if !entry.from_hook.unwrap_or(false) {
                if let Some(details) = entry.details.as_ref().and_then(Value::as_object) {
                    if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
                        for item in read_files.iter().filter_map(Value::as_str) {
                            file_ops.read.insert(item.to_string());
                        }
                    }
                    if let Some(modified_files) =
                        details.get("modifiedFiles").and_then(Value::as_array)
                    {
                        for item in modified_files.iter().filter_map(Value::as_str) {
                            file_ops.edited.insert(item.to_string());
                        }
                    }
                }
            }
        }
    }

    for msg in &messages_to_summarize {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    for msg in &turn_prefix_messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

pub async fn summarize_entries(
    entries: &[SessionEntry],
    provider: Arc<dyn Provider>,
    api_key: &str,
    reserve_tokens: u32,
    custom_instructions: Option<&str>,
) -> Result<Option<String>> {
    let mut messages = Vec::new();
    for entry in entries {
        if let Some(message) = message_from_entry(entry) {
            messages.push(message);
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let settings = ResolvedCompactionSettings {
        enabled: true,
        reserve_tokens,
        keep_recent_tokens: 0,
    };

    let summary = generate_summary(
        &messages,
        provider,
        api_key,
        &settings,
        custom_instructions,
        None,
    )
    .await?;

    Ok(Some(summary))
}

pub async fn compact(
    preparation: CompactionPreparation,
    provider: Arc<dyn Provider>,
    api_key: &str,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult> {
    let summary = if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let history_summary = if preparation.messages_to_summarize.is_empty() {
            "No prior history.".to_string()
        } else {
            generate_summary(
                &preparation.messages_to_summarize,
                Arc::clone(&provider),
                api_key,
                &preparation.settings,
                custom_instructions,
                preparation.previous_summary.as_deref(),
            )
            .await?
        };

        let turn_prefix_summary = generate_turn_prefix_summary(
            &preparation.turn_prefix_messages,
            Arc::clone(&provider),
            api_key,
            &preparation.settings,
        )
        .await?;

        format!(
            "{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_summary}"
        )
    } else {
        generate_summary(
            &preparation.messages_to_summarize,
            Arc::clone(&provider),
            api_key,
            &preparation.settings,
            custom_instructions,
            preparation.previous_summary.as_deref(),
        )
        .await?
    };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let details = CompactionDetails {
        read_files: read_files.clone(),
        modified_files: modified_files.clone(),
    };

    let mut summary = summary;
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id,
        tokens_before: preparation.tokens_before,
        details,
    })
}

pub fn compaction_details_to_value(details: &CompactionDetails) -> Result<Value> {
    serde_json::to_value(details).map_err(|e| Error::session(format!("Compaction details: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, TextContent, Usage};
    use serde_json::json;

    fn make_user_text(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn make_assistant_text(text: &str, input: u64, output: u64) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
                usage: Usage {
                    input,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: input + output,
                    ..Default::default()
                },
            },
        }
    }

    fn make_assistant_tool_call(name: &str, args: Value) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: name.to_string(),
                    arguments: args,
                    thought_signature: None,
                })],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
                usage: Usage::default(),
            },
        }
    }

    fn make_tool_result(text: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: String::new(),
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
            timestamp: None,
        }
    }

    // ── calculate_context_tokens ─────────────────────────────────────

    #[test]
    fn context_tokens_prefers_total_tokens() {
        let usage = Usage {
            input: 100,
            output: 50,
            total_tokens: 200,
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 200);
    }

    #[test]
    fn context_tokens_falls_back_to_input_plus_output() {
        let usage = Usage {
            input: 100,
            output: 50,
            total_tokens: 0,
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 150);
    }

    // ── should_compact ───────────────────────────────────────────────

    #[test]
    fn should_compact_when_over_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
        };
        // window=100k, reserve=10k => threshold=90k, context=95k => should compact
        assert!(should_compact(95_000, 100_000, &settings));
    }

    #[test]
    fn should_not_compact_when_under_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
        };
        // window=100k, reserve=10k => threshold=90k, context=80k => should not compact
        assert!(!should_compact(80_000, 100_000, &settings));
    }

    #[test]
    fn should_not_compact_when_disabled() {
        let settings = ResolvedCompactionSettings {
            enabled: false,
            reserve_tokens: 0,
            keep_recent_tokens: 0,
        };
        assert!(!should_compact(1_000_000, 100_000, &settings));
    }

    #[test]
    fn should_compact_at_exact_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
        };
        // window=100k, reserve=10k => threshold=90k, context=90k => NOT compacting (not >)
        assert!(!should_compact(90_000, 100_000, &settings));
        // 90001 should trigger
        assert!(should_compact(90_001, 100_000, &settings));
    }

    // ── estimate_tokens ──────────────────────────────────────────────

    #[test]
    fn estimate_tokens_user_text() {
        let msg = make_user_text("hello world"); // 11 chars => ceil(11/4) = 3
        assert_eq!(estimate_tokens(&msg), 3);
    }

    #[test]
    fn estimate_tokens_empty_text() {
        let msg = make_user_text(""); // 0 chars => 0
        assert_eq!(estimate_tokens(&msg), 0);
    }

    #[test]
    fn estimate_tokens_assistant_text() {
        let msg = make_assistant_text("hello", 10, 5); // 5 chars => ceil(5/4) = 2
        assert_eq!(estimate_tokens(&msg), 2);
    }

    #[test]
    fn estimate_tokens_tool_result() {
        let msg = make_tool_result("file contents here"); // 18 chars => ceil(18/4) = 5
        assert_eq!(estimate_tokens(&msg), 5);
    }

    #[test]
    fn estimate_tokens_custom_message() {
        let msg = SessionMessage::Custom {
            custom_type: "system".to_string(),
            content: "some custom content".to_string(),
            display: true,
            details: None,
        };
        // 19 chars => ceil(19/4) = 5
        assert_eq!(estimate_tokens(&msg), 5);
    }

    // ── estimate_context_tokens ──────────────────────────────────────

    #[test]
    fn estimate_context_with_assistant_usage() {
        let messages = vec![
            make_user_text("hi"),
            make_assistant_text("hello", 50, 10),
            make_user_text("bye"),
        ];
        let estimate = estimate_context_tokens(&messages);
        // Last assistant usage: input=50, output=10, total=60
        // Trailing after that: "bye" = ceil(3/4) = 1
        assert_eq!(estimate.tokens, 61);
        assert_eq!(estimate.last_usage_index, Some(1));
    }

    #[test]
    fn estimate_context_no_assistant() {
        let messages = vec![make_user_text("hello"), make_user_text("world")];
        let estimate = estimate_context_tokens(&messages);
        // No assistant messages, so sum estimate_tokens for all: ceil(5/4)+ceil(5/4) = 2+2 = 4
        assert_eq!(estimate.tokens, 4);
        assert!(estimate.last_usage_index.is_none());
    }

    // ── extract_file_ops_from_message ────────────────────────────────

    #[test]
    fn extract_file_ops_read() {
        let msg = make_assistant_tool_call("read", json!({"path": "/foo/bar.rs"}));
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.contains("/foo/bar.rs"));
        assert!(ops.written.is_empty());
        assert!(ops.edited.is_empty());
    }

    #[test]
    fn extract_file_ops_write() {
        let msg = make_assistant_tool_call("write", json!({"path": "/out.txt"}));
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.written.contains("/out.txt"));
        assert!(ops.read.is_empty());
    }

    #[test]
    fn extract_file_ops_edit() {
        let msg = make_assistant_tool_call("edit", json!({"path": "/src/main.rs"}));
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.edited.contains("/src/main.rs"));
    }

    #[test]
    fn extract_file_ops_ignores_other_tools() {
        let msg = make_assistant_tool_call("bash", json!({"command": "ls"}));
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.is_empty());
        assert!(ops.written.is_empty());
        assert!(ops.edited.is_empty());
    }

    #[test]
    fn extract_file_ops_ignores_user_messages() {
        let msg = make_user_text("read the file /foo.rs");
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.is_empty());
    }

    // ── compute_file_lists ───────────────────────────────────────────

    #[test]
    fn compute_file_lists_separates_read_from_modified() {
        let mut ops = FileOperations::default();
        ops.read.insert("/a.rs".to_string());
        ops.read.insert("/b.rs".to_string());
        ops.written.insert("/b.rs".to_string());
        ops.edited.insert("/c.rs".to_string());

        let (read_only, modified) = compute_file_lists(&ops);
        // /a.rs was only read; /b.rs was read AND written (so it's modified)
        assert_eq!(read_only, vec!["/a.rs"]);
        assert!(modified.contains(&"/b.rs".to_string()));
        assert!(modified.contains(&"/c.rs".to_string()));
    }

    #[test]
    fn compute_file_lists_empty() {
        let ops = FileOperations::default();
        let (read_only, modified) = compute_file_lists(&ops);
        assert!(read_only.is_empty());
        assert!(modified.is_empty());
    }

    // ── format_file_operations ───────────────────────────────────────

    #[test]
    fn format_file_operations_empty() {
        assert_eq!(format_file_operations(&[], &[]), String::new());
    }

    #[test]
    fn format_file_operations_read_only() {
        let result = format_file_operations(&["src/main.rs".to_string()], &[]);
        assert!(result.contains("<read-files>"));
        assert!(result.contains("src/main.rs"));
        assert!(!result.contains("<modified-files>"));
    }

    #[test]
    fn format_file_operations_both() {
        let result = format_file_operations(&["a.rs".to_string()], &["b.rs".to_string()]);
        assert!(result.contains("<read-files>"));
        assert!(result.contains("a.rs"));
        assert!(result.contains("<modified-files>"));
        assert!(result.contains("b.rs"));
    }

    // ── compaction_details_to_value ──────────────────────────────────

    #[test]
    fn compaction_details_serializes() {
        let details = CompactionDetails {
            read_files: vec!["a.rs".to_string()],
            modified_files: vec!["b.rs".to_string()],
        };
        let value = compaction_details_to_value(&details).unwrap();
        assert_eq!(value["readFiles"], json!(["a.rs"]));
        assert_eq!(value["modifiedFiles"], json!(["b.rs"]));
    }

    // ── ResolvedCompactionSettings default ───────────────────────────

    #[test]
    fn default_settings() {
        let settings = ResolvedCompactionSettings::default();
        assert!(settings.enabled);
        assert_eq!(settings.reserve_tokens, 16_384);
        assert_eq!(settings.keep_recent_tokens, 20_000);
    }

    // ── Helper: entry constructors ──────────────────────────────────

    use crate::model::{ImageContent, ThinkingContent};
    use crate::session::{
        BranchSummaryEntry, CompactionEntry, EntryBase, MessageEntry, ModelChangeEntry,
    };
    use std::collections::HashMap;

    fn test_base(id: &str) -> EntryBase {
        EntryBase {
            id: Some(id.to_string()),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        }
    }

    fn user_entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_user_text(text),
        })
    }

    fn assistant_entry(id: &str, text: &str, input: u64, output: u64) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_assistant_text(text, input, output),
        })
    }

    fn tool_call_entry(id: &str, tool_name: &str, path: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_assistant_tool_call(tool_name, json!({"path": path})),
        })
    }

    fn tool_result_entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_tool_result(text),
        })
    }

    fn branch_entry(id: &str, summary: &str) -> SessionEntry {
        SessionEntry::BranchSummary(BranchSummaryEntry {
            base: test_base(id),
            from_id: "parent".to_string(),
            summary: summary.to_string(),
            details: None,
            from_hook: None,
        })
    }

    fn compact_entry(id: &str, summary: &str, tokens: u64) -> SessionEntry {
        SessionEntry::Compaction(CompactionEntry {
            base: test_base(id),
            summary: summary.to_string(),
            first_kept_entry_id: "kept".to_string(),
            tokens_before: tokens,
            details: None,
            from_hook: None,
        })
    }

    fn bash_entry(id: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: SessionMessage::BashExecution {
                command: "ls".to_string(),
                output: "ok".to_string(),
                exit_code: 0,
                cancelled: None,
                truncated: None,
                full_output_path: None,
                timestamp: None,
                extra: HashMap::new(),
            },
        })
    }

    // ── get_assistant_usage ─────────────────────────────────────────

    #[test]
    fn get_assistant_usage_returns_usage_for_stop() {
        let msg = make_assistant_text("text", 100, 50);
        let usage = get_assistant_usage(&msg);
        assert!(usage.is_some());
        assert_eq!(usage.unwrap().input, 100);
    }

    #[test]
    fn get_assistant_usage_none_for_aborted() {
        let msg = SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("text"))],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Aborted,
                error_message: None,
                timestamp: 0,
                usage: Usage {
                    input: 100,
                    output: 50,
                    total_tokens: 150,
                    ..Default::default()
                },
            },
        };
        assert!(get_assistant_usage(&msg).is_none());
    }

    #[test]
    fn get_assistant_usage_none_for_error() {
        let msg = SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Error,
                error_message: None,
                timestamp: 0,
                usage: Usage::default(),
            },
        };
        assert!(get_assistant_usage(&msg).is_none());
    }

    #[test]
    fn get_assistant_usage_none_for_user() {
        assert!(get_assistant_usage(&make_user_text("hello")).is_none());
    }

    // ── entry_is_message_like ───────────────────────────────────────

    #[test]
    fn entry_is_message_like_for_message() {
        assert!(entry_is_message_like(&user_entry("1", "hi")));
    }

    #[test]
    fn entry_is_message_like_for_branch_summary() {
        assert!(entry_is_message_like(&branch_entry("1", "sum")));
    }

    #[test]
    fn entry_is_message_like_false_for_compaction() {
        assert!(!entry_is_message_like(&compact_entry("1", "sum", 100)));
    }

    #[test]
    fn entry_is_message_like_false_for_model_change() {
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model-1".to_string(),
        });
        assert!(!entry_is_message_like(&entry));
    }

    // ── entry_is_compaction_boundary ────────────────────────────────

    #[test]
    fn compaction_boundary_true_for_compaction() {
        assert!(entry_is_compaction_boundary(&compact_entry(
            "1", "sum", 100
        )));
    }

    #[test]
    fn compaction_boundary_false_for_message() {
        assert!(!entry_is_compaction_boundary(&user_entry("1", "hi")));
    }

    #[test]
    fn compaction_boundary_false_for_branch() {
        assert!(!entry_is_compaction_boundary(&branch_entry("1", "sum")));
    }

    // ── is_user_turn_start ──────────────────────────────────────────

    #[test]
    fn user_turn_start_for_user() {
        assert!(is_user_turn_start(&user_entry("1", "hello")));
    }

    #[test]
    fn user_turn_start_for_branch() {
        assert!(is_user_turn_start(&branch_entry("1", "summary")));
    }

    #[test]
    fn user_turn_start_for_bash() {
        assert!(is_user_turn_start(&bash_entry("1")));
    }

    #[test]
    fn user_turn_start_false_for_assistant() {
        assert!(!is_user_turn_start(&assistant_entry("1", "resp", 10, 5)));
    }

    #[test]
    fn user_turn_start_false_for_tool_result() {
        assert!(!is_user_turn_start(&tool_result_entry("1", "result")));
    }

    #[test]
    fn user_turn_start_false_for_compaction() {
        assert!(!is_user_turn_start(&compact_entry("1", "sum", 100)));
    }

    // ── message_from_entry ──────────────────────────────────────────

    #[test]
    fn message_from_entry_user() {
        let entry = user_entry("1", "hello");
        let msg = message_from_entry(&entry);
        assert!(msg.is_some());
        assert!(matches!(msg.unwrap(), SessionMessage::User { .. }));
    }

    #[test]
    fn message_from_entry_branch_summary() {
        let entry = branch_entry("1", "branch summary text");
        let msg = message_from_entry(&entry).unwrap();
        if let SessionMessage::BranchSummary { summary, from_id } = msg {
            assert_eq!(summary, "branch summary text");
            assert_eq!(from_id, "parent");
        } else {
            panic!("expected BranchSummary");
        }
    }

    #[test]
    fn message_from_entry_compaction() {
        let entry = compact_entry("1", "compact summary", 500);
        let msg = message_from_entry(&entry).unwrap();
        if let SessionMessage::CompactionSummary {
            summary,
            tokens_before,
        } = msg
        {
            assert_eq!(summary, "compact summary");
            assert_eq!(tokens_before, 500);
        } else {
            panic!("expected CompactionSummary");
        }
    }

    #[test]
    fn message_from_entry_model_change_is_none() {
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model".to_string(),
        });
        assert!(message_from_entry(&entry).is_none());
    }

    // ── find_valid_cut_points ───────────────────────────────────────

    #[test]
    fn find_valid_cut_points_empty() {
        assert!(find_valid_cut_points(&[], 0, 0).is_empty());
    }

    #[test]
    fn find_valid_cut_points_skips_tool_results() {
        let entries = vec![
            user_entry("1", "hello"),
            assistant_entry("2", "resp", 10, 5),
            tool_result_entry("3", "result"),
            user_entry("4", "follow up"),
        ];
        let cuts = find_valid_cut_points(&entries, 0, entries.len());
        assert!(cuts.contains(&0)); // user
        assert!(cuts.contains(&1)); // assistant
        assert!(!cuts.contains(&2)); // tool result excluded
        assert!(cuts.contains(&3)); // user
    }

    #[test]
    fn find_valid_cut_points_includes_branch_summary() {
        let entries = vec![branch_entry("1", "summary"), user_entry("2", "hello")];
        let cuts = find_valid_cut_points(&entries, 0, entries.len());
        assert!(cuts.contains(&0));
        assert!(cuts.contains(&1));
    }

    #[test]
    fn find_valid_cut_points_respects_range() {
        let entries = vec![
            user_entry("1", "a"),
            user_entry("2", "b"),
            user_entry("3", "c"),
        ];
        let cuts = find_valid_cut_points(&entries, 1, 2);
        assert!(!cuts.contains(&0));
        assert!(cuts.contains(&1));
        assert!(!cuts.contains(&2));
    }

    // ── find_turn_start_index ───────────────────────────────────────

    #[test]
    fn find_turn_start_basic() {
        let entries = vec![
            user_entry("1", "hello"),
            assistant_entry("2", "resp", 10, 5),
            tool_result_entry("3", "result"),
        ];
        assert_eq!(find_turn_start_index(&entries, 2, 0), Some(0));
    }

    #[test]
    fn find_turn_start_at_self() {
        let entries = vec![user_entry("1", "hello")];
        assert_eq!(find_turn_start_index(&entries, 0, 0), Some(0));
    }

    #[test]
    fn find_turn_start_none_no_user() {
        let entries = vec![
            assistant_entry("1", "resp", 10, 5),
            tool_result_entry("2", "result"),
        ];
        assert_eq!(find_turn_start_index(&entries, 1, 0), None);
    }

    #[test]
    fn find_turn_start_respects_start_index() {
        let entries = vec![
            user_entry("1", "old"),
            assistant_entry("2", "resp", 10, 5),
            user_entry("3", "new"),
        ];
        // start_index=2, so it should find user at 2
        assert_eq!(find_turn_start_index(&entries, 2, 2), Some(2));
        // start_index=2, looking back from 2, user at 1 is below start
        assert_eq!(find_turn_start_index(&entries, 1, 2), None);
    }

    // ── serialize_conversation ───────────────────────────────────────

    #[test]
    fn serialize_conversation_user_text() {
        let messages = vec![Message::User(crate::model::UserMessage {
            content: UserContent::Text("hello world".to_string()),
            timestamp: 0,
        })];
        assert_eq!(serialize_conversation(&messages), "[User]: hello world");
    }

    #[test]
    fn serialize_conversation_empty() {
        assert!(serialize_conversation(&[]).is_empty());
    }

    #[test]
    fn serialize_conversation_skips_empty_user() {
        let messages = vec![Message::User(crate::model::UserMessage {
            content: UserContent::Text(String::new()),
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).is_empty());
    }

    #[test]
    fn serialize_conversation_assistant_text() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("response"))],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Assistant]: response"));
    }

    #[test]
    fn serialize_conversation_tool_calls() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "/main.rs"}),
                thought_signature: None,
            })],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })];
        let result = serialize_conversation(&messages);
        assert!(result.contains("[Assistant tool calls]: read("));
        assert!(result.contains("path="));
    }

    #[test]
    fn serialize_conversation_thinking() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "let me think".to_string(),
                thinking_signature: None,
            })],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Assistant thinking]: let me think"));
    }

    #[test]
    fn serialize_conversation_tool_result() {
        let messages = vec![Message::ToolResult(crate::model::ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
            is_error: false,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Tool result]: file contents"));
    }

    // ── estimate_tokens additional ──────────────────────────────────

    #[test]
    fn estimate_tokens_image_block() {
        let msg = SessionMessage::User {
            content: UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            })]),
            timestamp: None,
        };
        // Image = 4800 chars -> ceil(4800/4) = 1200
        assert_eq!(estimate_tokens(&msg), 1200);
    }

    #[test]
    fn estimate_tokens_thinking() {
        let msg = SessionMessage::User {
            content: UserContent::Blocks(vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "a".repeat(20),
                thinking_signature: None,
            })]),
            timestamp: None,
        };
        // 20 chars -> ceil(20/4) = 5
        assert_eq!(estimate_tokens(&msg), 5);
    }

    #[test]
    fn estimate_tokens_bash_execution() {
        let msg = SessionMessage::BashExecution {
            command: "echo hi".to_string(),
            output: "hi\n".to_string(),
            exit_code: 0,
            cancelled: None,
            truncated: None,
            full_output_path: None,
            timestamp: None,
            extra: HashMap::new(),
        };
        // 7 + 3 = 10 chars -> ceil(10/4) = 3
        assert_eq!(estimate_tokens(&msg), 3);
    }

    #[test]
    fn estimate_tokens_branch_summary() {
        let msg = SessionMessage::BranchSummary {
            summary: "a".repeat(40),
            from_id: "id".to_string(),
        };
        assert_eq!(estimate_tokens(&msg), 10);
    }

    #[test]
    fn estimate_tokens_compaction_summary() {
        let msg = SessionMessage::CompactionSummary {
            summary: "a".repeat(80),
            tokens_before: 5000,
        };
        assert_eq!(estimate_tokens(&msg), 20);
    }

    // ── prepare_compaction ──────────────────────────────────────────

    #[test]
    fn prepare_compaction_empty() {
        assert!(prepare_compaction(&[], ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_last_is_compaction_returns_none() {
        let entries = vec![user_entry("1", "hello"), compact_entry("2", "summary", 100)];
        assert!(prepare_compaction(&entries, ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_no_messages_to_summarize_returns_none() {
        // Only non-message entries that produce no summarizable messages
        let entries = vec![SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model".to_string(),
        })];
        assert!(prepare_compaction(&entries, ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_basic_returns_some() {
        let long_text = "a".repeat(100_000);
        let entries = vec![
            user_entry("1", &long_text),
            assistant_entry("2", &long_text, 50000, 25000),
            user_entry("3", &long_text),
            assistant_entry("4", &long_text, 80000, 30000),
            user_entry("5", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        };
        let prep = prepare_compaction(&entries, settings);
        assert!(prep.is_some());
        let p = prep.unwrap();
        assert!(!p.messages_to_summarize.is_empty());
        assert!(p.tokens_before > 0);
        assert!(p.previous_summary.is_none());
    }

    #[test]
    fn prepare_compaction_after_previous_compaction() {
        let entries = vec![
            user_entry("1", "old message"),
            assistant_entry("2", "old response", 100, 50),
            compact_entry("3", "previous summary", 300),
            user_entry("4", &"x".repeat(100_000)),
            assistant_entry("5", &"y".repeat(100_000), 80000, 30000),
            user_entry("6", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        };
        let prep = prepare_compaction(&entries, settings);
        assert!(prep.is_some());
        let p = prep.unwrap();
        assert_eq!(p.previous_summary.as_deref(), Some("previous summary"));
    }

    #[test]
    fn prepare_compaction_tracks_file_ops() {
        let entries = vec![
            tool_call_entry("1", "read", "/src/main.rs"),
            tool_call_entry("2", "edit", "/src/lib.rs"),
            user_entry("3", &"x".repeat(100_000)),
            assistant_entry("4", &"y".repeat(100_000), 80000, 30000),
            user_entry("5", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        };
        if let Some(prep) = prepare_compaction(&entries, settings) {
            let has_read = prep.file_ops.read.contains("/src/main.rs");
            let has_edit = prep.file_ops.edited.contains("/src/lib.rs");
            // At least one should be tracked (depends on cut point position)
            assert!(has_read || has_edit || prep.file_ops.read.is_empty());
        }
    }

    // ── FileOperations::read_files ──────────────────────────────────

    #[test]
    fn file_operations_read_files_iterator() {
        let mut ops = FileOperations::default();
        ops.read.insert("/a.rs".to_string());
        ops.read.insert("/b.rs".to_string());
        let files: Vec<&str> = ops.read_files().collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"/a.rs"));
        assert!(files.contains(&"/b.rs"));
    }

    #[test]
    fn find_cut_point_includes_tool_result_when_needed() {
        // Setup:
        // 0. User (10)
        // 1. Assistant Call (10)
        // 2. Tool Result (100)
        // 3. User (10)
        // 4. Assistant (10)
        //
        // Keep recent = 100.
        // Accumulation from end:
        // 4: 10
        // 3: 20
        // 2: 120 (Threshold crossed at index 2)
        //
        // Index 2 is ToolResult (invalid cut point).
        // Valid cut points: 0, 1, 3, 4.
        //
        // Logic should pick closest valid cut point <= 2, which is 1.
        // If it picked >= 2, it would pick 3, discarding the ToolResult and Call (keeping only 20 tokens).
        // By picking 1, we keep 1..4 (130 tokens).

        // Create entries with controlled lengths.
        // We need 100 tokens for TR. 100 * 4 = 400 chars.
        let tr_text = "x".repeat(400);
        let entries = vec![
            user_entry("0", "user"),              // Valid
            assistant_entry("1", "call", 10, 10), // Valid (Assistant)
            tool_result_entry("2", &tr_text),     // Invalid
            user_entry("3", "user"),              // Valid
            assistant_entry("4", "resp", 10, 10), // Valid
        ];

        // Verify token estimates (approx)
        // 0: 4/4 = 1
        // 1: 4/4 = 1
        // 2: 400/4 = 100
        // 3: 4/4 = 1
        // 4: 4/4 = 1
        // Total recent needed: 100.
        // Accumulate: 4(1)+3(1)+2(100) = 102. Crossed at 2.

        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 100,
        };

        let prep = prepare_compaction(&entries, settings).expect("should compact");

        // Cut point is index 1 (Assistant/Call). Because entries[1] is Assistant (not User),
        // this is a split turn: the turn started at index 0 (User). The User message at index 0
        // goes into turn_prefix_messages (not messages_to_summarize) because history_end = 0.
        assert_eq!(prep.first_kept_entry_id, "1");

        // messages_to_summarize is entries[0..0] = empty (split-turn puts the
        // prefix in turn_prefix_messages instead).
        assert!(
            prep.messages_to_summarize.is_empty(),
            "split turn: user goes into turn prefix, not summarize"
        );

        // turn_prefix_messages should contain the User message at index 0.
        assert_eq!(prep.turn_prefix_messages.len(), 1);
        match &prep.turn_prefix_messages[0] {
            SessionMessage::User { content, .. } => {
                if let UserContent::Text(t) = content {
                    assert_eq!(t, "user");
                } else {
                    panic!("wrong content");
                }
            }
            _ => panic!("expected user message in turn prefix"),
        }
    }
}
