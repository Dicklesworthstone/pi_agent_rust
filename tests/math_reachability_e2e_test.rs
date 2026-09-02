//! Math reachability E2E test (bd-math-reachability-evidence).
//!
//! Verifies that every math technique in the README's "Math at a Glance"
//! table (L1294-1302) has at least one *proven* code path that exercises it.
//!
//! Approach: every technique MUST have at least one passing cargo test
//! in `src/extensions/tests/reactor.rs` or `src/extension_scoring.rs` that
//! drives the technique past its trigger condition. This test scans
//! those source files for the technique-specific test function names
//! and verifies each one is present (i.e. the technique has a real test).
//!
//! This is the "static reachability" half of the evidence. The "dynamic
//! reachability" half — proving the technique actually fires on production
//! telemetry — is a separate run: it requires a live DSR perf run
//! (bd-ri-phase1-full-refresh) and is checked by
//! `scripts/perf/scan_math_technique_traces.py` against a captured
//! `tracing.jsonl` from that run.
//!
//! Run:
//! ```bash
//! cargo test --test math_reachability_e2e -- --nocapture
//! ```
//!
//! Bead: bd-math-reachability-evidence

#![allow(clippy::expect_used)]

use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// One technique from the README's "Math at a Glance" table.
struct Technique {
    name: &'static str,
    /// Substring that should appear in at least one test function
    /// name in `src/extensions/tests/reactor.rs` or
    /// `src/extension_scoring.rs`.
    test_marker: &'static str,
    /// Source file most likely to contain the test.
    source_file: &'static str,
    /// `true` iff the technique has a unit test that proves it can fire.
    has_unit_test: bool,
}

fn techniques() -> Vec<Technique> {
    vec![
        Technique {
            name: "CUSUM",
            test_marker: "cusum",
            source_file: "src/extensions/tests/reactor.rs",
            has_unit_test: true,
        },
        Technique {
            name: "BOCPD",
            test_marker: "bocpd",
            source_file: "src/extensions/tests/reactor.rs",
            has_unit_test: true,
        },
        Technique {
            name: "conformal",
            test_marker: "conformal",
            source_file: "src/extensions/tests/reactor.rs",
            has_unit_test: true,
        },
        Technique {
            name: "pac_bayes",
            test_marker: "pac_bayes",
            source_file: "src/extensions/tests/reactor.rs",
            has_unit_test: true,
        },
        Technique {
            name: "VOI",
            test_marker: "voi",
            source_file: "src/extension_scoring.rs",
            has_unit_test: true,
        },
        Technique {
            name: "OCO",
            test_marker: "regret",
            source_file: "src/extensions.rs",
            has_unit_test: true,
        },
        // Weighted bottleneck attribution lives in the perf orchestrator
        // (scripts/perf/orchestrate.sh) and is validated by the phase-1 matrix
        // validator tests, not by the extension runtime.
        Technique {
            name: "weighted_attribution",
            test_marker: "phase1_matrix_validator_rejects_weighted",
            source_file: "tests/bench_schema.rs",
            has_unit_test: true,
        },
    ]
}

#[test]
fn each_math_technique_has_a_reachable_test() {
    let root = project_root();
    let mut missing: Vec<String> = Vec::new();
    let mut summary: Vec<(String, bool)> = Vec::new();
    for t in techniques() {
        let path = root.join(t.source_file);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
        // The technique is "reachable" if (a) it has a unit test, AND
        // (b) the unit test is present in the expected source file.
        let reachable = t.has_unit_test && body.contains(&format!("fn {}_", t.test_marker))
            || body.contains(&format!("fn {}", t.test_marker));
        summary.push((t.name.to_string(), reachable));
        if !reachable {
            missing.push(t.name.to_string());
        }
    }
    for (name, ok) in &summary {
        eprintln!("  [{}] {}", if *ok { "OK" } else { "MISSING" }, name);
    }
    assert!(
        missing.is_empty(),
        "math techniques without a reachable test: {missing:?}"
    );
}

#[test]
fn reactor_test_covers_cusum_bocpd_conformal_pac_bayes() {
    let path = project_root().join("src/extensions/tests/reactor.rs");
    let body = std::fs::read_to_string(&path).expect("reactor.rs");
    for required in [
        "cusum_detects_rate_increase",
        "cusum_no_alarm_on_stable_signal",
        "bocpd_detects_changepoint_on_mean_shift",
        "bocpd_posterior_spikes_at_synthetic_change_point",
        "conformal_state_observe_marks_anomaly_when_out_of_interval",
        "conformal_interval_empirical_coverage_matches_confidence",
        "pac_bayes_bound_increases_with_errors",
        "pac_bayes_bound_is_worst_case_with_no_data",
    ] {
        assert!(
            body.contains(required),
            "reactor.rs missing required math test: {required}"
        );
    }
}

#[test]
fn extension_scoring_covers_voi() {
    let path = project_root().join("src/extension_scoring.rs");
    let body = std::fs::read_to_string(&path).expect("extension_scoring.rs");
    for required in [
        "fn voi_selection_state_dominates",
        "fn insert_voi_selection_state",
        "VoiSelectionState",
    ] {
        assert!(
            body.contains(required),
            "extension_scoring.rs missing required VOI test/code: {required}"
        );
    }
}

#[test]
fn oco_state_present_in_extensions() {
    let path = project_root().join("src/extensions.rs");
    let body = std::fs::read_to_string(&path).expect("extensions.rs");
    // OCO is the "Online convex optimization" + regret rollback layer.
    // Verify the state struct + a test function exist.
    assert!(
        body.contains("online convex optimization")
            || body.contains("OnlineConvexOpt")
            || body.contains("OcoState"),
        "extensions.rs missing OCO state struct"
    );
    // Check for at least one OCO-related test
    let has_oco_test =
        body.contains("#[test]") && (body.contains("fn oco_") || body.contains("fn regret_"));
    assert!(has_oco_test, "extensions.rs missing OCO/regret test");
}
