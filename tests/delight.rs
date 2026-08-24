#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::delight::{
    compute_shimmer_intensity, format_terminal_title, render_sparkline, FireworksState, ShimmerMode,
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
fn test_shimmer_sweep_intensity_progression() {
    let harness = TestHarness::new("shimmer_sweep");

    let t0 = compute_shimmer_intensity(0, 0, ShimmerMode::Cosine);
    let t10 = compute_shimmer_intensity(0, 10, ShimmerMode::Cosine);
    assert!((0.0..=1.0).contains(&t0));
    assert!((0.0..=1.0).contains(&t10));

    let kitt0 = compute_shimmer_intensity(0, 0, ShimmerMode::Kitt);
    let kitt10 = compute_shimmer_intensity(0, 10, ShimmerMode::Kitt);
    assert!((0.0..=1.0).contains(&kitt0));
    assert!((0.0..=1.0).contains(&kitt10));

    finish_case(&harness, "shimmer_sweep");
}

#[test]
fn test_unicode_sparkline_series() {
    let harness = TestHarness::new("unicode_sparkline");

    let series = vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0];
    let sparkline = render_sparkline(&series);
    assert_eq!(sparkline.chars().count(), 8);
    assert!(sparkline.starts_with(' '));
    assert!(sparkline.ends_with('█'));

    finish_case(&harness, "unicode_sparkline");
}

#[test]
fn test_fireworks_particle_burst_and_decay() {
    let harness = TestHarness::new("fireworks_particle_decay");

    let mut state = FireworksState::default();
    assert!(!state.is_active);

    state.trigger_burst(50.0, 15.0, 24);
    assert!(state.is_active);
    assert_eq!(state.particles.len(), 24);

    for _ in 0..40 {
        state.tick();
    }

    assert!(!state.is_active);
    assert!(state.particles.is_empty());

    finish_case(&harness, "fireworks_particle_decay");
}

#[test]
fn test_terminal_title_escape() {
    let harness = TestHarness::new("terminal_title_escape");

    let title = format_terminal_title("Pi Session #1");
    assert_eq!(title, "\x1b]0;Pi Session #1\x07");

    finish_case(&harness, "terminal_title_escape");
}
