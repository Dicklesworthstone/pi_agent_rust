use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use asupersync::sync::OwnedMutexGuard;

use crate::model::{ContentBlock, UserContent};
use crate::session::{Session, SessionEntry, SessionMessage};
use crate::theme::TuiStyles;
use serde_json::json;

use super::conversation::{assistant_content_to_text, user_content_to_text};
use super::{
    AgentState, Cmd, ConversationMessage, EXTENSION_EVENT_TIMEOUT_MS, MessageRole, PiApp, PiMsg,
    conversation_from_session,
};

#[derive(Debug, Clone)]
pub(super) enum TreeUiState {
    Selector(TreeSelectorState),
    SummaryPrompt(TreeSummaryPromptState),
    CustomPrompt(TreeCustomPromptState),
}

#[derive(Debug, Clone)]
pub(super) struct TreeSelectorRow {
    pub(super) id: String,
    pub(super) parent_id: Option<String>,
    pub(super) display: String,
    pub(super) resubmit_text: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct TreeSelectorState {
    pub(super) rows: Vec<TreeSelectorRow>,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) max_visible_lines: usize,
    pub(super) label: Option<String>,
    pub(super) user_only: bool,
    pub(super) show_all: bool,
    pub(super) current_leaf_id: Option<String>,
    pub(super) last_selected_id: Option<String>,
    parent_by_id: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingTreeNavigation {
    pub(super) session_id: String,
    pub(super) old_leaf_id: Option<String>,
    pub(super) new_leaf_id: Option<String>,
    pub(super) editor_text: Option<String>,
    pub(super) entries_to_summarize: Vec<SessionEntry>,
    pub(super) summary_from_id: String,
    pub(super) api_key_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeSummaryChoice {
    NoSummary,
    Summarize,
    SummarizeWithCustomPrompt,
}

impl TreeSummaryChoice {
    pub(super) const fn all() -> [Self; 3] {
        [
            Self::NoSummary,
            Self::Summarize,
            Self::SummarizeWithCustomPrompt,
        ]
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::NoSummary => "No summary",
            Self::Summarize => "Summarize",
            Self::SummarizeWithCustomPrompt => "Summarize with custom prompt",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct TreeSummaryPromptState {
    pub(super) pending: PendingTreeNavigation,
    pub(super) selected: usize,
}

#[derive(Debug, Clone)]
pub(super) struct TreeCustomPromptState {
    pub(super) pending: PendingTreeNavigation,
    pub(super) instructions: String,
}

impl TreeSelectorState {
    pub(super) fn new(
        session: &Session,
        term_height: usize,
        initial_selected_id: Option<&str>,
        label: Option<String>,
    ) -> Self {
        let max_visible_lines = (term_height / 2).max(5);
        let current_leaf_id = session.leaf_id.clone();

        let mut state = Self {
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            max_visible_lines,
            label,
            user_only: false,
            show_all: false,
            current_leaf_id,
            last_selected_id: None,
            parent_by_id: HashMap::new(),
        };

        state.rebuild(session);

        let target_id = initial_selected_id.or(state.current_leaf_id.as_deref());
        state.selected = state.find_nearest_visible_index(target_id);
        state.last_selected_id = state.rows.get(state.selected).map(|row| row.id.clone());
        state.ensure_scroll_visible();
        state
    }

    pub(super) fn rebuild(&mut self, session: &Session) {
        (self.rows, self.parent_by_id) = build_tree_selector_rows(
            session,
            self.user_only,
            self.show_all,
            self.current_leaf_id.as_deref(),
        );

        if self.rows.is_empty() {
            self.selected = 0;
            self.scroll = 0;
        } else {
            let target = self
                .last_selected_id
                .as_deref()
                .or(self.current_leaf_id.as_deref());
            self.selected = self.find_nearest_visible_index(target);
            self.last_selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
            self.ensure_scroll_visible();
        }
    }

    fn ensure_scroll_visible(&mut self) {
        if self.rows.is_empty() {
            self.scroll = 0;
            return;
        }

        let max_visible = self.max_visible_lines.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + max_visible {
            self.scroll = self.selected + 1 - max_visible;
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max_index = self.rows.len() - 1;
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            let delta = usize::try_from(delta).unwrap_or(usize::MAX);
            self.selected = self.selected.saturating_add(delta).min(max_index);
        }
        self.last_selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
        self.ensure_scroll_visible();
    }

    fn find_nearest_visible_index(&self, target_id: Option<&str>) -> usize {
        if self.rows.is_empty() {
            return 0;
        }

        let visible: HashMap<&str, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(idx, row)| (row.id.as_str(), idx))
            .collect();

        let mut current = target_id.map(str::to_string);
        while let Some(id) = current.take() {
            if let Some(&idx) = visible.get(id.as_str()) {
                return idx;
            }
            current = self.parent_by_id.get(&id).and_then(Clone::clone);
        }

        self.rows.len().saturating_sub(1)
    }
}

pub(super) fn resolve_tree_selector_initial_id(session: &Session, args: &str) -> Option<String> {
    let arg = args.trim();
    if arg.is_empty() {
        return None;
    }

    // Backwards compatible: `/tree <index>` where index refers to leaf list.
    if let Ok(index) = arg.parse::<usize>() {
        let leaves = session.list_leaves();
        if index > 0 && index <= leaves.len() {
            return Some(leaves[index - 1].clone());
        }
        return None;
    }

    if session.get_entry(arg).is_some() {
        return Some(arg.to_string());
    }

    // Prefix match (only if unambiguous).
    let matches = session
        .entries
        .iter()
        .filter_map(|entry| entry.base_id().cloned())
        .filter(|id| id.starts_with(arg))
        .take(2)
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        return Some(matches[0].clone());
    }

    None
}

#[derive(Debug, Clone)]
struct ForkCandidate {
    id: String,
    summary: String,
}

fn fork_candidates(session: &Session) -> Vec<ForkCandidate> {
    let mut out = Vec::new();

    for entry in session.entries_for_current_path() {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };

        let Some(id) = message_entry.base.id.as_ref() else {
            continue;
        };

        let SessionMessage::User { content, .. } = &message_entry.message else {
            continue;
        };

        let text = user_content_to_text(content);
        let first_line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        let summary = if first_line.is_empty() {
            "(empty)".to_string()
        } else {
            super::truncate(first_line, 80)
        };

        out.push(ForkCandidate {
            id: id.clone(),
            summary,
        });
    }

    out
}

impl PiApp {
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_fork(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot fork while processing a request".to_string());
            return None;
        }

        let candidates = if let Ok(mut session_guard) = self.session.try_lock() {
            session_guard.ensure_entry_ids();
            fork_candidates(&session_guard)
        } else {
            self.status_message = Some("Session busy; try again".to_string());
            return None;
        };
        if candidates.is_empty() {
            self.status_message = Some("No user messages to fork from".to_string());
            return None;
        }

        if args.eq_ignore_ascii_case("list") || args.eq_ignore_ascii_case("ls") {
            let list = candidates
                .iter()
                .enumerate()
                .map(|(i, c)| format!("  {}. {} - {}", i + 1, c.id, c.summary))
                .collect::<Vec<_>>()
                .join("\n");
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: format!("Forkable user messages (use /fork <id|index>):\n{list}"),
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            return None;
        }

        let selection = if args.is_empty() {
            candidates.last().expect("candidates is non-empty").clone()
        } else if let Ok(index) = args.parse::<usize>() {
            if index == 0 || index > candidates.len() {
                self.status_message =
                    Some(format!("Invalid index: {index} (1-{})", candidates.len()));
                return None;
            }
            candidates[index - 1].clone()
        } else {
            let matches = candidates
                .iter()
                .filter(|c| c.id == args || c.id.starts_with(args))
                .cloned()
                .collect::<Vec<_>>();
            if matches.is_empty() {
                self.status_message = Some(format!("No user message id matches \"{args}\""));
                return None;
            }
            if matches.len() > 1 {
                self.status_message = Some(format!(
                    "Ambiguous id \"{args}\" (matches {})",
                    matches.len()
                ));
                return None;
            }
            matches[0].clone()
        };

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let model_provider = self.model_entry.model.provider.clone();
        let model_id = self.model_entry.model.id.clone();
        let (thinking_level, session_id) = if let Ok(guard) = self.session.try_lock() {
            (guard.header.thinking_level.clone(), guard.header.id.clone())
        } else {
            self.status_message = Some("Session busy; try again".to_string());
            return None;
        };

        self.agent_state = AgentState::Processing;
        self.status_message = Some("Forking session...".to_string());

        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            let cx = asupersync::Cx::for_request();
            if let Some(manager) = extensions.clone() {
                let cancelled = manager
                    .dispatch_cancellable_event(
                        crate::extensions::ExtensionEventName::SessionBeforeFork,
                        Some(json!({
                            "entryId": selection.id.clone(),
                            "summary": selection.summary.clone(),
                            "sessionId": session_id.clone(),
                        })),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(false);
                if cancelled {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &cx,
                        PiMsg::System("Fork cancelled by extension".to_string()),
                    )
                    .await;
                    return;
                }
            }

            let (fork_plan, parent_path, session_dir) = {
                let guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                let fork_plan = match guard.plan_fork_from_user_message(&selection.id) {
                    Ok(plan) => plan,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to build fork: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                let parent_path = guard.path.as_ref().map(|p| p.display().to_string());
                let session_dir = guard.session_dir.clone();
                drop(guard);
                (fork_plan, parent_path, session_dir)
            };

            let selected_text = fork_plan.selected_text.clone();

            let mut new_session = Session::create_with_dir(session_dir);
            new_session.header.provider = Some(model_provider);
            new_session.header.model_id = Some(model_id);
            new_session.header.thinking_level = thinking_level;
            if let Some(parent_path) = parent_path {
                new_session.set_branched_from(Some(parent_path));
            }
            new_session.init_from_fork_plan(fork_plan);
            let new_session_id = new_session.header.id.clone();

            if let Err(err) = new_session.save().await {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!("Failed to save fork: {err}")),
                )
                .await;
                return;
            }

            let messages_for_agent = new_session.to_messages_for_current_path();
            {
                let mut agent_guard = match OwnedMutexGuard::lock(Arc::clone(&agent), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                agent_guard.replace_messages(messages_for_agent);
            }

            {
                let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                *guard = new_session;
            }

            let (messages, usage) = {
                let guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                conversation_from_session(&guard)
            };

            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::ConversationReset {
                    messages,
                    usage,
                    status: Some(format!("Forked new session from {}", selection.summary)),
                },
            )
            .await;

            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::SetEditorText(selected_text),
            )
            .await;

            if let Some(manager) = extensions {
                let _ = manager
                    .dispatch_event(
                        crate::extensions::ExtensionEventName::SessionFork,
                        Some(json!({
                            "entryId": selection.id,
                            "summary": selection.summary,
                            "sessionId": session_id,
                            "newSessionId": new_session_id,
                        })),
                    )
                    .await;
            }
        });
        None
    }
}

#[allow(clippy::too_many_lines)]
fn build_tree_selector_rows(
    session: &Session,
    user_only: bool,
    show_all: bool,
    current_leaf_id: Option<&str>,
) -> (Vec<TreeSelectorRow>, HashMap<String, Option<String>>) {
    const fn is_settings_entry(entry: &SessionEntry) -> bool {
        matches!(
            entry,
            SessionEntry::Label(_)
                | SessionEntry::Custom(_)
                | SessionEntry::ModelChange(_)
                | SessionEntry::ThinkingLevelChange(_)
        )
    }

    const fn entry_is_user_message(entry: &SessionEntry) -> bool {
        match entry {
            SessionEntry::Message(message_entry) => {
                matches!(message_entry.message, SessionMessage::User { .. })
            }
            _ => false,
        }
    }

    const fn entry_is_visible(entry: &SessionEntry, user_only: bool, show_all: bool) -> bool {
        if user_only {
            return entry_is_user_message(entry);
        }
        if show_all {
            return true;
        }
        !is_settings_entry(entry)
    }

    fn extract_user_text(content: &UserContent) -> Option<String> {
        match content {
            UserContent::Text(text) => Some(text.clone()),
            UserContent::Blocks(blocks) => {
                let mut out = String::new();
                for block in blocks {
                    if let ContentBlock::Text(t) = block {
                        out.push_str(&t.text);
                    }
                }
                if out.trim().is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
        }
    }

    fn truncate_inline(text: &str, max: usize) -> String {
        use unicode_width::UnicodeWidthChar;
        if max == 0 {
            return String::new();
        }
        let mut out = String::with_capacity(max);
        let mut current_width = 0;
        for c in text.chars() {
            let c = if c == '\n' { ' ' } else { c };
            let w = c.width().unwrap_or(0);
            if current_width + w > max {
                while current_width > max.saturating_sub(1) {
                    if let Some(last) = out.pop() {
                        current_width -= last.width().unwrap_or(0);
                    } else {
                        break;
                    }
                }
                out.push('…');
                return out;
            }
            out.push(c);
            current_width += w;
        }
        out
    }

    fn describe_entry(entry: &SessionEntry) -> (String, Option<String>) {
        match entry {
            SessionEntry::Message(message_entry) => match &message_entry.message {
                SessionMessage::User { content, .. } => {
                    let text = extract_user_text(content).unwrap_or_default();
                    let preview = truncate_inline(text.trim(), 60);
                    (format!("user: \"{preview}\""), Some(text))
                }
                SessionMessage::Custom {
                    custom_type,
                    content,
                    ..
                } => {
                    let preview = truncate_inline(content.trim(), 60);
                    (
                        format!("custom:{custom_type}: \"{preview}\""),
                        Some(content.clone()),
                    )
                }
                SessionMessage::Assistant { message } => {
                    let (text, _) = assistant_content_to_text(&message.content);
                    let preview = truncate_inline(text.trim(), 60);
                    if preview.is_empty() {
                        ("assistant".to_string(), None)
                    } else {
                        (format!("assistant: \"{preview}\""), None)
                    }
                }
                SessionMessage::ToolResult { tool_name, .. } => {
                    (format!("tool_result: {tool_name}"), None)
                }
                SessionMessage::BashExecution { command, .. } => (format!("bash: {command}"), None),
                SessionMessage::BranchSummary { .. } => ("branch_summary".to_string(), None),
                SessionMessage::CompactionSummary { .. } => {
                    ("compaction_summary".to_string(), None)
                }
            },
            SessionEntry::Compaction(entry) => (
                format!("[compaction: {} tokens]", entry.tokens_before),
                None,
            ),
            SessionEntry::BranchSummary(_entry) => ("[branch_summary]".to_string(), None),
            SessionEntry::ModelChange(entry) => (
                format!("[model: {}/{}]", entry.provider, entry.model_id),
                None,
            ),
            SessionEntry::ThinkingLevelChange(entry) => {
                (format!("[thinking: {}]", entry.thinking_level), None)
            }
            SessionEntry::Label(entry) => (
                format!(
                    "[label: {} -> {}]",
                    entry.target_id,
                    entry.label.as_deref().unwrap_or("(cleared)")
                ),
                None,
            ),
            SessionEntry::SessionInfo(entry) => (
                format!(
                    "[session_info: {}]",
                    entry.name.as_deref().unwrap_or("(unnamed)")
                ),
                None,
            ),
            SessionEntry::Custom(entry) => (format!("[custom: {}]", entry.custom_type), None),
        }
    }

    #[derive(Debug, Clone)]
    struct DisplayNode {
        id: String,
        parent_id: Option<String>,
        text: String,
        resubmit_text: Option<String>,
        children: Vec<Self>,
    }

    fn build_display_nodes(
        id: &str,
        session: &Session,
        entry_index_by_id: &HashMap<String, usize>,
        children_by_parent: &HashMap<Option<String>, Vec<String>>,
        labels_by_target: &HashMap<String, String>,
        user_only: bool,
        show_all: bool,
    ) -> Vec<DisplayNode> {
        let Some(&idx) = entry_index_by_id.get(id) else {
            return Vec::new();
        };
        let Some(entry) = session.entries.get(idx) else {
            return Vec::new();
        };
        let is_visible = entry_is_visible(entry, user_only, show_all);

        let mut children_out = Vec::new();
        let child_ids = children_by_parent
            .get(&Some(id.to_string()))
            .cloned()
            .unwrap_or_default();
        for child_id in child_ids {
            children_out.extend(build_display_nodes(
                &child_id,
                session,
                entry_index_by_id,
                children_by_parent,
                labels_by_target,
                user_only,
                show_all,
            ));
        }

        if !is_visible {
            return children_out;
        }

        let (mut text, resubmit_text) = describe_entry(entry);
        if let Some(label) = labels_by_target.get(id) {
            let _ = write!(text, " [{label}]");
        }

        vec![DisplayNode {
            id: id.to_string(),
            parent_id: entry.base().parent_id.clone(),
            text,
            resubmit_text,
            children: children_out,
        }]
    }

    fn flatten_display_nodes(
        nodes: &[DisplayNode],
        prefix: &mut Vec<bool>,
        out: &mut Vec<TreeSelectorRow>,
        current_leaf_id: Option<&str>,
    ) {
        for (idx, node) in nodes.iter().enumerate() {
            let is_last = idx + 1 == nodes.len();

            let mut line = String::new();
            for has_more in prefix.iter().copied() {
                if has_more {
                    line.push_str("│  ");
                } else {
                    line.push_str("   ");
                }
            }
            line.push_str(if is_last { "└─ " } else { "├─ " });
            line.push_str(&node.text);

            if current_leaf_id.is_some_and(|leaf| leaf == node.id) {
                line.push_str(" ← active");
            }

            out.push(TreeSelectorRow {
                id: node.id.clone(),
                parent_id: node.parent_id.clone(),
                display: line,
                resubmit_text: node.resubmit_text.clone(),
            });

            prefix.push(!is_last);
            flatten_display_nodes(&node.children, prefix, out, current_leaf_id);
            prefix.pop();
        }
    }

    let mut parent_by_id: HashMap<String, Option<String>> = HashMap::new();
    let mut timestamp_by_id: HashMap<String, String> = HashMap::new();
    let mut entry_index_by_id: HashMap<String, usize> = HashMap::new();
    let mut children_by_parent: HashMap<Option<String>, Vec<String>> = HashMap::new();
    let mut labels_by_target: HashMap<String, String> = HashMap::new();

    for (idx, entry) in session.entries.iter().enumerate() {
        let Some(id) = entry.base_id().cloned() else {
            continue;
        };
        entry_index_by_id.insert(id.clone(), idx);
        parent_by_id.insert(id.clone(), entry.base().parent_id.clone());
        timestamp_by_id.insert(id.clone(), entry.base().timestamp.clone());

        children_by_parent
            .entry(entry.base().parent_id.clone())
            .or_default()
            .push(id.clone());

        if let SessionEntry::Label(label_entry) = entry {
            if let Some(label) = &label_entry.label {
                labels_by_target.insert(label_entry.target_id.clone(), label.clone());
            } else {
                labels_by_target.remove(&label_entry.target_id);
            }
        }
    }

    // Sort children by timestamp (oldest first).
    for children in children_by_parent.values_mut() {
        children.sort_by(|a, b| {
            let ta = timestamp_by_id
                .get(a)
                .map(String::as_str)
                .unwrap_or_default();
            let tb = timestamp_by_id
                .get(b)
                .map(String::as_str)
                .unwrap_or_default();
            ta.cmp(tb)
        });
    }

    let roots = children_by_parent.get(&None).cloned().unwrap_or_default();
    let mut display_roots = Vec::new();
    for root_id in roots {
        display_roots.extend(build_display_nodes(
            &root_id,
            session,
            &entry_index_by_id,
            &children_by_parent,
            &labels_by_target,
            user_only,
            show_all,
        ));
    }

    let mut rows = Vec::new();
    flatten_display_nodes(&display_roots, &mut Vec::new(), &mut rows, current_leaf_id);

    (rows, parent_by_id)
}

pub(super) fn collect_tree_branch_entries(
    session: &Session,
    old_leaf_id: Option<&str>,
    target_leaf_id: Option<&str>,
) -> (Vec<SessionEntry>, String) {
    let Some(old_leaf_id) = old_leaf_id else {
        return (Vec::new(), "root".to_string());
    };

    let common_ancestor_id: Option<String> = target_leaf_id.and_then(|target_id| {
        let old_path = session.get_path_to_entry(old_leaf_id);
        let target_path = session.get_path_to_entry(target_id);
        let mut lca: Option<String> = None;
        for (a, b) in old_path.iter().zip(target_path.iter()) {
            if a == b {
                lca = Some(a.clone());
            } else {
                break;
            }
        }
        lca
    });

    let mut entries_rev: Vec<SessionEntry> = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    let mut boundary_id: Option<String> = None;

    while let Some(id) = current.clone() {
        if common_ancestor_id
            .as_ref()
            .is_some_and(|ancestor| ancestor == &id)
        {
            boundary_id = Some(id);
            break;
        }

        let Some(entry) = session.get_entry(&id).cloned() else {
            break;
        };

        if matches!(entry, SessionEntry::Compaction(_)) {
            boundary_id = Some(id);
            entries_rev.push(entry);
            break;
        }

        current.clone_from(&entry.base().parent_id);
        entries_rev.push(entry);
        if current.is_none() {
            boundary_id = Some("root".to_string());
            break;
        }
    }

    entries_rev.reverse();

    let boundary = boundary_id
        .or(common_ancestor_id)
        .unwrap_or_else(|| "root".to_string());
    (entries_rev, boundary)
}

pub(super) fn view_tree_ui(tree_ui: &TreeUiState, styles: &TuiStyles) -> String {
    match tree_ui {
        TreeUiState::Selector(state) => view_tree_selector(state, styles),
        TreeUiState::SummaryPrompt(state) => view_tree_summary_prompt(state, styles),
        TreeUiState::CustomPrompt(state) => view_tree_custom_prompt(state, styles),
    }
}

fn view_tree_selector(state: &TreeSelectorState, styles: &TuiStyles) -> String {
    let mut out = String::new();
    let title = state
        .label
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Session Tree");
    let _ = writeln!(out, "  {}", styles.title.render(title));

    let filters = format!(
        "  Filters: user-only={}  show-all={}",
        if state.user_only { "on" } else { "off" },
        if state.show_all { "on" } else { "off" }
    );
    let _ = writeln!(out, "{}", styles.muted.render(&filters));
    out.push('\n');

    if state.rows.is_empty() {
        let _ = writeln!(out, "  {}", styles.muted_italic.render("(no entries)"));
    } else {
        let start = state.scroll.min(state.rows.len().saturating_sub(1));
        let end = (start + state.max_visible_lines).min(state.rows.len());

        for (idx, row) in state.rows.iter().enumerate().take(end).skip(start) {
            let prefix = if idx == state.selected { ">" } else { " " };
            let rendered = if idx == state.selected {
                styles.selection.render(&row.display)
            } else {
                row.display.clone()
            };
            let _ = writeln!(out, "{prefix} {rendered}");
        }
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "  {}",
        styles.muted.render(
            "↑/↓: navigate  Enter: select  Esc: cancel  Ctrl+U: user-only  Ctrl+O: show-all"
        )
    );
    out
}

fn view_tree_summary_prompt(state: &TreeSummaryPromptState, styles: &TuiStyles) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "  {}", styles.title.render("Branch Summary"));
    out.push('\n');

    if !state.pending.api_key_present {
        let _ = writeln!(
            out,
            "  {}",
            styles.warning.render(
                "Note: no API key configured; summarize options will behave like no summary."
            )
        );
        out.push('\n');
    }

    let options = TreeSummaryChoice::all();
    for (idx, opt) in options.iter().enumerate() {
        let prefix = if idx == state.selected { ">" } else { " " };
        let label = opt.label();
        let rendered = if idx == state.selected {
            styles.selection.render(label)
        } else {
            label.to_string()
        };
        let _ = writeln!(out, "  {prefix} {rendered}");
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "  {}",
        styles
            .muted
            .render("↑/↓: choose  Enter: confirm  Esc: cancel")
    );
    out
}

fn view_tree_custom_prompt(state: &TreeCustomPromptState, styles: &TuiStyles) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "  {}", styles.title.render("Custom Summary Prompt"));
    out.push('\n');

    let _ = writeln!(
        out,
        "  {}",
        styles
            .muted
            .render("Type extra instructions to guide the summary. Enter: run  Esc: back")
    );
    out.push('\n');

    let shown = if state.instructions.is_empty() {
        "(empty)".to_string()
    } else {
        state.instructions.clone()
    };
    let _ = writeln!(out, "  {}", styles.accent.render(&shown));
    out
}
