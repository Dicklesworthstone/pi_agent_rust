#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::computer::{ComputerSettings, ComputerTool};
use pi::config::Config;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolRegistry};
use serde_json::json;
use std::fs;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case {case} assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_computer_schema_and_metadata() {
    let harness = TestHarness::new("computer_schema");
    let tool = ComputerTool::new(harness.temp_dir());

    assert_eq!(tool.name(), "computer");
    assert_eq!(tool.label(), "Computer");
    assert!(!tool.description().is_empty());

    let params = tool.parameters();
    assert_eq!(params["type"], "object");
    assert!(params["properties"]["action"].is_object());

    finish_case(&harness, "computer_schema");
}

#[test]
fn test_computer_list_displays() {
    let harness = TestHarness::new("computer_list_displays");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);
        let output = tool
            .execute("call_1", json!({ "action": "list_displays" }), None)
            .await
            .expect("list_displays execute");

        let first_text = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("Found 2 display(s)"));
        assert!(first_text.contains("Retina Display"));

        let details = output.details.as_ref().expect("details present");
        assert!(details["displays"].is_array());
        assert_eq!(details["displays"].as_array().map(Vec::len), Some(2));
    });

    finish_case(&harness, "computer_list_displays");
}

#[test]
fn test_computer_list_windows() {
    let harness = TestHarness::new("computer_list_windows");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);
        let output = tool
            .execute("call_2", json!({ "action": "list_windows" }), None)
            .await
            .expect("list_windows execute");

        let first_text = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("Found 2 window(s)"));
        assert!(first_text.contains("Pi Agent Terminal"));

        let details = output.details.as_ref().expect("details present");
        assert!(details["windows"].is_array());
        assert_eq!(details["windows"].as_array().map(Vec::len), Some(2));
    });

    finish_case(&harness, "computer_list_windows");
}

#[test]
fn test_computer_screenshot_writes_valid_png() {
    let harness = TestHarness::new("computer_screenshot");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);
        let out_target = "screenshots/desktop.png";
        let output = tool
            .execute(
                "call_3",
                json!({
                    "action": "screenshot",
                    "display_id": 1,
                    "output_path": out_target
                }),
                None,
            )
            .await
            .expect("screenshot execute");

        let full_path = harness.temp_path(out_target);
        assert!(full_path.is_file(), "screenshot PNG must exist");

        let bytes = fs::read(&full_path).expect("read screenshot");
        assert!(
            bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "must be valid PNG"
        );

        let details = output.details.as_ref().expect("details present");
        assert_eq!(details["display_id"], 1);
    });

    finish_case(&harness, "computer_screenshot");
}

#[test]
fn test_computer_mouse_actions() {
    let harness = TestHarness::new("computer_mouse");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);

        // mouse_move
        let out_move = tool
            .execute(
                "call_m1",
                json!({ "action": "mouse_move", "x": 500, "y": 300 }),
                None,
            )
            .await
            .expect("mouse_move");
        assert_eq!(
            out_move.details.as_ref().map(|d| d["x"].as_i64()),
            Some(Some(500))
        );

        // mouse_click
        let out_click = tool
            .execute(
                "call_m2",
                json!({ "action": "mouse_click", "button": "left", "x": 500, "y": 300 }),
                None,
            )
            .await
            .expect("mouse_click");
        assert_eq!(
            out_click.details.as_ref().map(|d| d["button"].as_str()),
            Some(Some("left"))
        );

        // mouse_drag
        let out_drag = tool
            .execute(
                "call_m3",
                json!({ "action": "mouse_drag", "x": 700, "y": 400 }),
                None,
            )
            .await
            .expect("mouse_drag");
        assert_eq!(
            out_drag.details.as_ref().map(|d| d["x"].as_i64()),
            Some(Some(700))
        );
    });

    finish_case(&harness, "computer_mouse");
}

#[test]
fn test_computer_keyboard_actions() {
    let harness = TestHarness::new("computer_keyboard");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);

        // key_type
        let out_type = tool
            .execute(
                "call_k1",
                json!({ "action": "key_type", "text": "cargo check" }),
                None,
            )
            .await
            .expect("key_type");
        assert_eq!(
            out_type.details.as_ref().map(|d| d["char_count"].as_u64()),
            Some(Some(11))
        );

        // key_press
        let out_press = tool
            .execute(
                "call_k2",
                json!({ "action": "key_press", "key": "Return" }),
                None,
            )
            .await
            .expect("key_press");
        assert_eq!(
            out_press.details.as_ref().map(|d| d["key"].as_str()),
            Some(Some("Return"))
        );
    });

    finish_case(&harness, "computer_keyboard");
}

#[test]
fn test_computer_ax_tree_dump() {
    let harness = TestHarness::new("computer_ax_tree");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);
        let output = tool
            .execute(
                "call_ax",
                json!({ "action": "ax_tree", "window_id": 101 }),
                None,
            )
            .await
            .expect("ax_tree");

        let first_text = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("Accessibility tree for window 101"));
        assert!(first_text.contains("AXApplication"));
        assert!(first_text.contains("AXTextArea"));
    });

    finish_case(&harness, "computer_ax_tree");
}

#[test]
fn test_computer_clipboard_roundtrip() {
    let harness = TestHarness::new("computer_clipboard");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);

        // Write
        tool.execute(
            "call_cb1",
            json!({ "action": "clipboard_write", "text": "deterministic token payload" }),
            None,
        )
        .await
        .expect("clipboard_write");

        // Read
        let output = tool
            .execute("call_cb2", json!({ "action": "clipboard_read" }), None)
            .await
            .expect("clipboard_read");

        let first_text = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("deterministic token payload"));
    });

    finish_case(&harness, "computer_clipboard");
}

#[test]
fn test_computer_audit_logging() {
    let harness = TestHarness::new("computer_audit_log");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);

        tool.execute("call_a1", json!({ "action": "list_displays" }), None)
            .await
            .expect("exec 1");
        tool.execute(
            "call_a2",
            json!({ "action": "key_type", "text": "ls -la" }),
            None,
        )
        .await
        .expect("exec 2");

        let audit = tool.get_audit_log();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].action, "list_displays");
        assert_eq!(audit[1].action, "key_type");
        assert!(audit[0].timestamp_ms > 0);
    });

    finish_case(&harness, "computer_audit_log");
}

#[test]
fn test_computer_unknown_action_error() {
    let harness = TestHarness::new("computer_unknown_action");

    asupersync::test_utils::run_test(|| async {
        let tool = ComputerTool::new(harness.temp_dir()).with_mock(true);
        let res = tool
            .execute("call_err", json!({ "action": "invalid_op" }), None)
            .await;

        match res {
            Err(e) => assert!(e.to_string().contains("unknown action")),
            Ok(_) => panic!("expected error for unknown action"),
        }
    });

    finish_case(&harness, "computer_unknown_action");
}

#[test]
fn test_computer_default_gated_off() {
    let harness = TestHarness::new("computer_default_gated");
    let default_registry = ToolRegistry::new(&["read", "grep", "find"], harness.temp_dir(), None);

    assert!(default_registry.get("computer").is_none());

    finish_case(&harness, "computer_default_gated");
}

#[test]
fn test_computer_opt_in_activation() {
    let harness = TestHarness::new("computer_opt_in");
    let config = Config {
        computer: Some(ComputerSettings {
            enable_computer: Some(true),
            require_approval: Some(true),
            screenshot_dir: Some("screenshots".to_string()),
        }),
        ..Default::default()
    };

    let enabled_registry =
        ToolRegistry::new(&["read", "computer"], harness.temp_dir(), Some(&config));

    assert!(enabled_registry.get("computer").is_some());

    finish_case(&harness, "computer_opt_in");
}
