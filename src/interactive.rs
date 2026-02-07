//! Interactive TUI mode using charmed_rust (bubbletea/lipgloss/bubbles/glamour).
//!
//! This module provides the full interactive terminal interface for Pi,
//! implementing the Elm Architecture for state management.
//!
//! ## Features
//!
//! - **Multi-line editor**: Full text area with line wrapping and history
//! - **Viewport scrolling**: Scrollable conversation history with keyboard navigation
//! - **Slash commands**: Built-in commands like /help, /clear, /model, /exit
//! - **Token tracking**: Real-time cost and token usage display
//! - **Markdown rendering**: Assistant responses rendered with syntax highlighting

use asupersync::Cx;
use asupersync::channel::mpsc;
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::Mutex;
use async_trait::async_trait;
use bubbles::list::{DefaultDelegate, Item as ListItem, List};
use bubbles::spinner::{SpinnerModel, spinners};
use bubbles::textarea::TextArea;
use bubbles::viewport::Viewport;
use bubbletea::{
    Cmd, KeyMsg, KeyType, Message, Model as BubbleteaModel, Program, WindowSizeMsg, batch, quit,
};
use chrono::Utc;
use crossterm::{cursor, terminal};
use futures::future::BoxFuture;
use glamour::{Renderer as MarkdownRenderer, StyleConfig as GlamourStyleConfig};
use glob::Pattern;
use serde_json::{Value, json};
use url::Url;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::agent::{AbortHandle, Agent, AgentEvent, QueueMode};
use crate::autocomplete::{
    AutocompleteCatalog, AutocompleteItem, AutocompleteItemKind, AutocompleteProvider,
    AutocompleteResponse,
};
use crate::config::{Config, SettingsScope};
use crate::extension_events::{InputEventOutcome, apply_input_event_response};
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionDeliverAs, ExtensionEventName, ExtensionHostActions,
    ExtensionManager, ExtensionSendMessage, ExtensionSendUserMessage, ExtensionSession,
    ExtensionUiRequest, ExtensionUiResponse, extension_event_from_agent,
};
use crate::keybindings::{AppAction, KeyBinding, KeyBindings};
use crate::model::{
    AssistantMessageEvent, ContentBlock, CustomMessage, ImageContent, Message as ModelMessage,
    StopReason, TextContent, ThinkingLevel, Usage, UserContent, UserMessage,
};
use crate::models::{ModelEntry, ModelRegistry, OAuthConfig, default_models_path};
use crate::package_manager::PackageManager;
use crate::providers;
use crate::resources::{DiagnosticKind, ResourceCliOptions, ResourceDiagnostic, ResourceLoader};
use crate::session::{Session, SessionEntry, SessionMessage, bash_execution_to_text};
use crate::session_index::{SessionIndex, SessionMeta};
use crate::session_picker::delete_session_file;
use crate::theme::{Theme, TuiStyles};
use crate::tools::{process_file_arguments, resolve_read_path};

#[cfg(all(feature = "clipboard", feature = "image-resize"))]
use arboard::Clipboard as ArboardClipboard;
#[cfg(feature = "clipboard")]
use clipboard::{ClipboardContext, ClipboardProvider};

// ============================================================================
// Slash Commands
// ============================================================================

/// Available slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Login,
    Logout,
    Clear,
    Model,
    Thinking,
    ScopedModels,
    Exit,
    History,
    Export,
    Session,
    Settings,
    Theme,
    Resume,
    New,
    Copy,
    Name,
    Hotkeys,
    Changelog,
    Tree,
    Fork,
    Compact,
    Reload,
    Share,
}

impl PiApp {
    /// Scroll the conversation viewport to the bottom.
    fn scroll_to_bottom(&mut self) {
        let content = self.build_conversation_content();
        self.conversation_viewport.set_content(&content);
        self.conversation_viewport.goto_bottom();
    }

    fn scroll_to_last_match(&mut self, needle: &str) {
        let content = self.build_conversation_content();
        self.conversation_viewport.set_content(&content);

        let mut last_index = None;
        for (idx, line) in content.lines().enumerate() {
            if line.contains(needle) {
                last_index = Some(idx);
            }
        }

        if let Some(idx) = last_index {
            self.conversation_viewport.set_y_offset(idx);
        } else {
            self.conversation_viewport.goto_bottom();
        }
    }

    fn apply_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.styles = self.theme.tui_styles();
        self.markdown_style = self.theme.glamour_style_config();
        self.spinner =
            SpinnerModel::with_spinner(spinners::dot()).style(self.styles.accent.clone());

        let content = self.build_conversation_content();
        self.conversation_viewport.set_content(&content);
    }

    fn format_themes_list(&self) -> String {
        let mut names = Vec::new();
        names.push("dark".to_string());
        names.push("light".to_string());
        names.push("solarized".to_string());

        for path in Theme::discover_themes(&self.cwd) {
            if let Ok(theme) = Theme::load(&path) {
                names.push(theme.name);
            }
        }

        names.sort_by_key(|a| a.to_ascii_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

        let mut output = String::from("Available themes:\n");
        for name in names {
            let marker = if name.eq_ignore_ascii_case(&self.theme.name) {
                "* "
            } else {
                "  "
            };
            let _ = writeln!(output, "{marker}{name}");
        }
        output.push_str("\nUse /theme <name> to switch");
        output
    }

    fn format_scoped_models_status(&self) -> String {
        let patterns = self.config.enabled_models.as_deref().unwrap_or(&[]);
        let scope_configured = !patterns.is_empty();

        let mut output = String::new();
        let current = format!(
            "{}/{}",
            self.model_entry.model.provider, self.model_entry.model.id
        );
        let _ = writeln!(output, "Current model: {current}");
        let _ = writeln!(output);

        if !scope_configured {
            let _ = writeln!(output, "Scoped models: (all models)");
            let _ = writeln!(output);
            output.push_str("Use /scoped-models <patterns> to scope Ctrl+P cycling.\n");
            output.push_str("Use /scoped-models clear to clear scope.\n");
            return output;
        }

        output.push_str("Scoped model patterns:\n");
        for pattern in patterns {
            let _ = writeln!(output, "  - {pattern}");
        }
        let _ = writeln!(output);

        output.push_str("Scoped models (matched):\n");
        if self.model_scope.is_empty() {
            output.push_str("  (none)\n");
        } else {
            let mut models = self
                .model_scope
                .iter()
                .map(|entry| format!("{}/{}", entry.model.provider, entry.model.id))
                .collect::<Vec<_>>();
            models.sort_by_key(|value| value.to_ascii_lowercase());
            models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            for model in models {
                let _ = writeln!(output, "  - {model}");
            }
        }
        let _ = writeln!(output);

        output.push_str("Use /scoped-models clear to cycle all models.\n");
        output
    }

    fn persist_project_theme(&self, theme_name: &str) -> crate::error::Result<()> {
        let settings_path = self.cwd.join(Config::project_dir()).join("settings.json");
        let mut settings = if settings_path.exists() {
            let content = std::fs::read_to_string(&settings_path)?;
            serde_json::from_str::<Value>(&content)?
        } else {
            json!({})
        };

        let obj = settings.as_object_mut().ok_or_else(|| {
            crate::error::Error::config(format!(
                "Settings file is not a JSON object: {}",
                settings_path.display()
            ))
        })?;
        obj.insert("theme".to_string(), Value::String(theme_name.to_string()));

        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(settings_path, serde_json::to_string_pretty(&settings)?)?;
        Ok(())
    }

    fn apply_queue_modes(&self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        if let Ok(mut queue) = self.message_queue.lock() {
            queue.set_modes(steering_mode, follow_up_mode);
        }

        if let Ok(mut agent_guard) = self.agent.try_lock() {
            agent_guard.set_queue_modes(steering_mode, follow_up_mode);
            return;
        }

        let agent = Arc::clone(&self.agent);
        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            let cx = Cx::for_request();
            if let Ok(mut agent_guard) = agent.lock(&cx).await {
                agent_guard.set_queue_modes(steering_mode, follow_up_mode);
            }
        });
    }

    fn toggle_queue_mode_setting(&mut self, entry: SettingsUiEntry) {
        let (key, current) = match entry {
            SettingsUiEntry::SteeringMode => ("steeringMode", self.config.steering_queue_mode()),
            SettingsUiEntry::FollowUpMode => ("followUpMode", self.config.follow_up_queue_mode()),
            _ => return,
        };

        let next = match current {
            QueueMode::All => QueueMode::OneAtATime,
            QueueMode::OneAtATime => QueueMode::All,
        };

        let patch = match entry {
            SettingsUiEntry::SteeringMode => json!({ "steeringMode": next.as_str() }),
            SettingsUiEntry::FollowUpMode => json!({ "followUpMode": next.as_str() }),
            _ => json!({}),
        };

        let global_dir = Config::global_dir();
        if let Err(err) =
            Config::patch_settings_with_roots(SettingsScope::Project, &global_dir, &self.cwd, patch)
        {
            self.status_message = Some(format!("Failed to update {key}: {err}"));
            return;
        }

        match entry {
            SettingsUiEntry::SteeringMode => {
                self.config.steering_mode = Some(next.as_str().to_string());
            }
            SettingsUiEntry::FollowUpMode => {
                self.config.follow_up_mode = Some(next.as_str().to_string());
            }
            _ => {}
        }

        let steering_mode = self.config.steering_queue_mode();
        let follow_up_mode = self.config.follow_up_queue_mode();
        self.apply_queue_modes(steering_mode, follow_up_mode);
        self.status_message = Some(format!("Updated {key}: {}", next.as_str()));
    }

    fn persist_project_settings_patch(&mut self, key: &str, patch: Value) -> bool {
        let global_dir = Config::global_dir();
        if let Err(err) =
            Config::patch_settings_with_roots(SettingsScope::Project, &global_dir, &self.cwd, patch)
        {
            self.status_message = Some(format!("Failed to update {key}: {err}"));
            return false;
        }
        true
    }

    fn effective_show_hardware_cursor(&self) -> bool {
        self.config.show_hardware_cursor.unwrap_or_else(|| {
            std::env::var("PI_HARDWARE_CURSOR")
                .ok()
                .is_some_and(|val| val == "1")
        })
    }

    fn apply_hardware_cursor(show: bool) {
        let mut stdout = std::io::stdout();
        if show {
            let _ = crossterm::execute!(stdout, cursor::Show);
        } else {
            let _ = crossterm::execute!(stdout, cursor::Hide);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn toggle_settings_entry(&mut self, entry: SettingsUiEntry) {
        match entry {
            SettingsUiEntry::SteeringMode | SettingsUiEntry::FollowUpMode => {
                self.toggle_queue_mode_setting(entry);
            }
            SettingsUiEntry::QuietStartup => {
                let next = !self.config.quiet_startup.unwrap_or(false);
                if self.persist_project_settings_patch(
                    "quietStartup",
                    json!({ "quiet_startup": next }),
                ) {
                    self.config.quiet_startup = Some(next);
                    self.status_message =
                        Some(format!("Updated quietStartup: {}", bool_label(next)));
                }
            }
            SettingsUiEntry::CollapseChangelog => {
                let next = !self.config.collapse_changelog.unwrap_or(false);
                if self.persist_project_settings_patch(
                    "collapseChangelog",
                    json!({ "collapse_changelog": next }),
                ) {
                    self.config.collapse_changelog = Some(next);
                    self.status_message =
                        Some(format!("Updated collapseChangelog: {}", bool_label(next)));
                }
            }
            SettingsUiEntry::HideThinkingBlock => {
                let next = !self.config.hide_thinking_block.unwrap_or(false);
                if self.persist_project_settings_patch(
                    "hideThinkingBlock",
                    json!({ "hide_thinking_block": next }),
                ) {
                    self.config.hide_thinking_block = Some(next);
                    self.thinking_visible = !next;
                    self.scroll_to_bottom();
                    self.status_message =
                        Some(format!("Updated hideThinkingBlock: {}", bool_label(next)));
                }
            }
            SettingsUiEntry::ShowHardwareCursor => {
                let next = !self.effective_show_hardware_cursor();
                if self.persist_project_settings_patch(
                    "showHardwareCursor",
                    json!({ "show_hardware_cursor": next }),
                ) {
                    self.config.show_hardware_cursor = Some(next);
                    Self::apply_hardware_cursor(next);
                    self.status_message =
                        Some(format!("Updated showHardwareCursor: {}", bool_label(next)));
                }
            }
            SettingsUiEntry::DoubleEscapeAction => {
                let current = self
                    .config
                    .double_escape_action
                    .as_deref()
                    .unwrap_or("tree");
                let next = if current.eq_ignore_ascii_case("tree") {
                    "fork"
                } else {
                    "tree"
                };
                if self.persist_project_settings_patch(
                    "doubleEscapeAction",
                    json!({ "double_escape_action": next }),
                ) {
                    self.config.double_escape_action = Some(next.to_string());
                    self.status_message = Some(format!("Updated doubleEscapeAction: {next}"));
                }
            }
            SettingsUiEntry::EditorPaddingX => {
                let current = self.editor_padding_x.min(3);
                let next = match current {
                    0 => 1,
                    1 => 2,
                    2 => 3,
                    _ => 0,
                };
                if self.persist_project_settings_patch(
                    "editorPaddingX",
                    json!({ "editor_padding_x": next }),
                ) {
                    self.config.editor_padding_x = u32::try_from(next).ok();
                    self.editor_padding_x = next;
                    self.input
                        .set_width(self.term_width.saturating_sub(4 + self.editor_padding_x));
                    self.scroll_to_bottom();
                    self.status_message = Some(format!("Updated editorPaddingX: {next}"));
                }
            }
            SettingsUiEntry::AutocompleteMaxVisible => {
                let cycle = [3usize, 5, 8, 10, 12, 15, 20];
                let current = self.autocomplete.max_visible;
                let next = cycle
                    .iter()
                    .position(|value| *value == current)
                    .map_or(cycle[0], |idx| cycle[(idx + 1) % cycle.len()]);
                if self.persist_project_settings_patch(
                    "autocompleteMaxVisible",
                    json!({ "autocomplete_max_visible": next }),
                ) {
                    self.config.autocomplete_max_visible = u32::try_from(next).ok();
                    self.autocomplete.max_visible = next;
                    self.status_message = Some(format!("Updated autocompleteMaxVisible: {next}"));
                }
            }
            SettingsUiEntry::Theme => {
                self.settings_ui = None;
                self.theme_picker = Some(ThemePickerOverlay::new(&self.cwd));
            }
            SettingsUiEntry::Summary => {}
        }
    }

    fn format_input_history(&self) -> String {
        let entries = self.history.entries();
        if entries.is_empty() {
            return "No input history yet.".to_string();
        }

        let mut output = String::from("Input history (most recent first):\n");
        for (idx, entry) in entries.iter().rev().take(50).enumerate() {
            let trimmed = entry.value.trim();
            if trimmed.is_empty() {
                continue;
            }
            let preview = trimmed.replace('\n', "\\n");
            let preview = preview.chars().take(120).collect::<String>();
            let _ = writeln!(output, "  {}. {preview}", idx + 1);
        }
        output
    }

    fn format_session_info(&self, session: &Session) -> String {
        let file = session.path.as_ref().map_or_else(
            || "(not saved yet)".to_string(),
            |p| p.display().to_string(),
        );
        let name = session.get_name().unwrap_or_else(|| "-".to_string());
        let thinking = session
            .header
            .thinking_level
            .as_deref()
            .unwrap_or("off")
            .to_string();

        let message_count = session
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();

        let total_tokens = self.total_usage.total_tokens;
        let total_cost = self.total_usage.cost.total;
        let cost_str = if total_cost > 0.0 {
            format!("${total_cost:.4}")
        } else {
            "$0.0000".to_string()
        };

        format!(
            "Session info:\n  file: {file}\n  id: {id}\n  name: {name}\n  model: {model}\n  thinking: {thinking}\n  messageCount: {message_count}\n  tokens: {total_tokens}\n  cost: {cost_str}",
            id = session.header.id,
            model = self.model,
        )
    }

    fn format_settings_summary(&self) -> String {
        let theme_setting = self
            .config
            .theme
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        let theme_setting = if theme_setting.is_empty() {
            "(default)".to_string()
        } else {
            theme_setting
        };

        let compaction_enabled = self.config.compaction_enabled();
        let reserve_tokens = self.config.compaction_reserve_tokens();
        let keep_recent = self.config.compaction_keep_recent_tokens();
        let steering = self.config.steering_queue_mode();
        let follow_up = self.config.follow_up_queue_mode();
        let quiet_startup = self.config.quiet_startup.unwrap_or(false);
        let collapse_changelog = self.config.collapse_changelog.unwrap_or(false);
        let hide_thinking_block = self.config.hide_thinking_block.unwrap_or(false);
        let show_hardware_cursor = self.effective_show_hardware_cursor();
        let double_escape_action = self
            .config
            .double_escape_action
            .as_deref()
            .unwrap_or("tree");

        let mut output = String::new();
        let _ = writeln!(output, "Settings:");
        let _ = writeln!(
            output,
            "  theme: {} (config: {})",
            self.theme.name, theme_setting
        );
        let _ = writeln!(output, "  model: {}", self.model);
        let _ = writeln!(
            output,
            "  compaction: {compaction_enabled} (reserve={reserve_tokens}, keepRecent={keep_recent})"
        );
        let _ = writeln!(output, "  steeringMode: {}", steering.as_str());
        let _ = writeln!(output, "  followUpMode: {}", follow_up.as_str());
        let _ = writeln!(output, "  quietStartup: {}", bool_label(quiet_startup));
        let _ = writeln!(
            output,
            "  collapseChangelog: {}",
            bool_label(collapse_changelog)
        );
        let _ = writeln!(
            output,
            "  hideThinkingBlock: {}",
            bool_label(hide_thinking_block)
        );
        let _ = writeln!(
            output,
            "  showHardwareCursor: {}",
            bool_label(show_hardware_cursor)
        );
        let _ = writeln!(output, "  doubleEscapeAction: {double_escape_action}");
        let _ = writeln!(output, "  editorPaddingX: {}", self.editor_padding_x);
        let _ = writeln!(
            output,
            "  autocompleteMaxVisible: {}",
            self.autocomplete.max_visible
        );
        let _ = writeln!(
            output,
            "  skillCommands: {}",
            if self.config.enable_skill_commands() {
                "enabled"
            } else {
                "disabled"
            }
        );

        let _ = writeln!(output, "\nResources:");
        let _ = writeln!(output, "  skills: {}", self.resources.skills().len());
        let _ = writeln!(output, "  prompts: {}", self.resources.prompts().len());
        let _ = writeln!(output, "  themes: {}", self.resources.themes().len());

        let skill_diags = self.resources.skill_diagnostics().len();
        let prompt_diags = self.resources.prompt_diagnostics().len();
        let theme_diags = self.resources.theme_diagnostics().len();
        if skill_diags + prompt_diags + theme_diags > 0 {
            let _ = writeln!(output, "\nDiagnostics:");
            let _ = writeln!(output, "  skills: {skill_diags}");
            let _ = writeln!(output, "  prompts: {prompt_diags}");
            let _ = writeln!(output, "  themes: {theme_diags}");
        }

        output
    }

    fn default_export_path(&self, session: &Session) -> PathBuf {
        if let Some(path) = session.path.as_ref() {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session");
            return self.cwd.join(format!("pi-session-{stem}.html"));
        }
        let id = session.header.id.chars().take(8).collect::<String>();
        self.cwd.join(format!("pi-session-unsaved-{id}.html"))
    }

    fn resolve_output_path(&self, raw: &str) -> PathBuf {
        let raw = raw.trim();
        if raw.is_empty() {
            return self.cwd.join("pi-session.html");
        }
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    fn spawn_save_session(&self) {
        if !self.save_enabled {
            return;
        }

        let session = Arc::clone(&self.session);
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            let cx = Cx::for_request();

            let mut session_guard = match session.lock(&cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                    return;
                }
            };

            if let Err(err) = session_guard.save().await {
                let _ =
                    event_tx.try_send(PiMsg::AgentError(format!("Failed to save session: {err}")));
            }
        });
    }

    fn maybe_trigger_autocomplete(&mut self) {
        if self.agent_state != AgentState::Idle
            || self.session_picker.is_some()
            || self.settings_ui.is_some()
        {
            self.autocomplete.close();
            return;
        }

        let text = self.input.value();
        if text.trim().is_empty() {
            self.autocomplete.close();
            return;
        }

        // Autocomplete provider expects a byte offset cursor.
        let cursor = self.input.cursor_byte_offset();
        let response = self.autocomplete.provider.suggest(&text, cursor);
        // Path completion is Tab-triggered to avoid noisy dropdowns for URL-like tokens.
        if response
            .items
            .iter()
            .all(|item| item.kind == AutocompleteItemKind::Path)
        {
            self.autocomplete.close();
            return;
        }
        self.autocomplete.open_with(response);
    }

    fn trigger_autocomplete(&mut self) {
        self.maybe_trigger_autocomplete();
    }

    /// Compute the conversation viewport height based on the current input area height.
    /// Layout budget: header (2 rows) + input decoration (2 rows) + input lines + footer (2 rows).
    fn conversation_viewport_height(&self) -> usize {
        let chrome = 2 + 2 + 2; // header + input_decoration + footer
        self.term_height
            .saturating_sub(chrome + self.input.height())
    }

    /// Compute the effective conversation viewport height for the current
    /// render frame, accounting for conditional chrome (scroll indicator,
    /// tool status, status message) that reduce available space.
    ///
    /// Used in [`view()`] for conversation line slicing so the total output
    /// never exceeds `term_height` rows.  The stored
    /// `conversation_viewport.height` still drives scroll-position management.
    fn view_effective_conversation_height(&self) -> usize {
        // Fixed chrome: header(2) + footer(2).
        let mut chrome: usize = 2 + 2;

        // Budget 1 row for the scroll indicator.  Slightly conservative
        // when content is short, but prevents the off-by-one that triggers
        // terminal scrolling.
        chrome += 1;

        // Tool status: "\n  spinner Running {tool} ...\n" = 2 rows.
        if self.current_tool.is_some() {
            chrome += 2;
        }

        // Status message: "\n  {status}\n" = 2 rows.
        if self.status_message.is_some() {
            chrome += 2;
        }

        // Capability prompt overlay: ~8 lines (title, ext name, desc, blank, buttons, timer, help, blank).
        if self.capability_prompt.is_some() {
            chrome += 8;
        }

        // Branch picker overlay: header + N visible branches + help line + padding.
        if let Some(ref picker) = self.branch_picker {
            let visible = picker.branches.len().min(picker.max_visible);
            chrome += 3 + visible + 2; // title + header + separator + items + help + blank
        }

        // Input area vs processing spinner.
        let show_input = self.agent_state == AgentState::Idle
            && self.session_picker.is_none()
            && self.settings_ui.is_none()
            && self.theme_picker.is_none()
            && self.capability_prompt.is_none()
            && self.branch_picker.is_none()
            && self.model_selector.is_none();

        if show_input {
            // render_input: "\n  header\n" (2 rows) + input.height() rows.
            chrome += 2 + self.input.height();
        } else if self.agent_state != AgentState::Idle {
            // Processing spinner: "\n  spinner Processing...\n" = 2 rows.
            chrome += 2;
        }

        self.term_height.saturating_sub(chrome)
    }

    /// Set the input area height and recalculate the conversation viewport
    /// so the total layout fits the terminal.
    fn set_input_height(&mut self, h: usize) {
        self.input.set_height(h);
        self.resize_conversation_viewport();
    }

    /// Rebuild the conversation viewport after a height change (terminal resize or
    /// input area growth). Preserves mouse-wheel settings and scroll position.
    fn resize_conversation_viewport(&mut self) {
        let viewport_height = self.conversation_viewport_height();
        let mut viewport = Viewport::new(self.term_width.saturating_sub(2), viewport_height);
        viewport.mouse_wheel_enabled = true;
        viewport.mouse_wheel_delta = 3;
        self.conversation_viewport = viewport;
        self.scroll_to_bottom();
    }

    pub fn set_terminal_size(&mut self, width: usize, height: usize) {
        let test_mode = std::env::var_os("PI_TEST_MODE").is_some();
        let previous_height = self.term_height;
        self.term_width = width.max(1);
        self.term_height = height.max(1);
        self.input
            .set_width(self.term_width.saturating_sub(4 + self.editor_padding_x));

        if !test_mode
            && self.term_height < previous_height
            && self.config.terminal_clear_on_shrink()
        {
            let _ = crossterm::execute!(
                std::io::stdout(),
                terminal::Clear(terminal::ClearType::Purge)
            );
        }

        self.resize_conversation_viewport();
    }

    fn accept_autocomplete(&mut self, item: &AutocompleteItem) {
        let text = self.input.value();
        let range = self.autocomplete.replace_range.clone();

        let mut new_text = String::with_capacity(text.len().saturating_add(item.insert.len()));
        new_text.push_str(&text[..range.start]);
        new_text.push_str(&item.insert);
        new_text.push_str(&text[range.end..]);

        self.input.set_value(&new_text);
        self.input.cursor_end();
    }

    fn extract_file_references(&mut self, message: &str) -> (String, Vec<String>) {
        let mut cleaned = String::with_capacity(message.len());
        let mut file_args = Vec::new();
        let mut idx = 0usize;

        while idx < message.len() {
            let ch = message[idx..].chars().next().unwrap_or(' ');
            if ch == '@' && is_file_ref_boundary(message, idx) {
                let token_start = idx + ch.len_utf8();
                let parsed = parse_quoted_file_ref(message, token_start);
                let (path, trailing, token_end) = parsed.unwrap_or_else(|| {
                    let (token, token_end) = next_non_whitespace_token(message, token_start);
                    let (path, trailing) = split_trailing_punct(token);
                    (path.to_string(), trailing.to_string(), token_end)
                });

                if !path.is_empty() {
                    let resolved =
                        self.autocomplete
                            .provider
                            .resolve_file_ref(&path)
                            .or_else(|| {
                                let resolved_path = resolve_read_path(&path, &self.cwd);
                                resolved_path.exists().then(|| path.clone())
                            });

                    if let Some(resolved) = resolved {
                        file_args.push(resolved);
                        if !trailing.is_empty()
                            && cleaned.chars().last().is_some_and(char::is_whitespace)
                        {
                            cleaned.pop();
                        }
                        cleaned.push_str(&trailing);
                        idx = token_end;
                        continue;
                    }
                }
            }

            cleaned.push(ch);
            idx += ch.len_utf8();
        }

        (cleaned, file_args)
    }

    fn handle_paste_event(&mut self, key: &KeyMsg) -> bool {
        if key.key_type != KeyType::Runes || key.runes.is_empty() {
            return false;
        }

        let pasted: String = key.runes.iter().collect();
        let Some((insert, count)) = self.normalize_pasted_paths(&pasted) else {
            return false;
        };

        self.input.insert_string(&insert);
        if count > 0 {
            self.status_message = Some(format!(
                "Attached {} file{}",
                count,
                if count == 1 { "" } else { "s" }
            ));
        }
        true
    }

    fn normalize_pasted_paths(&self, pasted: &str) -> Option<(String, usize)> {
        let mut refs = Vec::new();
        for line in pasted.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let path = self.normalize_pasted_path(trimmed)?;
            refs.push(path);
        }

        if refs.is_empty() {
            return None;
        }

        let mut insert = refs
            .iter()
            .map(|path| format_file_ref(path))
            .collect::<Vec<_>>()
            .join(" ");
        if !insert.ends_with(' ') {
            insert.push(' ');
        }

        Some((insert, refs.len()))
    }

    fn normalize_pasted_path(&self, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('@') {
            return None;
        }

        let unquoted = strip_wrapping_quotes(trimmed);
        let unescaped = unescape_dragged_path(unquoted);
        let path = file_url_to_path(&unescaped).unwrap_or_else(|| PathBuf::from(&unescaped));
        let resolved = resolve_read_path(path.to_string_lossy().as_ref(), &self.cwd);
        if !resolved.exists() {
            return None;
        }

        Some(path_for_display(&resolved, &self.cwd))
    }

    fn insert_file_ref_path(&mut self, path: &Path) {
        let display = path_for_display(path, &self.cwd);
        let mut insert_text = format_file_ref(&display);
        if !insert_text.ends_with(' ') {
            insert_text.push(' ');
        }
        self.input.insert_string(&insert_text);
    }

    fn paste_image_from_clipboard() -> Option<PathBuf> {
        #[cfg(all(feature = "clipboard", feature = "image-resize"))]
        {
            use image::ImageEncoder;

            let mut clipboard = ArboardClipboard::new().ok()?;
            let image = clipboard.get_image().ok()?;

            let width = u32::try_from(image.width).ok()?;
            let height = u32::try_from(image.height).ok()?;
            let bytes = image.bytes.into_owned();
            let width_usize = usize::try_from(width).ok()?;
            let height_usize = usize::try_from(height).ok()?;
            let expected = width_usize.checked_mul(height_usize)?.checked_mul(4)?;
            if bytes.len() != expected {
                return None;
            }

            let mut temp_file = tempfile::Builder::new()
                .prefix("pi-paste-")
                .suffix(".png")
                .tempfile()
                .ok()?;
            let encoder = image::codecs::png::PngEncoder::new(&mut temp_file);
            if encoder
                .write_image(&bytes, width, height, image::ExtendedColorType::Rgba8)
                .is_err()
            {
                return None;
            }
            let (_file, path) = temp_file.keep().ok()?;
            Some(path)
        }

        #[cfg(not(all(feature = "clipboard", feature = "image-resize")))]
        {
            None
        }
    }

    #[allow(clippy::too_many_lines)]
    fn load_session_from_path(&mut self, path: &str) -> Option<Cmd> {
        let path = path.to_string();
        let session = Arc::clone(&self.session);
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();

        let (session_dir, previous_session_file) = {
            let Ok(guard) = self.session.try_lock() else {
                self.status_message = Some("Session busy; try again".to_string());
                return None;
            };
            (
                guard.session_dir.clone(),
                guard.path.as_ref().map(|p| p.display().to_string()),
            )
        };

        runtime_handle.spawn(async move {
            let cx = Cx::for_request();

            if let Some(manager) = extensions.clone() {
                let cancelled = manager
                    .dispatch_cancellable_event(
                        ExtensionEventName::SessionBeforeSwitch,
                        Some(json!({
                            "reason": "resume",
                            "targetSessionFile": path.clone(),
                        })),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(false);
                if cancelled {
                    let _ = event_tx.try_send(PiMsg::System(
                        "Session switch cancelled by extension".to_string(),
                    ));
                    return;
                }
            }

            let mut loaded_session = match Session::open(&path).await {
                Ok(session) => session,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to open session: {err}")));
                    return;
                }
            };
            let new_session_id = loaded_session.header.id.clone();
            loaded_session.session_dir = session_dir;

            let messages_for_agent = loaded_session.to_messages_for_current_path();

            // Replace the session.
            {
                let mut session_guard = match session.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                        return;
                    }
                };
                *session_guard = loaded_session;
            }

            // Update the agent messages.
            {
                let mut agent_guard = match agent.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock agent: {err}")));
                        return;
                    }
                };
                agent_guard.replace_messages(messages_for_agent);
            }

            let (messages, usage) = {
                let session_guard = match session.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                        return;
                    }
                };
                conversation_from_session(&session_guard)
            };

            let _ = event_tx.try_send(PiMsg::ConversationReset {
                messages,
                usage,
                status: Some("Session resumed".to_string()),
            });

            if let Some(manager) = extensions {
                let _ = manager
                    .dispatch_event(
                        ExtensionEventName::SessionSwitch,
                        Some(json!({
                            "reason": "resume",
                            "previousSessionFile": previous_session_file,
                            "targetSessionFile": path,
                            "sessionId": new_session_id,
                        })),
                    )
                    .await;
            }
        });

        self.status_message = Some("Loading session...".to_string());
        None
    }

    fn render_header(&self) -> String {
        let model_label = format!("({})", self.model);

        // Branch indicator: show "Branch N/M" when session has multiple leaves.
        let branch_indicator = self
            .session
            .try_lock()
            .ok()
            .and_then(|guard| {
                let info = guard.branch_summary();
                if info.leaf_count <= 1 {
                    return None;
                }
                let current_idx = info
                    .current_leaf
                    .as_ref()
                    .and_then(|leaf| info.leaves.iter().position(|l| l == leaf))
                    .map_or(1, |i| i + 1);
                Some(format!(" [branch {current_idx}/{}]", info.leaf_count))
            })
            .unwrap_or_default();

        let model_key = self.header_binding_hint(AppAction::SelectModel, "ctrl+l");
        let next_model_key = self.header_binding_hint(AppAction::CycleModelForward, "ctrl+p");
        let prev_model_key =
            self.header_binding_hint(AppAction::CycleModelBackward, "ctrl+shift+p");
        let tools_key = self.header_binding_hint(AppAction::ExpandTools, "ctrl+o");
        let thinking_key = self.header_binding_hint(AppAction::ToggleThinking, "ctrl+t");
        let max_width = self.term_width.saturating_sub(2);

        let hints_line = truncate(
            &format!(
                "{model_key}: model  {next_model_key}: next  {prev_model_key}: prev  \
                 {tools_key}: tools  {thinking_key}: thinking"
            ),
            max_width,
        );

        let resources_line = truncate(
            &format!(
                "resources: {} skills, {} prompts, {} themes, {} extensions",
                self.resources.skills().len(),
                self.resources.prompts().len(),
                self.resources.themes().len(),
                self.resources.extensions().len()
            ),
            max_width,
        );

        format!(
            "  {} {}{}\n  {}\n  {}\n",
            self.styles.title.render("Pi"),
            self.styles.muted.render(&model_label),
            self.styles.accent.render(&branch_indicator),
            self.styles.muted.render(&hints_line),
            self.styles.muted.render(&resources_line),
        )
    }

    fn header_binding_hint(&self, action: AppAction, fallback: &str) -> String {
        self.keybindings
            .get_bindings(action)
            .first()
            .map_or_else(|| fallback.to_string(), std::string::ToString::to_string)
    }

    fn render_input(&self) -> String {
        let mut output = String::new();

        let thinking_level = self
            .session
            .try_lock()
            .ok()
            .and_then(|guard| guard.header.thinking_level.clone())
            .and_then(|level| level.parse::<ThinkingLevel>().ok())
            .or_else(|| {
                self.config
                    .default_thinking_level
                    .as_deref()
                    .and_then(|level| level.parse::<ThinkingLevel>().ok())
            })
            .unwrap_or(ThinkingLevel::Off);

        let input_text = self.input.value();
        let is_bash_mode = parse_bash_command(&input_text).is_some();

        let (thinking_label, thinking_style, thinking_border_style) = match thinking_level {
            ThinkingLevel::Off => (
                "off",
                self.styles.muted_bold.clone(),
                self.styles.border.clone(),
            ),
            ThinkingLevel::Minimal => (
                "minimal",
                self.styles.accent.clone(),
                self.styles.accent.clone(),
            ),
            ThinkingLevel::Low => (
                "low",
                self.styles.accent.clone(),
                self.styles.accent.clone(),
            ),
            ThinkingLevel::Medium => (
                "medium",
                self.styles.accent_bold.clone(),
                self.styles.accent.clone(),
            ),
            ThinkingLevel::High => (
                "high",
                self.styles.warning_bold.clone(),
                self.styles.warning.clone(),
            ),
            ThinkingLevel::XHigh => (
                "xhigh",
                self.styles.error_bold.clone(),
                self.styles.error_bold.clone(),
            ),
        };

        let thinking_plain = format!("[thinking: {thinking_label}]");
        let thinking_badge = thinking_style.render(&thinking_plain);
        let bash_badge = is_bash_mode.then(|| self.styles.warning_bold.render("[bash]"));

        let max_width = self.term_width.saturating_sub(2);
        let reserved = 2
            + thinking_plain.chars().count()
            + if is_bash_mode {
                2 + "[bash]".chars().count()
            } else {
                0
            };
        let available_for_mode = max_width.saturating_sub(reserved);
        let mut mode_text = match self.input_mode {
            InputMode::SingleLine => "Enter: send  Shift+Enter: newline  Alt+Enter: multi-line",
            InputMode::MultiLine => "Alt+Enter: send  Enter: newline  Esc: single-line",
        }
        .to_string();
        if mode_text.chars().count() > available_for_mode {
            mode_text = truncate(&mode_text, available_for_mode);
        }
        let mut header_line = String::new();
        header_line.push_str(&self.styles.muted.render(&mode_text));
        header_line.push_str("  ");
        header_line.push_str(&thinking_badge);
        if let Some(bash_badge) = bash_badge {
            header_line.push_str("  ");
            header_line.push_str(&bash_badge);
        }
        let _ = writeln!(output, "\n  {header_line}");

        let padding = " ".repeat(self.editor_padding_x);
        let line_prefix = format!("  {padding}");
        let border_style = if is_bash_mode {
            self.styles.warning_bold.clone()
        } else {
            thinking_border_style
        };
        let border = border_style.render("│");
        for line in self.input.view().lines() {
            output.push_str(&line_prefix);
            output.push_str(&border);
            output.push(' ');
            output.push_str(line);
            output.push('\n');
        }

        output
    }

    fn render_footer(&self) -> String {
        let total_cost = self.total_usage.cost.total;
        let cost_str = if total_cost > 0.0 {
            format!(" (${total_cost:.4})")
        } else {
            String::new()
        };

        let input = self.total_usage.input;
        let output_tokens = self.total_usage.output;
        let mode_hint = match self.input_mode {
            InputMode::SingleLine => "Shift+Enter: newline  |  Alt+Enter: multi-line",
            InputMode::MultiLine => "Enter: newline  |  Alt+Enter: send  |  Esc: single-line",
        };
        let footer_long = format!(
            "Tokens: {input} in / {output_tokens} out{cost_str}  |  {mode_hint}  |  /help  |  Ctrl+C: quit"
        );
        let footer_short = format!(
            "Tokens: {input} in / {output_tokens} out{cost_str}  |  /help  |  Ctrl+C: quit"
        );
        let max_width = self.term_width.saturating_sub(2);
        let mut footer = if footer_long.chars().count() <= max_width {
            footer_long
        } else {
            footer_short
        };
        if footer.chars().count() > max_width {
            footer = truncate(&footer, max_width);
        }
        format!("\n  {}\n", self.styles.muted.render(&footer))
    }

    fn render_pending_message_queue(&self) -> Option<String> {
        if self.agent_state == AgentState::Idle {
            return None;
        }

        let Ok(queue) = self.message_queue.lock() else {
            return None;
        };

        let steering_len = queue.steering.len();
        let follow_len = queue.follow_up.len();
        if steering_len == 0 && follow_len == 0 {
            return None;
        }

        let max_preview = self.term_width.saturating_sub(24).max(20);

        let mut out = String::new();
        out.push_str("\n  ");
        out.push_str(&self.styles.muted_bold.render("Pending:"));
        out.push(' ');
        out.push_str(
            &self
                .styles
                .accent_bold
                .render(&format!("{steering_len} steering")),
        );
        out.push_str(&self.styles.muted.render(", "));
        out.push_str(&self.styles.muted.render(&format!("{follow_len} follow-up")));
        out.push('\n');

        if let Some(text) = queue.steering.front() {
            let preview = queued_message_preview(text, max_preview);
            out.push_str("  ");
            out.push_str(&self.styles.accent_bold.render("steering →"));
            out.push(' ');
            out.push_str(&preview);
            out.push('\n');
        }

        if let Some(text) = queue.follow_up.front() {
            let preview = queued_message_preview(text, max_preview);
            out.push_str("  ");
            out.push_str(&self.styles.muted_bold.render("follow-up →"));
            out.push(' ');
            out.push_str(&self.styles.muted.render(&preview));
            out.push('\n');
        }

        Some(out)
    }

    #[allow(clippy::too_many_lines)]
    fn render_autocomplete_dropdown(&self) -> String {
        let mut output = String::new();

        let offset = self.autocomplete.scroll_offset();
        let visible_count = self
            .autocomplete
            .max_visible
            .min(self.autocomplete.items.len());
        let end = (offset + visible_count).min(self.autocomplete.items.len());

        // Styles
        let border_style = &self.styles.border;
        let selected_style = &self.styles.selection;
        let kind_style = &self.styles.warning;
        let desc_style = &self.styles.muted_italic;

        // Top border
        let width = 60;
        let _ = write!(
            output,
            "\n  {}",
            border_style.render(&format!("┌{:─<width$}┐", ""))
        );

        for (idx, item) in self.autocomplete.items[offset..end].iter().enumerate() {
            let global_idx = offset + idx;
            let is_selected = global_idx == self.autocomplete.selected;

            let kind_icon = match item.kind {
                AutocompleteItemKind::SlashCommand => "⚡",
                AutocompleteItemKind::ExtensionCommand => "🧩",
                AutocompleteItemKind::PromptTemplate => "📄",
                AutocompleteItemKind::Skill => "🔧",
                AutocompleteItemKind::File => "📁",
                AutocompleteItemKind::Path => "📂",
            };

            let max_label_len = width.saturating_sub(6);
            let label = if item.label.chars().count() > max_label_len {
                let mut out = item
                    .label
                    .chars()
                    .take(max_label_len.saturating_sub(1))
                    .collect::<String>();
                out.push('…');
                out
            } else {
                item.label.clone()
            };

            let line_content = format!("{kind_icon} {label:<max_label_len$}");
            let styled_line = if is_selected {
                selected_style.render(&line_content)
            } else {
                format!("{} {label:<max_label_len$}", kind_style.render(kind_icon))
            };

            let _ = write!(
                output,
                "\n  {}{}{}",
                border_style.render("│"),
                styled_line,
                border_style.render("│")
            );

            if is_selected {
                if let Some(desc) = &item.description {
                    let truncated_desc = if desc.chars().count() > width.saturating_sub(4) {
                        let mut out = desc
                            .chars()
                            .take(width.saturating_sub(5))
                            .collect::<String>();
                        out.push('…');
                        out
                    } else {
                        desc.clone()
                    };

                    let _ = write!(
                        output,
                        "\n  {}  {}{}",
                        border_style.render("│"),
                        desc_style.render(&truncated_desc),
                        border_style.render(&format!(
                            "{:>pad$}│",
                            "",
                            pad = width.saturating_sub(2).saturating_sub(truncated_desc.len())
                        ))
                    );
                }
            }
        }

        if self.autocomplete.items.len() > visible_count {
            let shown = format!(
                "{}-{} of {}",
                offset + 1,
                end,
                self.autocomplete.items.len()
            );
            let _ = write!(
                output,
                "\n  {}",
                border_style.render(&format!("│{shown:^width$}│"))
            );
        }

        let _ = write!(
            output,
            "\n  {}",
            border_style.render(&format!("└{:─<width$}┘", ""))
        );

        let _ = write!(
            output,
            "\n  {}",
            self.styles
                .muted_italic
                .render("↑/↓ navigate  Enter/Tab accept  Esc cancel")
        );

        output
    }

    fn render_session_picker(&self, picker: &SessionPickerOverlay) -> String {
        let mut output = String::new();

        let _ = writeln!(
            output,
            "\n  {}\n",
            self.styles.title.render("Select a session to resume")
        );

        if picker.sessions.is_empty() {
            let _ = writeln!(
                output,
                "  {}",
                self.styles
                    .muted
                    .render("No sessions found for this project.")
            );
        } else {
            let _ = writeln!(
                output,
                "  {:<20}  {:<30}  {:<8}  {}",
                self.styles.muted_bold.render("Time"),
                self.styles.muted_bold.render("Name"),
                self.styles.muted_bold.render("Messages"),
                self.styles.muted_bold.render("Session ID")
            );
            output.push_str("  ");
            output.push_str(&"-".repeat(78));
            output.push('\n');

            let offset = picker.scroll_offset();
            let visible_count = picker.max_visible.min(picker.sessions.len());
            let end = (offset + visible_count).min(picker.sessions.len());

            for (idx, session) in picker.sessions[offset..end].iter().enumerate() {
                let global_idx = offset + idx;
                let is_selected = global_idx == picker.selected;

                let prefix = if is_selected { ">" } else { " " };
                let time = crate::session_picker::format_time(&session.timestamp);
                let name = session
                    .name
                    .as_deref()
                    .unwrap_or("-")
                    .chars()
                    .take(28)
                    .collect::<String>();
                let messages = session.message_count.to_string();
                let id = crate::session_picker::truncate_session_id(&session.id, 8);

                let row = format!(" {time:<20}  {name:<30}  {messages:<8}  {id}");
                let rendered = if is_selected {
                    self.styles.selection.render(&row)
                } else {
                    row
                };

                let _ = writeln!(output, "{prefix} {rendered}");
            }

            if picker.sessions.len() > visible_count {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}-{} of {})",
                        offset + 1,
                        end,
                        picker.sessions.len()
                    ))
                );
            }
        }

        output.push('\n');
        if picker.confirm_delete {
            let _ = writeln!(
                output,
                "  {}",
                self.styles.warning_bold.render(
                    picker
                        .status_message
                        .as_deref()
                        .unwrap_or("Delete session? Press y/n to confirm."),
                )
            );
        } else {
            let _ = writeln!(
                output,
                "  {}",
                self.styles
                    .muted_italic
                    .render("↑/↓/j/k: navigate  Enter: select  Ctrl+D: delete  Esc/q: cancel")
            );
            if let Some(message) = &picker.status_message {
                let _ = writeln!(output, "  {}", self.styles.warning_bold.render(message));
            }
        }

        output
    }

    fn render_settings_ui(&self, settings_ui: &SettingsUiState) -> String {
        let mut output = String::new();

        let _ = writeln!(output, "\n  {}\n", self.styles.title.render("Settings"));

        if settings_ui.entries.is_empty() {
            let _ = writeln!(
                output,
                "  {}",
                self.styles.muted.render("No settings available.")
            );
        } else {
            let offset = settings_ui.scroll_offset();
            let visible_count = settings_ui.max_visible.min(settings_ui.entries.len());
            let end = (offset + visible_count).min(settings_ui.entries.len());

            for (idx, entry) in settings_ui.entries[offset..end].iter().enumerate() {
                let global_idx = offset + idx;
                let is_selected = global_idx == settings_ui.selected;

                let prefix = if is_selected { ">" } else { " " };
                let label = match *entry {
                    SettingsUiEntry::Summary => "Summary".to_string(),
                    SettingsUiEntry::Theme => "Theme".to_string(),
                    SettingsUiEntry::SteeringMode => format!(
                        "steeringMode: {}",
                        self.config.steering_queue_mode().as_str()
                    ),
                    SettingsUiEntry::FollowUpMode => format!(
                        "followUpMode: {}",
                        self.config.follow_up_queue_mode().as_str()
                    ),
                    SettingsUiEntry::QuietStartup => format!(
                        "quietStartup: {}",
                        bool_label(self.config.quiet_startup.unwrap_or(false))
                    ),
                    SettingsUiEntry::CollapseChangelog => format!(
                        "collapseChangelog: {}",
                        bool_label(self.config.collapse_changelog.unwrap_or(false))
                    ),
                    SettingsUiEntry::HideThinkingBlock => format!(
                        "hideThinkingBlock: {}",
                        bool_label(self.config.hide_thinking_block.unwrap_or(false))
                    ),
                    SettingsUiEntry::ShowHardwareCursor => format!(
                        "showHardwareCursor: {}",
                        bool_label(self.effective_show_hardware_cursor())
                    ),
                    SettingsUiEntry::DoubleEscapeAction => format!(
                        "doubleEscapeAction: {}",
                        self.config
                            .double_escape_action
                            .as_deref()
                            .unwrap_or("tree")
                    ),
                    SettingsUiEntry::EditorPaddingX => {
                        format!("editorPaddingX: {}", self.editor_padding_x)
                    }
                    SettingsUiEntry::AutocompleteMaxVisible => {
                        format!("autocompleteMaxVisible: {}", self.autocomplete.max_visible)
                    }
                };
                let row = format!(" {label}");
                let rendered = if is_selected {
                    self.styles.selection.render(&row)
                } else {
                    row
                };

                let _ = writeln!(output, "{prefix} {rendered}");
            }

            if settings_ui.entries.len() > visible_count {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}-{} of {})",
                        offset + 1,
                        end,
                        settings_ui.entries.len()
                    ))
                );
            }
        }

        output.push('\n');
        let _ = writeln!(
            output,
            "  {}",
            self.styles
                .muted_italic
                .render("↑/↓/j/k: navigate  Enter: select  Esc/q: cancel")
        );

        output
    }

    fn render_theme_picker(&self, picker: &ThemePickerOverlay) -> String {
        let mut output = String::new();

        let _ = writeln!(output, "\n  {}\n", self.styles.title.render("Select Theme"));

        if picker.items.is_empty() {
            let _ = writeln!(output, "  {}", self.styles.muted.render("No themes found."));
        } else {
            let offset = picker.scroll_offset();
            let visible_count = picker.max_visible.min(picker.items.len());
            let end = (offset + visible_count).min(picker.items.len());

            for (idx, item) in picker.items[offset..end].iter().enumerate() {
                let global_idx = offset + idx;
                let is_selected = global_idx == picker.selected;

                let prefix = if is_selected { ">" } else { " " };
                let (name, label) = match item {
                    ThemePickerItem::BuiltIn(name) => {
                        (name.to_string(), format!("{name} (built-in)"))
                    }
                    ThemePickerItem::File(path) => {
                        // Load theme to get name, or fallback to file stem.
                        // Performance note: repetitive load, but themes are small JSON files.
                        let name = Theme::load(path).map_or_else(
                            |_| {
                                path.file_stem().map_or_else(
                                    || "unknown".to_string(),
                                    |s| s.to_string_lossy().to_string(),
                                )
                            },
                            |t| t.name,
                        );
                        (name.clone(), format!("{name} (custom)"))
                    }
                };

                let active = name.eq_ignore_ascii_case(&self.theme.name);
                let marker = if active { " *" } else { "" };

                let row = format!(" {label}{marker}");
                let rendered = if is_selected {
                    self.styles.selection.render(&row)
                } else {
                    row
                };

                let _ = writeln!(output, "{prefix} {rendered}");
            }

            if picker.items.len() > visible_count {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}-{} of {})",
                        offset + 1,
                        end,
                        picker.items.len()
                    ))
                );
            }
        }

        output.push('\n');
        let _ = writeln!(
            output,
            "  {}",
            self.styles
                .muted_italic
                .render("↑/↓/j/k: navigate  Enter: select  Esc/q: back")
        );

        output
    }

    fn render_capability_prompt(&self, prompt: &CapabilityPromptOverlay) -> String {
        let mut output = String::new();

        // Title line.
        let _ = writeln!(
            output,
            "\n  {}",
            self.styles.title.render("Extension Permission Request")
        );

        // Extension and capability info.
        let _ = writeln!(
            output,
            "  {} requests {}",
            self.styles.accent_bold.render(&prompt.extension_id),
            self.styles.warning_bold.render(&prompt.capability),
        );

        // Description.
        if !prompt.description.is_empty() {
            let _ = writeln!(
                output,
                "\n  {}",
                self.styles.muted.render(&prompt.description),
            );
        }

        // Button row.
        output.push('\n');
        output.push_str("  ");
        for (idx, action) in CapabilityAction::ALL.iter().enumerate() {
            let label = action.label();
            let rendered = if idx == prompt.focused {
                self.styles.selection.render(&format!("[{label}]"))
            } else {
                self.styles.muted.render(&format!(" {label} "))
            };
            output.push_str(&rendered);
            output.push_str("  ");
        }
        output.push('\n');

        // Auto-deny timer.
        if let Some(secs) = prompt.auto_deny_secs {
            let _ = writeln!(
                output,
                "  {}",
                self.styles
                    .muted_italic
                    .render(&format!("Auto-deny in {secs}s")),
            );
        }

        // Help text.
        let _ = writeln!(
            output,
            "  {}",
            self.styles
                .muted_italic
                .render("←/→/Tab: navigate  Enter: confirm  Esc: deny"),
        );

        output
    }
}

const fn bool_label(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn run_command_output(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    abort_signal: &crate::agent::AbortSignal,
) -> std::io::Result<std::process::Output> {
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();

    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    let tick = Duration::from_millis(10);
    loop {
        if abort_signal.is_aborted() {
            crate::tools::kill_process_tree(Some(pid));
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "command aborted",
            ));
        }

        match rx.recv_timeout(tick) {
            Ok(result) => return result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("command output channel disconnected"));
            }
        }
    }
}

fn parse_gist_url_and_id(output: &str) -> Option<(String, String)> {
    for raw in output.split_whitespace() {
        let candidate_url = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';'));
        let Ok(url) = Url::parse(candidate_url) else {
            continue;
        };
        let Some(host) = url.host_str() else {
            continue;
        };
        if host != "gist.github.com" {
            continue;
        }
        let Some(gist_id) = url.path_segments().and_then(|mut seg| seg.next_back()) else {
            continue;
        };
        if gist_id.is_empty() {
            continue;
        }
        return Some((candidate_url.to_string(), gist_id.to_string()));
    }
    None
}

fn format_command_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(no output)".to_string(),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
    }
}

use crate::config::parse_queue_mode_or_default;

fn parse_extension_command(input: &str) -> Option<(String, Vec<String>)> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }

    // Built-in slash commands are handled elsewhere.
    if SlashCommand::parse(input).is_some() {
        return None;
    }

    let (cmd, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let cmd = cmd.trim_start_matches('/').trim();
    if cmd.is_empty() {
        return None;
    }
    let args = rest
        .split_whitespace()
        .map(std::string::ToString::to_string)
        .collect();
    Some((cmd.to_string(), args))
}

fn extension_commands_for_catalog(
    manager: &ExtensionManager,
) -> Vec<crate::autocomplete::NamedEntry> {
    manager
        .list_commands()
        .into_iter()
        .filter_map(|cmd| {
            let name = cmd.get("name")?.as_str()?.to_string();
            let description = cmd
                .get("description")
                .and_then(|d| d.as_str())
                .map(std::string::ToString::to_string);
            Some(crate::autocomplete::NamedEntry { name, description })
        })
        .collect()
}

fn parse_bash_command(input: &str) -> Option<(String, bool)> {
    let trimmed = input.trim_start();
    if trimmed.starts_with("!!") {
        let command = trimmed.trim_start_matches("!!").trim();
        if command.is_empty() {
            None
        } else {
            Some((command.to_string(), true))
        }
    } else if trimmed.starts_with('!') {
        let command = trimmed.trim_start_matches('!').trim();
        if command.is_empty() {
            None
        } else {
            Some((command.to_string(), false))
        }
    } else {
        None
    }
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

fn assistant_content_to_text(content: &[ContentBlock]) -> (String, Option<String>) {
    let mut text = String::new();
    let mut thinking = String::new();

    for block in content {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Thinking(t) => thinking.push_str(&t.thinking),
            _ => {}
        }
    }

    let thinking = if thinking.trim().is_empty() {
        None
    } else {
        Some(thinking)
    };

    (text, thinking)
}

fn build_user_message(text: String) -> ModelMessage {
    ModelMessage::User(UserMessage {
        content: UserContent::Text(text),
        timestamp: Utc::now().timestamp_millis(),
    })
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                let rendered =
                    crate::terminal_images::render_inline(&image.data, &image.mime_type, 72);
                push_line(&mut output, &rendered);
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
        }
    }
    output
}

fn split_content_blocks_for_input(blocks: &[ContentBlock]) -> (String, Vec<ImageContent>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut text, &text_block.text),
            ContentBlock::Image(image) => images.push(image.clone()),
            _ => {}
        }
    }
    (text, images)
}

fn build_content_blocks_for_input(text: &str, images: &[ImageContent]) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(ContentBlock::Text(TextContent::new(text.to_string())));
    }
    for image in images {
        content.push(ContentBlock::Image(image.clone()));
    }
    content
}

async fn dispatch_input_event(
    manager: &ExtensionManager,
    text: String,
    images: Vec<ImageContent>,
) -> crate::error::Result<InputEventOutcome> {
    let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
    let payload = json!({
        "text": text,
        "images": images_value,
        "source": "user",
    });
    let response = manager
        .dispatch_event_with_response(
            ExtensionEventName::Input,
            Some(payload),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await?;
    Ok(apply_input_event_response(response, text, images))
}

fn next_non_whitespace_token(text: &str, start: usize) -> (&str, usize) {
    if start >= text.len() {
        return ("", text.len());
    }
    let mut end = text.len();
    for (offset, ch) in text[start..].char_indices() {
        if ch.is_whitespace() {
            end = start + offset;
            break;
        }
    }
    (&text[start..end], end)
}

fn parse_quoted_file_ref(text: &str, start: usize) -> Option<(String, String, usize)> {
    let mut chars = text[start..].chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut path = String::new();
    let mut escaped = false;
    let mut end = None;
    let after_quote = start + quote.len_utf8();

    for (offset, ch) in text[after_quote..].char_indices() {
        if escaped {
            path.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            end = Some(after_quote + offset);
            break;
        }
        path.push(ch);
    }

    let end = end?;
    let mut trailing = String::new();
    let mut token_end = end + quote.len_utf8();
    for ch in text[token_end..].chars() {
        if is_trailing_punct(ch) {
            trailing.push(ch);
            token_end += ch.len_utf8();
        } else {
            break;
        }
    }

    Some((path, trailing, token_end))
}

fn strip_wrapping_quotes(input: &str) -> &str {
    let bytes = input.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..bytes.len() - 1];
        }
    }
    input
}

fn looks_like_windows_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    (bytes.len() >= 2 && bytes[1] == b':') || input.starts_with("\\\\")
}

fn unescape_dragged_path(input: &str) -> String {
    if looks_like_windows_path(input) {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.peek().copied() {
                if matches!(
                    next,
                    ' ' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '\\'
                ) {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

fn file_url_to_path(input: &str) -> Option<PathBuf> {
    if !input.starts_with("file://") {
        return None;
    }
    Url::parse(input).ok()?.to_file_path().ok()
}

fn path_for_display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.to_string_lossy().to_string(),
        |p| p.to_string_lossy().to_string(),
    )
}

fn format_file_ref(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        if !path.contains('"') {
            format!("@\"{path}\"")
        } else if !path.contains('\'') {
            format!("@'{path}'")
        } else {
            format!("@\"{}\"", path.replace('"', "\\\""))
        }
    } else {
        format!("@{path}")
    }
}

fn split_trailing_punct(token: &str) -> (&str, &str) {
    let mut split = token.len();
    for (idx, ch) in token.char_indices().rev() {
        if is_trailing_punct(ch) {
            split = idx;
        } else {
            break;
        }
    }
    token.split_at(split)
}

const fn is_trailing_punct(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
    )
}

fn is_file_ref_boundary(text: &str, at: usize) -> bool {
    if at == 0 {
        return true;
    }
    let prev = text[..at].chars().last().unwrap_or(' ');
    prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | '<' | '"' | '\'')
}

fn format_tool_output(
    content: &[ContentBlock],
    details: Option<&Value>,
    show_images: bool,
) -> Option<String> {
    let mut output = tool_content_blocks_to_text(content, show_images);
    if let Some(details) = details {
        // `edit` includes a unified diff-like view in `details.diff`. Surface it in the TUI
        // even when the primary content is a short "success" message.
        if let Some(diff) = details.get("diff").and_then(Value::as_str) {
            let diff = diff.trim();
            if !diff.is_empty() {
                if !output.trim().is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str("Diff:\n");
                output.push_str(diff);
            }
        } else if output.trim().is_empty() {
            output = pretty_json(details);
        }
    } else if output.trim().is_empty() {
        // No primary content and no details payload.
    }
    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

/// Maximum number of diff lines to show before truncating.
const DIFF_TRUNCATE_THRESHOLD: usize = 50;
/// Lines to show at the beginning of a truncated diff.
const DIFF_TRUNCATE_HEAD: usize = 20;
/// Lines to show at the end of a truncated diff.
const DIFF_TRUNCATE_TAIL: usize = 10;

fn render_tool_message(text: &str, styles: &TuiStyles) -> String {
    let mut out = String::new();
    let mut diff_lines: Vec<&str> = Vec::new();

    // First pass: separate pre-diff text from diff lines.
    let mut pre_diff_lines: Vec<&str> = Vec::new();
    let mut found_diff_header = false;
    for line in text.lines() {
        if found_diff_header {
            diff_lines.push(line);
        } else if line.trim() == "Diff:" {
            found_diff_header = true;
        } else {
            pre_diff_lines.push(line);
        }
    }

    // Render pre-diff content (tool name, success message, etc.)
    for (idx, line) in pre_diff_lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(&styles.muted.render(line));
    }

    if !found_diff_header {
        return out;
    }

    // Extract file path from "Successfully replaced text in {path}." pattern.
    let file_path = pre_diff_lines.iter().find_map(|line| {
        line.strip_prefix("Successfully replaced text in ")
            .and_then(|rest| rest.strip_suffix('.'))
    });

    // Render diff header.
    if !out.is_empty() {
        out.push('\n');
    }
    if let Some(path) = file_path {
        out.push_str(&styles.muted_bold.render(&format!("@@ {path} @@")));
    } else {
        out.push_str(&styles.muted_bold.render("Diff:"));
    }

    // Truncate large diffs.
    let total_changed = diff_lines
        .iter()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .count();
    let truncated = total_changed > DIFF_TRUNCATE_THRESHOLD;
    let visible_lines = if truncated {
        // Show head + tail with separator.
        let mut visible = Vec::with_capacity(DIFF_TRUNCATE_HEAD + DIFF_TRUNCATE_TAIL + 1);
        visible.extend_from_slice(&diff_lines[..DIFF_TRUNCATE_HEAD.min(diff_lines.len())]);
        let omitted = diff_lines
            .len()
            .saturating_sub(DIFF_TRUNCATE_HEAD + DIFF_TRUNCATE_TAIL);
        if omitted > 0 {
            // We'll render a separator inline.
            visible.push(""); // placeholder for separator
            let tail_start = diff_lines.len().saturating_sub(DIFF_TRUNCATE_TAIL);
            visible.extend_from_slice(&diff_lines[tail_start..]);
        }
        visible
    } else {
        diff_lines
    };

    // Collect diff lines for word-level highlighting.
    render_diff_lines(&visible_lines, truncated, styles, &mut out);

    out
}

/// Render diff lines with word-level highlighting for paired -/+ lines.
fn render_diff_lines(lines: &[&str], truncated: bool, styles: &TuiStyles, out: &mut String) {
    let mut i = 0;
    let mut rendered_separator = false;
    while i < lines.len() {
        let line = lines[i];

        // Handle truncation separator placeholder.
        if truncated && !rendered_separator && line.is_empty() && i > 0 {
            out.push('\n');
            out.push_str(&styles.muted.render("  ... (diff truncated) ..."));
            rendered_separator = true;
            i += 1;
            continue;
        }

        out.push('\n');

        // Check for paired -/+ lines for word-level highlighting.
        if line.starts_with('-') {
            // Look ahead for a matching + line.
            if i + 1 < lines.len() && lines[i + 1].starts_with('+') {
                let removed = line;
                let added = lines[i + 1];
                render_word_diff_pair(removed, added, styles, out);
                i += 2;
                continue;
            }
            out.push_str(&styles.error_bold.render(line));
        } else if line.starts_with('+') {
            out.push_str(&styles.success_bold.render(line));
        } else {
            out.push_str(&styles.muted.render(line));
        }

        i += 1;
    }
}

/// Render a paired removed/added line with word-level change highlighting.
///
/// The line format from `generate_diff_string` is: `-NN content` / `+NN content`.
/// We diff the content portions and bold just the changed segments.
fn render_word_diff_pair(removed: &str, added: &str, styles: &TuiStyles, out: &mut String) {
    // Extract the prefix (e.g. "-  3 ") and the content after it.
    let (rem_prefix, rem_content) = split_diff_prefix(removed);
    let (add_prefix, add_content) = split_diff_prefix(added);

    // If either line has no content (just a prefix), fall back to simple coloring.
    if rem_content.is_empty() || add_content.is_empty() {
        out.push_str(&styles.error_bold.render(removed));
        out.push('\n');
        out.push_str(&styles.success_bold.render(added));
        return;
    }

    // Compute word-level diff.
    let diff = similar::TextDiff::from_words(rem_content, add_content);

    // Render removed line with deletions highlighted.
    out.push_str(&styles.error_bold.render(rem_prefix));
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => {
                // Bold + underline for the specific changed text.
                let styled = styles.error_bold.clone().underline();
                out.push_str(&styled.render(change.value()));
            }
            similar::ChangeTag::Equal => {
                out.push_str(&styles.error_bold.render(change.value()));
            }
            similar::ChangeTag::Insert => {} // skip insertions on removed line
        }
    }

    // Render added line with insertions highlighted.
    out.push('\n');
    out.push_str(&styles.success_bold.render(add_prefix));
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => {
                let styled = styles.success_bold.clone().underline();
                out.push_str(&styled.render(change.value()));
            }
            similar::ChangeTag::Equal => {
                out.push_str(&styles.success_bold.render(change.value()));
            }
            similar::ChangeTag::Delete => {} // skip deletions on added line
        }
    }
}

/// Split a diff line like `"-  3 content here"` into prefix `"-  3 "` and content `"content here"`.
fn split_diff_prefix(line: &str) -> (&str, &str) {
    // Format: [+-] then line number with spaces, then a space, then content.
    // E.g., "+  3 let x = 1;" → prefix "+  3 ", content "let x = 1;"
    // Or "- 12 old text"    → prefix "- 12 ", content "old text"
    if line.len() < 2 {
        return (line, "");
    }
    let rest = &line[1..]; // skip +/-
    // Find the first non-whitespace-non-digit character after the prefix.
    // The prefix is: sign + digits/spaces + one space separator.
    rest.find(|c: char| !c.is_ascii_digit() && c != ' ')
        .map_or((line, ""), |content_start| {
            // Include the sign character in the prefix.
            let prefix_end = 1 + content_start;
            (&line[..prefix_end], &line[prefix_end..])
        })
}

#[cfg(test)]
mod render_tool_message_tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn colors_diff_only_after_header() {
        let styles = Theme::dark().tui_styles();
        let input = "+notdiff\nDiff:\n+added\n-removed\n 1 ctx";
        let rendered = render_tool_message(input, &styles);

        assert!(rendered.contains(&styles.muted.render("+notdiff")));
        assert!(rendered.contains(&styles.muted_bold.render("Diff:")));
        assert!(rendered.contains(&styles.success_bold.render("+added")));
        assert!(rendered.contains(&styles.error_bold.render("-removed")));
        assert!(rendered.contains(&styles.muted.render(" 1 ctx")));
    }

    #[test]
    fn file_path_header_extracted() {
        let styles = Theme::dark().tui_styles();
        let input = "Successfully replaced text in src/main.rs.\nDiff:\n+ 1 new line";
        let rendered = render_tool_message(input, &styles);
        assert!(
            rendered.contains(&styles.muted_bold.render("@@ src/main.rs @@")),
            "Expected @@ src/main.rs @@ header, got: {rendered}"
        );
        assert!(!rendered.contains(&styles.muted_bold.render("Diff:")));
    }

    #[test]
    fn fallback_diff_header_when_no_path() {
        let styles = Theme::dark().tui_styles();
        let input = "Some other tool output.\nDiff:\n+ 1 added";
        let rendered = render_tool_message(input, &styles);
        assert!(
            rendered.contains(&styles.muted_bold.render("Diff:")),
            "Expected fallback Diff: header, got: {rendered}"
        );
    }

    #[test]
    fn word_level_diff_for_paired_lines() {
        let styles = Theme::dark().tui_styles();
        let input =
            "Successfully replaced text in foo.rs.\nDiff:\n- 1 let x = old;\n+ 1 let x = new;";
        let rendered = render_tool_message(input, &styles);
        let underline_old = styles.error_bold.underline();
        let underline_new = styles.success_bold.underline();
        assert!(
            rendered.contains(&underline_old.render("old;")),
            "Expected underlined 'old;' in removed line, got: {rendered}"
        );
        assert!(
            rendered.contains(&underline_new.render("new;")),
            "Expected underlined 'new;' in added line, got: {rendered}"
        );
    }

    #[test]
    fn split_diff_prefix_basic() {
        assert_eq!(
            split_diff_prefix("-  3 let x = 1;"),
            ("-  3 ", "let x = 1;")
        );
        assert_eq!(split_diff_prefix("+ 12 new text"), ("+ 12 ", "new text"));
    }

    #[test]
    fn split_diff_prefix_edge_cases() {
        assert_eq!(split_diff_prefix("-"), ("-", ""));
        assert_eq!(split_diff_prefix("+  1 "), ("+  1 ", ""));
        assert_eq!(split_diff_prefix(""), ("", ""));
    }

    #[test]
    fn large_diff_truncation() {
        let styles = Theme::dark().tui_styles();
        let mut lines = vec!["Successfully replaced text in big.rs.".to_string()];
        lines.push("Diff:".to_string());
        for i in 1..=60 {
            lines.push(format!("- {i} old line {i}"));
            lines.push(format!("+ {i} new line {i}"));
        }
        let input = lines.join("\n");
        let rendered = render_tool_message(&input, &styles);
        assert!(
            rendered.contains("diff truncated"),
            "Expected truncation marker, got: {rendered}"
        );
    }

    #[test]
    fn no_diff_renders_only_muted_text() {
        let styles = Theme::dark().tui_styles();
        let input = "Tool read:\nfile contents here";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.muted.render("Tool read:")));
        assert!(rendered.contains(&styles.muted.render("file contents here")));
        assert!(!rendered.contains("Diff:"));
        assert!(!rendered.contains("@@"));
    }

    #[test]
    fn empty_input_returns_empty() {
        let styles = Theme::dark().tui_styles();
        let rendered = render_tool_message("", &styles);
        assert!(rendered.is_empty() || rendered == styles.muted.render(""));
    }

    #[test]
    fn unpaired_minus_line_no_word_diff() {
        let styles = Theme::dark().tui_styles();
        // Single - line with no following + should render in error_bold without word diff
        let input = "output\nDiff:\n- 1 removed line\n 2 context";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.error_bold.render("- 1 removed line")));
        assert!(rendered.contains(&styles.muted.render(" 2 context")));
    }

    #[test]
    fn unpaired_plus_line_renders_success() {
        let styles = Theme::dark().tui_styles();
        // Standalone + line (no preceding -) should render in success_bold
        let input = "output\nDiff:\n+ 1 added line";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.success_bold.render("+ 1 added line")));
    }

    #[test]
    fn context_only_diff_no_color() {
        let styles = Theme::dark().tui_styles();
        let input = "output\nDiff:\n 1 unchanged line\n 2 also unchanged";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.muted.render(" 1 unchanged line")));
        assert!(rendered.contains(&styles.muted.render(" 2 also unchanged")));
    }

    #[test]
    fn word_diff_fallback_when_content_empty() {
        let styles = Theme::dark().tui_styles();
        // Prefix-only lines: split_diff_prefix returns ("- 1 ", "") for "- 1 "
        // render_word_diff_pair should fall back to simple coloring
        let input = "output\nDiff:\n-\n+";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.error_bold.render("-")));
        assert!(rendered.contains(&styles.success_bold.render("+")));
    }
}

fn tool_content_blocks_to_text(blocks: &[ContentBlock], show_images: bool) -> String {
    let mut output = String::new();
    let mut hidden_images = 0usize;

    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                if show_images {
                    let rendered =
                        crate::terminal_images::render_inline(&image.data, &image.mime_type, 72);
                    push_line(&mut output, &rendered);
                } else {
                    hidden_images = hidden_images.saturating_add(1);
                }
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
        }
    }

    if !show_images && hidden_images > 0 {
        push_line(&mut output, &format!("[{hidden_images} image(s) hidden]"));
    }

    output
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

const fn kind_rank(kind: &DiagnosticKind) -> u8 {
    match kind {
        DiagnosticKind::Warning => 0,
        DiagnosticKind::Collision => 1,
    }
}

fn format_resource_diagnostics(label: &str, diagnostics: &[ResourceDiagnostic]) -> (String, usize) {
    let mut ordered: Vec<&ResourceDiagnostic> = diagnostics.iter().collect();
    ordered.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)))
            .then_with(|| a.message.cmp(&b.message))
    });

    let mut out = String::new();
    let _ = writeln!(out, "{label}:");
    for diag in ordered {
        let kind = match diag.kind {
            DiagnosticKind::Warning => "warning",
            DiagnosticKind::Collision => "collision",
        };
        let _ = write!(out, "- {kind}: {} ({})", diag.message, diag.path.display());
        if let Some(collision) = &diag.collision {
            let _ = write!(
                out,
                " [winner: {} loser: {}]",
                collision.winner_path.display(),
                collision.loser_path.display()
            );
        }
        out.push('\n');
    }
    (out, diagnostics.len())
}

fn build_reload_diagnostics(
    models_error: Option<String>,
    resources: &ResourceLoader,
) -> (Option<String>, usize) {
    let mut sections = Vec::new();
    let mut count = 0usize;

    if let Some(err) = models_error {
        count = count.saturating_add(1);
        sections.push(format!("models.json:\n{err}"));
    }

    let mut resource_sections = Vec::new();
    let (skills_text, skills_count) =
        format_resource_diagnostics("Skills", resources.skill_diagnostics());
    if skills_count > 0 {
        resource_sections.push(skills_text);
        count = count.saturating_add(skills_count);
    }

    let (prompts_text, prompts_count) =
        format_resource_diagnostics("Prompts", resources.prompt_diagnostics());
    if prompts_count > 0 {
        resource_sections.push(prompts_text);
        count = count.saturating_add(prompts_count);
    }

    let (themes_text, themes_count) =
        format_resource_diagnostics("Themes", resources.theme_diagnostics());
    if themes_count > 0 {
        resource_sections.push(themes_text);
        count = count.saturating_add(themes_count);
    }

    if !resource_sections.is_empty() {
        sections.push(format!(
            "Resource diagnostics:\n{}",
            resource_sections.join("\n")
        ));
    }

    if sections.is_empty() {
        (None, 0)
    } else {
        (
            Some(format!("Reload diagnostics:\n\n{}", sections.join("\n\n"))),
            count,
        )
    }
}

fn push_line(out: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn truncate(s: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let count = s.chars().count();
    if count <= max_len {
        return s.to_string();
    }

    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let take_len = max_len - 3;
    let mut out = String::with_capacity(max_len);
    out.extend(s.chars().take(take_len));
    out.push_str("...");
    out
}

pub fn strip_thinking_level_suffix(pattern: &str) -> &str {
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return pattern;
    };
    match suffix.to_ascii_lowercase().as_str() {
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" => prefix,
        _ => pattern,
    }
}

pub fn parse_scoped_model_patterns(args: &str) -> Vec<String> {
    args.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn model_entry_matches(left: &ModelEntry, right: &ModelEntry) -> bool {
    left.model
        .provider
        .eq_ignore_ascii_case(&right.model.provider)
        && left.model.id.eq_ignore_ascii_case(&right.model.id)
}

pub fn resolve_scoped_model_entries(
    patterns: &[String],
    available_models: &[ModelEntry],
) -> Result<Vec<ModelEntry>, String> {
    let mut resolved: Vec<ModelEntry> = Vec::new();

    for pattern in patterns {
        let raw_pattern = strip_thinking_level_suffix(pattern);
        let is_glob =
            raw_pattern.contains('*') || raw_pattern.contains('?') || raw_pattern.contains('[');

        if is_glob {
            let glob = Pattern::new(&raw_pattern.to_lowercase())
                .map_err(|err| format!("Invalid model pattern \"{pattern}\": {err}"))?;

            for entry in available_models {
                let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
                let full_id_lower = full_id.to_lowercase();
                let id_lower = entry.model.id.to_lowercase();

                if (glob.matches(&full_id_lower) || glob.matches(&id_lower))
                    && !resolved
                        .iter()
                        .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
            }
            continue;
        }

        for entry in available_models {
            let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
            if raw_pattern.eq_ignore_ascii_case(&full_id)
                || raw_pattern.eq_ignore_ascii_case(&entry.model.id)
            {
                if !resolved
                    .iter()
                    .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
                break;
            }
        }
    }

    resolved.sort_by(|a, b| {
        let left = format!("{}/{}", a.model.provider, a.model.id);
        let right = format!("{}/{}", b.model.provider, b.model.id);
        left.cmp(&right)
    });

    Ok(resolved)
}

fn queued_message_preview(text: &str, max_len: usize) -> String {
    let first_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first_line.is_empty() {
        return "(empty)".to_string();
    }
    truncate(first_line, max_len)
}

/// Run the interactive mode.
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive(
    agent: Agent,
    session: Arc<Mutex<Session>>,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    pending_inputs: Vec<PendingInput>,
    save_enabled: bool,
    resources: ResourceLoader,
    resource_cli: ResourceCliOptions,
    extensions: Option<ExtensionManager>,
    cwd: PathBuf,
    runtime_handle: RuntimeHandle,
) -> anyhow::Result<()> {
    let show_hardware_cursor = config.show_hardware_cursor.unwrap_or_else(|| {
        std::env::var("PI_HARDWARE_CURSOR")
            .ok()
            .is_some_and(|val| val == "1")
    });
    let mut stdout = std::io::stdout();
    if show_hardware_cursor {
        let _ = crossterm::execute!(stdout, cursor::Show);
    } else {
        let _ = crossterm::execute!(stdout, cursor::Hide);
    }

    let (event_tx, event_rx) = mpsc::channel::<PiMsg>(1024);
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Message>();

    runtime_handle.spawn(async move {
        let cx = Cx::for_request();
        while let Ok(msg) = event_rx.recv(&cx).await {
            if matches!(msg, PiMsg::UiShutdown) {
                break;
            }
            let _ = ui_tx.send(Message::new(msg));
        }
    });

    let extensions = extensions;

    if let Some(manager) = &extensions {
        let (extension_ui_tx, extension_ui_rx) = mpsc::channel::<ExtensionUiRequest>(64);
        manager.set_ui_sender(extension_ui_tx);

        let extension_event_tx = event_tx.clone();
        runtime_handle.spawn(async move {
            let cx = Cx::for_request();
            while let Ok(request) = extension_ui_rx.recv(&cx).await {
                let _ = extension_event_tx.try_send(PiMsg::ExtensionUiRequest(request));
            }
        });
    }

    let (messages, usage) = {
        let cx = Cx::for_request();
        let guard = session
            .lock(&cx)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to lock session: {e}"))?;
        conversation_from_session(&guard)
    };

    let app = PiApp::new(
        agent,
        session,
        config,
        resources,
        resource_cli,
        cwd,
        model_entry,
        model_scope,
        available_models,
        pending_inputs,
        event_tx,
        runtime_handle,
        save_enabled,
        extensions,
        None,
        messages,
        usage,
    );

    Program::new(app)
        .with_alt_screen()
        .with_input_receiver(ui_rx)
        .run()?;

    let _ = crossterm::execute!(std::io::stdout(), cursor::Show);
    println!("Goodbye!");
    Ok(())
}

pub fn conversation_from_session(session: &Session) -> (Vec<ConversationMessage>, Usage) {
    let mut messages = Vec::new();
    let mut usage = Usage::default();

    for entry in session.entries_for_current_path() {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };

        match &message_entry.message {
            SessionMessage::User { content, .. } => {
                messages.push(ConversationMessage::new(
                    MessageRole::User,
                    user_content_to_text(content),
                    None,
                ));
            }
            SessionMessage::Assistant { message } => {
                let (text, thinking) = assistant_content_to_text(&message.content);
                add_usage(&mut usage, &message.usage);
                messages.push(ConversationMessage::new(
                    MessageRole::Assistant,
                    text,
                    thinking,
                ));
            }
            SessionMessage::ToolResult {
                tool_name,
                content,
                details,
                is_error,
                ..
            } => {
                let (mut text, _) = assistant_content_to_text(content);
                if let Some(diff) = details
                    .as_ref()
                    .and_then(|details| details.get("diff"))
                    .and_then(Value::as_str)
                {
                    let diff = diff.trim();
                    if !diff.is_empty() {
                        if !text.trim().is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str("Diff:\n");
                        text.push_str(diff);
                    }
                }
                let prefix = if *is_error {
                    "Tool error"
                } else {
                    "Tool result"
                };
                messages.push(ConversationMessage::tool(format!(
                    "{prefix} ({tool_name}): {text}"
                )));
            }
            SessionMessage::BashExecution {
                command,
                output,
                extra,
                ..
            } => {
                let mut text = bash_execution_to_text(command, output, 0, false, false, None);
                if extra
                    .get("excludeFromContext")
                    .and_then(Value::as_bool)
                    .is_some_and(|v| v)
                {
                    text.push_str("\n\n[Output excluded from model context]");
                }
                messages.push(ConversationMessage::tool(text));
            }
            SessionMessage::Custom {
                content, display, ..
            } => {
                if *display {
                    messages.push(ConversationMessage::new(
                        MessageRole::System,
                        content.clone(),
                        None,
                    ));
                }
            }
            _ => {}
        }
    }

    (messages, usage)
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
            truncate(first_line, 80)
        };

        out.push(ForkCandidate {
            id: id.clone(),
            summary,
        });
    }

    out
}

fn extension_model_from_entry(entry: &ModelEntry) -> Value {
    json!({
        "provider": entry.model.provider.as_str(),
        "id": entry.model.id.as_str(),
        "name": entry.model.name.as_str(),
        "api": entry.model.api.as_str(),
        "baseUrl": entry.model.base_url.as_str(),
        "reasoning": entry.model.reasoning,
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "apiKeyPresent": entry.api_key.is_some(),
    })
}

fn last_assistant_message(messages: &[ModelMessage]) -> Option<&crate::model::AssistantMessage> {
    messages.iter().rev().find_map(|msg| match msg {
        ModelMessage::Assistant(assistant) => Some(assistant),
        _ => None,
    })
}

fn add_usage(total: &mut Usage, delta: &Usage) {
    total.input = total.input.saturating_add(delta.input);
    total.output = total.output.saturating_add(delta.output);
    total.cache_read = total.cache_read.saturating_add(delta.cache_read);
    total.cache_write = total.cache_write.saturating_add(delta.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(delta.total_tokens);
    total.cost.input += delta.cost.input;
    total.cost.output += delta.cost.output;
    total.cost.cache_read += delta.cost.cache_read;
    total.cost.cache_write += delta.cost.cache_write;
    total.cost.total += delta.cost.total;
}

pub fn format_extension_ui_prompt(request: &ExtensionUiRequest) -> String {
    let title = request
        .payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Extension");
    let message = request
        .payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Show provenance: which extension is making this request.
    let provenance = request
        .extension_id
        .as_deref()
        .or_else(|| request.payload.get("extension_id").and_then(Value::as_str))
        .unwrap_or("unknown");

    match request.method.as_str() {
        "confirm" => {
            format!("[{provenance}] confirm: {title}\n{message}\n\nEnter yes/no, or 'cancel'.")
        }
        "select" => {
            let options = request
                .payload
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let mut out = String::new();
            let _ = writeln!(&mut out, "[{provenance}] select: {title}");
            if !message.trim().is_empty() {
                let _ = writeln!(&mut out, "{message}");
            }
            for (idx, opt) in options.iter().enumerate() {
                let label = opt
                    .get("label")
                    .and_then(Value::as_str)
                    .or_else(|| opt.get("value").and_then(Value::as_str))
                    .or_else(|| opt.as_str())
                    .unwrap_or("");
                let _ = writeln!(&mut out, "  {}) {label}", idx + 1);
            }
            out.push_str("\nEnter a number, label, or 'cancel'.");
            out
        }
        "input" => format!("[{provenance}] input: {title}\n{message}"),
        "editor" => format!("[{provenance}] editor: {title}\n{message}"),
        _ => format!("[{provenance}] {title} {message}"),
    }
}

pub fn parse_extension_ui_response(
    request: &ExtensionUiRequest,
    input: &str,
) -> Result<ExtensionUiResponse, String> {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("cancel") || trimmed.eq_ignore_ascii_case("c") {
        return Ok(ExtensionUiResponse {
            id: request.id.clone(),
            value: None,
            cancelled: true,
        });
    }

    match request.method.as_str() {
        "confirm" => {
            let value = match trimmed.to_lowercase().as_str() {
                "y" | "yes" | "true" | "1" => true,
                "n" | "no" | "false" | "0" => false,
                _ => {
                    return Err("Invalid confirmation. Enter yes/no, or 'cancel'.".to_string());
                }
            };
            Ok(ExtensionUiResponse {
                id: request.id.clone(),
                value: Some(Value::Bool(value)),
                cancelled: false,
            })
        }
        "select" => {
            let options = request
                .payload
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    "Invalid selection. Enter a number, label, or 'cancel'.".to_string()
                })?;

            if let Ok(index) = trimmed.parse::<usize>() {
                if index > 0 && index <= options.len() {
                    let chosen = &options[index - 1];
                    let value = chosen
                        .get("value")
                        .cloned()
                        .or_else(|| chosen.get("label").cloned())
                        .or_else(|| chosen.as_str().map(|s| Value::String(s.to_string())));
                    return Ok(ExtensionUiResponse {
                        id: request.id.clone(),
                        value,
                        cancelled: false,
                    });
                }
            }

            let lowered = trimmed.to_lowercase();
            for option in options {
                if let Some(value_str) = option.as_str() {
                    if value_str.to_lowercase() == lowered {
                        return Ok(ExtensionUiResponse {
                            id: request.id.clone(),
                            value: Some(Value::String(value_str.to_string())),
                            cancelled: false,
                        });
                    }
                }

                let label = option.get("label").and_then(Value::as_str).unwrap_or("");
                if !label.is_empty() && label.to_lowercase() == lowered {
                    let value = option.get("value").cloned().or_else(|| {
                        option
                            .get("label")
                            .and_then(Value::as_str)
                            .map(|s| Value::String(s.to_string()))
                    });
                    return Ok(ExtensionUiResponse {
                        id: request.id.clone(),
                        value,
                        cancelled: false,
                    });
                }

                if let Some(value_str) = option.get("value").and_then(Value::as_str) {
                    if value_str.to_lowercase() == lowered {
                        return Ok(ExtensionUiResponse {
                            id: request.id.clone(),
                            value: Some(Value::String(value_str.to_string())),
                            cancelled: false,
                        });
                    }
                }
            }

            Err("Invalid selection. Enter a number, label, or 'cancel'.".to_string())
        }
        _ => Ok(ExtensionUiResponse {
            id: request.id.clone(),
            value: Some(Value::String(input.to_string())),
            cancelled: false,
        }),
    }
}

impl SlashCommand {
    /// Parse a slash command from input.
    pub fn parse(input: &str) -> Option<(Self, &str)> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        let (cmd, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));

        let command = match cmd.to_lowercase().as_str() {
            "/help" | "/h" | "/?" => Self::Help,
            "/login" => Self::Login,
            "/logout" => Self::Logout,
            "/clear" | "/cls" => Self::Clear,
            "/model" | "/m" => Self::Model,
            "/thinking" | "/think" | "/t" => Self::Thinking,
            "/scoped-models" | "/scoped" => Self::ScopedModels,
            "/exit" | "/quit" | "/q" => Self::Exit,
            "/history" | "/hist" => Self::History,
            "/export" => Self::Export,
            "/session" | "/info" => Self::Session,
            "/settings" => Self::Settings,
            "/theme" => Self::Theme,
            "/resume" | "/r" => Self::Resume,
            "/new" => Self::New,
            "/copy" | "/cp" => Self::Copy,
            "/name" => Self::Name,
            "/hotkeys" | "/keys" | "/keybindings" => Self::Hotkeys,
            "/changelog" => Self::Changelog,
            "/tree" => Self::Tree,
            "/fork" => Self::Fork,
            "/compact" => Self::Compact,
            "/reload" => Self::Reload,
            "/share" => Self::Share,
            _ => return None,
        };

        Some((command, args.trim()))
    }

    /// Get help text for all commands.
    pub const fn help_text() -> &'static str {
        r"Available commands:
  /help, /h, /?      - Show this help message
  /login [provider]  - OAuth login (currently: anthropic)
  /logout [provider] - Remove stored OAuth credentials
  /clear, /cls       - Clear conversation history
  /model, /m [id|provider/id] - Show or change the current model
  /thinking, /t [level] - Set thinking level (off/minimal/low/medium/high/xhigh)
  /scoped-models [patterns|clear] - Show or set scoped models for cycling
  /history, /hist    - Show input history
  /export [path]     - Export conversation to HTML
  /session, /info    - Show session info (path, tokens, cost)
  /settings          - Open settings selector
  /theme [name]      - List or switch themes (dark/light/custom)
  /resume, /r        - Pick and resume a previous session
  /new               - Start a new session
  /copy, /cp         - Copy last assistant message to clipboard
  /name <name>       - Set session display name
  /hotkeys, /keys    - Show keyboard shortcuts
  /changelog         - Show changelog entries
  /tree              - Show session branch tree summary
  /fork [id|index]   - Fork from a user message (default: last on current path)
  /compact [notes]   - Compact older context with optional instructions
  /reload            - Reload skills/prompts from disk
  /share             - Upload session HTML to a secret GitHub gist and show URL
  /exit, /quit, /q   - Exit Pi

  Tips:
    • Use ↑/↓ arrows to navigate input history
    • Use Ctrl+L to open model selector
    • Use Ctrl+P to cycle scoped models
    • Use Shift+Enter (Ctrl+Enter on Windows) to insert a newline
    • Use PageUp/PageDown to scroll conversation history
    • Use Escape to cancel current input
    • Use /skill:name or /template to expand resources"
    }
}

/// Custom message types for async agent events.
#[derive(Debug, Clone)]
pub enum PiMsg {
    /// Agent started processing.
    AgentStart,
    /// Trigger processing of the next queued input (CLI startup messages).
    RunPending,
    /// Enqueue a pending input (extensions may inject while idle).
    EnqueuePendingInput(PendingInput),
    /// Internal: shut down the async→UI message bridge (used for clean exit).
    UiShutdown,
    /// Text delta from assistant.
    TextDelta(String),
    /// Thinking delta from assistant.
    ThinkingDelta(String),
    /// Tool execution started.
    ToolStart { name: String, tool_id: String },
    /// Tool execution update (streaming output).
    ToolUpdate {
        name: String,
        tool_id: String,
        content: Vec<ContentBlock>,
        details: Option<Value>,
    },
    /// Tool execution ended.
    ToolEnd {
        name: String,
        tool_id: String,
        is_error: bool,
    },
    /// Agent finished with final message.
    AgentDone {
        usage: Option<Usage>,
        stop_reason: StopReason,
        error_message: Option<String>,
    },
    /// Agent error.
    AgentError(String),
    /// Non-error system message.
    System(String),
    /// System note that does not mutate agent state (safe during streaming).
    SystemNote(String),
    /// Update last user message content (input transform/redaction).
    UpdateLastUserMessage(String),
    /// Bash command result (non-agent).
    BashResult {
        display: String,
        content_for_agent: Option<Vec<ContentBlock>>,
    },
    /// Replace conversation state from session (compaction/fork).
    ConversationReset {
        messages: Vec<ConversationMessage>,
        usage: Usage,
        status: Option<String>,
    },
    /// Set the editor contents (used by /tree selection of user/custom messages).
    SetEditorText(String),
    /// Reloaded skills/prompts/themes/extensions.
    ResourcesReloaded {
        resources: ResourceLoader,
        status: String,
        diagnostics: Option<String>,
    },
    /// Extension UI request (select/confirm/input/editor/notify).
    ExtensionUiRequest(ExtensionUiRequest),
    /// Extension command finished execution.
    ExtensionCommandDone {
        command: String,
        display: String,
        is_error: bool,
    },
}

// ============================================================================
// /tree navigation UI
// ============================================================================

#[derive(Debug, Clone)]
enum TreeUiState {
    Selector(TreeSelectorState),
    SummaryPrompt(TreeSummaryPromptState),
    CustomPrompt(TreeCustomPromptState),
}

#[derive(Debug, Clone)]
struct TreeSelectorRow {
    id: String,
    parent_id: Option<String>,
    display: String,
    resubmit_text: Option<String>,
}

#[derive(Debug, Clone)]
struct TreeSelectorState {
    rows: Vec<TreeSelectorRow>,
    selected: usize,
    scroll: usize,
    max_visible_lines: usize,
    user_only: bool,
    show_all: bool,
    current_leaf_id: Option<String>,
    last_selected_id: Option<String>,
    parent_by_id: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
struct PendingTreeNavigation {
    session_id: String,
    old_leaf_id: Option<String>,
    selected_entry_id: String,
    new_leaf_id: Option<String>,
    editor_text: Option<String>,
    entries_to_summarize: Vec<SessionEntry>,
    summary_from_id: String,
    api_key_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeSummaryChoice {
    NoSummary,
    Summarize,
    SummarizeWithCustomPrompt,
}

impl TreeSummaryChoice {
    const fn all() -> [Self; 3] {
        [
            Self::NoSummary,
            Self::Summarize,
            Self::SummarizeWithCustomPrompt,
        ]
    }

    const fn label(self) -> &'static str {
        match self {
            Self::NoSummary => "No summary",
            Self::Summarize => "Summarize",
            Self::SummarizeWithCustomPrompt => "Summarize with custom prompt",
        }
    }
}

#[derive(Debug, Clone)]
struct TreeSummaryPromptState {
    pending: PendingTreeNavigation,
    selected: usize,
}

#[derive(Debug, Clone)]
struct TreeCustomPromptState {
    pending: PendingTreeNavigation,
    instructions: String,
}

impl TreeSelectorState {
    fn new(session: &Session, term_height: usize, initial_selected_id: Option<&str>) -> Self {
        let max_visible_lines = (term_height / 2).max(5);
        let current_leaf_id = session.leaf_id.clone();

        let mut state = Self {
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            max_visible_lines,
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

    fn rebuild(&mut self, session: &Session) {
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

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max_index = self.rows.len() - 1;
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            let delta = usize::try_from(delta).unwrap_or(usize::MAX);
            self.selected = (self.selected + delta).min(max_index);
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

fn resolve_tree_selector_initial_id(session: &Session, args: &str) -> Option<String> {
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
        .filter_map(SessionEntry::base_id)
        .filter(|id| id.starts_with(arg))
        .take(2)
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        return Some(matches[0].clone());
    }

    None
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
        let normalized = text.replace('\n', " ");
        if normalized.chars().count() <= max {
            return normalized;
        }
        normalized
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
            + "…"
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

fn collect_tree_branch_entries(
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

/// Progress metrics emitted by long-running tools (e.g. bash).
#[derive(Debug, Clone)]
struct ToolProgress {
    started_at: std::time::Instant,
    elapsed_ms: u128,
    line_count: usize,
    byte_count: usize,
    timeout_ms: Option<u64>,
}

impl ToolProgress {
    fn new() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            elapsed_ms: 0,
            line_count: 0,
            byte_count: 0,
            timeout_ms: None,
        }
    }

    /// Update from a `details.progress` JSON object emitted by tool callbacks.
    fn update_from_details(&mut self, details: Option<&serde_json::Value>) {
        // Always update elapsed from wall clock as fallback.
        self.elapsed_ms = self.started_at.elapsed().as_millis();

        let Some(details) = details else {
            return;
        };
        if let Some(progress) = details.get("progress") {
            if let Some(v) = progress
                .get("elapsedMs")
                .and_then(serde_json::Value::as_u64)
            {
                self.elapsed_ms = u128::from(v);
            }
            if let Some(v) = progress
                .get("lineCount")
                .and_then(serde_json::Value::as_u64)
            {
                #[allow(clippy::cast_possible_truncation)]
                let count = v as usize;
                self.line_count = count;
            }
            if let Some(v) = progress
                .get("byteCount")
                .and_then(serde_json::Value::as_u64)
            {
                #[allow(clippy::cast_possible_truncation)]
                let count = v as usize;
                self.byte_count = count;
            }
            if let Some(v) = progress
                .get("timeoutMs")
                .and_then(serde_json::Value::as_u64)
            {
                self.timeout_ms = Some(v);
            }
        }
    }

    /// Format a compact status string like `"Running bash · 3s · 42 lines"`.
    fn format_display(&self, tool_name: &str) -> String {
        let secs = self.elapsed_ms / 1000;
        let mut parts = vec![format!("Running {tool_name}"), format!("{secs}s")];
        if self.line_count > 0 {
            parts.push(format!("{} lines", format_count(self.line_count)));
        } else if self.byte_count > 0 {
            parts.push(format!("{} bytes", format_count(self.byte_count)));
        }
        if let Some(timeout_ms) = self.timeout_ms {
            let timeout_s = timeout_ms / 1000;
            if timeout_s > 0 {
                parts.push(format!("timeout {timeout_s}s"));
            }
        }
        parts.join(" \u{2022} ")
    }
}

/// Format a count with K/M suffix for compact display.
#[allow(clippy::cast_precision_loss)]
fn format_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedMessageKind {
    Steering,
    FollowUp,
}

#[derive(Debug)]
struct InteractiveMessageQueue {
    steering: VecDeque<String>,
    follow_up: VecDeque<String>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl InteractiveMessageQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn push_steering(&mut self, text: String) {
        self.steering.push_back(text);
    }

    fn push_follow_up(&mut self, text: String) {
        self.follow_up.push_back(text);
    }

    fn pop_steering(&mut self) -> Vec<String> {
        self.pop_kind(QueuedMessageKind::Steering)
    }

    fn pop_follow_up(&mut self) -> Vec<String> {
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

    fn clear_all(&mut self) -> (Vec<String>, Vec<String>) {
        let steering = self.steering.drain(..).collect();
        let follow_up = self.follow_up.drain(..).collect();
        (steering, follow_up)
    }
}

#[derive(Debug)]
struct InjectedMessageQueue {
    steering: VecDeque<ModelMessage>,
    follow_up: VecDeque<ModelMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl InjectedMessageQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    fn push_kind(&mut self, kind: QueuedMessageKind, message: ModelMessage) {
        match kind {
            QueuedMessageKind::Steering => self.steering.push_back(message),
            QueuedMessageKind::FollowUp => self.follow_up.push_back(message),
        }
    }

    fn push_steering(&mut self, message: ModelMessage) {
        self.push_kind(QueuedMessageKind::Steering, message);
    }

    fn push_follow_up(&mut self, message: ModelMessage) {
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

    fn pop_steering(&mut self) -> Vec<ModelMessage> {
        self.pop_kind(QueuedMessageKind::Steering)
    }

    fn pop_follow_up(&mut self) -> Vec<ModelMessage> {
        self.pop_kind(QueuedMessageKind::FollowUp)
    }
}

#[derive(Clone)]
struct InteractiveExtensionHostActions {
    session: Arc<Mutex<Session>>,
    agent: Arc<Mutex<Agent>>,
    event_tx: mpsc::Sender<PiMsg>,
    extension_streaming: Arc<AtomicBool>,
    user_queue: Arc<StdMutex<InteractiveMessageQueue>>,
    injected_queue: Arc<StdMutex<InjectedMessageQueue>>,
}

impl InteractiveExtensionHostActions {
    #[allow(clippy::unnecessary_wraps)]
    fn queue_custom_message(
        &self,
        deliver_as: Option<ExtensionDeliverAs>,
        message: ModelMessage,
    ) -> crate::error::Result<()> {
        let deliver_as = deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
        let kind = match deliver_as {
            ExtensionDeliverAs::FollowUp => QueuedMessageKind::FollowUp,
            ExtensionDeliverAs::Steer | ExtensionDeliverAs::NextTurn => QueuedMessageKind::Steering,
        };
        let Ok(mut queue) = self.injected_queue.lock() else {
            return Ok(());
        };
        match kind {
            QueuedMessageKind::Steering => queue.push_steering(message),
            QueuedMessageKind::FollowUp => queue.push_follow_up(message),
        }
        Ok(())
    }

    async fn append_to_session(&self, message: ModelMessage) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut session_guard = self
            .session
            .lock(&cx)
            .await
            .map_err(|e| crate::error::Error::session(e.to_string()))?;
        session_guard.append_model_message(message);
        Ok(())
    }
}

#[async_trait]
impl ExtensionHostActions for InteractiveExtensionHostActions {
    async fn send_message(&self, message: ExtensionSendMessage) -> crate::error::Result<()> {
        let custom_message = ModelMessage::Custom(CustomMessage {
            content: message.content,
            custom_type: message.custom_type,
            display: message.display,
            details: message.details,
            timestamp: Utc::now().timestamp_millis(),
        });

        let is_streaming = self.extension_streaming.load(Ordering::SeqCst);
        if is_streaming {
            // Queue into the agent loop; session persistence happens when the message is delivered.
            self.queue_custom_message(message.deliver_as, custom_message.clone())?;
            if let ModelMessage::Custom(custom) = &custom_message {
                if custom.display {
                    let _ = self
                        .event_tx
                        .try_send(PiMsg::SystemNote(custom.content.clone()));
                }
            }
            return Ok(());
        }

        // Agent is idle: persist immediately and update in-memory history so it affects the next run.
        // Triggering a new turn for custom messages is handled separately and may be implemented later.
        let _ = message.trigger_turn;
        self.append_to_session(custom_message.clone()).await?;

        if let Ok(mut agent_guard) = self.agent.try_lock() {
            agent_guard.add_message(custom_message.clone());
        }

        if let ModelMessage::Custom(custom) = &custom_message {
            if custom.display {
                let _ = self
                    .event_tx
                    .try_send(PiMsg::SystemNote(custom.content.clone()));
            }
        }

        Ok(())
    }

    async fn send_user_message(
        &self,
        message: ExtensionSendUserMessage,
    ) -> crate::error::Result<()> {
        let is_streaming = self.extension_streaming.load(Ordering::SeqCst);
        if is_streaming {
            let deliver_as = message.deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
            let Ok(mut queue) = self.user_queue.lock() else {
                return Ok(());
            };
            match deliver_as {
                ExtensionDeliverAs::FollowUp => queue.push_follow_up(message.text),
                ExtensionDeliverAs::Steer | ExtensionDeliverAs::NextTurn => {
                    queue.push_steering(message.text);
                }
            }
            return Ok(());
        }

        let _ = self
            .event_tx
            .try_send(PiMsg::EnqueuePendingInput(PendingInput::Text(message.text)));
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct HistoryItem {
    value: String,
}

impl ListItem for HistoryItem {
    fn filter_value(&self) -> &str {
        &self.value
    }
}

#[derive(Clone)]
struct HistoryList {
    // We never render the list UI; we use it as a battle-tested cursor+navigation model.
    // The final item is always a sentinel representing "empty input".
    list: List<HistoryItem, DefaultDelegate>,
}

impl HistoryList {
    fn new() -> Self {
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

    fn entries(&self) -> &[HistoryItem] {
        let items = self.list.items();
        if items.len() <= 1 {
            return &[];
        }
        &items[..items.len().saturating_sub(1)]
    }

    fn has_entries(&self) -> bool {
        !self.entries().is_empty()
    }

    fn cursor_is_empty(&self) -> bool {
        // Sentinel is always the final item.
        self.list.index() + 1 == self.list.items().len()
    }

    fn reset_cursor(&mut self) {
        let last = self.list.items().len().saturating_sub(1);
        self.list.select(last);
    }

    fn push(&mut self, value: String) {
        let mut items = self.entries().to_vec();
        items.push(HistoryItem { value });
        items.push(HistoryItem {
            value: String::new(),
        });

        self.list.set_items(items);
        self.reset_cursor();
    }

    fn cursor_up(&mut self) {
        self.list.cursor_up();
    }

    fn cursor_down(&mut self) {
        self.list.cursor_down();
    }

    fn selected_value(&self) -> &str {
        self.list
            .selected_item()
            .map_or("", |item| item.value.as_str())
    }
}

/// The main interactive TUI application model.
#[allow(clippy::struct_excessive_bools)]
#[derive(bubbletea::Model)]
pub struct PiApp {
    // Input state
    input: TextArea,
    history: HistoryList,
    input_mode: InputMode,
    pending_inputs: VecDeque<PendingInput>,
    message_queue: Arc<StdMutex<InteractiveMessageQueue>>,

    // Display state - viewport for scrollable conversation
    conversation_viewport: Viewport,
    spinner: SpinnerModel,
    agent_state: AgentState,

    // Terminal dimensions
    term_width: usize,
    term_height: usize,
    editor_padding_x: usize,

    // Conversation state
    messages: Vec<ConversationMessage>,
    current_response: String,
    current_thinking: String,
    thinking_visible: bool,
    tools_expanded: bool,
    current_tool: Option<String>,
    tool_progress: Option<ToolProgress>,
    pending_tool_output: Option<String>,

    // Session and config
    session: Arc<Mutex<Session>>,
    config: Config,
    theme: Theme,
    styles: TuiStyles,
    markdown_style: GlamourStyleConfig,
    resources: ResourceLoader,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
    model_entry: ModelEntry,
    model_entry_shared: Arc<StdMutex<ModelEntry>>,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    model: String,
    agent: Arc<Mutex<Agent>>,
    save_enabled: bool,
    abort_handle: Option<AbortHandle>,
    bash_running: bool,

    // Token tracking
    total_usage: Usage,

    // Async channel for agent events
    event_tx: mpsc::Sender<PiMsg>,
    runtime_handle: RuntimeHandle,

    // Extension session state
    extension_streaming: Arc<AtomicBool>,
    extension_compacting: Arc<AtomicBool>,
    extension_ui_queue: VecDeque<ExtensionUiRequest>,
    active_extension_ui: Option<ExtensionUiRequest>,

    // Status message (for slash command feedback)
    status_message: Option<String>,

    // OAuth login flow state (awaiting code paste)
    pending_oauth: Option<PendingOAuth>,

    // Extension system
    extensions: Option<ExtensionManager>,

    // Keybindings for action dispatch
    keybindings: crate::keybindings::KeyBindings,

    // Track last Ctrl+C time for double-tap quit detection
    last_ctrlc_time: Option<std::time::Instant>,
    // Track last Escape time for double-tap tree/fork
    last_escape_time: Option<std::time::Instant>,

    // Autocomplete state
    autocomplete: AutocompleteState,

    // Session picker overlay for /resume
    session_picker: Option<SessionPickerOverlay>,

    // Settings UI overlay for /settings
    settings_ui: Option<SettingsUiState>,

    // Theme picker overlay
    theme_picker: Option<ThemePickerOverlay>,

    // Tree navigation UI state (for /tree command)
    tree_ui: Option<TreeUiState>,

    // Capability prompt overlay (extension permission request)
    capability_prompt: Option<CapabilityPromptOverlay>,

    // Branch picker overlay (Ctrl+B quick branch switching)
    branch_picker: Option<BranchPickerOverlay>,

    // Model selector overlay (Ctrl+L)
    model_selector: Option<crate::model_selector::ModelSelectorOverlay>,
}

/// Autocomplete dropdown state.
#[derive(Debug)]
struct AutocompleteState {
    /// The autocomplete provider that generates suggestions.
    provider: AutocompleteProvider,
    /// Whether the dropdown is currently visible.
    open: bool,
    /// Current list of suggestions.
    items: Vec<AutocompleteItem>,
    /// Index of the currently selected item.
    selected: usize,
    /// The range of text to replace when accepting a suggestion.
    replace_range: std::ops::Range<usize>,
    /// Maximum number of items to display in the dropdown.
    max_visible: usize,
}

impl AutocompleteState {
    const fn new(cwd: PathBuf, catalog: AutocompleteCatalog) -> Self {
        Self {
            provider: AutocompleteProvider::new(cwd, catalog),
            open: false,
            items: Vec::new(),
            selected: 0,
            replace_range: 0..0,
            max_visible: 10,
        }
    }

    fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
        self.replace_range = 0..0;
    }

    fn open_with(&mut self, response: AutocompleteResponse) {
        if response.items.is_empty() {
            self.close();
            return;
        }
        self.open = true;
        self.items = response.items;
        self.selected = 0;
        self.replace_range = response.replace;
    }

    fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.items.get(self.selected)
    }

    /// Returns the scroll offset for the dropdown view.
    const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }
}

/// Session picker overlay state for /resume command.
#[derive(Debug)]
struct SessionPickerOverlay {
    /// List of available sessions.
    sessions: Vec<SessionMeta>,
    /// Index of the currently selected session.
    selected: usize,
    /// Maximum number of sessions to display.
    max_visible: usize,
    /// Whether we're in delete confirmation mode.
    confirm_delete: bool,
    /// Status message to render in the picker overlay.
    status_message: Option<String>,
    /// Base directory for session storage (used for index cleanup).
    sessions_root: Option<PathBuf>,
}

/// Settings selector overlay state for /settings command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsUiEntry {
    Summary,
    Theme,
    SteeringMode,
    FollowUpMode,
    QuietStartup,
    CollapseChangelog,
    HideThinkingBlock,
    ShowHardwareCursor,
    DoubleEscapeAction,
    EditorPaddingX,
    AutocompleteMaxVisible,
}

#[derive(Debug, Clone)]
enum ThemePickerItem {
    BuiltIn(&'static str),
    File(PathBuf),
}

#[derive(Debug)]
struct ThemePickerOverlay {
    items: Vec<ThemePickerItem>,
    selected: usize,
    max_visible: usize,
}

impl ThemePickerOverlay {
    fn new(cwd: &Path) -> Self {
        let mut items = Vec::new();
        items.push(ThemePickerItem::BuiltIn("dark"));
        items.push(ThemePickerItem::BuiltIn("light"));
        items.push(ThemePickerItem::BuiltIn("solarized"));
        items.extend(
            Theme::discover_themes(cwd)
                .into_iter()
                .map(ThemePickerItem::File),
        );
        Self {
            items,
            selected: 0,
            max_visible: 10,
        }
    }

    fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    fn selected_item(&self) -> Option<&ThemePickerItem> {
        self.items.get(self.selected)
    }
}

#[derive(Debug)]
struct SettingsUiState {
    entries: Vec<SettingsUiEntry>,
    selected: usize,
    max_visible: usize,
}

impl SettingsUiState {
    fn new() -> Self {
        Self {
            entries: vec![
                SettingsUiEntry::Summary,
                SettingsUiEntry::Theme,
                SettingsUiEntry::SteeringMode,
                SettingsUiEntry::FollowUpMode,
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

    fn select_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.entries.len() - 1);
        }
    }

    fn selected_entry(&self) -> Option<SettingsUiEntry> {
        self.entries.get(self.selected).copied()
    }

    const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }
}

// ---------------------------------------------------------------------------
// Capability prompt overlay
// ---------------------------------------------------------------------------

/// User action choices for a capability prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityAction {
    AllowOnce,
    AllowAlways,
    Deny,
    DenyAlways,
}

impl CapabilityAction {
    const ALL: [Self; 4] = [
        Self::AllowOnce,
        Self::AllowAlways,
        Self::Deny,
        Self::DenyAlways,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "Allow Once",
            Self::AllowAlways => "Allow Always",
            Self::Deny => "Deny",
            Self::DenyAlways => "Deny Always",
        }
    }

    const fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }

    const fn is_persistent(self) -> bool {
        matches!(self, Self::AllowAlways | Self::DenyAlways)
    }
}

/// Modal overlay for extension capability prompts.
#[derive(Debug)]
struct CapabilityPromptOverlay {
    /// The underlying UI request (used to send response).
    request: ExtensionUiRequest,
    /// Extension that requested the capability.
    extension_id: String,
    /// Capability being requested (e.g. "exec", "http").
    capability: String,
    /// Human-readable description of what the capability does.
    description: String,
    /// Which button is focused.
    focused: usize,
    /// Auto-deny countdown (remaining seconds).  `None` = no timer.
    auto_deny_secs: Option<u32>,
}

impl CapabilityPromptOverlay {
    fn from_request(request: ExtensionUiRequest) -> Self {
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

    const fn focus_next(&mut self) {
        self.focused = (self.focused + 1) % CapabilityAction::ALL.len();
    }

    fn focus_prev(&mut self) {
        self.focused = self
            .focused
            .checked_sub(1)
            .unwrap_or(CapabilityAction::ALL.len() - 1);
    }

    const fn selected_action(&self) -> CapabilityAction {
        CapabilityAction::ALL[self.focused]
    }

    /// Returns `true` if this is a capability-specific confirm prompt (not a
    /// generic extension confirm).
    fn is_capability_prompt(request: &ExtensionUiRequest) -> bool {
        request.method == "confirm"
            && request.payload.get("capability").is_some()
            && request.payload.get("extension_id").is_some()
    }
}

/// Branch picker overlay for quick branch switching (Ctrl+B).
#[derive(Debug)]
struct BranchPickerOverlay {
    /// Sibling branches at the nearest fork point.
    branches: Vec<crate::session::SiblingBranch>,
    /// Which branch is currently selected in the picker.
    selected: usize,
    /// Maximum visible rows before scrolling.
    max_visible: usize,
}

impl BranchPickerOverlay {
    fn new(branches: Vec<crate::session::SiblingBranch>) -> Self {
        let current_idx = branches.iter().position(|b| b.is_current).unwrap_or(0);
        Self {
            branches,
            selected: current_idx,
            max_visible: 10,
        }
    }

    fn select_next(&mut self) {
        if !self.branches.is_empty() {
            self.selected = (self.selected + 1) % self.branches.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.branches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.branches.len() - 1);
        }
    }

    const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    fn selected_branch(&self) -> Option<&crate::session::SiblingBranch> {
        self.branches.get(self.selected)
    }
}

impl SessionPickerOverlay {
    const fn new(sessions: Vec<SessionMeta>) -> Self {
        Self {
            sessions,
            selected: 0,
            max_visible: 10,
            confirm_delete: false,
            status_message: None,
            sessions_root: None,
        }
    }

    const fn new_with_root(sessions: Vec<SessionMeta>, sessions_root: Option<PathBuf>) -> Self {
        Self {
            sessions,
            selected: 0,
            max_visible: 10,
            confirm_delete: false,
            status_message: None,
            sessions_root,
        }
    }

    fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
        }
    }

    fn selected_session(&self) -> Option<&SessionMeta> {
        self.sessions.get(self.selected)
    }

    /// Returns the scroll offset for the dropdown view.
    const fn scroll_offset(&self) -> usize {
        if self.selected < self.max_visible {
            0
        } else {
            self.selected - self.max_visible + 1
        }
    }

    /// Remove the selected session from the list and adjust selection.
    fn remove_selected(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.sessions.remove(self.selected);
        // Adjust selection to stay in bounds
        if self.selected >= self.sessions.len() && self.selected > 0 {
            self.selected = self.sessions.len() - 1;
        }
        // Clear confirmation state
        self.confirm_delete = false;
    }

    fn delete_selected(&mut self) -> crate::error::Result<()> {
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
}

#[derive(Debug, Clone)]
struct PendingOAuth {
    provider: String,
    verifier: String,
    /// OAuth config for extension-registered providers (None for built-in like anthropic).
    oauth_config: Option<OAuthConfig>,
}

struct InteractiveExtensionSession {
    session: Arc<Mutex<Session>>,
    model_entry: Arc<StdMutex<ModelEntry>>,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    config: Config,
    save_enabled: bool,
}

#[async_trait]
impl ExtensionSession for InteractiveExtensionSession {
    async fn get_state(&self) -> Value {
        let model = {
            let guard = self.model_entry.lock().unwrap();
            extension_model_from_entry(&guard)
        };

        let cx = Cx::for_request();
        let (session_file, session_id, session_name, message_count, thinking_level) =
            self.session.lock(&cx).await.map_or_else(
                |_| (None, String::new(), None, 0, "off".to_string()),
                |guard| {
                    let message_count = guard
                        .entries_for_current_path()
                        .iter()
                        .filter(|entry| matches!(entry, SessionEntry::Message(_)))
                        .count();
                    let session_name = guard.get_name();
                    let thinking_level = guard
                        .header
                        .thinking_level
                        .clone()
                        .unwrap_or_else(|| "off".to_string());
                    (
                        guard.path.as_ref().map(|p| p.display().to_string()),
                        guard.header.id.clone(),
                        session_name,
                        message_count,
                        thinking_level,
                    )
                },
            );

        json!({
            "model": model,
            "thinkingLevel": thinking_level,
            "isStreaming": self.is_streaming.load(Ordering::SeqCst),
            "isCompacting": self.is_compacting.load(Ordering::SeqCst),
            "steeringMode": "one-at-a-time",
            "followUpMode": "one-at-a-time",
            "sessionFile": session_file,
            "sessionId": session_id,
            "sessionName": session_name,
            "autoCompactionEnabled": self.config.compaction_enabled(),
            "messageCount": message_count,
            "pendingMessageCount": 0,
        })
    }

    async fn get_messages(&self) -> Vec<SessionMessage> {
        let cx = Cx::for_request();
        let Ok(guard) = self.session.lock(&cx).await else {
            return Vec::new();
        };
        guard
            .entries_for_current_path()
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(msg) => match msg.message {
                    SessionMessage::User { .. }
                    | SessionMessage::Assistant { .. }
                    | SessionMessage::ToolResult { .. }
                    | SessionMessage::BashExecution { .. } => Some(msg.message.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
    }

    async fn get_entries(&self) -> Vec<Value> {
        // Spec §3.1: return ALL session entries (entire session file), append order.
        let cx = Cx::for_request();
        let Ok(guard) = self.session.lock(&cx).await else {
            return Vec::new();
        };
        guard
            .entries
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect()
    }

    async fn get_branch(&self) -> Vec<Value> {
        // Spec §3.2: return current path from root to leaf.
        let cx = Cx::for_request();
        let Ok(guard) = self.session.lock(&cx).await else {
            return Vec::new();
        };
        guard
            .entries_for_current_path()
            .iter()
            .filter_map(|entry| serde_json::to_value(*entry).ok())
            .collect()
    }

    async fn set_name(&self, name: String) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        guard.set_name(&name);
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }

    async fn append_message(&self, message: SessionMessage) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        guard.append_message(message);
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> crate::error::Result<()> {
        if custom_type.trim().is_empty() {
            return Err(crate::error::Error::validation(
                "customType must not be empty",
            ));
        }
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        guard.append_custom_entry(custom_type, data);
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        guard.append_model_change(provider.clone(), model_id.clone());
        guard.set_model_header(Some(provider), Some(model_id), None);
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        let cx = Cx::for_request();
        let Ok(guard) = self.session.lock(&cx).await else {
            return (None, None);
        };
        (guard.header.provider.clone(), guard.header.model_id.clone())
    }

    async fn set_thinking_level(&self, level: String) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        guard.append_thinking_level_change(level.clone());
        guard.set_model_header(None, None, Some(level));
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        let cx = Cx::for_request();
        let Ok(guard) = self.session.lock(&cx).await else {
            return None;
        };
        guard.header.thinking_level.clone()
    }

    async fn set_label(
        &self,
        target_id: String,
        label: Option<String>,
    ) -> crate::error::Result<()> {
        let cx = Cx::for_request();
        let mut guard =
            self.session.lock(&cx).await.map_err(|err| {
                crate::error::Error::session(format!("session lock failed: {err}"))
            })?;
        if guard.add_label(&target_id, label).is_none() {
            return Err(crate::error::Error::validation(format!(
                "target entry '{target_id}' not found in session"
            )));
        }
        if self.save_enabled {
            guard.save().await?;
        }
        Ok(())
    }
}

/// Tool output line count above which blocks auto-collapse.
const TOOL_AUTO_COLLAPSE_THRESHOLD: usize = 20;
/// Number of preview lines to show when a tool block is collapsed.
const TOOL_COLLAPSE_PREVIEW_LINES: usize = 5;

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
    const fn new(role: MessageRole, content: String, thinking: Option<String>) -> Self {
        Self {
            role,
            content,
            thinking,
            collapsed: false,
        }
    }

    /// Create a tool output message with auto-collapse for large outputs.
    fn tool(content: String) -> Self {
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

impl PiApp {
    /// Create a new Pi application.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    pub fn new(
        agent: Agent,
        session: Arc<Mutex<Session>>,
        config: Config,
        resources: ResourceLoader,
        resource_cli: ResourceCliOptions,
        cwd: PathBuf,
        model_entry: ModelEntry,
        model_scope: Vec<ModelEntry>,
        available_models: Vec<ModelEntry>,
        pending_inputs: Vec<PendingInput>,
        event_tx: mpsc::Sender<PiMsg>,
        runtime_handle: RuntimeHandle,
        save_enabled: bool,
        extensions: Option<ExtensionManager>,
        keybindings_override: Option<KeyBindings>,
        messages: Vec<ConversationMessage>,
        total_usage: Usage,
    ) -> Self {
        // Get terminal size
        let (term_width, term_height) =
            terminal::size().map_or((80, 24), |(w, h)| (w as usize, h as usize));

        let theme = Theme::resolve(&config, &cwd);
        let styles = theme.tui_styles();
        let markdown_style = theme.glamour_style_config();
        let editor_padding_x = config.editor_padding_x.unwrap_or(0).min(3) as usize;
        let autocomplete_max_visible =
            config.autocomplete_max_visible.unwrap_or(5).clamp(3, 20) as usize;
        let thinking_visible = !config.hide_thinking_block.unwrap_or(false);

        // Configure text area for input
        let mut input = TextArea::new();
        input.placeholder = "Type a message... (/help, /exit)".to_string();
        input.show_line_numbers = false;
        input.prompt = "> ".to_string();
        input.set_height(3); // Start with 3 lines
        input.set_width(term_width.saturating_sub(4 + editor_padding_x));
        input.max_height = 10; // Allow expansion up to 10 lines
        input.focus();

        let spinner = SpinnerModel::with_spinner(spinners::dot()).style(styles.accent.clone());

        // Configure viewport for conversation history.
        // Height budget: header(2) + input_decoration(2) + input_lines + footer(2).
        let chrome = 2 + 2 + 2; // header + input_decoration + footer
        let viewport_height = term_height.saturating_sub(chrome + input.height());
        let mut conversation_viewport =
            Viewport::new(term_width.saturating_sub(2), viewport_height);
        conversation_viewport.mouse_wheel_enabled = true;
        conversation_viewport.mouse_wheel_delta = 3;

        let model = format!(
            "{}/{}",
            model_entry.model.provider.as_str(),
            model_entry.model.id.as_str()
        );

        let model_entry_shared = Arc::new(StdMutex::new(model_entry.clone()));
        let extension_streaming = Arc::new(AtomicBool::new(false));
        let extension_compacting = Arc::new(AtomicBool::new(false));
        let steering_mode = parse_queue_mode_or_default(config.steering_mode.as_deref());
        let follow_up_mode = parse_queue_mode_or_default(config.follow_up_mode.as_deref());
        let message_queue = Arc::new(StdMutex::new(InteractiveMessageQueue::new(
            steering_mode,
            follow_up_mode,
        )));
        let injected_queue = Arc::new(StdMutex::new(InjectedMessageQueue::new(
            steering_mode,
            follow_up_mode,
        )));

        let mut agent = agent;
        agent.set_queue_modes(steering_mode, follow_up_mode);
        {
            let steering_queue = Arc::clone(&message_queue);
            let follow_up_queue = Arc::clone(&message_queue);
            let injected_steering_queue = Arc::clone(&injected_queue);
            let injected_follow_up_queue = Arc::clone(&injected_queue);
            let steering_fetcher = move || -> BoxFuture<'static, Vec<ModelMessage>> {
                let steering_queue = Arc::clone(&steering_queue);
                let injected_steering_queue = Arc::clone(&injected_steering_queue);
                Box::pin(async move {
                    let mut out = Vec::new();
                    if let Ok(mut queue) = steering_queue.lock() {
                        out.extend(queue.pop_steering().into_iter().map(build_user_message));
                    }
                    if let Ok(mut queue) = injected_steering_queue.lock() {
                        out.extend(queue.pop_steering());
                    }
                    out
                })
            };
            let follow_up_fetcher = move || -> BoxFuture<'static, Vec<ModelMessage>> {
                let follow_up_queue = Arc::clone(&follow_up_queue);
                let injected_follow_up_queue = Arc::clone(&injected_follow_up_queue);
                Box::pin(async move {
                    let mut out = Vec::new();
                    if let Ok(mut queue) = follow_up_queue.lock() {
                        out.extend(queue.pop_follow_up().into_iter().map(build_user_message));
                    }
                    if let Ok(mut queue) = injected_follow_up_queue.lock() {
                        out.extend(queue.pop_follow_up());
                    }
                    out
                })
            };
            agent.set_message_fetchers(
                Some(Arc::new(steering_fetcher)),
                Some(Arc::new(follow_up_fetcher)),
            );
        }

        let keybindings = keybindings_override.unwrap_or_else(|| {
            // Load keybindings from user config (with defaults as fallback).
            let keybindings_result = KeyBindings::load_from_user_config();
            if keybindings_result.has_warnings() {
                tracing::warn!(
                    "Keybindings warnings: {}",
                    keybindings_result.format_warnings()
                );
            }
            keybindings_result.bindings
        });

        // Initialize autocomplete with catalog from resources
        let mut autocomplete_catalog = AutocompleteCatalog::from_resources(&resources);
        if let Some(manager) = &extensions {
            autocomplete_catalog.extension_commands = extension_commands_for_catalog(manager);
        }
        let mut autocomplete = AutocompleteState::new(cwd.clone(), autocomplete_catalog);
        autocomplete.max_visible = autocomplete_max_visible;

        let mut app = Self {
            input,
            history: HistoryList::new(),
            input_mode: InputMode::SingleLine,
            pending_inputs: VecDeque::from(pending_inputs),
            message_queue,
            conversation_viewport,
            spinner,
            agent_state: AgentState::Idle,
            term_width,
            term_height,
            editor_padding_x,
            messages,
            current_response: String::new(),
            current_thinking: String::new(),
            thinking_visible,
            tools_expanded: true,
            current_tool: None,
            tool_progress: None,
            pending_tool_output: None,
            session,
            config,
            theme,
            styles,
            markdown_style,
            resources,
            resource_cli,
            cwd,
            model_entry,
            model_entry_shared: model_entry_shared.clone(),
            model_scope,
            available_models,
            model,
            agent: Arc::new(Mutex::new(agent)),
            total_usage,
            event_tx,
            runtime_handle,
            extension_streaming: extension_streaming.clone(),
            extension_compacting: extension_compacting.clone(),
            extension_ui_queue: VecDeque::new(),
            active_extension_ui: None,
            status_message: None,
            save_enabled,
            abort_handle: None,
            bash_running: false,
            pending_oauth: None,
            extensions,
            keybindings,
            last_ctrlc_time: None,
            last_escape_time: None,
            autocomplete,
            session_picker: None,
            settings_ui: None,
            theme_picker: None,
            tree_ui: None,
            capability_prompt: None,
            branch_picker: None,
            model_selector: None,
        };

        if let Some(manager) = app.extensions.clone() {
            let session_handle = Arc::new(InteractiveExtensionSession {
                session: Arc::clone(&app.session),
                model_entry: model_entry_shared,
                is_streaming: extension_streaming,
                is_compacting: extension_compacting,
                config: app.config.clone(),
                save_enabled: app.save_enabled,
            });
            manager.set_session(session_handle);

            manager.set_host_actions(Arc::new(InteractiveExtensionHostActions {
                session: Arc::clone(&app.session),
                agent: Arc::clone(&app.agent),
                event_tx: app.event_tx.clone(),
                extension_streaming: Arc::clone(&app.extension_streaming),
                user_queue: Arc::clone(&app.message_queue),
                injected_queue,
            }));
        }

        app.scroll_to_bottom();
        app
    }

    #[must_use]
    pub fn session_handle(&self) -> Arc<Mutex<Session>> {
        Arc::clone(&self.session)
    }

    /// Get the current status message (for testing).
    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    /// Get a reference to the model selector overlay (for testing).
    pub const fn model_selector(&self) -> Option<&crate::model_selector::ModelSelectorOverlay> {
        self.model_selector.as_ref()
    }

    /// Check if the branch picker is currently open (for testing).
    pub const fn has_branch_picker(&self) -> bool {
        self.branch_picker.is_some()
    }

    /// Initialize the application.
    fn init(&self) -> Option<Cmd> {
        // Start text input cursor blink and spinner
        let test_mode = std::env::var_os("PI_TEST_MODE").is_some();
        let input_cmd = if test_mode {
            None
        } else {
            BubbleteaModel::init(&self.input)
        };
        let spinner_cmd = if test_mode {
            None
        } else {
            BubbleteaModel::init(&self.spinner)
        };
        let pending_cmd = if self.pending_inputs.is_empty() {
            None
        } else {
            Some(Cmd::new(|| Message::new(PiMsg::RunPending)))
        };

        // Batch commands
        batch(vec![input_cmd, spinner_cmd, pending_cmd])
    }

    /// Handle messages (keyboard input, async events, etc.).
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, msg: Message) -> Option<Cmd> {
        // Handle our custom Pi messages
        if let Some(pi_msg) = msg.downcast_ref::<PiMsg>() {
            return self.handle_pi_message(pi_msg.clone());
        }

        if let Some(size) = msg.downcast_ref::<WindowSizeMsg>() {
            self.set_terminal_size(size.width as usize, size.height as usize);
            return None;
        }

        // Handle keyboard input via keybindings layer
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            // Clear status message on any key press
            self.status_message = None;
            if key.key_type != KeyType::Esc {
                self.last_escape_time = None;
            }

            // /tree modal captures all input while active.
            if self.tree_ui.is_some() {
                return self.handle_tree_ui_key(key);
            }

            // Capability prompt modal captures all input while active.
            if self.capability_prompt.is_some() {
                return self.handle_capability_prompt_key(key);
            }

            // Branch picker modal captures all input while active.
            if self.branch_picker.is_some() {
                return self.handle_branch_picker_key(key);
            }

            // Model selector modal captures all input while active.
            if self.model_selector.is_some() {
                return self.handle_model_selector_key(key);
            }

            // Theme picker modal captures all input while active.
            if self.theme_picker.is_some() {
                let mut picker = self
                    .theme_picker
                    .take()
                    .expect("checked theme_picker is_some");
                match key.key_type {
                    KeyType::Up => picker.select_prev(),
                    KeyType::Down => picker.select_next(),
                    KeyType::Runes if key.runes == ['k'] => picker.select_prev(),
                    KeyType::Runes if key.runes == ['j'] => picker.select_next(),
                    KeyType::Enter => {
                        if let Some(item) = picker.selected_item() {
                            let loaded = match item {
                                ThemePickerItem::BuiltIn(name) => Ok(match *name {
                                    "light" => Theme::light(),
                                    "solarized" => Theme::solarized(),
                                    _ => Theme::dark(),
                                }),
                                ThemePickerItem::File(path) => Theme::load(path),
                            };

                            match loaded {
                                Ok(theme) => {
                                    let theme_name = theme.name.clone();
                                    self.apply_theme(theme);
                                    self.config.theme = Some(theme_name.clone());
                                    if let Err(e) = self.persist_project_theme(&theme_name) {
                                        self.status_message =
                                            Some(format!("Failed to persist theme: {e}"));
                                    } else {
                                        self.status_message =
                                            Some(format!("Switched to theme: {theme_name}"));
                                    }
                                }
                                Err(_) => {
                                    self.status_message =
                                        Some("Failed to load selected theme".to_string());
                                }
                            }
                        }
                        self.theme_picker = None;
                        return None;
                    }
                    KeyType::Esc => {
                        self.theme_picker = None;
                        self.settings_ui = Some(SettingsUiState::new());
                        return None;
                    }
                    KeyType::Runes if key.runes == ['q'] => {
                        self.theme_picker = None;
                        self.settings_ui = Some(SettingsUiState::new());
                        return None;
                    }
                    _ => {}
                }
                self.theme_picker = Some(picker);
                return None;
            }

            // /settings modal captures all input while active.
            if self.settings_ui.is_some() {
                let mut settings_ui = self
                    .settings_ui
                    .take()
                    .expect("checked settings_ui is_some");
                match key.key_type {
                    KeyType::Up => {
                        settings_ui.select_prev();
                        self.settings_ui = Some(settings_ui);
                        return None;
                    }
                    KeyType::Down => {
                        settings_ui.select_next();
                        self.settings_ui = Some(settings_ui);
                        return None;
                    }
                    KeyType::Runes if key.runes == ['k'] => {
                        settings_ui.select_prev();
                        self.settings_ui = Some(settings_ui);
                        return None;
                    }
                    KeyType::Runes if key.runes == ['j'] => {
                        settings_ui.select_next();
                        self.settings_ui = Some(settings_ui);
                        return None;
                    }
                    KeyType::Enter => {
                        if let Some(selected) = settings_ui.selected_entry() {
                            match selected {
                                SettingsUiEntry::Summary => {
                                    self.messages.push(ConversationMessage {
                                        role: MessageRole::System,
                                        content: self.format_settings_summary(),
                                        thinking: None,
                                        collapsed: false,
                                    });
                                    self.scroll_to_bottom();
                                    self.status_message =
                                        Some("Selected setting: Summary".to_string());
                                }
                                _ => {
                                    self.toggle_settings_entry(selected);
                                }
                            }
                        }
                        self.settings_ui = None;
                        return None;
                    }
                    KeyType::Esc => {
                        self.settings_ui = None;
                        self.status_message = Some("Settings cancelled".to_string());
                        return None;
                    }
                    KeyType::Runes if key.runes == ['q'] => {
                        self.settings_ui = None;
                        self.status_message = Some("Settings cancelled".to_string());
                        return None;
                    }
                    _ => {
                        self.settings_ui = Some(settings_ui);
                        return None;
                    }
                }
            }

            // Handle session picker navigation when overlay is open
            if let Some(ref mut picker) = self.session_picker {
                // If in delete confirmation mode, handle y/n/Esc/Enter
                if picker.confirm_delete {
                    match key.key_type {
                        KeyType::Runes if key.runes == ['y'] || key.runes == ['Y'] => {
                            picker.confirm_delete = false;
                            match picker.delete_selected() {
                                Ok(()) => {
                                    if picker.sessions.is_empty() {
                                        self.session_picker = None;
                                        self.status_message =
                                            Some("No sessions found for this project".to_string());
                                    } else {
                                        picker.status_message =
                                            Some("Session deleted.".to_string());
                                    }
                                }
                                Err(err) => {
                                    picker.status_message = Some(err.to_string());
                                }
                            }
                            return None;
                        }
                        KeyType::Runes if key.runes == ['n'] || key.runes == ['N'] => {
                            // Cancel delete
                            picker.confirm_delete = false;
                            picker.status_message = None;
                            return None;
                        }
                        KeyType::Esc => {
                            // Cancel delete
                            picker.confirm_delete = false;
                            picker.status_message = None;
                            return None;
                        }
                        _ => {
                            // Ignore other keys in confirmation mode
                            return None;
                        }
                    }
                }

                // Normal picker mode
                match key.key_type {
                    KeyType::Up => {
                        picker.select_prev();
                        return None;
                    }
                    KeyType::Down => {
                        picker.select_next();
                        return None;
                    }
                    KeyType::Runes if key.runes == ['k'] => {
                        picker.select_prev();
                        return None;
                    }
                    KeyType::Runes if key.runes == ['j'] => {
                        picker.select_next();
                        return None;
                    }
                    KeyType::Enter => {
                        // Load the selected session
                        if let Some(session_meta) = picker.selected_session().cloned() {
                            self.session_picker = None;
                            return self.load_session_from_path(&session_meta.path);
                        }
                        self.session_picker = None;
                        return None;
                    }
                    KeyType::CtrlD => {
                        picker.confirm_delete = true;
                        picker.status_message =
                            Some("Delete session? Press y/n to confirm.".to_string());
                        return None;
                    }
                    KeyType::Esc => {
                        self.session_picker = None;
                        return None;
                    }
                    KeyType::Runes if key.runes == ['q'] => {
                        self.session_picker = None;
                        return None;
                    }
                    _ => {
                        // Ignore other keys while picker is open
                        return None;
                    }
                }
            }

            // Handle autocomplete navigation when dropdown is open.
            //
            // IMPORTANT: Enter submits the current editor contents; Tab accepts autocomplete.
            if self.autocomplete.open {
                match key.key_type {
                    KeyType::Up => {
                        self.autocomplete.select_prev();
                        return None;
                    }
                    KeyType::Down => {
                        self.autocomplete.select_next();
                        return None;
                    }
                    KeyType::Tab => {
                        // Accept the selected item
                        if let Some(item) = self.autocomplete.selected_item().cloned() {
                            self.accept_autocomplete(&item);
                        }
                        self.autocomplete.close();
                        return None;
                    }
                    KeyType::Enter => {
                        // Close autocomplete and allow Enter to submit.
                        self.autocomplete.close();
                    }
                    KeyType::Esc => {
                        self.autocomplete.close();
                        return None;
                    }
                    _ => {
                        // Close autocomplete on other keys, then process normally
                        self.autocomplete.close();
                    }
                }
            }

            // Handle bracketed paste (drag/drop paths, etc.) before keybindings.
            if key.paste && self.handle_paste_event(key) {
                return None;
            }

            // Convert KeyMsg to KeyBinding and resolve action
            if let Some(binding) = KeyBinding::from_bubbletea_key(key) {
                let candidates = self.keybindings.matching_actions(&binding);
                if let Some(action) = self.resolve_action(&candidates) {
                    // Dispatch action based on current state
                    if let Some(cmd) = self.handle_action(action, key) {
                        return Some(cmd);
                    }
                    // Action was handled but returned None (no command needed)
                    // Check if we should suppress forwarding to text area
                    if self.should_consume_action(action) {
                        return None;
                    }
                }

                // Extension shortcuts: check if unhandled key matches an extension shortcut
                if self.agent_state == AgentState::Idle {
                    let key_id = binding.to_string().to_lowercase();
                    if let Some(manager) = &self.extensions {
                        if manager.has_shortcut(&key_id) {
                            return self.dispatch_extension_shortcut(&key_id);
                        }
                    }
                }
            }

            // Handle raw keys that don't map to actions but need special behavior
            // (e.g., text input handled by TextArea)
        }

        // Forward to appropriate component based on state
        if self.agent_state == AgentState::Idle {
            if let Some(key) = msg.downcast_ref::<KeyMsg>() {
                if key.key_type == KeyType::Space {
                    let mut key = key.clone();
                    key.key_type = KeyType::Runes;
                    key.runes = vec![' '];

                    let result = BubbleteaModel::update(&mut self.input, Message::new(key));
                    self.maybe_trigger_autocomplete();
                    return result;
                }
            }
            let result = BubbleteaModel::update(&mut self.input, msg);

            // After text area update, check if we should trigger autocomplete
            self.maybe_trigger_autocomplete();

            result
        } else {
            // While processing, forward to spinner
            self.spinner.update(msg)
        }
    }

    /// Render the view.
    #[allow(clippy::too_many_lines)]
    fn view(&self) -> String {
        let mut output = String::new();

        // Header
        output.push_str(&self.render_header());
        output.push('\n');

        // Modal overlays (e.g. /tree) take over the main view.
        if let Some(tree_ui) = &self.tree_ui {
            output.push_str(&Self::view_tree_ui(tree_ui, &self.styles));
            output.push_str(&self.render_footer());
            return output;
        }

        // Build conversation content for viewport
        let conversation_content = self.build_conversation_content();

        // Update viewport content (we can't mutate self in view, so we render with current offset)
        // The viewport will be updated in update() when new messages arrive
        let viewport_content = if conversation_content.is_empty() {
            if self.config.quiet_startup.unwrap_or(false) {
                String::new()
            } else {
                self.styles
                    .muted_italic
                    .render("  Welcome to Pi! Type a message to begin, or /help for commands.")
            }
        } else {
            conversation_content
        };

        // Render conversation area (scrollable).
        // Use the per-frame effective height so that conditional chrome
        // (scroll indicator, tool status, status message, …) is accounted
        // for and the total output never exceeds term_height rows.
        let effective_vp = self.view_effective_conversation_height();
        let conversation_lines: Vec<&str> = viewport_content.lines().collect();
        let start = self
            .conversation_viewport
            .y_offset()
            .min(conversation_lines.len().saturating_sub(1));
        let end = (start + effective_vp).min(conversation_lines.len());
        let visible_lines = conversation_lines.get(start..end).unwrap_or(&[]);
        output.push_str(&visible_lines.join("\n"));
        output.push('\n');

        // Scroll indicator
        if conversation_lines.len() > effective_vp {
            let total = conversation_lines.len().saturating_sub(effective_vp);
            let percent = (start * 100).checked_div(total).map_or(100, |p| p.min(100));
            let indicator = format!("  [{percent}%] ↑/↓ PgUp/PgDn to scroll");
            output.push_str(&self.styles.muted.render(&indicator));
            output.push('\n');
        }

        // Tool status
        if let Some(tool) = &self.current_tool {
            let progress_str = self.tool_progress.as_ref().map_or_else(String::new, |p| {
                let secs = p.elapsed_ms / 1000;
                if secs < 1 {
                    return String::new();
                }
                let mut parts = vec![format!("{secs}s")];
                if p.line_count > 0 {
                    parts.push(format!("{} lines", format_count(p.line_count)));
                } else if p.byte_count > 0 {
                    parts.push(format!("{} bytes", format_count(p.byte_count)));
                }
                if let Some(timeout_ms) = p.timeout_ms {
                    let timeout_s = timeout_ms / 1000;
                    if timeout_s > 0 {
                        parts.push(format!("timeout {timeout_s}s"));
                    }
                }
                format!(" ({})", parts.join(" \u{2022} "))
            });
            let _ = write!(
                output,
                "\n  {} {}{} ...\n",
                self.spinner.view(),
                self.styles.warning_bold.render(&format!("Running {tool}")),
                self.styles.muted.render(&progress_str),
            );
        }

        // Status message (slash command feedback)
        if let Some(status) = &self.status_message {
            let status_style = self.styles.accent.clone().italic();
            let _ = write!(output, "\n  {}\n", status_style.render(status));
        }

        // Session picker overlay (if open)
        if let Some(ref picker) = self.session_picker {
            output.push_str(&self.render_session_picker(picker));
        }

        // Settings overlay (if open)
        if let Some(ref settings_ui) = self.settings_ui {
            output.push_str(&self.render_settings_ui(settings_ui));
        }

        // Theme picker overlay (if open)
        if let Some(ref picker) = self.theme_picker {
            output.push_str(&self.render_theme_picker(picker));
        }

        // Capability prompt overlay (if open)
        if let Some(ref prompt) = self.capability_prompt {
            output.push_str(&self.render_capability_prompt(prompt));
        }

        // Branch picker overlay (if open)
        if let Some(ref picker) = self.branch_picker {
            output.push_str(&self.render_branch_picker(picker));
        }

        // Model selector overlay (if open)
        if let Some(ref selector) = self.model_selector {
            output.push_str(&self.render_model_selector(selector));
        }

        // Input area (only when idle and no overlay open)
        if self.agent_state == AgentState::Idle
            && self.session_picker.is_none()
            && self.settings_ui.is_none()
            && self.theme_picker.is_none()
            && self.capability_prompt.is_none()
            && self.branch_picker.is_none()
            && self.model_selector.is_none()
        {
            output.push_str(&self.render_input());

            // Autocomplete dropdown (if open)
            if self.autocomplete.open && !self.autocomplete.items.is_empty() {
                output.push_str(&self.render_autocomplete_dropdown());
            }
        } else if self.agent_state != AgentState::Idle {
            // Show spinner when processing
            let _ = write!(
                output,
                "\n  {} {}\n",
                self.spinner.view(),
                self.styles.accent.render("Processing...")
            );

            if let Some(pending_queue) = self.render_pending_message_queue() {
                output.push_str(&pending_queue);
            }
        }

        // Footer with usage stats
        output.push_str(&self.render_footer());

        // Clamp the output to `term_height` rows so the terminal never
        // scrolls in the alternate-screen buffer.
        let output = clamp_to_terminal_height(output, self.term_height);
        normalize_raw_terminal_newlines(output)
    }

    fn handle_capability_prompt_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let prompt = self.capability_prompt.as_mut()?;

        match key.key_type {
            // Navigate between buttons.
            KeyType::Right | KeyType::Tab => prompt.focus_next(),
            KeyType::Left => prompt.focus_prev(),
            KeyType::Runes if key.runes == ['l'] => prompt.focus_next(),
            KeyType::Runes if key.runes == ['h'] => prompt.focus_prev(),

            // Confirm selection.
            KeyType::Enter => {
                let action = prompt.selected_action();
                let response = ExtensionUiResponse {
                    id: prompt.request.id.clone(),
                    value: Some(Value::Bool(action.is_allow())),
                    cancelled: false,
                };
                // Record persistent decisions for "Always" choices.
                if action.is_persistent() {
                    if let Ok(mut store) = crate::permissions::PermissionStore::open_default() {
                        let _ = store.record(
                            &prompt.extension_id,
                            &prompt.capability,
                            action.is_allow(),
                        );
                    }
                }
                self.capability_prompt = None;
                self.send_extension_ui_response(response);
            }

            // Escape = deny once.
            KeyType::Esc => {
                let response = ExtensionUiResponse {
                    id: prompt.request.id.clone(),
                    value: Some(Value::Bool(false)),
                    cancelled: true,
                };
                self.capability_prompt = None;
                self.send_extension_ui_response(response);
            }

            _ => {}
        }

        None
    }

    /// Handle keyboard input when the branch picker overlay is active.
    pub fn handle_branch_picker_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let picker = self.branch_picker.as_mut()?;

        match key.key_type {
            KeyType::Up => picker.select_prev(),
            KeyType::Down => picker.select_next(),
            KeyType::Runes if key.runes == ['k'] => picker.select_prev(),
            KeyType::Runes if key.runes == ['j'] => picker.select_next(),
            KeyType::Enter => {
                if let Some(branch) = picker.selected_branch().cloned() {
                    self.branch_picker = None;
                    return self.switch_to_branch_leaf(&branch.leaf_id);
                }
                self.branch_picker = None;
            }
            KeyType::Esc | KeyType::CtrlC => {
                self.branch_picker = None;
                self.status_message = Some("Branch picker cancelled".to_string());
            }
            KeyType::Runes if key.runes == ['q'] => {
                self.branch_picker = None;
            }
            _ => {} // consume all other input while picker is open
        }

        None
    }

    /// Render the branch picker overlay.
    fn render_branch_picker(&self, picker: &BranchPickerOverlay) -> String {
        use std::fmt::Write;
        let mut output = String::new();

        let _ = writeln!(
            output,
            "\n  {}",
            self.styles.title.render("Select a branch")
        );
        let _ = writeln!(
            output,
            "  {}",
            self.styles
                .muted
                .render("-------------------------------------------")
        );

        if picker.branches.is_empty() {
            let _ = writeln!(
                output,
                "  {}",
                self.styles.muted_italic.render("No branches found.")
            );
        } else {
            let offset = picker.scroll_offset();
            let visible_count = picker.max_visible.min(picker.branches.len());
            let end = (offset + visible_count).min(picker.branches.len());

            for (idx, branch) in picker.branches[offset..end].iter().enumerate() {
                let global_idx = offset + idx;
                let is_selected = global_idx == picker.selected;
                let prefix = if is_selected { ">" } else { " " };

                let current_marker = if branch.is_current { " *" } else { "" };
                let msg_count = format!("({} msgs)", branch.message_count);
                let preview = if branch.preview.chars().count() > 40 {
                    let truncated: String = branch.preview.chars().take(37).collect();
                    format!("{truncated}...")
                } else {
                    branch.preview.clone()
                };

                let row = format!("{prefix} {preview:<42} {msg_count:>10}{current_marker}");
                let rendered = if is_selected {
                    self.styles.accent_bold.render(&row)
                } else if branch.is_current {
                    self.styles.accent.render(&row)
                } else {
                    self.styles.muted.render(&row)
                };
                let _ = writeln!(output, "  {rendered}");
            }
        }

        let _ = writeln!(
            output,
            "\n  {}",
            self.styles
                .muted_italic
                .render("\u{2191}/\u{2193}/j/k: navigate  Enter: switch  Esc: cancel  * = current")
        );
        output
    }

    /// Switch the active branch to a different leaf. Reloads the conversation.
    fn switch_to_branch_leaf(&mut self, leaf_id: &str) -> Option<Cmd> {
        let (session_id, old_leaf_id) = self
            .session
            .try_lock()
            .ok()
            .map(|g| (g.header.id.clone(), g.leaf_id.clone()))
            .unwrap_or_default();

        let pending = PendingTreeNavigation {
            session_id,
            old_leaf_id,
            selected_entry_id: leaf_id.to_string(),
            new_leaf_id: Some(leaf_id.to_string()),
            editor_text: None,
            entries_to_summarize: Vec::new(),
            summary_from_id: String::new(),
            api_key_present: false,
        };
        self.start_tree_navigation(pending, TreeSummaryChoice::NoSummary, None);
        None
    }

    /// Open the branch picker if the session has sibling branches.
    pub fn open_branch_picker(&mut self) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch branches while processing".to_string());
            return;
        }

        let branches = self
            .session
            .try_lock()
            .ok()
            .and_then(|guard| guard.sibling_branches().map(|(_, b)| b));

        match branches {
            Some(branches) if branches.len() > 1 => {
                self.branch_picker = Some(BranchPickerOverlay::new(branches));
            }
            _ => {
                self.status_message =
                    Some("No branches to pick (use /fork to create one)".to_string());
            }
        }
    }

    // ── Model selector (Ctrl+L) ────────────────────────────────────

    /// Open the model selector overlay.
    pub fn open_model_selector(&mut self) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch models while processing".to_string());
            return;
        }

        if self.available_models.is_empty() {
            self.status_message = Some("No models available".to_string());
            return;
        }

        self.model_selector = Some(crate::model_selector::ModelSelectorOverlay::new(
            &self.available_models,
        ));
    }

    /// Handle keyboard input while the model selector is open.
    pub fn handle_model_selector_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let selector = self.model_selector.as_mut()?;

        match key.key_type {
            KeyType::Up => selector.select_prev(),
            KeyType::Down => selector.select_next(),
            KeyType::Runes if key.runes == ['k'] => selector.select_prev(),
            KeyType::Runes if key.runes == ['j'] => selector.select_next(),
            KeyType::PgDown => selector.select_page_down(),
            KeyType::PgUp => selector.select_page_up(),
            KeyType::Backspace => selector.pop_char(),
            KeyType::Runes => selector.push_chars(key.runes.iter().copied()),
            KeyType::Enter => {
                let selected = selector.selected_item().cloned();
                self.model_selector = None;
                if let Some(selected) = selected {
                    self.apply_model_selection(&selected);
                } else {
                    self.status_message = Some("No model selected".to_string());
                }
                return None;
            }
            KeyType::Esc | KeyType::CtrlC => {
                self.model_selector = None;
                self.status_message = Some("Model selector cancelled".to_string());
            }
            _ => {} // consume all other input while selector is open
        }

        None
    }

    /// Apply a model selection from the model selector overlay.
    fn apply_model_selection(&mut self, selected: &crate::model_selector::ModelKey) {
        // Find the matching ModelEntry from available_models
        let entry = self
            .available_models
            .iter()
            .find(|e| {
                e.model.provider.eq_ignore_ascii_case(&selected.provider)
                    && e.model.id.eq_ignore_ascii_case(&selected.id)
            })
            .cloned();

        let Some(next) = entry else {
            self.status_message = Some(format!("Model {} not found", selected.full_id()));
            return;
        };

        if model_entry_matches(&next, &self.model_entry) {
            self.status_message = Some(format!("Already using {}", selected.full_id()));
            return;
        }

        let provider_impl = match providers::create_provider(&next, self.extensions.as_ref()) {
            Ok(p) => p,
            Err(err) => {
                self.status_message = Some(err.to_string());
                return;
            }
        };

        let Ok(mut agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            return;
        };
        agent_guard.set_provider(provider_impl);
        drop(agent_guard);

        let Ok(mut session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return;
        };
        session_guard.header.provider = Some(next.model.provider.clone());
        session_guard.header.model_id = Some(next.model.id.clone());
        session_guard.append_model_change(next.model.provider.clone(), next.model.id.clone());
        drop(session_guard);
        self.spawn_save_session();

        self.model_entry = next.clone();
        if let Ok(mut guard) = self.model_entry_shared.lock() {
            *guard = next;
        }
        self.model = format!(
            "{}/{}",
            self.model_entry.model.provider, self.model_entry.model.id
        );
        self.status_message = Some(format!("Switched model: {}", self.model));
    }

    /// Render the model selector overlay.
    fn render_model_selector(
        &self,
        selector: &crate::model_selector::ModelSelectorOverlay,
    ) -> String {
        use std::fmt::Write;
        let mut output = String::new();

        let _ = writeln!(output, "\n  {}", self.styles.title.render("Select a model"));

        // Search field
        let query = selector.query();
        let search_line = if query.is_empty() {
            "  > (type to filter)".to_string()
        } else {
            format!("  > {query}")
        };
        let _ = writeln!(output, "{}", self.styles.muted.render(&search_line));

        let _ = writeln!(
            output,
            "  {}",
            self.styles.muted.render("─".repeat(50).as_str())
        );

        if selector.filtered_len() == 0 {
            let _ = writeln!(
                output,
                "  {}",
                self.styles.muted_italic.render("No matching models.")
            );
        } else {
            let offset = selector.scroll_offset();
            let visible_count = selector.max_visible().min(selector.filtered_len());
            let end = (offset + visible_count).min(selector.filtered_len());

            let current_full = format!(
                "{}/{}",
                self.model_entry.model.provider, self.model_entry.model.id
            );

            for idx in offset..end {
                let is_selected = idx == selector.selected_index();
                let prefix = if is_selected { ">" } else { " " };

                if let Some(key) = selector.item_at(idx) {
                    let full = key.full_id();
                    let is_current = full.eq_ignore_ascii_case(&current_full);
                    let marker = if is_current { " *" } else { "" };
                    let row = format!("{prefix} {full}{marker}");
                    let rendered = if is_selected {
                        self.styles.accent_bold.render(&row)
                    } else if is_current {
                        self.styles.accent.render(&row)
                    } else {
                        self.styles.muted.render(&row)
                    };
                    let _ = writeln!(output, "  {rendered}");
                }
            }

            if selector.filtered_len() > visible_count {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}-{} of {})",
                        offset + 1,
                        end,
                        selector.filtered_len()
                    ))
                );
            }
        }

        let _ = writeln!(
            output,
            "\n  {}",
            self.styles
                .muted_italic
                .render("\u{2191}/\u{2193}/j/k: navigate  Enter: select  Esc: cancel  * = current")
        );
        output
    }

    /// Cycle to the next or previous sibling branch (Ctrl+Right / Ctrl+Left).
    pub fn cycle_sibling_branch(&mut self, forward: bool) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch branches while processing".to_string());
            return;
        }

        let target = self.session.try_lock().ok().and_then(|guard| {
            let (_, branches) = guard.sibling_branches()?;
            if branches.len() <= 1 {
                return None;
            }
            let current_idx = branches.iter().position(|b| b.is_current)?;
            let next_idx = if forward {
                (current_idx + 1) % branches.len()
            } else {
                current_idx.checked_sub(1).unwrap_or(branches.len() - 1)
            };
            Some(branches[next_idx].leaf_id.clone())
        });

        if let Some(leaf_id) = target {
            self.switch_to_branch_leaf(&leaf_id);
        } else {
            self.status_message = Some("No sibling branches (use /fork to create one)".to_string());
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_tree_ui_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let tree_ui = self.tree_ui.take()?;

        match tree_ui {
            TreeUiState::Selector(mut selector) => {
                match key.key_type {
                    KeyType::Up => selector.move_selection(-1),
                    KeyType::Down => selector.move_selection(1),
                    KeyType::CtrlU => {
                        selector.user_only = !selector.user_only;
                        if let Ok(session_guard) = self.session.try_lock() {
                            selector.rebuild(&session_guard);
                        }
                    }
                    KeyType::CtrlO => {
                        selector.show_all = !selector.show_all;
                        if let Ok(session_guard) = self.session.try_lock() {
                            selector.rebuild(&session_guard);
                        }
                    }
                    KeyType::Esc | KeyType::CtrlC => {
                        self.status_message = Some("Tree navigation cancelled".to_string());
                        self.tree_ui = None;
                        return None;
                    }
                    KeyType::Enter => {
                        if selector.rows.is_empty() {
                            self.tree_ui = None;
                            return None;
                        }

                        let selected = selector.rows[selector.selected].clone();
                        selector.last_selected_id = Some(selected.id.clone());

                        let (new_leaf_id, editor_text) = if let Some(text) = selected.resubmit_text
                        {
                            (selected.parent_id.clone(), Some(text))
                        } else {
                            (Some(selected.id.clone()), None)
                        };

                        // No-op if already at target leaf.
                        if selector.current_leaf_id.as_deref() == new_leaf_id.as_deref() {
                            self.status_message = Some("Already on that branch".to_string());
                            self.tree_ui = None;
                            return None;
                        }

                        let Ok(session_guard) = self.session.try_lock() else {
                            self.status_message = Some("Session busy; try again".to_string());
                            self.tree_ui = None;
                            return None;
                        };

                        let old_leaf_id = session_guard.leaf_id.clone();
                        let (entries_to_summarize, summary_from_id) = collect_tree_branch_entries(
                            &session_guard,
                            old_leaf_id.as_deref(),
                            new_leaf_id.as_deref(),
                        );
                        let session_id = session_guard.header.id.clone();
                        drop(session_guard);

                        let api_key_present = self.agent.try_lock().is_ok_and(|agent_guard| {
                            agent_guard.stream_options().api_key.is_some()
                        });

                        let pending = PendingTreeNavigation {
                            session_id,
                            old_leaf_id,
                            selected_entry_id: selected.id,
                            new_leaf_id,
                            editor_text,
                            entries_to_summarize,
                            summary_from_id,
                            api_key_present,
                        };

                        if pending.entries_to_summarize.is_empty() {
                            // Nothing to summarize; switch immediately.
                            self.start_tree_navigation(pending, TreeSummaryChoice::NoSummary, None);
                            return None;
                        }

                        self.tree_ui = Some(TreeUiState::SummaryPrompt(TreeSummaryPromptState {
                            pending,
                            selected: 0,
                        }));
                        return None;
                    }
                    _ => {}
                }

                self.tree_ui = Some(TreeUiState::Selector(selector));
            }
            TreeUiState::SummaryPrompt(mut prompt) => {
                match key.key_type {
                    KeyType::Up => {
                        if prompt.selected > 0 {
                            prompt.selected -= 1;
                        }
                    }
                    KeyType::Down => {
                        if prompt.selected < TreeSummaryChoice::all().len().saturating_sub(1) {
                            prompt.selected += 1;
                        }
                    }
                    KeyType::Esc | KeyType::CtrlC => {
                        self.status_message = Some("Tree navigation cancelled".to_string());
                        self.tree_ui = None;
                        return None;
                    }
                    KeyType::Enter => {
                        let choice = TreeSummaryChoice::all()[prompt.selected];
                        match choice {
                            TreeSummaryChoice::NoSummary | TreeSummaryChoice::Summarize => {
                                let pending = prompt.pending;
                                self.start_tree_navigation(pending, choice, None);
                                return None;
                            }
                            TreeSummaryChoice::SummarizeWithCustomPrompt => {
                                self.tree_ui =
                                    Some(TreeUiState::CustomPrompt(TreeCustomPromptState {
                                        pending: prompt.pending,
                                        instructions: String::new(),
                                    }));
                                return None;
                            }
                        }
                    }
                    _ => {}
                }
                self.tree_ui = Some(TreeUiState::SummaryPrompt(prompt));
            }
            TreeUiState::CustomPrompt(mut custom) => {
                match key.key_type {
                    KeyType::Esc | KeyType::CtrlC => {
                        self.tree_ui = Some(TreeUiState::SummaryPrompt(TreeSummaryPromptState {
                            pending: custom.pending,
                            selected: 2,
                        }));
                        return None;
                    }
                    KeyType::Backspace => {
                        custom.instructions.pop();
                    }
                    KeyType::Enter => {
                        let pending = custom.pending;
                        let instructions = if custom.instructions.trim().is_empty() {
                            None
                        } else {
                            Some(custom.instructions)
                        };
                        self.start_tree_navigation(
                            pending,
                            TreeSummaryChoice::SummarizeWithCustomPrompt,
                            instructions,
                        );
                        return None;
                    }
                    KeyType::Runes => {
                        for ch in key.runes.iter().copied() {
                            custom.instructions.push(ch);
                        }
                    }
                    _ => {}
                }
                self.tree_ui = Some(TreeUiState::CustomPrompt(custom));
            }
        }

        None
    }

    #[allow(clippy::too_many_lines)]
    fn start_tree_navigation(
        &mut self,
        pending: PendingTreeNavigation,
        choice: TreeSummaryChoice,
        custom_instructions: Option<String>,
    ) {
        let summary_requested = matches!(
            choice,
            TreeSummaryChoice::Summarize | TreeSummaryChoice::SummarizeWithCustomPrompt
        );

        // Fast path: no summary + no extensions. Keep it synchronous so unit tests can drive it
        // without running the async runtime.
        if !summary_requested && self.extensions.is_none() {
            let Ok(mut session_guard) = self.session.try_lock() else {
                self.status_message = Some("Session busy; try again".to_string());
                return;
            };

            if let Some(target_id) = &pending.new_leaf_id {
                if !session_guard.navigate_to(target_id) {
                    self.status_message = Some(format!("Branch target not found: {target_id}"));
                    return;
                }
            } else {
                session_guard.reset_leaf();
            }

            let (messages, usage) = conversation_from_session(&session_guard);
            let agent_messages = session_guard.to_messages_for_current_path();
            let status_leaf = pending
                .new_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());
            drop(session_guard);

            self.spawn_save_session();

            if let Ok(mut agent_guard) = self.agent.try_lock() {
                agent_guard.replace_messages(agent_messages);
            }

            self.messages = messages;
            self.total_usage = usage;
            self.current_response.clear();
            self.current_thinking.clear();
            self.agent_state = AgentState::Idle;
            self.current_tool = None;
            self.abort_handle = None;
            self.status_message = Some(format!("Switched to {status_leaf}"));
            self.scroll_to_bottom();

            if let Some(text) = pending.editor_text {
                self.input.set_value(&text);
            }
            self.input.focus();

            return;
        }

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let reserve_tokens = self.config.branch_summary_reserve_tokens();
        let runtime_handle = self.runtime_handle.clone();

        let Ok(agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            self.agent_state = AgentState::Idle;
            return;
        };
        let provider = agent_guard.provider();
        let key_opt = agent_guard.stream_options().api_key.clone();

        self.tree_ui = None;
        self.agent_state = AgentState::Processing;
        self.status_message = Some("Switching branches...".to_string());

        runtime_handle.spawn(async move {
            let cx = Cx::for_request();

            let from_id_for_event = pending
                .old_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());
            let to_id_for_event = pending
                .new_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());

            if let Some(manager) = extensions.clone() {
                let cancelled = manager
                    .dispatch_cancellable_event(
                        ExtensionEventName::SessionBeforeSwitch,
                        Some(json!({
                            "fromId": from_id_for_event.clone(),
                            "toId": to_id_for_event.clone(),
                            "sessionId": pending.session_id.clone(),
                        })),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(false);
                if cancelled {
                    let _ = event_tx.try_send(PiMsg::System(
                        "Session switch cancelled by extension".to_string(),
                    ));
                    return;
                }
            }

            let summary_skipped =
                summary_requested && key_opt.is_none() && !pending.entries_to_summarize.is_empty();
            let summary_text = if !summary_requested || pending.entries_to_summarize.is_empty() {
                None
            } else if let Some(key) = key_opt.as_deref() {
                match crate::compaction::summarize_entries(
                    &pending.entries_to_summarize,
                    provider,
                    key,
                    reserve_tokens,
                    custom_instructions.as_deref(),
                )
                .await
                {
                    Ok(summary) => summary,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Branch summary failed: {err}")));
                        return;
                    }
                }
            } else {
                None
            };

            let messages_for_agent = {
                let mut guard = match session.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                        return;
                    }
                };

                if let Some(target_id) = &pending.new_leaf_id {
                    if !guard.navigate_to(target_id) {
                        let _ = event_tx.try_send(PiMsg::AgentError(format!(
                            "Branch target not found: {target_id}"
                        )));
                        return;
                    }
                } else {
                    guard.reset_leaf();
                }

                if let Some(summary_text) = summary_text {
                    guard.append_branch_summary(
                        pending.summary_from_id.clone(),
                        summary_text,
                        None,
                        None,
                    );
                }

                let _ = guard.save().await;
                guard.to_messages_for_current_path()
            };

            {
                let mut agent_guard = match agent.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock agent: {err}")));
                        return;
                    }
                };
                agent_guard.replace_messages(messages_for_agent);
            }

            let (messages, usage) = {
                let guard = match session.lock(&cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                        return;
                    }
                };
                conversation_from_session(&guard)
            };

            let status = if summary_skipped {
                Some(format!(
                    "Switched to {to_id_for_event} (no summary: missing API key)"
                ))
            } else {
                Some(format!("Switched to {to_id_for_event}"))
            };

            let _ = event_tx.try_send(PiMsg::ConversationReset {
                messages,
                usage,
                status,
            });

            if let Some(text) = pending.editor_text {
                let _ = event_tx.try_send(PiMsg::SetEditorText(text));
            }

            if let Some(manager) = extensions {
                let _ = manager
                    .dispatch_event(
                        ExtensionEventName::SessionSwitch,
                        Some(json!({
                            "fromId": from_id_for_event,
                            "toId": to_id_for_event,
                            "sessionId": pending.session_id,
                        })),
                    )
                    .await;
            }
        });
    }

    fn view_tree_ui(tree_ui: &TreeUiState, styles: &TuiStyles) -> String {
        match tree_ui {
            TreeUiState::Selector(state) => Self::view_tree_selector(state, styles),
            TreeUiState::SummaryPrompt(state) => Self::view_tree_summary_prompt(state, styles),
            TreeUiState::CustomPrompt(state) => Self::view_tree_custom_prompt(state, styles),
        }
    }

    fn view_tree_selector(state: &TreeSelectorState, styles: &TuiStyles) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "  {}", styles.title.render("Session Tree"));

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

    /// Build the conversation content string for the viewport.
    #[allow(clippy::too_many_lines)]
    fn build_conversation_content(&self) -> String {
        let mut output = String::new();

        for msg in &self.messages {
            match msg.role {
                MessageRole::User => {
                    let _ = write!(
                        output,
                        "\n  {} {}\n",
                        self.styles.accent_bold.render("You:"),
                        msg.content
                    );
                }
                MessageRole::Assistant => {
                    let _ = write!(
                        output,
                        "\n  {}\n",
                        self.styles.success_bold.render("Assistant:")
                    );

                    // Render thinking if present
                    if self.thinking_visible {
                        if let Some(thinking) = &msg.thinking {
                            let truncated = truncate(thinking, 100);
                            let _ = writeln!(
                                output,
                                "  {}",
                                self.styles
                                    .muted_italic
                                    .render(&format!("Thinking: {truncated}"))
                            );
                        }
                    }

                    // Render markdown content
                    let rendered = MarkdownRenderer::new()
                        .with_style_config(self.markdown_style.clone())
                        .with_word_wrap(self.term_width.saturating_sub(6).max(40))
                        .render(&msg.content);
                    for line in rendered.lines() {
                        let _ = writeln!(output, "  {line}");
                    }
                }
                MessageRole::Tool => {
                    // Per-message collapse: global toggle overrides, then per-message.
                    let show_expanded = self.tools_expanded && !msg.collapsed;
                    if show_expanded {
                        let rendered = render_tool_message(&msg.content, &self.styles);
                        let _ = write!(output, "\n  {rendered}\n");
                    } else {
                        let header = msg.content.lines().next().unwrap_or("Tool output");
                        let line_count =
                            memchr::memchr_iter(b'\n', msg.content.as_bytes()).count() + 1;
                        let summary = format!(
                            "\u{25b6} {} ({line_count} lines, collapsed)",
                            header.trim_end()
                        );
                        let _ = write!(
                            output,
                            "\n  {}\n",
                            self.styles.muted_italic.render(&summary)
                        );
                        // Show preview when per-message collapsed (not global).
                        if self.tools_expanded && msg.collapsed {
                            for (i, line) in msg.content.lines().skip(1).enumerate() {
                                if i >= TOOL_COLLAPSE_PREVIEW_LINES {
                                    let remaining = line_count
                                        .saturating_sub(1)
                                        .saturating_sub(TOOL_COLLAPSE_PREVIEW_LINES);
                                    let _ = writeln!(
                                        output,
                                        "  {}",
                                        self.styles
                                            .muted
                                            .render(&format!("  ... {remaining} more lines"))
                                    );
                                    break;
                                }
                                let _ = writeln!(
                                    output,
                                    "  {}",
                                    self.styles.muted.render(&format!("  {line}"))
                                );
                            }
                        }
                    }
                }
                MessageRole::System => {
                    let _ = write!(output, "\n  {}\n", self.styles.warning.render(&msg.content));
                }
            }
        }

        // Add current streaming response
        if !self.current_response.is_empty() || !self.current_thinking.is_empty() {
            let _ = write!(
                output,
                "\n  {}\n",
                self.styles.success_bold.render("Assistant:")
            );

            // Show thinking if present
            if self.thinking_visible && !self.current_thinking.is_empty() {
                let truncated = truncate(&self.current_thinking, 100);
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles
                        .muted_italic
                        .render(&format!("Thinking: {truncated}"))
                );
            }

            // Show response (no markdown rendering while streaming)
            if !self.current_response.is_empty() {
                for line in self.current_response.lines() {
                    let _ = writeln!(output, "  {line}");
                }
            }
        }

        output
    }

    /// Handle custom Pi messages from the agent.
    #[allow(clippy::too_many_lines)]
    fn handle_pi_message(&mut self, msg: PiMsg) -> Option<Cmd> {
        match msg {
            PiMsg::AgentStart => {
                self.agent_state = AgentState::Processing;
                self.current_response.clear();
                self.current_thinking.clear();
                self.extension_streaming.store(true, Ordering::SeqCst);
            }
            PiMsg::RunPending => {
                return self.run_next_pending();
            }
            PiMsg::EnqueuePendingInput(input) => {
                self.pending_inputs.push_back(input);
                if self.agent_state == AgentState::Idle {
                    return self.run_next_pending();
                }
            }
            PiMsg::UiShutdown => {
                // Internal signal for shutting down the async→UI bridge; should not normally reach
                // the UI event loop, but handle it defensively.
            }
            PiMsg::TextDelta(text) => {
                self.current_response.push_str(&text);
            }
            PiMsg::ThinkingDelta(text) => {
                self.current_thinking.push_str(&text);
            }
            PiMsg::ToolStart { name, .. } => {
                self.agent_state = AgentState::ToolRunning;
                self.current_tool = Some(name);
                self.tool_progress = Some(ToolProgress::new());
                self.pending_tool_output = None;
            }
            PiMsg::ToolUpdate {
                name,
                content,
                details,
                ..
            } => {
                // Update progress metrics from details if present.
                if let Some(ref mut progress) = self.tool_progress {
                    progress.update_from_details(details.as_ref());
                } else {
                    let mut progress = ToolProgress::new();
                    progress.update_from_details(details.as_ref());
                    self.tool_progress = Some(progress);
                }
                if let Some(output) = format_tool_output(
                    &content,
                    details.as_ref(),
                    self.config.terminal_show_images(),
                ) {
                    self.pending_tool_output = Some(format!("Tool {name} output:\n{output}"));
                }
            }
            PiMsg::ToolEnd { .. } => {
                self.agent_state = AgentState::Processing;
                self.current_tool = None;
                self.tool_progress = None;
                if let Some(output) = self.pending_tool_output.take() {
                    self.messages.push(ConversationMessage::tool(output));
                    self.scroll_to_bottom();
                }
            }
            PiMsg::AgentDone {
                usage,
                stop_reason,
                error_message,
            } => {
                // Finalize the response
                let had_response = !self.current_response.is_empty();
                if had_response {
                    self.messages.push(ConversationMessage::new(
                        MessageRole::Assistant,
                        std::mem::take(&mut self.current_response),
                        if self.current_thinking.is_empty() {
                            None
                        } else {
                            Some(std::mem::take(&mut self.current_thinking))
                        },
                    ));
                }

                // Update usage
                if let Some(u) = usage {
                    self.total_usage.input += u.input;
                    self.total_usage.output += u.output;
                    self.total_usage.total_tokens += u.total_tokens;
                    self.total_usage.cost.total += u.cost.total;
                }

                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);

                if stop_reason == StopReason::Aborted {
                    self.status_message = Some("Request aborted".to_string());
                } else if stop_reason == StopReason::Error {
                    let message = error_message.unwrap_or_else(|| "Request failed".to_string());
                    self.status_message = Some(message.clone());
                    if !had_response {
                        self.messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content: format!("Error: {message}"),
                            thinking: None,
                            collapsed: false,
                        });
                    }
                }

                // Re-focus input
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::AgentError(error) => {
                self.current_response.clear();
                self.current_thinking.clear();
                let content = if error.contains('\n') || error.starts_with("Error:") {
                    error
                } else {
                    format!("Error: {error}")
                };
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content,
                    thinking: None,
                    collapsed: false,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::UpdateLastUserMessage(content) => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == MessageRole::User)
                {
                    message.content = content;
                }
                self.scroll_to_bottom();
            }
            PiMsg::System(message) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.extension_compacting.store(false, Ordering::SeqCst);
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::SystemNote(message) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            PiMsg::BashResult {
                display,
                content_for_agent,
            } => {
                self.bash_running = false;
                self.current_tool = None;
                self.agent_state = AgentState::Idle;

                if let Some(content) = content_for_agent {
                    self.scroll_to_bottom();
                    return self.submit_content(content);
                }

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: display,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::ConversationReset {
                messages,
                usage,
                status,
            } => {
                self.messages = messages;
                self.total_usage = usage;
                self.current_response.clear();
                self.current_thinking.clear();
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.status_message = status;
                self.scroll_to_bottom();
                self.input.focus();
            }
            PiMsg::SetEditorText(text) => {
                self.input.set_value(&text);
                self.input.focus();
            }
            PiMsg::ResourcesReloaded {
                resources,
                status,
                diagnostics,
            } => {
                let mut autocomplete_catalog = AutocompleteCatalog::from_resources(&resources);
                if let Some(manager) = &self.extensions {
                    autocomplete_catalog.extension_commands =
                        extension_commands_for_catalog(manager);
                }
                self.autocomplete.provider.set_catalog(autocomplete_catalog);
                self.autocomplete.close();
                self.resources = resources;
                self.apply_theme(Theme::resolve(&self.config, &self.cwd));
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.status_message = Some(status);
                if let Some(message) = diagnostics {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: message,
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_bottom();
                }
                self.input.focus();
            }
            PiMsg::ExtensionUiRequest(request) => {
                return self.handle_extension_ui_request(request);
            }
            PiMsg::ExtensionCommandDone {
                command: _,
                display,
                is_error: _,
            } => {
                self.agent_state = AgentState::Idle;
                self.current_tool = None;

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: display,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
        }
        None
    }

    fn handle_extension_ui_request(&mut self, request: ExtensionUiRequest) -> Option<Cmd> {
        // Capability-specific prompts get a dedicated modal overlay.
        if CapabilityPromptOverlay::is_capability_prompt(&request) {
            self.capability_prompt = Some(CapabilityPromptOverlay::from_request(request));
            return None;
        }
        if request.expects_response() {
            self.extension_ui_queue.push_back(request);
            self.advance_extension_ui_queue();
        } else {
            self.apply_extension_ui_effect(&request);
        }
        None
    }

    fn apply_extension_ui_effect(&mut self, request: &ExtensionUiRequest) {
        match request.method.as_str() {
            "notify" => {
                let title = request
                    .payload
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Notification");
                let message = request
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let level = request
                    .payload
                    .get("level")
                    .and_then(Value::as_str)
                    .or_else(|| request.payload.get("notifyType").and_then(Value::as_str))
                    .or_else(|| request.payload.get("notify_type").and_then(Value::as_str))
                    .unwrap_or("info");
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Extension notify ({level}): {title} {message}"),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            "setStatus" | "set_status" => {
                let status_text = request
                    .payload
                    .get("statusText")
                    .and_then(Value::as_str)
                    .or_else(|| request.payload.get("status_text").and_then(Value::as_str))
                    .or_else(|| request.payload.get("text").and_then(Value::as_str))
                    .unwrap_or("");
                if !status_text.is_empty() {
                    let status_key = request
                        .payload
                        .get("statusKey")
                        .and_then(Value::as_str)
                        .or_else(|| request.payload.get("status_key").and_then(Value::as_str))
                        .unwrap_or("");

                    self.status_message = Some(if status_key.is_empty() {
                        status_text.to_string()
                    } else {
                        format!("{status_key}: {status_text}")
                    });
                }
            }
            "setWidget" | "set_widget" => {
                let widget_key = request
                    .payload
                    .get("widgetKey")
                    .and_then(Value::as_str)
                    .or_else(|| request.payload.get("widget_key").and_then(Value::as_str))
                    .unwrap_or("widget");

                let content = request
                    .payload
                    .get("content")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| {
                        request
                            .payload
                            .get("widgetLines")
                            .or_else(|| request.payload.get("widget_lines"))
                            .or_else(|| request.payload.get("lines"))
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                    });

                if let Some(content) = content {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!("Extension widget ({widget_key}):\n{content}"),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_bottom();
                }
            }
            "setTitle" | "set_title" => {
                if let Some(title) = request.payload.get("title").and_then(Value::as_str) {
                    self.status_message = Some(format!("Title: {title}"));
                }
            }
            "set_editor_text" => {
                if let Some(text) = request.payload.get("text").and_then(Value::as_str) {
                    self.input.set_value(text);
                }
            }
            _ => {}
        }
    }

    fn send_extension_ui_response(&mut self, response: ExtensionUiResponse) {
        if let Some(manager) = &self.extensions {
            if !manager.respond_ui(response) {
                self.status_message = Some("No pending extension UI request".to_string());
            }
        } else {
            self.status_message = Some("Extensions are disabled".to_string());
        }
    }

    fn advance_extension_ui_queue(&mut self) {
        if self.active_extension_ui.is_some() {
            return;
        }
        if let Some(next) = self.extension_ui_queue.pop_front() {
            let prompt = format_extension_ui_prompt(&next);
            self.active_extension_ui = Some(next);
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: prompt,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            self.input.focus();
        }
    }

    fn dispatch_extension_command(&mut self, command: &str, args: &[String]) -> Option<Cmd> {
        let Some(manager) = &self.extensions else {
            self.status_message = Some("Extensions are disabled".to_string());
            return None;
        };

        let Some(runtime) = manager.js_runtime() else {
            self.status_message = Some(format!(
                "Extension command '/{command}' is not available (runtime not enabled)"
            ));
            return None;
        };

        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some(format!("/{command}"));

        let command_name = command.to_string();
        let args_str = args.join(" ");
        let cwd = self.cwd.display().to_string();
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();

        let ctx_payload = serde_json::json!({
            "cwd": cwd,
            "hasUI": true,
        });

        let cmd_for_msg = command_name.clone();
        runtime_handle.spawn(async move {
            let result = runtime
                .execute_command(
                    command_name,
                    args_str,
                    ctx_payload,
                    crate::extensions::EXTENSION_EVENT_TIMEOUT_MS,
                )
                .await;

            match result {
                Ok(value) => {
                    let display = if value.is_null() || value == serde_json::Value::Null {
                        format!("/{cmd_for_msg} completed.")
                    } else if let Some(s) = value.as_str() {
                        s.to_string()
                    } else {
                        format!("/{cmd_for_msg} completed: {value}")
                    };
                    let _ = event_tx.try_send(PiMsg::ExtensionCommandDone {
                        command: cmd_for_msg,
                        display,
                        is_error: false,
                    });
                }
                Err(err) => {
                    let _ = event_tx.try_send(PiMsg::ExtensionCommandDone {
                        command: cmd_for_msg,
                        display: format!("Extension command error: {err}"),
                        is_error: true,
                    });
                }
            }
        });

        None
    }

    fn dispatch_extension_shortcut(&mut self, key_id: &str) -> Option<Cmd> {
        let Some(manager) = &self.extensions else {
            self.status_message = Some("Extensions are disabled".to_string());
            return None;
        };

        let Some(runtime) = manager.js_runtime() else {
            self.status_message =
                Some("Extension shortcut not available (runtime not enabled)".to_string());
            return None;
        };

        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some(format!("shortcut:{key_id}"));

        let key_id_owned = key_id.to_string();
        let cwd = self.cwd.display().to_string();
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();

        let ctx_payload = serde_json::json!({
            "cwd": cwd,
            "hasUI": true,
        });

        let key_for_msg = key_id_owned.clone();
        runtime_handle.spawn(async move {
            let result = runtime
                .execute_shortcut(
                    key_id_owned,
                    ctx_payload,
                    crate::extensions::EXTENSION_EVENT_TIMEOUT_MS,
                )
                .await;

            match result {
                Ok(_) => {
                    let display = format!("Shortcut [{key_for_msg}] executed.");
                    let _ = event_tx.try_send(PiMsg::ExtensionCommandDone {
                        command: key_for_msg,
                        display,
                        is_error: false,
                    });
                }
                Err(err) => {
                    let _ = event_tx.try_send(PiMsg::ExtensionCommandDone {
                        command: key_for_msg,
                        display: format!("Shortcut error: {err}"),
                        is_error: true,
                    });
                }
            }
        });

        None
    }

    fn run_next_pending(&mut self) -> Option<Cmd> {
        loop {
            if self.agent_state != AgentState::Idle {
                return None;
            }
            let next = self.pending_inputs.pop_front()?;

            let cmd = match next {
                PendingInput::Text(text) => self.submit_message(&text),
                PendingInput::Content(content) => self.submit_content(content),
            };

            if cmd.is_some() {
                return cmd;
            }
        }
    }

    fn queue_input(&mut self, kind: QueuedMessageKind) {
        let raw_text = self.input.value();
        let trimmed = raw_text.trim();
        if trimmed.is_empty() {
            self.status_message = Some("No input to queue".to_string());
            return;
        }

        if let Some((command, _args)) = parse_extension_command(trimmed) {
            if let Some(manager) = &self.extensions {
                if manager.has_command(&command) {
                    self.status_message = Some(format!(
                        "Extension command '/{command}' cannot be queued while busy"
                    ));
                    return;
                }
            }
        }

        let expanded = self.resources.expand_input(trimmed);

        // Track input history
        self.history.push(trimmed.to_string());

        if let Ok(mut queue) = self.message_queue.lock() {
            match kind {
                QueuedMessageKind::Steering => queue.push_steering(expanded),
                QueuedMessageKind::FollowUp => queue.push_follow_up(expanded),
            }
        }

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        let label = match kind {
            QueuedMessageKind::Steering => "steering",
            QueuedMessageKind::FollowUp => "follow-up",
        };
        self.status_message = Some(format!("Queued {label} message"));
    }

    fn restore_queued_messages_to_editor(&mut self, abort: bool) -> usize {
        let (steering, follow_up) = self
            .message_queue
            .lock()
            .map_or_else(|_| (Vec::new(), Vec::new()), |mut queue| queue.clear_all());
        let mut all = steering;
        all.extend(follow_up);
        if all.is_empty() {
            if abort {
                self.abort_agent();
            }
            return 0;
        }

        let queued_text = all.join("\n\n");
        let current_text = self.input.value();
        let combined = [queued_text, current_text]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.input.set_value(&combined);
        if combined.contains('\n') {
            self.input_mode = InputMode::MultiLine;
            self.set_input_height(6);
        }
        self.input.focus();

        if abort {
            self.abort_agent();
        }

        all.len()
    }

    fn abort_agent(&self) {
        if let Some(handle) = &self.abort_handle {
            handle.abort();
        }
    }

    #[allow(clippy::too_many_lines)]
    fn submit_content(&mut self, content: Vec<ContentBlock>) -> Option<Cmd> {
        let display = content_blocks_to_text(&content);
        self.submit_content_with_display(content, &display)
    }

    #[allow(clippy::too_many_lines)]
    fn submit_content_with_display(
        &mut self,
        content: Vec<ContentBlock>,
        display: &str,
    ) -> Option<Cmd> {
        if content.is_empty() {
            return None;
        }

        let display_owned = display.to_string();
        if !display_owned.trim().is_empty() {
            self.messages.push(ConversationMessage {
                role: MessageRole::User,
                content: display_owned.clone(),
                thinking: None,
                collapsed: false,
            });
        }

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        let content_for_agent = content;
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
        let runtime_handle = self.runtime_handle.clone();
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        let runtime_handle_for_task = runtime_handle.clone();
        runtime_handle.spawn(async move {
            let mut content_for_agent = content_for_agent;
            if let Some(manager) = extensions.clone() {
                let (text, images) = split_content_blocks_for_input(&content_for_agent);
                match dispatch_input_event(&manager, text, images).await {
                    Ok(InputEventOutcome::Continue { text, images }) => {
                        content_for_agent = build_content_blocks_for_input(&text, &images);
                        let updated = content_blocks_to_text(&content_for_agent);
                        if updated != display_owned {
                            let _ = event_tx.try_send(PiMsg::UpdateLastUserMessage(updated));
                        }
                    }
                    Ok(InputEventOutcome::Block { reason }) => {
                        let _ = event_tx
                            .try_send(PiMsg::UpdateLastUserMessage("[input blocked]".to_string()));
                        let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                        let _ = event_tx.try_send(PiMsg::AgentError(message));
                        return;
                    }
                    Err(err) => {
                        let _ = event_tx.try_send(PiMsg::AgentError(err.to_string()));
                        return;
                    }
                }
                let _ = manager
                    .dispatch_event(ExtensionEventName::BeforeAgentStart, None)
                    .await;
            }

            let cx = Cx::for_request();
            let mut agent_guard = match agent.lock(&cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to lock agent: {err}")));
                    return;
                }
            };
            let previous_len = agent_guard.messages().len();

            let event_sender = event_tx.clone();
            let extensions = extensions.clone();
            let runtime_handle = runtime_handle_for_task.clone();
            let result = agent_guard
                .run_with_content_with_abort(content_for_agent, Some(abort_signal), move |event| {
                    let extension_event = extension_event_from_agent(&event);
                    let mapped = match &event {
                        AgentEvent::AgentStart { .. } => Some(PiMsg::AgentStart),
                        AgentEvent::MessageUpdate {
                            assistant_message_event,
                            ..
                        } => match assistant_message_event.as_ref() {
                            AssistantMessageEvent::TextDelta { delta, .. } => {
                                Some(PiMsg::TextDelta(delta.clone()))
                            }
                            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                                Some(PiMsg::ThinkingDelta(delta.clone()))
                            }
                            _ => None,
                        },
                        AgentEvent::ToolExecutionStart {
                            tool_name,
                            tool_call_id,
                            ..
                        } => Some(PiMsg::ToolStart {
                            name: tool_name.clone(),
                            tool_id: tool_call_id.clone(),
                        }),
                        AgentEvent::ToolExecutionUpdate {
                            tool_name,
                            tool_call_id,
                            partial_result,
                            ..
                        } => Some(PiMsg::ToolUpdate {
                            name: tool_name.clone(),
                            tool_id: tool_call_id.clone(),
                            content: partial_result.content.clone(),
                            details: partial_result.details.clone(),
                        }),
                        AgentEvent::ToolExecutionEnd {
                            tool_name,
                            tool_call_id,
                            is_error,
                            ..
                        } => Some(PiMsg::ToolEnd {
                            name: tool_name.clone(),
                            tool_id: tool_call_id.clone(),
                            is_error: *is_error,
                        }),
                        AgentEvent::AgentEnd { messages, .. } => {
                            let last = last_assistant_message(messages);
                            let mut usage = Usage::default();
                            for message in messages {
                                if let ModelMessage::Assistant(assistant) = message {
                                    add_usage(&mut usage, &assistant.usage);
                                }
                            }
                            Some(PiMsg::AgentDone {
                                usage: Some(usage),
                                stop_reason: last
                                    .as_ref()
                                    .map_or(StopReason::Stop, |msg| msg.stop_reason),
                                error_message: last
                                    .as_ref()
                                    .and_then(|msg| msg.error_message.clone()),
                            })
                        }
                        _ => None,
                    };

                    if let Some(msg) = mapped {
                        let _ = event_sender.try_send(msg);
                    }

                    if let Some(manager) = &extensions {
                        if let Some((event_name, data)) = extension_event {
                            if !matches!(
                                event_name,
                                ExtensionEventName::AgentStart
                                    | ExtensionEventName::AgentEnd
                                    | ExtensionEventName::TurnStart
                                    | ExtensionEventName::TurnEnd
                            ) {
                                let manager = manager.clone();
                                let runtime_handle = runtime_handle.clone();
                                runtime_handle.spawn(async move {
                                    let _ = manager.dispatch_event(event_name, data).await;
                                });
                            }
                        }
                    }
                })
                .await;

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

            let mut session_guard = match session.lock(&cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                    return;
                }
            };
            for message in new_messages {
                session_guard.append_model_message(message);
            }
            let mut save_error = None;

            if save_enabled {
                if let Err(err) = session_guard.save().await {
                    save_error = Some(format!("Failed to save session: {err}"));
                }
            }
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = event_tx.try_send(PiMsg::AgentError(err));
            }

            if let Err(err) = result {
                let formatted = crate::error_hints::format_error_with_hints(&err);
                let _ = event_tx.try_send(PiMsg::AgentError(formatted));
            }
        });

        None
    }

    fn submit_bash_command(
        &mut self,
        raw_message: &str,
        command: String,
        exclude_from_context: bool,
    ) -> Option<Cmd> {
        if self.bash_running {
            self.status_message = Some("A bash command is already running.".to_string());
            return None;
        }

        self.bash_running = true;
        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some("bash".to_string());
        self.history.push(raw_message.to_string());

        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let cwd = self.cwd.clone();
        let shell_path = self.config.shell_path.clone();
        let command_prefix = self.config.shell_command_prefix.clone();
        let runtime_handle = self.runtime_handle.clone();

        runtime_handle.spawn(async move {
            let cx = Cx::for_request();
            let result = crate::tools::run_bash_command(
                &cwd,
                shell_path.as_deref(),
                command_prefix.as_deref(),
                &command,
                None,
                None,
            )
            .await;

            match result {
                Ok(result) => {
                    let display =
                        bash_execution_to_text(&command, &result.output, 0, false, false, None);

                    if exclude_from_context {
                        let mut extra = HashMap::new();
                        extra.insert("excludeFromContext".to_string(), Value::Bool(true));

                        let bash_message = SessionMessage::BashExecution {
                            command: command.clone(),
                            output: result.output.clone(),
                            exit_code: result.exit_code,
                            cancelled: Some(result.cancelled),
                            truncated: Some(result.truncated),
                            full_output_path: result.full_output_path.clone(),
                            timestamp: Some(Utc::now().timestamp_millis()),
                            extra,
                        };

                        if let Ok(mut session_guard) = session.lock(&cx).await {
                            session_guard.append_message(bash_message);
                            if save_enabled {
                                let _ = session_guard.save().await;
                            }
                        }

                        let mut display = display;
                        display.push_str("\n\n[Output excluded from model context]");
                        let _ = event_tx.try_send(PiMsg::BashResult {
                            display,
                            content_for_agent: None,
                        });
                    } else {
                        let content_for_agent =
                            vec![ContentBlock::Text(TextContent::new(display.clone()))];
                        let _ = event_tx.try_send(PiMsg::BashResult {
                            display,
                            content_for_agent: Some(content_for_agent),
                        });
                    }
                }
                Err(err) => {
                    let _ = event_tx.try_send(PiMsg::BashResult {
                        display: format!("Bash command failed: {err}"),
                        content_for_agent: None,
                    });
                }
            }
        });

        None
    }

    /// Submit a message to the agent.
    #[allow(clippy::too_many_lines)]
    fn submit_message(&mut self, message: &str) -> Option<Cmd> {
        let message = message.trim();
        if message.is_empty() {
            return None;
        }

        if let Some(active) = self.active_extension_ui.take() {
            match parse_extension_ui_response(&active, message) {
                Ok(response) => {
                    self.send_extension_ui_response(response);
                    self.advance_extension_ui_queue();
                }
                Err(err) => {
                    self.status_message = Some(err);
                    self.active_extension_ui = Some(active);
                }
            }
            self.input.reset();
            self.input.focus();
            return None;
        }

        if let Some(pending) = self.pending_oauth.take() {
            return self.submit_oauth_code(message, pending);
        }

        if let Some((command, exclude_from_context)) = parse_bash_command(message) {
            return self.submit_bash_command(message, command, exclude_from_context);
        }

        // Check for slash commands
        if let Some((cmd, args)) = SlashCommand::parse(message) {
            return self.handle_slash_command(cmd, args);
        }

        if let Some((command, args)) = parse_extension_command(message) {
            if let Some(manager) = &self.extensions {
                if manager.has_command(&command) {
                    return self.dispatch_extension_command(&command, &args);
                }
            }
        }

        let message_owned = message.to_string();
        let (message_without_refs, file_refs) = self.extract_file_references(&message_owned);
        let message_for_agent = if file_refs.is_empty() {
            self.resources.expand_input(&message_owned)
        } else {
            self.resources.expand_input(message_without_refs.trim())
        };

        if !file_refs.is_empty() {
            let auto_resize = self
                .config
                .images
                .as_ref()
                .and_then(|images| images.auto_resize)
                .unwrap_or(true);

            let processed = match process_file_arguments(&file_refs, &self.cwd, auto_resize) {
                Ok(processed) => processed,
                Err(err) => {
                    self.status_message = Some(err.to_string());
                    return None;
                }
            };

            let mut text = processed.text;
            if !message_for_agent.trim().is_empty() {
                text.push_str(&message_for_agent);
            }

            let mut content = Vec::new();
            if !text.trim().is_empty() {
                content.push(ContentBlock::Text(TextContent::new(text)));
            }
            for image in processed.images {
                content.push(ContentBlock::Image(image));
            }

            self.history.push(message_owned.clone());

            let display = content_blocks_to_text(&content);
            return self.submit_content_with_display(content, &display);
        }
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        // Add to history
        self.history.push(message_owned.clone());

        // Add user message to display
        self.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: message_for_agent.clone(),
            thinking: None,
            collapsed: false,
        });
        let displayed_message = message_for_agent.clone();

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        let runtime_handle = self.runtime_handle.clone();

        // Spawn async task to run the agent
        let runtime_handle_for_agent = runtime_handle.clone();
        runtime_handle.spawn(async move {
            let mut message_for_agent = message_for_agent;
            let mut input_images = Vec::new();
            if let Some(manager) = extensions.clone() {
                match dispatch_input_event(&manager, message_for_agent.clone(), Vec::new()).await {
                    Ok(InputEventOutcome::Continue { text, images }) => {
                        message_for_agent = text;
                        input_images = images;
                        if message_for_agent != displayed_message {
                            let _ = event_tx
                                .try_send(PiMsg::UpdateLastUserMessage(message_for_agent.clone()));
                        }
                    }
                    Ok(InputEventOutcome::Block { reason }) => {
                        let _ = event_tx
                            .try_send(PiMsg::UpdateLastUserMessage("[input blocked]".to_string()));
                        let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                        let _ = event_tx.try_send(PiMsg::AgentError(message));
                        return;
                    }
                    Err(err) => {
                        let _ = event_tx.try_send(PiMsg::AgentError(err.to_string()));
                        return;
                    }
                }
                let _ = manager
                    .dispatch_event(ExtensionEventName::BeforeAgentStart, None)
                    .await;
            }

            let cx = Cx::for_request();
            let mut agent_guard = match agent.lock(&cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to lock agent: {err}")));
                    return;
                }
            };
            let previous_len = agent_guard.messages().len();

            let event_sender = event_tx.clone();
            let extensions = extensions.clone();
            let result = if input_images.is_empty() {
                agent_guard
                    .run_with_abort(message_for_agent, Some(abort_signal), move |event| {
                        let extension_event = extension_event_from_agent(&event);
                        let mapped = match &event {
                            AgentEvent::AgentStart { .. } => Some(PiMsg::AgentStart),
                            AgentEvent::MessageUpdate {
                                assistant_message_event,
                                ..
                            } => match assistant_message_event.as_ref() {
                                AssistantMessageEvent::TextDelta { delta, .. } => {
                                    Some(PiMsg::TextDelta(delta.clone()))
                                }
                                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                                    Some(PiMsg::ThinkingDelta(delta.clone()))
                                }
                                _ => None,
                            },
                            AgentEvent::ToolExecutionStart {
                                tool_name,
                                tool_call_id,
                                ..
                            } => Some(PiMsg::ToolStart {
                                name: tool_name.clone(),
                                tool_id: tool_call_id.clone(),
                            }),
                            AgentEvent::ToolExecutionUpdate {
                                tool_name,
                                tool_call_id,
                                partial_result,
                                ..
                            } => Some(PiMsg::ToolUpdate {
                                name: tool_name.clone(),
                                tool_id: tool_call_id.clone(),
                                content: partial_result.content.clone(),
                                details: partial_result.details.clone(),
                            }),
                            AgentEvent::ToolExecutionEnd {
                                tool_name,
                                tool_call_id,
                                is_error,
                                ..
                            } => Some(PiMsg::ToolEnd {
                                name: tool_name.clone(),
                                tool_id: tool_call_id.clone(),
                                is_error: *is_error,
                            }),
                            AgentEvent::AgentEnd { messages, .. } => {
                                let last = last_assistant_message(messages);
                                let mut usage = Usage::default();
                                for message in messages {
                                    if let ModelMessage::Assistant(assistant) = message {
                                        add_usage(&mut usage, &assistant.usage);
                                    }
                                }
                                Some(PiMsg::AgentDone {
                                    usage: Some(usage),
                                    stop_reason: last
                                        .as_ref()
                                        .map_or(StopReason::Stop, |msg| msg.stop_reason),
                                    error_message: last
                                        .as_ref()
                                        .and_then(|msg| msg.error_message.clone()),
                                })
                            }
                            _ => None,
                        };

                        if let Some(msg) = mapped {
                            let _ = event_sender.try_send(msg);
                        }

                        if let Some(manager) = &extensions {
                            if let Some((event_name, data)) = extension_event {
                                if !matches!(
                                    event_name,
                                    ExtensionEventName::AgentStart
                                        | ExtensionEventName::AgentEnd
                                        | ExtensionEventName::TurnStart
                                        | ExtensionEventName::TurnEnd
                                ) {
                                    let manager = manager.clone();
                                    runtime_handle_for_agent.spawn(async move {
                                        let _ = manager.dispatch_event(event_name, data).await;
                                    });
                                }
                            }
                        }
                    })
                    .await
            } else {
                let content_for_agent =
                    build_content_blocks_for_input(&message_for_agent, &input_images);
                agent_guard
                    .run_with_content_with_abort(
                        content_for_agent,
                        Some(abort_signal),
                        move |event| {
                            let extension_event = extension_event_from_agent(&event);
                            let mapped = match &event {
                                AgentEvent::AgentStart { .. } => Some(PiMsg::AgentStart),
                                AgentEvent::MessageUpdate {
                                    assistant_message_event,
                                    ..
                                } => match assistant_message_event.as_ref() {
                                    AssistantMessageEvent::TextDelta { delta, .. } => {
                                        Some(PiMsg::TextDelta(delta.clone()))
                                    }
                                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                                        Some(PiMsg::ThinkingDelta(delta.clone()))
                                    }
                                    _ => None,
                                },
                                AgentEvent::ToolExecutionStart {
                                    tool_name,
                                    tool_call_id,
                                    ..
                                } => Some(PiMsg::ToolStart {
                                    name: tool_name.clone(),
                                    tool_id: tool_call_id.clone(),
                                }),
                                AgentEvent::ToolExecutionUpdate {
                                    tool_name,
                                    tool_call_id,
                                    partial_result,
                                    ..
                                } => Some(PiMsg::ToolUpdate {
                                    name: tool_name.clone(),
                                    tool_id: tool_call_id.clone(),
                                    content: partial_result.content.clone(),
                                    details: partial_result.details.clone(),
                                }),
                                AgentEvent::ToolExecutionEnd {
                                    tool_name,
                                    tool_call_id,
                                    is_error,
                                    ..
                                } => Some(PiMsg::ToolEnd {
                                    name: tool_name.clone(),
                                    tool_id: tool_call_id.clone(),
                                    is_error: *is_error,
                                }),
                                AgentEvent::AgentEnd { messages, .. } => {
                                    let last = last_assistant_message(messages);
                                    let mut usage = Usage::default();
                                    for message in messages {
                                        if let ModelMessage::Assistant(assistant) = message {
                                            add_usage(&mut usage, &assistant.usage);
                                        }
                                    }
                                    Some(PiMsg::AgentDone {
                                        usage: Some(usage),
                                        stop_reason: last
                                            .as_ref()
                                            .map_or(StopReason::Stop, |msg| msg.stop_reason),
                                        error_message: last
                                            .as_ref()
                                            .and_then(|msg| msg.error_message.clone()),
                                    })
                                }
                                _ => None,
                            };

                            if let Some(msg) = mapped {
                                let _ = event_sender.try_send(msg);
                            }

                            if let Some(manager) = &extensions {
                                if let Some((event_name, data)) = extension_event {
                                    if !matches!(
                                        event_name,
                                        ExtensionEventName::AgentStart
                                            | ExtensionEventName::AgentEnd
                                            | ExtensionEventName::TurnStart
                                            | ExtensionEventName::TurnEnd
                                    ) {
                                        let manager = manager.clone();
                                        runtime_handle_for_agent.spawn(async move {
                                            let _ = manager.dispatch_event(event_name, data).await;
                                        });
                                    }
                                }
                            }
                        },
                    )
                    .await
            };

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

            let mut session_guard = match session.lock(&cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    let _ = event_tx
                        .try_send(PiMsg::AgentError(format!("Failed to lock session: {err}")));
                    return;
                }
            };
            for message in new_messages {
                session_guard.append_model_message(message);
            }
            let mut save_error = None;

            if save_enabled {
                if let Err(err) = session_guard.save().await {
                    save_error = Some(format!("Failed to save session: {err}"));
                }
            }
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = event_tx.try_send(PiMsg::AgentError(err));
            }

            if let Err(err) = result {
                let _ = event_tx.try_send(PiMsg::AgentError(err.to_string()));
            }
        });

        None
    }

    fn submit_oauth_code(&mut self, code_input: &str, pending: PendingOAuth) -> Option<Cmd> {
        // Do not store OAuth codes in history or session.
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        self.agent_state = AgentState::Processing;
        self.scroll_to_bottom();

        let event_tx = self.event_tx.clone();
        let PendingOAuth {
            provider,
            verifier,
            oauth_config,
        } = pending;
        let code_input = code_input.to_string();

        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            let auth_path = crate::config::Config::auth_path();
            let mut auth = match crate::auth::AuthStorage::load_async(auth_path).await {
                Ok(a) => a,
                Err(e) => {
                    let _ = event_tx.try_send(PiMsg::AgentError(e.to_string()));
                    return;
                }
            };

            let credential = if provider == "anthropic" {
                Box::pin(crate::auth::complete_anthropic_oauth(
                    &code_input,
                    &verifier,
                ))
                .await
            } else if let Some(config) = &oauth_config {
                Box::pin(crate::auth::complete_extension_oauth(
                    config,
                    &code_input,
                    &verifier,
                ))
                .await
            } else {
                Err(crate::error::Error::auth(format!(
                    "OAuth provider not supported: {provider}"
                )))
            };

            let credential = match credential {
                Ok(c) => c,
                Err(e) => {
                    let _ = event_tx.try_send(PiMsg::AgentError(e.to_string()));
                    return;
                }
            };

            auth.set(provider.clone(), credential);
            if let Err(e) = auth.save_async().await {
                let _ = event_tx.try_send(PiMsg::AgentError(e.to_string()));
                return;
            }

            let _ = event_tx.try_send(PiMsg::System(format!(
                "OAuth login successful for {provider}. Credentials saved to auth.json."
            )));
        });

        None
    }

    /// Navigate to previous history entry.
    fn navigate_history_back(&mut self) {
        if !self.history.has_entries() {
            return;
        }

        self.history.cursor_up();
        self.apply_history_selection();
    }

    /// Navigate to next history entry.
    fn navigate_history_forward(&mut self) {
        // Avoid clearing the editor when the user hasn't entered history navigation.
        if self.history.cursor_is_empty() {
            return;
        }

        self.history.cursor_down();
        self.apply_history_selection();
    }

    fn apply_history_selection(&mut self) {
        let selected = self.history.selected_value();
        if selected.is_empty() {
            self.input.reset();
        } else {
            self.input.set_value(selected);
        }
    }

    /// Open external editor with current input text.
    ///
    /// Uses $VISUAL if set, otherwise $EDITOR, otherwise "vi".
    /// Supports editors with arguments like "code --wait" or "vim -u NONE".
    fn open_external_editor(&self) -> std::io::Result<String> {
        use std::io::Write;

        // Determine editor command
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());

        // Create temp file with current editor content
        let mut temp_file = tempfile::NamedTempFile::new()?;
        let current_text = self.input.value();
        temp_file.write_all(current_text.as_bytes())?;
        temp_file.flush()?;

        let temp_path = temp_file.path().to_path_buf();

        // Spawn editor via shell to handle EDITOR with arguments (e.g., "code --wait")
        // The shell properly handles quoting, arguments, and PATH lookup
        #[cfg(unix)]
        let status = std::process::Command::new("sh")
            .args(["-c", &format!("{editor} \"$1\"")])
            .arg("--") // separator for positional args
            .arg(&temp_path)
            .status()?;

        #[cfg(not(unix))]
        let status = std::process::Command::new("cmd")
            .args(["/c", &format!("{} \"{}\"", editor, temp_path.display())])
            .status()?;

        if !status.success() {
            return Err(std::io::Error::other(format!(
                "Editor exited with status: {status}"
            )));
        }

        // Read back the edited content
        let new_text = std::fs::read_to_string(&temp_path)?;
        Ok(new_text)
    }

    /// Format keyboard shortcuts for /hotkeys display.
    ///
    /// Groups actions by category and shows their key bindings.
    fn format_hotkeys(&self) -> String {
        use crate::keybindings::ActionCategory;
        use std::fmt::Write;

        let mut output = String::new();
        let _ = writeln!(output, "Keyboard Shortcuts");
        let _ = writeln!(output, "==================");
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "Config: {}",
            KeyBindings::user_config_path().display()
        );
        let _ = writeln!(output);

        for category in ActionCategory::all() {
            let actions: Vec<_> = self.keybindings.iter_category(*category).collect();

            // Skip empty categories
            if actions.iter().all(|(_, bindings)| bindings.is_empty()) {
                continue;
            }

            let _ = writeln!(output, "## {}", category.display_name());
            let _ = writeln!(output);

            for (action, bindings) in actions {
                if bindings.is_empty() {
                    continue;
                }

                // Format bindings as comma-separated list
                let keys: Vec<_> = bindings
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect();
                let keys_str = keys.join(", ");

                let _ = writeln!(output, "  {:20} {}", keys_str, action.display_name());
            }
            let _ = writeln!(output);
        }

        output
    }

    fn resolve_action(&self, candidates: &[AppAction]) -> Option<AppAction> {
        let &first = candidates.first()?;

        // Some bindings are ambiguous and depend on UI state.
        // Example: `ctrl+d` can mean "delete forward" while editing, but "exit" when the editor
        // is empty (legacy behavior).
        if candidates.contains(&AppAction::Exit)
            && self.agent_state == AgentState::Idle
            && self.input.value().is_empty()
        {
            return Some(AppAction::Exit);
        }

        Some(first)
    }

    fn handle_double_escape_action(&mut self) -> (bool, Option<Cmd>) {
        let now = std::time::Instant::now();
        if let Some(last_time) = self.last_escape_time {
            if now.duration_since(last_time) < std::time::Duration::from_millis(500) {
                self.last_escape_time = None;
                return (true, self.trigger_double_escape_action());
            }
        }
        self.last_escape_time = Some(now);
        (false, None)
    }

    fn trigger_double_escape_action(&mut self) -> Option<Cmd> {
        let raw_action = self
            .config
            .double_escape_action
            .as_deref()
            .unwrap_or("tree")
            .trim();
        let action = raw_action.to_ascii_lowercase();
        match action.as_str() {
            "tree" => self.handle_slash_command(SlashCommand::Tree, ""),
            "fork" => self.handle_slash_command(SlashCommand::Fork, ""),
            _ => {
                self.status_message = Some(format!(
                    "Unknown doubleEscapeAction: {raw_action} (expected tree or fork)"
                ));
                self.handle_slash_command(SlashCommand::Tree, "")
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn cycle_model(&mut self, delta: i32) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch models while processing".to_string());
            return;
        }

        let scope_configured = self
            .config
            .enabled_models
            .as_ref()
            .is_some_and(|patterns| !patterns.is_empty());
        let use_scope = scope_configured || !self.model_scope.is_empty();
        let mut candidates = if use_scope {
            self.model_scope.clone()
        } else {
            self.available_models.clone()
        };

        candidates.sort_by(|a, b| {
            let left = format!("{}/{}", a.model.provider, a.model.id);
            let right = format!("{}/{}", b.model.provider, b.model.id);
            left.cmp(&right)
        });
        candidates.dedup_by(|left, right| model_entry_matches(left, right));

        if candidates.is_empty() {
            self.status_message = Some(if use_scope {
                "No models in scope".to_string()
            } else {
                "No models available".to_string()
            });
            return;
        }

        let current_index = candidates
            .iter()
            .position(|entry| model_entry_matches(entry, &self.model_entry));

        let next_index = current_index.map_or_else(
            || {
                if delta >= 0 { 0 } else { candidates.len() - 1 }
            },
            |idx| {
                if delta >= 0 {
                    (idx + 1) % candidates.len()
                } else {
                    idx.checked_sub(1).unwrap_or(candidates.len() - 1)
                }
            },
        );

        let next = candidates[next_index].clone();

        if model_entry_matches(&next, &self.model_entry) {
            self.status_message = Some(if use_scope {
                "Only one model in scope".to_string()
            } else {
                "Only one model available".to_string()
            });
            return;
        }

        let provider_impl = match providers::create_provider(&next, self.extensions.as_ref()) {
            Ok(provider_impl) => provider_impl,
            Err(err) => {
                self.status_message = Some(err.to_string());
                return;
            }
        };

        let Ok(mut agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            return;
        };
        agent_guard.set_provider(provider_impl);
        drop(agent_guard);

        let Ok(mut session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return;
        };
        session_guard.header.provider = Some(next.model.provider.clone());
        session_guard.header.model_id = Some(next.model.id.clone());
        session_guard.append_model_change(next.model.provider.clone(), next.model.id.clone());
        drop(session_guard);
        self.spawn_save_session();

        self.model_entry = next.clone();
        if let Ok(mut guard) = self.model_entry_shared.lock() {
            *guard = next;
        }
        self.model = format!(
            "{}/{}",
            self.model_entry.model.provider, self.model_entry.model.id
        );
        self.status_message = Some(format!("Switched model: {}", self.model));
    }

    fn quit_cmd(&mut self) -> Cmd {
        if let Some(manager) = &self.extensions {
            manager.clear_ui_sender();
        }

        // Drop the async → bubbletea bridge sender so bubbletea can shut down cleanly.
        // Without this, bubbletea's external forwarder thread can block on `recv()` during quit.
        let _ = self.event_tx.try_send(PiMsg::UiShutdown);
        let (tx, _rx) = mpsc::channel::<PiMsg>(1);
        drop(std::mem::replace(&mut self.event_tx, tx));
        quit()
    }

    /// Handle an action dispatched from the keybindings layer.
    ///
    /// Returns `Some(Cmd)` if a command should be executed,
    /// `None` if the action was handled without a command.
    #[allow(clippy::too_many_lines)]
    fn handle_action(&mut self, action: AppAction, key: &KeyMsg) -> Option<Cmd> {
        match action {
            // =========================================================
            // Application actions
            // =========================================================
            AppAction::Interrupt => {
                // Escape: Abort if processing, otherwise context-dependent
                if self.agent_state != AgentState::Idle {
                    self.last_escape_time = None;
                    let restored = self.restore_queued_messages_to_editor(true);
                    if restored > 0 {
                        self.status_message = Some(format!(
                            "Restored {restored} queued message{}",
                            if restored == 1 { "" } else { "s" }
                        ));
                    } else {
                        self.status_message = Some("Aborting request...".to_string());
                    }
                    return None;
                }
                if key.key_type == KeyType::Esc {
                    let (triggered, cmd) = self.handle_double_escape_action();
                    if triggered {
                        return cmd;
                    }
                }
                // When idle, Escape exits multi-line mode (but does NOT quit)
                if key.key_type == KeyType::Esc && self.input_mode == InputMode::MultiLine {
                    self.input_mode = InputMode::SingleLine;
                    self.set_input_height(3);
                    self.status_message = Some("Single-line mode".to_string());
                }
                // Legacy behavior: Escape when idle does nothing (no quit)
                None
            }
            AppAction::Clear | AppAction::Copy => {
                // Ctrl+C: abort if processing, clear editor if has text, or quit on double-tap
                // Note: Copy and Clear both bound to Ctrl+C - Copy takes precedence in lookup
                // When selection is implemented, Copy should only trigger with active selection
                if self.agent_state != AgentState::Idle {
                    if let Some(handle) = &self.abort_handle {
                        handle.abort();
                    }
                    self.status_message = Some("Aborting request...".to_string());
                    return None;
                }

                // If editor has text, clear it
                let editor_text = self.input.value();
                if !editor_text.is_empty() {
                    self.input.reset();
                    self.last_ctrlc_time = Some(std::time::Instant::now());
                    self.status_message = Some("Input cleared".to_string());
                    return None;
                }

                // Editor is empty - check for double-tap to quit
                let now = std::time::Instant::now();
                if let Some(last_time) = self.last_ctrlc_time {
                    // Double-tap within 500ms quits
                    if now.duration_since(last_time) < std::time::Duration::from_millis(500) {
                        return Some(self.quit_cmd());
                    }
                }
                // Record this Ctrl+C and show hint
                self.last_ctrlc_time = Some(now);
                self.status_message = Some("Press Ctrl+C again to quit".to_string());
                None
            }
            AppAction::PasteImage => {
                if let Some(path) = Self::paste_image_from_clipboard() {
                    self.insert_file_ref_path(&path);
                    self.status_message = Some("Image attached".to_string());
                }
                None
            }
            AppAction::Exit => {
                // Ctrl+D: Exit only when editor is empty (legacy behavior)
                if self.agent_state == AgentState::Idle && self.input.value().is_empty() {
                    return Some(self.quit_cmd());
                }
                // Editor has text - don't consume, let TextArea handle as delete char forward
                None
            }
            AppAction::Suspend => {
                // Ctrl+Z: Suspend to background (Unix only)
                #[cfg(unix)]
                {
                    use std::process::Command;
                    // Send SIGTSTP to our process. When resumed via `fg`, status() returns
                    // and we show the resumed message.
                    let pid = std::process::id().to_string();
                    let _ = Command::new("kill").args(["-TSTP", &pid]).status();
                    self.status_message = Some("Resumed from background".to_string());
                }
                #[cfg(not(unix))]
                {
                    self.status_message =
                        Some("Suspend not supported on this platform".to_string());
                }
                None
            }
            AppAction::ExternalEditor => {
                // Ctrl+G: Open external editor with current input
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open editor while processing".to_string());
                    return None;
                }
                match self.open_external_editor() {
                    Ok(new_text) => {
                        self.input.set_value(&new_text);
                        self.status_message = Some("Editor content loaded".to_string());
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Editor error: {e}"));
                    }
                }
                None
            }

            // =========================================================
            // Models & thinking
            // =========================================================
            AppAction::CycleModelForward => {
                self.cycle_model(1);
                None
            }
            AppAction::CycleModelBackward => {
                self.cycle_model(-1);
                None
            }
            AppAction::SelectModel => {
                self.open_model_selector();
                None
            }

            // =========================================================
            // Text input actions
            // =========================================================
            AppAction::Submit => {
                // Enter: Submit when idle, queue steering when busy
                if self.agent_state != AgentState::Idle {
                    self.queue_input(QueuedMessageKind::Steering);
                    return None;
                }
                if self.input_mode == InputMode::MultiLine {
                    // In multi-line mode, Enter inserts a newline (Alt+Enter submits).
                    self.input.insert_rune('\n');
                    return None;
                }
                let value = self.input.value();
                if !value.trim().is_empty() {
                    return self.submit_message(value.trim());
                }
                // Don't consume - let TextArea handle Enter if needed
                None
            }
            AppAction::FollowUp => {
                // Alt+Enter: queue follow-up when busy. When idle, toggles multi-line mode if the
                // editor is empty; otherwise it submits like Enter.
                if self.agent_state != AgentState::Idle {
                    self.queue_input(QueuedMessageKind::FollowUp);
                    return None;
                }
                let value = self.input.value();
                if self.input_mode == InputMode::SingleLine && value.trim().is_empty() {
                    self.input_mode = InputMode::MultiLine;
                    self.set_input_height(6);
                    self.status_message = Some("Multi-line mode".to_string());
                    return None;
                }
                if !value.trim().is_empty() {
                    return self.submit_message(value.trim());
                }
                None
            }
            AppAction::NewLine => {
                self.input.insert_rune('\n');
                self.input_mode = InputMode::MultiLine;
                self.set_input_height(6);
                None
            }

            // =========================================================
            // Cursor movement (history navigation in single-line mode)
            // =========================================================
            AppAction::CursorUp => {
                if self.agent_state == AgentState::Idle && self.input_mode == InputMode::SingleLine
                {
                    self.navigate_history_back();
                }
                // In multi-line mode, let TextArea handle cursor movement
                None
            }
            AppAction::CursorDown => {
                if self.agent_state == AgentState::Idle && self.input_mode == InputMode::SingleLine
                {
                    self.navigate_history_forward();
                }
                None
            }

            // =========================================================
            // Viewport scrolling
            // =========================================================
            AppAction::PageUp => {
                self.conversation_viewport.page_up();
                None
            }
            AppAction::PageDown => {
                self.conversation_viewport.page_down();
                None
            }

            // =========================================================
            // Autocomplete
            // =========================================================
            AppAction::Tab => {
                if self.agent_state != AgentState::Idle || self.session_picker.is_some() {
                    return None;
                }

                let text = self.input.value();
                if text.trim().is_empty() {
                    self.autocomplete.close();
                    return None;
                }

                let cursor = self.input.cursor_byte_offset();
                let response = self.autocomplete.provider.suggest(&text, cursor);

                if response.items.is_empty() {
                    self.autocomplete.close();
                    return None;
                }

                if response.items.len() == 1
                    && response
                        .items
                        .first()
                        .is_some_and(|item| item.kind == AutocompleteItemKind::Path)
                {
                    let item = response.items[0].clone();
                    self.autocomplete.replace_range = response.replace;
                    self.accept_autocomplete(&item);
                    self.autocomplete.close();
                    return None;
                }

                self.autocomplete.open_with(response);
                None
            }

            // =========================================================
            // Message queue actions
            // =========================================================
            AppAction::Dequeue => {
                let restored = self.restore_queued_messages_to_editor(false);
                if restored == 0 {
                    self.status_message = Some("No queued messages to restore".to_string());
                } else {
                    self.status_message = Some(format!(
                        "Restored {restored} queued message{}",
                        if restored == 1 { "" } else { "s" }
                    ));
                }
                None
            }

            // =========================================================
            // Display actions
            // =========================================================
            AppAction::ToggleThinking => {
                self.thinking_visible = !self.thinking_visible;
                let content = self.build_conversation_content();
                self.conversation_viewport.set_content(&content);
                self.status_message = Some(if self.thinking_visible {
                    "Thinking shown".to_string()
                } else {
                    "Thinking hidden".to_string()
                });
                None
            }
            AppAction::ExpandTools => {
                self.tools_expanded = !self.tools_expanded;
                // When expanding globally, also reset per-message collapse for
                // all tools so they show expanded. When collapsing globally,
                // the global flag is enough (render checks both).
                if self.tools_expanded {
                    for msg in &mut self.messages {
                        if msg.role == MessageRole::Tool {
                            msg.collapsed = false;
                        }
                    }
                }
                let content = self.build_conversation_content();
                self.conversation_viewport.set_content(&content);
                self.status_message = Some(if self.tools_expanded {
                    "Tool output expanded".to_string()
                } else {
                    "Tool output collapsed".to_string()
                });
                None
            }

            // =========================================================
            // Branch navigation
            // =========================================================
            AppAction::BranchPicker => {
                self.open_branch_picker();
                None
            }
            AppAction::BranchNextSibling => {
                self.cycle_sibling_branch(true);
                None
            }
            AppAction::BranchPrevSibling => {
                self.cycle_sibling_branch(false);
                None
            }

            // =========================================================
            // Actions not yet implemented - let through to component
            // =========================================================
            _ => {
                // Many actions (editor operations, model cycling, etc.) will be
                // implemented in future PRs. For now, don't consume them.
                None
            }
        }
    }

    /// Determine if an action should be consumed (not forwarded to TextArea).
    ///
    /// Some actions need to be consumed even when `handle_action` returns `None`,
    /// to prevent the TextArea from also handling the key.
    fn should_consume_action(&self, action: AppAction) -> bool {
        match action {
            // History navigation and Submit consume in single-line mode (otherwise TextArea
            // handles arrow keys or inserts a newline on Enter)
            AppAction::CursorUp | AppAction::CursorDown => {
                self.agent_state == AgentState::Idle && self.input_mode == InputMode::SingleLine
            }

            // Exit (Ctrl+D) only consumed when editor is empty (otherwise deleteCharForward)
            AppAction::Exit => {
                self.agent_state == AgentState::Idle && self.input.value().is_empty()
            }

            // Viewport scrolling should always be consumed.
            // FollowUp (Alt+Enter) should be consumed so TextArea doesn't insert text.
            // NewLine is handled directly (Shift+Enter / Ctrl+Enter).
            // Interrupt/Clear/Copy are always consumed.
            // Suspend/ExternalEditor are always consumed.
            // Tab is consumed (autocomplete).
            AppAction::PageUp
            | AppAction::PageDown
            | AppAction::CycleModelForward
            | AppAction::CycleModelBackward
            | AppAction::ToggleThinking
            | AppAction::ExpandTools
            | AppAction::FollowUp
            | AppAction::NewLine
            | AppAction::Submit
            | AppAction::Dequeue
            | AppAction::Interrupt
            | AppAction::Clear
            | AppAction::Copy
            | AppAction::PasteImage
            | AppAction::Suspend
            | AppAction::ExternalEditor
            | AppAction::Tab
            | AppAction::BranchPicker
            | AppAction::BranchNextSibling
            | AppAction::BranchPrevSibling
            | AppAction::SelectModel => true,

            // Other actions pass through to TextArea
            _ => false,
        }
    }

    /// Handle a slash command.
    #[allow(clippy::too_many_lines)]
    fn handle_slash_command(&mut self, cmd: SlashCommand, args: &str) -> Option<Cmd> {
        // Clear input
        self.input.reset();

        match cmd {
            SlashCommand::Help => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: SlashCommand::help_text().to_string(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_last_match("Available commands:");
                None
            }
            SlashCommand::Login => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot login while processing".to_string());
                    return None;
                }

                let provider = if args.is_empty() {
                    self.model_entry.model.provider.clone()
                } else {
                    args.to_string()
                };
                let provider = provider.trim().to_ascii_lowercase();

                // Look up OAuth config: built-in (anthropic) or extension-registered.
                let oauth_result = if provider == "anthropic" {
                    crate::auth::start_anthropic_oauth().map(|info| (info, None))
                } else {
                    // Check extension providers for OAuth config.
                    let ext_oauth = self
                        .available_models
                        .iter()
                        .find(|m| m.model.provider == provider)
                        .and_then(|m| m.oauth_config.clone());
                    if let Some(config) = ext_oauth {
                        crate::auth::start_extension_oauth(&provider, &config)
                            .map(|info| (info, Some(config)))
                    } else {
                        self.status_message = Some(format!(
                            "OAuth login not supported for {provider} (no OAuth config)"
                        ));
                        return None;
                    }
                };

                match oauth_result {
                    Ok((info, ext_config)) => {
                        let mut message = format!(
                            "OAuth login: {}\n\nOpen this URL:\n{}\n",
                            info.provider, info.url
                        );
                        if let Some(instructions) = info.instructions {
                            message.push('\n');
                            message.push_str(&instructions);
                            message.push('\n');
                        }
                        message.push_str(
                            "\nPaste the callback URL or authorization code into Pi to continue.",
                        );

                        self.messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content: message,
                            thinking: None,
                            collapsed: false,
                        });
                        self.scroll_to_bottom();
                        self.pending_oauth = Some(PendingOAuth {
                            provider: info.provider,
                            verifier: info.verifier,
                            oauth_config: ext_config,
                        });
                        self.input_mode = InputMode::SingleLine;
                        self.set_input_height(3);
                        self.input.focus();
                        None
                    }
                    Err(err) => {
                        self.status_message = Some(format!("OAuth login failed: {err}"));
                        None
                    }
                }
            }
            SlashCommand::Logout => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot logout while processing".to_string());
                    return None;
                }

                let provider = if args.is_empty() {
                    self.model_entry.model.provider.clone()
                } else {
                    args.to_string()
                };
                let provider = provider.trim().to_ascii_lowercase();

                let auth_path = crate::config::Config::auth_path();
                match crate::auth::AuthStorage::load(auth_path) {
                    Ok(mut auth) => {
                        let removed = auth.remove(&provider);
                        if let Err(err) = auth.save() {
                            self.status_message = Some(err.to_string());
                            return None;
                        }
                        if removed {
                            self.status_message =
                                Some(format!("Removed OAuth credentials for {provider}."));
                        } else {
                            self.status_message =
                                Some(format!("No OAuth credentials for {provider}."));
                        }
                    }
                    Err(err) => {
                        self.status_message = Some(err.to_string());
                    }
                }
                None
            }
            SlashCommand::Clear => {
                self.messages.clear();
                self.current_response.clear();
                self.current_thinking.clear();
                self.current_tool = None;
                self.pending_tool_output = None;
                self.abort_handle = None;
                self.autocomplete.close();
                self.status_message = Some("Conversation cleared".to_string());
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Model => {
                if args.trim().is_empty() {
                    self.status_message = Some(format!("Current model: {}", self.model));
                    return None;
                }

                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot switch models while processing".to_string());
                    return None;
                }

                let pattern = args.trim();
                let pattern_lower = pattern.to_ascii_lowercase();

                let mut exact_matches = Vec::new();
                for entry in &self.available_models {
                    let full = format!("{}/{}", entry.model.provider, entry.model.id);
                    if full.eq_ignore_ascii_case(pattern)
                        || entry.model.id.eq_ignore_ascii_case(pattern)
                    {
                        exact_matches.push(entry.clone());
                    }
                }

                let mut matches = if exact_matches.is_empty() {
                    let mut fuzzy = Vec::new();
                    for entry in &self.available_models {
                        let full = format!("{}/{}", entry.model.provider, entry.model.id);
                        let full_lower = full.to_ascii_lowercase();
                        if full_lower.contains(&pattern_lower)
                            || entry.model.id.to_ascii_lowercase().contains(&pattern_lower)
                        {
                            fuzzy.push(entry.clone());
                        }
                    }
                    fuzzy
                } else {
                    exact_matches
                };

                matches.sort_by(|a, b| {
                    let left = format!("{}/{}", a.model.provider, a.model.id);
                    let right = format!("{}/{}", b.model.provider, b.model.id);
                    left.cmp(&right)
                });
                matches.dedup_by(|a, b| {
                    a.model.provider.eq_ignore_ascii_case(&b.model.provider)
                        && a.model.id.eq_ignore_ascii_case(&b.model.id)
                });

                if matches.is_empty() {
                    if let Some((provider, model_id)) = pattern.split_once('/') {
                        let provider = provider.trim().to_ascii_lowercase();
                        let model_id = model_id.trim();
                        if !provider.is_empty() && !model_id.is_empty() {
                            if let Some(entry) =
                                crate::models::ad_hoc_model_entry(&provider, model_id)
                            {
                                matches.push(entry);
                            }
                        }
                    }
                }

                if matches.is_empty() {
                    self.status_message = Some(format!("Model not found: {pattern}"));
                    return None;
                }
                if matches.len() > 1 {
                    let preview = matches
                        .iter()
                        .take(8)
                        .map(|m| format!("  - {}/{}", m.model.provider, m.model.id))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!(
                            "Ambiguous model pattern \"{pattern}\". Matches:\n{preview}\n\nUse /model provider/id for an exact match."
                        ),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_bottom();
                    return None;
                }

                let next = matches.into_iter().next().expect("matches is non-empty");

                let resolved_key_opt = if next.api_key.is_some() {
                    next.api_key.clone()
                } else {
                    let auth_path = crate::config::Config::auth_path();
                    crate::auth::AuthStorage::load(auth_path)
                        .ok()
                        .and_then(|auth| auth.resolve_api_key(&next.model.provider, None))
                };
                if resolved_key_opt.is_none() {
                    self.status_message = Some(format!(
                        "Missing API key for provider {}",
                        next.model.provider
                    ));
                    return None;
                }

                if next.model.provider == self.model_entry.model.provider
                    && next.model.id == self.model_entry.model.id
                {
                    self.status_message = Some(format!("Current model: {}", self.model));
                    return None;
                }

                let provider_impl =
                    match providers::create_provider(&next, self.extensions.as_ref()) {
                        Ok(provider_impl) => provider_impl,
                        Err(err) => {
                            self.status_message = Some(err.to_string());
                            return None;
                        }
                    };

                let Ok(mut agent_guard) = self.agent.try_lock() else {
                    self.status_message = Some("Agent busy; try again".to_string());
                    return None;
                };
                agent_guard.set_provider(provider_impl);
                agent_guard
                    .stream_options_mut()
                    .api_key
                    .clone_from(&resolved_key_opt);
                agent_guard
                    .stream_options_mut()
                    .headers
                    .clone_from(&next.headers);
                drop(agent_guard);

                let Ok(mut session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                session_guard.header.provider = Some(next.model.provider.clone());
                session_guard.header.model_id = Some(next.model.id.clone());
                session_guard
                    .append_model_change(next.model.provider.clone(), next.model.id.clone());
                drop(session_guard);
                self.spawn_save_session();

                if !self
                    .available_models
                    .iter()
                    .any(|entry| model_entry_matches(entry, &next))
                {
                    self.available_models.push(next.clone());
                }
                self.model_entry = next.clone();
                if let Ok(mut guard) = self.model_entry_shared.lock() {
                    *guard = next.clone();
                }
                self.model = format!("{}/{}", next.model.provider, next.model.id);

                self.status_message = Some(format!("Switched model: {}", self.model));
                None
            }
            SlashCommand::Thinking => {
                let value = args.trim();
                if value.is_empty() {
                    let current = self
                        .session
                        .try_lock()
                        .ok()
                        .and_then(|guard| guard.header.thinking_level.clone())
                        .unwrap_or_else(|| ThinkingLevel::Off.to_string());
                    self.status_message = Some(format!("Thinking level: {current}"));
                    return None;
                }

                let level: ThinkingLevel = match value.parse() {
                    Ok(level) => level,
                    Err(err) => {
                        self.status_message = Some(err);
                        return None;
                    }
                };

                let Ok(mut session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                session_guard.header.thinking_level = Some(level.to_string());
                session_guard.append_thinking_level_change(level.to_string());
                drop(session_guard);
                self.spawn_save_session();

                if let Ok(mut agent_guard) = self.agent.try_lock() {
                    agent_guard.stream_options_mut().thinking_level = Some(level);
                }

                self.status_message = Some(format!("Thinking level: {level}"));
                None
            }
            SlashCommand::ScopedModels => {
                let value = args.trim();
                if value.is_empty() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: self.format_scoped_models_status(),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_last_match("Scoped models");
                    return None;
                }

                if value.eq_ignore_ascii_case("clear") {
                    self.config.enabled_models = Some(Vec::new());
                    self.model_scope.clear();

                    let global_dir = Config::global_dir();
                    let patch = json!({ "enabled_models": [] });
                    if let Err(err) = Config::patch_settings_with_roots(
                        SettingsScope::Project,
                        &global_dir,
                        &self.cwd,
                        patch,
                    ) {
                        tracing::warn!("Failed to persist enabled_models: {err}");
                        self.status_message =
                            Some(format!("Scoped models cleared (not saved: {err})"));
                    } else {
                        self.status_message = Some("Scoped models cleared".to_string());
                    }
                    return None;
                }

                let patterns = parse_scoped_model_patterns(value);
                if patterns.is_empty() {
                    self.status_message =
                        Some("Usage: /scoped-models [patterns|clear]".to_string());
                    return None;
                }

                let resolved = match resolve_scoped_model_entries(&patterns, &self.available_models)
                {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        self.status_message = Some(err);
                        return None;
                    }
                };

                self.model_scope = resolved;
                self.config.enabled_models = Some(patterns.clone());

                let match_count = self.model_scope.len();
                let global_dir = Config::global_dir();
                let patch = json!({ "enabled_models": patterns });
                if let Err(err) = Config::patch_settings_with_roots(
                    SettingsScope::Project,
                    &global_dir,
                    &self.cwd,
                    patch,
                ) {
                    tracing::warn!("Failed to persist enabled_models: {err}");
                    self.status_message = Some(format!(
                        "Scoped models updated: {match_count} matched (not saved: {err})"
                    ));
                } else {
                    self.status_message =
                        Some(format!("Scoped models updated: {match_count} matched"));
                }
                None
            }
            SlashCommand::Exit => Some(self.quit_cmd()),
            SlashCommand::History => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: self.format_input_history(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_last_match("Input history");
                None
            }
            SlashCommand::Export => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot export while processing".to_string());
                    return None;
                }

                let (output_path, html) = {
                    let Ok(session_guard) = self.session.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let output_path = if args.trim().is_empty() {
                        self.default_export_path(&session_guard)
                    } else {
                        self.resolve_output_path(args)
                    };
                    let html = session_guard.to_html();
                    (output_path, html)
                };

                if let Some(parent) = output_path.parent() {
                    if !parent.as_os_str().is_empty() {
                        if let Err(err) = std::fs::create_dir_all(parent) {
                            self.status_message = Some(format!("Failed to create dir: {err}"));
                            return None;
                        }
                    }
                }
                if let Err(err) = std::fs::write(&output_path, html) {
                    self.status_message = Some(format!("Failed to write export: {err}"));
                    return None;
                }

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Exported HTML: {}", output_path.display()),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some(format!("Exported: {}", output_path.display()));
                None
            }
            SlashCommand::Session => {
                let Ok(session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                let info = self.format_session_info(&session_guard);
                drop(session_guard);
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: info,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Settings => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open settings while processing".to_string());
                    return None;
                }

                self.settings_ui = Some(SettingsUiState::new());
                self.session_picker = None;
                self.autocomplete.close();
                None
            }
            SlashCommand::Theme => {
                let name = args.trim();
                if name.is_empty() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: self.format_themes_list(),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_last_match("Available themes:");
                    return None;
                }

                let theme = if name.eq_ignore_ascii_case("dark") {
                    Theme::dark()
                } else if name.eq_ignore_ascii_case("light") {
                    Theme::light()
                } else if name.eq_ignore_ascii_case("solarized") {
                    Theme::solarized()
                } else {
                    match Theme::load_by_name(name, &self.cwd) {
                        Ok(theme) => theme,
                        Err(err) => {
                            self.status_message = Some(err.to_string());
                            return None;
                        }
                    }
                };

                let theme_name = theme.name.clone();
                self.apply_theme(theme);
                self.config.theme = Some(theme_name.clone());

                if let Err(err) = self.persist_project_theme(&theme_name) {
                    tracing::warn!("Failed to persist theme preference: {err}");
                    self.status_message = Some(format!(
                        "Switched to theme: {theme_name} (not saved: {err})"
                    ));
                } else {
                    self.status_message = Some(format!("Switched to theme: {theme_name}"));
                }

                None
            }
            SlashCommand::Resume => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot resume while processing".to_string());
                    return None;
                }

                let override_dir = self
                    .session
                    .try_lock()
                    .ok()
                    .and_then(|guard| guard.session_dir.clone());
                let base_dir = override_dir.clone().unwrap_or_else(Config::sessions_dir);
                let sessions = crate::session_picker::list_sessions_for_project(
                    &self.cwd,
                    override_dir.as_deref(),
                );
                if sessions.is_empty() {
                    self.status_message = Some("No sessions found for this project".to_string());
                    return None;
                }

                self.session_picker = Some(SessionPickerOverlay::new_with_root(
                    sessions,
                    Some(base_dir),
                ));
                self.autocomplete.close();
                None
            }
            SlashCommand::New => {
                if self.agent_state != AgentState::Idle {
                    self.status_message =
                        Some("Cannot start a new session while processing".to_string());
                    return None;
                }

                let Some(extensions) = self.extensions.clone() else {
                    let Ok(mut session_guard) = self.session.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let session_dir = session_guard.session_dir.clone();
                    *session_guard = Session::create_with_dir(session_dir);
                    session_guard.header.provider = Some(self.model_entry.model.provider.clone());
                    session_guard.header.model_id = Some(self.model_entry.model.id.clone());
                    session_guard.header.thinking_level = Some(ThinkingLevel::Off.to_string());
                    drop(session_guard);

                    if let Ok(mut agent_guard) = self.agent.try_lock() {
                        agent_guard.replace_messages(Vec::new());
                        agent_guard.stream_options_mut().thinking_level = Some(ThinkingLevel::Off);
                    }

                    self.messages.clear();
                    self.total_usage = Usage::default();
                    self.current_response.clear();
                    self.current_thinking.clear();
                    self.current_tool = None;
                    self.pending_tool_output = None;
                    self.abort_handle = None;
                    self.pending_oauth = None;
                    self.session_picker = None;
                    self.tree_ui = None;
                    self.autocomplete.close();

                    self.status_message = Some(format!(
                        "Started new session\nModel set to {}\nThinking level: off",
                        self.model
                    ));
                    self.scroll_to_bottom();
                    self.input.focus();
                    return None;
                };

                let model_provider = self.model_entry.model.provider.clone();
                let model_id = self.model_entry.model.id.clone();
                let model_label = self.model.clone();
                let event_tx = self.event_tx.clone();
                let session = Arc::clone(&self.session);
                let agent = Arc::clone(&self.agent);
                let runtime_handle = self.runtime_handle.clone();

                let previous_session_file = self
                    .session
                    .try_lock()
                    .ok()
                    .and_then(|guard| guard.path.as_ref().map(|p| p.display().to_string()));

                self.agent_state = AgentState::Processing;
                self.status_message = Some("Starting new session...".to_string());

                runtime_handle.spawn(async move {
                    let cx = Cx::for_request();

                    let cancelled = extensions
                        .dispatch_cancellable_event(
                            ExtensionEventName::SessionBeforeSwitch,
                            Some(json!({ "reason": "new" })),
                            EXTENSION_EVENT_TIMEOUT_MS,
                        )
                        .await
                        .unwrap_or(false);
                    if cancelled {
                        let _ = event_tx.try_send(PiMsg::System(
                            "Session switch cancelled by extension".to_string(),
                        ));
                        return;
                    }

                    let new_session_id = {
                        let mut guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        let session_dir = guard.session_dir.clone();
                        let mut new_session = Session::create_with_dir(session_dir);
                        new_session.header.provider = Some(model_provider);
                        new_session.header.model_id = Some(model_id);
                        new_session.header.thinking_level = Some(ThinkingLevel::Off.to_string());
                        let new_id = new_session.header.id.clone();
                        *guard = new_session;
                        new_id
                    };

                    {
                        let mut agent_guard = match agent.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock agent: {err}"
                                )));
                                return;
                            }
                        };
                        agent_guard.replace_messages(Vec::new());
                        agent_guard.stream_options_mut().thinking_level = Some(ThinkingLevel::Off);
                    }

                    let _ = event_tx.try_send(PiMsg::ConversationReset {
                        messages: Vec::new(),
                        usage: Usage::default(),
                        status: Some(format!(
                            "Started new session\nModel set to {model_label}\nThinking level: off"
                        )),
                    });

                    let _ = extensions
                        .dispatch_event(
                            ExtensionEventName::SessionSwitch,
                            Some(json!({
                                "reason": "new",
                                "previousSessionFile": previous_session_file,
                                "sessionId": new_session_id,
                            })),
                        )
                        .await;
                });

                None
            }
            SlashCommand::Copy => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot copy while processing".to_string());
                    return None;
                }

                let text = self
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && !m.content.trim().is_empty())
                    .map(|m| m.content.clone());

                let Some(text) = text else {
                    self.status_message = Some("No agent messages to copy yet.".to_string());
                    return None;
                };

                let write_fallback = |text: &str| -> std::io::Result<std::path::PathBuf> {
                    let dir = std::env::temp_dir();
                    let filename = format!("pi_copy_{}.txt", Utc::now().timestamp_millis());
                    let path = dir.join(filename);
                    std::fs::write(&path, text)?;
                    Ok(path)
                };

                #[cfg(feature = "clipboard")]
                {
                    match ClipboardProvider::new()
                        .and_then(|mut ctx: ClipboardContext| ctx.set_contents(text.clone()))
                    {
                        Ok(()) => self.status_message = Some("Copied to clipboard".to_string()),
                        Err(err) => match write_fallback(&text) {
                            Ok(path) => {
                                self.status_message = Some(format!(
                                    "Clipboard support is disabled or unavailable ({err}). Wrote to {}",
                                    path.display()
                                ));
                            }
                            Err(io_err) => {
                                self.status_message = Some(format!(
                                    "Clipboard support is disabled or unavailable ({err}); also failed to write fallback file: {io_err}"
                                ));
                            }
                        },
                    }
                }

                #[cfg(not(feature = "clipboard"))]
                {
                    match write_fallback(&text) {
                        Ok(path) => {
                            self.status_message = Some(format!(
                                "Clipboard support is disabled. Wrote to {}",
                                path.display()
                            ));
                        }
                        Err(err) => {
                            self.status_message = Some(format!(
                                "Clipboard support is disabled; failed to write fallback file: {err}"
                            ));
                        }
                    }
                }

                None
            }
            SlashCommand::Name => {
                let name = args.trim();
                if name.is_empty() {
                    self.status_message = Some("Usage: /name <name>".to_string());
                    return None;
                }

                let Ok(mut session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                session_guard.append_session_info(Some(name.to_string()));
                drop(session_guard);
                self.spawn_save_session();

                self.status_message = Some(format!("Session name: {name}"));
                None
            }
            SlashCommand::Hotkeys => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: self.format_hotkeys(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Changelog => {
                let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md");
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        self.messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content,
                            thinking: None,
                            collapsed: false,
                        });
                        self.scroll_to_last_match("# ");
                    }
                    Err(err) => {
                        self.status_message = Some(format!(
                            "Failed to read changelog {}: {err}",
                            path.display()
                        ));
                    }
                }
                None
            }
            SlashCommand::Tree => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open tree while processing".to_string());
                    return None;
                }

                let Ok(session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                let initial_selected_id = resolve_tree_selector_initial_id(&session_guard, args);
                let selector = TreeSelectorState::new(
                    &session_guard,
                    self.term_height,
                    initial_selected_id.as_deref(),
                );
                drop(session_guard);
                self.tree_ui = Some(TreeUiState::Selector(selector));
                None
            }
            SlashCommand::Fork => {
                if self.agent_state != AgentState::Idle {
                    self.status_message =
                        Some("Cannot fork while processing a request".to_string());
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
                        self.status_message =
                            Some(format!("No user message id matches \"{args}\""));
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
                    let cx = Cx::for_request();
                    if let Some(manager) = extensions.clone() {
                        let cancelled = manager
                            .dispatch_cancellable_event(
                                ExtensionEventName::SessionBeforeFork,
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
                            let _ = event_tx
                                .try_send(PiMsg::System("Fork cancelled by extension".to_string()));
                            return;
                        }
                    }

                    let (fork_plan, parent_path, session_dir) = {
                        let guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        let fork_plan = match guard.plan_fork_from_user_message(&selection.id) {
                            Ok(plan) => plan,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to build fork: {err}"
                                )));
                                return;
                            }
                        };
                        let parent_path = guard.path.as_ref().map(|p| p.display().to_string());
                        let session_dir = guard.session_dir.clone();
                        drop(guard);
                        (fork_plan, parent_path, session_dir)
                    };

                    let crate::session::ForkPlan {
                        entries,
                        leaf_id,
                        selected_text,
                    } = fork_plan;

                    let mut new_session = Session::create_with_dir(session_dir);
                    new_session.header.provider = Some(model_provider);
                    new_session.header.model_id = Some(model_id);
                    new_session.header.thinking_level = thinking_level;
                    if let Some(parent_path) = parent_path {
                        new_session.set_branched_from(Some(parent_path));
                    }
                    new_session.entries = entries;
                    new_session.leaf_id = leaf_id;
                    new_session.ensure_entry_ids();
                    let new_session_id = new_session.header.id.clone();

                    if let Err(err) = new_session.save().await {
                        let _ = event_tx
                            .try_send(PiMsg::AgentError(format!("Failed to save fork: {err}")));
                        return;
                    }

                    let messages_for_agent = new_session.to_messages_for_current_path();
                    {
                        let mut agent_guard = match agent.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock agent: {err}"
                                )));
                                return;
                            }
                        };
                        agent_guard.replace_messages(messages_for_agent);
                    }

                    {
                        let mut guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        *guard = new_session;
                    }

                    let (messages, usage) = {
                        let guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        conversation_from_session(&guard)
                    };

                    let _ = event_tx.try_send(PiMsg::ConversationReset {
                        messages,
                        usage,
                        status: Some(format!("Forked new session from {}", selection.summary)),
                    });

                    let _ = event_tx.try_send(PiMsg::SetEditorText(selected_text));

                    if let Some(manager) = extensions {
                        let _ = manager
                            .dispatch_event(
                                ExtensionEventName::SessionFork,
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
            SlashCommand::Compact => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot compact while processing".to_string());
                    return None;
                }

                let Ok(agent_guard) = self.agent.try_lock() else {
                    self.status_message = Some("Agent busy; try again".to_string());
                    return None;
                };
                let provider = agent_guard.provider();
                let api_key_opt = agent_guard.stream_options().api_key.clone();
                drop(agent_guard);

                let Some(api_key) = api_key_opt else {
                    self.status_message =
                        Some("No API key configured; cannot run compaction".to_string());
                    return None;
                };

                let event_tx = self.event_tx.clone();
                let session = Arc::clone(&self.session);
                let agent = Arc::clone(&self.agent);
                let extensions = self.extensions.clone();
                let runtime_handle = self.runtime_handle.clone();
                let reserve_tokens = self.config.compaction_reserve_tokens();
                let keep_recent_tokens = self.config.compaction_keep_recent_tokens();
                let custom_instructions = args.trim().to_string();
                let custom_instructions = if custom_instructions.is_empty() {
                    None
                } else {
                    Some(custom_instructions)
                };
                let is_compacting = Arc::clone(&self.extension_compacting);

                self.agent_state = AgentState::Processing;
                self.status_message = Some("Compacting session...".to_string());
                self.extension_compacting.store(true, Ordering::SeqCst);

                runtime_handle.spawn(async move {
                    let cx = Cx::for_request();

                    let (session_id, path_entries) = {
                        let mut guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                is_compacting.store(false, Ordering::SeqCst);
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        guard.ensure_entry_ids();
                        let session_id = guard.header.id.clone();
                        let entries = guard
                            .entries_for_current_path()
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>();
                        (session_id, entries)
                    };

                    if let Some(manager) = extensions.clone() {
                        let cancelled = manager
                            .dispatch_cancellable_event(
                                ExtensionEventName::SessionBeforeCompact,
                                Some(json!({
                                    "sessionId": session_id,
                                    "notes": custom_instructions.as_deref(),
                                })),
                                EXTENSION_EVENT_TIMEOUT_MS,
                            )
                            .await
                            .unwrap_or(false);
                        if cancelled {
                            is_compacting.store(false, Ordering::SeqCst);
                            let _ = event_tx.try_send(PiMsg::System(
                                "Compaction cancelled by extension".to_string(),
                            ));
                            return;
                        }
                    }

                    let settings = crate::compaction::ResolvedCompactionSettings {
                        enabled: true,
                        reserve_tokens,
                        keep_recent_tokens,
                    };
                    let Some(prep) = crate::compaction::prepare_compaction(&path_entries, settings)
                    else {
                        is_compacting.store(false, Ordering::SeqCst);
                        let _ = event_tx.try_send(PiMsg::System(
                            "Nothing to compact (already compacted or too little history)"
                                .to_string(),
                        ));
                        return;
                    };

                    let result = match crate::compaction::compact(
                        prep,
                        Arc::clone(&provider),
                        &api_key,
                        custom_instructions.as_deref(),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(err) => {
                            is_compacting.store(false, Ordering::SeqCst);
                            let _ = event_tx
                                .try_send(PiMsg::AgentError(format!("Compaction failed: {err}")));
                            return;
                        }
                    };

                    let details =
                        crate::compaction::compaction_details_to_value(&result.details).ok();

                    let messages_for_agent = {
                        let mut guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                is_compacting.store(false, Ordering::SeqCst);
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };

                        guard.append_compaction(
                            result.summary.clone(),
                            result.first_kept_entry_id.clone(),
                            result.tokens_before,
                            details,
                            None,
                        );
                        let _ = guard.save().await;
                        guard.to_messages_for_current_path()
                    };

                    {
                        let mut agent_guard = match agent.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                is_compacting.store(false, Ordering::SeqCst);
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock agent: {err}"
                                )));
                                return;
                            }
                        };
                        agent_guard.replace_messages(messages_for_agent);
                    }

                    let (messages, usage) = {
                        let guard = match session.lock(&cx).await {
                            Ok(guard) => guard,
                            Err(err) => {
                                is_compacting.store(false, Ordering::SeqCst);
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to lock session: {err}"
                                )));
                                return;
                            }
                        };
                        conversation_from_session(&guard)
                    };

                    is_compacting.store(false, Ordering::SeqCst);
                    let _ = event_tx.try_send(PiMsg::ConversationReset {
                        messages,
                        usage,
                        status: Some("Compaction complete".to_string()),
                    });

                    if let Some(manager) = extensions {
                        let _ = manager
                            .dispatch_event(
                                ExtensionEventName::SessionCompact,
                                Some(json!({
                                    "tokensBefore": result.tokens_before,
                                    "firstKeptEntryId": result.first_kept_entry_id,
                                })),
                            )
                            .await;
                    }
                });
                None
            }
            SlashCommand::Reload => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot reload while processing".to_string());
                    return None;
                }

                let config = self.config.clone();
                let cli = self.resource_cli.clone();
                let cwd = self.cwd.clone();
                let event_tx = self.event_tx.clone();
                let runtime_handle = self.runtime_handle.clone();

                runtime_handle.spawn(async move {
                    let manager = PackageManager::new(cwd.clone());
                    match ResourceLoader::load(&manager, &cwd, &config, &cli).await {
                        Ok(resources) => {
                            let models_error =
                                match crate::auth::AuthStorage::load_async(Config::auth_path())
                                    .await
                                {
                                    Ok(auth) => {
                                        let models_path =
                                            default_models_path(&Config::global_dir());
                                        let registry =
                                            ModelRegistry::load(&auth, Some(models_path));
                                        registry.error().map(ToString::to_string)
                                    }
                                    Err(err) => Some(format!("Failed to load auth.json: {err}")),
                                };

                            let (diagnostics, diag_count) =
                                build_reload_diagnostics(models_error, &resources);

                            let mut status = format!(
                                "Reloaded resources: {} skills, {} prompts, {} themes",
                                resources.skills().len(),
                                resources.prompts().len(),
                                resources.themes().len()
                            );
                            if diag_count > 0 {
                                let _ = write!(status, " ({diag_count} diagnostics)");
                            }

                            let _ = event_tx.try_send(PiMsg::ResourcesReloaded {
                                resources,
                                status,
                                diagnostics,
                            });
                        }
                        Err(err) => {
                            let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                "Failed to reload resources: {err}"
                            )));
                        }
                    }
                });

                self.status_message = Some("Reloading resources...".to_string());
                None
            }
            SlashCommand::Share => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot share while processing".to_string());
                    return None;
                }

                self.agent_state = AgentState::Processing;
                self.status_message = Some("Sharing session... (Esc to cancel)".to_string());

                let (abort_handle, abort_signal) = AbortHandle::new();
                self.abort_handle = Some(abort_handle);

                let event_tx = self.event_tx.clone();
                let runtime_handle = self.runtime_handle.clone();
                let session = Arc::clone(&self.session);
                let cwd = self.cwd.clone();
                let gh_path_override = self.config.gh_path.clone();

                runtime_handle.spawn(async move {
                    let gh = gh_path_override
                        .as_ref()
                        .filter(|value| !value.trim().is_empty())
                        .cloned()
                        .unwrap_or_else(|| "gh".to_string());

                    let auth_args = vec![OsString::from("auth"), OsString::from("status")];
                    match run_command_output(&gh, &auth_args, &cwd, &abort_signal) {
                        Ok(output) => {
                            if !output.status.success() {
                                let details = format_command_output(&output);
                                let message = format!(
                                    "`gh` is not authenticated.\nRun `gh auth login` and retry.\n\n{details}"
                                );
                                let _ = event_tx.try_send(PiMsg::AgentError(message));
                                return;
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            let message = "GitHub CLI `gh` not found.\nInstall it and run `gh auth login`, then retry `/share`.".to_string();
                            let _ = event_tx.try_send(PiMsg::AgentError(message));
                            return;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                            let _ = event_tx.try_send(PiMsg::System("Share cancelled".to_string()));
                            return;
                        }
                        Err(err) => {
                            let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                "Failed to run `gh auth status`: {err}"
                            )));
                            return;
                        }
                    }

                    if abort_signal.is_aborted() {
                        let _ = event_tx.try_send(PiMsg::System("Share cancelled".to_string()));
                        return;
                    }

                    let cx = Cx::for_request();
                    let html = match session.lock(&cx).await {
                        Ok(guard) => guard.to_html(),
                        Err(err) => {
                            let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                "Failed to lock session: {err}"
                            )));
                            return;
                        }
                    };

                    if abort_signal.is_aborted() {
                        let _ = event_tx.try_send(PiMsg::System("Share cancelled".to_string()));
                        return;
                    }

                    let temp_file = match tempfile::Builder::new()
                        .prefix("pi-share-")
                        .suffix(".html")
                        .tempfile()
                    {
                        Ok(file) => file,
                        Err(err) => {
                            let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                "Failed to create temp file: {err}"
                            )));
                            return;
                        }
                    };
                    let temp_path = temp_file.into_temp_path();
                    if let Err(err) = std::fs::write(&temp_path, html.as_bytes()) {
                        let _ = event_tx.try_send(PiMsg::AgentError(format!(
                            "Failed to write temp file: {err}"
                        )));
                        return;
                    }

                    let gist_args = vec![
                        OsString::from("gist"),
                        OsString::from("create"),
                        OsString::from("--public=false"),
                        temp_path.as_os_str().to_os_string(),
                    ];
                    let output = match run_command_output(&gh, &gist_args, &cwd, &abort_signal) {
                            Ok(output) => output,
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                let message = "GitHub CLI `gh` not found.\nInstall it and run `gh auth login`, then retry `/share`.".to_string();
                                let _ = event_tx.try_send(PiMsg::AgentError(message));
                                return;
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                                let _ = event_tx
                                    .try_send(PiMsg::System("Share cancelled".to_string()));
                                return;
                            }
                            Err(err) => {
                                let _ = event_tx.try_send(PiMsg::AgentError(format!(
                                    "Failed to run `gh gist create`: {err}"
                                )));
                                return;
                            }
                        };

                    if !output.status.success() {
                        let details = format_command_output(&output);
                        let _ = event_tx.try_send(PiMsg::AgentError(format!(
                            "`gh gist create` failed.\n\n{details}"
                        )));
                        return;
                    }

                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let Some((gist_url, gist_id)) = parse_gist_url_and_id(&stdout) else {
                        let details = format_command_output(&output);
                        let _ = event_tx.try_send(PiMsg::AgentError(format!(
                            "Failed to parse gist URL from `gh gist create` output.\n\n{details}"
                        )));
                        return;
                    };

                    let share_url = crate::session::get_share_viewer_url(&gist_id);
                    drop(temp_path);

                    let message = format!("Share URL: {share_url}\nGist: {gist_url}");
                    let _ = event_tx.try_send(PiMsg::System(message));
                });
                None
            }
        }
    }
}

/// Ensure the view output fits within `term_height` terminal rows.
///
/// The output must contain at most `term_height - 1` newline characters so
/// that the cursor never advances past the last visible row, which would
/// trigger terminal scrolling in the alternate-screen buffer.
fn clamp_to_terminal_height(mut output: String, term_height: usize) -> String {
    if term_height == 0 {
        output.clear();
        return output;
    }
    let max_newlines = term_height.saturating_sub(1);

    // Fast path: count newlines and bail if we fit.
    let newline_count = memchr::memchr_iter(b'\n', output.as_bytes()).count();
    if newline_count <= max_newlines {
        return output;
    }

    // Truncate: keep only the first `max_newlines` newlines.
    let mut seen = 0usize;
    let cut = output
        .bytes()
        .position(|b| {
            if b == b'\n' {
                seen += 1;
                seen > max_newlines
            } else {
                false
            }
        })
        .unwrap_or(output.len());
    output.truncate(cut);
    output
}

fn normalize_raw_terminal_newlines(input: String) -> String {
    if !input.contains('\n') {
        return input;
    }

    let mut out = String::with_capacity(input.len() + 16);
    let mut prev_was_cr = false;
    for ch in input.chars() {
        if ch == '\n' {
            if !prev_was_cr {
                out.push('\r');
            }
            out.push('\n');
            prev_was_cr = false;
        } else {
            prev_was_cr = ch == '\r';
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_raw_terminal_newlines_inserts_crlf() {
        let normalized = normalize_raw_terminal_newlines("hello\nworld\n".to_string());
        assert_eq!(normalized, "hello\r\nworld\r\n");
    }

    #[test]
    fn normalize_raw_terminal_newlines_preserves_existing_crlf() {
        let normalized = normalize_raw_terminal_newlines("hello\r\nworld\r\n".to_string());
        assert_eq!(normalized, "hello\r\nworld\r\n");
    }

    #[test]
    fn normalize_raw_terminal_newlines_handles_mixed_newlines() {
        let normalized = normalize_raw_terminal_newlines("a\r\nb\nc\r\nd\n".to_string());
        assert_eq!(normalized, "a\r\nb\r\nc\r\nd\r\n");
    }

    #[test]
    fn clamp_to_terminal_height_noop_when_fits() {
        let input = "line1\nline2\nline3".to_string();
        // 2 newlines → 3 rows; term_height=4 allows 3 newlines → fits.
        assert_eq!(clamp_to_terminal_height(input.clone(), 4), input);
    }

    #[test]
    fn clamp_to_terminal_height_truncates_excess() {
        let input = "a\nb\nc\nd\ne\n".to_string(); // 5 newlines = 6 rows
        // term_height=4 → max 3 newlines → keeps "a\nb\nc\nd"
        let clamped = clamp_to_terminal_height(input, 4);
        assert_eq!(clamped, "a\nb\nc\nd");
    }

    #[test]
    fn clamp_to_terminal_height_zero_height() {
        let clamped = clamp_to_terminal_height("hello\nworld".to_string(), 0);
        assert_eq!(clamped, "");
    }

    #[test]
    fn clamp_to_terminal_height_exact_fit() {
        // term_height=3 → max 2 newlines.  Input has exactly 2 → fits.
        let input = "a\nb\nc".to_string();
        assert_eq!(clamp_to_terminal_height(input.clone(), 3), input);
    }

    #[test]
    fn clamp_to_terminal_height_trailing_newline() {
        // "a\nb\n" = 2 newlines, 3 rows (last row empty).
        // term_height=2 → max 1 newline → "a\nb"
        let clamped = clamp_to_terminal_height("a\nb\n".to_string(), 2);
        assert_eq!(clamped, "a\nb");
    }

    #[test]
    fn format_count_suffixes() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.0K");
        assert_eq!(format_count(1_500), "1.5K");
        assert_eq!(format_count(42_000), "42.0K");
        assert_eq!(format_count(1_000_000), "1.0M");
        assert_eq!(format_count(2_500_000), "2.5M");
    }

    #[test]
    fn tool_progress_format_display() {
        let mut p = ToolProgress::new();
        p.elapsed_ms = 5_000;
        p.line_count = 42;
        let display = p.format_display("bash");
        assert!(display.contains("Running bash"));
        assert!(display.contains("5s"));
        assert!(display.contains("42 lines"));

        // With byte count instead of lines
        p.line_count = 0;
        p.byte_count = 1_500;
        let display = p.format_display("grep");
        assert!(display.contains("Running grep"));
        assert!(display.contains("1.5K bytes"));
        assert!(!display.contains("lines"));

        // With timeout
        p.timeout_ms = Some(120_000);
        let display = p.format_display("bash");
        assert!(display.contains("timeout 120s"));
    }

    #[test]
    fn tool_progress_update_from_details() {
        let mut p = ToolProgress::new();
        let details = json!({
            "progress": {
                "elapsedMs": 3000,
                "lineCount": 100,
                "byteCount": 5000,
                "timeoutMs": 60000
            }
        });
        p.update_from_details(Some(&details));
        assert_eq!(p.elapsed_ms, 3000);
        assert_eq!(p.line_count, 100);
        assert_eq!(p.byte_count, 5000);
        assert_eq!(p.timeout_ms, Some(60000));
    }

    #[test]
    fn tool_progress_update_from_no_details() {
        let mut p = ToolProgress::new();
        // Sleep a tiny bit so elapsed > 0
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.update_from_details(None);
        assert!(p.elapsed_ms >= 5);
        assert_eq!(p.line_count, 0);
    }

    #[test]
    fn tool_message_auto_collapse_threshold() {
        // Small output: not collapsed.
        let small = ConversationMessage::tool("Tool bash:\nline1\nline2".to_string());
        assert!(!small.collapsed);
        assert_eq!(small.role, MessageRole::Tool);

        // Exactly at threshold: not collapsed (20 lines = threshold).
        let lines: String = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let at_threshold = ConversationMessage::tool(lines);
        assert!(!at_threshold.collapsed);

        // Over threshold: auto-collapsed.
        let lines: String = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD + 1)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let over_threshold = ConversationMessage::tool(lines);
        assert!(over_threshold.collapsed);
    }

    #[test]
    fn non_tool_message_never_collapsed() {
        let msg =
            ConversationMessage::new(MessageRole::User, "a very long message\n".repeat(100), None);
        assert!(!msg.collapsed);
    }

    #[test]
    fn parse_bash_command_distinguishes_exclusion() {
        let (command, exclude) = parse_bash_command("! ls -la").expect("bang command");
        assert_eq!(command, "ls -la");
        assert!(!exclude);

        let (command, exclude) = parse_bash_command("!! ls -la").expect("double bang command");
        assert_eq!(command, "ls -la");
        assert!(exclude);
    }

    #[test]
    fn extension_ui_select_accepts_string_options() {
        let request = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({
                "title": "Pick a color",
                "options": ["red", "green", "blue"],
            }),
        );

        let prompt = format_extension_ui_prompt(&request);
        assert!(prompt.contains("1) red"));
        assert!(prompt.contains("2) green"));
        assert!(prompt.contains("3) blue"));

        let response = parse_extension_ui_response(&request, "2").expect("parse selection");
        assert_eq!(response.value, Some(json!("green")));

        let response = parse_extension_ui_response(&request, "red").expect("parse selection");
        assert_eq!(response.value, Some(json!("red")));
    }

    #[test]
    fn extension_ui_select_accepts_object_options() {
        let request = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({
                "title": "Pick",
                "options": [
                    { "label": "A", "value": "alpha" },
                    { "label": "B" },
                ],
            }),
        );

        let response = parse_extension_ui_response(&request, "1").expect("parse selection");
        assert_eq!(response.value, Some(json!("alpha")));

        let response = parse_extension_ui_response(&request, "B").expect("parse selection");
        assert_eq!(response.value, Some(json!("B")));
    }

    #[cfg(all(feature = "clipboard", feature = "image-resize"))]
    #[test]
    fn paste_image_from_clipboard_writes_temp_png() {
        use arboard::ImageData;
        use std::borrow::Cow;

        let Ok(mut clipboard) = ArboardClipboard::new() else {
            return;
        };

        let image = ImageData {
            width: 1,
            height: 1,
            bytes: Cow::Owned(vec![255, 0, 0, 255]),
        };

        if clipboard.set_image(image).is_err() {
            return;
        }

        let Some(path) = PiApp::paste_image_from_clipboard() else {
            return;
        };

        assert!(path.exists());
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("png"));
    }

    // --- parse_extension_command tests ---

    #[test]
    fn parse_ext_cmd_basic() {
        let result = parse_extension_command("/deploy");
        assert_eq!(result, Some(("deploy".to_string(), vec![])));
    }

    #[test]
    fn parse_ext_cmd_with_args() {
        let result = parse_extension_command("/deploy staging fast");
        assert_eq!(
            result,
            Some((
                "deploy".to_string(),
                vec!["staging".to_string(), "fast".to_string()]
            ))
        );
    }

    #[test]
    fn parse_ext_cmd_builtin_filtered() {
        // Built-in slash commands should return None
        assert!(parse_extension_command("/help").is_none());
        assert!(parse_extension_command("/clear").is_none());
        assert!(parse_extension_command("/model").is_none());
        assert!(parse_extension_command("/exit").is_none());
        assert!(parse_extension_command("/compact").is_none());
    }

    #[test]
    fn parse_ext_cmd_no_slash() {
        assert!(parse_extension_command("deploy").is_none());
        assert!(parse_extension_command("hello world").is_none());
    }

    #[test]
    fn parse_ext_cmd_empty_slash() {
        assert!(parse_extension_command("/").is_none());
        assert!(parse_extension_command("/  ").is_none());
    }

    #[test]
    fn parse_ext_cmd_whitespace_trimming() {
        let result = parse_extension_command("  /deploy  arg1  arg2  ");
        assert_eq!(
            result,
            Some((
                "deploy".to_string(),
                vec!["arg1".to_string(), "arg2".to_string()]
            ))
        );
    }

    #[test]
    fn parse_ext_cmd_single_arg() {
        let result = parse_extension_command("/greet world");
        assert_eq!(
            result,
            Some(("greet".to_string(), vec!["world".to_string()]))
        );
    }

    // --- extension_commands_for_catalog tests ---

    #[test]
    fn ext_commands_catalog_builds_entries() {
        let manager = crate::extensions::ExtensionManager::new();
        manager.register(crate::extensions::RegisterPayload {
            name: "test-ext".to_string(),
            version: "1.0.0".to_string(),
            api_version: crate::extensions::PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: vec![
                json!({"name": "deploy", "description": "Deploy the app"}),
                json!({"name": "rollback"}),
            ],
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });

        let entries = extension_commands_for_catalog(&manager);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "deploy");
        assert_eq!(entries[0].description.as_deref(), Some("Deploy the app"));
        assert_eq!(entries[1].name, "rollback");
        assert!(entries[1].description.is_none());
    }

    #[test]
    fn ext_commands_catalog_empty_manager() {
        let manager = crate::extensions::ExtensionManager::new();
        let entries = extension_commands_for_catalog(&manager);
        assert!(entries.is_empty());
    }

    // --- strip_wrapping_quotes tests ---

    #[test]
    fn strip_wrapping_quotes_double() {
        assert_eq!(strip_wrapping_quotes("\"hello\""), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_single() {
        assert_eq!(strip_wrapping_quotes("'hello'"), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_mismatched() {
        assert_eq!(strip_wrapping_quotes("\"hello'"), "\"hello'");
    }

    #[test]
    fn strip_wrapping_quotes_no_quotes() {
        assert_eq!(strip_wrapping_quotes("hello"), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_empty() {
        assert_eq!(strip_wrapping_quotes(""), "");
    }

    #[test]
    fn strip_wrapping_quotes_single_char() {
        assert_eq!(strip_wrapping_quotes("\""), "\"");
    }

    // --- looks_like_windows_path tests ---

    #[test]
    fn windows_path_drive_letter() {
        assert!(looks_like_windows_path("C:\\Users\\foo"));
        assert!(looks_like_windows_path("D:file.txt"));
    }

    #[test]
    fn windows_path_unc() {
        assert!(looks_like_windows_path("\\\\server\\share"));
    }

    #[test]
    fn unix_path_not_windows() {
        assert!(!looks_like_windows_path("/home/user/file"));
        assert!(!looks_like_windows_path("relative/path"));
    }

    // --- unescape_dragged_path tests ---

    #[test]
    fn unescape_dragged_path_backslash_space() {
        assert_eq!(unescape_dragged_path("my\\ file.txt"), "my file.txt");
    }

    #[test]
    fn unescape_dragged_path_backslash_parens() {
        assert_eq!(unescape_dragged_path("file\\(1\\).txt"), "file(1).txt");
    }

    #[test]
    fn unescape_dragged_path_windows_preserved() {
        assert_eq!(unescape_dragged_path("C:\\Users\\foo"), "C:\\Users\\foo");
    }

    #[test]
    fn unescape_dragged_path_no_escapes() {
        assert_eq!(unescape_dragged_path("simple.txt"), "simple.txt");
    }

    // --- file_url_to_path tests ---

    #[test]
    fn file_url_to_path_valid() {
        let result = file_url_to_path("file:///tmp/test.txt");
        assert_eq!(result, Some(std::path::PathBuf::from("/tmp/test.txt")));
    }

    #[test]
    fn file_url_to_path_not_file_url() {
        assert!(file_url_to_path("https://example.com").is_none());
        assert!(file_url_to_path("/tmp/test.txt").is_none());
    }

    // --- format_file_ref tests ---

    #[test]
    fn format_file_ref_simple() {
        assert_eq!(format_file_ref("src/main.rs"), "@src/main.rs");
    }

    #[test]
    fn format_file_ref_with_spaces() {
        assert_eq!(format_file_ref("my file.rs"), "@\"my file.rs\"");
    }

    #[test]
    fn format_file_ref_with_double_quotes_in_path() {
        assert_eq!(format_file_ref("my \"file\".rs"), "@'my \"file\".rs'");
    }

    #[test]
    fn format_file_ref_with_both_quotes() {
        assert_eq!(
            format_file_ref("it's a \"file\" name.rs"),
            "@\"it's a \\\"file\\\" name.rs\""
        );
    }

    // --- split_trailing_punct tests ---

    #[test]
    fn split_trailing_punct_period() {
        assert_eq!(split_trailing_punct("file.rs."), ("file.rs", "."));
    }

    #[test]
    fn split_trailing_punct_comma() {
        assert_eq!(split_trailing_punct("word,"), ("word", ","));
    }

    #[test]
    fn split_trailing_punct_no_trailing() {
        assert_eq!(split_trailing_punct("word"), ("word", ""));
    }

    #[test]
    fn split_trailing_punct_all_punct() {
        assert_eq!(split_trailing_punct("!?"), ("", "!?"));
    }

    #[test]
    fn split_trailing_punct_empty() {
        assert_eq!(split_trailing_punct(""), ("", ""));
    }

    // --- is_file_ref_boundary tests ---

    #[test]
    fn file_ref_boundary_at_start() {
        assert!(is_file_ref_boundary("@file", 0));
    }

    #[test]
    fn file_ref_boundary_after_space() {
        assert!(is_file_ref_boundary("see @file", 4));
    }

    #[test]
    fn file_ref_boundary_after_paren() {
        assert!(is_file_ref_boundary("(@file)", 1));
    }

    #[test]
    fn file_ref_boundary_mid_word() {
        assert!(!is_file_ref_boundary("foo@bar", 3));
    }

    // --- truncate tests ---

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_exact_fit() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world!", 8), "hello...");
    }

    #[test]
    fn truncate_zero() {
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn truncate_very_small_max() {
        assert_eq!(truncate("hello", 1), ".");
        assert_eq!(truncate("hello", 2), "..");
        assert_eq!(truncate("hello", 3), "...");
    }

    // --- strip_thinking_level_suffix tests ---

    #[test]
    fn strip_thinking_suffix_present() {
        assert_eq!(
            strip_thinking_level_suffix("claude-opus:high"),
            "claude-opus"
        );
        assert_eq!(strip_thinking_level_suffix("model:off"), "model");
        assert_eq!(strip_thinking_level_suffix("m:xhigh"), "m");
    }

    #[test]
    fn strip_thinking_suffix_absent() {
        assert_eq!(strip_thinking_level_suffix("claude-opus"), "claude-opus");
    }

    #[test]
    fn strip_thinking_suffix_unknown_level() {
        assert_eq!(strip_thinking_level_suffix("claude:turbo"), "claude:turbo");
    }

    // --- parse_scoped_model_patterns tests ---

    #[test]
    fn parse_model_patterns_comma_separated() {
        assert_eq!(
            parse_scoped_model_patterns("gpt-4*,claude*"),
            vec!["gpt-4*", "claude*"]
        );
    }

    #[test]
    fn parse_model_patterns_space_separated() {
        assert_eq!(
            parse_scoped_model_patterns("gpt-4o claude-opus"),
            vec!["gpt-4o", "claude-opus"]
        );
    }

    #[test]
    fn parse_model_patterns_mixed() {
        assert_eq!(parse_scoped_model_patterns("a, b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_model_patterns_empty() {
        assert!(parse_scoped_model_patterns("").is_empty());
        assert!(parse_scoped_model_patterns("  ").is_empty());
    }

    // --- queued_message_preview tests ---

    #[test]
    fn queued_preview_short() {
        assert_eq!(queued_message_preview("hello", 10), "hello");
    }

    #[test]
    fn queued_preview_truncated() {
        assert_eq!(queued_message_preview("hello world!", 8), "hello...");
    }

    #[test]
    fn queued_preview_multiline() {
        assert_eq!(queued_message_preview("\n\nhello\nworld", 20), "hello");
    }

    #[test]
    fn queued_preview_empty() {
        assert_eq!(queued_message_preview("", 10), "(empty)");
        assert_eq!(queued_message_preview("  \n  \n  ", 10), "(empty)");
    }

    // --- parse_gist_url_and_id tests ---

    #[test]
    fn parse_gist_url_valid() {
        let output = "Created gist https://gist.github.com/user/abc123def456";
        let result = parse_gist_url_and_id(output);
        assert_eq!(
            result,
            Some((
                "https://gist.github.com/user/abc123def456".to_string(),
                "abc123def456".to_string()
            ))
        );
    }

    #[test]
    fn parse_gist_url_no_gist() {
        assert!(parse_gist_url_and_id("no url here").is_none());
    }

    #[test]
    fn parse_gist_url_wrong_host() {
        assert!(parse_gist_url_and_id("https://github.com/user/repo").is_none());
    }

    #[test]
    fn parse_gist_url_with_quotes_and_trailing_punctuation() {
        let output = "Created gist: 'https://gist.github.com/testuser/abc123def456', done.";
        let result = parse_gist_url_and_id(output);
        assert_eq!(
            result,
            Some((
                "https://gist.github.com/testuser/abc123def456".to_string(),
                "abc123def456".to_string()
            ))
        );
    }

    // --- parse_queue_mode tests ---

    #[test]
    fn parse_queue_mode_all() {
        assert!(matches!(
            parse_queue_mode_or_default(Some("all")),
            QueueMode::All
        ));
    }

    #[test]
    fn parse_queue_mode_default() {
        assert!(matches!(
            parse_queue_mode_or_default(None),
            QueueMode::OneAtATime
        ));
        assert!(matches!(
            parse_queue_mode_or_default(Some("anything")),
            QueueMode::OneAtATime
        ));
    }

    // --- push_line tests ---

    #[test]
    fn push_line_to_empty() {
        let mut s = String::new();
        push_line(&mut s, "hello");
        assert_eq!(s, "hello");
    }

    #[test]
    fn push_line_appends_with_newline() {
        let mut s = "hello".to_string();
        push_line(&mut s, "world");
        assert_eq!(s, "hello\nworld");
    }

    #[test]
    fn push_line_skips_empty() {
        let mut s = "hello".to_string();
        push_line(&mut s, "");
        assert_eq!(s, "hello");
    }

    // --- parse_bash_command additional edge cases ---

    #[test]
    fn parse_bash_command_empty_bang() {
        assert!(parse_bash_command("!").is_none());
        assert!(parse_bash_command("!!").is_none());
        assert!(parse_bash_command("!  ").is_none());
    }

    #[test]
    fn parse_bash_command_no_bang() {
        assert!(parse_bash_command("ls -la").is_none());
        assert!(parse_bash_command("").is_none());
    }

    #[test]
    fn parse_bash_command_leading_whitespace() {
        let (cmd, exclude) = parse_bash_command("  ! echo hi").expect("should parse");
        assert_eq!(cmd, "echo hi");
        assert!(!exclude);
    }

    // --- pretty_json tests ---

    #[test]
    fn pretty_json_formats_object() {
        let val = json!({"a": 1});
        let out = pretty_json(&val);
        assert!(out.contains("\"a\": 1"));
        assert!(out.contains('\n'));
    }

    #[test]
    fn pretty_json_formats_null() {
        assert_eq!(pretty_json(&json!(null)), "null");
    }

    // --- SlashCommand::parse tests ---

    #[test]
    fn slash_command_parse_known_commands() {
        assert!(matches!(
            SlashCommand::parse("/help"),
            Some((SlashCommand::Help, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/h"),
            Some((SlashCommand::Help, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/?"),
            Some((SlashCommand::Help, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/exit"),
            Some((SlashCommand::Exit, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/quit"),
            Some((SlashCommand::Exit, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/q"),
            Some((SlashCommand::Exit, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/clear"),
            Some((SlashCommand::Clear, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/cls"),
            Some((SlashCommand::Clear, ""))
        ));
    }

    #[test]
    fn slash_command_parse_with_args() {
        let (cmd, args) = SlashCommand::parse("/model claude-opus").unwrap();
        assert!(matches!(cmd, SlashCommand::Model));
        assert_eq!(args, "claude-opus");

        let (cmd, args) = SlashCommand::parse("/name my session").unwrap();
        assert!(matches!(cmd, SlashCommand::Name));
        assert_eq!(args, "my session");
    }

    #[test]
    fn slash_command_parse_case_insensitive() {
        assert!(SlashCommand::parse("/HELP").is_some());
        assert!(SlashCommand::parse("/Model").is_some());
        assert!(SlashCommand::parse("/EXIT").is_some());
    }

    #[test]
    fn slash_command_parse_unknown() {
        assert!(SlashCommand::parse("/deploy").is_none());
        assert!(SlashCommand::parse("/unknown").is_none());
    }

    #[test]
    fn slash_command_parse_no_slash() {
        assert!(SlashCommand::parse("help").is_none());
        assert!(SlashCommand::parse("model gpt-4").is_none());
    }

    #[test]
    fn slash_command_parse_aliases() {
        assert!(matches!(
            SlashCommand::parse("/m"),
            Some((SlashCommand::Model, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/t"),
            Some((SlashCommand::Thinking, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/think"),
            Some((SlashCommand::Thinking, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/r"),
            Some((SlashCommand::Resume, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/cp"),
            Some((SlashCommand::Copy, ""))
        ));
        assert!(matches!(
            SlashCommand::parse("/info"),
            Some((SlashCommand::Session, ""))
        ));
    }

    // --- format_tool_output tests ---

    #[test]
    fn format_tool_output_text_only() {
        let blocks = vec![ContentBlock::Text(TextContent::new("tool result"))];
        let result = format_tool_output(&blocks, None, false);
        assert_eq!(result.as_deref(), Some("tool result"));
    }

    #[test]
    fn format_tool_output_with_diff_details() {
        let blocks = vec![ContentBlock::Text(TextContent::new(
            "Successfully replaced text in foo.rs.",
        ))];
        let details = json!({ "diff": "- 1 old\n+ 1 new" });
        let result = format_tool_output(&blocks, Some(&details), false).unwrap();
        assert!(result.contains("Diff:"));
        assert!(result.contains("- 1 old"));
        assert!(result.contains("+ 1 new"));
    }

    #[test]
    fn format_tool_output_empty_returns_none() {
        let blocks: Vec<ContentBlock> = vec![];
        assert!(format_tool_output(&blocks, None, false).is_none());
    }

    #[test]
    fn format_tool_output_empty_text_with_details_shows_json() {
        let blocks: Vec<ContentBlock> = vec![];
        let details = json!({"key": "value"});
        let result = format_tool_output(&blocks, Some(&details), false).unwrap();
        assert!(result.contains("key"));
        assert!(result.contains("value"));
    }

    #[test]
    fn format_tool_output_empty_diff_in_details() {
        let blocks = vec![ContentBlock::Text(TextContent::new("Success"))];
        let details = json!({ "diff": "  " }); // whitespace-only diff
        let result = format_tool_output(&blocks, Some(&details), false).unwrap();
        // Should NOT contain Diff: header since diff is effectively empty
        assert!(!result.contains("Diff:"));
        assert!(result.contains("Success"));
    }

    // --- path_for_display tests ---

    #[test]
    fn path_for_display_within_cwd() {
        let cwd = Path::new("/home/user/project");
        let path = Path::new("/home/user/project/src/main.rs");
        assert_eq!(path_for_display(path, cwd), "src/main.rs");
    }

    #[test]
    fn path_for_display_outside_cwd() {
        let cwd = Path::new("/home/user/project");
        let path = Path::new("/tmp/file.txt");
        assert_eq!(path_for_display(path, cwd), "/tmp/file.txt");
    }

    #[test]
    fn path_for_display_same_as_cwd() {
        let cwd = Path::new("/home/user");
        let path = Path::new("/home/user");
        assert_eq!(path_for_display(path, cwd), "");
    }

    // --- assistant_content_to_text tests ---

    #[test]
    fn assistant_text_only() {
        let blocks = vec![ContentBlock::Text(TextContent::new("Hello"))];
        let (text, thinking) = assistant_content_to_text(&blocks);
        assert_eq!(text, "Hello");
        assert!(thinking.is_none());
    }

    #[test]
    fn assistant_text_with_thinking() {
        let blocks = vec![
            ContentBlock::Thinking(crate::model::ThinkingContent {
                thinking: "Let me reason...".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new("response")),
        ];
        let (text, thinking) = assistant_content_to_text(&blocks);
        assert_eq!(text, "response");
        assert_eq!(thinking.as_deref(), Some("Let me reason..."));
    }

    #[test]
    fn assistant_empty_thinking_is_none() {
        let blocks = vec![
            ContentBlock::Thinking(crate::model::ThinkingContent {
                thinking: "  ".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new("response")),
        ];
        let (_, thinking) = assistant_content_to_text(&blocks);
        assert!(
            thinking.is_none(),
            "whitespace-only thinking should be None"
        );
    }

    // --- ConversationMessage tests ---

    #[test]
    fn conversation_message_tool_role() {
        let msg = ConversationMessage::tool("Tool read:\nfile contents".to_string());
        assert_eq!(msg.role, MessageRole::Tool);
        assert!(msg.content.contains("file contents"));
    }

    #[test]
    fn conversation_message_new_user_not_collapsed() {
        let msg = ConversationMessage::new(MessageRole::User, "question".to_string(), None);
        assert_eq!(msg.role, MessageRole::User);
        assert!(!msg.collapsed);
    }

    #[test]
    fn conversation_message_with_thinking() {
        let msg = ConversationMessage::new(
            MessageRole::Assistant,
            "response".to_string(),
            Some("I'm thinking...".to_string()),
        );
        assert_eq!(msg.thinking.as_deref(), Some("I'm thinking..."));
    }

    // --- extension UI prompt/response ---

    #[test]
    fn extension_ui_confirm_prompt_format() {
        let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
        let prompt = format_extension_ui_prompt(&request);
        assert!(prompt.contains("Proceed?"));
    }

    #[test]
    fn extension_ui_confirm_yes() {
        let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
        let response = parse_extension_ui_response(&request, "yes").unwrap();
        assert_eq!(response.value, Some(json!(true)));
    }

    #[test]
    fn extension_ui_confirm_no() {
        let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
        let response = parse_extension_ui_response(&request, "no").unwrap();
        assert_eq!(response.value, Some(json!(false)));
    }

    #[test]
    fn extension_ui_input_response() {
        let request = ExtensionUiRequest::new("req-1", "input", json!({ "title": "Enter name:" }));
        let response = parse_extension_ui_response(&request, "Alice").unwrap();
        assert_eq!(response.value, Some(json!("Alice")));
    }

    #[test]
    fn extension_ui_select_by_label_text() {
        let request = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({
                "title": "Pick",
                "options": ["alpha", "beta", "gamma"],
            }),
        );
        let response = parse_extension_ui_response(&request, "beta").unwrap();
        assert_eq!(response.value, Some(json!("beta")));
    }

    // --- tool_content_blocks_to_text tests ---

    #[test]
    fn tool_content_blocks_text_only() {
        let blocks = vec![ContentBlock::Text(TextContent::new("hello world"))];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn tool_content_blocks_multiple_text() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("line 1")),
            ContentBlock::Text(TextContent::new("line 2")),
        ];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert!(result.contains("line 1"));
        assert!(result.contains("line 2"));
    }

    #[test]
    fn tool_content_blocks_images_hidden() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("text")),
            ContentBlock::Image(crate::model::ImageContent {
                data: String::new(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Image(crate::model::ImageContent {
                data: String::new(),
                mime_type: "image/png".to_string(),
            }),
        ];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert!(result.contains("text"));
        assert!(result.contains("[2 image(s) hidden]"));
    }

    #[test]
    fn tool_content_blocks_thinking() {
        let blocks = vec![ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "reasoning here".to_string(),
            thinking_signature: None,
        })];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert_eq!(result, "reasoning here");
    }

    #[test]
    fn tool_content_blocks_tool_call() {
        let blocks = vec![ContentBlock::ToolCall(crate::model::ToolCall {
            id: "tc-1".to_string(),
            name: "bash".to_string(),
            arguments: json!({"command": "ls"}),
            thought_signature: None,
        })];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert!(result.contains("[tool call: bash]"));
    }

    #[test]
    fn tool_content_blocks_empty() {
        let result = tool_content_blocks_to_text(&[], false);
        assert!(result.is_empty());
    }

    // --- format_resource_diagnostics tests ---

    #[test]
    fn format_resource_diagnostics_single_warning() {
        let diags = vec![crate::resources::ResourceDiagnostic {
            kind: crate::resources::DiagnosticKind::Warning,
            message: "File too large".to_string(),
            path: PathBuf::from("/tmp/skills/big.md"),
            collision: None,
        }];
        let (text, count) = format_resource_diagnostics("Skills", &diags);
        assert_eq!(count, 1);
        assert!(text.contains("Skills:"));
        assert!(text.contains("warning: File too large"));
        assert!(text.contains("/tmp/skills/big.md"));
    }

    #[test]
    fn format_resource_diagnostics_collision() {
        let diags = vec![crate::resources::ResourceDiagnostic {
            kind: crate::resources::DiagnosticKind::Collision,
            message: "Duplicate skill name".to_string(),
            path: PathBuf::from("/a/skill.md"),
            collision: Some(crate::resources::CollisionInfo {
                resource_type: "skill".to_string(),
                name: "deploy".to_string(),
                winner_path: PathBuf::from("/a/skill.md"),
                loser_path: PathBuf::from("/b/skill.md"),
            }),
        }];
        let (text, count) = format_resource_diagnostics("Skills", &diags);
        assert_eq!(count, 1);
        assert!(text.contains("collision:"));
        assert!(text.contains("[winner: /a/skill.md loser: /b/skill.md]"));
    }

    #[test]
    fn format_resource_diagnostics_sorts_by_path_then_kind() {
        let diags = vec![
            crate::resources::ResourceDiagnostic {
                kind: crate::resources::DiagnosticKind::Collision,
                message: "z-message".to_string(),
                path: PathBuf::from("/a"),
                collision: None,
            },
            crate::resources::ResourceDiagnostic {
                kind: crate::resources::DiagnosticKind::Warning,
                message: "a-message".to_string(),
                path: PathBuf::from("/a"),
                collision: None,
            },
            crate::resources::ResourceDiagnostic {
                kind: crate::resources::DiagnosticKind::Warning,
                message: "b-message".to_string(),
                path: PathBuf::from("/b"),
                collision: None,
            },
        ];
        let (text, count) = format_resource_diagnostics("Test", &diags);
        assert_eq!(count, 3);
        // Within /a: warning (rank 0) comes before collision (rank 1)
        let warn_pos = text.find("a-message").unwrap();
        let coll_pos = text.find("z-message").unwrap();
        assert!(
            warn_pos < coll_pos,
            "Warning should appear before collision for same path"
        );
    }

    #[test]
    fn format_resource_diagnostics_empty() {
        let (text, count) = format_resource_diagnostics("Skills", &[]);
        assert_eq!(count, 0);
        assert!(text.contains("Skills:"));
    }

    // --- kind_rank tests ---

    #[test]
    fn kind_rank_ordering() {
        assert!(
            kind_rank(&crate::resources::DiagnosticKind::Warning)
                < kind_rank(&crate::resources::DiagnosticKind::Collision)
        );
    }

    // --- user_content_to_text tests ---

    #[test]
    fn user_content_text_variant() {
        let content = UserContent::Text("hello".to_string());
        assert_eq!(user_content_to_text(&content), "hello");
    }

    #[test]
    fn user_content_blocks_variant() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ]);
        let result = user_content_to_text(&content);
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    // --- content_blocks_to_text tests ---

    #[test]
    fn content_blocks_to_text_mixed() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("text")),
            ContentBlock::Thinking(crate::model::ThinkingContent {
                thinking: "think".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::ToolCall(crate::model::ToolCall {
                id: "tc-1".to_string(),
                name: "read".to_string(),
                arguments: json!({}),
                thought_signature: None,
            }),
        ];
        let result = content_blocks_to_text(&blocks);
        assert!(result.contains("text"));
        assert!(result.contains("think"));
        assert!(result.contains("[tool call: read]"));
    }

    // --- split_content_blocks_for_input tests ---

    #[test]
    fn split_content_blocks_text_and_images() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::Image(crate::model::ImageContent {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Thinking(crate::model::ThinkingContent {
                thinking: "ignored".to_string(),
                thinking_signature: None,
            }),
        ];
        let (text, images) = split_content_blocks_for_input(&blocks);
        assert_eq!(text, "hello");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "base64data");
    }

    #[test]
    fn split_content_blocks_empty() {
        let (text, images) = split_content_blocks_for_input(&[]);
        assert!(text.is_empty());
        assert!(images.is_empty());
    }

    // --- build_content_blocks_for_input tests ---

    #[test]
    fn build_content_blocks_text_and_images() {
        let img = crate::model::ImageContent {
            data: "d".to_string(),
            mime_type: "image/png".to_string(),
        };
        let blocks = build_content_blocks_for_input("hello", &[img]);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hello"));
        assert!(matches!(&blocks[1], ContentBlock::Image(_)));
    }

    #[test]
    fn build_content_blocks_empty_text_skipped() {
        let blocks = build_content_blocks_for_input("  ", &[]);
        assert!(blocks.is_empty());
    }

    // --- next_non_whitespace_token tests ---

    #[test]
    fn next_token_basic() {
        let (token, end) = next_non_whitespace_token("hello world", 0);
        assert_eq!(token, "hello");
        assert_eq!(end, 5);
    }

    #[test]
    fn next_token_past_end() {
        let (token, end) = next_non_whitespace_token("abc", 10);
        assert_eq!(token, "");
        assert_eq!(end, 3);
    }

    #[test]
    fn next_token_last_word() {
        let (token, end) = next_non_whitespace_token("a bc", 2);
        assert_eq!(token, "bc");
        assert_eq!(end, 4);
    }

    // --- SlashCommand::parse additional coverage ---

    #[test]
    fn slash_command_all_variants_parse() {
        // Verify all main slash commands parse correctly
        let cases = vec![
            ("/login", SlashCommand::Login),
            ("/logout", SlashCommand::Logout),
            ("/settings", SlashCommand::Settings),
            ("/history", SlashCommand::History),
            ("/export", SlashCommand::Export),
            ("/session", SlashCommand::Session),
            ("/theme", SlashCommand::Theme),
            ("/resume", SlashCommand::Resume),
            ("/new", SlashCommand::New),
            ("/copy", SlashCommand::Copy),
            ("/name", SlashCommand::Name),
            ("/hotkeys", SlashCommand::Hotkeys),
            ("/changelog", SlashCommand::Changelog),
            ("/tree", SlashCommand::Tree),
            ("/fork", SlashCommand::Fork),
            ("/compact", SlashCommand::Compact),
            ("/reload", SlashCommand::Reload),
            ("/share", SlashCommand::Share),
        ];
        for (input, expected) in cases {
            let result = SlashCommand::parse(input);
            assert!(
                result.is_some(),
                "Expected {input} to parse as a SlashCommand"
            );
            let (cmd, _) = result.unwrap();
            assert_eq!(
                std::mem::discriminant(&cmd),
                std::mem::discriminant(&expected),
                "Mismatch for input {input}"
            );
        }
    }

    #[test]
    fn slash_command_empty_and_whitespace() {
        assert!(SlashCommand::parse("").is_none());
        assert!(SlashCommand::parse("  ").is_none());
        assert!(SlashCommand::parse("/").is_none());
    }

    // --- ConversationMessage collapse boundary ---

    #[test]
    fn tool_collapse_single_line() {
        let msg = ConversationMessage::tool("one line".to_string());
        assert!(!msg.collapsed);
    }

    #[test]
    fn tool_collapse_exactly_threshold_plus_one() {
        // TOOL_AUTO_COLLAPSE_THRESHOLD + 1 lines
        let content = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD + 1)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = ConversationMessage::tool(content);
        assert!(msg.collapsed);
    }

    // --- resolve_scoped_model_entries tests ---

    fn test_model_entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            model: crate::provider::Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "test".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.invalid".to_string(),
                reasoning: false,
                input: vec![crate::provider::InputType::Text],
                cost: crate::provider::ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 4096,
                max_tokens: 1024,
                headers: std::collections::HashMap::new(),
            },
            api_key: None,
            headers: std::collections::HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }
    }

    fn resolved_ids(entries: &[ModelEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| format!("{}/{}", e.model.provider, e.model.id))
            .collect()
    }

    #[test]
    fn resolve_scoped_exact_match_by_id() {
        let models = vec![
            test_model_entry("anthropic", "claude-sonnet-4"),
            test_model_entry("openai", "gpt-4o"),
        ];
        let patterns = vec!["gpt-4o".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o"]);
    }

    #[test]
    fn resolve_scoped_exact_match_by_full_id() {
        let models = vec![
            test_model_entry("anthropic", "claude-sonnet-4"),
            test_model_entry("openai", "gpt-4o"),
        ];
        let patterns = vec!["anthropic/claude-sonnet-4".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["anthropic/claude-sonnet-4"]);
    }

    #[test]
    fn resolve_scoped_glob_wildcard() {
        let models = vec![
            test_model_entry("openai", "gpt-4o"),
            test_model_entry("openai", "gpt-4o-mini"),
            test_model_entry("anthropic", "claude-sonnet-4"),
        ];
        let patterns = vec!["gpt-4*".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(
            resolved_ids(&result),
            vec!["openai/gpt-4o", "openai/gpt-4o-mini"]
        );
    }

    #[test]
    fn resolve_scoped_glob_provider_slash() {
        let models = vec![
            test_model_entry("openai", "gpt-4o"),
            test_model_entry("openai", "o1"),
            test_model_entry("anthropic", "claude-sonnet-4"),
        ];
        let patterns = vec!["openai/*".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o", "openai/o1"]);
    }

    #[test]
    fn resolve_scoped_case_insensitive() {
        let models = vec![test_model_entry("OpenAI", "GPT-4o")];
        let patterns = vec!["gpt-4o".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].model.id, "GPT-4o");
    }

    #[test]
    fn resolve_scoped_deduplicates() {
        let models = vec![
            test_model_entry("openai", "gpt-4o"),
            test_model_entry("anthropic", "claude-sonnet-4"),
        ];
        // Both patterns match gpt-4o, but it should appear only once.
        let patterns = vec!["gpt-4o".to_string(), "openai/*".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o"]);
    }

    #[test]
    fn resolve_scoped_output_sorted() {
        let models = vec![
            test_model_entry("openai", "gpt-4o"),
            test_model_entry("anthropic", "claude-sonnet-4"),
            test_model_entry("google", "gemini-pro"),
        ];
        let patterns = vec!["*".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        let ids = resolved_ids(&result);
        assert_eq!(
            ids,
            vec![
                "anthropic/claude-sonnet-4",
                "google/gemini-pro",
                "openai/gpt-4o"
            ]
        );
    }

    #[test]
    fn resolve_scoped_invalid_glob_returns_error() {
        let models = vec![test_model_entry("openai", "gpt-4o")];
        let patterns = vec!["[invalid".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid model pattern"));
    }

    #[test]
    fn resolve_scoped_no_match_returns_empty() {
        let models = vec![test_model_entry("openai", "gpt-4o")];
        let patterns = vec!["nonexistent-model".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_scoped_thinking_suffix_stripped() {
        let models = vec![
            test_model_entry("anthropic", "claude-sonnet-4"),
            test_model_entry("openai", "gpt-4o"),
        ];
        let patterns = vec!["claude-sonnet-4:high".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["anthropic/claude-sonnet-4"]);
    }

    #[test]
    fn resolve_scoped_question_mark_glob() {
        let models = vec![
            test_model_entry("openai", "o1"),
            test_model_entry("openai", "o3"),
            test_model_entry("openai", "gpt-4o"),
        ];
        let patterns = vec!["o?".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert_eq!(resolved_ids(&result), vec!["openai/o1", "openai/o3"]);
    }

    #[test]
    fn resolve_scoped_empty_available_returns_empty() {
        let models: Vec<ModelEntry> = Vec::new();
        let patterns = vec!["*".to_string()];
        let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
        assert!(result.is_empty());
    }
}
