//! POSIX-only. Every case builds a mock `rch` as an executable shell script
//! (`set_mode(0o755)`) under a `/tmp` case directory and admits it through
//! `PATH`, so there is nothing here for Windows to assert; `--all-targets` on a
//! Windows host only needs the file to compile away.
#![cfg(unix)]

use serde::Deserialize;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn case_dir(case_name: &str) -> PathBuf {
    PathBuf::from("/tmp")
        .join("pi_agent_rust_cargo_headroom_admission")
        .join(format!("{}-{}", case_name, std::process::id()))
}

fn run_admission_with_env(
    case_name: &str,
    path: &str,
    args: &[&str],
    envs: &[(&str, String)],
) -> Output {
    let root = repo_root();
    let dir = case_dir(case_name);
    let target_dir = dir.join("target");
    let tmpdir = dir.join("tmp");

    let mut command_args = vec![
        "--admit-only",
        "--min-free-mb",
        "1",
        "--target-dir",
        target_dir
            .to_str()
            .expect("test target dir should be valid UTF-8"),
        "--tmpdir",
        tmpdir.to_str().expect("test tmpdir should be valid UTF-8"),
    ];
    command_args.extend_from_slice(args);

    let mut command = Command::new(root.join("scripts/cargo_headroom.sh"));
    command.env("PATH", path).env("PI_CARGO_PROCESS_COUNT", "0");
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .args(command_args)
        .output()
        .expect("cargo_headroom.sh should execute")
}

fn run_admission(case_name: &str, path: &str, args: &[&str]) -> Output {
    run_admission_with_env(case_name, path, args, &[])
}

fn decision_from_stdout(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .expect("stdout should contain admission JSON");
    serde_json::from_str(line).expect("admission JSON should parse")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_mock_rch(
    dir: &Path,
    check_status: i32,
    check_stderr: &str,
    queue_status: i32,
    queue_stdout: &str,
) {
    std::fs::create_dir_all(dir).expect("mock rch dir should be created");
    let path = dir.join("rch");
    let body = format!(
        "#!/usr/bin/env bash\n\
if [[ \"$1\" == \"check\" ]]; then\n\
    printf '%s\\n' {} >&2\n\
    exit {check_status}\n\
fi\n\
if [[ \"$1\" == \"queue\" ]]; then\n\
    printf '%s\\n' {}\n\
    exit {queue_status}\n\
fi\n\
exit 1\n",
        shell_single_quote(check_stderr),
        shell_single_quote(queue_stdout)
    );
    std::fs::write(&path, body).expect("mock rch script should be written");
    let mut permissions = std::fs::metadata(&path)
        .expect("mock rch metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("mock rch script should be executable");
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PathMode {
    WithoutRch,
    MockRch,
}

const fn default_mock_rch_check_status() -> i32 {
    1
}

const fn default_mock_rch_queue_status() -> i32 {
    1
}

#[derive(Debug, Deserialize)]
struct AdmissionFixture {
    name: String,
    path_mode: PathMode,
    mock_rch_stderr: Option<String>,
    #[serde(default = "default_mock_rch_check_status")]
    mock_rch_check_status: i32,
    #[serde(default = "default_mock_rch_queue_status")]
    mock_rch_queue_status: i32,
    #[serde(default)]
    mock_rch_queue_stdout: Option<String>,
    args: Vec<String>,
    expected_status: i32,
    expected_decision: String,
    expected_reason: String,
    expected_command_class: String,
    expected_resolved_runner: String,
    expected_rch_detail: String,
    #[serde(default)]
    expected_rch_detail_contains: Option<String>,
    expected_allow_local_fallback: bool,
    expected_forecast_status: String,
    expected_forecast_recommended_action: String,
    expected_forecast_slot_pressure: String,
    #[serde(default)]
    process_count_override: Option<u64>,
    #[serde(default)]
    max_local_cargo_processes: Option<u64>,
    #[serde(default)]
    expected_local_process_status: Option<String>,
    #[serde(default)]
    expected_local_process_recommended_action: Option<String>,
    #[serde(default)]
    expected_force_override: bool,
}

fn admission_fixtures() -> Vec<AdmissionFixture> {
    let path = repo_root().join("tests/fixtures/cargo_headroom_admission/admission_cases.json");
    let content =
        std::fs::read_to_string(&path).expect("admission fixture file should be readable");
    serde_json::from_str(&content).expect("admission fixture file should parse")
}

fn assert_forecast_matches_fixture(decision: &Value, fixture: &AdmissionFixture) {
    let forecast = &decision["rch_queue_forecast"];
    assert_eq!(
        forecast["schema"].as_str(),
        Some("pi.cargo_headroom.rch_queue_forecast.v1")
    );
    assert_eq!(
        forecast["status"].as_str(),
        Some(fixture.expected_forecast_status.as_str()),
        "{} forecast status mismatch: {forecast}",
        fixture.name
    );
    assert_eq!(
        forecast["recommended_action"].as_str(),
        Some(fixture.expected_forecast_recommended_action.as_str()),
        "{} forecast recommended action mismatch: {forecast}",
        fixture.name
    );
    assert_eq!(
        forecast["slot_pressure"].as_str(),
        Some(fixture.expected_forecast_slot_pressure.as_str()),
        "{} forecast slot pressure mismatch: {forecast}",
        fixture.name
    );
}

fn expected_admission_action(decision: &str) -> &'static str {
    match decision {
        "allow" => "allow",
        "degraded" => "fallback",
        _ => "defer",
    }
}

fn assert_local_process_matches_fixture(decision: &Value, fixture: &AdmissionFixture) {
    let pressure = &decision["local_process_pressure"];
    assert_eq!(
        pressure["schema"].as_str(),
        Some("pi.cargo_headroom.local_process_pressure.v1")
    );
    assert_eq!(
        pressure["status"].as_str(),
        Some(
            fixture
                .expected_local_process_status
                .as_deref()
                .unwrap_or("ok"),
        ),
        "{} local process status mismatch: {pressure}",
        fixture.name
    );
    assert_eq!(
        pressure["recommended_action"].as_str(),
        Some(
            fixture
                .expected_local_process_recommended_action
                .as_deref()
                .unwrap_or("run"),
        ),
        "{} local process action mismatch: {pressure}",
        fixture.name
    );
    assert_eq!(
        pressure["force_override"].as_bool(),
        Some(fixture.expected_force_override),
        "{} local process force override mismatch: {pressure}",
        fixture.name
    );
    let expected_count = fixture.process_count_override.unwrap_or(0);
    assert_eq!(
        pressure["process_count"].as_u64(),
        Some(expected_count),
        "{} local process count mismatch: {pressure}",
        fixture.name
    );
}

fn fixture_path(fixture: &AdmissionFixture, mock_dir: &Path) -> String {
    match fixture.path_mode {
        PathMode::WithoutRch => "/usr/bin:/bin".to_string(),
        PathMode::MockRch => {
            write_mock_rch(
                mock_dir,
                fixture.mock_rch_check_status,
                fixture.mock_rch_stderr.as_deref().unwrap_or(""),
                fixture.mock_rch_queue_status,
                fixture.mock_rch_queue_stdout.as_deref().unwrap_or(""),
            );
            format!("{}:/usr/bin:/bin", mock_dir.display())
        }
    }
}

fn fixture_envs(fixture: &AdmissionFixture) -> Vec<(&'static str, String)> {
    let mut envs = Vec::new();
    if let Some(count) = fixture.process_count_override {
        envs.push(("PI_CARGO_PROCESS_COUNT", count.to_string()));
    }
    if let Some(max_processes) = fixture.max_local_cargo_processes {
        envs.push(("PI_CARGO_MAX_LOCAL_PROCESSES", max_processes.to_string()));
    }
    envs
}

fn assert_status_matches_fixture(output: &Output, fixture: &AdmissionFixture) {
    assert_eq!(
        output.status.code(),
        Some(fixture.expected_status),
        "{} status mismatch\nstdout:\n{}\nstderr:\n{}",
        fixture.name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_rch_detail_matches_fixture(decision: &Value, fixture: &AdmissionFixture) {
    if let Some(needle) = fixture.expected_rch_detail_contains.as_deref() {
        let detail = decision["rch_detail"]
            .as_str()
            .expect("rch_detail must be a string");
        assert!(
            detail.contains(needle),
            "{} rch_detail should contain {needle:?}: {detail}",
            fixture.name
        );
    } else {
        assert_eq!(decision["rch_detail"], fixture.expected_rch_detail);
    }
}

fn assert_paths_match_fixture(decision: &Value, fixture: &AdmissionFixture) {
    let cargo_target_dir = decision["cargo_target_dir"]
        .as_str()
        .expect("cargo_target_dir must be a string");
    let tmpdir = decision["tmpdir"]
        .as_str()
        .expect("tmpdir must be a string");
    assert!(
        cargo_target_dir.contains(&fixture.name),
        "{} cargo_target_dir should identify fixture run: {cargo_target_dir}",
        fixture.name
    );
    assert!(
        tmpdir.contains(&fixture.name),
        "{} tmpdir should identify fixture run: {tmpdir}",
        fixture.name
    );

    let recommended_target = decision["recommended_cargo_target_dir"]
        .as_str()
        .expect("recommended_cargo_target_dir must be a string");
    let recommended_tmp = decision["recommended_tmpdir"]
        .as_str()
        .expect("recommended_tmpdir must be a string");
    assert!(
        recommended_target.ends_with("/target"),
        "{} recommended target must be concrete: {recommended_target}",
        fixture.name
    );
    assert!(
        recommended_tmp.ends_with("/tmp"),
        "{} recommended tmpdir must be concrete: {recommended_tmp}",
        fixture.name
    );

    let target_remediation = decision["storage_remediation"]["cargo_target_dir"]
        .as_str()
        .expect("cargo target remediation must be a string");
    let tmpdir_remediation = decision["storage_remediation"]["tmpdir"]
        .as_str()
        .expect("tmpdir remediation must be a string");
    assert!(target_remediation.contains("CARGO_TARGET_DIR"));
    assert!(target_remediation.contains("--target-dir"));
    assert!(target_remediation.contains(cargo_target_dir));
    assert!(tmpdir_remediation.contains("TMPDIR"));
    assert!(tmpdir_remediation.contains("--tmpdir"));
    assert!(tmpdir_remediation.contains(tmpdir));
}

fn assert_decision_matches_fixture(decision: &Value, fixture: &AdmissionFixture) {
    assert_eq!(decision["schema"], "pi.cargo_headroom.admission.v1");
    assert_eq!(decision["decision"], fixture.expected_decision);
    assert_eq!(
        decision["admission_action"],
        expected_admission_action(&fixture.expected_decision)
    );
    assert_eq!(decision["reason"], fixture.expected_reason);
    assert_eq!(decision["command_class"], fixture.expected_command_class);
    assert_eq!(
        decision["resolved_runner"],
        fixture.expected_resolved_runner
    );
    assert_rch_detail_matches_fixture(decision, fixture);
    assert_eq!(
        decision["allow_local_fallback"],
        fixture.expected_allow_local_fallback
    );
    assert_eq!(decision["force_override"], fixture.expected_force_override);
    assert_forecast_matches_fixture(decision, fixture);
    assert_local_process_matches_fixture(decision, fixture);

    let planned_command = decision["planned_command"]
        .as_str()
        .expect("planned_command must be a string");
    assert!(
        planned_command.contains("cargo"),
        "{} planned command should expose the cargo invocation: {planned_command}",
        fixture.name
    );
    assert_paths_match_fixture(decision, fixture);
}

#[test]
fn fixture_matrix_keeps_rch_admission_decisions_stable() {
    for fixture in admission_fixtures() {
        let mock_dir = case_dir(&format!("fixture-{}", fixture.name)).join("bin");
        let path = fixture_path(&fixture, &mock_dir);
        let args: Vec<&str> = fixture.args.iter().map(String::as_str).collect();
        let envs = fixture_envs(&fixture);
        let output =
            run_admission_with_env(&format!("fixture-{}", fixture.name), &path, &args, &envs);

        assert_status_matches_fixture(&output, &fixture);
        let decision = decision_from_stdout(&output);
        assert_decision_matches_fixture(&decision, &fixture);
    }
}

#[test]
fn auto_runner_backs_off_heavy_command_when_rch_is_missing() {
    let output = run_admission(
        "missing-rch-heavy",
        "/usr/bin:/bin",
        &[
            "--runner",
            "auto",
            "clippy",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    let decision = decision_from_stdout(&output);
    assert_eq!(decision["decision"], "backoff");
    assert_eq!(decision["reason"], "rch_unavailable");
    assert_eq!(decision["command_class"], "heavy");
    assert_eq!(decision["resolved_runner"], "none");
    assert_eq!(decision["rch_queue_forecast"]["status"], "unavailable");
    assert_eq!(
        decision["rch_queue_forecast"]["recommended_action"],
        "backoff"
    );
}

#[test]
fn auto_runner_allows_safe_local_command_when_rch_is_missing() {
    let output = run_admission(
        "missing-rch-safe",
        "/usr/bin:/bin",
        &["--runner", "auto", "fmt", "--check"],
    );

    assert!(output.status.success());
    let decision = decision_from_stdout(&output);
    assert_eq!(decision["decision"], "degraded");
    assert_eq!(decision["reason"], "safe_local_command");
    assert_eq!(decision["command_class"], "safe_local");
    assert_eq!(decision["resolved_runner"], "local");
    assert_eq!(decision["rch_queue_forecast"]["status"], "unavailable");
}

#[test]
fn auto_runner_requires_explicit_local_fallback_for_heavy_command() {
    let output = run_admission(
        "explicit-local-fallback",
        "/usr/bin:/bin",
        &[
            "--runner",
            "auto",
            "--allow-local-fallback",
            "test",
            "--all-targets",
        ],
    );

    assert!(output.status.success());
    let decision = decision_from_stdout(&output);
    assert_eq!(decision["decision"], "degraded");
    assert_eq!(decision["reason"], "explicit_local_fallback");
    assert_eq!(decision["allow_local_fallback"], true);
    assert_eq!(decision["resolved_runner"], "local");
    assert_eq!(decision["rch_queue_forecast"]["status"], "unavailable");
}

#[test]
fn auto_runner_reports_saturated_rch_check_detail() {
    let mock_dir = case_dir("saturated-rch").join("bin");
    write_mock_rch(&mock_dir, 1, "queue saturated", 1, "");
    let path = format!("{}:/usr/bin:/bin", mock_dir.display());
    let output = run_admission(
        "saturated-rch",
        &path,
        &["--runner", "auto", "test", "--all-targets"],
    );

    assert_eq!(output.status.code(), Some(2));
    let decision = decision_from_stdout(&output);
    assert_eq!(decision["decision"], "backoff");
    assert_eq!(decision["reason"], "rch_unavailable");
    assert_eq!(decision["rch_detail"], "queue saturated");
    assert_eq!(decision["rch_queue_forecast"]["status"], "unavailable");
}

#[test]
fn insufficient_target_headroom_emits_backoff_decision() {
    let dir = case_dir("insufficient-headroom");
    let target_dir = dir.join("target");
    let tmpdir = dir.join("tmp");
    let output = Command::new(repo_root().join("scripts/cargo_headroom.sh"))
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--admit-only",
            "--runner",
            "local",
            "--min-free-mb",
            "999999999",
            "--target-dir",
            target_dir
                .to_str()
                .expect("test target dir should be valid UTF-8"),
            "--tmpdir",
            tmpdir.to_str().expect("test tmpdir should be valid UTF-8"),
            "fmt",
            "--check",
        ])
        .output()
        .expect("cargo_headroom.sh should execute");

    assert_eq!(output.status.code(), Some(2));
    let decision = decision_from_stdout(&output);
    assert_eq!(decision["decision"], "backoff");
    assert_eq!(decision["reason"], "insufficient_headroom");
    assert_eq!(decision["command_class"], "blocked");
}
