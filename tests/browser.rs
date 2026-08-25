#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::browser::{BrowserSettings, BrowserTool};
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
fn test_browser_schema_and_metadata() {
    let harness = TestHarness::new("browser_schema");
    let tool = BrowserTool::new(harness.temp_dir());

    assert_eq!(tool.name(), "browser");
    assert_eq!(tool.label(), "Browser");
    assert!(!tool.description().is_empty());

    let params = tool.parameters();
    assert_eq!(params["type"], "object");
    assert!(params["properties"]["action"].is_object());
    assert!(params["properties"]["url"].is_object());

    finish_case(&harness, "browser_schema");
}

#[test]
fn test_browser_open_and_goto_navigation() {
    let harness = TestHarness::new("browser_navigation");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);

        let output = tool
            .execute(
                "call_open",
                json!({
                    "action": "open",
                    "tab": "main",
                    "url": "https://example.com"
                }),
                None,
            )
            .await
            .expect("open execute");

        let first_text = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("Navigated tab main to https://example.com"));

        let details = output.details.as_ref().expect("details present");
        assert_eq!(details["tab"], "main");
        assert_eq!(details["url"], "https://example.com");
    });

    finish_case(&harness, "browser_navigation");
}

#[test]
fn test_browser_tab_lifecycle_and_switching() {
    let harness = TestHarness::new("browser_tabs");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);

        // Open two distinct tabs
        tool.execute(
            "call_t1",
            json!({ "action": "open", "tab": "tab1", "url": "https://example.com" }),
            None,
        )
        .await
        .expect("open tab1");

        tool.execute(
            "call_t2",
            json!({ "action": "open", "tab": "tab2", "url": "https://github.com" }),
            None,
        )
        .await
        .expect("open tab2");

        // List tabs
        let out_list = tool
            .execute("call_list", json!({ "action": "list_tabs" }), None)
            .await
            .expect("list_tabs");

        let details = out_list.details.as_ref().expect("details present");
        let tabs = details["tabs"].as_array().expect("tabs array");
        assert!(tabs.len() >= 2);

        // Close tab1
        let out_close = tool
            .execute(
                "call_close",
                json!({ "action": "close", "tab": "tab1" }),
                None,
            )
            .await
            .expect("close tab1");

        let close_details = out_close.details.as_ref().expect("details present");
        assert_eq!(close_details["closed_tab"], "tab1");
    });

    finish_case(&harness, "browser_tabs");
}

#[test]
fn test_browser_close_nonexistent_tab_error() {
    let harness = TestHarness::new("browser_close_missing");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);
        let res = tool
            .execute(
                "call_err",
                json!({ "action": "close", "tab": "ghost_tab" }),
                None,
            )
            .await;

        match res {
            Err(e) => assert!(e.to_string().contains("cannot close nonexistent tab")),
            Ok(_) => panic!("expected error for nonexistent tab"),
        }
    });

    finish_case(&harness, "browser_close_missing");
}

#[test]
fn test_browser_evaluate_javascript() {
    let harness = TestHarness::new("browser_eval");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);

        let out = tool
            .execute(
                "call_eval",
                json!({
                    "action": "evaluate",
                    "script": "1 + 1"
                }),
                None,
            )
            .await
            .expect("evaluate");

        let details = out.details.as_ref().expect("details present");
        assert_eq!(details["result"], 2);
    });

    finish_case(&harness, "browser_eval");
}

#[test]
fn test_browser_snapshot_and_a11y_tree() {
    let harness = TestHarness::new("browser_snapshot");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);

        let out = tool
            .execute("call_snap", json!({ "action": "snapshot" }), None)
            .await
            .expect("snapshot");

        let first_text = match out.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("Page Snapshot"));
        assert!(first_text.contains("@e1"));
        assert!(first_text.contains("@e2"));

        let details = out.details.as_ref().expect("details present");
        assert!(details["snapshot"]["elements"].is_array());
    });

    finish_case(&harness, "browser_snapshot");
}

#[test]
fn test_browser_input_actions() {
    let harness = TestHarness::new("browser_inputs");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);

        // click
        let out_click = tool
            .execute(
                "call_c",
                json!({ "action": "click", "selector": "@e3" }),
                None,
            )
            .await
            .expect("click");
        assert_eq!(
            out_click.details.as_ref().map(|d| d["action"].as_str()),
            Some(Some("click"))
        );

        // fill
        let out_fill = tool
            .execute(
                "call_f",
                json!({ "action": "fill", "selector": "@e2", "text": "rust search" }),
                None,
            )
            .await
            .expect("fill");
        assert_eq!(
            out_fill.details.as_ref().map(|d| d["char_count"].as_u64()),
            Some(Some(11))
        );

        // press
        let out_press = tool
            .execute("call_p", json!({ "action": "press", "key": "Enter" }), None)
            .await
            .expect("press");
        assert_eq!(
            out_press.details.as_ref().map(|d| d["key"].as_str()),
            Some(Some("Enter"))
        );

        // scroll
        let out_scroll = tool
            .execute("call_s", json!({ "action": "scroll" }), None)
            .await
            .expect("scroll");
        assert_eq!(
            out_scroll.details.as_ref().map(|d| d["action"].as_str()),
            Some(Some("scroll"))
        );

        // wait_for
        let out_wait = tool
            .execute(
                "call_w",
                json!({ "action": "wait_for", "selector": "div.results", "timeout_ms": 2000 }),
                None,
            )
            .await
            .expect("wait_for");
        assert_eq!(
            out_wait.details.as_ref().map(|d| d["found"].as_bool()),
            Some(Some(true))
        );
    });

    finish_case(&harness, "browser_inputs");
}

#[test]
fn test_browser_screenshot_writes_valid_png() {
    let harness = TestHarness::new("browser_screenshot");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir()).with_mock(true);
        let out_target = "screenshots/page.png";
        let output = tool
            .execute(
                "call_ss",
                json!({
                    "action": "screenshot",
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
        assert_eq!(details["tab"], "default");
    });

    finish_case(&harness, "browser_screenshot");
}

#[test]
fn test_browser_domain_allowlist_enforcement() {
    let harness = TestHarness::new("browser_domain_allowlist");

    asupersync::test_utils::run_test(|| async {
        let tool = BrowserTool::new(harness.temp_dir())
            .with_mock(true)
            .with_domain_allowlist(Some(vec![
                "example.com".to_string(),
                "rust-lang.org".to_string(),
            ]));

        // Allowed navigation
        let allowed = tool
            .execute(
                "call_ok",
                json!({ "action": "open", "url": "https://example.com/docs" }),
                None,
            )
            .await;
        assert!(allowed.is_ok(), "allowed domain must succeed");

        // Blocked navigation
        let blocked = tool
            .execute(
                "call_block",
                json!({ "action": "open", "url": "https://malicious-domain.net" }),
                None,
            )
            .await;
        match blocked {
            Err(e) => assert!(e.to_string().contains("blocked by domain allowlist")),
            Ok(_) => panic!("expected domain block error"),
        }
    });

    finish_case(&harness, "browser_domain_allowlist");
}

#[test]
fn test_browser_default_gated_off() {
    let harness = TestHarness::new("browser_default_gated");
    let default_registry = ToolRegistry::new(&["read", "grep", "find"], harness.temp_dir(), None);

    assert!(default_registry.get("browser").is_none());

    finish_case(&harness, "browser_default_gated");
}

#[test]
fn test_browser_opt_in_activation() {
    let harness = TestHarness::new("browser_opt_in");
    let config = Config {
        browser: Some(BrowserSettings {
            enable_browser: Some(true),
            executable_path: None,
            remote_debugging_port: Some(9222),
            headless: Some(true),
            user_agent: None,
            domain_allowlist: Some(vec!["*".to_string()]),
        }),
        ..Default::default()
    };

    let enabled_registry =
        ToolRegistry::new(&["read", "browser"], harness.temp_dir(), Some(&config));

    assert!(enabled_registry.get("browser").is_some());

    finish_case(&harness, "browser_opt_in");
}
