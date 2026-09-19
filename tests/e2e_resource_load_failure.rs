//! End-to-end tests for resource loading failure detection (gh #223 / bd-fpaso).
//!
//! Verifies that:
//! 1. When a configured package in `settings.json` fails to install/resolve,
//!    running without explicit resource flags (`-e`, `--skill`, etc.) fails
//!    with distinct exit code 4 (`EXIT_CODE_RESOURCE_LOAD_FAILED`), emits
//!    a machine-readable fatal error record with code `"resource.load_failed"`
//!    on stdout in `--mode json`, and prints the human-facing warning to stderr.
//! 2. When an explicit `-e` path is provided, the same configured package failure
//!    yields the existing hard error exit code 1, does not double-report fatal records,
//!    and also emits the warning to stderr.
//! 3. A healthy run with valid/empty configuration exits 0 with no warning and
//!    no error record.

mod common;

use common::TestHarness;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct ExecOutput {
    argv: Vec<String>,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn run_pi(harness: &TestHarness, env_vars: &[(&str, &str)], args: &[&str]) -> ExecOutput {
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_pi")); // ubs:ignore false positive: Cargo provides the compiled test binary path.
    let mut cmd = Command::new(&binary_path);
    cmd.args(args);
    cmd.current_dir(harness.temp_dir());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    for (key, val) in env_vars {
        cmd.env(key, val);
    }

    let output = cmd.output().expect("execute pi binary");
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut captured_argv = vec![binary_path.display().to_string()];
    captured_argv.extend(args.iter().map(|s| (*s).to_string()));

    ExecOutput {
        argv: captured_argv,
        exit_code,
        stdout,
        stderr,
    }
}

fn parse_fatal_error_records(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|val| val.get("type").and_then(Value::as_str) == Some("error"))
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_resource_load_failure_modes() {
    let harness = TestHarness::new("e2e_resource_load_failure_modes");

    let agent_dir = harness.temp_path("agent");
    let packages_dir = harness.temp_path("packages");
    let sessions_dir = harness.temp_path("sessions");
    let non_writable_prefix = harness.temp_path("ro-prefix");

    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::create_dir_all(&packages_dir).expect("create packages dir");
    std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
    std::fs::create_dir_all(&non_writable_prefix).expect("create prefix dir");

    #[cfg(unix)]
    {
        let ro_perms = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&non_writable_prefix, ro_perms).expect("set ro permissions");
    }

    // Broken settings.json referencing an unreachable package.
    let broken_settings = harness.temp_path("settings.broken.json");
    let broken_settings_content = serde_json::json!({
        "packages": [
            "npm:unreachable-package-xyz-nonexistent-12345"
        ]
    })
    .to_string();
    std::fs::write(&broken_settings, &broken_settings_content).expect("write broken settings");

    // Explicit dummy extension file for case 2.
    let dummy_ext = harness.temp_path("dummy_ext.js");
    std::fs::write(&dummy_ext, "export default function (pi) {}\n").expect("write dummy ext");

    // Healthy settings.json for case 3.
    let healthy_settings = harness.temp_path("settings.healthy.json");
    let healthy_settings_content = "{}".to_string();
    std::fs::write(&healthy_settings, &healthy_settings_content).expect("write healthy settings");

    let broken_env = [
        ("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap()),
        ("PI_CONFIG_PATH", broken_settings.to_str().unwrap()),
        ("PI_PACKAGE_DIR", packages_dir.to_str().unwrap()),
        ("PI_SESSIONS_DIR", sessions_dir.to_str().unwrap()),
        ("npm_config_prefix", non_writable_prefix.to_str().unwrap()),
        ("npm_config_audit", "false"),
        ("npm_config_fund", "false"),
        ("npm_config_update_notifier", "false"),
        ("npm_config_registry", "http://127.0.0.1:9/"),
        ("npm_config_fetch_retries", "0"),
        ("npm_config_fetch_retry_mintimeout", "1"),
        ("npm_config_fetch_retry_maxtimeout", "1"),
    ];

    let mut report_lines = Vec::new();
    report_lines
        .push("=== E2E RESOURCE LOAD FAILURE COMPARISON (gh #223 / bd-fpaso) ===".to_string());
    report_lines.push(format!("Settings (broken): {broken_settings_content}"));
    report_lines.push(format!(
        "Prefix (non-writable): {}",
        non_writable_prefix.display()
    ));
    report_lines.push(String::new());

    // ------------------------------------------------------------------------
    // CASE 1: Without -e (configured package load failure)
    // Must exit with code 4, emit warning to stderr, and emit code "resource.load_failed" in JSON.
    // ------------------------------------------------------------------------
    let case1_args = [
        "-p",
        "--mode",
        "json",
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet-4-5",
        "hello",
    ];
    let case1_out = run_pi(&harness, &broken_env, &case1_args);
    let case1_records = parse_fatal_error_records(&case1_out.stdout);

    report_lines.push("--- CASE 1: WITHOUT -e (Configured Resource Load Failure) ---".to_string());
    report_lines.push(format!("argv: {:?}", case1_out.argv));
    report_lines.push(format!("exit_code: {}", case1_out.exit_code));
    report_lines.push(format!("stdout:\n{}", case1_out.stdout));
    report_lines.push(format!("stderr:\n{}", case1_out.stderr));

    let pass_c1_exit = case1_out.exit_code == 4;
    let pass_c1_warning = case1_out
        .stderr
        .contains("Warning: Failed to load skills/prompts/themes/extensions:");
    let pass_c1_rec_count = case1_records.len() == 1;
    let pass_c1_rec_code = case1_records
        .first()
        .and_then(|r| r.get("code"))
        .and_then(Value::as_str)
        == Some("resource.load_failed");
    let pass_c1_rec_exit = case1_records
        .first()
        .and_then(|r| r.get("exit_code"))
        .and_then(Value::as_i64)
        == Some(4);
    let pass_c1_rec_phase = case1_records
        .first()
        .and_then(|r| r.get("phase"))
        .and_then(Value::as_str)
        == Some("startup");

    report_lines.push(format!(
        "[{}] case 1 exit code == 4 (actual: {})",
        if pass_c1_exit { "PASS" } else { "FAIL" },
        case1_out.exit_code
    ));
    report_lines.push(format!(
        "[{}] case 1 stderr contains warning",
        if pass_c1_warning { "PASS" } else { "FAIL" }
    ));
    report_lines.push(format!(
        "[{}] case 1 exactly one fatal record on stdout (actual: {})",
        if pass_c1_rec_count { "PASS" } else { "FAIL" },
        case1_records.len()
    ));
    report_lines.push(format!(
        "[{}] case 1 record code == 'resource.load_failed'",
        if pass_c1_rec_code { "PASS" } else { "FAIL" }
    ));
    report_lines.push(format!(
        "[{}] case 1 record exit_code == 4",
        if pass_c1_rec_exit { "PASS" } else { "FAIL" }
    ));
    report_lines.push(format!(
        "[{}] case 1 record phase == 'startup'",
        if pass_c1_rec_phase { "PASS" } else { "FAIL" }
    ));
    report_lines.push(String::new());

    // ------------------------------------------------------------------------
    // CASE 2: With explicit -e (explicit path hard error)
    // Must exit with code 1, emit warning to stderr, and not double-report fatal records.
    // ------------------------------------------------------------------------
    let dummy_ext_str = dummy_ext.to_str().unwrap();
    let case2_args = [
        "-p",
        "--mode",
        "json",
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet-4-5",
        "-e",
        dummy_ext_str,
        "hello",
    ];
    let case2_out = run_pi(&harness, &broken_env, &case2_args);
    let case2_records = parse_fatal_error_records(&case2_out.stdout);

    report_lines.push("--- CASE 2: WITH -e (Explicit Path Hard Error) ---".to_string());
    report_lines.push(format!("argv: {:?}", case2_out.argv));
    report_lines.push(format!("exit_code: {}", case2_out.exit_code));
    report_lines.push(format!("stdout:\n{}", case2_out.stdout));
    report_lines.push(format!("stderr:\n{}", case2_out.stderr));

    let pass_c2_exit = case2_out.exit_code == 1;
    let pass_c2_warning = case2_out
        .stderr
        .contains("Warning: Failed to load skills/prompts/themes/extensions:");
    let pass_c2_rec_count = case2_records.len() == 1; // Does not double-report
    let pass_c2_rec_exit = case2_records
        .first()
        .and_then(|r| r.get("exit_code"))
        .and_then(Value::as_i64)
        == Some(1);

    report_lines.push(format!(
        "[{}] case 2 exit code == 1 (actual: {})",
        if pass_c2_exit { "PASS" } else { "FAIL" },
        case2_out.exit_code
    ));
    report_lines.push(format!(
        "[{}] case 2 stderr contains warning",
        if pass_c2_warning { "PASS" } else { "FAIL" }
    ));
    report_lines.push(format!(
        "[{}] case 2 exactly one fatal record (no double-report, actual: {})",
        if pass_c2_rec_count { "PASS" } else { "FAIL" },
        case2_records.len()
    ));
    report_lines.push(format!(
        "[{}] case 2 record exit_code == 1",
        if pass_c2_rec_exit { "PASS" } else { "FAIL" }
    ));
    report_lines.push(String::new());

    // ------------------------------------------------------------------------
    // CASE 3: Healthy control run
    // Must exit 0, emit no warning, and emit no error records.
    // ------------------------------------------------------------------------
    let healthy_env = [
        ("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap()),
        ("PI_CONFIG_PATH", healthy_settings.to_str().unwrap()),
        ("PI_PACKAGE_DIR", packages_dir.to_str().unwrap()),
        ("PI_SESSIONS_DIR", sessions_dir.to_str().unwrap()),
    ];
    let case3_args = ["--version"];
    let case3_out = run_pi(&harness, &healthy_env, &case3_args);
    let case3_records = parse_fatal_error_records(&case3_out.stdout);

    report_lines.push("--- CASE 3: HEALTHY CONTROL RUN ---".to_string());
    report_lines.push(format!("argv: {:?}", case3_out.argv));
    report_lines.push(format!("exit_code: {}", case3_out.exit_code));
    report_lines.push(format!("stdout:\n{}", case3_out.stdout));
    report_lines.push(format!("stderr:\n{}", case3_out.stderr));

    let pass_c3_exit = case3_out.exit_code == 0;
    let pass_c3_warning = !case3_out
        .stderr
        .contains("Warning: Failed to load skills/prompts/themes/extensions:");
    let pass_c3_no_rec = case3_records.is_empty();

    report_lines.push(format!(
        "[{}] case 3 exit code == 0 (actual: {})",
        if pass_c3_exit { "PASS" } else { "FAIL" },
        case3_out.exit_code
    ));
    report_lines.push(format!(
        "[{}] case 3 stderr has no warning",
        if pass_c3_warning { "PASS" } else { "FAIL" }
    ));
    report_lines.push(format!(
        "[{}] case 3 stdout has no error records (actual: {})",
        if pass_c3_no_rec { "PASS" } else { "FAIL" },
        case3_records.len()
    ));
    report_lines
        .push("==================================================================".to_string());

    let report = report_lines.join("\n");
    println!("{report}");

    let report_file = harness.temp_path("e2e_resource_load_failure_summary.txt");
    std::fs::write(&report_file, &report).expect("write report artifact");
    harness.record_artifact("e2e_resource_load_failure_summary.txt", &report_file);

    // Assert all conditions.
    assert!(
        pass_c1_exit,
        "Case 1 exit code must be 4, got {}",
        case1_out.exit_code
    );
    assert!(
        pass_c1_warning,
        "Case 1 stderr must contain warning, got:\n{}",
        case1_out.stderr
    );
    assert!(
        pass_c1_rec_count,
        "Case 1 must have 1 fatal record, got {}",
        case1_records.len()
    );
    assert!(
        pass_c1_rec_code,
        "Case 1 record code must be resource.load_failed, got {:?}",
        case1_records.first()
    );
    assert!(
        pass_c1_rec_exit,
        "Case 1 record exit_code must be 4, got {:?}",
        case1_records.first()
    );
    assert!(
        pass_c1_rec_phase,
        "Case 1 record phase must be startup, got {:?}",
        case1_records.first()
    );

    assert!(
        pass_c2_exit,
        "Case 2 exit code must be 1, got {}",
        case2_out.exit_code
    );
    assert!(
        pass_c2_warning,
        "Case 2 stderr must contain warning, got:\n{}",
        case2_out.stderr
    );
    assert!(
        pass_c2_rec_count,
        "Case 2 must have 1 fatal record, got {}",
        case2_records.len()
    );
    assert!(
        pass_c2_rec_exit,
        "Case 2 record exit_code must be 1, got {:?}",
        case2_records.first()
    );

    assert!(
        pass_c3_exit,
        "Case 3 exit code must be 0, got {}",
        case3_out.exit_code
    );
    assert!(
        pass_c3_warning,
        "Case 3 stderr must not contain warning, got:\n{}",
        case3_out.stderr
    );
    assert!(
        pass_c3_no_rec,
        "Case 3 stdout must have no error records, got {}",
        case3_records.len()
    );
}
