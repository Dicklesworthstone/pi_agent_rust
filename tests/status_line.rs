#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::status_line::{
    PowerlineStatusLine, SegmentId, SeparatorStyle, StatusContext, StatusLinePreset, StatusSegment,
    compute_session_accent_hue,
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
fn test_status_line_presets_rendering() {
    let harness = TestHarness::new("status_line_presets");

    let ctx = StatusContext {
        model: "claude-3-7-sonnet",
        thinking_level: Some("high"),
        mode: "agent",
        cwd: "pi_agent_rust",
        git_branch: Some("main"),
        git_dirty: false,
        context_pct: 35,
        cost_usd: 0.045,
        tokens_used: 4500,
        subagent_count: 1,
        session_name: "test-session",
        timestamp_str: "12:00:00",
    };

    let min = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
    let min_rendered = min.render(&ctx, 80);
    assert!(min_rendered.contains("claude-3-7-sonnet"));

    let full = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
    let full_rendered = full.render(&ctx, 160);
    assert!(full_rendered.contains("claude-3-7-sonnet"));
    assert!(full_rendered.contains("main"));
    assert!(full_rendered.contains("35%"));
    assert!(full_rendered.contains("$0.045"));

    finish_case(&harness, "status_line_presets");
}

#[test]
fn test_responsive_priority_dropping() {
    let harness = TestHarness::new("responsive_priority_dropping");

    let ctx = StatusContext {
        model: "gpt-4o",
        thinking_level: None,
        mode: "plan",
        cwd: "/Users/dev/projects/pi_agent_rust/src",
        git_branch: Some("feat/powerline"),
        git_dirty: true,
        context_pct: 75,
        cost_usd: 0.890,
        tokens_used: 65000,
        subagent_count: 3,
        session_name: "feature-session",
        timestamp_str: "15:30:00",
    };

    let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
    let wide = status_line.render(&ctx, 180);
    assert!(wide.contains("gpt-4o"));
    assert!(wide.contains("feat/powerline*"));

    let narrow = status_line.render(&ctx, 25);
    // At very narrow width, lowest priority segments are dropped to preserve high priority model/mode
    assert!(narrow.contains("gpt-4o") || narrow.contains("PLAN"));

    finish_case(&harness, "responsive_priority_dropping");
}

#[test]
fn test_session_accent_hue_calculation() {
    let harness = TestHarness::new("session_accent_hue");

    let h1 = compute_session_accent_hue("session-1");
    let h2 = compute_session_accent_hue("session-2");

    assert!(h1 < 360);
    assert!(h2 < 360);
    assert_ne!(h1, h2);

    finish_case(&harness, "session_accent_hue");
}
