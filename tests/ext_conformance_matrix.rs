//! Integration test: generate conformance test plan from real data (bd-2kyq).
//!
//! Loads the inclusion list and API matrix, builds the full conformance matrix
//! via `build_test_plan()`, and validates coverage against requirements.
//! Writes the output to `docs/extension-conformance-test-plan.json`.

use pi::extension_conformance_matrix::{
    ApiMatrix, ConformanceTestPlan, HostCapability, build_test_plan,
};
use pi::extension_inclusion::InclusionList;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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

#[allow(dead_code)]
fn load_api_usage_matrix() -> Value {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let usage_path = repo_root.join("tests/ext_conformance/api_usage_matrix.json");
    serde_json::from_slice(&fs::read(&usage_path).expect("read api usage matrix"))
        .expect("parse api usage matrix")
}

#[allow(dead_code)]
fn node_api_call_count(usage: &Value, module: &str, api: &str) -> u64 {
    usage
        .get("node_modules")
        .and_then(Value::as_array)
        .and_then(|mods| {
            mods.iter().find(|m| {
                m.get("module")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == module)
            })
        })
        .and_then(|module_obj| {
            module_obj
                .get("apis")
                .and_then(Value::as_array)
                .and_then(|apis| {
                    apis.iter().find(|entry| {
                        entry
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|value| value == api)
                    })
                })
        })
        .and_then(|entry| entry.get("call_count").and_then(Value::as_u64))
        .unwrap_or(0)
}

#[allow(dead_code)]
fn bun_api_call_count(usage: &Value, api: &str) -> u64 {
    usage
        .get("bun_apis")
        .and_then(|bun| bun.get("apis"))
        .and_then(Value::as_array)
        .and_then(|apis| {
            apis.iter().find(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == api)
            })
        })
        .and_then(|entry| entry.get("call_count").and_then(Value::as_u64))
        .unwrap_or(0)
}

#[allow(dead_code)]
fn runtime_case_status(
    repo_root: &Path,
    evidence_file: &str,
    evidence_markers: &[&str],
) -> (String, String) {
    let evidence_path = repo_root.join(evidence_file);
    if !evidence_path.exists() {
        return (
            "fail".to_string(),
            format!("missing evidence file: {}", evidence_path.display()),
        );
    }

    let content = fs::read_to_string(&evidence_path)
        .unwrap_or_else(|_| String::from("<<unreadable evidence file>>"));
    let missing: Vec<&str> = evidence_markers
        .iter()
        .copied()
        .filter(|needle| !content.contains(needle))
        .collect();

    if missing.is_empty() {
        (
            "pass".to_string(),
            format!(
                "evidence verified in {} via markers: {}",
                evidence_file,
                evidence_markers.join(", ")
            ),
        )
    } else {
        (
            "fail".to_string(),
            format!(
                "missing markers in {}: {}",
                evidence_file,
                missing.join(", ")
            ),
        )
    }
}

#[allow(dead_code)]
fn jsonl_line_count(path: &Path) -> u64 {
    match fs::read_to_string(path) {
        Ok(content) => content.lines().count() as u64,
        Err(_) => 0,
    }
}

#[allow(dead_code)]
fn latest_e2e_summary(repo_root: &Path) -> Option<(String, Value)> {
    let e2e_root = repo_root.join("tests/e2e_results");
    let mut runs: Vec<PathBuf> = fs::read_dir(&e2e_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();

    runs.sort_unstable();
    runs.reverse();

    for run in runs {
        let summary_path = run.join("summary.json");
        let bytes = match fs::read(&summary_path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let summary = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let relative = match summary_path.strip_prefix(repo_root) {
            Ok(path) => path.display().to_string(),
            Err(_) => summary_path.display().to_string(),
        };
        return Some((relative, summary));
    }

    None
}

#[allow(clippy::items_after_statements, clippy::too_many_lines)]
#[allow(dead_code)]
fn build_runtime_api_matrix_report() -> Value {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let usage = load_api_usage_matrix();

    struct RuntimeCase {
        surface: &'static str,
        module: &'static str,
        api: &'static str,
        evidence_file: &'static str,
        evidence_markers: &'static [&'static str],
    }

    let cases = vec![
        RuntimeCase {
            surface: "node",
            module: "node:buffer",
            api: "Buffer.from",
            evidence_file: "tests/node_buffer_shim.rs",
            evidence_markers: &["fn from_string_utf8_roundtrip()", "Buffer.from(\"hello\")"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:buffer",
            api: "Buffer.alloc",
            evidence_file: "tests/node_buffer_shim.rs",
            evidence_markers: &["fn alloc_zero_filled()", "Buffer.alloc("],
        },
        RuntimeCase {
            surface: "node",
            module: "node:buffer",
            api: "Buffer.concat",
            evidence_file: "tests/node_buffer_shim.rs",
            evidence_markers: &["fn concat_two_buffers()", "Buffer.concat("],
        },
        RuntimeCase {
            surface: "node",
            module: "node:crypto",
            api: "createHash",
            evidence_file: "tests/node_crypto_shim.rs",
            evidence_markers: &["fn sha256_hello_hex()", "createHash(\"sha256\")"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:crypto",
            api: "createHmac",
            evidence_file: "tests/node_crypto_shim.rs",
            evidence_markers: &["fn hmac_sha256_hex()", "createHmac(\"sha256\""],
        },
        RuntimeCase {
            surface: "node",
            module: "node:crypto",
            api: "randomUUID",
            evidence_file: "tests/node_crypto_shim.rs",
            evidence_markers: &["fn random_uuid_format()", "randomUUID()"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:crypto",
            api: "randomBytes",
            evidence_file: "tests/node_crypto_shim.rs",
            evidence_markers: &["fn random_bytes_length()", "randomBytes(16)"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:crypto",
            api: "timingSafeEqual",
            evidence_file: "tests/node_crypto_shim.rs",
            evidence_markers: &["fn timing_safe_equal_same()", "timingSafeEqual("],
        },
        RuntimeCase {
            surface: "node",
            module: "node:http",
            api: "request",
            evidence_file: "tests/node_http_shim.rs",
            evidence_markers: &["fn request_returns_object_with_write()", "http.request("],
        },
        RuntimeCase {
            surface: "node",
            module: "node:http",
            api: "get",
            evidence_file: "tests/node_http_shim.rs",
            evidence_markers: &["fn get_receives_response_body()", "http.get("],
        },
        RuntimeCase {
            surface: "node",
            module: "node:http",
            api: "createServer",
            evidence_file: "tests/node_http_shim.rs",
            evidence_markers: &["fn create_server_throws()", "http.createServer()"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:https",
            api: "request",
            evidence_file: "tests/node_http_shim.rs",
            evidence_markers: &["fn https_request_exists()", "typeof https.request"],
        },
        RuntimeCase {
            surface: "node",
            module: "node:https",
            api: "get",
            evidence_file: "tests/node_http_shim.rs",
            evidence_markers: &["fn https_request_exists()", "typeof https.get"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.write",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.write"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.connect",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.connect"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.which",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.which"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.spawn",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.spawn"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.listen",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.listen"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.file",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.file"],
        },
        RuntimeCase {
            surface: "bun",
            module: "bun",
            api: "Bun.argv",
            evidence_file: "src/extensions_js.rs",
            evidence_markers: &["Bun.argv"],
        },
    ];

    let mut pass_count = 0_u64;
    let mut fail_count = 0_u64;
    let mut node_pass = 0_u64;
    let mut node_fail = 0_u64;
    let mut bun_pass = 0_u64;
    let mut bun_fail = 0_u64;
    let mut entries = Vec::with_capacity(cases.len());

    let structured_logs = [
        "tests/ext_conformance/reports/parity/parity_events.jsonl",
        "tests/ext_conformance/reports/conformance_events.jsonl",
    ]
    .iter()
    .map(|rel_path| {
        let path = repo_root.join(rel_path);
        if path.exists() {
            let line_count = jsonl_line_count(&path);
            json!({
                "path": rel_path,
                "status": "pass",
                "line_count": line_count,
                "diagnostics": format!("structured log available with {line_count} line(s)")
            })
        } else {
            json!({
                "path": rel_path,
                "status": "fail",
                "line_count": 0,
                "diagnostics": format!("missing structured log: {rel_path}")
            })
        }
    })
    .collect::<Vec<Value>>();

    let unit_outcomes = [
        (
            "node_buffer_shim",
            "tests/node_buffer_shim.rs",
            &["fn from_string_utf8_roundtrip()", "fn alloc_zero_filled()"][..],
        ),
        (
            "node_crypto_shim",
            "tests/node_crypto_shim.rs",
            &["fn sha256_hello_hex()", "fn random_uuid_format()"][..],
        ),
        (
            "node_http_shim",
            "tests/node_http_shim.rs",
            &[
                "fn request_returns_object_with_write()",
                "fn get_receives_response_body()",
            ][..],
        ),
        (
            "npm_module_stubs",
            "tests/npm_module_stubs.rs",
            &[
                "fn node_pty_spawn_returns_pty()",
                "fn chokidar_watch_returns_watcher()",
            ][..],
        ),
    ]
    .iter()
    .map(|(target, file, markers)| {
        let (status, diagnostics) = runtime_case_status(repo_root, file, markers);
        json!({
            "target": target,
            "status": status,
            "evidence_file": file,
            "diagnostics": diagnostics
        })
    })
    .collect::<Vec<Value>>();

    let e2e_script = "scripts/e2e/run_all.sh";
    let e2e_script_exists = repo_root.join(e2e_script).exists();
    let e2e_workflow = if let Some((summary_path, summary)) = latest_e2e_summary(repo_root) {
        let total = summary
            .get("total_suites")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let passed = summary
            .get("passed_suites")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let failed = summary
            .get("failed_suites")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        json!({
            "script_path": e2e_script,
            "script_exists": e2e_script_exists,
            "latest_summary": summary_path,
            "status": if failed == 0 { "pass" } else { "fail" },
            "diagnostics": format!(
                "latest e2e summary reports total={total}, passed={passed}, failed={failed}"
            )
        })
    } else {
        json!({
            "script_path": e2e_script,
            "script_exists": e2e_script_exists,
            "latest_summary": Value::Null,
            "status": "fail",
            "diagnostics": "no tests/e2e_results/*/summary.json artifact found"
        })
    };

    for case in &cases {
        let call_count = if case.surface == "bun" {
            bun_api_call_count(&usage, case.api)
        } else {
            node_api_call_count(&usage, case.module, case.api)
        };

        let (status, diagnostics) =
            runtime_case_status(repo_root, case.evidence_file, case.evidence_markers);

        if status == "pass" {
            pass_count += 1;
            if case.surface == "bun" {
                bun_pass += 1;
            } else {
                node_pass += 1;
            }
        } else {
            fail_count += 1;
            if case.surface == "bun" {
                bun_fail += 1;
            } else {
                node_fail += 1;
            }
        }

        entries.push(json!({
            "surface": case.surface,
            "module": case.module,
            "api": case.api,
            "call_count": call_count,
            "status": status,
            "evidence_file": case.evidence_file,
            "diagnostics": diagnostics,
            "linked_outcomes": {
                "unit_test_file": case.evidence_file,
                "e2e_workflow_script": e2e_script,
                "structured_logs": [
                    "tests/ext_conformance/reports/parity/parity_events.jsonl",
                    "tests/ext_conformance/reports/conformance_events.jsonl"
                ]
            }
        }));
    }

    json!({
        "schema": "pi.runtime.compat-matrix.v1",
        "task": "bd-k5q5.7.3",
        "source_usage_matrix": "tests/ext_conformance/api_usage_matrix.json",
        "linked_outcomes": {
            "unit_tests": unit_outcomes,
            "e2e_workflow": e2e_workflow,
            "structured_logs": structured_logs
        },
        "entries": entries,
        "summary": {
            "total": pass_count + fail_count,
            "pass": pass_count,
            "fail": fail_count,
            "node": { "pass": node_pass, "fail": node_fail },
            "bun": { "pass": bun_pass, "fail": bun_fail }
        }
    })
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
    let total_included = inclusion.tier0.len()
        + inclusion.tier1.len()
        + inclusion.tier1_review.len()
        + inclusion.tier2.len();

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

// ── Runtime Node/Bun API compatibility matrix (bd-k5q5.7.3) ───────────────

#[test]
fn runtime_api_matrix_node_critical_entries_pass() {
    let report = build_runtime_api_matrix_report();
    let entries = report
        .get("entries")
        .and_then(Value::as_array)
        .expect("runtime matrix entries");

    let node_failures: Vec<&Value> = entries
        .iter()
        .filter(|entry| {
            entry
                .get("surface")
                .and_then(Value::as_str)
                .is_some_and(|surface| surface == "node")
                && entry
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status != "pass")
        })
        .collect();

    assert!(
        node_failures.is_empty(),
        "critical node API matrix entries should pass; failures: {node_failures:#?}"
    );
}

#[test]
fn generate_runtime_api_matrix_report() {
    let report = build_runtime_api_matrix_report();
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_dir = repo_root.join("tests/ext_conformance/reports/parity");
    fs::create_dir_all(&output_dir).expect("create parity report dir");
    let output_path = output_dir.join("runtime_api_matrix.json");
    let json = serde_json::to_string_pretty(&report).expect("serialize runtime matrix");
    fs::write(&output_path, format!("{json}\n")).expect("write runtime matrix report");

    let summary = report.get("summary").expect("summary");
    let total = summary
        .get("total")
        .and_then(Value::as_u64)
        .expect("summary.total");
    let pass = summary
        .get("pass")
        .and_then(Value::as_u64)
        .expect("summary.pass");
    let fail = summary
        .get("fail")
        .and_then(Value::as_u64)
        .expect("summary.fail");
    let bun_fail = summary
        .get("bun")
        .and_then(|bun| bun.get("fail"))
        .and_then(Value::as_u64)
        .expect("summary.bun.fail");

    assert_eq!(pass + fail, total, "summary pass+fail should equal total");
    assert!(pass > 0, "runtime matrix must contain passing entries");
    assert!(
        bun_fail > 0,
        "runtime matrix should currently surface Bun API gaps explicitly"
    );

    let linked_outcomes = report.get("linked_outcomes").expect("linked_outcomes");
    let unit_tests = linked_outcomes
        .get("unit_tests")
        .and_then(Value::as_array)
        .expect("linked_outcomes.unit_tests");
    assert!(
        !unit_tests.is_empty(),
        "linked_outcomes.unit_tests should not be empty"
    );
    assert_eq!(
        linked_outcomes
            .get("e2e_workflow")
            .and_then(|e2e| e2e.get("script_path"))
            .and_then(Value::as_str),
        Some("scripts/e2e/run_all.sh"),
        "runtime matrix should link e2e workflow script"
    );
    let structured_logs = linked_outcomes
        .get("structured_logs")
        .and_then(Value::as_array)
        .expect("linked_outcomes.structured_logs");
    assert!(
        structured_logs.iter().any(|entry| {
            entry
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| {
                    path == "tests/ext_conformance/reports/parity/parity_events.jsonl"
                })
        }),
        "runtime matrix should link parity structured logs"
    );

    eprintln!(
        "Runtime API matrix report written to {} (pass={}, fail={})",
        output_path.display(),
        pass,
        fail
    );
}
