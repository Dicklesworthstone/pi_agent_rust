#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use serde_json::Value;
use std::fs;
use std::process::Command;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_dropin_certification_contract_schema_and_gates() {
    let harness = TestHarness::new("dropin_contract_schema");
    let contract_path = "docs/contracts/dropin-certification-contract.json";
    assert!(
        std::path::Path::new(contract_path).is_file(),
        "dropin certification contract must exist"
    );

    let raw = fs::read_to_string(contract_path).expect("read dropin contract");
    let parsed: Value = serde_json::from_str(&raw).expect("parse dropin contract JSON");

    assert_eq!(parsed["schema"], "pi.dropin.certification_contract.v1");
    assert_eq!(parsed["status"], "active_blocking_policy");

    let hard_gates = parsed["hard_gates"]
        .as_array()
        .expect("hard_gates must be an array");
    assert!(
        hard_gates.len() >= 10,
        "must define comprehensive hard gates"
    );

    // Verify key hard gates exist
    let gate_ids: Vec<&str> = hard_gates
        .iter()
        .filter_map(|g| g["gate_id"].as_str())
        .collect();

    assert!(gate_ids.contains(&"G01-baseline-freeze"));
    assert!(gate_ids.contains(&"G02-feature-inventory-complete"));

    finish_case(&harness, "dropin_contract_schema");
}

#[test]
fn test_clean_release_commit_script_execution() {
    let harness = TestHarness::new("clean_release_commit_script");

    let script_path = "scripts/check_clean_release_commit.py";
    assert!(
        std::path::Path::new(script_path).is_file(),
        "check_clean_release_commit.py script must exist"
    );

    let output = Command::new("python3")
        .args([script_path, "HEAD~1", "HEAD", "--json"])
        .output()
        .expect("execute check_clean_release_commit.py");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.is_empty(),
        "stdout must contain json verdict: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: Value = serde_json::from_str(&stdout).expect("parse script JSON output");
    assert_eq!(parsed["schema"], "pi.release.clean_commit_check.v1");
    assert!(parsed["is_clean_release_commit"].is_boolean());

    finish_case(&harness, "clean_release_commit_script");
}
