#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;

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
fn test_chrome_modules_tui_integration() {
    let harness = TestHarness::new("chrome_tui_integration");

    // 1. StatusLine integration
    let status_ctx = pi::status_line::StatusContext {
        model: "claude-3-5-sonnet",
        thinking_level: Some("high"),
        mode: "act",
        cwd: "/test/project",
        git_branch: Some("main"),
        git_dirty: false,
        context_pct: 12,
        cost_usd: 0.042,
        tokens_used: 1250,
        subagent_count: 0,
        session_name: "test-session",
        timestamp_str: "12:00:00",
    };
    let status_line = pi::status_line::PowerlineStatusLine::with_preset(
        pi::status_line::StatusLinePreset::Default,
    );
    let rendered_status = status_line.render(&status_ctx, 120);
    assert!(rendered_status.contains("claude-3-5-sonnet"));
    assert!(rendered_status.contains("main"));
    harness
        .log()
        .info("status_line", "powerline rendered successfully");

    // 2. OverlaySystem WelcomeScreen integration
    let welcome = pi::overlay_system::WelcomeScreen::default();
    assert!(welcome.greeting.contains("Welcome to Pi"));
    assert!(!welcome.current_tip().is_empty());
    harness
        .log()
        .info("overlay_system", "welcome screen rendered");

    // 3. MarkdownRich enhancement integration
    let raw_md = "Use colour #ff5500 for alpha = \\alpha and beta = \\beta.";
    let latex_converted = pi::markdown_rich::latex_to_unicode(raw_md);
    assert!(latex_converted.contains('α'));
    assert!(latex_converted.contains('β'));
    let hex_swatched = pi::markdown_rich::render_hex_swatches(&latex_converted);
    assert!(hex_swatched.contains("■ #ff5500"));
    harness
        .log()
        .info("markdown_rich", "latex and hex swatches rendered");

    // 4. Delight terminal title & sparkline integration
    let title = pi::delight::format_terminal_title("Pi · gpt-4o · processing");
    assert!(title.contains("gpt-4o"));
    assert!(title.contains("processing"));
    let sparkline = pi::delight::render_sparkline(&[1.0, 5.0, 3.0, 8.0, 2.0]);
    assert_eq!(sparkline.chars().count(), 5);
    harness
        .log()
        .info("delight", "title and sparkline rendered");

    // 5. Gallery matrix verification
    let matrix = pi::gallery::GalleryMatrix::new();
    let json_report = matrix.render_report_json();
    assert!(json_report.contains("pi.gallery.matrix.v1"));
    assert!(!matrix.items.is_empty());
    harness
        .log()
        .info("gallery", "gallery matrix report generated");

    finish_case(&harness, "chrome_tui_integration");
}
