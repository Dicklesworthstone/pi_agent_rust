//! Session picker TUI for selecting from available sessions.
//!
//! Provides an interactive list for choosing which session to resume.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bubbletea::{Cmd, KeyMsg, KeyType, Message, Program, quit};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionHeader, encode_cwd};
use crate::session_index::{SessionIndex, SessionMeta};
use crate::theme::{Theme, TuiStyles};

/// Format a timestamp for display.
pub fn format_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp).map_or_else(
        |_| timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// The session picker TUI model.
#[derive(bubbletea::Model)]
pub struct SessionPicker {
    sessions: Vec<SessionMeta>,
    selected: usize,
    chosen: Option<usize>,
    cancelled: bool,
    confirm_delete: Option<usize>,
    status_message: Option<String>,
    sessions_root: Option<PathBuf>,
    styles: TuiStyles,
}

impl SessionPicker {
    /// Create a new session picker.
    #[allow(clippy::missing_const_for_fn)] // sessions: Vec cannot be const
    #[must_use]
    pub fn new(sessions: Vec<SessionMeta>) -> Self {
        let theme = Theme::dark();
        let styles = theme.tui_styles();
        Self {
            sessions,
            selected: 0,
            chosen: None,
            cancelled: false,
            confirm_delete: None,
            status_message: None,
            sessions_root: None,
            styles,
        }
    }

    #[must_use]
    pub fn with_theme(sessions: Vec<SessionMeta>, theme: &Theme) -> Self {
        let styles = theme.tui_styles();
        Self {
            sessions,
            selected: 0,
            chosen: None,
            cancelled: false,
            confirm_delete: None,
            status_message: None,
            sessions_root: None,
            styles,
        }
    }

    #[must_use]
    pub fn with_theme_and_root(
        sessions: Vec<SessionMeta>,
        theme: &Theme,
        sessions_root: PathBuf,
    ) -> Self {
        let styles = theme.tui_styles();
        Self {
            sessions,
            selected: 0,
            chosen: None,
            cancelled: false,
            confirm_delete: None,
            status_message: None,
            sessions_root: Some(sessions_root),
            styles,
        }
    }

    /// Get the selected session path after the picker completes.
    pub fn selected_path(&self) -> Option<&str> {
        self.chosen
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.path.as_str())
    }

    /// Check if the picker was cancelled.
    pub const fn was_cancelled(&self) -> bool {
        self.cancelled
    }

    #[allow(clippy::unused_self, clippy::missing_const_for_fn)]
    fn init(&self) -> Option<Cmd> {
        None
    }

    #[allow(clippy::needless_pass_by_value)] // Required by Model trait
    pub fn update(&mut self, msg: Message) -> Option<Cmd> {
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            if self.confirm_delete.is_some() {
                return self.handle_delete_prompt(key);
            }
            match key.key_type {
                KeyType::Up => {
                    if self.selected > 0 {
                        self.selected -= 1;
                    }
                }
                KeyType::Down => {
                    if self.selected < self.sessions.len().saturating_sub(1) {
                        self.selected += 1;
                    }
                }
                KeyType::Runes if key.runes == ['k'] => {
                    if self.selected > 0 {
                        self.selected -= 1;
                    }
                }
                KeyType::Runes if key.runes == ['j'] => {
                    if self.selected < self.sessions.len().saturating_sub(1) {
                        self.selected += 1;
                    }
                }
                KeyType::Enter => {
                    if !self.sessions.is_empty() {
                        self.chosen = Some(self.selected);
                    }
                    return Some(quit());
                }
                KeyType::Esc | KeyType::CtrlC => {
                    self.cancelled = true;
                    return Some(quit());
                }
                KeyType::Runes if key.runes == ['q'] => {
                    self.cancelled = true;
                    return Some(quit());
                }
                KeyType::CtrlD => {
                    if !self.sessions.is_empty() {
                        self.confirm_delete = Some(self.selected);
                        self.status_message =
                            Some("Delete session? Press y/n to confirm.".to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn handle_delete_prompt(&mut self, key: &KeyMsg) -> Option<Cmd> {
        match key.key_type {
            KeyType::Runes if key.runes == ['y'] || key.runes == ['Y'] => {
                if let Some(index) = self.confirm_delete.take() {
                    if let Err(err) = self.delete_session_at(index) {
                        self.status_message = Some(err.to_string());
                    } else {
                        self.status_message = Some("Session deleted.".to_string());
                        if self.sessions.is_empty() {
                            self.cancelled = true;
                            return Some(quit());
                        }
                    }
                }
            }
            KeyType::Runes if key.runes == ['n'] || key.runes == ['N'] => {
                self.confirm_delete = None;
                self.status_message = None;
            }
            KeyType::Esc | KeyType::CtrlC => {
                self.confirm_delete = None;
                self.status_message = None;
            }
            _ => {}
        }
        None
    }

    fn delete_session_at(&mut self, index: usize) -> Result<()> {
        let Some(meta) = self.sessions.get(index) else {
            return Ok(());
        };
        let path = PathBuf::from(&meta.path);
        delete_session_file(&path)?;
        if let Some(root) = self.sessions_root.as_ref() {
            let index = SessionIndex::for_sessions_root(root);
            let _ = index.delete_session_path(&path);
        }
        self.sessions.remove(index);
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        Ok(())
    }

    pub fn view(&self) -> String {
        let mut output = String::new();

        // Header
        let _ = writeln!(
            output,
            "\n  {}\n",
            self.styles.title.render("Select a session to resume")
        );

        if self.sessions.is_empty() {
            let _ = writeln!(
                output,
                "  {}",
                self.styles
                    .muted
                    .render("No sessions found for this project.")
            );
        } else {
            // Column headers
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

            // Session rows
            for (i, session) in self.sessions.iter().enumerate() {
                let is_selected = i == self.selected;

                let prefix = if is_selected { ">" } else { " " };
                let time = format_time(&session.timestamp);
                let name = session
                    .name
                    .as_deref()
                    .unwrap_or("-")
                    .chars()
                    .take(28)
                    .collect::<String>();
                let messages = session.message_count.to_string();
                let id = &session.id[..8.min(session.id.len())];

                let _ = writeln!(
                    output,
                    "{prefix} {}",
                    if is_selected {
                        self.styles
                            .selection
                            .render(&format!(" {time:<20}  {name:<30}  {messages:<8}  {id}"))
                    } else {
                        format!(" {time:<20}  {name:<30}  {messages:<8}  {id}")
                    }
                );
            }
        }

        // Help text
        output.push('\n');
        let _ = writeln!(
            output,
            "  {}",
            self.styles
                .muted
                .render("↑/↓/j/k: navigate  Enter: select  Ctrl+D: delete  Esc/q: cancel")
        );
        if let Some(message) = &self.status_message {
            let _ = writeln!(output, "  {}", self.styles.warning_bold.render(message));
        }

        output
    }
}

/// List sessions for the current working directory using the session index.
pub fn list_sessions_for_cwd() -> Vec<SessionMeta> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    list_sessions_for_project(&cwd, None)
}

/// Run the session picker and return the selected session.
pub async fn pick_session(override_dir: Option<&Path>) -> Option<Session> {
    let cwd = std::env::current_dir().ok()?;
    let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
    let sessions = list_sessions_for_project(&cwd, override_dir);

    if sessions.is_empty() {
        return None;
    }

    if sessions.len() == 1 {
        // Only one session, just open it
        let mut session = Session::open(&sessions[0].path).await.ok()?;
        session.session_dir = Some(base_dir);
        return Some(session);
    }

    let config = Config::load().unwrap_or_default();
    let theme = Theme::resolve(&config, &cwd);
    let picker = SessionPicker::with_theme_and_root(sessions, &theme, base_dir.clone());

    // Run the TUI
    let result = Program::new(picker).with_alt_screen().run();

    match result {
        Ok(picker) => {
            if picker.was_cancelled() {
                return None;
            }

            if let Some(path) = picker.selected_path() {
                let mut session = Session::open(path).await.ok()?;
                session.session_dir = Some(base_dir);
                Some(session)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

pub fn list_sessions_for_project(cwd: &Path, override_dir: Option<&Path>) -> Vec<SessionMeta> {
    let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
    let project_session_dir = base_dir.join(encode_cwd(cwd));
    if !project_session_dir.exists() {
        return Vec::new();
    }

    let cwd_key = cwd.display().to_string();
    let index = SessionIndex::for_sessions_root(&base_dir);
    let mut sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();

    if sessions.is_empty() && index.reindex_all().is_ok() {
        sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();
    }

    sessions.retain(|meta| Path::new(&meta.path).exists());

    let scanned = scan_sessions_on_disk(&project_session_dir);
    if !scanned.is_empty() {
        let mut by_path: HashMap<String, SessionMeta> = sessions
            .into_iter()
            .map(|meta| (meta.path.clone(), meta))
            .collect();

        for meta in scanned {
            let should_replace = by_path
                .get(&meta.path)
                .is_some_and(|existing| meta.last_modified_ms > existing.last_modified_ms);
            if should_replace || !by_path.contains_key(&meta.path) {
                by_path.insert(meta.path.clone(), meta);
            }
        }

        sessions = by_path.into_values().collect();
    }

    sessions.sort_by_key(|m| Reverse(m.last_modified_ms));
    sessions.truncate(50);
    sessions
}

fn scan_sessions_on_disk(project_session_dir: &Path) -> Vec<SessionMeta> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(project_session_dir) else {
        return out;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if is_session_file_path(&path) {
            if let Ok(meta) = build_meta_from_file(&path) {
                out.push(meta);
            }
        }
    }

    out
}

fn build_meta_from_file(path: &Path) -> crate::error::Result<SessionMeta> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => build_meta_from_jsonl(path),
        #[cfg(feature = "sqlite-sessions")]
        Some("sqlite") => build_meta_from_sqlite(path),
        _ => Err(Error::session(format!(
            "Unsupported session file extension: {}",
            path.display()
        ))),
    }
}

fn build_meta_from_jsonl(path: &Path) -> crate::error::Result<SessionMeta> {
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();
    let header: SessionHeader = lines
        .next()
        .map(serde_json::from_str)
        .transpose()?
        .ok_or_else(|| crate::error::Error::session("Empty session file"))?;

    let mut message_count = 0u64;
    let mut name = None;
    for line in lines {
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
            match entry {
                SessionEntry::Message(_) => message_count += 1,
                SessionEntry::SessionInfo(info) => {
                    if info.name.is_some() {
                        name.clone_from(&info.name);
                    }
                }
                _ => {}
            }
        }
    }

    let meta = fs::metadata(path)?;
    let size_bytes = meta.len();
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let last_modified_ms = i64::try_from(millis).unwrap_or(i64::MAX);

    Ok(SessionMeta {
        path: path.display().to_string(),
        id: header.id,
        cwd: header.cwd,
        timestamp: header.timestamp,
        message_count,
        last_modified_ms,
        size_bytes,
        name,
    })
}

#[cfg(feature = "sqlite-sessions")]
fn build_meta_from_sqlite(path: &Path) -> crate::error::Result<SessionMeta> {
    let meta = futures::executor::block_on(async {
        crate::session_sqlite::load_session_meta(path).await
    })?;
    let header = meta.header;

    let sqlite_meta = fs::metadata(path)?;
    let size_bytes = sqlite_meta.len();
    let modified = sqlite_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let last_modified_ms = i64::try_from(millis).unwrap_or(i64::MAX);

    Ok(SessionMeta {
        path: path.display().to_string(),
        id: header.id,
        cwd: header.cwd,
        timestamp: header.timestamp,
        message_count: meta.message_count,
        last_modified_ms,
        size_bytes,
        name: meta.name,
    })
}

fn is_session_file_path(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => true,
        #[cfg(feature = "sqlite-sessions")]
        Some("sqlite") => true,
        _ => false,
    }
}

pub(crate) fn delete_session_file(path: &Path) -> Result<()> {
    if try_trash(path)? {
        return Ok(());
    }
    fs::remove_file(path).map_err(|err| {
        Error::session(format!(
            "Failed to delete session {}: {err}",
            path.display()
        ))
    })
}

fn try_trash(path: &Path) -> Result<bool> {
    match std::process::Command::new("trash").arg(path).status() {
        Ok(status) => {
            if status.success() {
                Ok(true)
            } else {
                Err(Error::session(format!(
                    "trash failed for {} (exit={})",
                    path.display(),
                    status.code().unwrap_or(-1)
                )))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::session(format!(
            "trash invocation failed for {}: {err}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta(path: &Path) -> SessionMeta {
        SessionMeta {
            path: path.display().to_string(),
            id: "sess".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2025-01-15T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        }
    }

    fn key_msg(key_type: KeyType, runes: Vec<char>) -> Message {
        Message::new(KeyMsg {
            key_type,
            runes,
            alt: false,
            paste: false,
        })
    }

    #[test]
    fn test_format_time() {
        let ts = "2025-01-15T10:30:00.000Z";
        let formatted = format_time(ts);
        assert!(formatted.contains("2025-01-15"));
        assert!(formatted.contains("10:30"));
    }

    #[test]
    fn test_format_time_invalid_returns_input() {
        let ts = "not-a-timestamp";
        assert_eq!(format_time(ts), ts);
    }

    #[test]
    fn test_is_session_file_path() {
        assert!(is_session_file_path(Path::new("/tmp/sess.jsonl")));
        assert!(!is_session_file_path(Path::new("/tmp/sess.txt")));
        assert!(!is_session_file_path(Path::new("/tmp/noext")));
        #[cfg(feature = "sqlite-sessions")]
        assert!(is_session_file_path(Path::new("/tmp/sess.sqlite")));
    }

    #[test]
    fn test_session_picker_navigation() {
        let sessions = vec![
            SessionMeta {
                path: "/test/a.jsonl".to_string(),
                id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T10:00:00.000Z".to_string(),
                message_count: 1,
                last_modified_ms: 1000,
                size_bytes: 100,
                name: None,
            },
            SessionMeta {
                path: "/test/b.jsonl".to_string(),
                id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T11:00:00.000Z".to_string(),
                message_count: 2,
                last_modified_ms: 2000,
                size_bytes: 200,
                name: Some("Test session".to_string()),
            },
        ];

        let mut picker = SessionPicker::new(sessions);
        assert_eq!(picker.selected, 0);

        // Navigate down
        picker.update(key_msg(KeyType::Down, vec![]));
        assert_eq!(picker.selected, 1);

        // Navigate up
        picker.update(key_msg(KeyType::Up, vec![]));
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn test_session_picker_vim_keys() {
        let sessions = vec![
            SessionMeta {
                path: "/test/a.jsonl".to_string(),
                id: "aaaaaaaa".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T10:00:00.000Z".to_string(),
                message_count: 1,
                last_modified_ms: 1000,
                size_bytes: 100,
                name: None,
            },
            SessionMeta {
                path: "/test/b.jsonl".to_string(),
                id: "bbbbbbbb".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T11:00:00.000Z".to_string(),
                message_count: 2,
                last_modified_ms: 2000,
                size_bytes: 200,
                name: None,
            },
        ];

        let mut picker = SessionPicker::new(sessions);
        assert_eq!(picker.selected, 0);

        // Navigate down with 'j'
        picker.update(key_msg(KeyType::Runes, vec!['j']));
        assert_eq!(picker.selected, 1);

        // Navigate up with 'k'
        picker.update(key_msg(KeyType::Runes, vec!['k']));
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn session_picker_delete_prompt_and_cancel() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("sess.jsonl");
        fs::write(&session_path, "test").expect("write session");

        let sessions = vec![make_meta(&session_path)];
        let mut picker = SessionPicker::new(sessions);

        picker.update(key_msg(KeyType::CtrlD, vec![]));
        assert!(picker.confirm_delete.is_some());

        picker.update(key_msg(KeyType::Runes, vec!['n']));
        assert!(picker.confirm_delete.is_none());
        assert!(session_path.exists());
    }

    #[test]
    fn session_picker_delete_confirm_removes_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("sess.jsonl");
        fs::write(&session_path, "test").expect("write session");

        let sessions = vec![make_meta(&session_path)];
        let mut picker = SessionPicker::new(sessions);

        picker.update(key_msg(KeyType::CtrlD, vec![]));

        picker.update(key_msg(KeyType::Runes, vec!['y']));

        assert!(!session_path.exists());
        assert!(picker.sessions.is_empty());
    }

    #[test]
    fn session_picker_navigation_bounds() {
        let sessions = vec![
            SessionMeta {
                path: "/test/a.jsonl".to_string(),
                id: "aaaaaaaa".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T10:00:00.000Z".to_string(),
                message_count: 1,
                last_modified_ms: 1000,
                size_bytes: 100,
                name: None,
            },
            SessionMeta {
                path: "/test/b.jsonl".to_string(),
                id: "bbbbbbbb".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T11:00:00.000Z".to_string(),
                message_count: 2,
                last_modified_ms: 2000,
                size_bytes: 200,
                name: None,
            },
        ];

        let mut picker = SessionPicker::new(sessions);
        picker.update(key_msg(KeyType::Up, vec![]));
        assert_eq!(picker.selected, 0);

        picker.update(key_msg(KeyType::Down, vec![]));
        picker.update(key_msg(KeyType::Down, vec![]));
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn session_picker_enter_selects_current_session() {
        let sessions = vec![
            SessionMeta {
                path: "/test/a.jsonl".to_string(),
                id: "aaaaaaaa".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T10:00:00.000Z".to_string(),
                message_count: 1,
                last_modified_ms: 1000,
                size_bytes: 100,
                name: None,
            },
            SessionMeta {
                path: "/test/b.jsonl".to_string(),
                id: "bbbbbbbb".to_string(),
                cwd: "/test".to_string(),
                timestamp: "2025-01-15T11:00:00.000Z".to_string(),
                message_count: 2,
                last_modified_ms: 2000,
                size_bytes: 200,
                name: Some("chosen".to_string()),
            },
        ];

        let mut picker = SessionPicker::new(sessions);
        picker.update(key_msg(KeyType::Down, vec![]));
        picker.update(key_msg(KeyType::Enter, vec![]));
        assert_eq!(picker.selected_path(), Some("/test/b.jsonl"));
        assert!(!picker.was_cancelled());
    }

    #[test]
    fn session_picker_cancel_keys_mark_cancelled() {
        let sessions = vec![SessionMeta {
            path: "/test/a.jsonl".to_string(),
            id: "aaaaaaaa".to_string(),
            cwd: "/test".to_string(),
            timestamp: "2025-01-15T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        }];

        let mut esc_picker = SessionPicker::new(sessions.clone());
        esc_picker.update(key_msg(KeyType::Esc, vec![]));
        assert!(esc_picker.was_cancelled());

        let mut q_picker = SessionPicker::new(sessions.clone());
        q_picker.update(key_msg(KeyType::Runes, vec!['q']));
        assert!(q_picker.was_cancelled());

        let mut ctrl_c_picker = SessionPicker::new(sessions);
        ctrl_c_picker.update(key_msg(KeyType::CtrlC, vec![]));
        assert!(ctrl_c_picker.was_cancelled());
    }

    #[test]
    fn session_picker_view_empty_and_populated_states() {
        let empty_picker = SessionPicker::new(Vec::new());
        let empty_view = empty_picker.view();
        assert!(empty_view.contains("Select a session to resume"));
        assert!(empty_view.contains("No sessions found for this project."));

        let sessions = vec![SessionMeta {
            path: "/test/a.jsonl".to_string(),
            id: "aaaaaaaa-bbbb".to_string(),
            cwd: "/test".to_string(),
            timestamp: "2025-01-15T10:00:00.000Z".to_string(),
            message_count: 3,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: Some("demo".to_string()),
        }];
        let mut populated = SessionPicker::new(sessions);
        populated.update(key_msg(KeyType::CtrlD, vec![]));
        let view = populated.view();
        assert!(view.contains("Messages"));
        assert!(view.contains("Session ID"));
        assert!(view.contains("Delete session? Press y/n to confirm."));
    }
}
