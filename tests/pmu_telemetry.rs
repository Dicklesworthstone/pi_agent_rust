#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::pmu_telemetry::{
    PMU_TELEMETRY_SCHEMA, PmuOpportunityRanker, PmuRegressionBudget, PmuSample,
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
fn test_pmu_sample_derived_metrics() {
    let harness = TestHarness::new("pmu_sample_derived_metrics");

    let sample = PmuSample {
        cycles: 100_000,
        instructions: 150_000,
        llc_references: 2_000,
        llc_misses: 200,
        branch_instructions: 25_000,
        branch_misses: 500,
        frontend_stall_cycles: 10_000,
        backend_stall_cycles: 15_000,
    };

    assert!((sample.ipc() - 1.5).abs() < 1e-6);
    assert!((sample.llc_miss_rate() - 0.10).abs() < 1e-6);
    assert!((sample.branch_miss_rate() - 0.02).abs() < 1e-6);
    assert!((sample.frontend_stall_ratio() - 0.10).abs() < 1e-6);
    assert!((sample.backend_stall_ratio() - 0.15).abs() < 1e-6);
    assert!((sample.total_stall_ratio() - 0.25).abs() < 1e-6);

    harness.log().info(
        "metrics",
        format!("schema={PMU_TELEMETRY_SCHEMA} ipc={:.2}", sample.ipc()),
    );

    finish_case(&harness, "pmu_sample_derived_metrics");
}

#[test]
fn test_pmu_budget_evaluation() {
    let harness = TestHarness::new("pmu_budget_evaluation");

    let budget = PmuRegressionBudget {
        max_llc_miss_rate: 0.15,
        max_branch_miss_rate: 0.04,
        max_stall_ratio: 0.30,
        min_ipc: 1.0,
    };

    // Healthy sample
    let healthy = PmuSample {
        cycles: 100_000,
        instructions: 120_000,
        llc_references: 1_000,
        llc_misses: 50,
        branch_instructions: 10_000,
        branch_misses: 200,
        frontend_stall_cycles: 5_000,
        backend_stall_cycles: 10_000,
    };
    let verdict = budget.evaluate(&healthy);
    assert!(verdict.passed);
    assert!(verdict.violations.is_empty());

    // Regressed sample: excessive LLC misses and stalls
    let regressed = PmuSample {
        cycles: 100_000,
        instructions: 60_000,
        llc_references: 1_000,
        llc_misses: 350, // 35% miss rate > 15% budget
        branch_instructions: 10_000,
        branch_misses: 600, // 6% miss rate > 4% budget
        frontend_stall_cycles: 20_000,
        backend_stall_cycles: 30_000, // 50% total stalls > 30% budget
    };
    let bad_verdict = budget.evaluate(&regressed);
    assert!(!bad_verdict.passed);
    assert_eq!(bad_verdict.violations.len(), 4); // LLC, branch, stall, IPC

    harness.log().info(
        "verdict",
        format!("violations_count={}", bad_verdict.violations.len()),
    );

    finish_case(&harness, "pmu_budget_evaluation");
}

#[test]
fn test_pmu_opportunity_ranker() {
    let harness = TestHarness::new("pmu_opportunity_ranker");

    let memory_heavy_sample = PmuSample {
        cycles: 200_000,
        instructions: 80_000,
        llc_references: 5_000,
        llc_misses: 2_000, // 40% LLC miss rate
        branch_instructions: 15_000,
        branch_misses: 300,
        frontend_stall_cycles: 10_000,
        backend_stall_cycles: 70_000,
    };

    let opportunity =
        PmuOpportunityRanker::score_candidate("hot_hostcall_dispatch", &memory_heavy_sample);
    assert_eq!(opportunity.name, "hot_hostcall_dispatch");
    assert_eq!(opportunity.bottleneck_category, "memory_bound_llc");
    assert!(opportunity.estimated_speedup > 1.20);
    assert!(opportunity.confidence >= 0.90);

    harness.log().info(
        "opportunity",
        format!(
            "candidate={} speedup={:.2} category={}",
            opportunity.name, opportunity.estimated_speedup, opportunity.bottleneck_category
        ),
    );

    finish_case(&harness, "pmu_opportunity_ranker");
}

#[test]
fn test_pmu_edge_cases_and_categories() {
    let harness = TestHarness::new("pmu_edge_cases_and_categories");

    // Zero-cycle sample shouldn't divide by zero or panic
    let zero_sample = PmuSample::default();
    assert_eq!(zero_sample.ipc(), 0.0);
    assert_eq!(zero_sample.llc_miss_rate(), 0.0);
    assert_eq!(zero_sample.branch_miss_rate(), 0.0);
    assert_eq!(zero_sample.frontend_stall_ratio(), 0.0);
    assert_eq!(zero_sample.backend_stall_ratio(), 0.0);
    assert_eq!(zero_sample.total_stall_ratio(), 0.0);

    let zero_opp = PmuOpportunityRanker::score_candidate("zero_sample", &zero_sample);
    assert_eq!(zero_opp.estimated_speedup, 1.0);
    assert_eq!(zero_opp.bottleneck_category, "compute_bound");
    assert_eq!(zero_opp.confidence, 0.50);

    // Branch-heavy candidate
    let branch_sample = PmuSample {
        cycles: 60_000,
        instructions: 40_000,
        llc_references: 100,
        llc_misses: 5,
        branch_instructions: 10_000,
        branch_misses: 1_200, // 12% branch mispredict rate > 8%
        frontend_stall_cycles: 5_000,
        backend_stall_cycles: 10_000,
    };
    let branch_opp = PmuOpportunityRanker::score_candidate("branch_heavy", &branch_sample);
    assert_eq!(branch_opp.bottleneck_category, "branch_mispredict_heavy");
    assert_eq!(branch_opp.confidence, 0.95);

    // Frontend starvation candidate
    let frontend_sample = PmuSample {
        cycles: 60_000,
        instructions: 30_000,
        llc_references: 100,
        llc_misses: 5,
        branch_instructions: 2_000,
        branch_misses: 20,
        frontend_stall_cycles: 20_000, // 33% frontend stall ratio > 25%
        backend_stall_cycles: 2_000,
    };
    let frontend_opp = PmuOpportunityRanker::score_candidate("frontend_heavy", &frontend_sample);
    assert_eq!(
        frontend_opp.bottleneck_category,
        "frontend_instruction_starvation"
    );

    finish_case(&harness, "pmu_edge_cases_and_categories");
}
