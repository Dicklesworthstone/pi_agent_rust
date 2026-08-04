// tests/non_mock_compliance_gate.rs
//
// Non-mock compliance gate (bd-1f42.2.6).
// Enforces the rubric from docs/non-mock-rubric.json against the actual
// codebase state: test double inventory, allowlist exceptions, suite
// classification coverage, and failure-diagnostic schema compliance.
//
// Generates a structured compliance report at target/compliance-report.json
// when the COMPLIANCE_REPORT=1 env var is set.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(relative: &str) -> Value {
    let path = project_root().join(relative);
    let content = std::fs::read_to_string(&path).expect("JSON fixture must be readable");
    serde_json::from_str(&content).expect("JSON fixture must parse")
}

fn load_rubric() -> Value {
    load_json("docs/non-mock-rubric.json")
}

fn load_inventory() -> Value {
    load_json("docs/test_double_inventory.json")
}

fn load_extension_remediation_backlog() -> Value {
    load_json("tests/full_suite_gate/extension_remediation_backlog.json")
}

fn load_testing_policy() -> String {
    let path = project_root().join("docs/testing-policy.md");
    std::fs::read_to_string(&path).expect("testing-policy.md must be readable")
}

fn load_suite_classification() -> String {
    let path = project_root().join("tests/suite_classification.toml");
    std::fs::read_to_string(&path).expect("suite_classification.toml must be readable")
}

// ─── Exception Time-Boxing Validation ───────────────────────────────

/// Parse the allowlist table from testing-policy.md and extract identifiers.
fn parse_allowlist_identifiers(policy: &str) -> Vec<String> {
    let mut in_allowlist = false;
    let mut identifiers = Vec::new();

    for line in policy.lines() {
        if line.contains("## Allowlisted Exceptions") || line.contains("Allowlisted Exceptions") {
            in_allowlist = true;
            continue;
        }
        // Stop at next heading or process section after the allowlist table.
        if in_allowlist
            && (line.starts_with("## ")
                || line.starts_with("### ")
                || line.starts_with("**Process"))
            && !line.contains("Allowlisted")
        {
            in_allowlist = false;
        }
        if in_allowlist
            && line.starts_with('|')
            && !line.contains("---")
            && !line.contains("Identifier")
            && !line.contains("Location")
        {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if cells.len() >= 3 {
                let ident = cells[1].trim_matches('`').to_string();
                if !ident.is_empty() {
                    identifiers.push(ident);
                }
            }
        }
    }
    identifiers
}

#[test]
fn all_allowlisted_exceptions_have_rationale_in_policy() {
    let policy = load_testing_policy();
    let identifiers = parse_allowlist_identifiers(&policy);

    assert!(
        !identifiers.is_empty(),
        "No allowlisted exception identifiers found in testing-policy.md"
    );

    // Each identifier in the allowlist must have a non-empty rationale column.
    // Only scan within the Allowlisted Exceptions section.
    let mut in_allowlist = false;
    for line in policy.lines() {
        if line.contains("## Allowlisted Exceptions") {
            in_allowlist = true;
            continue;
        }
        if in_allowlist
            && (line.starts_with("## ")
                || line.starts_with("### ")
                || line.starts_with("**Process"))
            && !line.contains("Allowlisted")
        {
            in_allowlist = false;
        }
        if in_allowlist
            && line.starts_with('|')
            && !line.contains("---")
            && !line.contains("Identifier")
            && !line.contains("Location")
        {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if cells.len() >= 5 {
                let ident = cells[1].trim_matches('`');
                let rationale = cells[4];
                if !ident.is_empty() {
                    assert!(
                        !rationale.is_empty(),
                        "Allowlisted exception '{ident}' has empty rationale"
                    );
                }
            }
        }
    }
}

#[test]
fn exception_template_mandates_expiry() {
    let rubric = load_rubric();
    let fields = rubric["exception_template"]["required_fields"]
        .as_array()
        .unwrap();

    let has_expiry = fields
        .iter()
        .any(|f| f["field"].as_str() == Some("expires_at"));
    assert!(
        has_expiry,
        "Exception template must mandate expires_at field for time-boxing"
    );
}

#[test]
fn exception_template_mandates_owner() {
    let rubric = load_rubric();
    let fields = rubric["exception_template"]["required_fields"]
        .as_array()
        .unwrap();

    let has_owner = fields.iter().any(|f| f["field"].as_str() == Some("owner"));
    assert!(
        has_owner,
        "Exception template must mandate owner field for accountability"
    );
}

#[test]
fn exception_template_mandates_replacement_plan() {
    let rubric = load_rubric();
    let fields = rubric["exception_template"]["required_fields"]
        .as_array()
        .unwrap();

    let has_plan = fields
        .iter()
        .any(|f| f["field"].as_str() == Some("replacement_plan"));
    assert!(
        has_plan,
        "Exception template must mandate replacement_plan field"
    );
}

// ─── Test Double Inventory Cross-Reference ──────────────────────────

#[test]
fn inventory_exists_and_has_entries() {
    let inventory = load_inventory();
    let count = inventory["summary"]["entry_count"]
        .as_u64()
        .expect("entry_count must be a number");
    assert!(
        count > 0,
        "Test double inventory must have at least one entry"
    );
}

#[test]
fn extension_stub_reconciliation_declares_disposition_policy() {
    let inventory = load_inventory();
    let reconciliation = &inventory["extension_stub_placeholder_reconciliation"];

    assert_eq!(
        reconciliation["schema"].as_str(),
        Some("pi.qa.extension_stub_placeholder_reconciliation.v1"),
        "extension stub reconciliation must declare a stable schema"
    );
    assert_eq!(
        reconciliation["bead_id"].as_str(),
        Some("bd-8t27h.14"),
        "extension stub reconciliation must identify the owning bead"
    );

    let policy = reconciliation["disposition_policy"]
        .as_object()
        .expect("disposition_policy must be an object");
    for key in [
        "test_fixture",
        "production_compatibility_stub",
        "temporary_conformance_exception",
        "active_child_bead",
    ] {
        assert!(
            policy
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()),
            "disposition_policy.{key} must explain how findings are resolved"
        );
    }
}

#[test]
fn extension_stub_reconciliation_dispositions_have_evidence() {
    let inventory = load_inventory();
    let dispositions = inventory["extension_stub_placeholder_reconciliation"]["dispositions"]
        .as_array()
        .expect("reconciliation.dispositions must be an array");

    assert!(
        !dispositions.is_empty(),
        "extension stub reconciliation must include dispositions"
    );

    for disposition in dispositions {
        let category = disposition["category"]
            .as_str()
            .expect("disposition.category must be a string");
        let status = disposition["status"]
            .as_str()
            .expect("disposition.status must be a string");
        let rationale = disposition["rationale"]
            .as_str()
            .expect("disposition.rationale must be a string");
        assert!(
            !rationale.is_empty(),
            "disposition {category} needs rationale"
        );

        let evidence = disposition["evidence_refs"]
            .as_array()
            .expect("disposition.evidence_refs must be an array");
        assert!(
            !evidence.is_empty(),
            "disposition {category} needs evidence refs"
        );

        if status == "active_child_bead" {
            let child = disposition["child_bead"]
                .as_str()
                .expect("active child bead disposition must name child_bead");
            assert!(
                child.starts_with("bd-8t27h.14."),
                "active child bead for {category} must stay under bd-8t27h.14"
            );
        }
    }
}

#[test]
fn extension_stub_reconciliation_matches_current_backlog_families() {
    let inventory = load_inventory();
    let backlog = load_extension_remediation_backlog();
    let entries = backlog["entries"]
        .as_array()
        .expect("extension remediation backlog entries must be an array");

    let mut family_counts = BTreeMap::<String, u64>::new();
    for entry in entries {
        let family = entry["root_cause_family"]
            .as_str()
            .expect("backlog entry root_cause_family must be a string");
        *family_counts.entry(family.to_string()).or_insert(0) += 1;
    }

    let recorded_families = inventory["extension_stub_placeholder_reconciliation"]
        ["current_conformance_snapshot"]["extension_remediation_backlog"]["families"]
        .as_object()
        .expect("recorded backlog family counts must be an object");

    assert_eq!(
        recorded_families.len(),
        family_counts.len(),
        "reconciliation family inventory must not retain stale backlog families"
    );

    for (family, count) in family_counts {
        assert_eq!(
            recorded_families.get(&family).and_then(Value::as_u64),
            Some(count),
            "reconciliation count for {family} must match extension remediation backlog"
        );
    }
}

#[test]
fn runtime_api_compatibility_disposition_points_to_audited_virtual_module_stubs() {
    let inventory = load_inventory();
    let dispositions = inventory["extension_stub_placeholder_reconciliation"]["dispositions"]
        .as_array()
        .expect("reconciliation dispositions must be an array");
    let disposition = dispositions
        .iter()
        .find(|entry| entry["category"] == "runtime_api_gap")
        .expect("runtime API compatibility disposition must exist");

    assert_eq!(
        disposition["status"].as_str(),
        Some("production_compatibility_stub"),
        "runtime API stubs must remain explicitly classified"
    );
    assert_eq!(
        disposition["resolved_by_bead"].as_str(),
        Some("bd-8t27h.14.1"),
        "runtime API stub provenance must retain its resolution bead"
    );
    assert_eq!(
        disposition["backlog_entries"].as_u64(),
        Some(0),
        "resolved compatibility stubs must not remain in the current-failure backlog"
    );

    let audited_extensions = disposition["audited_extensions"]
        .as_array()
        .expect("runtime API disposition must enumerate audited extensions");
    let evidence_refs = disposition["evidence_refs"]
        .as_array()
        .expect("runtime API disposition must include evidence refs");
    let mut expected = BTreeMap::from([
        ("npm/pi-search-agent", ("openai", "openai_imports")),
        ("npm/pi-wakatime", ("adm-zip", "adm_zip_import")),
        ("npm/pi-web-access", ("linkedom", "linkedom_parse_html")),
        (
            "npm/qualisero-pi-agent-scip",
            ("@sourcegraph/scip-typescript", "scip_typescript_import"),
        ),
        (
            "third-party/qualisero-pi-agent-scip",
            (
                "@sourcegraph/scip-typescript",
                "scip_typescript_subpath_import",
            ),
        ),
    ]);
    let mut unexpected = Vec::new();

    for entry in audited_extensions {
        let extension_id = entry["extension_id"]
            .as_str()
            .expect("audited runtime API entry must name extension_id");
        let Some((package, test_name)) = expected.remove(extension_id) else {
            unexpected.push(extension_id);
            continue;
        };

        assert_eq!(
            entry["virtual_module"].as_str(),
            Some(package),
            "{extension_id} must identify its audited virtual module"
        );
        assert_eq!(
            entry["test_name"].as_str(),
            Some(test_name),
            "{extension_id} must identify its focused import test"
        );
        let has_impl_ref = evidence_refs
            .iter()
            .filter_map(Value::as_str)
            .any(|evidence| evidence == format!("src/extensions_js.rs#virtual_module:{package}"));
        let has_stub_test_ref = evidence_refs
            .iter()
            .filter_map(Value::as_str)
            .any(|evidence| evidence == format!("tests/npm_module_stubs.rs#{test_name}"));
        assert!(
            has_impl_ref,
            "{extension_id} must cite the production virtual module implementation"
        );
        assert!(
            has_stub_test_ref,
            "{extension_id} must cite focused npm module stub coverage"
        );
    }

    assert!(
        unexpected.is_empty(),
        "runtime API disposition has unexpected entries: {unexpected:?}"
    );
    assert!(
        expected.is_empty(),
        "runtime API disposition is missing audited entries for: {expected:?}"
    );
}

#[test]
fn high_risk_doubles_are_in_allowlist_or_vcr_suite() {
    let inventory = load_inventory();
    let policy = load_testing_policy();
    let allowlist_ids = parse_allowlist_identifiers(&policy);
    let mut violations = Vec::new();

    let clusters = inventory["summary"]["top_risk_clusters"]
        .as_array()
        .expect("top_risk_clusters must be an array");

    // High-risk clusters that are in unit-inline suite need allowlist entries
    // or must have been addressed. We check that the policy at least acknowledges them.
    for cluster in clusters {
        let module = cluster["module"].as_str().unwrap();
        let suite_counts = cluster["suite_counts"].as_object().unwrap();

        if suite_counts.contains_key("unit-inline") {
            let unit_count = suite_counts["unit-inline"].as_u64().unwrap_or(0);
            if unit_count > 10 {
                // Large unit-inline mock usage: either it's a known pattern
                // or it should be in the allowlist.
                let is_extension_module = module.contains("extension");
                let is_in_allowlist = policy.contains(module)
                    || allowlist_ids
                        .iter()
                        .any(|id| module.contains(id) || id.contains(module));

                // Extension modules have documented stub usage patterns.
                // Non-extension modules with large mock counts need attention.
                if !is_extension_module && !is_in_allowlist {
                    violations.push(format!(
                        "Module '{module}' has {unit_count} unit-inline doubles but is not in \
                         allowlist or extension module"
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "High-risk unit-inline test doubles must be allowlisted, extension-scoped, or \
         remediated:\n{}",
        violations.join("\n")
    );
}

// ─── Suite Classification Completeness ──────────────────────────────

#[test]
fn all_test_files_are_classified() {
    let classification = load_suite_classification();
    let tests_dir = project_root().join("tests");

    // Get all .rs test files (excluding common/ directory and mod.rs).
    let test_files: Vec<String> = std::fs::read_dir(&tests_dir)
        .expect("Could not read tests/ directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
                let stem = path.file_stem()?.to_str()?.to_string();
                Some(stem)
            } else {
                None
            }
        })
        .collect();

    let mut unclassified = Vec::new();
    for file in &test_files {
        if !classification.contains(&format!("\"{file}\"")) {
            unclassified.push(file.as_str());
        }
    }

    // Allow a small number of unclassified files during active development,
    // but flag them.
    if !unclassified.is_empty() {
        eprintln!(
            "COMPLIANCE WARNING: {} test file(s) not in suite_classification.toml: {:?}",
            unclassified.len(),
            unclassified
        );
    }

    // Hard gate: no more than 5% of test files can be unclassified.
    // Use integer arithmetic to avoid float-to-int casting warnings.
    let threshold = test_files.len().div_ceil(20); // ceil(len / 20) = ceil(len * 0.05)
    assert!(
        unclassified.len() <= threshold,
        "Too many unclassified test files ({}/{}). Maximum allowed: {}. Unclassified: {:?}",
        unclassified.len(),
        test_files.len(),
        threshold,
        unclassified
    );
}

#[test]
fn no_vcr_imports_in_unit_suite_files() {
    let classification = load_suite_classification();

    // Extract unit suite file names.
    let unit_start = classification
        .find("[suite.unit]")
        .expect("[suite.unit] not found");
    let unit_end = classification[unit_start..]
        .find("[suite.")
        .map_or(classification.len(), |i| unit_start + i);
    let unit_section = &classification[unit_start..unit_end];

    let vcr_imports = [
        "use pi::vcr",
        "VcrRecorder",
        "VcrMode",
        "cassette_root",
        "fixtures/vcr",
    ];

    for line in unit_section.lines() {
        if let Some(file) = line
            .trim()
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix("\",").or_else(|| s.strip_suffix('"')))
        {
            let path = project_root().join("tests").join(format!("{file}.rs"));
            if path.exists() {
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                for import in &vcr_imports {
                    assert!(
                        !content.contains(import),
                        "Unit suite file '{file}.rs' contains VCR import '{import}'. \
                         Unit tests must not use VCR cassettes."
                    );
                }
            }
        }
    }
}

// ─── Rubric Module Coverage Mapping ─────────────────────────────────

/// Map rubric module names to expected test file patterns.
fn module_test_patterns() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "agent_loop",
            vec!["agent_loop_vcr", "agent_loop_reliability", "e2e_agent_loop"],
        ),
        (
            "tools",
            vec!["tools_conformance", "tools_hardened", "e2e_tools"],
        ),
        (
            "providers",
            vec![
                "provider_streaming",
                "provider_factory",
                "provider_error_paths",
                "provider_backward_lock",
                "provider_contract",
                "provider_unit_checklist",
                "provider_native_verify",
                "provider_metadata_comprehensive",
                "provider_native_contract",
            ],
        ),
        (
            "session",
            vec![
                "session_conformance",
                "session_index_tests",
                "session_sqlite",
                "session_picker",
            ],
        ),
        (
            "extensions",
            vec![
                "extensions_manifest",
                "extensions_registration",
                "extensions_event_wiring",
                "extensions_message_session",
                "extensions_stress",
                "ext_conformance",
            ],
        ),
        ("auth", vec!["auth_oauth_refresh_vcr"]),
        ("error", vec!["error_types", "error_handling"]),
        ("model", vec!["model_serialization", "model_registry"]),
        ("config", vec!["config_precedence"]),
        (
            "sse",
            vec![
                "sse_strict_compliance",
                "repro_sse_flush",
                "repro_sse_newline",
            ],
        ),
        ("compaction", vec!["compaction", "compaction_bug"]),
        ("vcr", vec!["provider_contract"]),
        ("rpc", vec!["rpc_mode", "rpc_protocol"]),
        ("interactive", vec!["tui_state", "tui_snapshot"]),
    ]
}

/// Evaluate a single module's test evidence and return a compliance entry.
fn evaluate_module_compliance(
    module: &Value,
    patterns: &[&str],
    tests_dir: &std::path::Path,
    classification: &str,
) -> Value {
    let name = module["name"].as_str().unwrap();
    let criticality = module["criticality"].as_str().unwrap();

    let mut found_tests = Vec::new();
    let mut missing_tests = Vec::new();

    for pattern in patterns {
        let test_path = tests_dir.join(format!("{pattern}.rs"));
        let in_classification = classification.contains(&format!("\"{pattern}\""));

        if test_path.exists() && in_classification {
            found_tests.push(*pattern);
        } else if test_path.exists() {
            missing_tests.push(format!("{pattern} (exists but not classified)"));
        } else {
            missing_tests.push(format!("{pattern} (not found)"));
        }
    }

    let status = if found_tests.is_empty() && criticality == "critical" {
        "FAIL"
    } else if found_tests.is_empty() {
        "WARN"
    } else if !missing_tests.is_empty() {
        "PARTIAL"
    } else {
        "PASS"
    };

    json!({
        "module": name,
        "criticality": criticality,
        "status": status,
        "found_tests": found_tests,
        "missing_tests": missing_tests,
        "line_floor_pct": module["line_floor_pct"],
        "function_floor_pct": module["function_floor_pct"],
    })
}

#[test]
fn rubric_modules_have_corresponding_test_evidence() {
    let rubric = load_rubric();
    let modules = rubric["module_thresholds"]["modules"].as_array().unwrap();
    let classification = load_suite_classification();
    let tests_dir = project_root().join("tests");
    let patterns_map = module_test_patterns();

    let mut compliance_entries = Vec::new();

    for module in modules {
        let name = module["name"].as_str().unwrap();
        let criticality = module["criticality"].as_str().unwrap();

        let patterns = patterns_map
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(&[] as &[&str], |(_, p)| p.as_slice());

        let entry = evaluate_module_compliance(module, patterns, &tests_dir, &classification);
        compliance_entries.push(entry);

        // Critical modules must have at least one test file.
        if criticality == "critical" {
            let has_evidence = patterns.iter().any(|p| {
                tests_dir.join(format!("{p}.rs")).exists()
                    && classification.contains(&format!("\"{p}\""))
            });
            assert!(
                has_evidence,
                "Critical module '{name}' has no matching test evidence. \
                 Expected at least one of: {patterns:?}"
            );
        }
    }

    // Optionally write compliance report.
    if std::env::var("COMPLIANCE_REPORT").is_ok() {
        let report = json!({
            "schema": "pi.qa.compliance_report.v1",
            "bead_id": "bd-1f42.2.6",
            "generated_at": chrono_lite_now(),
            "modules": compliance_entries,
        });
        let report_path = project_root().join("target/compliance-report.json");
        if let Some(parent) = report_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&report_path, serde_json::to_string_pretty(&report).unwrap());
        eprintln!("Compliance report written to: {}", report_path.display());
    }
}

/// Cheap timestamp without chrono dependency.
fn chrono_lite_now() -> String {
    // Use a fixed format; actual timestamp would come from system.
    "2026-02-12T00:00:00Z".to_string()
}

// ─── Failure-Diagnostic Schema Reference ────────────────────────────

#[test]
fn rubric_defines_failure_log_schema() {
    let rubric = load_rubric();
    let schema = &rubric["failure_log_schema"];

    assert_eq!(
        schema["schema_id"].as_str().unwrap(),
        "pi.test.failure_log.v1"
    );

    let fields = schema["fields"].as_array().unwrap();
    assert!(
        fields.len() >= 10,
        "Failure log schema must have at least 10 fields, got {}",
        fields.len()
    );
}

#[test]
fn failure_log_schema_env_field_has_redaction() {
    let rubric = load_rubric();
    let fields = rubric["failure_log_schema"]["fields"].as_array().unwrap();

    let env_field = fields
        .iter()
        .find(|f| f["field"].as_str() == Some("env"))
        .expect("Schema must have env field");

    let rules = env_field["redaction_rules"]
        .as_array()
        .expect("env field must have redaction_rules");

    // Must have rules covering API keys and secrets.
    let rules_text: String = rules
        .iter()
        .map(|r| r.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        rules_text.contains("KEY") || rules_text.contains("SECRET") || rules_text.contains("TOKEN"),
        "Redaction rules must cover API keys, secrets, or tokens"
    );
}

// ─── Mock Identifier Scanning ───────────────────────────────────────

#[test]
fn no_mock_crate_dependencies() {
    let cargo_toml = project_root().join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml).expect("Cargo.toml must be readable");

    let banned_crates = ["mockall", "mockito", "wiremock"];
    for crate_name in banned_crates {
        // Check for crate as dependency (not just mention in comments).
        let pattern = format!("{crate_name} =");
        assert!(
            !content.contains(&pattern),
            "Cargo.toml contains banned mock crate dependency: {crate_name}"
        );
    }
}

#[test]
fn no_disallowed_doubles_in_unit_suite() {
    let classification = load_suite_classification();

    // Extract unit suite file names.
    let unit_files = extract_suite_files(&classification, "unit");
    let disallowed = ["NullSession", "NullUiHandler", "DummyProvider"];

    // Known pre-existing violations that need time-boxed remediation.
    // Each entry: (file_stem, double_identifier, tracking_bead).
    let known_violations: &[(&str, &str)] = &[
        ("model_selector_cycling", "DummyProvider"),
        // The compliance gate itself references double names as string literals
        // for scanning purposes — these are not actual test-double usage.
        ("non_mock_compliance_gate", "NullSession"),
        ("non_mock_compliance_gate", "NullUiHandler"),
        ("non_mock_compliance_gate", "DummyProvider"),
        // Documentation-policy tests reference rejected double names as
        // string literals to verify the policy documents them explicitly.
        ("qa_docs_policy_validation", "NullSession"),
        ("qa_docs_policy_validation", "NullUiHandler"),
        ("qa_docs_policy_validation", "DummyProvider"),
    ];

    let mut violations = Vec::new();

    for file in &unit_files {
        let path = project_root().join("tests").join(format!("{file}.rs"));
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            for double in &disallowed {
                if content.contains(double) {
                    let is_known = known_violations
                        .iter()
                        .any(|(f, d)| *f == file.as_str() && *d == *double);
                    if is_known {
                        eprintln!(
                            "COMPLIANCE FINDING (known): '{file}.rs' uses '{double}' - \
                             tracked for remediation"
                        );
                    } else {
                        violations.push(format!("{file}.rs: {double}"));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "NEW disallowed doubles found in unit suite (not in known violations list): {violations:?}"
    );
}

fn extract_suite_files(classification: &str, suite_name: &str) -> Vec<String> {
    let marker = format!("[suite.{suite_name}]");
    let start = match classification.find(&marker) {
        Some(i) => i + marker.len(),
        None => return Vec::new(),
    };

    let remaining = &classification[start..];
    let end = remaining
        .find("[suite.")
        .or_else(|| remaining.find("[quarantine"))
        .unwrap_or(remaining.len());

    let section = &remaining[..end];
    section
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('"') {
                Some(
                    trimmed
                        .trim_matches(|c: char| c == '"' || c == ',' || c.is_whitespace())
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect()
}

// ─── Per-Module Threshold Sanity ────────────────────────────────────

#[test]
fn critical_modules_have_line_floor_at_least_75() {
    let rubric = load_rubric();
    let modules = rubric["module_thresholds"]["modules"].as_array().unwrap();

    for module in modules {
        let name = module["name"].as_str().unwrap();
        let criticality = module["criticality"].as_str().unwrap();
        let floor = module["line_floor_pct"].as_f64().unwrap();

        if criticality == "critical" {
            assert!(
                floor >= 75.0,
                "Critical module '{name}' has line_floor_pct {floor}% below minimum 75%"
            );
        }
    }
}

#[test]
fn high_modules_have_line_floor_at_least_70() {
    let rubric = load_rubric();
    let modules = rubric["module_thresholds"]["modules"].as_array().unwrap();

    for module in modules {
        let name = module["name"].as_str().unwrap();
        let criticality = module["criticality"].as_str().unwrap();
        let floor = module["line_floor_pct"].as_f64().unwrap();

        if criticality == "high" {
            assert!(
                floor >= 70.0,
                "High-criticality module '{name}' has line_floor_pct {floor}% below minimum 70%"
            );
        }
    }
}

#[test]
fn all_modules_have_positive_targets() {
    let rubric = load_rubric();
    let modules = rubric["module_thresholds"]["modules"].as_array().unwrap();

    for module in modules {
        let name = module["name"].as_str().unwrap();
        let line_target = module["line_target_pct"].as_f64().unwrap();
        let fn_target = module["function_target_pct"].as_f64().unwrap();

        assert!(
            line_target > 0.0,
            "Module '{name}' has non-positive line_target_pct"
        );
        assert!(
            fn_target > 0.0,
            "Module '{name}' has non-positive function_target_pct"
        );
    }
}

// ─── Compliance Report Generation Test ──────────────────────────────

#[test]
fn compliance_report_format_is_valid() {
    // Build a sample compliance report and validate its structure.
    let report = json!({
        "schema": "pi.qa.compliance_report.v1",
        "bead_id": "bd-1f42.2.6",
        "generated_at": "2026-02-12T00:00:00Z",
        "modules": [
            {
                "module": "agent_loop",
                "criticality": "critical",
                "status": "PASS",
                "found_tests": ["agent_loop_vcr"],
                "missing_tests": [],
                "line_floor_pct": 75.0,
                "function_floor_pct": 70.0
            }
        ]
    });

    assert_eq!(
        report["schema"].as_str().unwrap(),
        "pi.qa.compliance_report.v1"
    );

    let modules = report["modules"].as_array().unwrap();
    assert!(!modules.is_empty());

    let entry = &modules[0];
    let valid_statuses = ["PASS", "PARTIAL", "WARN", "FAIL"];
    let status = entry["status"].as_str().unwrap();
    assert!(
        valid_statuses.contains(&status),
        "Invalid compliance status: {status}"
    );
}

// ─── Quarantine Section Validation ──────────────────────────────────

#[test]
fn quarantine_entries_have_required_fields_if_present() {
    let classification = load_suite_classification();

    // Check if there are any non-commented quarantine entries.
    let has_real_quarantine = classification
        .lines()
        .any(|l| l.starts_with("[quarantine.") && !l.starts_with("[quarantine]"));

    if !has_real_quarantine {
        // No active quarantine entries; that's fine.
        return;
    }

    // If quarantine entries exist, validate they have required fields.
    let required_fields = [
        "category",
        "owner",
        "quarantined",
        "expires",
        "bead",
        "evidence",
        "repro",
        "reason",
        "remove_when",
    ];

    // Find all non-commented quarantine entry headers.
    for line in classification.lines() {
        let trimmed = line.trim();
        // Skip comments and the bare [quarantine] section.
        if trimmed.starts_with('#') || !trimmed.starts_with("[quarantine.") {
            continue;
        }
        if trimmed == "[quarantine]" {
            continue;
        }

        let entry_name = trimmed
            .trim_start_matches("[quarantine.")
            .trim_end_matches(']');

        // Find this entry's content (from the header to the next [ section).
        let entry_start = classification.find(line).unwrap();
        let entry_end = classification[entry_start + line.len()..]
            .find("\n[")
            .map_or(classification.len(), |i| entry_start + line.len() + i);
        let entry_content = &classification[entry_start..entry_end];

        for field in required_fields {
            assert!(
                entry_content.contains(&format!("{field} =")),
                "Quarantine entry '{entry_name}' missing required field: {field}"
            );
        }
    }
}

// ─── Global Compliance Summary ──────────────────────────────────────

#[test]
fn global_compliance_summary() {
    let rubric = load_rubric();
    let _inventory = load_inventory();
    let policy = load_testing_policy();
    let classification = load_suite_classification();

    let modules = rubric["module_thresholds"]["modules"].as_array().unwrap();

    let allowlist_ids = parse_allowlist_identifiers(&policy);
    let unit_files = extract_suite_files(&classification, "unit");
    let vcr_files = extract_suite_files(&classification, "vcr");
    let e2e_files = extract_suite_files(&classification, "e2e");

    let total_classified = unit_files.len() + vcr_files.len() + e2e_files.len();

    eprintln!("=== Non-Mock Compliance Summary ===");
    eprintln!("Rubric modules defined: {}", modules.len());
    eprintln!("Allowlisted exceptions: {}", allowlist_ids.len());
    eprintln!(
        "Suite classification: {} unit, {} vcr, {} e2e ({} total)",
        unit_files.len(),
        vcr_files.len(),
        e2e_files.len(),
        total_classified
    );
    eprintln!("===================================");

    // Basic sanity: we should have a reasonable number of classified test files.
    assert!(
        total_classified >= 50,
        "Expected at least 50 classified test files, got {total_classified}"
    );

    // Rubric should define at least 10 modules.
    assert!(
        modules.len() >= 10,
        "Rubric should define at least 10 modules, got {}",
        modules.len()
    );
}
