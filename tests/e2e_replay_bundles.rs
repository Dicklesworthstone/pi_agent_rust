#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]

//! Replay bundle validation tests (bd-1f42.8.7).
//!
//! Validates that every failing E2E/unit suite can be reproduced from emitted
//! artifacts with a single deterministic command sequence. Also validates that
//! replay metadata is consistent across scenario matrix, CI gates, and suite
//! classification.
//!
//! Ordinary test runs validate replay bundles in memory without modifying
//! tracked evidence. Set `PI_GENERATE_REPLAY_BUNDLE_ARTIFACTS=1` to regenerate
//! the canonical examples explicitly.

use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const GENERATE_REPLAY_BUNDLE_ARTIFACTS_ENV: &str = "PI_GENERATE_REPLAY_BUNDLE_ARTIFACTS";

fn replay_bundle_artifact_generation_requested() -> bool {
    let raw = std::env::var(GENERATE_REPLAY_BUNDLE_ARTIFACTS_ENV).ok();
    replay_bundle_artifact_generation_requested_from(raw.as_deref())
}

fn replay_bundle_artifact_generation_requested_from(raw: Option<&str>) -> bool {
    matches!(raw, Some("1"))
}

fn write_non_empty_replay_artifact(path: &Path, payload: &str) {
    assert!(
        !payload.trim().is_empty(),
        "refusing to emit empty replay artifact {}",
        path.display()
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|err| {
            panic!(
                "failed to create replay artifact directory {}: {err}",
                parent.display()
            )
        });
    }
    std::fs::write(path, payload)
        .unwrap_or_else(|err| panic!("failed to write replay artifact {}: {err}", path.display()));
    let size = std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("failed to stat replay artifact {}: {err}", path.display()))
        .len();
    assert!(size > 0, "replay artifact is empty: {}", path.display());
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Parse suite_classification.toml and return all known test file stems.
fn all_classified_suites(root: &Path) -> HashSet<String> {
    let toml_path = root.join("tests/suite_classification.toml");
    let Ok(text) = std::fs::read_to_string(&toml_path) else {
        return HashSet::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return HashSet::new();
    };

    let mut stems = HashSet::new();
    if let Some(suite) = table.get("suite").and_then(|v| v.as_table()) {
        for (_category, files_val) in suite {
            if let Some(files) = files_val.get("files").and_then(|v| v.as_array()) {
                for f in files {
                    if let Some(s) = f.as_str() {
                        stems.insert(s.to_string());
                    }
                }
            }
        }
    }
    stems
}

/// Extract suite names from a replay command string.
/// Looks for `--suite <name>` and `--test <name>` patterns.
fn extract_suites_from_command(cmd: &str) -> Vec<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let mut suites = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if (tokens[i] == "--suite" || tokens[i] == "--test") && i + 1 < tokens.len() {
            suites.push(tokens[i + 1].to_string());
            i += 2;
        } else {
            i += 1;
        }
    }
    suites
}

/// Extract test target from a cargo test command.
/// Looks for `cargo test --test <name>` pattern.
fn extract_cargo_test_target(cmd: &str) -> Option<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    for (i, &tok) in tokens.iter().enumerate() {
        if tok == "--test" && i + 1 < tokens.len() {
            return Some(tokens[i + 1].to_string());
        }
    }
    None
}

// ── Replay Bundle Schema ──────────────────────────────────────────────────

/// Represents a consolidated replay bundle artifact.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReplayBundle {
    schema: String,
    generated_at: String,
    correlation_id: String,
    source_summary_path: String,
    one_command_replay: String,
    environment: ReplayEnvironment,
    failed_suites: Vec<SuiteReplayEntry>,
    failed_unit_targets: Vec<UnitReplayEntry>,
    failed_gates: Vec<GateReplayEntry>,
    summary: ReplayBundleSummary,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReplayEnvironment {
    profile: String,
    shard_kind: String,
    shard_index: Option<u32>,
    shard_total: Option<u32>,
    rustc_version: String,
    cargo_target_dir: String,
    vcr_mode: String,
    git_sha: String,
    git_branch: String,
    os: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SuiteReplayEntry {
    suite: String,
    exit_code: i32,
    root_cause_class: String,
    runner_replay: String,
    cargo_replay: String,
    targeted_replay: String,
    digest_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UnitReplayEntry {
    target: String,
    exit_code: i32,
    cargo_replay: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateReplayEntry {
    gate_id: String,
    gate_name: String,
    reproduce_command: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReplayBundleSummary {
    total_failed_suites: usize,
    total_failed_units: usize,
    total_failed_gates: usize,
    all_commands_reference_valid_targets: bool,
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Validate that all replay_command entries in the scenario matrix reference
/// suites that exist in suite_classification.toml.
#[test]
fn scenario_matrix_replay_commands_reference_valid_suites() {
    let root = repo_root();
    let matrix_path = root.join("docs/e2e_scenario_matrix.json");
    let matrix = load_json(&matrix_path).expect("e2e_scenario_matrix.json must exist");
    let classified = all_classified_suites(&root);

    eprintln!("\n=== Scenario Matrix Replay Command Validation ===\n");
    eprintln!("  Classified suites: {}", classified.len());

    let rows = matrix["rows"].as_array().expect("rows must be an array");
    let mut invalid_refs: Vec<String> = Vec::new();

    for row in rows {
        let workflow_id = row["workflow_id"].as_str().unwrap_or("unknown");
        let status = row["status"].as_str().unwrap_or("unknown");

        // Check replay_command if present
        if let Some(cmd) = row.get("replay_command").and_then(|v| v.as_str()) {
            let suites = extract_suites_from_command(cmd);
            for suite in &suites {
                if !classified.contains(suite.as_str()) {
                    invalid_refs.push(format!(
                        "{workflow_id}: replay_command references unknown suite '{suite}'"
                    ));
                }
            }
            eprintln!(
                "  [OK] {workflow_id} ({status}): {} suite ref(s) in replay_command",
                suites.len()
            );
        }

        // Check suite_ids reference valid entries
        let suite_ids = row
            .get("suite_ids")
            .or_else(|| row.get("planned_suite_ids"))
            .and_then(|v| v.as_array());
        if let Some(ids) = suite_ids {
            for id in ids {
                if let Some(s) = id.as_str()
                    && !classified.contains(s)
                {
                    invalid_refs.push(format!(
                        "{workflow_id}: suite_id '{s}' not in suite_classification.toml"
                    ));
                }
            }
        }
    }

    eprintln!();
    if invalid_refs.is_empty() {
        eprintln!("  All replay commands reference valid suites.");
    } else {
        for msg in &invalid_refs {
            eprintln!("  [INVALID] {msg}");
        }
    }
    eprintln!();

    assert!(
        invalid_refs.is_empty(),
        "Found {} invalid suite references in scenario matrix replay commands:\n{}",
        invalid_refs.len(),
        invalid_refs.join("\n")
    );
}

/// Validate that all CI gate reproduce_commands reference valid test file stems.
#[test]
fn gate_reproduce_commands_reference_valid_targets() {
    let root = repo_root();
    let classified = all_classified_suites(&root);

    eprintln!("\n=== CI Gate Reproduce Command Validation ===\n");

    // Read the full_suite_verdict.json to get gate definitions
    let verdict_path = root.join("tests/full_suite_gate/full_suite_verdict.json");
    let verdict = load_json(&verdict_path);

    let mut invalid_refs: Vec<String> = Vec::new();
    let mut checked = 0;

    if let Some(v) = &verdict
        && let Some(gates) = v["gates"].as_array()
    {
        for gate in gates {
            let gate_id = gate["id"].as_str().unwrap_or("unknown");
            if let Some(cmd) = gate.get("reproduce_command").and_then(|v| v.as_str()) {
                if cmd.is_empty() {
                    continue;
                }
                checked += 1;

                // Extract test target from cargo test command
                if let Some(target) = extract_cargo_test_target(cmd) {
                    if classified.contains(&target) {
                        eprintln!("  [OK] {gate_id}: target '{target}' is valid");
                    } else {
                        invalid_refs.push(format!(
                            "gate '{gate_id}': reproduce_command references unknown test target '{target}'"
                        ));
                        eprintln!("  [INVALID] {gate_id}: target '{target}' not classified");
                    }
                } else if cmd.contains("python3") || cmd.contains("scripts/") {
                    // Script-based commands are valid by definition
                    eprintln!("  [OK] {gate_id}: script command");
                } else {
                    eprintln!("  [WARN] {gate_id}: could not extract test target from: {cmd}");
                }
            }
        }
    }

    eprintln!();
    eprintln!("  Checked: {checked} reproduce commands");
    if invalid_refs.is_empty() {
        eprintln!("  All reproduce commands reference valid targets.");
    }
    eprintln!();

    assert!(
        invalid_refs.is_empty(),
        "Found {} invalid target references in CI gate reproduce commands:\n{}",
        invalid_refs.len(),
        invalid_refs.join("\n")
    );
}

/// Validate the replay bundle schema: a synthetic bundle must have all
/// required fields and valid structure.
#[test]
fn replay_bundle_schema_validation() {
    eprintln!("\n=== Replay Bundle Schema Validation ===\n");

    let bundle = ReplayBundle {
        schema: "pi.e2e.replay_bundle.v1".to_string(),
        generated_at: "2026-02-13T00:00:00Z".to_string(),
        correlation_id: "test-correlation-001".to_string(),
        source_summary_path: "tests/e2e_results/20260213T000000Z/summary.json".to_string(),
        one_command_replay: "./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/20260213T000000Z/summary.json".to_string(),
        environment: ReplayEnvironment {
            profile: "ci".to_string(),
            shard_kind: "none".to_string(),
            shard_index: None,
            shard_total: None,
            rustc_version: "rustc 1.83.0".to_string(),
            cargo_target_dir: "target".to_string(),
            vcr_mode: "playback".to_string(),
            git_sha: "abc1234".to_string(),
            git_branch: "main".to_string(),
            os: "Linux x86_64".to_string(),
        },
        failed_suites: vec![SuiteReplayEntry {
            suite: "e2e_tui".to_string(),
            exit_code: 1,
            root_cause_class: "assertion_failure".to_string(),
            runner_replay: "./scripts/e2e/run_all.sh --profile focused --skip-lint --suite e2e_tui"
                .to_string(),
            cargo_replay: "cargo test --test e2e_tui -- --nocapture".to_string(),
            targeted_replay: "cargo test --test e2e_tui test_basic_chat -- --nocapture"
                .to_string(),
            digest_path: "tests/e2e_results/20260213T000000Z/e2e_tui/failure_digest.json"
                .to_string(),
        }],
        failed_unit_targets: vec![UnitReplayEntry {
            target: "node_http_shim".to_string(),
            exit_code: 101,
            cargo_replay: "cargo test --test node_http_shim -- --nocapture".to_string(),
        }],
        failed_gates: vec![GateReplayEntry {
            gate_id: "cross_platform".to_string(),
            gate_name: "Cross-platform matrix validation".to_string(),
            reproduce_command:
                "cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture --exact"
                    .to_string(),
        }],
        summary: ReplayBundleSummary {
            total_failed_suites: 1,
            total_failed_units: 1,
            total_failed_gates: 1,
            all_commands_reference_valid_targets: true,
        },
    };

    // Serialize and deserialize to verify round-trip
    let json = serde_json::to_string_pretty(&bundle).expect("bundle must serialize");
    let parsed: ReplayBundle = serde_json::from_str(&json).expect("bundle must deserialize");

    assert_eq!(parsed.schema, "pi.e2e.replay_bundle.v1");
    assert_eq!(parsed.correlation_id, "test-correlation-001");
    assert!(!parsed.one_command_replay.is_empty());
    assert!(!parsed.environment.profile.is_empty());
    assert_eq!(parsed.failed_suites.len(), 1);
    assert_eq!(parsed.failed_unit_targets.len(), 1);
    assert_eq!(parsed.failed_gates.len(), 1);
    assert_eq!(parsed.summary.total_failed_suites, 1);
    assert!(parsed.summary.all_commands_reference_valid_targets);

    // Verify the one_command_replay contains --rerun-from
    assert!(
        parsed.one_command_replay.contains("--rerun-from"),
        "one_command_replay must contain --rerun-from"
    );

    // Verify suite replay entry fields
    let suite = &parsed.failed_suites[0];
    assert_eq!(suite.suite, "e2e_tui");
    assert!(!suite.runner_replay.is_empty());
    assert!(!suite.cargo_replay.is_empty());
    assert!(!suite.digest_path.is_empty());

    eprintln!("  Schema round-trip: OK");
    eprintln!("  Required fields: OK");
    eprintln!("  One-command replay: OK");
    eprintln!();

    // Write the schema example only in the explicit generation lane.
    let artifact_dir = repo_root().join("tests").join("full_suite_gate");
    let artifact_path = artifact_dir.join("replay_bundle_schema_example.json");
    if replay_bundle_artifact_generation_requested() {
        write_non_empty_replay_artifact(&artifact_path, &json);
        eprintln!("  Schema example: {}", artifact_path.display());
    } else {
        eprintln!(
            "  Schema example validated in memory; set {GENERATE_REPLAY_BUNDLE_ARTIFACTS_ENV}=1 to write it"
        );
    }
}

/// Validate that env context restoration is properly captured in replay commands.
/// Replay commands must include profile and other context needed for deterministic replay.
#[test]
fn env_context_in_replay_commands() {
    let root = repo_root();
    let matrix_path = root.join("docs/e2e_scenario_matrix.json");
    let matrix = load_json(&matrix_path).expect("e2e_scenario_matrix.json must exist");

    eprintln!("\n=== Environment Context in Replay Commands ===\n");

    let rows = matrix["rows"].as_array().expect("rows must be an array");
    let mut issues: Vec<String> = Vec::new();

    for row in rows {
        let workflow_id = row["workflow_id"].as_str().unwrap_or("unknown");
        let status = row["status"].as_str().unwrap_or("unknown");

        if let Some(cmd) = row.get("replay_command").and_then(|v| v.as_str()) {
            // Verify runner commands include --profile or --suite for context
            if cmd.contains("run_all.sh") {
                let has_profile = cmd.contains("--profile");
                let has_suite = cmd.contains("--suite");

                if !has_profile && !has_suite {
                    issues.push(format!(
                        "{workflow_id}: runner command missing --profile or --suite context"
                    ));
                }
            }

            eprintln!("  [{status}] {workflow_id}: replay_command has context");
        }
    }

    eprintln!();
    if issues.is_empty() {
        eprintln!("  All replay commands include environment context.");
    } else {
        for msg in &issues {
            eprintln!("  [ISSUE] {msg}");
        }
    }
    eprintln!();

    assert!(
        issues.is_empty(),
        "Found {} replay commands missing environment context:\n{}",
        issues.len(),
        issues.join("\n")
    );
}

/// Validate the failure_digest schema requires all three replay command fields.
/// Reads the evidence contract validation in run_all.sh to confirm enforcement.
#[test]
fn failure_digest_replay_fields_enforced() {
    let root = repo_root();

    eprintln!("\n=== Failure Digest Replay Fields Enforcement ===\n");

    // The evidence contract in run_all.sh validates these fields.
    // We verify here that the schema definition includes all three.
    let run_all_path = root.join("scripts/e2e/run_all.sh");
    let content = std::fs::read_to_string(&run_all_path).expect("run_all.sh must exist");

    let required_fields = [
        "replay_command",
        "suite_replay_command",
        "targeted_test_replay_command",
    ];

    for field in &required_fields {
        let pattern = format!("\"{field}\"");
        let found = content.contains(&pattern);
        eprintln!("  {field}: {}", if found { "enforced" } else { "MISSING" });
        assert!(found, "failure_digest schema must enforce field: {field}");
    }

    // Verify the evidence contract checks these fields
    let contract_checks = content.contains("remediation_pointer");
    eprintln!(
        "  remediation_pointer validation: {}",
        if contract_checks {
            "present"
        } else {
            "MISSING"
        }
    );
    assert!(
        contract_checks,
        "Evidence contract must validate remediation_pointer"
    );

    eprintln!();
    eprintln!("  All three replay command fields are enforced in the evidence contract.");
}

/// Generate a replay bundle from current CI gate state and validate it.
/// This is the main end-to-end validation: read real artifacts and produce
/// a valid replay_bundle.json.
#[test]
fn generate_and_validate_replay_bundle() {
    use chrono::{SecondsFormat, Utc};

    let root = repo_root();
    let report_dir = root.join("tests").join("full_suite_gate");
    let classified = all_classified_suites(&root);

    eprintln!("\n=== Generate and Validate Replay Bundle ===\n");

    // ── Read CI gate verdict for failed gates ──
    let verdict_path = report_dir.join("full_suite_verdict.json");
    let mut failed_gates: Vec<GateReplayEntry> = Vec::new();

    if let Some(v) = load_json(&verdict_path)
        && let Some(gates) = v["gates"].as_array()
    {
        for gate in gates {
            let status = gate["status"].as_str().unwrap_or("pass");
            if status == "fail" {
                let gate_id = gate["id"].as_str().unwrap_or("unknown").to_string();
                let gate_name = gate["name"].as_str().unwrap_or("unknown").to_string();
                let cmd = gate
                    .get("reproduce_command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                failed_gates.push(GateReplayEntry {
                    gate_id,
                    gate_name,
                    reproduce_command: cmd,
                });
            }
        }
    }

    // ── Read preflight verdict for failed blocking gates ──
    let preflight_path = report_dir.join("preflight_verdict.json");
    if let Some(pf) = load_json(&preflight_path)
        && let Some(blocking) = pf["blocking_gates"].as_array()
    {
        for gate in blocking {
            let status = gate["status"].as_str().unwrap_or("pass");
            if status == "fail" {
                let gate_id = gate["id"].as_str().unwrap_or("unknown").to_string();
                // Avoid duplicates
                if !failed_gates.iter().any(|g| g.gate_id == gate_id) {
                    let gate_name = gate["name"].as_str().unwrap_or("unknown").to_string();
                    let cmd = gate
                        .get("reproduce_command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    failed_gates.push(GateReplayEntry {
                        gate_id,
                        gate_name,
                        reproduce_command: cmd,
                    });
                }
            }
        }
    }

    // ── Read scenario matrix for coverage ──
    let matrix_path = root.join("docs/e2e_scenario_matrix.json");
    let matrix = load_json(&matrix_path);
    let covered_workflows: usize =
        matrix
            .as_ref()
            .and_then(|m| m["rows"].as_array())
            .map_or(0, |rows| {
                rows.iter()
                    .filter(|r| r["status"].as_str() == Some("covered"))
                    .count()
            });

    // ── Validate all replay commands reference valid targets ──
    let mut all_valid = true;
    for gate in &failed_gates {
        if !gate.reproduce_command.is_empty()
            && let Some(target) = extract_cargo_test_target(&gate.reproduce_command)
            && !classified.contains(&target)
        {
            eprintln!(
                "  [INVALID] gate '{}' references unknown target '{}'",
                gate.gate_id, target
            );
            all_valid = false;
        }
    }

    // ── Build the replay bundle ──
    let git_sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    let git_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    let rustc_version = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    let bundle = ReplayBundle {
        schema: "pi.e2e.replay_bundle.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        correlation_id: format!("replay-bundle-{}", Utc::now().format("%Y%m%dT%H%M%SZ")),
        source_summary_path: "tests/full_suite_gate/full_suite_verdict.json".to_string(),
        one_command_replay:
            "cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact"
                .to_string(),
        environment: ReplayEnvironment {
            profile: "ci".to_string(),
            shard_kind: "none".to_string(),
            shard_index: None,
            shard_total: None,
            rustc_version,
            cargo_target_dir: std::env::var("CARGO_TARGET_DIR")
                .unwrap_or_else(|_| "target".to_string()),
            vcr_mode: std::env::var("VCR_MODE").unwrap_or_else(|_| "unset".to_string()),
            git_sha,
            git_branch,
            os: std::env::consts::OS.to_string(),
        },
        failed_suites: Vec::new(), // No E2E run in this context
        failed_unit_targets: Vec::new(),
        failed_gates: failed_gates.clone(),
        summary: ReplayBundleSummary {
            total_failed_suites: 0,
            total_failed_units: 0,
            total_failed_gates: failed_gates.len(),
            all_commands_reference_valid_targets: all_valid,
        },
    };

    // ── Write the replay bundle ──
    let bundle_path = report_dir.join("replay_bundle.json");
    let json = serde_json::to_string_pretty(&bundle).expect("bundle must serialize");
    assert!(
        !json.trim().is_empty(),
        "generated replay bundle must be non-empty"
    );
    let generate = replay_bundle_artifact_generation_requested();
    if generate {
        write_non_empty_replay_artifact(&bundle_path, &json);
    }

    eprintln!("  Failed gates: {}", failed_gates.len());
    eprintln!("  Covered workflows: {covered_workflows}");
    eprintln!("  All commands valid: {all_valid}");
    if generate {
        eprintln!("  Bundle: {}", bundle_path.display());
    } else {
        eprintln!(
            "  Bundle validated in memory; set {GENERATE_REPLAY_BUNDLE_ARTIFACTS_ENV}=1 to write it"
        );
    }
    eprintln!();

    // ── Verify the bundle is valid ──
    let reloaded: ReplayBundle =
        serde_json::from_str(&json).expect("written bundle must be valid JSON");
    assert_eq!(reloaded.schema, "pi.e2e.replay_bundle.v1");
    assert!(!reloaded.generated_at.is_empty());
    assert!(!reloaded.correlation_id.is_empty());
    assert!(!reloaded.environment.rustc_version.is_empty());
    assert!(!reloaded.environment.os.is_empty());

    // Every failed gate must have a non-empty reproduce command
    for gate in &reloaded.failed_gates {
        assert!(
            !gate.reproduce_command.is_empty(),
            "gate '{}' has empty reproduce_command",
            gate.gate_id
        );
    }
}

#[test]
fn replay_bundle_artifact_generation_requires_exact_one() {
    assert!(replay_bundle_artifact_generation_requested_from(Some("1")));
    for raw in [None, Some(""), Some("0"), Some("true"), Some(" 1 ")] {
        assert!(
            !replay_bundle_artifact_generation_requested_from(raw),
            "unexpected generation request for {raw:?}"
        );
    }
}

/// Validate that the --rerun-from mechanism in run_all.sh reads the correct
/// fields from summary.json for deterministic replay.
#[test]
fn rerun_from_reads_failed_names() {
    let root = repo_root();

    eprintln!("\n=== --rerun-from Mechanism Validation ===\n");

    let run_all_path = root.join("scripts/e2e/run_all.sh");
    let content = std::fs::read_to_string(&run_all_path).expect("run_all.sh must exist");

    // Verify the rerun-from mechanism reads failed_names
    assert!(
        content.contains("failed_names"),
        "run_all.sh --rerun-from must read failed_names from summary.json"
    );

    // Verify it supports the --rerun-from flag
    assert!(
        content.contains("--rerun-from"),
        "run_all.sh must support --rerun-from flag"
    );

    // Verify it sets SELECTED_SUITES from rerun list
    assert!(
        content.contains("SELECTED_SUITES"),
        "run_all.sh must set SELECTED_SUITES from rerun list"
    );

    // Verify diff baseline is auto-set from rerun source
    assert!(
        content.contains("DIFF_FROM=\"$RERUN_FROM\""),
        "run_all.sh should auto-set DIFF_FROM from RERUN_FROM for triage diff"
    );

    eprintln!("  --rerun-from flag: present");
    eprintln!("  failed_names field read: confirmed");
    eprintln!("  SELECTED_SUITES wiring: confirmed");
    eprintln!("  Auto-diff baseline: confirmed");
    eprintln!();
}

/// Validate that the triage_diff output includes replay commands.
#[test]
fn triage_diff_includes_replay_metadata() {
    let root = repo_root();

    eprintln!("\n=== Triage Diff Replay Metadata ===\n");

    let run_all_path = root.join("scripts/e2e/run_all.sh");
    let content = std::fs::read_to_string(&run_all_path).expect("run_all.sh must exist");

    // Verify triage_diff includes recommended commands
    let has_runner_repro = content.contains("runner_repro_command");
    let has_target_commands = content.contains("target_commands");
    let has_ranked_repro = content.contains("ranked_repro_commands");
    let has_semantic_diffs = content.contains("semantic_diffs");
    let has_mirrored_scenarios = content.contains("mirrored_scenarios");
    let has_semantic_schema = content.contains("pi.e2e.semantic_diff.v1");
    let has_mirrored_schema = content.contains("pi.e2e.mirrored_scenarios.v1");
    let has_semantic_focus = content.contains("semantic_focus_commands");

    eprintln!(
        "  runner_repro_command: {}",
        if has_runner_repro {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  target_commands: {}",
        if has_target_commands {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  ranked_repro_commands: {}",
        if has_ranked_repro {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  semantic_diffs: {}",
        if has_semantic_diffs {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  mirrored_scenarios: {}",
        if has_mirrored_scenarios {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  semantic_diff schema: {}",
        if has_semantic_schema {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  mirrored_scenarios schema: {}",
        if has_mirrored_schema {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  semantic_focus_commands: {}",
        if has_semantic_focus {
            "present"
        } else {
            "MISSING"
        }
    );

    assert!(
        has_runner_repro,
        "triage_diff must include runner_repro_command"
    );
    assert!(
        has_target_commands,
        "triage_diff must include target_commands"
    );
    assert!(
        has_ranked_repro,
        "triage_diff must include ranked_repro_commands"
    );
    assert!(
        has_semantic_diffs,
        "triage_diff must include semantic_diffs"
    );
    assert!(
        has_mirrored_scenarios,
        "triage_diff must include mirrored_scenarios"
    );
    assert!(
        has_semantic_schema,
        "triage_diff semantic_diffs must declare pi.e2e.semantic_diff.v1 schema"
    );
    assert!(
        has_mirrored_schema,
        "triage_diff mirrored_scenarios must declare pi.e2e.mirrored_scenarios.v1 schema"
    );
    assert!(
        has_semantic_focus,
        "triage_diff must include semantic_focus_commands"
    );

    // Verify triage_diff is written to summary.json
    let has_triage_in_summary = content.contains("triage_diff");
    eprintln!(
        "  triage_diff in summary.json: {}",
        if has_triage_in_summary {
            "present"
        } else {
            "MISSING"
        }
    );
    assert!(
        has_triage_in_summary,
        "triage_diff must be included in summary.json"
    );

    eprintln!();
}

/// Validate that release readiness summary includes replay-related metadata.
#[test]
fn release_readiness_includes_replay_context() {
    let root = repo_root();

    eprintln!("\n=== Release Readiness Replay Context ===\n");

    let run_all_path = root.join("scripts/e2e/run_all.sh");
    let content = std::fs::read_to_string(&run_all_path).expect("run_all.sh must exist");

    // Release readiness should reference failure diagnostics
    let has_failure_diag = content.contains("failure_diagnostics");
    // Release readiness should be generated
    let has_readiness = content.contains("release_readiness_summary");

    eprintln!(
        "  failure_diagnostics reference: {}",
        if has_failure_diag {
            "present"
        } else {
            "MISSING"
        }
    );
    eprintln!(
        "  release_readiness_summary: {}",
        if has_readiness { "present" } else { "MISSING" }
    );

    assert!(
        has_failure_diag,
        "release readiness must reference failure diagnostics"
    );
    assert!(has_readiness, "release readiness summary must be generated");

    eprintln!();
}

/// Cross-validate that every E2E suite in the scenario matrix has a corresponding
/// entry in suite_classification.toml, and that its test file actually exists.
#[test]
fn e2e_suite_test_files_exist() {
    let root = repo_root();
    let matrix_path = root.join("docs/e2e_scenario_matrix.json");
    let matrix = load_json(&matrix_path).expect("e2e_scenario_matrix.json must exist");
    let classified = all_classified_suites(&root);

    eprintln!("\n=== E2E Suite Test File Existence ===\n");

    let rows = matrix["rows"].as_array().expect("rows must be an array");
    let mut missing_files: Vec<String> = Vec::new();
    let mut missing_classification: Vec<String> = Vec::new();

    for row in rows {
        let workflow_id = row["workflow_id"].as_str().unwrap_or("unknown");
        let suite_ids = row
            .get("suite_ids")
            .or_else(|| row.get("planned_suite_ids"))
            .and_then(|v| v.as_array());

        if let Some(ids) = suite_ids {
            for id in ids {
                if let Some(s) = id.as_str() {
                    // Check classification
                    if !classified.contains(s) {
                        missing_classification.push(format!("{workflow_id}: {s}"));
                    }

                    // Check test file exists
                    let test_path = root.join("tests").join(format!("{s}.rs"));
                    if !test_path.exists() {
                        missing_files.push(format!("{workflow_id}: tests/{s}.rs"));
                    }
                }
            }
        }
    }

    eprintln!("  Missing test files: {}", missing_files.len());
    for f in &missing_files {
        eprintln!("    {f}");
    }
    eprintln!(
        "  Missing classifications: {}",
        missing_classification.len()
    );
    for c in &missing_classification {
        eprintln!("    {c}");
    }
    eprintln!();

    assert!(
        missing_files.is_empty(),
        "Missing test files for scenario matrix suites:\n{}",
        missing_files.join("\n")
    );
    assert!(
        missing_classification.is_empty(),
        "Missing suite_classification entries for scenario matrix suites:\n{}",
        missing_classification.join("\n")
    );
}
