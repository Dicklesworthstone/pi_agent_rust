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
        usage.input + usage.output + usage.cache_read + usage.cache_write
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
            for &cut_point in &cut_points {
                if cut_point >= i {
                    cut_index = cut_point;
                    break;
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

    Ok(assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

// =============================================================================
// Public API
// =============================================================================

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: ResolvedCompactionSettings,
) -> Option<CompactionPreparation> {
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
