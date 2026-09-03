//! Integration tests for slash command differential parity.
//!
//! This test suite tracks slash-command differential coverage and fails closed
//! until the real pi-mono/Rust Pi RPC runner is wired.

#[path = "dropin_slash_differential/mod.rs"]
mod dropin_slash_differential;
use dropin_slash_differential::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn load_repo_json(relative: &str) -> Result<Value, String> {
    let path = repo_path(relative);
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn string_field<'a>(value: &'a Value, field: &str, label: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{label} must contain string field {field}"))
}

fn find_json_entry<'a>(
    entries: &'a [Value],
    field: &str,
    expected: &str,
    label: &str,
) -> Result<&'a Value, String> {
    entries
        .iter()
        .find(|entry| entry.get(field).and_then(Value::as_str) == Some(expected))
        .ok_or_else(|| format!("{label} must contain {field}={expected}"))
}

fn result_has_execution_gap(result: &TestResult) -> bool {
    result.differences.iter().any(|diff| {
        diff.contains("unavailable")
            || diff.contains("not observable through the shared RPC protocol")
            || diff.contains("not credential-free")
            || diff.contains("not implemented")
    })
}

fn assert_runner_fail_closed(scenario: &SlashCommandScenario, result: &TestResult) {
    assert!(
        !result.success,
        "scenario '{}' must not report synthetic differential success",
        scenario.name
    );
    assert_eq!(result.scenario_name, scenario.name);
    assert_eq!(result.rust_response["status"], "blocked");
    assert_eq!(result.pi_mono_response["status"], "blocked");
    assert_eq!(result.rust_response["command"], scenario.command);
    assert_eq!(result.pi_mono_response["command"], scenario.command);
    assert!(
        result_has_execution_gap(result),
        "scenario '{}' should explain that the real runner could not produce pass evidence",
        scenario.name
    );
}

fn assert_real_mirrored_result(scenario_name: &str, result: &TestResult) {
    assert_eq!(result.scenario_name, scenario_name);
    assert!(
        result.success,
        "scenario '{scenario_name}' must produce mirrored RPC pass evidence; differences={:?}; rust={}; pi_mono={}",
        result.differences, result.rust_response, result.pi_mono_response
    );
    assert_eq!(
        result.rust_response, result.pi_mono_response,
        "successful scenario '{scenario_name}' must compare mirrored canonical RPC output"
    );
    assert_ne!(
        result.rust_response["status"], "blocked",
        "scenario '{scenario_name}' must not count a blocked placeholder as pass evidence"
    );
    assert!(
        result.differences.is_empty(),
        "successful scenario '{scenario_name}' should not report differences"
    );
}

/// The harness must distinguish real mirrored parity from fail-closed policy exclusions.
///
/// Needs the legacy pi-mono tsx runner (`legacy_pi_mono_code/pi-mono/node_modules`),
/// which is git-ignored and never synced to the DSR workers; the drop-in parity
/// program is retired (AGENTS.md "Historical Drop-In Ledger"), so the gate does
/// not carry it. Run with `--ignored` on a host that provisioned pi-mono.
#[test]
#[ignore = "retired drop-in parity program: needs the legacy pi-mono tsx runner (bd-werhk)"]
fn test_slash_command_differential_harness_fails_closed_without_mirrored_success()
-> Result<(), String> {
    let tester = DifferentialTester::new()
        .map_err(|err| format!("failed to create differential tester: {err:?}"))?;
    let scenarios: BTreeMap<String, SlashCommandScenario> = tester
        .scenarios
        .iter()
        .map(|scenario| (scenario.name.clone(), scenario.clone()))
        .collect();

    let results = tester.run_all_scenarios();
    assert!(!results.is_empty(), "expected slash command scenarios");

    let mut pass_evidence_scenarios = 0usize;
    for (scenario_name, result) in &results {
        let Some(scenario) = scenarios.get(scenario_name) else {
            continue;
        };
        if scenario_requires_pass_evidence(scenario) {
            pass_evidence_scenarios += 1;
            assert_real_mirrored_result(scenario_name, result);
        } else {
            assert_runner_fail_closed(scenario, result);
        }
    }

    assert!(
        pass_evidence_scenarios > 0,
        "expected at least one RPC-observable pass-evidence scenario"
    );

    Ok(())
}

/// Release evidence must not certify slash-command parity until every scenario has real pass evidence.
#[test]
#[ignore = "retired drop-in parity program: needs the legacy pi-mono tsx runner (bd-werhk)"]
fn test_certification_artifacts_fail_closed_until_full_runner_pass() -> Result<(), String> {
    let tester = DifferentialTester::new()
        .map_err(|err| format!("failed to create differential tester: {err:?}"))?;
    let results = tester.run_all_scenarios();
    if results.is_empty() {
        return Err("expected slash command scenarios".to_owned());
    }

    let scenarios: BTreeMap<String, SlashCommandScenario> = tester
        .scenarios
        .iter()
        .map(|scenario| (scenario.name.clone(), scenario.clone()))
        .collect();
    let mut pass_evidence_results = Vec::new();
    for (scenario_name, result) in &results {
        if let Some(scenario) = scenarios.get(scenario_name)
            && scenario_requires_pass_evidence(scenario)
        {
            pass_evidence_results.push(result);
        }
    }
    if pass_evidence_results.is_empty() {
        return Err("expected RPC-observable slash command scenarios".to_owned());
    }

    let all_pass_evidence_success = pass_evidence_results.iter().all(|result| result.success);
    if all_pass_evidence_success {
        return Ok(());
    }

    let suite = load_repo_json("docs/evidence/dropin-differential-evidence-suite.json")?;
    let suite_status = string_field(&suite, "overall_status", "differential suite")?;
    if suite_status == "pass" {
        return Err(
            "G10 evidence suite must not pass before slash differential scenarios all pass"
                .to_owned(),
        );
    }
    let components = suite
        .get("component_evidence")
        .and_then(Value::as_array)
        .ok_or_else(|| "differential suite must contain component_evidence".to_owned())?;
    let slash_component = find_json_entry(
        components,
        "component",
        "slash_command_differential",
        "G04 evidence",
    )?;
    let slash_component_status = string_field(slash_component, "status", "slash component")?;
    if slash_component_status == "pass" {
        return Err(
            "G04 slash_command_differential must not pass before slash scenarios all pass"
                .to_owned(),
        );
    }

    let ledger = load_repo_json("docs/evidence/dropin-parity-gap-ledger.json")?;
    let ledger_entries = ledger
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "gap ledger must contain entries".to_owned())?;
    let slash_gap = find_json_entry(
        ledger_entries,
        "gap_id",
        "gap-cli-slash-command-surface",
        "gap ledger",
    )?;
    let gap_status = string_field(slash_gap, "status", "slash gap")?;
    if !matches!(gap_status, "open" | "in_progress") {
        return Err(format!(
            "slash gap must be active until slash scenarios all pass, found status={gap_status}"
        ));
    }
    let gap_severity = string_field(slash_gap, "severity", "slash gap")?;
    if !matches!(gap_severity, "critical" | "high") {
        return Err(format!(
            "slash gap must block release claims while runner is fail-closed, found severity={gap_severity}"
        ));
    }

    let verdict = load_repo_json("docs/evidence/dropin-certification-verdict.json")?;
    let overall_verdict = string_field(&verdict, "overall_verdict", "drop-in verdict")?;
    if overall_verdict == "CERTIFIED" {
        return Err(
            "strict drop-in verdict must not be CERTIFIED before slash scenarios all pass"
                .to_owned(),
        );
    }
    let blocking_reasons = verdict
        .get("blocking_reasons")
        .and_then(Value::as_array)
        .ok_or_else(|| "drop-in verdict must contain blocking_reasons".to_owned())?;
    if !blocking_reasons.iter().any(|reason| {
        reason
            .as_str()
            .is_some_and(|text| text.contains("gap-cli-slash-command-surface"))
    }) {
        return Err(
            "drop-in verdict must name gap-cli-slash-command-surface as a blocking reason"
                .to_owned(),
        );
    }

    Ok(())
}

/// Test that basic slash command parsing works correctly.
#[test]
fn test_slash_command_parsing() {
    // Verify that our test scenarios cover the actual slash commands
    // supported by the Rust implementation
    let tester = DifferentialTester::new().expect("Failed to create tester");

    // Check that we have test scenarios for core commands
    let scenario_commands: Vec<String> =
        tester.scenarios.iter().map(|s| s.command.clone()).collect();

    // Verify coverage of essential commands
    let essential_commands = vec![
        "/help",
        "/h",
        "/?",
        "/clear",
        "/cls",
        "/model",
        "/m",
        "/thinking",
        "/t",
        "/exit",
        "/quit",
        "/q",
        "/session",
        "/info",
        "/tree",
        "/compact",
    ];

    for essential in essential_commands {
        assert!(
            scenario_commands
                .iter()
                .any(|cmd| cmd.starts_with(essential)),
            "Missing test scenario for essential command: {essential}"
        );
    }
}

/// Every scenario input must carry an explicit pass-evidence observability policy.
#[test]
fn test_slash_command_scenarios_have_observability_policy() -> Result<(), String> {
    let tester = DifferentialTester::new()
        .map_err(|err| format!("failed to create differential tester: {err:?}"))?;
    let mut policy_counts = BTreeMap::new();

    for scenario in &tester.scenarios {
        let policy = slash_command_observability(&scenario.command);
        assert!(
            !policy.reason.is_empty(),
            "scenario {} command {} must explain its observability policy",
            scenario.name,
            scenario.command
        );
        *policy_counts.entry(policy.kind).or_insert(0usize) += 1;

        for setup in &scenario.setup {
            let setup_policy = slash_command_observability(setup);
            assert!(
                !setup_policy.reason.is_empty(),
                "scenario {} setup {setup} must explain its observability policy",
                scenario.name
            );
            *policy_counts.entry(setup_policy.kind).or_insert(0usize) += 1;
        }
    }

    assert!(
        policy_counts.contains_key(&SlashCommandObservability::RpcObservable),
        "slash differential policy inventory should include RPC-observable commands"
    );
    assert!(
        policy_counts.contains_key(&SlashCommandObservability::UiOnlyNeedsAdapter),
        "slash differential policy inventory should name UI-only commands needing adapters"
    );
    assert!(
        policy_counts.contains_key(&SlashCommandObservability::ExcludedFromPassEvidence),
        "slash differential policy inventory should name excluded non-credential-free inputs"
    );

    Ok(())
}

/// Test response canonicalization functionality.
#[test]
fn test_response_canonicalization() {
    use serde_json::json;

    let test_response = json!({
        "status": "success",
        "timestamp": "2024-04-22T17:49:00Z",
        "id": "req-test-123",
        "duration": 150,
        "path": "/tmp/test-session",
        "data": {
            "message": "Command executed",
            "nested_timestamp": "2024-04-22T17:49:01Z",
            "tokens": 42
        }
    });

    let canonicalized = canonicalize_response(test_response);

    // Non-deterministic fields should be removed
    assert!(canonicalized.get("timestamp").is_none());
    assert!(canonicalized.get("id").is_none());
    assert!(canonicalized.get("duration").is_none());
    assert!(canonicalized["data"].get("nested_timestamp").is_none());

    // Deterministic fields should be preserved
    assert_eq!(canonicalized["status"], "success");
    assert_eq!(canonicalized["data"]["message"], "Command executed");
    assert_eq!(canonicalized["data"]["tokens"], 42);
}

/// Test combinatorial slash command scenarios.
#[test]
#[ignore = "retired drop-in parity program: needs the legacy pi-mono tsx runner (bd-werhk)"]
fn test_combinatorial_slash_commands() {
    let mut tester = DifferentialTester::new().expect("Failed to create tester");

    // Add combinatorial test scenarios
    tester.add_scenario(SlashCommandScenario {
        name: "model_then_thinking".to_string(),
        command: "/thinking high".to_string(),
        description: "Set thinking level after potential model change".to_string(),
        supports_streaming: false,
        setup: vec!["/model".to_string()], // First show model selector
    });

    tester.add_scenario(SlashCommandScenario {
        name: "clear_then_help".to_string(),
        command: "/help".to_string(),
        description: "Help command should work after clearing history".to_string(),
        supports_streaming: false,
        setup: vec!["some conversation".to_string(), "/clear".to_string()],
    });

    tester.add_scenario(SlashCommandScenario {
        name: "multiple_thinking_changes".to_string(),
        command: "/thinking off".to_string(),
        description: "Multiple thinking level changes should work".to_string(),
        supports_streaming: false,
        setup: vec!["/thinking high".to_string(), "/thinking medium".to_string()],
    });

    // Run just the combinatorial scenarios
    let combinatorial_scenarios: Vec<_> = tester
        .scenarios
        .iter()
        .filter(|s| {
            s.name.contains("model_then_")
                || s.name.contains("clear_then_")
                || s.name.contains("multiple_")
        })
        .cloned()
        .collect();

    for scenario in combinatorial_scenarios {
        let result = DifferentialTester::run_scenario(&scenario);
        if scenario_requires_pass_evidence(&scenario) {
            assert_real_mirrored_result(&scenario.name, &result);
        } else {
            assert_runner_fail_closed(&scenario, &result);
        }
    }
}

/// Test error handling for invalid slash commands.
#[test]
fn test_invalid_slash_command_handling() {
    let mut tester = DifferentialTester::new().expect("Failed to create tester");

    // Add invalid command scenarios
    let invalid_scenarios = vec![
        SlashCommandScenario {
            name: "invalid_command".to_string(),
            command: "/nonexistent".to_string(),
            description: "Invalid slash command should be handled gracefully".to_string(),
            supports_streaming: false,
            setup: vec![],
        },
        SlashCommandScenario {
            name: "malformed_thinking".to_string(),
            command: "/thinking invalid_level".to_string(),
            description: "Invalid thinking level should show error".to_string(),
            supports_streaming: false,
            setup: vec![],
        },
        SlashCommandScenario {
            name: "empty_slash".to_string(),
            command: "/".to_string(),
            description: "Empty slash command should be handled".to_string(),
            supports_streaming: false,
            setup: vec![],
        },
    ];

    for scenario in invalid_scenarios {
        tester.add_scenario(scenario.clone());
        let result = DifferentialTester::run_scenario(&scenario);
        if scenario_requires_pass_evidence(&scenario) {
            assert_real_mirrored_result(&scenario.name, &result);
        } else {
            assert_runner_fail_closed(&scenario, &result);
        }
    }
}
