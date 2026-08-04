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
use std::process::Command;

const HIGH_VALUE_ARTIFACT_INVENTORY: &str =
    "docs/evidence/high-value-suite-artifact-inventory.json";
const UBS_EXTENSION_RUNTIME_NOISE_BASELINE: &str =
    "docs/evidence/ubs-extension-runtime-noise-baseline.json";
const PROVIDER_SUPPORT_MODULES: &[&str] = &["model_fetch"];
const REQUIRED_ARTIFACT_INVENTORY_AREAS: &[&str] = &[
    "provider_streaming",
    "sessions",
    "extensions",
    "resource_scheduler_admission",
    "rpc_tui_e2e",
    "perf_report_generators",
    "security_scenarios",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn test_fail(message: impl std::fmt::Display) -> ! {
    panic!("{message}"); // ubs:ignore test harness assertion, not production runtime.
}

/// Parse `suite_classification.toml` → {`suite_name`: [stem, ...]}
fn load_suite_classification(root: &Path) -> HashMap<String, Vec<String>> {
    let path = root.join("tests/suite_classification.toml");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let table: toml::Table = content
        .parse()
        .unwrap_or_else(|e| panic!("invalid TOML in {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.

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
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let matrix: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("invalid JSON: {e}")); // ubs:ignore test harness assertion, not production runtime.

    let mut stems = BTreeSet::new();
    if let Some(requirements) = matrix.get("requirements").and_then(|v| v.as_array()) {
        for req in requirements {
            for category in &["unit_tests", "e2e_scripts"] {
                if let Some(entries) = req.get(*category).and_then(|v| v.as_array()) {
                    for entry in entries {
                        if let Some(p) = entry.get("path").and_then(|v| v.as_str())
                            && let Some(stem) =
                                p.strip_prefix("tests/").and_then(|s| s.strip_suffix(".rs"))
                        {
                            stems.insert(stem.to_string());
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
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let matrix: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("invalid JSON: {e}")); // ubs:ignore test harness assertion, not production runtime.

    matrix
        .get("ci_policy")
        .and_then(|v| v.get("min_classified_trace_coverage_pct"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_else(|| {
            test_fail(format!(
                "missing ci_policy.min_classified_trace_coverage_pct in {}",
                path.display()
            ))
        })
}

fn load_json_value(root: &Path, relative_path: &str) -> serde_json::Value {
    let path = root.join(relative_path);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display())) // ubs:ignore test harness assertion, not production runtime.
}

fn git_tracked_test_stems(root: &Path) -> Option<BTreeSet<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--", "tests/*.rs"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(
        stdout
            .lines()
            .filter_map(|line| {
                let stem = line.strip_prefix("tests/")?.strip_suffix(".rs")?;
                (!stem.contains('/')).then(|| stem.to_string())
            })
            .collect(),
    )
}

fn literal_gitignored_test_stems(root: &Path) -> BTreeSet<String> {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return BTreeSet::new();
    };

    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.starts_with('#')
                && !line.starts_with('!')
                && !line.contains('*')
                && !line.contains('?')
                && !line.contains('[')
                && line.starts_with("tests/")
                && Path::new(line)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        })
        .filter_map(|line| {
            let stem = line.strip_prefix("tests/")?.strip_suffix(".rs")?;
            (!stem.contains('/')).then(|| stem.to_string())
        })
        .collect()
}

/// Discover tracked `tests/*.rs` stems, falling back to disk without Git metadata.
fn on_disk_test_stems(root: &Path) -> BTreeSet<String> {
    if let Some(stems) = git_tracked_test_stems(root) {
        return stems;
    }

    let tests_dir = root.join("tests");
    let ignored = literal_gitignored_test_stems(root);
    let mut stems = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !ignored.contains(stem)
            {
                stems.insert(stem.to_string());
            }
        }
    }
    stems
}

#[derive(Debug, PartialEq, Eq)]
struct SourceInventoryDiff {
    missing_from_matrix: Vec<String>,
    stale_matrix_entries: Vec<String>,
}

impl SourceInventoryDiff {
    const fn is_empty(&self) -> bool {
        self.missing_from_matrix.is_empty() && self.stale_matrix_entries.is_empty()
    }
}

fn collect_source_files(repo_root: &Path, dir: &Path, files: &mut BTreeSet<String>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read source directory {}: {e}", dir.display())); // ubs:ignore test harness assertion, not production runtime.

    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            test_fail(format!(
                "cannot read source directory entry in {}: {e}",
                dir.display()
            ))
        });
        let path = entry.path();
        if path.is_dir() {
            collect_source_files(repo_root, &path, files);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(repo_root)
                .unwrap_or_else(|e| {
                    test_fail(format!(
                        "source path {} is not under repo root {}: {e}",
                        path.display(),
                        repo_root.display()
                    ))
                })
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative);
        }
    }
}

fn on_disk_source_files(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    collect_source_files(root, &root.join("src"), &mut files);
    files
}

fn load_coverage_matrix_source_files(root: &Path) -> BTreeSet<String> {
    let path = root.join("docs/TEST_COVERAGE_MATRIX.md");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.

    content
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("| `src/")?;
            let (path_after_src, _) = rest.split_once('`')?;
            Some(format!("src/{path_after_src}"))
        })
        .collect()
}

fn on_disk_native_provider_modules(root: &Path) -> BTreeSet<String> {
    let providers_dir = root.join("src/providers");
    let entries = std::fs::read_dir(&providers_dir).unwrap_or_else(|e| {
        test_fail(format!(
            "cannot read provider directory {}: {e}",
            providers_dir.display()
        ))
    });

    let mut modules = BTreeSet::new();
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            test_fail(format!(
                "cannot read provider directory entry in {}: {e}",
                providers_dir.display()
            ))
        });
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_else(|| panic!("provider file has invalid UTF-8 path: {}", path.display())); // ubs:ignore test harness assertion, not production runtime.
        if stem != "mod" && !PROVIDER_SUPPORT_MODULES.contains(&stem) {
            modules.insert(stem.to_string());
        }
    }
    modules
}

fn load_provider_doc_native_modules(root: &Path) -> BTreeSet<String> {
    let path = root.join("docs/providers.md");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let marker = "Those modules are ";
    let module_text = content
        .split(marker)
        .nth(1)
        .and_then(|rest| rest.split(". User-visible").next())
        .unwrap_or_else(|| panic!("docs/providers.md missing provider-count rule marker")); // ubs:ignore test harness assertion, not production runtime.

    module_text
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn load_readme_native_provider_rule_modules(root: &Path) -> BTreeSet<String> {
    let path = root.join("README.md");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let rule_line = content
        .lines()
        .find(|line| line.starts_with("Provider-count rule: Pi has "))
        .unwrap_or_else(|| test_fail("README.md missing Provider-count rule line"));
    let marker = "Those modules are ";
    let module_text = rule_line
        .split(marker)
        .nth(1)
        .and_then(|rest| rest.split(". User-visible").next())
        .unwrap_or_else(|| test_fail("README.md provider-count rule missing module list"));

    module_text
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn assert_readme_native_provider_count_claims(root: &Path, expected_count: usize) {
    let path = root.join("README.md");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let claim_phrase = "native provider implementation modules";
    let expected_claim = format!("{expected_count} {claim_phrase}");
    let mut claim_lines = 0usize;

    for (line_index, line) in content.lines().enumerate() {
        if !line.contains(claim_phrase) {
            continue;
        }
        claim_lines += 1;
        assert!(
            line.contains(&expected_claim),
            "README.md line {} native-provider count must be `{expected_claim}`: {line}",
            line_index + 1
        );
    }

    assert!(
        claim_lines > 0,
        "README.md should contain at least one native-provider count claim"
    );
}

fn source_inventory_diff(
    on_disk: &BTreeSet<String>,
    documented: &BTreeSet<String>,
) -> SourceInventoryDiff {
    SourceInventoryDiff {
        missing_from_matrix: on_disk.difference(documented).cloned().collect(),
        stale_matrix_entries: documented.difference(on_disk).cloned().collect(),
    }
}

fn format_source_inventory_diff(diff: &SourceInventoryDiff) -> String {
    let mut sections = Vec::new();
    if !diff.missing_from_matrix.is_empty() {
        sections.push(format!(
            "source files missing from docs/TEST_COVERAGE_MATRIX.md:\n{}",
            diff.missing_from_matrix
                .iter()
                .map(|path| format!("  - {path}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !diff.stale_matrix_entries.is_empty() {
        sections.push(format!(
            "stale docs/TEST_COVERAGE_MATRIX.md source rows:\n{}",
            diff.stale_matrix_entries
                .iter()
                .map(|path| format!("  - {path}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    sections.join("\n\n")
}

#[test]
fn native_provider_module_inventory_matches_provider_docs() {
    let root = repo_root();
    let on_disk = on_disk_native_provider_modules(&root);
    let documented = load_provider_doc_native_modules(&root);

    assert_eq!(
        documented, on_disk,
        "docs/providers.md provider-count rule must match direct provider implementations"
    );
    assert_eq!(
        on_disk.len(),
        11,
        "native provider module count changed; update docs/providers.md and this expectation"
    );
}

#[test]
fn native_provider_module_inventory_matches_readme_claims() {
    let root = repo_root();
    let on_disk = on_disk_native_provider_modules(&root);
    let documented = load_readme_native_provider_rule_modules(&root);

    assert_readme_native_provider_count_claims(&root, on_disk.len());
    assert_eq!(
        documented, on_disk,
        "README.md provider-count rule must match direct provider implementations"
    );
}

#[test]
fn source_coverage_matrix_matches_current_src_inventory() {
    let root = repo_root();
    let on_disk = on_disk_source_files(&root);
    let documented = load_coverage_matrix_source_files(&root);
    let diff = source_inventory_diff(&on_disk, &documented);

    assert!(diff.is_empty(), "{}", format_source_inventory_diff(&diff));
    assert_eq!(
        on_disk.len(),
        documented.len(),
        "source coverage matrix row count must match current src/**/*.rs inventory"
    );
}

#[test]
fn source_inventory_diff_reports_missing_and_stale_entries() {
    let on_disk = BTreeSet::from(["src/a.rs".to_string(), "src/b.rs".to_string()]);
    let documented = BTreeSet::from(["src/a.rs".to_string(), "src/stale.rs".to_string()]);

    let diff = source_inventory_diff(&on_disk, &documented);

    assert_eq!(
        diff,
        SourceInventoryDiff {
            missing_from_matrix: vec!["src/b.rs".to_string()],
            stale_matrix_entries: vec!["src/stale.rs".to_string()],
        }
    );
    let message = format_source_inventory_diff(&diff);
    assert!(message.contains("source files missing"));
    assert!(message.contains("stale docs/TEST_COVERAGE_MATRIX.md source rows"));
}

#[test]
fn high_value_artifact_inventory_covers_required_lanes() {
    let root = repo_root();
    let inventory = load_json_value(&root, HIGH_VALUE_ARTIFACT_INVENTORY);
    assert_eq!(
        inventory["schema"],
        "pi.traceability.high_value_suite_artifact_inventory.v1"
    );

    let suites = inventory["selected_suites"]
        .as_array()
        .expect("selected_suites must be an array");
    assert!(
        !suites.is_empty(),
        "selected_suites must include high-value coverage lanes"
    );

    let mut areas = BTreeSet::new();
    for suite in suites {
        let area = suite["coverage_area"]
            .as_str()
            .expect("suite coverage_area must be a string");
        areas.insert(area.to_string());

        for field in ["suite_ids", "test_paths", "artifact_refs", "schema_tags"] {
            assert!(
                suite[field]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "{area}.{field} must be a non-empty array"
            );
        }
        assert!(
            suite["tmpdir_policy"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            "{area}.tmpdir_policy must be non-empty"
        );
        assert!(
            suite["deterministic_replay_command"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            "{area}.deterministic_replay_command must be non-empty"
        );

        for artifact in suite["artifact_refs"].as_array().unwrap() {
            let path = artifact["path"]
                .as_str()
                .expect("artifact path must be a string");
            let generated_by_ci = artifact["generated_by_ci"].as_bool().unwrap_or(false);
            let is_glob = path.contains('*') || path.contains('?') || path.contains('[');
            if !generated_by_ci && !is_glob {
                assert!(
                    root.join(path).exists(),
                    "non-generated artifact ref must exist: {path}"
                );
            }
        }
    }

    for required in REQUIRED_ARTIFACT_INVENTORY_AREAS {
        assert!(
            areas.contains(*required),
            "artifact inventory missing required coverage area: {required}"
        );
    }
}

#[test]
fn traceability_matrix_links_high_value_artifact_inventory() {
    let root = repo_root();
    let matrix = load_json_value(&root, "docs/traceability_matrix.json");
    let requirements = matrix["requirements"]
        .as_array()
        .expect("requirements must be an array");

    let referenced = requirements.iter().any(|requirement| {
        requirement["evidence_logs"]
            .as_array()
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry["path"].as_str() == Some(HIGH_VALUE_ARTIFACT_INVENTORY))
            })
    });

    assert!(
        referenced,
        "docs/traceability_matrix.json evidence_logs must link {HIGH_VALUE_ARTIFACT_INVENTORY}"
    );
}

#[test]
fn ubs_extension_runtime_noise_baseline_classifies_blocking_categories() {
    let root = repo_root();
    let baseline = load_json_value(&root, UBS_EXTENSION_RUNTIME_NOISE_BASELINE);
    assert_eq!(
        baseline["schema"],
        "pi.ubs.extension_runtime_noise_baseline.v1"
    );
    assert_eq!(baseline["bead_id"], "bd-wv10l");
    assert!(
        root.join("scripts/check_ubs_staged_delta.py").exists(),
        "changed-line UBS gate script must exist"
    );
    assert!(
        baseline["changed_line_gate"]["unchanged"]
            .as_bool()
            .unwrap_or(false),
        "UBS runtime baseline must not weaken the changed-line gate"
    );

    let scanned_files: BTreeSet<String> = baseline["generated_from"]["scanned_files"]
        .as_array()
        .expect("generated_from.scanned_files must be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("scanned file must be a string")
                .to_string()
        })
        .collect();
    assert_eq!(
        scanned_files,
        BTreeSet::from([
            "src/extensions.rs".to_string(),
            "src/pi_wasm.rs".to_string()
        ])
    );

    let categories = baseline["finding_categories"]
        .as_array()
        .expect("finding_categories must be an array");
    assert!(
        !categories.is_empty(),
        "UBS runtime baseline must classify warning/critical categories"
    );

    let mut critical_total = 0_u64;
    let mut warning_total = 0_u64;
    let mut dispositions = BTreeSet::new();
    for category in categories {
        let severity = category["severity"]
            .as_str()
            .expect("category severity must be a string");
        let count = category["count"]
            .as_u64()
            .expect("category count must be a number");
        assert!(
            count > 0,
            "category counts must be positive for classified UBS rows"
        );
        let disposition = category["disposition"]
            .as_str()
            .expect("category disposition must be a string");
        assert!(
            !disposition.trim().is_empty(),
            "category disposition must be non-empty"
        );
        assert_eq!(
            category["changed_line_policy"].as_str(),
            Some("fail_on_new_or_modified_lines"),
            "each UBS category must preserve changed-line blocking policy"
        );
        dispositions.insert(disposition.to_string());
        match severity {
            "critical" => critical_total = critical_total.saturating_add(count),
            "warning" => warning_total = warning_total.saturating_add(count),
            other => test_fail(format!("unexpected blocking UBS severity: {other}")),
        }
    }

    assert_eq!(
        critical_total,
        baseline["raw_ubs_summary"]["critical"]
            .as_u64()
            .expect("raw critical total must be present")
    );
    assert_eq!(
        warning_total,
        baseline["raw_ubs_summary"]["warning"]
            .as_u64()
            .expect("raw warning total must be present")
    );
    for required in [
        "test_only",
        "guarded_runtime_pattern",
        "heuristic_false_positive",
        "optimization_inventory",
    ] {
        assert!(
            dispositions.contains(required),
            "UBS baseline missing required disposition: {required}"
        );
    }
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
