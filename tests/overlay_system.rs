#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::overlay_system::{
    OverlayEntry, OverlayKind, OverlayStack, ToastLevel, ToastNotification, ToastQueue,
    WelcomeScreen,
};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_toast_queue_lifecycle() {
    let harness = TestHarness::new("toast_queue_lifecycle");

    let mut queue = ToastQueue::default();
    queue.push(ToastNotification::new(
        "t1",
        "Compaction Complete",
        "Saved 4500 tokens",
        ToastLevel::Success,
    ));
    queue.push(ToastNotification::new(
        "t2",
        "Background Job Failed",
        "Exit code 1",
        ToastLevel::Error,
    ));

    assert_eq!(queue.active_toasts().len(), 2);
    let first = queue.pop();
    assert!(first.is_some());
    assert_eq!(first.unwrap().title, "Compaction Complete");

    finish_case(&harness, "toast_queue_lifecycle");
}

#[test]
fn test_welcome_screen_and_tips() {
    let harness = TestHarness::new("welcome_screen_and_tips");

    let mut welcome = WelcomeScreen::default();
    assert!(!welcome.greeting.is_empty());
    assert!(!welcome.current_tip().is_empty());

    let initial_tip = welcome.current_tip().to_string();
    welcome.next_tip();
    let second_tip = welcome.current_tip().to_string();
    assert_ne!(initial_tip, second_tip);

    finish_case(&harness, "welcome_screen_and_tips");
}

#[test]
fn test_overlay_stack_focus_trap_and_esc() {
    let harness = TestHarness::new("overlay_stack_focus_trap");

    let mut stack = OverlayStack::default();
    assert!(stack.is_empty());

    // Push base picker
    stack.push(OverlayEntry {
        kind: OverlayKind::Picker,
        title: "Session List".to_string(),
        items: vec!["session-a".to_string(), "session-b".to_string()],
        selected_index: 0,
        is_dismissible: true,
    });

    // Push layered help modal over picker
    stack.push(OverlayEntry {
        kind: OverlayKind::Help,
        title: "Keybindings Help".to_string(),
        items: vec!["Esc: Dismiss".to_string(), "Enter: Select".to_string()],
        selected_index: 0,
        is_dismissible: true,
    });

    assert_eq!(stack.len(), 2);
    assert_eq!(stack.top().unwrap().title, "Keybindings Help");

    // Esc dismisses top (Help)
    assert!(stack.dismiss_top());
    assert_eq!(stack.len(), 1);
    assert_eq!(stack.top().unwrap().title, "Session List");

    // Esc dismisses base (Session List)
    assert!(stack.dismiss_top());
    assert!(stack.is_empty());

    finish_case(&harness, "overlay_stack_focus_trap");
}
