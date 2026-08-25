#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::hostcall_egraph::{
    HostcallEGraphEngine, RULE_DROP_ROUNDTRIP_CONVERT, RULE_FUSE_MARSHAL_VALIDATE,
    RULE_FUSE_TYPED_PIPELINE, SaturationLimits, SaturationOutcome, canonical_plan,
    typed_plan_with_roundtrip,
};
use pi::hostcall_rewrite::HostcallRewritePlanKind;

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
fn test_egraph_roundtrip_elimination_and_fusion() {
    let harness = TestHarness::new("egraph_roundtrip_elimination");

    let engine = HostcallEGraphEngine::default();
    let baseline = typed_plan_with_roundtrip("tool.read");
    let decision = engine.optimize(&baseline);

    assert!(
        decision.rewrote(),
        "expected rewrite optimization to succeed"
    );
    assert!(
        decision.expected_cost_delta > 0,
        "expected positive cost delta"
    );
    assert!(
        decision
            .applied_rules
            .contains(&RULE_DROP_ROUNDTRIP_CONVERT)
    );
    assert!(decision.applied_rules.contains(&RULE_FUSE_MARSHAL_VALIDATE));
    assert!(decision.applied_rules.contains(&RULE_FUSE_TYPED_PIPELINE));
    assert_eq!(decision.outcome, SaturationOutcome::Fixpoint);

    let rewrite_plan = decision.to_rewrite_plan(
        HostcallRewritePlanKind::FusedIntrinsic,
        RULE_FUSE_TYPED_PIPELINE,
    );
    assert_eq!(rewrite_plan.estimated_cost, decision.selected_cost);

    let json_telemetry = decision.to_json();
    assert_eq!(json_telemetry["rewrote"], true);

    harness.log().info(
        "decision",
        format!(
            "baseline_cost={} selected_cost={} delta={}",
            decision.baseline_cost, decision.selected_cost, decision.expected_cost_delta
        ),
    );

    finish_case(&harness, "egraph_roundtrip_elimination");
}

#[test]
fn test_egraph_budget_exhaustion_fallback() {
    let harness = TestHarness::new("egraph_budget_fallback");

    // Engine with tiny node budget forces fallback
    let engine = HostcallEGraphEngine::default().with_limits(SaturationLimits {
        max_nodes: 3,
        ..SaturationLimits::default()
    });

    let baseline = canonical_plan("tool.write");
    let decision = engine.optimize(&baseline);

    assert!(!decision.rewrote(), "expected budget exhaustion fallback");
    assert_eq!(decision.selected_cost, decision.baseline_cost);
    assert_eq!(decision.fallback_reason, Some("node_budget_exhausted"));

    harness
        .log()
        .info("fallback", format!("reason={:?}", decision.fallback_reason));

    finish_case(&harness, "egraph_budget_fallback");
}

#[test]
fn test_egraph_disabled_kill_switch() {
    let harness = TestHarness::new("egraph_disabled_kill_switch");

    let engine = HostcallEGraphEngine::from_opt(Some("disabled"));
    assert!(!engine.enabled());

    let baseline = canonical_plan("tool.write");
    let decision = engine.optimize(&baseline);

    assert!(!decision.rewrote());
    assert_eq!(decision.fallback_reason, Some("egraph_disabled"));

    finish_case(&harness, "egraph_disabled_kill_switch");
}
