//! Session picker TUI for selecting from available sessions.
//!
//! Provides an interactive list for choosing which session to resume.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use bubbletea::{Cmd, KeyMsg, KeyType, Message, Program, quit};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::{Session, encode_cwd};
use crate::session_index::session_file_stats;
use crate::session_index::{SessionIndex, SessionMeta, build_meta_from_file, is_session_file_path};
use crate::theme::{Theme, TuiStyles};

/// Format a timestamp for display.
pub fn format_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp).map_or_else(
        |_| timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// Truncate a session id by character count for display.
#[must_use]
pub fn truncate_session_id(session_id: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    let end = session_id
        .char_indices()
        .nth(max_chars)
        .map_or(session_id.len(), |(idx, _)| idx);
    &session_id[..end]
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
                KeyType::Up if self.selected > 0 => {
                    self.selected -= 1;
                }
                KeyType::Down if self.selected < self.sessions.len().saturating_sub(1) => {
                    self.selected += 1;
                }
                KeyType::Runes if key.runes == ['k'] && self.selected > 0 => {
                    self.selected -= 1;
                }
                KeyType::Runes
                    if key.runes == ['j']
                        && self.selected < self.sessions.len().saturating_sub(1) =>
                {
                    self.selected += 1;
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
                KeyType::CtrlD if !self.sessions.is_empty() => {
                    self.confirm_delete = Some(self.selected);
                    self.status_message = Some("Delete session? Press y/n to confirm.".to_string());
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
                let id = truncate_session_id(&session.id, 8);

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
    let cwd_key = cwd.display().to_string();
    let index = SessionIndex::for_sessions_root(&base_dir);
    let mut sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();
    let project_session_dir_missing = indexed_session_path_is_missing(&project_session_dir);

    if !project_session_dir_missing && sessions.is_empty() && index.reindex_all().is_ok() {
        sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();
    }

    let mut missing_paths = Vec::new();
    let mut by_path = HashMap::new();
    for meta in sessions {
        let path = PathBuf::from(&meta.path);
        if indexed_session_path_is_missing(&path) {
            missing_paths.push(path);
        } else {
            by_path.insert(meta.path.clone(), meta);
        }
    }

    for path in &missing_paths {
        let _ = index.delete_session_path(path);
    }

    if project_session_dir_missing {
        return Vec::new();
    }

    let scanned = scan_sessions_on_disk(&project_session_dir, &by_path);
    for path in &scanned.failed_paths {
        let _ = index.delete_session_path(path);
        by_path.remove(&path.display().to_string());
    }

    for meta in scanned.metas {
        let _ = index.upsert_session_meta(meta.clone());
        by_path.insert(meta.path.clone(), meta);
    }

    sessions = by_path.into_values().collect();
    sessions.sort_by_key(|m| Reverse(m.last_modified_ms));
    sessions.truncate(50);
    sessions
}

fn indexed_session_path_is_missing(path: &Path) -> bool {
    match crate::session::session_path_try_exists(path) {
        Ok(exists) => !exists,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to determine whether indexed session path exists; deferring prune"
            );
            false
        }
    }
}

struct ScanSessionsResult {
    metas: Vec<SessionMeta>,
    failed_paths: Vec<PathBuf>,
}

#[cfg(test)]
thread_local! {
    static SESSION_SCAN_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_session_scan_parse_count() {
    SESSION_SCAN_PARSE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn take_session_scan_parse_count() -> usize {
    SESSION_SCAN_PARSE_COUNT.with(|count| {
        let value = count.get();
        count.set(0);
        value
    })
}

fn build_scanned_meta(path: &Path) -> crate::error::Result<SessionMeta> {
    #[cfg(test)]
    SESSION_SCAN_PARSE_COUNT.with(|count| count.set(count.get().saturating_add(1)));

    build_meta_from_file(path)
}

fn cached_meta_matches_disk(meta: &SessionMeta, path: &Path) -> bool {
    let Ok((last_modified_ms, size_bytes)) = session_file_stats(path) else {
        return false;
    };
    meta.last_modified_ms == last_modified_ms && meta.size_bytes == size_bytes
}

fn scan_sessions_on_disk(
    project_session_dir: &Path,
    cached_by_path: &HashMap<String, SessionMeta>,
) -> ScanSessionsResult {
    let mut out = Vec::new();
    let mut failed_paths = Vec::new();
    if let Err(err) = crate::session::ensure_session_directory_readable(project_session_dir) {
        tracing::warn!(
            path = %project_session_dir.display(),
            error = %err,
            "Failed to read project session directory; retaining indexed rows"
        );
        return ScanSessionsResult {
            metas: out,
            failed_paths,
        };
    }
    let Ok(entries) = fs::read_dir(project_session_dir) else {
        return ScanSessionsResult {
            metas: out,
            failed_paths,
        };
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if is_session_file_path(&path) {
            let path_key = path.display().to_string();
            if cached_by_path
                .get(&path_key)
                .is_some_and(|meta| cached_meta_matches_disk(meta, &path))
            {
                continue;
            }

            match build_scanned_meta(&path) {
                Ok(meta) => out.push(meta),
                Err(_) => failed_paths.push(path),
            }
        }
    }

    ScanSessionsResult {
        metas: out,
        failed_paths,
    }
}

pub(crate) fn delete_session_file(path: &Path) -> Result<()> {
    delete_session_file_with_trash_cmd(path, "trash")
}

fn delete_session_file_with_trash_cmd(path: &Path, trash_cmd: &str) -> Result<()> {
    if !session_artifacts_exist(path)? {
        return Ok(());
    }

    // Writers for JSONL, SQLite, and their sidecars all participate in this
    // persistent per-session lock. Re-check after acquisition so a delete
    // waiting behind a writer cannot operate on a stale artifact inventory.
    let _lock = crate::session::lock_session_persistence(path)?;
    if !session_artifacts_exist(path)? {
        return Ok(());
    }
    crate::session::ensure_session_parent_writable(path).map_err(|err| Error::Io(Box::new(err)))?;

    if try_trash_with_cmd(path, trash_cmd) {
        if crate::session::session_path_entry_exists(path)
            .map_err(|error| Error::Io(Box::new(error)))?
        {
            return Err(Error::session(format!(
                "Trash command reported success but left the session in place; sidecars were preserved: {}",
                path.display()
            )));
        }
        remove_sqlite_sidecars_best_effort(path, trash_cmd)?;
        remove_sidecar_dir_best_effort(&crate::session_store_v2::v2_sidecar_path(path), trash_cmd)?;
        return ensure_session_artifacts_removed(path);
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(Error::session(format!(
                "Failed to delete session {}: {err}",
                path.display()
            )));
        }
    }

    remove_sqlite_sidecars_best_effort(path, trash_cmd)?;
    remove_sidecar_dir_best_effort(&crate::session_store_v2::v2_sidecar_path(path), trash_cmd)?;
    ensure_session_artifacts_removed(path)
}

fn ensure_session_artifacts_removed(path: &Path) -> Result<()> {
    if session_artifacts_exist(path)? {
        return Err(Error::session(format!(
            "Session deletion left one or more artifacts behind: {}",
            path.display()
        )));
    }
    Ok(())
}

fn session_artifacts_exist(path: &Path) -> Result<bool> {
    let primary_exists =
        crate::session::session_path_entry_exists(path).map_err(|err| Error::Io(Box::new(err)))?;
    let v2_exists =
        crate::session::session_path_entry_exists(&crate::session_store_v2::v2_sidecar_path(path))
            .map_err(|err| Error::Io(Box::new(err)))?;
    #[cfg(feature = "sqlite-sessions")]
    let sqlite_sidecar_exists = sqlite_auxiliary_paths(path)
        .into_iter()
        .try_fold(false, |found, auxiliary_path| {
            crate::session::session_path_entry_exists(&auxiliary_path).map(|exists| found || exists)
        })
        .map_err(|err| Error::Io(Box::new(err)))?;
    #[cfg(not(feature = "sqlite-sessions"))]
    let sqlite_sidecar_exists = false;
    Ok(primary_exists || v2_exists || sqlite_sidecar_exists)
}

fn sqlite_auxiliary_paths(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        PathBuf::from(candidate)
    })
}

#[cfg(feature = "sqlite-sessions")]
fn remove_sqlite_sidecars_best_effort(path: &Path, trash_cmd: &str) -> Result<()> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("sqlite") {
        for auxiliary_path in sqlite_auxiliary_paths(path) {
            if !crate::session::session_path_entry_exists(&auxiliary_path)
                .map_err(|error| Error::Io(Box::new(error)))?
            {
                continue;
            }
            if try_trash_with_cmd(&auxiliary_path, trash_cmd)
                && !crate::session::session_path_entry_exists(&auxiliary_path)
                    .map_err(|error| Error::Io(Box::new(error)))?
            {
                continue;
            }
            if let Err(err) = fs::remove_file(&auxiliary_path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    path = %auxiliary_path.display(),
                    error = %err,
                    "Failed to remove SQLite sidecar"
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "sqlite-sessions"))]
fn remove_sqlite_sidecars_best_effort(_path: &Path, _trash_cmd: &str) -> Result<()> {
    Ok(())
}

fn remove_sidecar_dir_best_effort(sidecar_path: &Path, trash_cmd: &str) -> Result<()> {
    if !crate::session::session_path_entry_exists(sidecar_path)
        .map_err(|error| Error::Io(Box::new(error)))?
    {
        return Ok(());
    }

    if try_trash_with_cmd(sidecar_path, trash_cmd)
        && !crate::session::session_path_entry_exists(sidecar_path)
            .map_err(|error| Error::Io(Box::new(error)))?
    {
        return Ok(());
    }

    let metadata =
        fs::symlink_metadata(sidecar_path).map_err(|error| Error::Io(Box::new(error)))?;
    let removal = if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(sidecar_path)
    } else {
        fs::remove_dir_all(sidecar_path)
    };
    if let Err(err) = removal {
        tracing::warn!(
            path = %sidecar_path.display(),
            error = %err,
            "Failed to remove session sidecar"
        );
    }
    Ok(())
}

fn try_trash_with_cmd(path: &Path, trash_cmd: &str) -> bool {
    let child = std::process::Command::new(trash_cmd)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "trash command invocation failed; falling back to direct file removal"
            );
            return false;
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return true,
            Ok(Some(status)) => {
                tracing::warn!(
                    path = %path.display(),
                    exit = status.code().unwrap_or(-1),
                    "trash command failed; falling back to direct file removal"
                );
                return false;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    path = %path.display(),
                    "trash command timed out; falling back to direct file removal"
                );
                return false;
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "trash command wait failed; falling back to direct file removal"
                );
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionHeader;

    #[cfg(feature = "sqlite-sessions")]
    use crate::model::UserContent;
    #[cfg(feature = "sqlite-sessions")]
    use crate::session::{SessionMessage, SessionStoreKind};
    #[cfg(feature = "sqlite-sessions")]
    use asupersync::runtime::RuntimeBuilder;
    use sqlmodel_core::Value;
    use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
    #[cfg(feature = "sqlite-sessions")]
    use std::future::Future;

    #[cfg(unix)]
    struct UnixModeGuard {
        path: PathBuf,
        original: Option<fs::Permissions>,
    }

    #[cfg(unix)]
    impl UnixModeGuard {
        fn apply(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let original = fs::metadata(path)
                .expect("permission fixture metadata")
                .permissions();
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .expect("apply permission fixture mode");
            Self {
                path: path.to_path_buf(),
                original: Some(original),
            }
        }

        fn restore(&mut self) {
            if let Some(original) = self.original.as_ref() {
                fs::set_permissions(&self.path, original.clone())
                    .expect("restore permission fixture mode");
                self.original = None;
            }
        }
    }

    #[cfg(unix)]
    impl Drop for UnixModeGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.take() {
                let _ = fs::set_permissions(&self.path, original);
            }
        }
    }

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

    #[cfg(feature = "sqlite-sessions")]
    fn run_async<T>(future: impl Future<Output = T>) -> T {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
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
    fn truncate_session_id_handles_unicode_boundaries() {
        assert_eq!(truncate_session_id("abcdefghijk", 8), "abcdefgh");
        assert_eq!(truncate_session_id("αβγδεζηθικ", 8), "αβγδεζηθ");
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

    #[test]
    fn session_picker_view_handles_non_ascii_session_ids() {
        let sessions = vec![SessionMeta {
            path: "/test/u.jsonl".to_string(),
            id: "αβγδεζηθι".to_string(),
            cwd: "/test".to_string(),
            timestamp: "2025-01-15T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: Some("unicode".to_string()),
        }];

        let view = SessionPicker::new(sessions).view();
        assert!(view.contains("αβγδεζηθ"));
    }

    // ── selected_path when nothing chosen ──────────────────────────────

    #[test]
    fn selected_path_returns_none_when_no_selection() {
        let picker = SessionPicker::new(vec![make_meta(Path::new("/tmp/a.jsonl"))]);
        assert!(picker.selected_path().is_none());
        assert!(!picker.was_cancelled());
    }

    // ── with_theme constructor ─────────────────────────────────────────

    #[test]
    fn with_theme_constructor_sets_initial_state() {
        let theme = Theme::dark();
        let sessions = vec![make_meta(Path::new("/tmp/a.jsonl"))];
        let picker = SessionPicker::with_theme(sessions, &theme);
        assert_eq!(picker.selected, 0);
        assert!(!picker.was_cancelled());
        assert!(picker.selected_path().is_none());
    }

    // ── delete last session causes quit ────────────────────────────────

    #[test]
    fn delete_last_session_sets_cancelled_true() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("only.jsonl");
        fs::write(&session_path, "test").expect("write");

        let mut picker = SessionPicker::new(vec![make_meta(&session_path)]);

        picker.update(key_msg(KeyType::CtrlD, vec![]));
        let cmd = picker.update(key_msg(KeyType::Runes, vec!['y']));
        assert!(picker.was_cancelled());
        assert!(cmd.is_some()); // quit command issued
    }

    // ── Esc during delete prompt cancels prompt ────────────────────────

    #[test]
    fn esc_cancels_delete_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("sess.jsonl");
        fs::write(&session_path, "test").expect("write");

        let mut picker = SessionPicker::new(vec![make_meta(&session_path)]);
        picker.update(key_msg(KeyType::CtrlD, vec![]));
        assert!(picker.confirm_delete.is_some());

        picker.update(key_msg(KeyType::Esc, vec![]));
        assert!(picker.confirm_delete.is_none());
        assert!(picker.status_message.is_none());
    }

    // ── enter on empty list still returns quit ─────────────────────────

    #[test]
    fn enter_on_empty_list_returns_quit() {
        let mut picker = SessionPicker::new(Vec::new());
        let cmd = picker.update(key_msg(KeyType::Enter, vec![]));
        assert!(cmd.is_some()); // quit
        assert!(picker.selected_path().is_none());
    }

    // ── ctrl-d on empty list is a noop ─────────────────────────────────

    #[test]
    fn ctrl_d_on_empty_list_is_noop() {
        let mut picker = SessionPicker::new(Vec::new());
        picker.update(key_msg(KeyType::CtrlD, vec![]));
        assert!(picker.confirm_delete.is_none());
    }

    // ── build_meta_from_file ──────────────────────────────────────────

    #[test]
    fn build_meta_from_file_parses_session_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("test.jsonl");
        let mut header = SessionHeader::new();
        header.id = "abc123".to_string();
        header.cwd = "/work".to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        let msg1 = serde_json::json!({
            "type": "message",
            "timestamp": "2025-06-01T12:00:01.000Z",
            "message": {"role": "user", "content": "hi"}
        });
        let msg2 = serde_json::json!({
            "type": "message",
            "timestamp": "2025-06-01T12:00:02.000Z",
            "message": {"role": "user", "content": "hello again"}
        });
        let info = serde_json::json!({
            "type": "session_info",
            "timestamp": "2025-06-01T12:00:03.000Z",
            "name": "My Session"
        });
        let content = format!(
            "{}\n{}\n{}\n{}",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&msg1).unwrap(),
            serde_json::to_string(&msg2).unwrap(),
            serde_json::to_string(&info).unwrap(),
        );
        fs::write(&session_path, content).expect("write");

        let meta = build_meta_from_file(&session_path).expect("parse meta");
        assert_eq!(meta.id, "abc123");
        assert_eq!(meta.cwd, "/work");
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.name.as_deref(), Some("My Session"));
        assert!(meta.size_bytes > 0);
    }

    #[test]
    fn build_meta_from_file_rejects_semantically_invalid_header() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("invalid.jsonl");
        let header = serde_json::json!({
            "type": "header",
            "id": "abc123",
            "cwd": "/work",
            "timestamp": "2025-06-01T12:00:00.000Z"
        });
        fs::write(
            &session_path,
            format!(
                "{}\n",
                serde_json::to_string(&header).expect("serialize header")
            ),
        )
        .expect("write");

        let err = build_meta_from_file(&session_path).expect_err("invalid header should fail");
        assert!(
            matches!(err, crate::error::Error::Session(ref msg) if msg.contains("Invalid session header")),
            "expected invalid session header error, got {err:?}"
        );
    }

    #[test]
    fn build_meta_from_file_empty_file_returns_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("empty.jsonl");
        fs::write(&session_path, "").expect("write");

        assert!(build_meta_from_file(&session_path).is_err());
    }

    // ── is_session_file_path additional cases ──────────────────────────

    #[test]
    fn is_session_file_path_rejects_common_non_session_extensions() {
        assert!(!is_session_file_path(Path::new("/tmp/file.json")));
        assert!(!is_session_file_path(Path::new("/tmp/file.md")));
        assert!(!is_session_file_path(Path::new("/tmp/file.rs")));
    }

    // ── scan_sessions_on_disk ──────────────────────────────────────────

    #[test]
    fn scan_sessions_on_disk_finds_valid_session_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("session.jsonl");
        let mut header = SessionHeader::new();
        header.id = "scan-test".to_string();
        header.cwd = "/work".to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        fs::write(&session_path, serde_json::to_string(&header).unwrap()).expect("write");

        // Also create a non-session file that should be ignored
        fs::write(tmp.path().join("notes.txt"), "not a session").expect("write");

        let found = scan_sessions_on_disk(tmp.path(), &HashMap::new());
        assert_eq!(found.metas.len(), 1);
        assert_eq!(found.metas[0].id, "scan-test");
        assert!(found.failed_paths.is_empty());
    }

    #[test]
    fn scan_sessions_on_disk_nonexistent_dir_returns_empty() {
        let found = scan_sessions_on_disk(Path::new("/nonexistent/dir"), &HashMap::new());
        assert!(found.metas.is_empty());
        assert!(found.failed_paths.is_empty());
    }

    #[test]
    fn scan_sessions_on_disk_skips_unchanged_cached_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("session.jsonl");
        let mut header = SessionHeader::new();
        header.id = "cached-scan".to_string();
        header.cwd = "/work".to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        fs::write(&session_path, serde_json::to_string(&header).unwrap()).expect("write");

        let cached = build_meta_from_file(&session_path).expect("cached meta");
        let mut cached_by_path = HashMap::new();
        cached_by_path.insert(cached.path.clone(), cached);

        reset_session_scan_parse_count();
        let found = scan_sessions_on_disk(tmp.path(), &cached_by_path);

        assert!(found.metas.is_empty());
        assert!(found.failed_paths.is_empty());
        assert_eq!(take_session_scan_parse_count(), 0);
    }

    #[test]
    fn list_sessions_for_project_prefers_scanned_meta_when_cached_row_is_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(encode_cwd(&cwd));
        fs::create_dir_all(&project_dir).expect("create project sessions");

        let session_path = project_dir.join("stale-index.jsonl");
        let mut header = SessionHeader::new();
        header.id = "stale-index".to_string();
        header.cwd = cwd.display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();

        let content = format!(
            "{}\n{{\"type\":\"message\"}}\n{{\"type\":\"message\"}}\n{{\"type\":\"session_info\",\"name\":\"Fresh name\"}}\n",
            serde_json::to_string(&header).expect("serialize header"),
        );
        fs::write(&session_path, content).expect("write session");

        let expected = build_meta_from_file(&session_path).expect("load fresh meta");
        let index = SessionIndex::for_sessions_root(&base_dir);
        index.reindex_all().expect("seed session index");

        let db_path = base_dir.join("session-index.sqlite");
        let config = SqliteConfig::file(db_path.to_string_lossy())
            .flags(OpenFlags::create_read_write())
            .busy_timeout(5000);
        let conn = SqliteConnection::open(&config).expect("open session index sqlite");
        conn.execute_sync(
            "UPDATE sessions
             SET message_count=?1, size_bytes=?2, name=?3
             WHERE path=?4",
            &[
                Value::BigInt(0),
                Value::BigInt(
                    i64::try_from(expected.size_bytes.saturating_sub(1)).expect("size fits in i64"),
                ),
                Value::Text("Stale name".to_string()),
                Value::Text(session_path.display().to_string()),
            ],
        )
        .expect("corrupt cached row");

        let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
        assert_eq!(sessions.len(), 1);

        let session = &sessions[0];
        assert_eq!(session.path, session_path.display().to_string());
        assert_eq!(session.message_count, expected.message_count);
        assert_eq!(session.size_bytes, expected.size_bytes);
        assert_eq!(session.name, expected.name);
        assert_eq!(session.last_modified_ms, expected.last_modified_ms);
    }

    #[test]
    fn list_sessions_for_project_refreshes_index_after_changed_disk_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(encode_cwd(&cwd));
        fs::create_dir_all(&project_dir).expect("create project sessions");

        let session_path = project_dir.join("steady-state.jsonl");
        let mut header = SessionHeader::new();
        header.id = "steady-state".to_string();
        header.cwd = cwd.display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();

        let initial = format!(
            "{}\n{{\"type\":\"message\"}}\n{{\"type\":\"session_info\",\"name\":\"Initial\"}}\n",
            serde_json::to_string(&header).expect("serialize header"),
        );
        fs::write(&session_path, initial).expect("write initial session");

        let index = SessionIndex::for_sessions_root(&base_dir);
        index.reindex_all().expect("seed session index");

        let refreshed = format!(
            "{}\n{{\"type\":\"message\"}}\n{{\"type\":\"message\"}}\n{{\"type\":\"session_info\",\"name\":\"Refreshed\"}}\n",
            serde_json::to_string(&header).expect("serialize header"),
        );
        fs::write(&session_path, refreshed).expect("write refreshed session");

        reset_session_scan_parse_count();
        let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
        assert_eq!(take_session_scan_parse_count(), 1);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].name.as_deref(), Some("Refreshed"));

        let indexed = index
            .list_sessions(Some(&cwd.display().to_string()))
            .expect("list indexed sessions");
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].message_count, 2);
        assert_eq!(indexed[0].name.as_deref(), Some("Refreshed"));

        reset_session_scan_parse_count();
        let steady_state = list_sessions_for_project(&cwd, Some(&base_dir));
        assert_eq!(take_session_scan_parse_count(), 0);
        assert_eq!(steady_state.len(), 1);
        assert_eq!(steady_state[0].message_count, 2);
        assert_eq!(steady_state[0].name.as_deref(), Some("Refreshed"));
    }

    #[test]
    fn list_sessions_for_project_evicts_cached_row_when_disk_session_is_invalid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(encode_cwd(&cwd));
        fs::create_dir_all(&project_dir).expect("create project sessions");

        let session_path = project_dir.join("stale-invalid.jsonl");
        let mut header = SessionHeader::new();
        header.id = "stale-invalid".to_string();
        header.cwd = cwd.display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        fs::write(
            &session_path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n",
                serde_json::to_string(&header).expect("serialize header"),
            ),
        )
        .expect("write session");

        let index = SessionIndex::for_sessions_root(&base_dir);
        index.reindex_all().expect("seed session index");

        let invalid_header = serde_json::json!({
            "type": "header",
            "id": "stale-invalid",
            "cwd": cwd.display().to_string(),
            "timestamp": "2025-06-01T12:00:00.000Z"
        });
        fs::write(
            &session_path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n",
                serde_json::to_string(&invalid_header).expect("serialize invalid header"),
            ),
        )
        .expect("corrupt session");

        let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
        assert!(sessions.is_empty());

        let indexed = index
            .list_sessions(Some(&cwd.display().to_string()))
            .expect("list sessions");
        assert!(indexed.is_empty());
    }

    #[test]
    fn list_sessions_for_project_prunes_index_when_project_dir_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(encode_cwd(&cwd));
        fs::create_dir_all(&project_dir).expect("create project sessions");

        let session_path = project_dir.join("missing-project-dir.jsonl");
        let mut header = SessionHeader::new();
        header.id = "missing-project-dir".to_string();
        header.cwd = cwd.display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        fs::write(
            &session_path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n",
                serde_json::to_string(&header).expect("serialize header"),
            ),
        )
        .expect("write session");

        let index = SessionIndex::for_sessions_root(&base_dir);
        index.reindex_all().expect("seed session index");

        let moved_project_dir = tmp.path().join("moved-project-dir");
        fs::rename(&project_dir, &moved_project_dir).expect("move project dir away");

        let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
        assert!(sessions.is_empty());

        let indexed = index
            .list_sessions(Some(&cwd.display().to_string()))
            .expect("list indexed sessions");
        assert!(indexed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn list_sessions_for_project_keeps_permission_denied_row_indexed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(encode_cwd(&cwd));
        fs::create_dir_all(&project_dir).expect("create project sessions");

        let session_path = project_dir.join("guarded.jsonl");
        let mut header = SessionHeader::new();
        header.id = "guarded-session".to_string();
        header.cwd = cwd.display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();
        fs::write(
            &session_path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n",
                serde_json::to_string(&header).expect("serialize header"),
            ),
        )
        .expect("write session");

        let index = SessionIndex::for_sessions_root(&base_dir);
        index.reindex_all().expect("seed session index");

        let mut mode_guard = UnixModeGuard::apply(&project_dir, 0o000);

        let denied_probe = crate::session::session_path_try_exists(&session_path);
        let denied_scan = crate::session::ensure_session_directory_readable(&project_dir);

        let sessions = list_sessions_for_project(&cwd, Some(&base_dir));

        mode_guard.restore();

        let denied = denied_probe
            .expect_err("a path below a mode-000 project directory must fail its existence probe");
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        let denied = denied_scan
            .expect_err("listing a mode-000 project session directory must fail permission checks");
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].path, session_path.display().to_string());

        let indexed = index
            .list_sessions(Some(&cwd.display().to_string()))
            .expect("list indexed sessions");
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].path, session_path.display().to_string());
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn build_meta_from_file_uses_session_file_stats() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut session = Session::create_with_dir_and_store(
            Some(tmp.path().to_path_buf()),
            SessionStoreKind::Sqlite,
        );
        session.append_message(SessionMessage::User {
            content: UserContent::Text("sqlite".to_string()),
            timestamp: Some(0),
        });
        run_async(async { session.save().await }).expect("save sqlite session");

        let session_path = session.path.clone().expect("sqlite session path");
        let meta = build_meta_from_file(&session_path).expect("sqlite meta");
        let (expected_ms, expected_size) =
            session_file_stats(&session_path).expect("sqlite file stats");

        assert_eq!(meta.message_count, 1);
        assert_eq!(meta.size_bytes, expected_size);
        assert_eq!(meta.last_modified_ms, expected_ms);
    }

    // ── with_theme_and_root constructor ────────────────────────────────

    #[test]
    fn with_theme_and_root_stores_sessions_root() {
        let theme = Theme::dark();
        let root = PathBuf::from("/sessions");
        let picker = SessionPicker::with_theme_and_root(Vec::new(), &theme, root);
        assert!(picker.sessions_root.is_some());
    }

    // ── delete adjusts selection when at end ───────────────────────────

    #[test]
    fn delete_adjusts_selection_when_at_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_a = tmp.path().join("a.jsonl");
        let path_b = tmp.path().join("b.jsonl");
        fs::write(&path_a, "test").expect("write a");
        fs::write(&path_b, "test").expect("write b");

        let mut picker = SessionPicker::new(vec![make_meta(&path_a), make_meta(&path_b)]);

        // Navigate to second item
        picker.update(key_msg(KeyType::Down, vec![]));
        assert_eq!(picker.selected, 1);

        // Delete it
        picker.update(key_msg(KeyType::CtrlD, vec![]));
        picker.update(key_msg(KeyType::Runes, vec!['y']));

        // Selection should clamp back to 0
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.sessions.len(), 1);
    }

    #[test]
    fn delete_session_file_falls_back_when_trash_command_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("missing-trash-fallback.jsonl");
        fs::write(&session_path, "test").expect("write");

        let result = delete_session_file_with_trash_cmd(
            &session_path,
            "__pi_agent_rust_nonexistent_trash_command__",
        );
        assert!(result.is_ok(), "delete should fall back to remove_file");
        assert!(!session_path.exists(), "session file should be deleted");
    }

    #[cfg(unix)]
    #[test]
    fn delete_session_file_falls_back_when_trash_exits_non_zero() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("failing-trash-fallback.jsonl");
        fs::write(&session_path, "test").expect("write");

        let trash_script = tmp.path().join("fake-trash.sh");
        fs::write(&trash_script, "#!/bin/sh\nexit 2\n").expect("write script");
        let mut perms = fs::metadata(&trash_script).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&trash_script, perms).expect("chmod");

        let trash_cmd = trash_script.to_string_lossy();
        let result = delete_session_file_with_trash_cmd(&session_path, &trash_cmd);
        assert!(result.is_ok(), "delete should fall back to remove_file");
        assert!(!session_path.exists(), "session file should be deleted");
    }

    #[cfg(unix)]
    #[test]
    fn delete_session_file_succeeds_when_trash_deleted_file_then_failed() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("trash-deleted-then-failed.jsonl");
        fs::write(&session_path, "test").expect("write");

        let trash_script = tmp.path().join("fake-trash-delete-then-fail.sh");
        fs::write(
            &trash_script,
            format!("#!/bin/sh\nrm -f \"{}\"\nexit 2\n", session_path.display()),
        )
        .expect("write script");
        let mut perms = fs::metadata(&trash_script).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&trash_script, perms).expect("chmod");

        let trash_cmd = trash_script.to_string_lossy();
        let result = delete_session_file_with_trash_cmd(&session_path, &trash_cmd);
        assert!(
            result.is_ok(),
            "delete should be idempotent when file is already gone"
        );
        assert!(!session_path.exists(), "session file should remain deleted");
    }

    #[cfg(all(unix, feature = "sqlite-sessions"))]
    #[test]
    fn successful_noop_trash_preserves_primary_and_every_sidecar() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("noop-trash.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        let v2_path = crate::session_store_v2::v2_sidecar_path(&session_path);
        fs::write(&session_path, "db").expect("write SQLite primary");
        fs::write(&wal_path, "wal").expect("write SQLite WAL");
        fs::write(&shm_path, "shm").expect("write SQLite SHM");
        fs::write(&journal_path, "journal").expect("write SQLite rollback journal");
        fs::create_dir(&v2_path).expect("create V2 sidecar");
        fs::write(v2_path.join("manifest.json"), "manifest").expect("write V2 manifest");

        let trash_script = tmp.path().join("successful-noop-trash.sh");
        fs::write(&trash_script, "#!/bin/sh\nexit 0\n").expect("write trash script");
        let mut permissions = fs::metadata(&trash_script)
            .expect("trash script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&trash_script, permissions).expect("chmod trash script");

        let error = delete_session_file_with_trash_cmd(
            &session_path,
            trash_script.to_string_lossy().as_ref(),
        )
        .expect_err("an exit-zero no-op trash command must not authorize sidecar deletion");
        assert!(error.to_string().contains("left the session in place"));
        for artifact in [&session_path, &wal_path, &shm_path, &journal_path, &v2_path] {
            assert!(
                crate::session::session_path_entry_exists(artifact)
                    .expect("inspect preserved artifact"),
                "no-op trash removed or lost {}",
                artifact.display()
            );
        }
    }

    #[cfg(all(unix, feature = "sqlite-sessions"))]
    #[test]
    fn auxiliary_noop_trash_falls_back_after_primary_was_trashed() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("aux-noop-trash.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        let v2_path = crate::session_store_v2::v2_sidecar_path(&session_path);
        fs::write(&session_path, "db").expect("write SQLite primary");
        fs::write(&wal_path, "wal").expect("write SQLite WAL");
        fs::write(&shm_path, "shm").expect("write SQLite SHM");
        fs::write(&journal_path, "journal").expect("write SQLite rollback journal");
        fs::create_dir(&v2_path).expect("create V2 sidecar");
        fs::write(v2_path.join("manifest.json"), "manifest").expect("write V2 manifest");

        let trash_script = tmp.path().join("primary-only-trash.sh");
        fs::write(
            &trash_script,
            "#!/bin/sh\ncase \"$1\" in\n  *.sqlite) rm -f -- \"$1\" ;;\n  *) : ;;\nesac\nexit 0\n",
        )
        .expect("write trash script");
        let mut permissions = fs::metadata(&trash_script)
            .expect("trash script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&trash_script, permissions).expect("chmod trash script");

        delete_session_file_with_trash_cmd(&session_path, trash_script.to_string_lossy().as_ref())
            .expect("direct fallback must finish auxiliary cleanup");

        for artifact in [&session_path, &wal_path, &shm_path, &journal_path, &v2_path] {
            assert!(
                !crate::session::session_path_entry_exists(artifact)
                    .expect("inspect removed artifact"),
                "auxiliary cleanup left {}",
                artifact.display()
            );
        }
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn fresh_eyes_delete_sqlite_session_removes_all_sidecars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("sqlite-session.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        fs::write(&session_path, "db").expect("write sqlite session");
        fs::write(&wal_path, "wal").expect("write sqlite wal");
        fs::write(&shm_path, "shm").expect("write sqlite shm");
        fs::write(&journal_path, "journal").expect("write sqlite rollback journal");

        let result = delete_session_file_with_trash_cmd(
            &session_path,
            "__pi_agent_rust_nonexistent_trash_command__",
        );
        assert!(result.is_ok(), "delete should fall back to remove_file");
        assert!(
            !session_path.exists(),
            "sqlite session file should be deleted"
        );
        assert!(!wal_path.exists(), "sqlite wal sidecar should be deleted");
        assert!(!shm_path.exists(), "sqlite shm sidecar should be deleted");
        assert!(
            !journal_path.exists(),
            "sqlite rollback journal should be deleted"
        );
    }

    #[cfg(all(unix, feature = "sqlite-sessions"))]
    #[test]
    fn delete_removes_dangling_sqlite_and_v2_sidecar_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("dangling-sidecars.sqlite");
        let [wal_path, _, _] = sqlite_auxiliary_paths(&session_path);
        let v2_path = crate::session_store_v2::v2_sidecar_path(&session_path);
        let missing_target = tmp.path().join("missing-target");
        symlink(&missing_target, &wal_path).expect("create dangling WAL symlink");
        symlink(&missing_target, &v2_path).expect("create dangling V2 symlink");

        delete_session_file_with_trash_cmd(
            &session_path,
            "__pi_agent_rust_nonexistent_trash_command__",
        )
        .expect("delete dangling sidecars");

        assert!(
            !crate::session::session_path_entry_exists(&wal_path).expect("inspect WAL link"),
            "dangling WAL symlink must be removed"
        );
        assert!(
            !crate::session::session_path_entry_exists(&v2_path).expect("inspect V2 link"),
            "dangling V2 symlink must be removed"
        );
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn delete_holds_persistent_lock_across_primary_and_sidecar_removal() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("locked-delete.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        fs::write(&session_path, "db").expect("write SQLite primary");
        fs::write(&wal_path, "wal").expect("write SQLite WAL");
        fs::write(&shm_path, "shm").expect("write SQLite SHM");
        let held_lock = crate::session::lock_session_persistence(&session_path)
            .expect("hold writer-compatible persistence lock");
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let path_for_delete = session_path.clone();
        let delete_thread = std::thread::spawn(move || {
            started_tx.send(()).expect("announce delete start");
            let result = delete_session_file_with_trash_cmd(
                &path_for_delete,
                "__pi_agent_rust_nonexistent_trash_command__",
            );
            done_tx.send(result).expect("report delete result");
        });

        started_rx.recv().expect("delete thread started");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "delete bypassed the persistent session lock"
        );
        fs::write(&journal_path, "journal-created-by-locked-writer")
            .expect("create sidecar while writer lock is held");
        drop(held_lock);

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("delete completed after lock release")
            .expect("delete succeeded");
        delete_thread.join().expect("join delete thread");
        for artifact in [&session_path, &wal_path, &shm_path, &journal_path] {
            assert!(
                !artifact.exists(),
                "locked delete left artifact {}",
                artifact.display()
            );
        }
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn delete_sqlite_session_preserves_sidecars_when_primary_delete_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("delete-fails.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        fs::create_dir(&session_path).expect("create directory in place of sqlite session");
        fs::write(&wal_path, "wal").expect("write sqlite wal");
        fs::write(&shm_path, "shm").expect("write sqlite shm");
        fs::write(&journal_path, "journal").expect("write sqlite rollback journal");

        let result = delete_session_file_with_trash_cmd(
            &session_path,
            "__pi_agent_rust_nonexistent_trash_command__",
        );
        assert!(
            result.is_err(),
            "directory-backed sqlite session path should fail deletion"
        );
        assert!(
            wal_path.exists(),
            "wal sidecar must be preserved on primary delete failure"
        );
        assert!(
            shm_path.exists(),
            "shm sidecar must be preserved on primary delete failure"
        );
        assert!(
            journal_path.exists(),
            "rollback journal must be preserved on primary delete failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_session_file_preserves_sidecar_when_primary_delete_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("delete-fails.jsonl");
        fs::create_dir(&session_path).expect("create directory in place of session file");

        let sidecar_path = crate::session_store_v2::v2_sidecar_path(&session_path);
        fs::create_dir_all(&sidecar_path).expect("create sidecar");
        fs::write(sidecar_path.join("manifest.json"), "{}\n").expect("write sidecar marker");

        let trash_script = tmp.path().join("fake-trash-sidecar-only.sh");
        fs::write(
            &trash_script,
            r#"#!/bin/sh
case "$1" in
  *.v2) mv "$1" "$1.trashed"; exit 0 ;;
  *) exit 2 ;;
esac
"#,
        )
        .expect("write script");
        let mut perms = fs::metadata(&trash_script).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&trash_script, perms).expect("chmod");

        let trash_cmd = trash_script.to_string_lossy();
        let result = delete_session_file_with_trash_cmd(&session_path, &trash_cmd);
        assert!(
            result.is_err(),
            "directory-backed session path should fail deletion"
        );
        assert!(
            sidecar_path.exists(),
            "sidecar must be preserved when the main session path could not be deleted"
        );
    }

    #[cfg(all(unix, feature = "sqlite-sessions"))]
    #[test]
    fn delete_denial_preserves_primary_and_every_sidecar_without_invoking_trash() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let marker_dir = tempfile::tempdir().expect("marker tempdir");
        let invocation_marker = marker_dir.path().join("trash-invoked");
        let session_path = tmp.path().join("guarded.sqlite");
        let [wal_path, shm_path, journal_path] = sqlite_auxiliary_paths(&session_path);
        let v2_path = crate::session_store_v2::v2_sidecar_path(&session_path);
        fs::write(&session_path, b"database").expect("write primary");
        fs::write(&wal_path, b"wal").expect("write WAL");
        fs::write(&shm_path, b"shm").expect("write SHM");
        fs::write(&journal_path, b"journal").expect("write rollback journal");
        fs::create_dir(&v2_path).expect("create V2 sidecar");
        fs::write(v2_path.join("manifest.json"), b"manifest").expect("write manifest");

        let trash_script = tmp.path().join("fake-trash.sh");
        fs::write(
            &trash_script,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 0\n",
                invocation_marker.display()
            ),
        )
        .expect("write trash script");
        let mut script_permissions = fs::metadata(&trash_script)
            .expect("trash script metadata")
            .permissions();
        script_permissions.set_mode(0o755);
        fs::set_permissions(&trash_script, script_permissions).expect("chmod trash script");

        // The fixture owner lacks parent-directory write while group/other
        // deliberately have it, exercising selected-class semantics as UID 0
        // and UID 1000 without conditional skips.
        let mut mode_guard = UnixModeGuard::apply(tmp.path(), 0o577);
        let result = delete_session_file_with_trash_cmd(
            &session_path,
            trash_script.to_string_lossy().as_ref(),
        );
        mode_guard.restore();

        let error = result.expect_err("parent owner class must deny deletion");
        let Error::Io(io_error) = error else {
            panic!("expected typed I/O error, got {error}");
        };
        assert_eq!(io_error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(session_path.exists(), "primary session must be preserved");
        assert!(wal_path.exists(), "WAL sidecar must be preserved");
        assert!(shm_path.exists(), "SHM sidecar must be preserved");
        assert!(journal_path.exists(), "rollback journal must be preserved");
        assert!(v2_path.exists(), "V2 sidecar must be preserved");
        assert!(
            !invocation_marker.exists(),
            "trash must not run after permission preflight denial"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deleting_already_absent_session_is_idempotent_in_read_only_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let marker_dir = tempfile::tempdir().expect("marker tempdir");
        let invocation_marker = marker_dir.path().join("trash-invoked");
        let trash_script = tmp.path().join("fake-trash.sh");
        fs::write(
            &trash_script,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 0\n",
                invocation_marker.display()
            ),
        )
        .expect("write trash script");
        let mut script_permissions = fs::metadata(&trash_script)
            .expect("trash script metadata")
            .permissions();
        script_permissions.set_mode(0o755);
        fs::set_permissions(&trash_script, script_permissions).expect("chmod trash script");

        let absent_session = tmp.path().join("already-absent.jsonl");
        let mut mode_guard = UnixModeGuard::apply(tmp.path(), 0o577);
        let result = delete_session_file_with_trash_cmd(
            &absent_session,
            trash_script.to_string_lossy().as_ref(),
        );
        mode_guard.restore();

        result.expect("deleting an already-absent session must remain idempotent");
        assert!(
            !invocation_marker.exists(),
            "an idempotent no-op must not invoke the trash command"
        );
    }

    mod proptest_session_picker {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `truncate_session_id` never returns more chars than requested.
            #[test]
            fn truncate_respects_limit(s in "[a-z0-9\\-]{1,40}", max in 0..50usize) {
                let result = truncate_session_id(&s, max);
                assert!(result.chars().count() <= max);
            }

            /// `truncate_session_id` is a prefix of the original.
            #[test]
            fn truncate_is_prefix(s in "[a-z0-9\\-]{1,40}", max in 1..50usize) {
                let result = truncate_session_id(&s, max);
                assert!(s.starts_with(result));
            }

            /// `truncate_session_id` with max >= len returns the whole string.
            #[test]
            fn truncate_large_limit_identity(s in "[a-z0-9\\-]{1,20}") {
                let len = s.chars().count();
                let result = truncate_session_id(&s, len + 10);
                assert_eq!(result, s.as_str());
            }

            /// `truncate_session_id` with max=0 returns empty.
            #[test]
            fn truncate_zero_is_empty(s in "\\PC{1,20}") {
                assert_eq!(truncate_session_id(&s, 0), "");
            }

            /// `format_time` never panics on arbitrary strings.
            #[test]
            fn format_time_never_panics(ts in "\\PC{0,40}") {
                let _ = format_time(&ts);
            }

            /// Valid RFC3339 timestamps format to YYYY-MM-DD HH:MM.
            #[test]
            fn format_time_valid_rfc3339(
                year in 2020..2030u32,
                month in 1..12u32,
                day in 1..28u32,
                hour in 0..23u32,
                min in 0..59u32
            ) {
                let ts = format!("{year}-{month:02}-{day:02}T{hour:02}:{min:02}:00Z");
                let result = format_time(&ts);
                assert!(result.contains(&format!("{year}-{month:02}-{day:02}")));
                assert!(result.contains(&format!("{hour:02}:{min:02}")));
            }

            /// Invalid timestamps are returned as-is.
            #[test]
            fn format_time_invalid_passthrough(s in "[a-z]{5,15}") {
                assert_eq!(format_time(&s), s);
            }

            /// `is_session_file_path` accepts .jsonl files.
            #[test]
            fn is_session_file_path_accepts_jsonl(name in "[a-z]{1,10}") {
                let path = format!("/tmp/{name}.jsonl");
                assert!(is_session_file_path(Path::new(&path)));
            }

            /// `is_session_file_path` rejects random extensions.
            #[test]
            fn is_session_file_path_rejects_other(
                name in "[a-z]{1,10}",
                ext in "[a-z]{1,5}"
            ) {
                prop_assume!(ext != "jsonl" && ext != "sqlite");
                let path = format!("/tmp/{name}.{ext}");
                assert!(!is_session_file_path(Path::new(&path)));
            }

            /// `is_session_file_path` rejects files without extensions.
            #[test]
            fn is_session_file_path_rejects_no_ext(name in "[a-z]{1,10}") {
                assert!(!is_session_file_path(Path::new(&format!("/tmp/{name}"))));
            }

            /// `truncate_session_id` handles multi-byte unicode.
            #[test]
            fn truncate_unicode(max in 0..10usize) {
                let s = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F604}"; // 5 emoji
                let result = truncate_session_id(s, max);
                assert!(result.chars().count() <= max);
                assert!(s.starts_with(result));
            }

            /// Truncation is idempotent for a fixed limit.
            #[test]
            fn truncate_idempotent(s in "\\PC{1,40}", max in 0..40usize) {
                let once = truncate_session_id(&s, max);
                let twice = truncate_session_id(once, max);
                assert_eq!(once, twice);
            }

            /// Valid RFC3339 formatting is fixed-width (`YYYY-MM-DD HH:MM`).
            #[test]
            fn format_time_valid_rfc3339_fixed_width(
                year in 2020..2030u32,
                month in 1..12u32,
                day in 1..28u32,
                hour in 0..23u32,
                min in 0..59u32
            ) {
                let ts = format!("{year}-{month:02}-{day:02}T{hour:02}:{min:02}:00Z");
                let result = format_time(&ts);
                assert_eq!(result.len(), 16);
            }
        }
    }
}
