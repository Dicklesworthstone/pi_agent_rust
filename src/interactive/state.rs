use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use bubbles::list::{DefaultDelegate, Item as ListItem, List};

use crate::agent::QueueMode;
use crate::autocomplete::{
    AutocompleteCatalog, AutocompleteItem, AutocompleteProvider, AutocompleteResponse,
};
use crate::extensions::ExtensionUiRequest;
use crate::model::{ContentBlock, Message as ModelMessage};
use crate::models::OAuthConfig;
use crate::session::SiblingBranch;
use crate::session_index::{SessionIndex, SessionMeta};
use crate::session_picker::delete_session_file;
use crate::theme::Theme;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingLoginKind {
    OAuth,
    ApiKey,
    /// Device flow (RFC 8628) — user completes browser authorization and Pi polls for token.
    DeviceFlow,
}

#[derive(Debug, Clone)]
pub(super) struct PendingOAuth {
    pub(super) provider: String,
    pub(super) kind: PendingLoginKind,
    pub(super) verifier: String,
    /// OAuth config for extension-registered providers (None for built-in like anthropic).
    pub(super) oauth_config: Option<OAuthConfig>,
    /// Device code for RFC 8628 device flow providers.
    pub(super) device_code: Option<String>,
    /// The redirect URI used in the authorization request (needed for token exchange per RFC 6749 §4.1.3).
    pub(super) redirect_uri: Option<String>,
}

/// Tool output line count above which blocks auto-collapse.
pub(super) const TOOL_AUTO_COLLAPSE_THRESHOLD: usize = 20;
/// Number of preview lines to show when a tool block is collapsed.
pub(super) const TOOL_COLLAPSE_PREVIEW_LINES: usize = 5;

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
    pub thinking: Option<String>,
    /// Per-message collapse state for tool outputs.
    pub collapsed: bool,
}

impl ConversationMessage {
    /// Create a non-tool message (never collapsed).
    pub(super) const fn new(role: MessageRole, content: String, thinking: Option<String>) -> Self {
        Self {
            role,
            content,
            thinking,
            collapsed: false,
        }
    }

    /// Create a tool output message with auto-collapse for large outputs.
    pub(super) fn tool(content: String) -> Self {
        let line_count = memchr::memchr_iter(b'\n', content.as_bytes()).count() + 1;
        Self {
            role: MessageRole::Tool,
            content,
            thinking: None,
            collapsed: line_count > TOOL_AUTO_COLLAPSE_THRESHOLD,
        }
    }
}

/// Role of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

/// State of the agent processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Ready for input.
    Idle,
    /// Processing user request.
    Processing,
    /// Executing a tool.
    ToolRunning,
}

/// Input mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Single-line input mode (default).
    SingleLine,
    /// Multi-line input mode (activated with Shift+Enter or \).
    MultiLine,
}

#[derive(Debug, Clone)]
pub enum PendingInput {
    Text(String),
    Content(Vec<ContentBlock>),
    Continue,
}

/// Autocomplete dropdown state.
#[derive(Debug)]
pub(super) struct AutocompleteState {
    /// The autocomplete provider that generates suggestions.
    pub(super) provider: AutocompleteProvider,
    /// Whether the dropdown is currently visible.
    pub(super) open: bool,
    /// Current list of suggestions.
    pub(super) items: Vec<AutocompleteItem>,
    /// Index of the currently selected item, or `None` when the popup is open
    /// but the user has not yet navigated with arrow keys / Tab.
    pub(super) selected: Option<usize>,
    /// The range of text to replace when accepting a suggestion.
    pub(super) replace_range: std::ops::Range<usize>,
    /// Maximum number of items to display in the dropdown.
    pub(super) max_visible: usize,
}

impl AutocompleteState {
    pub(super) const fn new(cwd: PathBuf, catalog: AutocompleteCatalog) -> Self {
        Self {
            provider: AutocompleteProvider::new(cwd, catalog),
            open: false,
            items: Vec::new(),
            selected: None,
            replace_range: 0..0,
            max_visible: 10,
        }
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = None;
        self.replace_range = 0..0;
    }

    pub(super) fn open_with(&mut self, response: AutocompleteResponse) {
        if response.items.is_empty() {
            self.close();
            return;
        }

        // Preserve the selected item across periodic refreshes when the edit
        // target range is unchanged. This keeps arrow-key navigation stable
        // while typing (e.g. `/model ...`) even if suggestions are recomputed.
        let previous_selection = if response.replace == self.replace_range {
            self.selected_item().cloned()
        } else {
            None
        };

        self.open = true;
        self.items = response.items;
        self.selected = previous_selection.and_then(|selected| {
            self.items.iter().position(|candidate| {
                candidate.kind == selected.kind
                    && candidate.insert == selected.insert
                    && candidate.label == selected.label
            })
        });
        self.replace_range = response.replace;
    }

    pub(super) const fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = Some(match self.selected {
                Some(idx) => (idx + 1) % self.items.len(),
                None => 0,
            });
        }
    }

    pub(super) fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = Some(match self.selected {
                Some(idx) => idx.checked_sub(1).unwrap_or(self.items.len() - 1),
                None => self.items.len() - 1,
            });
        }
    }

    pub(super) fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.selected.and_then(|idx| self.items.get(idx))
    }

    /// Returns the scroll offset for the dropdown view.
    pub(super) const fn scroll_offset(&self) -> usize {
        match self.selected {
            Some(idx) if idx >= self.max_visible => idx - self.max_visible + 1,
            _ => 0,
        }
    }
}

/// Session picker overlay state for /resume command.
#[derive(Debug)]
pub(super) struct SessionPickerOverlay {
    /// Full list of available sessions.
    pub(super) all_sessions: Vec<SessionMeta>,
    /// List of available sessions.
    pub(super) sessions: Vec<SessionMeta>,
    /// Query used for typed filtering.
    query: String,
    /// Index of the currently selected session.
    pub(super) selected: usize,
    /// Maximum number of sessions to display.
    pub(super) max_visible: usize,
    /// Whether we're in delete confirmation mode.
    pub(super) confirm_delete: bool,
    /// Status message to render in the picker overlay.
    pub(super) status_message: Option<String>,
    /// Base directory for session storage (used for index cleanup).
    sessions_root: Option<PathBuf>,
}

impl SessionPickerOverlay {
    pub(super) fn new_with_root(
        sessions: Vec<SessionMeta>,
        sessions_root: Option<PathBuf>,
    ) -> Self {
        Self {
            all_sessions: sessions.clone(),
            sessions,
            query: String::new(),
            selected: 0,
            max_visible: 10,
            confirm_delete: false,
            status_message: None,
            sessions_root,
        }
    }

    pub(super) const fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    pub(super) fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
        }
    }

    pub(super) fn select_page_down(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = (self.selected + step).min(self.sessions.len().saturating_sub(1));
    }

    pub(super) fn select_page_up(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    pub(super) fn selected_session(&self) -> Option<&SessionMeta> {
        self.sessions.get(self.selected)
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) const fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    pub(super) fn push_chars<I: IntoIterator<Item = char>>(&mut self, chars: I) {
        let mut changed = false;
        for ch in chars {
            if !ch.is_control() {
                self.query.push(ch);
                changed = true;
            }
        }
        if changed {
            self.rebuild_filtered_sessions();
        }
    }

    pub(super) fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.rebuild_filtered_sessions();
        }
    }

    /// Returns the scroll offset for the dropdown view.
    pub(super) const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    /// Remove the selected session from the list and adjust selection.
    pub(super) fn remove_selected(&mut self) {
        let Some(selected_session) = self.selected_session().cloned() else {
            return;
        };
        self.all_sessions
            .retain(|session| session.path != selected_session.path);
        self.rebuild_filtered_sessions();
        // Clear confirmation state
        self.confirm_delete = false;
    }

    pub(super) fn delete_selected(&mut self) -> crate::error::Result<()> {
        let Some(session_meta) = self.selected_session().cloned() else {
            return Ok(());
        };
        let path = PathBuf::from(&session_meta.path);
        delete_session_file(&path)?;
        if let Some(root) = self.sessions_root.as_ref() {
            let index = SessionIndex::for_sessions_root(root);
            let _ = index.delete_session_path(&path);
        }
        self.remove_selected();
        Ok(())
    }

    fn rebuild_filtered_sessions(&mut self) {
        let query = self.query.trim().to_ascii_lowercase();
        if query.is_empty() {
            self.sessions = self.all_sessions.clone();
        } else {
            self.sessions = self
                .all_sessions
                .iter()
                .filter(|session| Self::session_matches_query(session, &query))
                .cloned()
                .collect();
        }

        if self.sessions.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }
    }

    fn session_matches_query(session: &SessionMeta, query_lower: &str) -> bool {
        let in_name = session
            .name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().contains(query_lower));
        let in_id = session.id.to_ascii_lowercase().contains(query_lower);
        let in_file_name = Path::new(&session.path)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|file_name| file_name.to_ascii_lowercase().contains(query_lower));
        let in_timestamp = session.timestamp.to_ascii_lowercase().contains(query_lower);
        let in_message_count = session.message_count.to_string().contains(query_lower);

        in_name || in_id || in_file_name || in_timestamp || in_message_count
    }
}

/// Settings selector overlay state for /settings command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsUiEntry {
    Summary,
    Theme,
    SteeringMode,
    FollowUpMode,
    DefaultPermissive,
    QuietStartup,
    CollapseChangelog,
    HideThinkingBlock,
    ShowHardwareCursor,
    DoubleEscapeAction,
    EditorPaddingX,
    AutocompleteMaxVisible,
}

#[derive(Debug, Clone)]
pub(super) enum ThemePickerItem {
    BuiltIn(&'static str),
    File { path: PathBuf, name: String },
}

#[derive(Debug)]
pub(super) struct ThemePickerOverlay {
    pub(super) items: Vec<ThemePickerItem>,
    pub(super) selected: usize,
    pub(super) max_visible: usize,
}

impl ThemePickerOverlay {
    pub(super) fn new(cwd: &Path) -> Self {
        let mut items = Vec::new();
        items.push(ThemePickerItem::BuiltIn("dark"));
        items.push(ThemePickerItem::BuiltIn("light"));
        items.push(ThemePickerItem::BuiltIn("solarized"));
        items.extend(Theme::discover_themes(cwd).into_iter().map(|path| {
            let name = Theme::load(&path).map_or_else(
                |_| {
                    path.file_stem().map_or_else(
                        || "unknown".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    )
                },
                |t| t.name,
            );
            ThemePickerItem::File { path, name }
        }));
        Self {
            items,
            selected: 0,
            max_visible: 10,
        }
    }

    pub(super) const fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub(super) fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    pub(super) fn select_page_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = (self.selected + step).min(self.items.len().saturating_sub(1));
    }

    pub(super) fn select_page_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    pub(super) const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    pub(super) fn selected_item(&self) -> Option<&ThemePickerItem> {
        self.items.get(self.selected)
    }
}

#[derive(Debug)]
pub(super) struct SettingsUiState {
    pub(super) entries: Vec<SettingsUiEntry>,
    pub(super) selected: usize,
    pub(super) max_visible: usize,
}

impl SettingsUiState {
    pub(super) fn new() -> Self {
        Self {
            entries: vec![
                SettingsUiEntry::Summary,
                SettingsUiEntry::Theme,
                SettingsUiEntry::SteeringMode,
                SettingsUiEntry::FollowUpMode,
                SettingsUiEntry::DefaultPermissive,
                SettingsUiEntry::QuietStartup,
                SettingsUiEntry::CollapseChangelog,
                SettingsUiEntry::HideThinkingBlock,
                SettingsUiEntry::ShowHardwareCursor,
                SettingsUiEntry::DoubleEscapeAction,
                SettingsUiEntry::EditorPaddingX,
                SettingsUiEntry::AutocompleteMaxVisible,
            ],
            selected: 0,
            max_visible: 10,
        }
    }

    pub(super) const fn select_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    pub(super) fn select_prev(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.entries.len() - 1);
        }
    }

    pub(super) fn select_page_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = (self.selected + step).min(self.entries.len().saturating_sub(1));
    }

    pub(super) fn select_page_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    pub(super) fn selected_entry(&self) -> Option<SettingsUiEntry> {
        self.entries.get(self.selected).copied()
    }

    pub(super) const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }
}

/// User action choices for a capability prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapabilityAction {
    AllowOnce,
    AllowAlways,
    Deny,
    DenyAlways,
}

impl CapabilityAction {
    pub(super) const ALL: [Self; 4] = [
        Self::AllowOnce,
        Self::AllowAlways,
        Self::Deny,
        Self::DenyAlways,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "Allow Once",
            Self::AllowAlways => "Allow Always",
            Self::Deny => "Deny",
            Self::DenyAlways => "Deny Always",
        }
    }

    pub(super) const fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }

    pub(super) const fn is_persistent(self) -> bool {
        matches!(self, Self::AllowAlways | Self::DenyAlways)
    }
}

/// Modal overlay for extension capability prompts.
#[derive(Debug)]
pub(super) struct CapabilityPromptOverlay {
    /// The underlying UI request (used to send response).
    pub(super) request: ExtensionUiRequest,
    /// Extension that requested the capability.
    pub(super) extension_id: String,
    /// Capability being requested (e.g. "exec", "http").
    pub(super) capability: String,
    /// Human-readable description of what the capability does.
    pub(super) description: String,
    /// Which button is focused.
    pub(super) focused: usize,
    /// Auto-deny countdown (remaining seconds).  `None` = no timer.
    pub(super) auto_deny_secs: Option<u32>,
}

impl CapabilityPromptOverlay {
    pub(super) fn from_request(request: ExtensionUiRequest) -> Self {
        let extension_id = request
            .payload
            .get("extension_id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();
        let capability = request
            .payload
            .get("capability")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let description = request
            .payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Self {
            request,
            extension_id,
            capability,
            description,
            focused: 0,
            auto_deny_secs: Some(30),
        }
    }

    pub(super) const fn focus_next(&mut self) {
        self.focused = (self.focused + 1) % CapabilityAction::ALL.len();
    }

    pub(super) fn focus_prev(&mut self) {
        self.focused = self
            .focused
            .checked_sub(1)
            .unwrap_or(CapabilityAction::ALL.len() - 1);
    }

    pub(super) const fn selected_action(&self) -> CapabilityAction {
        CapabilityAction::ALL[self.focused]
    }

    /// Returns `true` if this is a capability-specific confirm prompt (not a
    /// generic extension confirm).
    pub(super) fn is_capability_prompt(request: &ExtensionUiRequest) -> bool {
        request.method == "confirm"
            && request.payload.get("capability").is_some()
            && request.payload.get("extension_id").is_some()
    }
}

/// Runtime state for extension-driven `ui.custom()` overlays.
#[derive(Debug, Clone, Default)]
pub(super) struct ExtensionCustomOverlay {
    /// Extension that owns the active custom overlay.
    pub(super) extension_id: Option<String>,
    /// Optional overlay title.
    pub(super) title: Option<String>,
    /// Latest rendered frame lines.
    pub(super) lines: Vec<String>,
}

/// Branch picker overlay for quick branch switching (Ctrl+B).
#[derive(Debug)]
pub(super) struct BranchPickerOverlay {
    /// Sibling branches at the nearest fork point.
    pub(super) branches: Vec<SiblingBranch>,
    /// Which branch is currently selected in the picker.
    pub(super) selected: usize,
    /// Maximum visible rows before scrolling.
    pub(super) max_visible: usize,
}

impl BranchPickerOverlay {
    pub(super) fn new(branches: Vec<SiblingBranch>) -> Self {
        let current_idx = branches.iter().position(|b| b.is_current).unwrap_or(0);
        Self {
            branches,
            selected: current_idx,
            max_visible: 10,
        }
    }

    pub(super) const fn select_next(&mut self) {
        if !self.branches.is_empty() {
            self.selected = (self.selected + 1) % self.branches.len();
        }
    }

    pub(super) fn select_prev(&mut self) {
        if !self.branches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.branches.len() - 1);
        }
    }

    pub(super) fn select_page_down(&mut self) {
        if self.branches.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = (self.selected + step).min(self.branches.len().saturating_sub(1));
    }

    pub(super) fn select_page_up(&mut self) {
        if self.branches.is_empty() {
            return;
        }
        let step = self.max_visible.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    pub(super) const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    pub(super) fn selected_branch(&self) -> Option<&SiblingBranch> {
        self.branches.get(self.selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuedMessageKind {
    Steering,
    FollowUp,
}

#[derive(Debug)]
pub(super) struct InteractiveMessageQueue {
    pub(super) steering: VecDeque<String>,
    pub(super) follow_up: VecDeque<String>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl InteractiveMessageQueue {
    pub(super) const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    pub(super) const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    pub(super) fn push_steering(&mut self, text: String) {
        self.steering.push_back(text);
    }

    pub(super) fn push_follow_up(&mut self, text: String) {
        self.follow_up.push_back(text);
    }

    pub(super) fn pop_steering(&mut self) -> Vec<String> {
        self.pop_kind(QueuedMessageKind::Steering)
    }

    pub(super) fn pop_follow_up(&mut self) -> Vec<String> {
        self.pop_kind(QueuedMessageKind::FollowUp)
    }

    fn pop_kind(&mut self, kind: QueuedMessageKind) -> Vec<String> {
        let (queue, mode) = match kind {
            QueuedMessageKind::Steering => (&mut self.steering, self.steering_mode),
            QueuedMessageKind::FollowUp => (&mut self.follow_up, self.follow_up_mode),
        };
        match mode {
            QueueMode::All => queue.drain(..).collect(),
            QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
        }
    }

    pub(super) fn clear_all(&mut self) -> (Vec<String>, Vec<String>) {
        let steering = self.steering.drain(..).collect();
        let follow_up = self.follow_up.drain(..).collect();
        (steering, follow_up)
    }

    pub(super) fn steering_len(&self) -> usize {
        self.steering.len()
    }

    pub(super) fn follow_up_len(&self) -> usize {
        self.follow_up.len()
    }

    pub(super) fn steering_front(&self) -> Option<&String> {
        self.steering.front()
    }

    pub(super) fn follow_up_front(&self) -> Option<&String> {
        self.follow_up.front()
    }
}

#[derive(Debug)]
pub(super) struct InjectedMessageQueue {
    steering: VecDeque<ModelMessage>,
    follow_up: VecDeque<ModelMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl InjectedMessageQueue {
    pub(super) const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    pub(super) const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn push_kind(&mut self, kind: QueuedMessageKind, message: ModelMessage) {
        match kind {
            QueuedMessageKind::Steering => self.steering.push_back(message),
            QueuedMessageKind::FollowUp => self.follow_up.push_back(message),
        }
    }

    pub(super) fn push_steering(&mut self, message: ModelMessage) {
        self.push_kind(QueuedMessageKind::Steering, message);
    }

    pub(super) fn push_follow_up(&mut self, message: ModelMessage) {
        self.push_kind(QueuedMessageKind::FollowUp, message);
    }

    fn pop_kind(&mut self, kind: QueuedMessageKind) -> Vec<ModelMessage> {
        let (queue, mode) = match kind {
            QueuedMessageKind::Steering => (&mut self.steering, self.steering_mode),
            QueuedMessageKind::FollowUp => (&mut self.follow_up, self.follow_up_mode),
        };
        match mode {
            QueueMode::All => queue.drain(..).collect(),
            QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
        }
    }

    pub(super) fn pop_steering(&mut self) -> Vec<ModelMessage> {
        self.pop_kind(QueuedMessageKind::Steering)
    }

    pub(super) fn pop_follow_up(&mut self) -> Vec<ModelMessage> {
        self.pop_kind(QueuedMessageKind::FollowUp)
    }
}

#[derive(Debug, Clone)]
pub(super) struct HistoryItem {
    pub(super) value: String,
}

impl ListItem for HistoryItem {
    fn filter_value(&self) -> &str {
        &self.value
    }
}

#[derive(Clone)]
pub(super) struct HistoryList {
    // We never render the list UI; we use it as a battle-tested cursor+navigation model.
    // The final item is always a sentinel representing "empty input".
    list: List<HistoryItem, DefaultDelegate>,
}

impl HistoryList {
    pub(super) fn new() -> Self {
        let mut list = List::new(
            vec![HistoryItem {
                value: String::new(),
            }],
            DefaultDelegate::new(),
            0,
            0,
        );

        // Keep behavior minimal/predictable for now; this is used as an index model.
        list.filtering_enabled = false;
        list.infinite_scrolling = false;

        // Start at the "empty input" sentinel.
        list.select(0);

        Self { list }
    }

    pub(super) fn entries(&self) -> &[HistoryItem] {
        let items = self.list.items();
        if items.len() <= 1 {
            return &[];
        }
        &items[..items.len().saturating_sub(1)]
    }

    pub(super) fn has_entries(&self) -> bool {
        !self.entries().is_empty()
    }

    pub(super) fn cursor_is_empty(&self) -> bool {
        // Sentinel is always the final item.
        self.list.index() + 1 == self.list.items().len()
    }

    pub(super) fn reset_cursor(&mut self) {
        let last = self.list.items().len().saturating_sub(1);
        self.list.select(last);
    }

    pub(super) fn push(&mut self, value: String) {
        let mut items = self.entries().to_vec();
        items.push(HistoryItem { value });
        items.push(HistoryItem {
            value: String::new(),
        });

        self.list.set_items(items);
        self.reset_cursor();
    }

    pub(super) fn cursor_up(&mut self) {
        self.list.cursor_up();
    }

    pub(super) fn cursor_down(&mut self) {
        self.list.cursor_down();
    }

    pub(super) fn selected_value(&self) -> &str {
        self.list
            .selected_item()
            .map_or("", |item| item.value.as_str())
    }
}

/// Progress metrics emitted by long-running tools (e.g. bash).
#[derive(Debug, Clone)]
pub(super) struct ToolProgress {
    pub(super) started_at: std::time::Instant,
    pub(super) elapsed_ms: u128,
    pub(super) line_count: usize,
    pub(super) byte_count: usize,
    pub(super) timeout_ms: Option<u64>,
}

impl ToolProgress {
    pub(super) fn new() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            elapsed_ms: 0,
            line_count: 0,
            byte_count: 0,
            timeout_ms: None,
        }
    }

    /// Update from a `details.progress` JSON object emitted by tool callbacks.
    pub(super) fn update_from_details(&mut self, details: Option<&Value>) {
        // Always update elapsed from wall clock as fallback.
        self.elapsed_ms = self.started_at.elapsed().as_millis();

        let Some(details) = details else {
            return;
        };
        if let Some(progress) = details.get("progress") {
            if let Some(v) = progress.get("elapsedMs").and_then(Value::as_u64) {
                self.elapsed_ms = u128::from(v);
            }
            if let Some(v) = progress.get("lineCount").and_then(Value::as_u64) {
                #[allow(clippy::cast_possible_truncation)]
                let count = v as usize;
                self.line_count = count;
            }
            if let Some(v) = progress.get("byteCount").and_then(Value::as_u64) {
                #[allow(clippy::cast_possible_truncation)]
                let count = v as usize;
                self.byte_count = count;
            }
            if let Some(v) = progress.get("timeoutMs").and_then(Value::as_u64) {
                self.timeout_ms = Some(v);
            }
        }
    }
}

/// Format a count with K/M suffix for compact display.
#[allow(clippy::cast_precision_loss)]
pub(super) fn format_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_item(id: &str) -> AutocompleteItem {
        AutocompleteItem {
            kind: crate::autocomplete::AutocompleteItemKind::Model,
            label: id.to_string(),
            insert: id.to_string(),
            description: None,
        }
    }

    fn response(
        replace_range: std::ops::Range<usize>,
        items: impl IntoIterator<Item = &'static str>,
    ) -> AutocompleteResponse {
        AutocompleteResponse {
            replace: replace_range,
            items: items.into_iter().map(model_item).collect(),
        }
    }

    #[test]
    fn autocomplete_refresh_preserves_selected_item_when_replace_range_unchanged() {
        let mut state = AutocompleteState::new(PathBuf::from("."), AutocompleteCatalog::default());
        state.open_with(response(0..6, ["gpt-4o", "gpt-5.2", "claude-opus-4-5"]));

        state.select_next();
        state.select_next();
        assert_eq!(
            state.selected_item().map(|item| item.label.as_str()),
            Some("gpt-5.2")
        );

        // Recompute suggestions (same replace range) in a different order.
        state.open_with(response(0..6, ["claude-opus-4-5", "gpt-5.2", "gpt-4o"]));

        assert_eq!(
            state.selected_item().map(|item| item.label.as_str()),
            Some("gpt-5.2")
        );
    }

    #[test]
    fn autocomplete_refresh_clears_selection_when_replace_range_changes() {
        let mut state = AutocompleteState::new(PathBuf::from("."), AutocompleteCatalog::default());
        state.open_with(response(0..6, ["gpt-4o", "gpt-5.2"]));
        state.select_next();
        assert_eq!(
            state.selected_item().map(|item| item.label.as_str()),
            Some("gpt-4o")
        );

        // Cursor/token moved: replace range changed, so selection should reset.
        state.open_with(response(2..8, ["gpt-4o", "gpt-5.2"]));
        assert!(state.selected_item().is_none());
    }

    #[test]
    fn autocomplete_refresh_clears_selection_when_selected_item_disappears() {
        let mut state = AutocompleteState::new(PathBuf::from("."), AutocompleteCatalog::default());
        state.open_with(response(0..6, ["gpt-4o", "gpt-5.2"]));
        state.select_next();
        state.select_next();
        assert_eq!(
            state.selected_item().map(|item| item.label.as_str()),
            Some("gpt-5.2")
        );

        // Selected suggestion no longer present after refresh.
        state.open_with(response(0..6, ["gpt-4o"]));
        assert!(state.selected_item().is_none());
    }

    #[test]
    fn settings_ui_includes_default_permissive_toggle() {
        let state = SettingsUiState::new();
        assert!(state.entries.contains(&SettingsUiEntry::DefaultPermissive));
    }
}
