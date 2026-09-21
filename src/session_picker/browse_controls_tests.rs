//! Regression coverage for the production picker control path. These tests
//! feed ordinary key messages through SessionPicker::update, not a test-only
//! replica of the projection or dispatch logic.

use super::*;
use bubbletea::{Message, WindowSizeMsg};

fn records() -> Vec<SessionMeta> {
    [Some("Zeta"), Some("alpha"), Some("ALPHA"), None]
        .into_iter()
        .enumerate()
        .map(|(index, name)| SessionMeta {
            path: format!("/sessions/record-{index}.jsonl"),
            id: format!("id-{index}"),
            cwd: "/project".to_string(),
            timestamp: "2026-09-01T00:00:00Z".to_string(),
            message_count: 1,
            last_modified_ms: [10, 30, 20, 30][index],
            size_bytes: 10,
            name: name.map(str::to_string),
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

fn selected_record(picker: &SessionPicker) -> Option<usize> {
    picker.browser.original_index(picker.selected)
}

#[test]
fn sort_cycles_keep_record_identity_and_restore_original_order() {
    let mut picker = SessionPicker::new(records());
    assert_eq!(picker.browser.visible, vec![0, 1, 2, 3]);
    press(&mut picker, KeyType::CtrlS, "");
    assert_eq!(picker.browser.sort, BrowserSort::Recent);
    assert_eq!(picker.browser.visible, vec![1, 3, 2, 0]);
    assert_eq!(selected_record(&picker), Some(0));
    press(&mut picker, KeyType::CtrlS, "");
    assert_eq!(picker.browser.sort, BrowserSort::Name);
    assert_eq!(picker.browser.visible, vec![1, 2, 0, 3]);
    assert_eq!(selected_record(&picker), Some(0));
    press(&mut picker, KeyType::CtrlS, "");
    assert_eq!(picker.browser.sort, BrowserSort::Listed);
    assert_eq!(picker.browser.visible, vec![0, 1, 2, 3]);
    assert_eq!(selected_record(&picker), Some(0));
    assert!(press(&mut picker, KeyType::Enter, "").is_some());
    assert_eq!(picker.selected_path(), Some("/sessions/record-0.jsonl"));
}

#[test]
fn named_filter_excludes_absent_empty_and_whitespace_only_names() {
    let mut rows = records();
    rows[0].name = Some(String::new());
    rows[1].name = Some(" \t\u{3000}".to_string());
    rows[2].name = Some("  ALPHA  ".to_string());
    let mut picker = SessionPicker::new(rows);
    press(&mut picker, KeyType::CtrlN, "");
    assert_eq!(picker.browser.visible, vec![2]);
    assert_eq!(selected_record(&picker), Some(2));
    assert!(picker.view().contains("named only"));
    press(&mut picker, KeyType::CtrlN, "");
    assert_eq!(picker.browser.visible, vec![0, 1, 2, 3]);
    assert_eq!(selected_record(&picker), Some(2));
    assert!(!picker.view().contains("named only"));
}

#[test]
fn zero_named_matches_cannot_confirm_or_delete_a_hidden_session() {
    let mut rows = records();
    for row in &mut rows {
        row.name = None;
    }
    let mut picker = SessionPicker::new(rows);
    press(&mut picker, KeyType::CtrlN, "");
    assert!(picker.browser.visible.is_empty());
    assert!(press(&mut picker, KeyType::Enter, "").is_none());
    press(&mut picker, KeyType::CtrlD, "");
    assert!(picker.confirm_delete.is_none());
    assert!(picker.chosen.is_none());
    assert!(!picker.cancelled);
    press(&mut picker, KeyType::CtrlS, "");
    press(&mut picker, KeyType::CtrlN, "");
    assert_eq!(picker.browser.visible, vec![1, 3, 2, 0]);
    assert_eq!(selected_record(&picker), Some(1));
}

#[test]
fn search_and_named_filter_intersect_in_the_selected_sort_order() {
    let mut picker = SessionPicker::new(records());
    press(&mut picker, KeyType::CtrlN, "");
    press(&mut picker, KeyType::CtrlS, "");
    press(&mut picker, KeyType::Runes, "/");
    press(&mut picker, KeyType::Runes, "ALPHA PROJECT");
    assert_eq!(picker.browser.visible, vec![1, 2]);
    assert!(press(&mut picker, KeyType::Enter, "").is_none());
    assert!(press(&mut picker, KeyType::Enter, "").is_some());
    assert_eq!(picker.selected_path(), Some("/sessions/record-1.jsonl"));
}

#[test]
fn sorted_delete_arms_original_record_and_modal_blocks_view_changes() {
    let mut picker = SessionPicker::new(records());
    press(&mut picker, KeyType::CtrlS, "");
    press(&mut picker, KeyType::PgUp, "");
    assert_eq!(selected_record(&picker), Some(1));
    press(&mut picker, KeyType::CtrlD, "");
    assert_eq!(picker.confirm_delete, Some(1));
    for kind in [KeyType::CtrlS, KeyType::CtrlN, KeyType::CtrlP] {
        press(&mut picker, kind, "");
    }
    assert_eq!(picker.confirm_delete, Some(1));
    assert_eq!(picker.browser.sort, BrowserSort::Recent);
    assert!(!picker.browser.named_only);
    assert!(picker.browser.show_path);
    press(&mut picker, KeyType::Esc, "");
    assert!(picker.confirm_delete.is_none());
    assert_eq!(selected_record(&picker), Some(1));
}

#[test]
fn post_delete_projection_rebuild_keeps_filter_sort_and_valid_indices() {
    let mut picker = SessionPicker::new(records());
    press(&mut picker, KeyType::CtrlN, "");
    press(&mut picker, KeyType::CtrlS, "");
    press(&mut picker, KeyType::PgUp, "");
    assert_eq!(selected_record(&picker), Some(1));
    // The persistence layer already owns deletion. Exercise its production
    // post-delete helper after the same vector removal it performs.
    picker.sessions.remove(1);
    picker.refresh_browser_after_delete();
    assert_eq!(picker.browser.visible, vec![1, 0]);
    assert_eq!(selected_record(&picker), Some(1));
    assert_eq!(picker.sessions[1].id, "id-2");
    assert!(picker.browser.named_only);
    assert_eq!(picker.browser.sort, BrowserSort::Recent);
}

#[test]
fn path_toggle_only_changes_display_not_selection_or_query_matching() {
    let mut picker = SessionPicker::new(records());
    let path = "/sessions/record-0.jsonl";
    assert!(picker.view().contains(path));
    press(&mut picker, KeyType::CtrlP, "");
    assert!(!picker.view().contains(path));
    assert_eq!(selected_record(&picker), Some(0));
    press(&mut picker, KeyType::Runes, "/");
    press(&mut picker, KeyType::Runes, "record-2.jsonl");
    assert_eq!(picker.browser.visible, vec![2]);
    press(&mut picker, KeyType::Enter, "");
    press(&mut picker, KeyType::CtrlP, "");
    assert!(picker.view().contains("/sessions/record-2.jsonl"));
    assert_eq!(selected_record(&picker), Some(2));
}

#[test]
fn controls_honor_custom_bindings_unbinding_and_rendered_key_hints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    std::fs::write(
        &path,
        r#"{"toggleSessionNamedFilter":["ctrl+g"],"toggleSessionSort":["ctrl+b"],"toggleSessionPath":[]}"#,
    )
    .unwrap();
    let mut picker =
        SessionPicker::new(records()).with_keybindings(KeyBindings::load(&path).unwrap());
    for kind in [KeyType::CtrlN, KeyType::CtrlS, KeyType::CtrlP] {
        press(&mut picker, kind, "");
    }
    assert!(!picker.browser.named_only);
    assert_eq!(picker.browser.sort, BrowserSort::Listed);
    assert!(picker.browser.show_path);
    press(&mut picker, KeyType::CtrlG, "");
    press(&mut picker, KeyType::CtrlB, "");
    assert_eq!(picker.browser.visible, vec![1, 2, 0]);
    picker.update(Message::new(WindowSizeMsg {
        width: 180,
        height: 24,
    }));
    let view = picker.view();
    assert!(view.contains("ctrl+g named"));
    assert!(view.contains("ctrl+b sort"));
    assert!(view.contains("unbound path"));
}

#[test]
fn pasted_keys_never_toggle_and_rebound_letters_remain_search_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    std::fs::write(&path, r#"{"toggleSessionNamedFilter":["n"]}"#).unwrap();
    let mut picker =
        SessionPicker::new(records()).with_keybindings(KeyBindings::load(&path).unwrap());
    picker.update(Message::new(KeyMsg {
        key_type: KeyType::Runes,
        runes: vec!['n'],
        alt: false,
        paste: true,
    }));
    assert!(!picker.browser.named_only);
    press(&mut picker, KeyType::Runes, "/");
    press(&mut picker, KeyType::Runes, "n");
    assert_eq!(picker.browser.query, "n");
    assert!(!picker.browser.named_only);
    press(&mut picker, KeyType::Esc, "");
    press(&mut picker, KeyType::Runes, "n");
    assert!(picker.browser.named_only);
}

#[test]
fn search_cancel_restores_record_after_sort_changes_without_reverting_controls() {
    let mut picker = SessionPicker::new(records());
    press(&mut picker, KeyType::Runes, "/");
    press(&mut picker, KeyType::Runes, "alpha");
    press(&mut picker, KeyType::CtrlS, "");
    assert_eq!(picker.browser.visible, vec![1, 2]);
    press(&mut picker, KeyType::Esc, "");
    assert!(picker.browser.query.is_empty());
    assert_eq!(picker.browser.sort, BrowserSort::Recent);
    assert_eq!(picker.browser.visible, vec![1, 3, 2, 0]);
    assert_eq!(selected_record(&picker), Some(0));
    assert!(!picker.cancelled);
}

#[test]
fn empty_collection_and_small_viewports_remain_bounded_under_all_controls() {
    let mut picker = SessionPicker::new(Vec::new());
    for height in [0, 1, 8, 9, 12, 24] {
        picker.update(Message::new(WindowSizeMsg { width: 20, height }));
        for kind in [KeyType::CtrlN, KeyType::CtrlS, KeyType::CtrlP, KeyType::PgDown] {
            press(&mut picker, kind, "");
            assert!(picker.browser.visible.is_empty());
            assert_eq!(picker.selected, 0);
            assert_eq!(picker.browser.top, 0);
            assert!(picker.view().lines().count() <= usize::from(height).max(1));
        }
    }
}
