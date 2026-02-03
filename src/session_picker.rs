//! Session picker TUI for selecting from available sessions.
//!
//! Provides an interactive list for choosing which session to resume.

use std::fmt::Write;

use bubbletea::{Cmd, KeyMsg, KeyType, Message, Program, quit};
use lipgloss::Style;

use crate::session::Session;
use crate::session_index::{SessionIndex, SessionMeta};

/// Format a timestamp for display.
fn format_time(timestamp: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        dt.format("%Y-%m-%d %H:%M").to_string()
    } else {
        timestamp.to_string()
    }
}

/// The session picker TUI model.
#[derive(bubbletea::Model)]
pub struct SessionPicker {
    sessions: Vec<SessionMeta>,
    selected: usize,
    chosen: Option<usize>,
    cancelled: bool,
}

impl SessionPicker {
    /// Create a new session picker.
    pub fn new(sessions: Vec<SessionMeta>) -> Self {
        Self {
            sessions,
            selected: 0,
            chosen: None,
            cancelled: false,
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

    fn init(&self) -> Option<Cmd> {
        None
    }

    fn update(&mut self, msg: Message) -> Option<Cmd> {
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
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
                _ => {}
            }
        }
        None
    }

    fn view(&self) -> String {
        let mut output = String::new();

        // Header
        let title_style = Style::new().bold().foreground("212");
        let _ = writeln!(
            output,
            "\n  {}\n",
            title_style.render("Select a session to resume")
        );

        if self.sessions.is_empty() {
            let dim_style = Style::new().foreground("241");
            let _ = writeln!(
                output,
                "  {}",
                dim_style.render("No sessions found for this project.")
            );
        } else {
            // Column headers
            let header_style = Style::new().foreground("241").bold();
            let _ = writeln!(
                output,
                "  {:<20}  {:<30}  {:<8}  {}",
                header_style.render("Time"),
                header_style.render("Name"),
                header_style.render("Messages"),
                header_style.render("Session ID")
            );
            output.push_str("  ");
            output.push_str(&"-".repeat(78));
            output.push('\n');

            // Session rows
            for (i, session) in self.sessions.iter().enumerate() {
                let is_selected = i == self.selected;

                let row_style = if is_selected {
                    Style::new().bold().foreground("cyan")
                } else {
                    Style::new()
                };

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
                    row_style.render(&format!(
                        " {time:<20}  {name:<30}  {messages:<8}  {id}"
                    ))
                );
            }
        }

        // Help text
        output.push('\n');
        let help_style = Style::new().foreground("241");
        let _ = writeln!(
            output,
            "  {}",
            help_style.render("↑/↓/j/k: navigate  Enter: select  Esc/q: cancel")
        );

        output
    }
}

/// List sessions for the current working directory using the session index.
pub fn list_sessions_for_cwd() -> Vec<SessionMeta> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };

    let index = SessionIndex::new();

    // Try to reindex if needed (this is a no-op if already indexed)
    let _ = index.reindex_all();

    index
        .list_sessions(Some(&cwd.display().to_string()))
        .unwrap_or_default()
}

/// Run the session picker and return the selected session.
pub async fn pick_session() -> Option<Session> {
    let sessions = list_sessions_for_cwd();

    if sessions.is_empty() {
        return None;
    }

    if sessions.len() == 1 {
        // Only one session, just open it
        return Session::open(&sessions[0].path).await.ok();
    }

    let picker = SessionPicker::new(sessions);

    // Run the TUI
    let result = Program::new(picker).with_alt_screen().run();

    match result {
        Ok(picker) => {
            if picker.was_cancelled() {
                return None;
            }

            if let Some(path) = picker.selected_path() {
                Session::open(path).await.ok()
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time() {
        let ts = "2025-01-15T10:30:00.000Z";
        let formatted = format_time(ts);
        assert!(formatted.contains("2025-01-15"));
        assert!(formatted.contains("10:30"));
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
        let down_msg = Message::new(KeyMsg {
            key_type: KeyType::Down,
            runes: vec![],
            alt: false,
            paste: false,
        });
        picker.update(down_msg);
        assert_eq!(picker.selected, 1);

        // Navigate up
        let up_msg = Message::new(KeyMsg {
            key_type: KeyType::Up,
            runes: vec![],
            alt: false,
            paste: false,
        });
        picker.update(up_msg);
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
        let j_msg = Message::new(KeyMsg {
            key_type: KeyType::Runes,
            runes: vec!['j'],
            alt: false,
            paste: false,
        });
        picker.update(j_msg);
        assert_eq!(picker.selected, 1);

        // Navigate up with 'k'
        let k_msg = Message::new(KeyMsg {
            key_type: KeyType::Runes,
            runes: vec!['k'],
            alt: false,
            paste: false,
        });
        picker.update(k_msg);
        assert_eq!(picker.selected, 0);
    }
}
