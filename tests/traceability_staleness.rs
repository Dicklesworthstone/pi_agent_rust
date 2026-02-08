//! Governance gate: stale-mapping detection between traceability matrix,
//! suite classification, and on-disk test files.
//!
//! Enforced by bd-k5q5.7.12. Fails CI when:
//! - A test file on disk is not in `suite_classification.toml`
//! - A `suite_classification.toml` entry has no matching file on disk
//! - The traceability matrix references a test not in `suite_classification.toml`
//!
//! Warnings (logged but not fatal):
//! - Classified test files not traced to any requirement

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Parse `suite_classification.toml` → {`suite_name`: [stem, ...]}
fn load_suite_classification(root: &Path) -> HashMap<String, Vec<String>> {
    let path = root.join("tests/suite_classification.toml");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let table: toml::Value = content
        .parse()
        .unwrap_or_else(|e| panic!("invalid TOML in {}: {e}", path.display()));

    let mut result = HashMap::new();
    if let Some(suite) = table.get("suite").and_then(|v| v.as_table()) {
        for (name, data) in suite {
            let files = data
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            result.insert(name.clone(), files);
        }
    }
    result
}

/// Extract test file stems from `traceability_matrix.json` requirements.
fn load_matrix_test_stems(root: &Path) -> BTreeSet<String> {
    let path = root.join("docs/traceability_matrix.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let matrix: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("invalid JSON: {e}"));

    let mut stems = BTreeSet::new();
    if let Some(requirements) = matrix.get("requirements").and_then(|v| v.as_array()) {
        for req in requirements {
            for category in &["unit_tests", "e2e_scripts"] {
                if let Some(entries) = req.get(*category).and_then(|v| v.as_array()) {
                    for entry in entries {
                        if let Some(p) = entry.get("path").and_then(|v| v.as_str()) {
                            if let Some(stem) =
                                p.strip_prefix("tests/").and_then(|s| s.strip_suffix(".rs"))
                            {
                                stems.insert(stem.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    stems
}

/// Extract CI coverage threshold from `traceability_matrix.json`.
fn load_matrix_min_trace_coverage_pct(root: &Path) -> usize {
    let path = root.join("docs/traceability_matrix.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let matrix: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("invalid JSON: {e}"));

    matrix
        .get("ci_policy")
        .and_then(|v| v.get("min_classified_trace_coverage_pct"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_else(|| {
            panic!(
                "missing ci_policy.min_classified_trace_coverage_pct in {}",
                path.display()
            )
        })
}

/// Discover all `tests/*.rs` file stems on disk.
fn on_disk_test_stems(root: &Path) -> BTreeSet<String> {
    let tests_dir = root.join("tests");
    let mut stems = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    stems.insert(stem.to_string());
                }
            }
        }
    }
    stems
}

#[test]
fn no_unclassified_test_files() {
    let root = repo_root();
    let suites = load_suite_classification(&root);
    let classified: BTreeSet<String> = suites.values().flatten().cloned().collect();
    let on_disk = on_disk_test_stems(&root);

    let unclassified: Vec<_> = on_disk.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "test files on disk but missing from suite_classification.toml:\n{}",
        unclassified
            .iter()
            .map(|s| format!("  - tests/{s}.rs"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn no_phantom_classified_entries() {
    let root = repo_root();
    let suites = load_suite_classification(&root);
    let classified: BTreeSet<String> = suites.values().flatten().cloned().collect();
    let on_disk = on_disk_test_stems(&root);

    let phantom: Vec<_> = classified.difference(&on_disk).collect();
    assert!(
        phantom.is_empty(),
        "suite_classification.toml lists entries with no matching file:\n{}",
        phantom
            .iter()
            .map(|s| format!("  - {s} (tests/{s}.rs not found)"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn matrix_references_only_classified_tests() {
    let root = repo_root();
    let suites = load_suite_classification(&root);
    let classified: BTreeSet<String> = suites.values().flatten().cloned().collect();
    let matrix_stems = load_matrix_test_stems(&root);

    let not_classified: Vec<_> = matrix_stems.difference(&classified).collect();
    assert!(
        not_classified.is_empty(),
        "traceability matrix references test files not in suite_classification.toml:\n{}",
        not_classified
            .iter()
            .map(|s| format!("  - tests/{s}.rs"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn matrix_references_only_existing_tests() {
    let root = repo_root();
    let on_disk = on_disk_test_stems(&root);
    let matrix_stems = load_matrix_test_stems(&root);

    let missing: Vec<_> = matrix_stems.difference(&on_disk).collect();
    assert!(
        missing.is_empty(),
        "traceability matrix references test files that don't exist:\n{}",
        missing
            .iter()
            .map(|s| format!("  - tests/{s}.rs"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn staleness_coverage_report() {
    let root = repo_root();
    let suites = load_suite_classification(&root);
    let classified: BTreeSet<String> = suites.values().flatten().cloned().collect();
    let on_disk = on_disk_test_stems(&root);
    let matrix_stems = load_matrix_test_stems(&root);
    let min_coverage_pct = load_matrix_min_trace_coverage_pct(&root);

    let traced_count = classified.intersection(&matrix_stems).count();
    let total = classified.len();
    let pct_tenths = traced_count
        .saturating_mul(1000)
        .checked_div(total)
        .unwrap_or(0);
    let pct_whole = pct_tenths / 10;
    let pct_frac = pct_tenths % 10;

    eprintln!("--- Staleness Coverage Report ---");
    eprintln!("  on-disk test files:    {}", on_disk.len());
    eprintln!("  classified test files:  {}", classified.len());
    eprintln!("  matrix-traced files:    {}", matrix_stems.len());
    eprintln!("  coverage:               {traced_count}/{total} ({pct_whole}.{pct_frac}%)");
    eprintln!("  min coverage policy:    {min_coverage_pct}%");

    let untraceable: Vec<_> = classified.difference(&matrix_stems).collect();
    if !untraceable.is_empty() {
        eprintln!("  untraceable ({}):", untraceable.len());
        for stem in &untraceable {
            eprintln!("    - tests/{stem}.rs");
        }
    }
}

#[test]
fn staleness_coverage_meets_policy_threshold() {
    let root = repo_root();
    let suites = load_suite_classification(&root);
    let classified: BTreeSet<String> = suites.values().flatten().cloned().collect();
    let matrix_stems = load_matrix_test_stems(&root);
    let min_coverage_pct = load_matrix_min_trace_coverage_pct(&root);

    let traced_count = classified.intersection(&matrix_stems).count();
    let total = classified.len();
    assert!(total > 0, "classified test set should not be empty");

    let coverage_tenths = traced_count
        .saturating_mul(1000)
        .checked_div(total)
        .unwrap_or(0);
    let threshold_tenths = min_coverage_pct.saturating_mul(10);
    let missing: Vec<_> = classified
        .difference(&matrix_stems)
        .take(10)
        .map(|stem| format!("tests/{stem}.rs"))
        .collect();

    let coverage_whole = coverage_tenths / 10;
    let coverage_frac = coverage_tenths % 10;

    assert!(
        coverage_tenths >= threshold_tenths,
        "classified traceability coverage below policy threshold: \
         {coverage_whole}.{coverage_frac}% < {min_coverage_pct}% (traced={traced_count}, classified={total}). \
         Sample missing mappings: {}",
        if missing.is_empty() {
            "(none)".to_string()
        } else {
            missing.join(", ")
        }
    );
}

#[test]
fn python_governance_script_passes() {
    let root = repo_root();
    let script = root.join("scripts/check_traceability_matrix.py");
    let output = std::process::Command::new("python3")
        .arg(&script)
        .current_dir(&root)
        .output()
        .expect("failed to run check_traceability_matrix.py");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "check_traceability_matrix.py failed (exit {}):\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code().unwrap_or(-1)
    );
    assert!(
        stdout.contains("TRACEABILITY CHECK PASSED"),
        "expected PASSED in output:\n{stdout}"
    );
}
