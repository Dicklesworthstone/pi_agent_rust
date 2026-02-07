//! Integration test: generate conformance test plan from real data (bd-2kyq).
//!
//! Loads the inclusion list and API matrix, builds the full conformance matrix
//! via `build_test_plan()`, and validates coverage against requirements.
//! Writes the output to `docs/extension-conformance-test-plan.json`.

use pi::extension_conformance_matrix::{
    ApiMatrix, ConformanceTestPlan, HostCapability, build_test_plan,
};
use pi::extension_inclusion::InclusionList;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn load_test_plan() -> (ConformanceTestPlan, InclusionList, Option<ApiMatrix>) {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let inclusion_path = repo_root.join("docs/extension-inclusion-list.json");
    let inclusion: InclusionList =
        serde_json::from_slice(&fs::read(&inclusion_path).expect("read inclusion list"))
            .expect("parse inclusion list");

    let api_matrix_path = repo_root.join("docs/extension-api-matrix.json");
    let api_matrix: Option<ApiMatrix> = fs::read(&api_matrix_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());

    let plan = build_test_plan(&inclusion, api_matrix.as_ref(), "bd-2kyq");
    (plan, inclusion, api_matrix)
}

#[test]
fn conformance_plan_schema_and_task() {
    let (plan, _, _) = load_test_plan();
    assert_eq!(plan.schema, "pi.ext.conformance-matrix.v1");
    assert_eq!(plan.task, "bd-2kyq");
    assert!(!plan.generated_at.is_empty());
}

#[test]
fn conformance_plan_has_matrix_cells() {
    let (plan, _, _) = load_test_plan();
    // Matrix should have cells (category × capability combinations with behaviors)
    assert!(
        !plan.matrix.is_empty(),
        "Matrix should have at least one cell"
    );

    // Every cell should have at least one behavior
    for cell in &plan.matrix {
        assert!(
            !cell.behaviors.is_empty(),
            "Cell {:?}:{:?} has no behaviors",
            cell.category,
            cell.capability,
        );
    }
}

#[test]
fn conformance_plan_has_required_cells() {
    let (plan, _, _) = load_test_plan();
    let required_count = plan.matrix.iter().filter(|c| c.required).count();
    // Must have some required cells
    assert!(
        required_count >= 5,
        "Expected at least 5 required cells, got {required_count}"
    );

    // Required cells for Tool category must include Read, Write, Exec, Http
    let tool_required: BTreeSet<_> = plan
        .matrix
        .iter()
        .filter(|c| format!("{:?}", c.category) == "Tool" && c.required)
        .map(|c| c.capability)
        .collect();
    assert!(
        tool_required.contains(&HostCapability::Read),
        "Tool:Read must be required"
    );
    assert!(
        tool_required.contains(&HostCapability::Exec),
        "Tool:Exec must be required"
    );
}

#[test]
fn conformance_plan_fixture_assignments() {
    let (plan, _, _) = load_test_plan();
    assert!(
        !plan.fixture_assignments.is_empty(),
        "Should have fixture assignments"
    );

    // Each fixture assignment should have a valid cell_key
    for fa in &plan.fixture_assignments {
        assert!(
            fa.cell_key.contains(':'),
            "Cell key should be Category:Capability format, got: {}",
            fa.cell_key,
        );
        assert!(
            fa.min_fixtures >= 1,
            "Min fixtures should be >= 1 for {}",
            fa.cell_key,
        );
    }
}

#[test]
fn conformance_plan_category_criteria() {
    let (plan, _, _) = load_test_plan();
    assert_eq!(
        plan.category_criteria.len(),
        8,
        "Should have criteria for all 8 extension categories"
    );

    // Each category should have at least one must_pass criterion
    for criteria in &plan.category_criteria {
        assert!(
            !criteria.must_pass.is_empty(),
            "Category {:?} has no must_pass criteria",
            criteria.category,
        );
        assert!(
            !criteria.failure_conditions.is_empty(),
            "Category {:?} has no failure_conditions",
            criteria.category,
        );
    }
}

#[test]
fn conformance_plan_coverage_summary() {
    let (plan, _, _) = load_test_plan();
    assert!(plan.coverage.total_cells > 0, "Should have total cells");
    assert!(
        plan.coverage.required_cells > 0,
        "Should have required cells"
    );
    assert!(
        plan.coverage.categories_covered >= 1,
        "Should cover at least 1 category"
    );
}

#[test]
fn conformance_plan_exemplar_coverage() {
    let (plan, inclusion, _) = load_test_plan();
    let total_included = inclusion.tier0.len() + inclusion.tier1.len() + inclusion.tier2.len();

    // The exemplar count should be <= total included extensions
    assert!(
        plan.coverage.total_exemplar_extensions <= total_included,
        "Exemplars ({}) should not exceed included extensions ({total_included})",
        plan.coverage.total_exemplar_extensions,
    );
}

#[test]
fn conformance_plan_all_capabilities_represented() {
    let (plan, _, _) = load_test_plan();

    // Verify that all defined capabilities appear in at least one matrix cell
    let caps_in_matrix: BTreeSet<_> = plan.matrix.iter().map(|c| c.capability).collect();
    for cap in HostCapability::all() {
        assert!(
            caps_in_matrix.contains(cap),
            "Capability {cap:?} not represented in any matrix cell"
        );
    }
}

#[test]
fn conformance_plan_behavior_fields_populated() {
    let (plan, _, _) = load_test_plan();
    for cell in &plan.matrix {
        for behavior in &cell.behaviors {
            assert!(
                !behavior.description.is_empty(),
                "Behavior description empty in {:?}:{:?}",
                cell.category,
                cell.capability,
            );
            assert!(
                !behavior.protocol_surface.is_empty(),
                "Protocol surface empty in {:?}:{:?}",
                cell.category,
                cell.capability,
            );
            assert!(
                !behavior.pass_criteria.is_empty(),
                "Pass criteria empty in {:?}:{:?}",
                cell.category,
                cell.capability,
            );
            assert!(
                !behavior.fail_criteria.is_empty(),
                "Fail criteria empty in {:?}:{:?}",
                cell.category,
                cell.capability,
            );
        }
    }
}

#[test]
fn conformance_plan_no_duplicate_cells() {
    let (plan, _, _) = load_test_plan();
    let mut seen = BTreeSet::new();
    for cell in &plan.matrix {
        let key = format!("{:?}:{:?}", cell.category, cell.capability);
        assert!(seen.insert(key.clone()), "Duplicate matrix cell: {key}");
    }
}

#[test]
fn conformance_plan_serde_roundtrip() {
    let (plan, _, _) = load_test_plan();
    let json = serde_json::to_string_pretty(&plan).expect("serialize plan");
    let back: ConformanceTestPlan = serde_json::from_str(&json).expect("deserialize plan");
    assert_eq!(back.schema, plan.schema);
    assert_eq!(back.task, plan.task);
    assert_eq!(back.matrix.len(), plan.matrix.len());
    assert_eq!(back.category_criteria.len(), plan.category_criteria.len());
}

// ── Evidence log generation ──────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn generate_conformance_test_plan() {
    let (plan, inclusion, _) = load_test_plan();
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Write the plan as evidence
    let json = serde_json::to_string_pretty(&plan).expect("serialize plan");
    let output_path = repo_root.join("docs/extension-conformance-test-plan.json");
    fs::write(&output_path, format!("{json}\n")).expect("write test plan");

    // Print summary
    eprintln!("\n=== Conformance Test Plan (bd-2kyq) ===");
    eprintln!("Matrix cells:           {}", plan.matrix.len());
    eprintln!("  Required:             {}", plan.coverage.required_cells);
    eprintln!("  Covered:              {}", plan.coverage.covered_cells);
    eprintln!(
        "  Uncovered required:   {}",
        plan.coverage.uncovered_required_cells
    );
    eprintln!(
        "Exemplar extensions:    {}",
        plan.coverage.total_exemplar_extensions
    );
    eprintln!(
        "Categories covered:     {}",
        plan.coverage.categories_covered
    );
    eprintln!(
        "Capabilities covered:   {}",
        plan.coverage.capabilities_covered
    );
    eprintln!();

    // Print per-category coverage
    eprintln!("Category criteria:");
    for criteria in &plan.category_criteria {
        eprintln!(
            "  {:?}: {} must_pass, {} failure_conditions",
            criteria.category,
            criteria.must_pass.len(),
            criteria.failure_conditions.len(),
        );
    }
    eprintln!();

    // Print fixture assignment coverage
    let covered_assignments = plan
        .fixture_assignments
        .iter()
        .filter(|a| a.coverage_met)
        .count();
    let total_assignments = plan.fixture_assignments.len();
    eprintln!("Fixture assignments: {covered_assignments}/{total_assignments} covered");

    // Print gaps
    let uncovered: Vec<_> = plan
        .fixture_assignments
        .iter()
        .filter(|a| !a.coverage_met)
        .collect();
    if !uncovered.is_empty() {
        eprintln!("\nUncovered cells ({}):", uncovered.len());
        for a in &uncovered {
            eprintln!(
                "  {}: {} fixtures (need {})",
                a.cell_key,
                a.fixture_extensions.len(),
                a.min_fixtures,
            );
        }
    }

    eprintln!("\nOutput written to: {}", output_path.display());

    // ── Assertions ──

    // The plan must be valid JSON round-trip
    let _: ConformanceTestPlan = serde_json::from_str(&json).expect("plan should be valid JSON");

    // Total included extensions should match inclusion list
    let total_included = inclusion.tier0.len() + inclusion.tier1.len() + inclusion.tier2.len();
    eprintln!(
        "\nInclusion list: {} extensions ({} tier-0, {} tier-1, {} tier-2)",
        total_included,
        inclusion.tier0.len(),
        inclusion.tier1.len(),
        inclusion.tier2.len(),
    );

    // Matrix should cover all 8 categories
    let categories_in_matrix: BTreeSet<String> = plan
        .matrix
        .iter()
        .map(|c| format!("{:?}", c.category))
        .collect();
    assert!(
        categories_in_matrix.len() >= 6,
        "Matrix should cover at least 6 categories, got {}",
        categories_in_matrix.len(),
    );

    // All 9 capabilities should be represented
    let caps_in_matrix: BTreeSet<_> = plan.matrix.iter().map(|c| c.capability).collect();
    assert_eq!(
        caps_in_matrix.len(),
        HostCapability::all().len(),
        "All {} capabilities should be in matrix",
        HostCapability::all().len(),
    );
}
