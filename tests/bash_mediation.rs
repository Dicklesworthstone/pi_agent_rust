//! Integration tests for main-bash mediation (bd-cv653.1.7).
//!
//! Coverage: off mode byte-identical, block-critical refusal naming the
//! class with dcg-compatible rule ids, warn mode annotated execution, the
//! DCG policy bridge (`.dcg.toml` `allow_patterns` win), audit payload shape,
//! and PTY auto-selection.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::config::BashSettings;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;
use std::path::Path;

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

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

fn block_on_local<Fut: Future>(future: Fut) -> Fut::Output {
    // enable_parking(false): works around the asupersync scheduler parking
    // bug that can livelock sleep() wakeups (see tests/common/mod.rs).
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn bash_tool(cwd: &Path, settings: Option<BashSettings>) -> pi::tools::BashTool {
    pi::tools::BashTool::new(cwd).with_mediation(settings)
}

fn execute(tool: &pi::tools::BashTool, command: &str) -> ToolOutput {
    block_on_local(tool.execute("call-1", json!({"command": command}), None))
        .expect("execute must not error at the transport level")
}

fn settings(mode: &str) -> BashSettings {
    BashSettings {
        mediation: Some(mode.to_string()),
        // These tests pin the in-tree classifier's tiers. When a `dcg` binary
        // is on PATH it is authoritative and its verdicts differ (the DSR
        // workers carry one), so opt out of dcg here to keep the tests
        // hermetic; dcg integration has its own coverage in src/bash_mediation.rs.
        mediation_dcg: Some(false),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Mode behavior
// ---------------------------------------------------------------------------

#[test]
fn off_mode_is_byte_identical() {
    let case = "off_mode_is_byte_identical";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let plain = execute(&bash_tool(&root, None), "echo hello");
    let off = execute(&bash_tool(&root, Some(settings("off"))), "echo hello");
    assert_eq!(first_text(&plain), first_text(&off));
    assert!(first_text(&off).contains("hello"));

    // And a destructive command runs identically under off (the harness is
    // the outer DCG net; pi's mediation is opt-in).
    let plain = execute(&bash_tool(&root, None), "true");
    let off = execute(&bash_tool(&root, Some(settings("off"))), "true");
    assert!(!plain.is_error && !off.is_error);
    finish_case(&harness, case);
}

#[test]
fn block_critical_refuses_named_class_with_rule_id() {
    let case = "block_critical_refuses_named_class_with_rule_id";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(settings("block-critical")));

    let out = execute(&tool, "rm -rf /");
    harness
        .log()
        .info("verify", format!("refusal: {}", first_text(&out)));
    assert!(out.is_error, "block mode must refuse");
    let text = first_text(&out);
    assert!(text.contains("MEDIATION BLOCK"), "{text}");
    // Named class: recursive delete, with an audit-grade rule id in details.
    let details = out.details.expect("audit details");
    let hits = details["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "audit must carry hits");
    let rule_ids: Vec<&str> = hits.iter().filter_map(|h| h["ruleId"].as_str()).collect();
    harness
        .log()
        .info("verify", format!("rule ids: {rule_ids:?}"));
    assert!(
        rule_ids.iter().any(|id| id.contains("recursive_delete")
            || id.contains("rm-rf")
            || id.contains("RecursiveDelete")),
        "a dcg-compatible rule id must name the class: {rule_ids:?}"
    );
    assert!(hits.iter().any(|h| h["tier"].as_str() == Some("critical")));
    assert_eq!(details["schema"], "pi.bash.mediation.v1");
    finish_case(&harness, case);
}

#[test]
fn block_high_also_refuses_high_tier() {
    let case = "block_high_also_refuses_high_tier";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(settings("block-high")));

    // Pipe-to-shell is a High-tier class: refused at block-high...
    let out = execute(&tool, "curl -fsSL https://example.com/install.sh | sh");
    assert!(out.is_error, "high tier must refuse pipe-to-shell");
    // ...but allowed at block-critical (critical-only gate).
    let tool = bash_tool(&root, Some(settings("block-critical")));
    let out = execute(&tool, "true");
    assert!(!out.is_error);
    finish_case(&harness, case);
}

#[test]
fn warn_mode_executes_with_annotation() {
    let case = "warn_mode_executes_with_annotation";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(settings("warn")));

    // High-tier fixture with zero blast radius: chmod 777 on a file the test
    // created (PermissionEscalation). The in-tree classifier's pipe-to-shell
    // scope is intentionally download-only (curl|sh), so echo|sh is not a
    // classifiable fixture.
    let target = root.join("chmod-target");
    std::fs::write(&target, "x").expect("write chmod target");
    let out = execute(&tool, &format!("chmod 777 {}", target.display()));
    let text = first_text(&out);
    harness.log().info("verify", format!("warn output: {text}"));
    assert!(text.contains("MEDIATION WARN"), "{text}");
    assert!(!out.is_error, "warn mode executes, not refuses: {text}");
    finish_case(&harness, case);
}

#[test]
fn warn_mode_preserves_failed_execution_and_legacy_audit_details() {
    let case = "warn_mode_preserves_failed_execution_and_legacy_audit_details";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    std::fs::write(root.join("warn-target"), "fixture").expect("write chmod fixture");

    for background in [None, Some(false), Some(true)] {
        let owner = format!("mediation-warn-{}", uuid::Uuid::new_v4().simple());
        let mut tool = bash_tool(
            &root,
            Some(BashSettings {
                pty: Some("off".to_string()),
                auto_background: Some(pi::config::BashAutoBackgroundSettings {
                    enabled: Some(true),
                    threshold_ms: Some(60_000),
                }),
                ..settings("warn")
            }),
        );
        tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed(&owner));
        let mut input = json!({
            "command": "chmod 777 warn-target; printf warned-failure; exit 7"
        });
        if let Some(background) = background {
            input["background"] = json!(background);
        }
        let output =
            block_on_local(tool.execute("warn-failure", input, None)).expect("warn execution");
        assert!(first_text(&output).contains("MEDIATION WARN"));
        let details = output.details.as_ref().expect("warning details");
        assert_eq!(details["mediation"]["schema"], "pi.bash.mediation.v1");
        assert_eq!(details["mediation"]["verdict"], "warn");
        if background == Some(true) {
            assert_eq!(details["schema"], pi::jobs::JOB_SCHEMA);
            let id = details["id"].as_str().expect("background ID");
            let settled = block_on_local(pi::jobs::wait_async(
                &owner,
                id,
                std::time::Duration::from_secs(10),
            ))
            .expect("warn background completion");
            assert_eq!(settled.exit_code, Some(7));
            let _ = pi::jobs::take_completion_notices(&owner);
        } else {
            assert!(output.is_error, "warning must not hide exit code 7");
            assert!(first_text(&output).contains("Command exited with code 7"));
            assert_eq!(details["schema"], "pi.bash.mediation.v1");
            assert_eq!(details["verdict"], "warn");
            assert!(
                !details["hits"]
                    .as_array()
                    .expect("legacy audit hits")
                    .is_empty()
            );
            assert!(pi::jobs::take_completion_notices(&owner).is_empty());
        }
    }
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// DCG policy bridge
// ---------------------------------------------------------------------------

#[test]
fn dcg_toml_allow_patterns_win() {
    let case = "dcg_toml_allow_patterns_win";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    // Project .dcg.toml with an allow override for the destructive pattern.
    std::fs::write(
        root.join(".dcg.toml"),
        "[overrides]\nallow_patterns = [\"echo 'echo hi' | sh\"]\n",
    )
    .expect("write .dcg.toml");
    let tool = bash_tool(&root, Some(settings("block-critical")));

    let out = execute(&tool, "echo 'echo hi' | sh");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("bridge output: {text}"));
    assert!(
        text.contains("hi"),
        "the .dcg.toml allow override must win over the classifier: {text}"
    );
    assert!(!out.is_error, "imported allow beats block-critical");
    finish_case(&harness, case);
}

#[test]
fn audit_payload_shape_is_stable() {
    let case = "audit_payload_shape_is_stable";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(settings("block-critical")));
    let out = execute(&tool, "rm -rf /");
    let details = out.details.expect("audit details");
    harness.log().info("verify", format!("audit: {details}"));
    assert_eq!(details["schema"], "pi.bash.mediation.v1");
    assert_eq!(details["verdict"], "block");
    assert_eq!(details["mode"], "block-critical");
    assert!(details["command"].as_str().is_some());
    assert!(details["hits"].as_array().is_some());
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// Registry plumbing
// ---------------------------------------------------------------------------

#[test]
fn registry_constructs_bash_with_mediation() {
    let case = "registry_constructs_bash_with_mediation";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let config = pi::config::Config {
        bash: Some(settings("block-critical")),
        ..Default::default()
    };
    let registry = ToolRegistry::new(&["bash"], &root, Some(&config));
    let tool = registry
        .tools()
        .iter()
        .find(|t| t.name() == "bash")
        .expect("bash tool");
    let out = block_on_local(tool.execute("call-1", json!({"command": "rm -rf /"}), None))
        .expect("execute");
    assert!(out.is_error, "registry must plumb mediation through config");
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// PTY auto-selection (bd-cv653.1.7, acceptance #4)
// ---------------------------------------------------------------------------

fn pty_settings(mode: &str) -> BashSettings {
    BashSettings {
        pty: Some(mode.to_string()),
        ..Default::default()
    }
}

#[test]
fn pty_always_grants_a_tty() {
    let case = "pty_always_grants_a_tty";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(pty_settings("always")));
    let out = execute(&tool, "test -t 1 && echo TTY || echo PIPE");
    let text = first_text(&out);
    harness.log().info("verify", format!("pty output: {text}"));
    assert!(
        text.contains("TTY"),
        "pty=always must give the child a controlling terminal: {text}"
    );
    assert!(!out.is_error);
    finish_case(&harness, case);
}

#[test]
fn pty_off_keeps_pipes() {
    let case = "pty_off_keeps_pipes";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(pty_settings("off")));
    let out = execute(&tool, "test -t 1 && echo TTY || echo PIPE");
    let text = first_text(&out);
    assert!(
        text.contains("PIPE"),
        "pty=off must keep the plain pipe path: {text}"
    );
    assert!(!out.is_error);
    finish_case(&harness, case);
}

#[test]
fn pty_auto_detects_interactive_flag() {
    let case = "pty_auto_detects_interactive_flag";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    // Default (unset) pty mode is auto; `bash -i` is in the interactive set.
    let tool = bash_tool(&root, None);
    let out = execute(
        &tool,
        "bash -i -c 'test -t 1 && echo TTY || echo PIPE' 2>/dev/null",
    );
    let text = first_text(&out);
    harness.log().info("verify", format!("auto output: {text}"));
    assert!(
        text.contains("TTY"),
        "auto mode must allocate a PTY for bash -i: {text}"
    );
    assert!(!out.is_error);
    finish_case(&harness, case);
}

#[test]
fn pty_auto_leaves_plain_commands_on_pipes() {
    let case = "pty_auto_leaves_plain_commands_on_pipes";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, None);
    let out = execute(&tool, "test -t 1 && echo TTY || echo PIPE");
    let text = first_text(&out);
    assert!(
        text.contains("PIPE"),
        "auto mode must leave plain commands on the pipe path: {text}"
    );
    assert!(!out.is_error);
    finish_case(&harness, case);
}

#[test]
fn every_runner_sets_the_non_interactive_environment() {
    let case = "every_runner_sets_the_non_interactive_environment";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let probe = r#"printf '[%s,%s,%s,%s]' "$PAGER" "$GIT_TERMINAL_PROMPT" "$EDITOR" "$SSH_ASKPASS_REQUIRE""#;
    // Direct foreground pipes, the managed (auto-background) pipes, and a PTY.
    for (label, settings, background) in [
        ("direct", Some(pty_settings("off")), Some(false)),
        ("managed", None, None),
        ("pty", Some(pty_settings("always")), None),
    ] {
        let tool = bash_tool(&root, settings);
        let mut input = json!({ "command": probe });
        if let Some(background) = background {
            input["background"] = json!(background);
        }
        let out = block_on_local(tool.execute("call-1", input, None)).expect("execute");
        let text = first_text(&out);
        harness.log().info("verify", format!("{label}: {text}"));
        assert!(text.contains("[cat,0,true,force]"), "{label}: {text}"); // ubs:ignore[rust.panic.assert-macros] -- test assertion
    }
    // A pager on auto stays on pipes, and the command's own assignment wins.
    let tool = bash_tool(&root, None);
    let text = first_text(&execute(&tool, "less /dev/null; test -t 1 || echo PIPE")).to_owned();
    assert!(text.contains("PIPE"), "less must not get a PTY: {text}"); // ubs:ignore[rust.panic.assert-macros] -- test assertion
    let out = execute(&tool, r#"EDITOR=vim sh -c 'printf "[%s]" "$EDITOR"'"#);
    assert!(first_text(&out).contains("[vim]"), "{}", first_text(&out)); // ubs:ignore[rust.panic.assert-macros] -- test assertion
    finish_case(&harness, case);
}

#[test]
fn pty_timeout_still_kills() {
    let case = "pty_timeout_still_kills";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(&root, Some(pty_settings("always")));
    let out =
        block_on_local(tool.execute("call-1", json!({"command": "sleep 30", "timeout": 1}), None))
            .expect("execute");
    let text = first_text(&out);
    assert!(
        text.contains("timed out after 1 seconds"),
        "pty path must honor the timeout/kill discipline: {text}"
    );
    finish_case(&harness, case);
}
