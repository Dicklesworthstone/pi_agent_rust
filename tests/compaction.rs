#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]

mod common;

use common::{TestHarness, run_async};
use pi::compaction::{CompactionPreparation, CompactionResult, compact, prepare_compaction};
use pi::model::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, TextContent,
    ThinkingContent, ToolCall, Usage, UserContent, UserMessage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::session::{
    BranchSummaryEntry, CompactionEntry, EntryBase, MessageEntry, ModelChangeEntry, Session,
    SessionEntry, SessionMessage,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

const TS: &str = "2026-02-03T00:00:00.000Z";

struct ScriptedProvider {
    responses: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Into::into).collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("lock prompts").clone()
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "scripted"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn api(&self) -> &str {
        "scripted"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_id(&self) -> &str {
        "scripted-model"
    }

    async fn stream(
        &self,
        context: &Context,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>>,
    > {
        let prompt_text = extract_prompt_text(context);
        self.prompts.lock().expect("lock prompts").push(prompt_text);

        let response = self
            .responses
            .lock()
            .expect("lock responses")
            .pop_front()
            .unwrap_or_else(|| "<missing scripted provider response>".to_string());

        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(response))],
            api: "scripted".to_string(),
            provider: "scripted".to_string(),
            model: "scripted-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };

        Ok(Box::pin(futures::stream::iter(vec![Ok(
            pi::model::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        )])))
    }
}

fn extract_prompt_text(context: &Context) -> String {
    let Some(Message::User(UserMessage { content, .. })) = context.messages.first() else {
        return "<missing user prompt>".to_string();
    };
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn base(id: Option<&str>, parent_id: Option<&str>) -> EntryBase {
    EntryBase {
        id: id.map(str::to_string),
        parent_id: parent_id.map(str::to_string),
        timestamp: TS.to_string(),
    }
}

fn message_entry(id: &str, parent_id: Option<&str>, message: SessionMessage) -> SessionEntry {
    SessionEntry::Message(MessageEntry {
        base: base(Some(id), parent_id),
        message,
    })
}

fn model_change_entry(
    id: &str,
    parent_id: Option<&str>,
    provider: &str,
    model_id: &str,
) -> SessionEntry {
    SessionEntry::ModelChange(ModelChangeEntry {
        base: base(Some(id), parent_id),
        provider: provider.to_string(),
        model_id: model_id.to_string(),
    })
}

fn branch_summary_entry(
    id: &str,
    parent_id: Option<&str>,
    from_id: &str,
    summary: &str,
) -> SessionEntry {
    SessionEntry::BranchSummary(BranchSummaryEntry {
        base: base(Some(id), parent_id),
        from_id: from_id.to_string(),
        summary: summary.to_string(),
        details: None,
        from_hook: None,
    })
}

fn compaction_entry(
    id: &str,
    parent_id: Option<&str>,
    summary: &str,
    first_kept_entry_id: &str,
    tokens_before: u64,
    details: Option<Value>,
    from_hook: Option<bool>,
) -> SessionEntry {
    SessionEntry::Compaction(CompactionEntry {
        base: base(Some(id), parent_id),
        summary: summary.to_string(),
        first_kept_entry_id: first_kept_entry_id.to_string(),
        tokens_before,
        details,
        from_hook,
    })
}

fn user_text(text: impl Into<String>) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.into()),
        timestamp: Some(0),
    }
}

fn assistant_message(
    blocks: Vec<ContentBlock>,
    usage: Usage,
    stop_reason: StopReason,
) -> SessionMessage {
    SessionMessage::Assistant {
        message: AssistantMessage {
            content: blocks,
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            usage,
            stop_reason,
            error_message: None,
            timestamp: 0,
        },
    }
}

fn assistant_text(text: impl Into<String>, usage_total_tokens: u64) -> SessionMessage {
    assistant_message(
        vec![ContentBlock::Text(TextContent::new(text))],
        Usage {
            total_tokens: usage_total_tokens,
            ..Usage::default()
        },
        StopReason::Stop,
    )
}

fn assistant_with_tool_calls(calls: Vec<(&str, Value)>) -> SessionMessage {
    let blocks = calls
        .into_iter()
        .enumerate()
        .map(|(idx, (name, args))| {
            ContentBlock::ToolCall(ToolCall {
                id: format!("call-{idx}"),
                name: name.to_string(),
                arguments: args,
                thought_signature: None,
            })
        })
        .collect::<Vec<_>>();
    assistant_message(blocks, Usage::default(), StopReason::Stop)
}

fn assistant_with_thinking_and_text(thinking: &str, text: &str) -> SessionMessage {
    assistant_message(
        vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: thinking.to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new(text)),
        ],
        Usage::default(),
        StopReason::Stop,
    )
}

fn user_with_image_and_text(image_tokens: bool, text: &str) -> SessionMessage {
    let mut blocks = Vec::new();
    if image_tokens {
        blocks.push(ContentBlock::Image(ImageContent {
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
        }));
    }
    blocks.push(ContentBlock::Text(TextContent::new(text)));
    SessionMessage::User {
        content: UserContent::Blocks(blocks),
        timestamp: Some(0),
    }
}

fn tool_result(tool_call_id: &str, tool_name: &str, content: &str) -> SessionMessage {
    SessionMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![ContentBlock::Text(TextContent::new(content))],
        details: None,
        is_error: false,
        timestamp: Some(0),
    }
}

fn text_of_tokens(tokens: usize) -> String {
    "a".repeat(tokens.saturating_mul(4))
}

fn entry_id(entry: &SessionEntry) -> Option<&str> {
    entry.base_id().map(String::as_str)
}

fn find_entry_index(entries: &[SessionEntry], id: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| entry.base_id().is_some_and(|entry_id| entry_id == id))
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

fn find_turn_start_index(entries: &[SessionEntry], entry_index: usize) -> Option<usize> {
    (0..=entry_index)
        .rev()
        .find(|&idx| is_user_turn_start(&entries[idx]))
}

fn describe_entry(entry: &SessionEntry) -> String {
    match entry {
        SessionEntry::Message(msg_entry) => match &msg_entry.message {
            SessionMessage::User { content, .. } => {
                format!("Message(User): {}", user_content_preview(content))
            }
            SessionMessage::Assistant { message } => {
                let kinds = message
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text(_) => "text",
                        ContentBlock::Thinking(_) => "thinking",
                        ContentBlock::Image(_) => "image",
                        ContentBlock::ToolCall(_) => "tool_call",
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("Message(Assistant): [{kinds}]")
            }
            SessionMessage::ToolResult { tool_name, .. } => {
                format!("Message(ToolResult): {tool_name}")
            }
            SessionMessage::Custom { custom_type, .. } => format!("Message(Custom): {custom_type}"),
            SessionMessage::BashExecution { command, .. } => {
                format!("Message(BashExecution): {command}")
            }
            SessionMessage::BranchSummary { summary, .. } => {
                format!("Message(BranchSummary): {}", preview(summary))
            }
            SessionMessage::CompactionSummary { summary, .. } => {
                format!("Message(CompactionSummary): {}", preview(summary))
            }
        },
        SessionEntry::Compaction(compaction) => format!(
            "Entry(Compaction): first_kept={}, tokens_before={}",
            compaction.first_kept_entry_id, compaction.tokens_before
        ),
        SessionEntry::BranchSummary(summary) => {
            format!("Entry(BranchSummary): {}", preview(&summary.summary))
        }
        SessionEntry::ModelChange(change) => format!(
            "Entry(ModelChange): {} {}",
            change.provider, change.model_id
        ),
        SessionEntry::ThinkingLevelChange(change) => {
            format!("Entry(ThinkingLevelChange): {}", change.thinking_level)
        }
        SessionEntry::Label(label) => format!("Entry(Label): {}", label.target_id),
        SessionEntry::SessionInfo(_) => "Entry(SessionInfo)".to_string(),
        SessionEntry::Custom(custom) => format!("Entry(Custom): {}", custom.custom_type),
    }
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 48;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    format!("{}…", &text[..LIMIT])
}

fn user_content_preview(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => preview(text),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn dump_timeline(harness: &TestHarness, label: &str, entries: &[SessionEntry]) {
    let mut lines = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        let id = entry_id(entry).unwrap_or("<no-id>");
        lines.push(format!("{idx:>3}: {id} {}", describe_entry(entry)));
    }
    harness
        .log()
        .debug(label, format!("timeline:\n{}", lines.join("\n")));
}

fn log_preparation(harness: &TestHarness, entries: &[SessionEntry], prep: &CompactionPreparation) {
    let first_kept_index = find_entry_index(entries, &prep.first_kept_entry_id)
        .expect("first_kept_entry_id exists in entries");
    let prev_compaction_index = entries
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::Compaction(_)));
    let boundary_start = prev_compaction_index.map_or(0, |idx| idx + 1);
    let turn_start_index = if prep.is_split_turn {
        find_turn_start_index(entries, first_kept_index)
    } else {
        None
    };

    harness
        .log()
        .info_ctx("compaction", "prepare_compaction", |ctx| {
            ctx.push(("entries".into(), entries.len().to_string()));
            ctx.push((
                "keepRecentToks".into(),
                prep.settings.keep_recent_tokens.to_string(),
            ));
            ctx.push((
                "reserveToks".into(),
                prep.settings.reserve_tokens.to_string(),
            ));
            ctx.push(("boundaryStart".into(), boundary_start.to_string()));
            ctx.push(("firstKeptEntryIndex".into(), first_kept_index.to_string()));
            ctx.push((
                "turnStartIndex".into(),
                turn_start_index.map_or_else(|| "none".to_string(), |i| i.to_string()),
            ));
            ctx.push(("isSplitTurn".into(), prep.is_split_turn.to_string()));
            ctx.push((
                "messagesToSummarize".into(),
                prep.messages_to_summarize.len().to_string(),
            ));
            ctx.push((
                "turnPrefixMessages".into(),
                prep.turn_prefix_messages.len().to_string(),
            ));
            ctx.push(("toksBefore".into(), prep.tokens_before.to_string()));
            ctx.push((
                "hasPreviousSummary".into(),
                prep.previous_summary.is_some().to_string(),
            ));
        });
}

fn log_result(harness: &TestHarness, result: &CompactionResult) {
    harness.log().info_ctx("compaction", "compact", |ctx| {
        ctx.push(("summaryLen".into(), result.summary.len().to_string()));
        ctx.push((
            "firstKeptEntryId".into(),
            result.first_kept_entry_id.clone(),
        ));
        ctx.push(("toksBefore".into(), result.tokens_before.to_string()));
        ctx.push(("readFiles".into(), result.details.read_files.join(",")));
        ctx.push((
            "modifiedFiles".into(),
            result.details.modified_files.join(","),
        ));
    });
}

const fn make_settings(keep_recent_tokens: u32) -> pi::compaction::ResolvedCompactionSettings {
    pi::compaction::ResolvedCompactionSettings {
        enabled: true,
        reserve_tokens: 1024,
        keep_recent_tokens,
    }
}

#[test]
fn prepare_compaction_returns_none_when_last_entry_is_compaction() {
    let harness = TestHarness::new("prepare_compaction_returns_none_when_last_entry_is_compaction");

    let entries = vec![
        message_entry("u1", None, user_text("hello")),
        compaction_entry("c1", Some("u1"), "summary", "u1", 10, None, None),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(0));
    assert!(prep.is_none());
}

#[test]
fn prepare_compaction_returns_none_when_first_kept_entry_missing_id() {
    let harness =
        TestHarness::new("prepare_compaction_returns_none_when_first_kept_entry_missing_id");

    let entries = vec![SessionEntry::Message(MessageEntry {
        base: base(None, None),
        message: user_text("hello"),
    })];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(1));
    assert!(prep.is_none());
}

#[test]
fn prepare_compaction_selects_cutpoint_by_keep_recent_tokens() {
    let harness = TestHarness::new("prepare_compaction_selects_cutpoint_by_keep_recent_tokens");

    let entries = vec![
        message_entry("m0", None, user_text(text_of_tokens(1))),
        message_entry("m1", Some("m0"), user_text(text_of_tokens(1))),
        message_entry("m2", Some("m1"), user_text(text_of_tokens(1))),
        message_entry("m3", Some("m2"), user_text(text_of_tokens(1))),
        message_entry("m4", Some("m3"), user_text(text_of_tokens(1))),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(2)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "m3");
    assert_eq!(prep.messages_to_summarize.len(), 3);
    assert!(prep.turn_prefix_messages.is_empty());
    assert!(!prep.is_split_turn);
}

#[test]
fn prepare_compaction_keep_recent_tokens_zero_keeps_only_last_cut_point() {
    let harness =
        TestHarness::new("prepare_compaction_keep_recent_tokens_zero_keeps_only_last_cut_point");

    let entries = vec![
        message_entry("m0", None, user_text(text_of_tokens(1))),
        message_entry("m1", Some("m0"), user_text(text_of_tokens(1))),
        message_entry("m2", Some("m1"), user_text(text_of_tokens(1))),
        message_entry("m3", Some("m2"), user_text(text_of_tokens(1))),
        message_entry("m4", Some("m3"), user_text(text_of_tokens(1))),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(0)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "m4");
    assert_eq!(prep.messages_to_summarize.len(), 4);
}

#[test]
fn prepare_compaction_skips_tool_result_as_cut_point_and_marks_split_turn() {
    let harness =
        TestHarness::new("prepare_compaction_skips_tool_result_as_cut_point_and_marks_split_turn");

    let entries = vec![
        message_entry("u1", None, user_text(text_of_tokens(1))),
        message_entry(
            "a1",
            Some("u1"),
            assistant_with_tool_calls(vec![("read", json!({"path": "a.txt"}))]),
        ),
        message_entry("tr1", Some("a1"), tool_result("call-0", "read", "ok")),
        message_entry("a2", Some("tr1"), assistant_text(text_of_tokens(1), 0)),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(2)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "a2");
    assert!(prep.is_split_turn);
    assert!(prep.messages_to_summarize.is_empty());
    assert_eq!(prep.turn_prefix_messages.len(), 3);
}

#[test]
fn prepare_compaction_includes_non_message_entries_in_kept_region() {
    let harness =
        TestHarness::new("prepare_compaction_includes_non_message_entries_in_kept_region");

    let entries = vec![
        message_entry("u1", None, user_text(text_of_tokens(1))),
        model_change_entry("mc1", Some("u1"), "p", "m"),
        message_entry("a1", Some("mc1"), assistant_text(text_of_tokens(1), 0)),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(0)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "mc1");
    assert!(prep.is_split_turn);
    assert_eq!(prep.turn_prefix_messages.len(), 1);
}

#[test]
fn prepare_compaction_respects_previous_compaction_boundary() {
    let harness = TestHarness::new("prepare_compaction_respects_previous_compaction_boundary");

    let entries = vec![
        message_entry("pre_u", None, user_text("pre")),
        message_entry("pre_a", Some("pre_u"), assistant_text("pre", 0)),
        compaction_entry(
            "c1",
            Some("pre_a"),
            "PREV_SUMMARY",
            "pre_a",
            100,
            None,
            None,
        ),
        message_entry("post_u", Some("c1"), user_text(text_of_tokens(1))),
        message_entry(
            "post_a",
            Some("post_u"),
            assistant_text(text_of_tokens(1), 0),
        ),
        message_entry("post_u2", Some("post_a"), user_text(text_of_tokens(1))),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.previous_summary.as_deref(), Some("PREV_SUMMARY"));
    let summarized_text = prep
        .messages_to_summarize
        .iter()
        .filter_map(|m| match m {
            SessionMessage::User { content, .. } => Some(user_content_preview(content)),
            SessionMessage::Assistant { .. } => Some("<assistant>".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(!summarized_text.contains("pre"));
}

#[test]
fn compact_non_split_turn_calls_provider_once() {
    let harness = TestHarness::new("compact_non_split_turn_calls_provider_once");

    let entries = vec![
        message_entry("m0", None, user_text(text_of_tokens(1))),
        message_entry("m1", Some("m0"), user_text(text_of_tokens(1))),
        message_entry("m2", Some("m1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    let provider = Arc::new(ScriptedProvider::new(["SUMMARY"]));
    let provider_dyn: Arc<dyn Provider> = provider.clone();

    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert!(result.summary.contains("SUMMARY"));
    assert_eq!(provider.prompts().len(), 1);
}

#[test]
fn compact_split_turn_calls_provider_twice_and_formats_sections() {
    let harness = TestHarness::new("compact_split_turn_calls_provider_twice_and_formats_sections");

    let entries = vec![
        message_entry("h_u", None, user_text(text_of_tokens(1))),
        message_entry("h_a", Some("h_u"), assistant_text(text_of_tokens(1), 0)),
        message_entry("u1", Some("h_a"), user_text(text_of_tokens(1))),
        message_entry(
            "a1",
            Some("u1"),
            assistant_with_tool_calls(vec![("read", json!({"path": "a.txt"}))]),
        ),
        message_entry("tr1", Some("a1"), tool_result("call-0", "read", "ok")),
        message_entry("a2", Some("tr1"), assistant_text(text_of_tokens(1), 0)),
    ];

    let prep = prepare_compaction(&entries, make_settings(2)).expect("prep");
    log_preparation(&harness, &entries, &prep);
    assert!(prep.is_split_turn);
    assert!(!prep.messages_to_summarize.is_empty());
    assert!(!prep.turn_prefix_messages.is_empty());

    let provider = Arc::new(ScriptedProvider::new([
        "HISTORY_SUMMARY",
        "TURN_PREFIX_SUMMARY",
    ]));
    let provider_dyn: Arc<dyn Provider> = provider.clone();

    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert!(result.summary.contains("HISTORY_SUMMARY"));
    assert!(result.summary.contains("**Turn Context (split turn):**"));
    assert!(result.summary.contains("TURN_PREFIX_SUMMARY"));
    assert_eq!(provider.prompts().len(), 2);
}

#[test]
fn compact_appends_file_operations_and_sorts_lists() {
    let harness = TestHarness::new("compact_appends_file_operations_and_sorts_lists");

    let entries = vec![
        message_entry("u1", None, user_text(text_of_tokens(1))),
        message_entry(
            "a1",
            Some("u1"),
            assistant_with_tool_calls(vec![
                ("read", json!({"path": "b.txt"})),
                ("write", json!({"path": "c.txt"})),
                ("edit", json!({"path": "a.txt"})),
                ("read", json!({"path": "c.txt"})),
            ]),
        ),
        message_entry("u2", Some("a1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    let provider_dyn: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(["S"]));
    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert!(result.summary.contains("<read-files>"));
    assert!(result.summary.contains("<modified-files>"));
    assert_eq!(result.details.read_files, vec!["b.txt".to_string()]);
    assert_eq!(
        result.details.modified_files,
        vec!["a.txt".to_string(), "c.txt".to_string()]
    );
}

#[test]
fn compact_seeds_file_ops_from_previous_compaction_details() {
    let harness = TestHarness::new("compact_seeds_file_ops_from_previous_compaction_details");

    let prev_details = json!({
        "readFiles": ["r1.txt"],
        "modifiedFiles": ["m1.txt"]
    });

    let entries = vec![
        message_entry("u0", None, user_text("pre")),
        compaction_entry(
            "c0",
            Some("u0"),
            "prev summary",
            "u0",
            10,
            Some(prev_details),
            None,
        ),
        message_entry(
            "a1",
            Some("c0"),
            assistant_with_tool_calls(vec![("read", json!({"path": "r2.txt"}))]),
        ),
        message_entry("u1", Some("a1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    let provider_dyn: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(["S"]));
    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert_eq!(
        result.details.read_files,
        vec!["r1.txt".to_string(), "r2.txt".to_string()]
    );
    assert_eq!(result.details.modified_files, vec!["m1.txt".to_string()]);
}

#[test]
fn compact_does_not_seed_file_ops_when_previous_compaction_from_hook() {
    let harness =
        TestHarness::new("compact_does_not_seed_file_ops_when_previous_compaction_from_hook");

    let prev_details = json!({
        "readFiles": ["r1.txt"],
        "modifiedFiles": ["m1.txt"]
    });

    let entries = vec![
        message_entry("u0", None, user_text("pre")),
        compaction_entry(
            "c0",
            Some("u0"),
            "prev summary",
            "u0",
            10,
            Some(prev_details),
            Some(true),
        ),
        message_entry(
            "a1",
            Some("c0"),
            assistant_with_tool_calls(vec![("read", json!({"path": "r2.txt"}))]),
        ),
        message_entry("u1", Some("a1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    let provider_dyn: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(["S"]));
    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert_eq!(result.details.read_files, vec!["r2.txt".to_string()]);
    assert!(result.details.modified_files.is_empty());
}

#[test]
fn compact_includes_previous_summary_in_prompt_for_incremental_update() {
    let harness =
        TestHarness::new("compact_includes_previous_summary_in_prompt_for_incremental_update");

    let entries = vec![
        message_entry("u0", None, user_text("pre")),
        compaction_entry("c0", Some("u0"), "PREV", "u0", 10, None, None),
        message_entry("u1", Some("c0"), user_text(text_of_tokens(1))),
        message_entry("u2", Some("u1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);
    assert_eq!(prep.previous_summary.as_deref(), Some("PREV"));

    let provider = Arc::new(ScriptedProvider::new(["UPDATED"]));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    let prompt = provider.prompts().first().expect("prompt").clone();
    assert!(prompt.contains("<previous-summary>"));
    assert!(prompt.contains("PREV"));
    assert!(prompt.contains("Update the existing structured summary"));
}

#[test]
fn prepare_compaction_tokens_before_uses_last_assistant_usage_total_tokens() {
    let harness =
        TestHarness::new("prepare_compaction_tokens_before_uses_last_assistant_usage_total_tokens");

    let entries = vec![
        message_entry("a0", None, assistant_text("ignored", 100)),
        message_entry("u1", Some("a0"), user_text(text_of_tokens(2))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.tokens_before, 102);
}

#[test]
fn prepare_compaction_tokens_before_uses_usage_fields_when_total_tokens_zero() {
    let harness = TestHarness::new(
        "prepare_compaction_tokens_before_uses_usage_fields_when_total_tokens_zero",
    );

    let usage = Usage {
        input: 10,
        output: 5,
        ..Usage::default()
    };

    let entries = vec![
        message_entry(
            "a0",
            None,
            assistant_message(
                vec![ContentBlock::Text(TextContent::new("x"))],
                usage,
                StopReason::Stop,
            ),
        ),
        message_entry("u1", Some("a0"), user_text(text_of_tokens(2))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.tokens_before, 17);
}

#[test]
fn prepare_compaction_tokens_before_ignores_aborted_usage_but_counts_trailing_messages() {
    let harness = TestHarness::new(
        "prepare_compaction_tokens_before_ignores_aborted_usage_but_counts_trailing_messages",
    );

    let usage_ok = Usage {
        total_tokens: 100,
        ..Usage::default()
    };
    let usage_bad = Usage {
        total_tokens: 999,
        ..Usage::default()
    };

    let entries = vec![
        message_entry(
            "a_ok",
            None,
            assistant_message(
                vec![ContentBlock::Text(TextContent::new(text_of_tokens(1)))],
                usage_ok,
                StopReason::Stop,
            ),
        ),
        message_entry(
            "a_bad",
            Some("a_ok"),
            assistant_message(
                vec![ContentBlock::Text(TextContent::new(text_of_tokens(1)))],
                usage_bad,
                StopReason::Aborted,
            ),
        ),
    ];

    let prep = prepare_compaction(&entries, make_settings(0)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.tokens_before, 101);
}

#[test]
fn prepare_compaction_image_blocks_affect_window_selection() {
    let harness = TestHarness::new("prepare_compaction_image_blocks_affect_window_selection");

    let entries = vec![
        message_entry("m0", None, user_text(text_of_tokens(1))),
        message_entry("m1", Some("m0"), user_text(text_of_tokens(1))),
        message_entry("img", Some("m1"), user_with_image_and_text(true, "")),
        message_entry("m2", Some("img"), user_text(text_of_tokens(1))),
    ];

    dump_timeline(&harness, "setup", &entries);
    let prep = prepare_compaction(&entries, make_settings(1200)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "img");
}

#[test]
fn compact_prompt_includes_thinking_and_tool_calls_in_serialized_conversation() {
    let harness = TestHarness::new(
        "compact_prompt_includes_thinking_and_tool_calls_in_serialized_conversation",
    );

    let entries = vec![
        message_entry("u0", None, user_text("User asks")),
        message_entry(
            "a0",
            Some("u0"),
            assistant_with_thinking_and_text("reasoning", "answer"),
        ),
        message_entry(
            "a1",
            Some("a0"),
            assistant_with_tool_calls(vec![("read", json!({"path": "x.txt"}))]),
        ),
        message_entry("u1", Some("a1"), user_text(text_of_tokens(1))),
    ];

    let prep = prepare_compaction(&entries, make_settings(1)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    let provider = Arc::new(ScriptedProvider::new(["S"]));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let _result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");

    let prompts = provider.prompts();
    let prompt = prompts.first().expect("prompt");
    assert!(prompt.contains("[Assistant thinking]: reasoning"));
    assert!(prompt.contains("[Assistant]: answer"));
    assert!(prompt.contains("[Assistant tool calls]: read("));
    assert!(prompt.contains("path="));
}

#[test]
fn to_messages_for_current_path_inserts_compaction_summary_before_kept_region() {
    let harness = TestHarness::new(
        "to_messages_for_current_path_inserts_compaction_summary_before_kept_region",
    );

    let mut session = Session::in_memory();
    session.entries = vec![
        message_entry("u0", None, user_text("old")),
        message_entry("a0", Some("u0"), assistant_text("old", 0)),
        message_entry("u1", Some("a0"), user_text("keep")),
        message_entry("a1", Some("u1"), assistant_text("keep", 0)),
        compaction_entry("c1", Some("a1"), "SUM", "u1", 123, None, None),
    ];
    session.leaf_id = Some("c1".to_string());

    dump_timeline(&harness, "setup", &session.entries);
    let messages = session.to_messages_for_current_path();
    assert!(!messages.is_empty());

    let first_text = model_message_text(&messages[0]);
    assert!(first_text.contains("compacted into the following summary"));
    assert!(first_text.contains("<summary>"));
    assert!(first_text.contains("SUM"));

    let remaining = messages
        .iter()
        .skip(1)
        .map(model_message_text)
        .collect::<Vec<_>>()
        .join("|");
    assert!(remaining.contains("keep"));
    assert!(!remaining.contains("old"));
}

#[test]
fn to_messages_for_current_path_with_missing_first_kept_entry_id_keeps_only_summary() {
    let harness = TestHarness::new(
        "to_messages_for_current_path_with_missing_first_kept_entry_id_keeps_only_summary",
    );

    let mut session = Session::in_memory();
    session.entries = vec![
        message_entry("u0", None, user_text("old")),
        message_entry("a0", Some("u0"), assistant_text("old", 0)),
        compaction_entry("c1", Some("a0"), "SUM", "does-not-exist", 123, None, None),
    ];
    session.leaf_id = Some("c1".to_string());

    dump_timeline(&harness, "setup", &session.entries);
    let messages = session.to_messages_for_current_path();
    assert_eq!(messages.len(), 1);
    assert!(model_message_text(&messages[0]).contains("SUM"));
}

#[test]
fn prepare_compaction_turn_prefix_tool_calls_contribute_to_file_ops() {
    let harness =
        TestHarness::new("prepare_compaction_turn_prefix_tool_calls_contribute_to_file_ops");

    let entries = vec![
        message_entry("h_u", None, user_text(text_of_tokens(1))),
        message_entry("h_a", Some("h_u"), assistant_text(text_of_tokens(1), 0)),
        message_entry("u1", Some("h_a"), user_text(text_of_tokens(1))),
        message_entry(
            "a1",
            Some("u1"),
            assistant_with_tool_calls(vec![("edit", json!({"path": "turn.txt"}))]),
        ),
        message_entry("a2", Some("a1"), assistant_text(text_of_tokens(1), 0)),
    ];

    let prep = prepare_compaction(&entries, make_settings(0)).expect("prep");
    log_preparation(&harness, &entries, &prep);
    assert!(prep.is_split_turn);
    assert_eq!(prep.messages_to_summarize.len(), 2);
    assert_eq!(prep.turn_prefix_messages.len(), 2);

    let provider_dyn: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(["TURN"]));
    let result = run_async(async move { compact(prep, provider_dyn, "test-key", None).await })
        .expect("compact");
    log_result(&harness, &result);

    assert!(result.details.read_files.is_empty());
    assert_eq!(result.details.modified_files, vec!["turn.txt".to_string()]);
}

#[test]
fn prepare_compaction_considers_branch_summary_as_turn_start() {
    let harness = TestHarness::new("prepare_compaction_considers_branch_summary_as_turn_start");

    let entries = vec![
        branch_summary_entry("bs", None, "from", "summary"),
        message_entry("a0", Some("bs"), assistant_text(text_of_tokens(1), 0)),
        message_entry("a1", Some("a0"), assistant_text(text_of_tokens(1), 0)),
    ];

    let prep = prepare_compaction(&entries, make_settings(0)).expect("prep");
    log_preparation(&harness, &entries, &prep);

    assert_eq!(prep.first_kept_entry_id, "a1");
    assert!(prep.is_split_turn);
    assert_eq!(prep.turn_prefix_messages.len(), 2);
}

fn model_message_text(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        },
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::ToolResult(result) => result
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
