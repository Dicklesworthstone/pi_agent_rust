//! Session management and persistence.
//!
//! Sessions are stored as JSONL files with a tree structure that enables
//! branching and history navigation.

use crate::agent_cx::AgentCx;
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, TextContent, ToolResultMessage, UserContent,
    UserMessage,
};
use crate::session_index::SessionIndex;
use crate::tui::PiConsole;
use asupersync::channel::oneshot;
use rich_rust::Theme;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current session file format version.
pub const SESSION_VERSION: u8 = 3;

/// Default base URL for the Pi session share viewer.
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://buildwithpi.ai/session/";

fn build_share_viewer_url(base_url: Option<&str>, gist_id: &str) -> String {
    let base_url = base_url
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SHARE_VIEWER_URL);
    format!("{base_url}#{gist_id}")
}

/// Get the share viewer URL for a gist ID.
///
/// Matches legacy Pi Agent semantics:
/// - Use `PI_SHARE_VIEWER_URL` env var when set and non-empty
/// - Otherwise fall back to `DEFAULT_SHARE_VIEWER_URL`
/// - Final URL is `{base}#{gist_id}` (no trailing-slash normalization)
#[must_use]
pub fn get_share_viewer_url(gist_id: &str) -> String {
    let base_url = std::env::var("PI_SHARE_VIEWER_URL").ok();
    build_share_viewer_url(base_url.as_deref(), gist_id)
}

// ============================================================================
// Session
// ============================================================================

/// A session manages conversation state and persistence.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session header
    pub header: SessionHeader,
    /// Session entries (messages, changes, etc.)
    pub entries: Vec<SessionEntry>,
    /// Path to the session file (None for in-memory)
    pub path: Option<PathBuf>,
    /// Current leaf entry ID
    pub leaf_id: Option<String>,
    /// Base directory for session storage (optional override)
    pub session_dir: Option<PathBuf>,
}

/// Result of planning a `/fork` operation from a specific user message.
///
/// Mirrors legacy semantics:
/// - The new session's leaf is the *parent* of the selected user message (or `None` if root),
///   so the selected message can be re-submitted as a new branch without creating consecutive
///   user messages.
/// - The selected user message text is returned for editor pre-fill.
#[derive(Debug, Clone)]
pub struct ForkPlan {
    /// Entries to copy into the new session file (path to the fork leaf, inclusive).
    pub entries: Vec<SessionEntry>,
    /// Leaf ID to set in the new session (parent of selected user entry).
    pub leaf_id: Option<String>,
    /// Text of the selected user message (for editor pre-fill).
    pub selected_text: String,
}

/// Diagnostics captured while opening a session file.
#[derive(Debug, Clone, Default)]
pub struct SessionOpenDiagnostics {
    pub skipped_entries: Vec<SessionOpenSkippedEntry>,
}

#[derive(Debug, Clone)]
pub struct SessionOpenSkippedEntry {
    /// 1-based line number in the session file.
    pub line_number: usize,
    pub error: String,
}

impl SessionOpenDiagnostics {
    fn warning_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for skipped in &self.skipped_entries {
            lines.push(format!(
                "Warning: Skipping corrupted entry at line {} in session file: {}",
                skipped.line_number, skipped.error
            ));
        }

        if !self.skipped_entries.is_empty() {
            lines.push(format!(
                "Warning: Skipped {} corrupted entries while loading session",
                self.skipped_entries.len()
            ));
        }

        lines
    }
}

impl Session {
    /// Create a new session from CLI args and config.
    pub async fn new(cli: &Cli, config: &Config) -> Result<Self> {
        let session_dir = cli.session_dir.as_ref().map(PathBuf::from);
        if cli.no_session {
            return Ok(Self::in_memory());
        }

        if let Some(path) = &cli.session {
            return Self::open(path).await;
        }

        if cli.resume {
            return Box::pin(Self::resume_with_picker(
                session_dir.as_deref(),
                config,
                None,
            ))
            .await;
        }

        if cli.r#continue {
            return Self::continue_recent_in_dir(session_dir.as_deref(), config).await;
        }

        // Create a new session
        Ok(Self::create_with_dir(session_dir))
    }

    /// Resume a session by prompting the user to select from recent sessions.
    pub async fn resume_with_picker(
        override_dir: Option<&Path>,
        config: &Config,
        picker_input_override: Option<String>,
    ) -> Result<Self> {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            if let Some(session) = crate::session_picker::pick_session(override_dir).await {
                return Ok(session);
            }
        }

        let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
        let cwd = std::env::current_dir()?;
        let encoded_cwd = encode_cwd(&cwd);
        let project_session_dir = base_dir.join(&encoded_cwd);

        if !project_session_dir.exists() {
            return Ok(Self::create_with_dir(Some(base_dir)));
        }

        let mut entries: Vec<SessionPickEntry> = SessionIndex::for_sessions_root(&base_dir)
            .list_sessions(Some(&cwd.display().to_string()))
            .map(|list| {
                list.into_iter()
                    .filter_map(SessionPickEntry::from_meta)
                    .collect()
            })
            .unwrap_or_default();

        if entries.is_empty() {
            entries = scan_sessions_on_disk(&project_session_dir)?;
        }

        if entries.is_empty() {
            return Ok(Self::create_with_dir(Some(base_dir)));
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_modified_ms));
        let max_entries = 20usize.min(entries.len());
        let entries = entries.into_iter().take(max_entries).collect::<Vec<_>>();

        let theme = Self::resolve_console_theme(config, &cwd);
        let console = PiConsole::new_with_theme(theme);
        console.render_info("Select a session to resume:");

        let mut rows: Vec<Vec<String>> = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            rows.push(vec![
                format!("{}", idx + 1),
                entry.timestamp.clone(),
                entry.message_count.to_string(),
                entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                entry.path.display().to_string(),
            ]);
        }

        let headers = ["#", "Timestamp", "Messages", "Name", "Path"];
        let row_refs: Vec<Vec<&str>> = rows
            .iter()
            .map(|row| row.iter().map(String::as_str).collect())
            .collect();
        console.render_table(&headers, &row_refs);

        let mut picker_input_override = picker_input_override;
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 3 {
                console.render_warning("No selection made. Starting a new session.");
                return Ok(Self::create_with_dir(Some(base_dir)));
            }

            print!(
                "Enter selection (1-{}, blank to start new): ",
                entries.len()
            );
            let _ = std::io::stdout().flush();

            let input = if let Some(override_input) = picker_input_override.take() {
                override_input
            } else {
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input
            };
            let input = input.trim();
            if input.is_empty() {
                console.render_info("Starting a new session.");
                return Ok(Self::create_with_dir(Some(base_dir)));
            }

            match input.parse::<usize>() {
                Ok(index) if index > 0 && index <= entries.len() => {
                    let selected = &entries[index - 1];
                    let mut session = Self::open(selected.path.to_string_lossy().as_ref()).await?;
                    session.session_dir = Some(base_dir.clone());
                    return Ok(session);
                }
                _ => {
                    console.render_warning("Invalid selection. Try again.");
                }
            }
        }
    }

    fn resolve_console_theme(config: &Config, cwd: &Path) -> Option<Theme> {
        let name = config.theme.as_deref()?;
        if name.trim().is_empty() {
            return None;
        }

        let direct_path = Path::new(name);
        if direct_path.exists() {
            if let Ok(theme) = Theme::read(direct_path, true) {
                return Some(theme);
            }
        }

        let project_dir = cwd.join(Config::project_dir()).join("themes");
        let global_dir = Config::global_dir().join("themes");
        for base in [project_dir, global_dir] {
            for ext in ["ini", "theme"] {
                let path = base.join(format!("{name}.{ext}"));
                if path.exists() {
                    if let Ok(theme) = Theme::read(&path, true) {
                        return Some(theme);
                    }
                }
            }
        }

        None
    }

    /// Create an in-memory (ephemeral) session.
    pub fn in_memory() -> Self {
        Self {
            header: SessionHeader::new(),
            entries: Vec::new(),
            path: None,
            leaf_id: None,
            session_dir: None,
        }
    }

    /// Create a new session.
    pub fn create() -> Self {
        Self::create_with_dir(None)
    }

    /// Create a new session with an optional base directory override.
    pub fn create_with_dir(session_dir: Option<PathBuf>) -> Self {
        let header = SessionHeader::new();
        Self {
            header,
            entries: Vec::new(),
            path: None,
            leaf_id: None,
            session_dir,
        }
    }

    /// Open an existing session.
    pub async fn open(path: &str) -> Result<Self> {
        let (session, diagnostics) = Self::open_with_diagnostics(path).await?;
        for warning in diagnostics.warning_lines() {
            eprintln!("{warning}");
        }
        Ok(session)
    }

    /// Open an existing session and return diagnostics about any recovered corruption.
    pub async fn open_with_diagnostics(path: &str) -> Result<(Self, SessionOpenDiagnostics)> {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(crate::Error::SessionNotFound {
                path: path.display().to_string(),
            });
        }

        let content = asupersync::fs::read_to_string(&path).await?;
        let mut lines = content.lines();

        // Parse header (first line)
        let header: SessionHeader = lines
            .next()
            .map(serde_json::from_str)
            .transpose()?
            .ok_or_else(|| crate::Error::session("Empty session file"))?;

        // Parse entries
        let mut entries = Vec::new();
        let mut diagnostics = SessionOpenDiagnostics::default();
        for (line_num, line) in lines.enumerate() {
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    diagnostics.skipped_entries.push(SessionOpenSkippedEntry {
                        line_number: line_num + 2, // +2 for 1-based indexing and header line
                        error: e.to_string(),
                    });
                }
            }
        }

        ensure_entry_ids(&mut entries);

        let leaf_id = entries.iter().rev().find_map(|e| e.base_id().cloned());

        Ok((
            Self {
                header,
                entries,
                path: Some(path),
                leaf_id,
                session_dir: None,
            },
            diagnostics,
        ))
    }

    /// Continue the most recent session.
    pub async fn continue_recent_in_dir(
        override_dir: Option<&Path>,
        _config: &Config,
    ) -> Result<Self> {
        let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
        let cwd = std::env::current_dir()?;
        let cwd_display = cwd.display().to_string();
        let encoded_cwd = encode_cwd(&cwd);
        let project_session_dir = base_dir.join(&encoded_cwd);

        if !project_session_dir.exists() {
            return Ok(Self::create_with_dir(Some(base_dir)));
        }

        // Prefer the session index for fast lookup.
        let index = SessionIndex::for_sessions_root(&base_dir);
        let mut indexed_sessions = index.list_sessions(Some(&cwd_display)).ok();

        if indexed_sessions
            .as_ref()
            .is_some_and(std::vec::Vec::is_empty)
            && index.reindex_all().is_ok()
        {
            indexed_sessions = index.list_sessions(Some(&cwd_display)).ok();
        }

        if let Some(list) = indexed_sessions {
            if let Some(meta) = list.first() {
                let mut session = Self::open(&meta.path).await?;
                session.session_dir = Some(base_dir);
                return Ok(session);
            }
        }

        // Fallback: scan the filesystem for the most recent session file.
        let mut entries: Vec<_> = std::fs::read_dir(&project_session_dir)?
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();

        entries.sort_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });

        if let Some(entry) = entries.pop() {
            let mut session = Self::open(entry.path().to_string_lossy().as_ref()).await?;
            session.session_dir = Some(base_dir);
            Ok(session)
        } else {
            Ok(Self::create_with_dir(Some(base_dir)))
        }
    }

    /// Save the session to disk.
    pub async fn save(&mut self) -> Result<()> {
        ensure_entry_ids(&mut self.entries);
        if self.path.is_none() {
            // Create a new path
            let base_dir = self
                .session_dir
                .clone()
                .unwrap_or_else(Config::sessions_dir);
            let cwd = std::env::current_dir()?;
            let encoded_cwd = encode_cwd(&cwd);
            let project_session_dir = base_dir.join(&encoded_cwd);

            asupersync::fs::create_dir_all(&project_session_dir).await?;

            let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ");
            let filename = format!("{}_{}.jsonl", timestamp, &self.header.id[..8]);
            self.path = Some(project_session_dir.join(filename));
        }

        let path = self.path.clone().unwrap();
        let mut content = String::new();

        // Write header
        content.push_str(&serde_json::to_string(&self.header)?);
        content.push('\n');

        // Write entries
        for entry in &self.entries {
            content.push_str(&serde_json::to_string(entry)?);
            content.push('\n');
        }

        let session_clone = self.clone();
        let path_clone = path.clone();
        let content_clone = content.clone();
        let session_dir_clone = self.session_dir.clone();

        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = || -> Result<()> {
                let parent = path_clone.parent().unwrap_or_else(|| Path::new("."));
                let temp_file = tempfile::NamedTempFile::new_in(parent)?;
                std::fs::write(temp_file.path(), content_clone)?;
                temp_file
                    .persist(&path_clone)
                    .map_err(|e| crate::Error::Io(Box::new(e.error)))?;

                let sessions_root = session_dir_clone.unwrap_or_else(Config::sessions_dir);
                if let Err(err) =
                    SessionIndex::for_sessions_root(&sessions_root).index_session(&session_clone)
                {
                    tracing::warn!("Failed to update session index: {err}");
                }
                Ok(())
            }();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| crate::Error::session("Save task cancelled"))??;
        Ok(())
    }

    /// Append a session message entry.
    pub fn append_message(&mut self, message: SessionMessage) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Message(MessageEntry { base, message });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    /// Append a message from the model message types.
    pub fn append_model_message(&mut self, message: Message) -> String {
        self.append_message(SessionMessage::from(message))
    }

    pub fn append_model_change(&mut self, provider: String, model_id: String) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base,
            provider,
            model_id,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    pub fn append_thinking_level_change(&mut self, thinking_level: String) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            base,
            thinking_level,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    pub fn append_session_info(&mut self, name: Option<String>) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::SessionInfo(SessionInfoEntry { base, name });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    /// Append a custom entry (extension state, etc).
    pub fn append_custom_entry(
        &mut self,
        custom_type: String,
        data: Option<serde_json::Value>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Custom(CustomEntry {
            base,
            custom_type,
            data,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    pub fn append_bash_execution(
        &mut self,
        command: String,
        output: String,
        exit_code: i32,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<String>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Message(MessageEntry {
            base,
            message: SessionMessage::BashExecution {
                command,
                output,
                exit_code,
                cancelled: Some(cancelled),
                truncated: Some(truncated),
                full_output_path,
                timestamp: Some(chrono::Utc::now().timestamp_millis()),
                extra: HashMap::new(),
            },
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    /// Get the current session name from the most recent SessionInfo entry.
    pub fn get_name(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| {
            if let SessionEntry::SessionInfo(info) = entry {
                info.name.clone()
            } else {
                None
            }
        })
    }

    /// Set the session name by appending a SessionInfo entry.
    pub fn set_name(&mut self, name: &str) -> String {
        self.append_session_info(Some(name.to_string()))
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Compaction(CompactionEntry {
            base,
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    pub fn append_branch_summary(
        &mut self,
        from_id: String,
        summary: String,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::BranchSummary(BranchSummaryEntry {
            base,
            from_id,
            summary,
            details,
            from_hook,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }

    pub fn ensure_entry_ids(&mut self) {
        ensure_entry_ids(&mut self.entries);
    }

    /// Convert session entries to model messages (for provider context).
    pub fn to_messages(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        for entry in &self.entries {
            if let SessionEntry::Message(msg_entry) = entry {
                if let Some(message) = session_message_to_model(&msg_entry.message) {
                    messages.push(message);
                }
            }
        }
        messages
    }

    /// Render the session as a standalone HTML document.
    pub fn to_html(&self) -> String {
        let mut html = String::new();
        html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
        html.push_str("<title>Pi Session</title>");
        html.push_str("<style>");
        html.push_str(
            "body{font-family:system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;margin:24px;background:#0b0c10;color:#e6e6e6;}
            h1{margin:0 0 8px 0;}
            .meta{color:#9aa0a6;margin-bottom:24px;font-size:14px;}
            .msg{padding:16px 18px;margin:12px 0;border-radius:8px;background:#14161b;}
            .msg.user{border-left:4px solid #4fc3f7;}
            .msg.assistant{border-left:4px solid #81c784;}
            .msg.tool{border-left:4px solid #ffb74d;}
            .msg.system{border-left:4px solid #ef9a9a;}
            .role{font-weight:600;margin-bottom:8px;}
            pre{white-space:pre-wrap;background:#0f1115;padding:12px;border-radius:6px;overflow:auto;}
            .thinking summary{cursor:pointer;}
            img{max-width:100%;height:auto;border-radius:6px;margin-top:8px;}
            .note{color:#9aa0a6;font-size:13px;margin:6px 0;}
            ",
        );
        html.push_str("</style></head><body>");

        let _ = write!(
            html,
            "<h1>Pi Session</h1><div class=\"meta\">Session {} • {} • cwd: {}</div>",
            escape_html(&self.header.id),
            escape_html(&self.header.timestamp),
            escape_html(&self.header.cwd)
        );

        for entry in &self.entries {
            match entry {
                SessionEntry::Message(message) => {
                    html.push_str(&render_session_message(&message.message));
                }
                SessionEntry::ModelChange(change) => {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">Model</div><div class=\"note\">{} / {}</div></div>",
                        escape_html(&change.provider),
                        escape_html(&change.model_id)
                    );
                }
                SessionEntry::ThinkingLevelChange(change) => {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">Thinking</div><div class=\"note\">{}</div></div>",
                        escape_html(&change.thinking_level)
                    );
                }
                SessionEntry::Compaction(compaction) => {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">Compaction</div><pre>{}</pre></div>",
                        escape_html(&compaction.summary)
                    );
                }
                SessionEntry::BranchSummary(summary) => {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">Branch Summary</div><pre>{}</pre></div>",
                        escape_html(&summary.summary)
                    );
                }
                SessionEntry::SessionInfo(info) => {
                    if let Some(name) = &info.name {
                        let _ = write!(
                            html,
                            "<div class=\"msg system\"><div class=\"role\">Session Name</div><div class=\"note\">{}</div></div>",
                            escape_html(name)
                        );
                    }
                }
                SessionEntry::Custom(custom) => {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">{}</div></div>",
                        escape_html(&custom.custom_type)
                    );
                }
                SessionEntry::Label(_) => {}
            }
        }

        html.push_str("</body></html>");
        html
    }

    /// Update header model info.
    pub fn set_model_header(
        &mut self,
        provider: Option<String>,
        model_id: Option<String>,
        thinking_level: Option<String>,
    ) {
        if provider.is_some() {
            self.header.provider = provider;
        }
        if model_id.is_some() {
            self.header.model_id = model_id;
        }
        if thinking_level.is_some() {
            self.header.thinking_level = thinking_level;
        }
    }

    pub fn set_branched_from(&mut self, path: Option<String>) {
        self.header.parent_session = path;
    }

    /// Plan a `/fork` from a user message entry ID.
    ///
    /// Returns the entries to copy into a new session (path to the parent of the selected
    /// user message), the new leaf id, and the selected user message text for editor pre-fill.
    pub fn plan_fork_from_user_message(&self, entry_id: &str) -> Result<ForkPlan> {
        let entry = self
            .get_entry(entry_id)
            .ok_or_else(|| Error::session(format!("Fork target not found: {entry_id}")))?;

        let SessionEntry::Message(message_entry) = entry else {
            return Err(Error::session(format!(
                "Fork target is not a message entry: {entry_id}"
            )));
        };

        let SessionMessage::User { content, .. } = &message_entry.message else {
            return Err(Error::session(format!(
                "Fork target is not a user message: {entry_id}"
            )));
        };

        let selected_text = user_content_to_text(content);
        let leaf_id = message_entry.base.parent_id.clone();

        let entries = if let Some(ref leaf_id) = leaf_id {
            let path_ids = self.get_path_to_entry(leaf_id);
            let mut entries = Vec::new();
            for path_id in path_ids {
                let entry = self.get_entry(&path_id).ok_or_else(|| {
                    Error::session(format!("Failed to build fork: missing entry {path_id}"))
                })?;
                entries.push(entry.clone());
            }
            entries
        } else {
            Vec::new()
        };

        Ok(ForkPlan {
            entries,
            leaf_id,
            selected_text,
        })
    }

    fn next_entry_id(&self) -> String {
        let existing = entry_id_set(&self.entries);
        generate_entry_id(&existing)
    }

    // ========================================================================
    // Tree Navigation
    // ========================================================================

    /// Build a map from entry ID to its parent ID.
    fn build_parent_map(&self) -> HashMap<String, Option<String>> {
        self.entries
            .iter()
            .filter_map(|e| {
                e.base_id()
                    .map(|id| (id.clone(), e.base().parent_id.clone()))
            })
            .collect()
    }

    /// Build a map from parent ID to children IDs.
    fn build_children_map(&self) -> HashMap<Option<String>, Vec<String>> {
        let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        for entry in &self.entries {
            if let Some(id) = entry.base_id() {
                children
                    .entry(entry.base().parent_id.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        children
    }

    /// Get the path from an entry back to the root (inclusive).
    /// Returns entry IDs in order from root to the specified entry.
    pub fn get_path_to_entry(&self, entry_id: &str) -> Vec<String> {
        let parent_map = self.build_parent_map();
        let mut path = Vec::new();
        let mut current = Some(entry_id.to_string());

        while let Some(id) = current {
            path.push(id.clone());
            current = parent_map.get(&id).and_then(Clone::clone);
        }

        path.reverse();
        path
    }

    /// Get direct children of an entry.
    pub fn get_children(&self, entry_id: Option<&str>) -> Vec<String> {
        let children_map = self.build_children_map();
        let key = entry_id.map(String::from);
        children_map.get(&key).cloned().unwrap_or_default()
    }

    /// List all leaf nodes (entries with no children).
    pub fn list_leaves(&self) -> Vec<String> {
        let children_map = self.build_children_map();
        self.entries
            .iter()
            .filter_map(|e| {
                let id = e.base_id()?;
                // An entry is a leaf if it has no children
                if children_map
                    .get(&Some(id.clone()))
                    .is_none_or(Vec::is_empty)
                {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Navigate to a specific entry, making it the current leaf.
    /// Returns true if the entry exists.
    pub fn navigate_to(&mut self, entry_id: &str) -> bool {
        let exists = self
            .entries
            .iter()
            .any(|e| e.base_id().is_some_and(|id| id == entry_id));
        if exists {
            self.leaf_id = Some(entry_id.to_string());
            true
        } else {
            false
        }
    }

    /// Reset the leaf pointer to root (before any entries).
    ///
    /// After calling this, the next appended entry will become a new root entry
    /// (`parent_id = None`). This is used by interactive `/tree` navigation when
    /// re-editing the first user message.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// Create a new branch starting from a specific entry.
    /// Sets the leaf_id to the specified entry so new entries branch from there.
    /// Returns true if the entry exists.
    pub fn create_branch_from(&mut self, entry_id: &str) -> bool {
        self.navigate_to(entry_id)
    }

    /// Get the entry at a specific ID.
    pub fn get_entry(&self, entry_id: &str) -> Option<&SessionEntry> {
        self.entries
            .iter()
            .find(|e| e.base_id().is_some_and(|id| id == entry_id))
    }

    /// Get the entry at a specific ID (mutable).
    pub fn get_entry_mut(&mut self, entry_id: &str) -> Option<&mut SessionEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.base_id().is_some_and(|id| id == entry_id))
    }

    /// Entries along the current leaf path, in chronological order.
    pub fn entries_for_current_path(&self) -> Vec<&SessionEntry> {
        let Some(leaf_id) = &self.leaf_id else {
            return Vec::new();
        };

        let path = self.get_path_to_entry(leaf_id);
        let path_set: HashSet<&str> = path.iter().map(String::as_str).collect();

        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .base_id()
                    .is_some_and(|id| path_set.contains(id.as_str()))
            })
            .collect()
    }

    /// Convert session entries along the current path to model messages.
    /// This follows parent_id links from leaf_id back to root.
    pub fn to_messages_for_current_path(&self) -> Vec<Message> {
        let path_entries = self.entries_for_current_path();

        // If the current path contains a compaction entry, omit older messages
        // and insert the compaction summary before the kept region.
        let last_compaction = path_entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction(compaction) => Some(compaction),
            _ => None,
        });

        if let Some(compaction) = last_compaction {
            let mut messages = Vec::new();

            let summary_message = SessionMessage::CompactionSummary {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
            };
            if let Some(message) = session_message_to_model(&summary_message) {
                messages.push(message);
            }

            let mut keep = false;
            for entry in path_entries {
                if !keep {
                    if entry
                        .base_id()
                        .is_some_and(|id| id == &compaction.first_kept_entry_id)
                    {
                        keep = true;
                    } else {
                        continue;
                    }
                }

                match entry {
                    SessionEntry::Message(msg_entry) => {
                        if let Some(message) = session_message_to_model(&msg_entry.message) {
                            messages.push(message);
                        }
                    }
                    SessionEntry::BranchSummary(summary) => {
                        let summary_message = SessionMessage::BranchSummary {
                            summary: summary.summary.clone(),
                            from_id: summary.from_id.clone(),
                        };
                        if let Some(message) = session_message_to_model(&summary_message) {
                            messages.push(message);
                        }
                    }
                    _ => {}
                }
            }

            return messages;
        }

        let mut messages = Vec::new();
        for entry in path_entries {
            match entry {
                SessionEntry::Message(msg_entry) => {
                    if let Some(message) = session_message_to_model(&msg_entry.message) {
                        messages.push(message);
                    }
                }
                SessionEntry::BranchSummary(summary) => {
                    let summary_message = SessionMessage::BranchSummary {
                        summary: summary.summary.clone(),
                        from_id: summary.from_id.clone(),
                    };
                    if let Some(message) = session_message_to_model(&summary_message) {
                        messages.push(message);
                    }
                }
                _ => {}
            }
        }
        messages
    }

    /// Get a summary of branches in this session.
    pub fn branch_summary(&self) -> BranchInfo {
        let leaves = self.list_leaves();
        let children_map = self.build_children_map();

        // Find branch points (entries with multiple children)
        let branch_points: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| {
                let id = e.base_id()?;
                let children = children_map.get(&Some(id.clone()))?;
                if children.len() > 1 {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        BranchInfo {
            total_entries: self.entries.len(),
            leaf_count: leaves.len(),
            branch_point_count: branch_points.len(),
            current_leaf: self.leaf_id.clone(),
            leaves,
            branch_points,
        }
    }

    /// Add a label to an entry.
    pub fn add_label(&mut self, target_id: &str, label: Option<String>) -> Option<String> {
        // Verify target exists
        self.get_entry(target_id)?;

        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Label(LabelEntry {
            base,
            target_id: target_id.to_string(),
            label,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        Some(id)
    }
}

/// Summary of branches in a session.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub total_entries: usize,
    pub leaf_count: usize,
    pub branch_point_count: usize,
    pub current_leaf: Option<String>,
    pub leaves: Vec<String>,
    pub branch_points: Vec<String>,
}

#[derive(Debug, Clone)]
struct SessionPickEntry {
    path: PathBuf,
    id: String,
    timestamp: String,
    message_count: u64,
    name: Option<String>,
    last_modified_ms: i64,
}

impl SessionPickEntry {
    fn from_meta(meta: crate::session_index::SessionMeta) -> Option<Self> {
        let path = PathBuf::from(meta.path);
        if !path.exists() {
            return None;
        }
        Some(Self {
            path,
            id: meta.id,
            timestamp: meta.timestamp,
            message_count: meta.message_count,
            name: meta.name,
            last_modified_ms: meta.last_modified_ms,
        })
    }
}

fn scan_sessions_on_disk(project_session_dir: &Path) -> Result<Vec<SessionPickEntry>> {
    let mut entries = Vec::new();
    let dir_entries = std::fs::read_dir(project_session_dir)
        .map_err(|e| Error::session(format!("Failed to read sessions: {e}")))?;
    for entry in dir_entries {
        let entry = entry.map_err(|e| Error::session(format!("Read dir entry: {e}")))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Ok(meta) = load_session_meta(&path) {
                entries.push(meta);
            }
        }
    }
    Ok(entries)
}

fn load_session_meta(path: &Path) -> Result<SessionPickEntry> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::session(format!("Failed to read session: {e}")))?;
    let mut lines = content.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| Error::session("Empty session file"))?;
    let header: SessionHeader =
        serde_json::from_str(header_line).map_err(|e| Error::session(format!("{e}")))?;

    let mut message_count = 0u64;
    let mut name = None;
    for line in lines {
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
            match entry {
                SessionEntry::Message(_) => message_count += 1,
                SessionEntry::SessionInfo(info) => {
                    if info.name.is_some() {
                        name = info.name;
                    }
                }
                _ => {}
            }
        }
    }

    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    #[allow(clippy::cast_possible_truncation)]
    let last_modified_ms = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64; // i64::MAX ms = ~292 million years, so truncation is safe

    Ok(SessionPickEntry {
        path: path.to_path_buf(),
        id: header.id,
        timestamp: header.timestamp,
        message_count,
        name,
        last_modified_ms,
    })
}

// ============================================================================
// Session Header
// ============================================================================

/// Session file header.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u8>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "branchedFrom",
        alias = "parentSession"
    )]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new() -> Self {
        let now = chrono::Utc::now();
        Self {
            r#type: "session".to_string(),
            version: Some(SESSION_VERSION),
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            provider: None,
            model_id: None,
            thinking_level: None,
            parent_session: None,
        }
    }
}

impl Default for SessionHeader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Session Entries
// ============================================================================

/// A session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message(MessageEntry),
    ModelChange(ModelChangeEntry),
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Label(LabelEntry),
    SessionInfo(SessionInfoEntry),
    Custom(CustomEntry),
}

impl SessionEntry {
    pub const fn base(&self) -> &EntryBase {
        match self {
            Self::Message(e) => &e.base,
            Self::ModelChange(e) => &e.base,
            Self::ThinkingLevelChange(e) => &e.base,
            Self::Compaction(e) => &e.base,
            Self::BranchSummary(e) => &e.base,
            Self::Label(e) => &e.base,
            Self::SessionInfo(e) => &e.base,
            Self::Custom(e) => &e.base,
        }
    }

    pub const fn base_mut(&mut self) -> &mut EntryBase {
        match self {
            Self::Message(e) => &mut e.base,
            Self::ModelChange(e) => &mut e.base,
            Self::ThinkingLevelChange(e) => &mut e.base,
            Self::Compaction(e) => &mut e.base,
            Self::BranchSummary(e) => &mut e.base,
            Self::Label(e) => &mut e.base,
            Self::SessionInfo(e) => &mut e.base,
            Self::Custom(e) => &mut e.base,
        }
    }

    pub const fn base_id(&self) -> Option<&String> {
        self.base().id.as_ref()
    }
}

/// Base entry fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub timestamp: String,
}

impl EntryBase {
    pub fn new(parent_id: Option<String>, id: String) -> Self {
        Self {
            id: Some(id),
            parent_id,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

/// Message entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub message: SessionMessage,
}

/// Session message payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "role",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionMessage {
    User {
        content: UserContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
    },
    Assistant {
        #[serde(flatten)]
        message: AssistantMessage,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default)]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
    },
    Custom {
        custom_type: String,
        content: String,
        #[serde(default)]
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    BashExecution {
        command: String,
        output: String,
        exit_code: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancelled: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        full_output_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    BranchSummary {
        summary: String,
        from_id: String,
    },
    CompactionSummary {
        summary: String,
        tokens_before: u64,
    },
}

impl From<Message> for SessionMessage {
    fn from(message: Message) -> Self {
        match message {
            Message::User(user) => Self::User {
                content: user.content,
                timestamp: Some(user.timestamp),
            },
            Message::Assistant(assistant) => Self::Assistant { message: assistant },
            Message::ToolResult(result) => Self::ToolResult {
                tool_call_id: result.tool_call_id,
                tool_name: result.tool_name,
                content: result.content,
                details: result.details,
                is_error: result.is_error,
                timestamp: Some(result.timestamp),
            },
            Message::Custom(custom) => Self::Custom {
                custom_type: custom.custom_type,
                content: custom.content,
                display: custom.display,
                details: custom.details,
            },
        }
    }
}

/// Model change entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub provider: String,
    pub model_id: String,
}

/// Thinking level change entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub thinking_level: String,
}

/// Compaction entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Branch summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub from_id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Label entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Session info entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Custom entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ============================================================================
// Utilities
// ============================================================================

/// Encode a working directory path for use in session directory names.
pub fn encode_cwd(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    let s = s.trim_start_matches(['/', '\\']);
    let s = s.replace(['/', '\\', ':'], "-");
    format!("--{s}--")
}

pub(crate) fn session_message_to_model(message: &SessionMessage) -> Option<Message> {
    match message {
        SessionMessage::User { content, timestamp } => Some(Message::User(UserMessage {
            content: content.clone(),
            timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        })),
        SessionMessage::Assistant { message } => Some(Message::Assistant(message.clone())),
        SessionMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            is_error,
            timestamp,
        } => Some(Message::ToolResult(ToolResultMessage {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            content: content.clone(),
            details: details.clone(),
            is_error: *is_error,
            timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        })),
        SessionMessage::Custom {
            custom_type,
            content,
            display,
            details,
        } => Some(Message::Custom(crate::model::CustomMessage {
            content: content.clone(),
            custom_type: custom_type.clone(),
            display: *display,
            details: details.clone(),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })),
        SessionMessage::BashExecution {
            command,
            output,
            exit_code,
            cancelled,
            truncated,
            full_output_path,
            timestamp,
            extra,
        } => {
            if extra
                .get("excludeFromContext")
                .and_then(Value::as_bool)
                .is_some_and(|v| v)
            {
                return None;
            }
            let text = bash_execution_to_text(
                command,
                output,
                *exit_code,
                cancelled.unwrap_or(false),
                truncated.unwrap_or(false),
                full_output_path.as_deref(),
            );
            Some(Message::User(UserMessage {
                content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(text))]),
                timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            }))
        }
        SessionMessage::BranchSummary { summary, .. } => Some(Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(format!(
                "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
            )))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })),
        SessionMessage::CompactionSummary { summary, .. } => Some(Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(format!(
                "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
            )))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })),
    }
}

const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

pub(crate) fn bash_execution_to_text(
    command: &str,
    output: &str,
    exit_code: i32,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<&str>,
) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }

    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if exit_code != 0 {
        let _ = write!(text, "\n\nCommand exited with code {exit_code}");
    }

    if truncated {
        if let Some(path) = full_output_path {
            let _ = write!(text, "\n\n[Output truncated. Full output: {path}]");
        }
    }

    text
}

fn render_session_message(message: &SessionMessage) -> String {
    match message {
        SessionMessage::User { content, .. } => {
            let mut html = String::new();
            html.push_str("<div class=\"msg user\"><div class=\"role\">User</div>");
            html.push_str(&render_user_content(content));
            html.push_str("</div>");
            html
        }
        SessionMessage::Assistant { message } => {
            let mut html = String::new();
            html.push_str("<div class=\"msg assistant\"><div class=\"role\">Assistant</div>");
            html.push_str(&render_blocks(&message.content));
            html.push_str("</div>");
            html
        }
        SessionMessage::ToolResult {
            tool_name,
            content,
            is_error,
            details,
            ..
        } => {
            let mut html = String::new();
            let role = if *is_error { "Tool Error" } else { "Tool" };
            let _ = write!(
                html,
                "<div class=\"msg tool\"><div class=\"role\">{}: {}</div>",
                role,
                escape_html(tool_name)
            );
            html.push_str(&render_blocks(content));
            if let Some(details) = details {
                let details_str =
                    serde_json::to_string_pretty(details).unwrap_or_else(|_| details.to_string());
                let _ = write!(html, "<pre>{}</pre>", escape_html(&details_str));
            }
            html.push_str("</div>");
            html
        }
        SessionMessage::Custom {
            custom_type,
            content,
            ..
        } => {
            let mut html = String::new();
            let _ = write!(
                html,
                "<div class=\"msg system\"><div class=\"role\">{}</div><pre>{}</pre></div>",
                escape_html(custom_type),
                escape_html(content)
            );
            html
        }
        SessionMessage::BashExecution {
            command,
            output,
            exit_code,
            ..
        } => {
            let mut html = String::new();
            let _ = write!(
                html,
                "<div class=\"msg tool\"><div class=\"role\">Bash (exit {exit_code})</div><pre>{}</pre><pre>{}</pre></div>",
                escape_html(command),
                escape_html(output)
            );
            html
        }
        SessionMessage::BranchSummary { summary, .. } => {
            format!(
                "<div class=\"msg system\"><div class=\"role\">Branch Summary</div><pre>{}</pre></div>",
                escape_html(summary)
            )
        }
        SessionMessage::CompactionSummary { summary, .. } => {
            format!(
                "<div class=\"msg system\"><div class=\"role\">Compaction</div><pre>{}</pre></div>",
                escape_html(summary)
            )
        }
    }
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => format!("<pre>{}</pre>", escape_html(text)),
        UserContent::Blocks(blocks) => render_blocks(blocks),
    }
}

fn render_blocks(blocks: &[ContentBlock]) -> String {
    let mut html = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                let _ = write!(html, "<pre>{}</pre>", escape_html(&text.text));
            }
            ContentBlock::Thinking(thinking) => {
                let _ = write!(
                    html,
                    "<details class=\"thinking\"><summary>Thinking</summary><pre>{}</pre></details>",
                    escape_html(&thinking.thinking)
                );
            }
            ContentBlock::Image(image) => {
                let _ = write!(
                    html,
                    "<img src=\"data:{};base64,{}\" alt=\"image\"/>",
                    escape_html(&image.mime_type),
                    escape_html(&image.data)
                );
            }
            ContentBlock::ToolCall(tool_call) => {
                let args = serde_json::to_string_pretty(&tool_call.arguments)
                    .unwrap_or_else(|_| tool_call.arguments.to_string());
                let _ = write!(
                    html,
                    "<div class=\"note\">Tool call: {}</div><pre>{}</pre>",
                    escape_html(&tool_call.name),
                    escape_html(&args)
                );
            }
        }
    }
    html
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                push_line(&mut output, &format!("[image: {}]", image.mime_type));
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

fn push_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn entry_id_set(entries: &[SessionEntry]) -> HashSet<String> {
    entries
        .iter()
        .filter_map(|e| e.base_id().cloned())
        .collect()
}

fn ensure_entry_ids(entries: &mut [SessionEntry]) {
    let mut existing = entry_id_set(entries);
    for entry in entries.iter_mut() {
        if entry.base().id.is_none() {
            let id = generate_entry_id(&existing);
            entry.base_mut().id = Some(id.clone());
            existing.insert(id);
        }
    }
}

/// Generate a unique entry ID (8 hex characters), falling back to UUID on collision.
fn generate_entry_id(existing: &HashSet<String>) -> String {
    for _ in 0..100 {
        let uuid = uuid::Uuid::new_v4();
        let id = uuid.simple().to_string()[..8].to_string();
        if !existing.contains(&id) {
            return id;
        }
    }
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{StopReason, Usage};
    use asupersync::runtime::RuntimeBuilder;
    use std::future::Future;

    fn make_test_message(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn run_async<T>(future: impl Future<Output = T>) -> T {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    #[test]
    fn test_get_share_viewer_url_matches_legacy() {
        assert_eq!(
            build_share_viewer_url(None, "gist-123"),
            "https://buildwithpi.ai/session/#gist-123"
        );
        assert_eq!(
            build_share_viewer_url(Some("https://example.com/session/"), "gist-123"),
            "https://example.com/session/#gist-123"
        );
        assert_eq!(
            build_share_viewer_url(Some("https://example.com/session"), "gist-123"),
            "https://example.com/session#gist-123"
        );
        // Legacy JS uses `process.env.PI_SHARE_VIEWER_URL || DEFAULT`, so empty-string should
        // fall back to default.
        assert_eq!(
            build_share_viewer_url(Some(""), "gist-123"),
            "https://buildwithpi.ai/session/#gist-123"
        );
    }

    #[test]
    fn test_session_linear_history() {
        let mut session = Session::in_memory();

        let id1 = session.append_message(make_test_message("Hello"));
        let id2 = session.append_message(make_test_message("World"));
        let id3 = session.append_message(make_test_message("Test"));

        // Check leaf is the last entry
        assert_eq!(session.leaf_id.as_deref(), Some(id3.as_str()));

        // Check path from last entry
        let path = session.get_path_to_entry(&id3);
        assert_eq!(path, vec![id1.as_str(), id2.as_str(), id3.as_str()]);

        // Check only one leaf
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0], id3);
    }

    #[test]
    fn test_session_branching() {
        let mut session = Session::in_memory();

        // Create linear history: A -> B -> C
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let id_c = session.append_message(make_test_message("C"));

        // Now branch from B: A -> B -> D
        assert!(session.create_branch_from(&id_b));
        let id_d = session.append_message(make_test_message("D"));

        // Should have 2 leaves: C and D
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&id_c));
        assert!(leaves.contains(&id_d));

        // Path to D should be A -> B -> D
        let path_to_d = session.get_path_to_entry(&id_d);
        assert_eq!(path_to_d, vec![id_a.as_str(), id_b.as_str(), id_d.as_str()]);

        // Path to C should be A -> B -> C
        let path_to_c = session.get_path_to_entry(&id_c);
        assert_eq!(path_to_c, vec![id_a.as_str(), id_b.as_str(), id_c.as_str()]);
    }

    #[test]
    fn test_session_navigation() {
        let mut session = Session::in_memory();

        let id1 = session.append_message(make_test_message("First"));
        let id2 = session.append_message(make_test_message("Second"));

        // Navigate to first entry
        assert!(session.navigate_to(&id1));
        assert_eq!(session.leaf_id.as_deref(), Some(id1.as_str()));

        // Navigate to non-existent entry
        assert!(!session.navigate_to("nonexistent"));
        // leaf_id unchanged
        assert_eq!(session.leaf_id.as_deref(), Some(id1.as_str()));

        // Navigate back to second
        assert!(session.navigate_to(&id2));
        assert_eq!(session.leaf_id.as_deref(), Some(id2.as_str()));
    }

    #[test]
    fn test_session_get_children() {
        let mut session = Session::in_memory();

        // A -> B -> C
        //   -> D
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Branch from A
        session.create_branch_from(&id_a);
        let id_d = session.append_message(make_test_message("D"));

        // A should have 2 children: B and D
        let children_a = session.get_children(Some(&id_a));
        assert_eq!(children_a.len(), 2);
        assert!(children_a.contains(&id_b));
        assert!(children_a.contains(&id_d));

        // Root (None) should have 1 child: A
        let root_children = session.get_children(None);
        assert_eq!(root_children.len(), 1);
        assert_eq!(root_children[0], id_a);
    }

    #[test]
    fn test_branch_summary() {
        let mut session = Session::in_memory();

        // Linear: A -> B
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));

        let info = session.branch_summary();
        assert_eq!(info.total_entries, 2);
        assert_eq!(info.leaf_count, 1);
        assert_eq!(info.branch_point_count, 0);

        // Create branch: A -> B, A -> C
        session.create_branch_from(&id_a);
        let _id_c = session.append_message(make_test_message("C"));

        let info = session.branch_summary();
        assert_eq!(info.total_entries, 3);
        assert_eq!(info.leaf_count, 2);
        assert_eq!(info.branch_point_count, 1);
        assert!(info.branch_points.contains(&id_a));
        assert!(info.leaves.contains(&id_b));
    }

    #[test]
    fn test_session_jsonl_serialization() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-test".to_string());
        session.header.thinking_level = Some("medium".to_string());

        let user_id = session.append_message(make_test_message("Hello"));
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Hi!"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });
        session.append_model_change("anthropic".to_string(), "claude-test".to_string());
        session.append_thinking_level_change("high".to_string());
        session.append_compaction("summary".to_string(), user_id.clone(), 123, None, None);
        session.append_branch_summary(user_id, "branch".to_string(), None, None);
        session.append_session_info(Some("my-session".to_string()));

        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let mut lines = contents.lines();

        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], SESSION_VERSION);

        let mut types = Vec::new();
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let entry_type = value["type"].as_str().unwrap_or_default().to_string();
            types.push(entry_type);
        }

        assert!(types.contains(&"message".to_string()));
        assert!(types.contains(&"model_change".to_string()));
        assert!(types.contains(&"thinking_level_change".to_string()));
        assert!(types.contains(&"compaction".to_string()));
        assert!(types.contains(&"branch_summary".to_string()));
        assert!(types.contains(&"session_info".to_string()));
    }

    #[test]
    fn test_open_with_diagnostics_skips_corrupted_last_entry_and_recovers_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let first_id = session.append_message(make_test_message("Hello"));
        let second_id = session.append_message(make_test_message("World"));
        assert_eq!(session.leaf_id.as_deref(), Some(second_id.as_str()));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path set");

        let mut lines = std::fs::read_to_string(&path)
            .expect("read session")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(lines.len() >= 3, "expected header + 2 entries");

        let corrupted_line_number = lines.len(); // 1-based
        let last_index = lines.len() - 1;
        lines[last_index] = "{ this is not json }".to_string();

        let corrupted_path = temp.path().join("corrupted.jsonl");
        std::fs::write(&corrupted_path, format!("{}\n", lines.join("\n")))
            .expect("write corrupted session");

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(corrupted_path.to_string_lossy().as_ref()).await
        })
        .expect("open corrupted session");

        assert_eq!(diagnostics.skipped_entries.len(), 1);
        assert_eq!(
            diagnostics.skipped_entries[0].line_number,
            corrupted_line_number
        );

        let warnings = diagnostics.warning_lines();
        assert_eq!(warnings.len(), 2, "expected per-line warning + summary");
        assert!(
            warnings[0].starts_with(&format!(
                "Warning: Skipping corrupted entry at line {corrupted_line_number} in session file:"
            )),
            "unexpected warning: {}",
            warnings[0]
        );
        assert_eq!(
            warnings[1],
            "Warning: Skipped 1 corrupted entries while loading session"
        );

        assert_eq!(
            loaded.entries.len(),
            session.entries.len() - 1,
            "expected last entry to be dropped"
        );
        assert_eq!(loaded.leaf_id.as_deref(), Some(first_id.as_str()));
    }

    #[test]
    fn test_save_and_open_round_trip_preserves_compaction_and_branch_summary() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let root_id = session.append_message(make_test_message("Hello"));
        session.append_compaction("compacted".to_string(), root_id.clone(), 123, None, None);
        session.append_branch_summary(root_id, "branch summary".to_string(), None, None);

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path set");

        let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect("reopen session");

        assert!(loaded.entries.iter().any(|entry| {
            matches!(entry, SessionEntry::Compaction(compaction) if compaction.summary == "compacted" && compaction.tokens_before == 123)
        }));
        assert!(loaded.entries.iter().any(|entry| {
            matches!(entry, SessionEntry::BranchSummary(summary) if summary.summary == "branch summary")
        }));

        let html = loaded.to_html();
        assert!(html.contains("compacted"));
        assert!(html.contains("branch summary"));
    }

    #[test]
    fn test_concurrent_saves_do_not_corrupt_session_file_unit() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path().join("sessions");

        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_test_message("Hello"));

        run_async(async { session.save().await }).expect("initial save");
        let path = session.path.clone().expect("session path set");

        let path1 = path.clone();
        let path2 = path.clone();

        let t1 = std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            runtime.block_on(async move {
                let mut s = Session::open(path1.to_string_lossy().as_ref())
                    .await
                    .expect("open session");
                s.append_message(make_test_message("From thread 1"));
                s.save().await
            })
        });

        let t2 = std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            runtime.block_on(async move {
                let mut s = Session::open(path2.to_string_lossy().as_ref())
                    .await
                    .expect("open session");
                s.append_message(make_test_message("From thread 2"));
                s.save().await
            })
        });

        let r1 = t1.join().expect("thread 1 join");
        let r2 = t2.join().expect("thread 2 join");
        assert!(
            r1.is_ok() || r2.is_ok(),
            "Expected at least one save to succeed: r1={r1:?} r2={r2:?}"
        );

        let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect("open after concurrent saves");
        assert!(!loaded.entries.is_empty());
    }

    #[test]
    fn test_to_messages_for_current_path() {
        let mut session = Session::in_memory();

        // Tree structure:
        // A -> B -> C
        //       \-> D  (D branches from B)
        let _id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Navigate to B and add D
        session.create_branch_from(&id_b);
        let id_d = session.append_message(make_test_message("D"));

        // Current path should be A -> B -> D
        session.navigate_to(&id_d);
        let messages = session.to_messages_for_current_path();
        assert_eq!(messages.len(), 3);

        // Verify content
        if let Message::User(user) = &messages[0] {
            if let UserContent::Text(text) = &user.content {
                assert_eq!(text, "A");
            }
        }
        if let Message::User(user) = &messages[2] {
            if let UserContent::Text(text) = &user.content {
                assert_eq!(text, "D");
            }
        }
    }

    #[test]
    fn test_reset_leaf_produces_empty_current_path() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("A"));
        let _id_b = session.append_message(make_test_message("B"));

        session.reset_leaf();
        assert!(session.entries_for_current_path().is_empty());
        assert!(session.to_messages_for_current_path().is_empty());

        // After reset, the next entry becomes a new root.
        let id_root = session.append_message(make_test_message("Root"));
        let entry = session.get_entry(&id_root).expect("entry");
        assert!(entry.base().parent_id.is_none());
    }

    #[test]
    fn test_encode_cwd() {
        let path = std::path::Path::new("/home/user/project");
        let encoded = encode_cwd(path);
        assert!(encoded.starts_with("--"));
        assert!(encoded.ends_with("--"));
        assert!(encoded.contains("home-user-project"));
    }
}
