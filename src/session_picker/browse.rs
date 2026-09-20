//! Search and viewport state for the resume picker. Visible row indices never
//! double as persistence identities: confirm/delete resolve through `visible`.

use super::{SessionMeta, SessionPicker, format_time, truncate_session_id};
use crate::keybindings::{AppAction, KeyBinding, KeyBindings};
use bubbletea::{Cmd, KeyMsg, KeyType, quit};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_QUERY_BYTES: usize = 1024;
const PICKER_ACTIONS: &[AppAction] = &[
    AppAction::SelectCancel,
    AppAction::SelectConfirm,
    AppAction::SelectPageUp,
    AppAction::SelectPageDown,
    AppAction::SelectUp,
    AppAction::SelectDown,
    AppAction::DeleteSession,
];

pub(super) struct BrowserState {
    visible: Vec<usize>,
    query: String,
    /// Query and original record selected before entering search. Escape rolls
    /// back the edit, not just its text; Enter commits without opening a session.
    search_before: Option<(String, Option<usize>)>,
    top: usize,
    width: usize,
    height: usize,
    bindings: KeyBindings,
}

impl BrowserState {
    pub(super) fn new(count: usize) -> Self {
        Self {
            visible: (0..count).collect(),
            query: String::new(),
            search_before: None,
            top: 0,
            width: 96,
            height: 24,
            bindings: KeyBindings::new(),
        }
    }

    fn page_size(&self) -> usize {
        self.height.saturating_sub(8).max(1)
    }

    fn original_index(&self, selected: usize) -> Option<usize> {
        self.visible.get(selected).copied()
    }

    fn keep_visible(&mut self, selected: &mut usize) {
        *selected = (*selected).min(self.visible.len().saturating_sub(1));
        let page = self.page_size();
        self.top = self.top.min(self.visible.len().saturating_sub(page));
        if *selected < self.top {
            self.top = *selected;
        } else if *selected >= self.top.saturating_add(page) {
            self.top = selected.saturating_add(1).saturating_sub(page);
        }
    }

    fn rebuild(&mut self, sessions: &[SessionMeta], selected: &mut usize, anchor: Option<usize>) {
        let query = self.query.to_lowercase();
        let terms: Vec<_> = query.split_whitespace().collect();
        self.visible = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                if terms.is_empty() {
                    return true;
                }
                let fields = [
                    session.id.to_lowercase(),
                    session.name.as_deref().unwrap_or_default().to_lowercase(),
                    session.cwd.to_lowercase(),
                    session.path.to_lowercase(),
                ];
                terms
                    .iter()
                    .all(|term| fields.iter().any(|field| field.contains(*term)))
            })
            .map(|(index, _)| index)
            .collect();
        *selected = anchor
            .and_then(|index| {
                self.visible
                    .iter()
                    .position(|candidate| *candidate == index)
            })
            .unwrap_or(0);
        self.keep_visible(selected);
    }

    fn action(&self, key: &KeyMsg) -> Option<AppAction> {
        let binding = KeyBinding::from_bubbletea_key(key)?;
        let matches = self.bindings.matching_actions(&binding);
        PICKER_ACTIONS
            .iter()
            .copied()
            .find(|action| matches.contains(action))
    }

    fn keys(&self, action: AppAction) -> String {
        let keys = self.bindings.get_bindings(action);
        if keys.is_empty() {
            "unbound".to_string()
        } else {
            keys.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("/")
        }
    }

    // Preserve historical j/k/q aliases only while their action retains its
    // defaults. Explicit replacement or unbinding must really remove the key.
    fn default_alias(&self, key: &KeyMsg) -> Option<AppAction> {
        if key.alt || key.key_type != KeyType::Runes {
            return None;
        }
        let action = match key.runes.as_slice() {
            ['j'] => AppAction::SelectDown,
            ['k'] => AppAction::SelectUp,
            ['q'] => AppAction::SelectCancel,
            _ => return None,
        };
        (self.bindings.get_bindings(action) == KeyBindings::new().get_bindings(action))
            .then_some(action)
    }
}

impl SessionPicker {
    /// Override picker actions with the same catalog used by the main UI.
    #[must_use]
    pub fn with_keybindings(mut self, bindings: KeyBindings) -> Self {
        self.browser.bindings = bindings;
        self
    }

    pub(super) fn resize_browser(&mut self, width: u16, height: u16) {
        self.browser.width = usize::from(width).max(1);
        self.browser.height = usize::from(height).max(1);
        self.browser.keep_visible(&mut self.selected);
    }

    pub(super) fn refresh_browser_after_delete(&mut self) {
        let position = self.selected;
        self.browser
            .rebuild(&self.sessions, &mut self.selected, None);
        self.selected = position;
        self.browser.keep_visible(&mut self.selected);
    }

    pub(super) fn browser_cancel_key(&self, key: &KeyMsg) -> bool {
        self.browser.action(key) == Some(AppAction::SelectCancel)
    }

    fn edit_query(&mut self, chars: impl IntoIterator<Item = char>) {
        let anchor = self.browser.original_index(self.selected);
        for ch in chars {
            let ch = if ch.is_whitespace() { ' ' } else { ch };
            if !safe_char(ch) {
                continue;
            }
            if self.browser.query.len().saturating_add(ch.len_utf8()) > MAX_QUERY_BYTES {
                break;
            }
            self.browser.query.push(ch);
        }
        self.browser
            .rebuild(&self.sessions, &mut self.selected, anchor);
    }

    fn handle_search_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        // A bracketed paste is text, never a selection or deletion command.
        if key.paste || (key.key_type == KeyType::Runes && !key.alt) {
            self.edit_query(key.runes.iter().copied());
            return None;
        }
        if key.key_type == KeyType::Space && !key.alt {
            self.edit_query([' ']);
            return None;
        }
        match self.browser.action(key) {
            Some(AppAction::SelectCancel) => {
                if let Some((query, anchor)) = self.browser.search_before.take() {
                    self.browser.query = query;
                    self.browser
                        .rebuild(&self.sessions, &mut self.selected, anchor);
                }
            }
            Some(AppAction::SelectConfirm) => {
                self.browser.search_before = None;
            }
            Some(
                action @ (AppAction::SelectUp
                | AppAction::SelectDown
                | AppAction::SelectPageUp
                | AppAction::SelectPageDown),
            ) => {
                self.move_selection(action);
            }
            _ if key.key_type == KeyType::Backspace || key.key_type == KeyType::CtrlH => {
                let anchor = self.browser.original_index(self.selected);
                self.browser.query.pop();
                self.browser
                    .rebuild(&self.sessions, &mut self.selected, anchor);
            }
            _ if key.key_type == KeyType::CtrlU => {
                let anchor = self.browser.original_index(self.selected);
                self.browser.query.clear();
                self.browser
                    .rebuild(&self.sessions, &mut self.selected, anchor);
            }
            _ => {}
        }
        None
    }

    fn move_selection(&mut self, action: AppAction) {
        let page = self.browser.page_size();
        self.selected = match action {
            AppAction::SelectUp => self.selected.saturating_sub(1),
            AppAction::SelectDown => self.selected.saturating_add(1),
            AppAction::SelectPageUp => self.selected.saturating_sub(page),
            AppAction::SelectPageDown => self.selected.saturating_add(page),
            _ => self.selected,
        };
        self.browser.keep_visible(&mut self.selected);
    }

    pub(super) fn handle_browse_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        if self.browser.search_before.is_some() {
            return self.handle_search_key(key);
        }
        if key.paste {
            return None;
        }
        let action = self
            .browser
            .action(key)
            .or_else(|| self.browser.default_alias(key));
        match action {
            Some(AppAction::SelectConfirm) => {
                self.chosen = self.browser.original_index(self.selected);
                // A zero-result search is not an empty session collection. Keep
                // it editable instead of silently starting a new conversation.
                if self.chosen.is_some() || self.sessions.is_empty() {
                    return Some(quit());
                }
            }
            Some(AppAction::SelectCancel) => {
                self.cancelled = true;
                return Some(quit());
            }
            Some(AppAction::DeleteSession) => {
                if let Some(index) = self.browser.original_index(self.selected) {
                    self.confirm_delete = Some(index);
                    self.status_message = Some("Delete session? Press y/n to confirm.".to_string());
                }
            }
            Some(action) => self.move_selection(action),
            None if !key.alt && key.key_type == KeyType::Runes && key.runes == ['/'] => {
                self.browser.search_before = Some((
                    self.browser.query.clone(),
                    self.browser.original_index(self.selected),
                ));
            }
            None => {}
        }
        None
    }

    pub(super) fn render_browser(&self) -> String {
        let browser = &self.browser;
        let width = browser.width;
        let mut lines = Vec::new();
        let row_text = |meta: &SessionMeta| {
            format!(
                "{}  {}  {:<8}  {}",
                cell(&format_time(&meta.timestamp), 20),
                cell(meta.name.as_deref().unwrap_or("-"), 28),
                meta.message_count,
                display_line(truncate_session_id(&meta.id, 8), 16),
            )
        };
        if browser.height < 9 {
            if let Some(message) = &self.status_message {
                lines.push(
                    self.styles
                        .warning_bold
                        .render(&display_line(message, width)),
                );
            }
            let current = browser
                .original_index(self.selected)
                .and_then(|i| self.sessions.get(i));
            lines.push(display_line(
                &current.map_or_else(|| "No matching sessions".to_string(), row_text),
                width,
            ));
            lines.push(display_line(&format!("Search: {}", browser.query), width));
            return lines
                .into_iter()
                .take(browser.height)
                .collect::<Vec<_>>()
                .join("\n");
        }
        lines.push(
            self.styles
                .title
                .render(&display_line("Select a session to resume", width)),
        );
        let start = if browser.visible.is_empty() {
            0
        } else {
            browser.top + 1
        };
        let end = browser
            .top
            .saturating_add(browser.page_size())
            .min(browser.visible.len());
        lines.push(display_line(
            &format!(
                "{} matches / {} sessions · rows {start}-{end}",
                browser.visible.len(),
                self.sessions.len(),
            ),
            width,
        ));
        lines.push(display_line(
            &format!(
                "Search{}: {}",
                if browser.search_before.is_some() {
                    " (editing)"
                } else {
                    ""
                },
                browser.query,
            ),
            width,
        ));
        lines.push(self.styles.muted_bold.render(&display_line(
            "  Time                  Name                          Messages  Session ID",
            width,
        )));
        if browser.visible.is_empty() {
            lines.push(display_line(
                if self.sessions.is_empty() {
                    "No sessions found for this project."
                } else {
                    "No matching sessions. Press / to edit the search."
                },
                width,
            ));
        }
        for (offset, &idx) in browser.visible[browser.top..end].iter().enumerate() {
            let position = browser.top + offset;
            let Some(session) = self.sessions.get(idx) else {
                continue;
            };
            let selected = position == self.selected;
            let inner = display_line(&row_text(session), width.saturating_sub(2));
            let row = if selected {
                format!("> {}", self.styles.selection.render(&inner))
            } else {
                format!("  {inner}")
            };
            lines.push(row);
        }
        let help = if browser.search_before.is_some() {
            format!(
                "{}: apply search  {}: undo search  Ctrl+U: clear",
                browser.keys(AppAction::SelectConfirm),
                browser.keys(AppAction::SelectCancel)
            )
        } else {
            format!(
                "/: search  {}: select  {}: delete  {}: cancel",
                browser.keys(AppAction::SelectConfirm),
                browser.keys(AppAction::DeleteSession),
                browser.keys(AppAction::SelectCancel)
            )
        };
        lines.push(self.styles.muted.render(&display_line(&help, width)));
        lines.push(self.styles.muted.render(&display_line(
            &format!(
                "{} / {}: move  {} / {}: page",
                browser.keys(AppAction::SelectUp),
                browser.keys(AppAction::SelectDown),
                browser.keys(AppAction::SelectPageUp),
                browser.keys(AppAction::SelectPageDown),
            ),
            width,
        )));
        let path = browser
            .original_index(self.selected)
            .and_then(|i| self.sessions.get(i))
            .map_or("", |meta| meta.path.as_str());
        lines.push(self.styles.muted.render(&display_line(path, width)));
        if let Some(message) = &self.status_message {
            lines.push(
                self.styles
                    .warning_bold
                    .render(&display_line(message, width)),
            );
        }
        lines
            .into_iter()
            .take(browser.height)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn safe_char(ch: char) -> bool {
    !ch.is_control() && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Bound both display columns and work on hostile metadata with many zero-width
/// marks. Strip control/bidi characters before applying trusted terminal styles.
fn display_line(value: &str, columns: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for ch in value
        .chars()
        .take(columns.saturating_mul(8).saturating_add(32))
    {
        let ch = if safe_char(ch) { ch } else { ' ' };
        let width = ch.width().unwrap_or(0);
        if used + width > columns {
            break;
        }
        used += width;
        output.push(ch);
    }
    output
}

fn cell(value: &str, columns: usize) -> String {
    let mut output = display_line(value, columns);
    let padding = columns.saturating_sub(output.width());
    output.extend(std::iter::repeat_n(' ', padding));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubbletea::{Message, WindowSizeMsg};
    use std::path::Path;

    fn sessions(count: usize) -> Vec<SessionMeta> {
        (0..count)
            .map(|i| SessionMeta {
                path: format!("/sessions/{i:04}.jsonl"),
                id: format!("session-{i:04}"),
                cwd: "/project".to_string(),
                timestamp: "2026-09-01T00:00:00Z".to_string(),
                message_count: u64::try_from(i).unwrap(),
                last_modified_ms: i64::try_from(i).unwrap(),
                size_bytes: 10,
                name: Some(format!("Work {i:04}")),
            })
            .collect()
    }

    fn press(picker: &mut SessionPicker, key_type: KeyType, runes: &str) -> Option<Cmd> {
        picker.update(Message::new(KeyMsg {
            key_type,
            runes: runes.chars().collect(),
            alt: false,
            paste: false,
        }))
    }

    fn search(picker: &mut SessionPicker, query: &str) {
        press(picker, KeyType::Runes, "/");
        press(picker, KeyType::CtrlU, "");
        press(picker, KeyType::Runes, query);
        assert!(press(picker, KeyType::Enter, "").is_none());
    }

    #[test]
    fn full_history_is_paged_without_dropping_old_records() {
        let mut picker = SessionPicker::new(sessions(1000));
        picker.update(Message::new(WindowSizeMsg {
            width: 96,
            height: 18,
        }));
        assert!(picker.view().lines().count() <= 18);
        assert!(!picker.view().contains("Work 0999"));
        for _ in 0..110 {
            press(&mut picker, KeyType::PgDown, "");
        }
        assert_eq!(picker.selected, 999);
        assert!(picker.view().contains("Work 0999"));
        press(&mut picker, KeyType::Enter, "");
        assert_eq!(picker.selected_path(), Some("/sessions/0999.jsonl"));
        for _ in 0..110 {
            press(&mut picker, KeyType::PgUp, "");
        }
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn search_matches_all_terms_case_insensitively_across_metadata_fields() {
        let mut picker = SessionPicker::new(sessions(100));
        search(&mut picker, "WORK 0091 PROJECT");
        assert_eq!(picker.browser.visible, vec![91]);
        press(&mut picker, KeyType::Enter, "");
        assert_eq!(picker.selected_path(), Some("/sessions/0091.jsonl"));
    }

    #[test]
    fn ordinary_space_keys_form_multiword_queries() {
        let mut picker = SessionPicker::new(sessions(100));
        press(&mut picker, KeyType::Runes, "/");
        press(&mut picker, KeyType::Runes, "WORK");
        press(&mut picker, KeyType::Space, "");
        press(&mut picker, KeyType::Runes, "0091");
        assert_eq!(picker.browser.query, "WORK 0091");
        assert_eq!(picker.browser.visible, vec![91]);
    }

    #[test]
    fn no_match_cannot_select_or_arm_a_hidden_record_for_deletion() {
        let mut picker = SessionPicker::new(sessions(3));
        search(&mut picker, "does-not-exist");
        assert!(press(&mut picker, KeyType::Enter, "").is_none());
        press(&mut picker, KeyType::CtrlD, "");
        assert!(picker.chosen.is_none());
        assert!(picker.confirm_delete.is_none());
        assert!(!picker.cancelled);
        assert!(picker.view().contains("No matching sessions"));
    }

    #[test]
    fn escape_restores_query_and_original_selection_without_quitting() {
        let mut picker = SessionPicker::new(sessions(5));
        press(&mut picker, KeyType::Down, "");
        press(&mut picker, KeyType::Runes, "/");
        press(&mut picker, KeyType::Runes, "0004");
        assert_eq!(picker.browser.visible, vec![4]);
        press(&mut picker, KeyType::Esc, "");
        assert_eq!(picker.selected, 1);
        assert!(picker.browser.query.is_empty());
        assert!(!picker.cancelled);
    }

    #[test]
    fn filtered_delete_targets_the_original_record_and_keeps_confirmation_modal() {
        let mut picker = SessionPicker::new(sessions(5));
        search(&mut picker, "0004");
        press(&mut picker, KeyType::CtrlD, "");
        assert_eq!(picker.confirm_delete, Some(4));
        press(&mut picker, KeyType::Runes, "/");
        press(&mut picker, KeyType::Up, "");
        assert_eq!(picker.confirm_delete, Some(4));
        picker.update(Message::new(KeyMsg {
            key_type: KeyType::Runes,
            runes: vec!['y'],
            alt: false,
            paste: true,
        }));
        assert_eq!(
            picker.confirm_delete,
            Some(4),
            "paste must not confirm deletion"
        );
        press(&mut picker, KeyType::Esc, "");
        assert!(picker.confirm_delete.is_none());
        assert_eq!(picker.browser.visible, vec![4]);
    }

    #[test]
    fn query_editing_handles_unicode_and_paste_as_bounded_literal_text() {
        let mut picker = SessionPicker::new(sessions(5));
        press(&mut picker, KeyType::Runes, "/");
        press(&mut picker, KeyType::Runes, "日本");
        press(&mut picker, KeyType::Backspace, "");
        assert_eq!(picker.browser.query, "日");
        picker.update(Message::new(KeyMsg {
            key_type: KeyType::Runes,
            runes: "q\ny\u{1b}\u{202e}".chars().collect(),
            alt: false,
            paste: true,
        }));
        assert_eq!(picker.browser.query, "日q y");
        assert!(picker.browser.search_before.is_some());
        assert!(!picker.cancelled);
        assert!(picker.confirm_delete.is_none());
        press(&mut picker, KeyType::Runes, &"界".repeat(MAX_QUERY_BYTES));
        assert!(picker.browser.query.len() <= MAX_QUERY_BYTES);
    }

    #[test]
    fn explicit_key_overrides_and_unbinding_are_honored_including_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        std::fs::write(&path, r#"{"selectDown":["ctrl+j"],"selectUp":[],"selectConfirm":["ctrl+g"],"deleteSession":[]}"#).unwrap();
        let mut picker =
            SessionPicker::new(sessions(5)).with_keybindings(KeyBindings::load(&path).unwrap());
        for (kind, runes) in [
            (KeyType::Down, ""),
            (KeyType::Runes, "j"),
            (KeyType::CtrlD, ""),
        ] {
            press(&mut picker, kind, runes);
        }
        assert_eq!(picker.selected, 0);
        assert!(picker.confirm_delete.is_none());
        press(&mut picker, KeyType::CtrlJ, "");
        assert_eq!(picker.selected, 1);
        press(&mut picker, KeyType::Up, "");
        press(&mut picker, KeyType::Runes, "k");
        assert_eq!(picker.selected, 1);
        assert!(press(&mut picker, KeyType::Enter, "").is_none());
        assert!(press(&mut picker, KeyType::CtrlG, "").is_some());
        assert_eq!(picker.selected_path(), Some("/sessions/0001.jsonl"));
    }

    #[test]
    fn resize_and_metadata_controls_cannot_escape_the_viewport() {
        let mut rows = sessions(30);
        rows[0].name = Some("visible\n\u{1b}[2J\u{202e}hidden".to_string());
        let mut picker = SessionPicker::new(rows);
        for height in [0, 1, 7, 8, 12, 30] {
            picker.update(Message::new(WindowSizeMsg { width: 20, height }));
            assert!(picker.view().lines().count() <= usize::from(height).max(1));
        }
        let clean = display_line("日本\n\u{1b}[2J\u{202e}", 6);
        assert!(clean.width() <= 6);
        assert!(!clean.contains(['\n', '\u{1b}', '\u{202e}']));
    }

    #[test]
    fn project_listing_keeps_sessions_older_than_the_previous_fifty_row_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/history-test-project");
        let project = tmp.path().join(crate::session::encode_cwd(cwd));
        std::fs::create_dir_all(&project).unwrap();
        for i in 0..55 {
            let mut header = crate::session::SessionHeader::new();
            header.id = format!("history-{i}");
            header.cwd = cwd.display().to_string();
            std::fs::write(
                project.join(format!("{i}.jsonl")),
                serde_json::to_vec(&header).unwrap(),
            )
            .unwrap();
        }
        let rows = super::super::list_sessions_for_project(cwd, Some(tmp.path()));
        assert_eq!(rows.len(), 55);
        for i in 0..55 {
            assert!(rows.iter().any(|row| row.id == format!("history-{i}")));
        }
    }
}
