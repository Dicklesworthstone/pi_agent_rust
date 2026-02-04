//! End-to-end CLI tests (offline).
//!
//! These tests invoke the compiled `pi` binary directly and verify that
//! offline flags/subcommands behave as expected, with verbose logging
//! and artifact capture for debugging failures.

mod common;

use common::TestHarness;
use serde::Deserialize;
use serde_json::json;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct CliResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration: Duration,
}

struct CliTestHarness {
    harness: TestHarness,
    binary_path: PathBuf,
    #[allow(dead_code)]
    env_root: PathBuf,
    env: BTreeMap<String, String>,
    run_seq: Cell<usize>,
}

#[cfg(unix)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PackageCommandStubs {
    npm_log: PathBuf,
    git_log: PathBuf,
    #[allow(dead_code)]
    npm_global_root: PathBuf,
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CommandInvocation {
    argv: Vec<String>,
    #[allow(dead_code)]
    cwd: String,
}

impl CliTestHarness {
    fn new(name: &str) -> Self {
        let harness = TestHarness::new(name);
        let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_pi"));

        let mut env = BTreeMap::new();

        let env_root = harness.temp_path("pi-env");
        let _ = std::fs::create_dir_all(&env_root);

        // Fully isolate global/project state for determinism.
        env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            env_root.join("agent").display().to_string(),
        );
        env.insert(
            "PI_CONFIG_PATH".to_string(),
            env_root.join("settings.json").display().to_string(),
        );
        env.insert(
            "PI_SESSIONS_DIR".to_string(),
            env_root.join("sessions").display().to_string(),
        );
        env.insert(
            "PI_PACKAGE_DIR".to_string(),
            env_root.join("packages").display().to_string(),
        );

        Self {
            harness,
            binary_path,
            env_root,
            env,
            run_seq: Cell::new(0),
        }
    }

    fn global_settings_path(&self) -> PathBuf {
        self.env
            .get("PI_CONFIG_PATH")
            .map(PathBuf::from)
            .expect("PI_CONFIG_PATH set by CliTestHarness::new")
    }

    fn project_settings_path(&self) -> PathBuf {
        self.harness.temp_dir().join(".pi").join("settings.json")
    }

    fn snapshot_path(&self, source: &Path, artifact_name: &str) {
        if !source.exists() {
            return;
        }

        let dest = self.harness.temp_path(artifact_name);
        std::fs::copy(source, &dest).expect("copy snapshot");
        self.harness.record_artifact(artifact_name, &dest);
    }

    fn snapshot_settings(&self, label: &str) {
        self.snapshot_path(
            &self.global_settings_path(),
            &format!("settings.global.{label}.json"),
        );
        self.snapshot_path(
            &self.project_settings_path(),
            &format!("settings.project.{label}.json"),
        );
    }

    #[allow(clippy::too_many_lines)]
    #[cfg(unix)]
    fn enable_offline_package_stubs(&mut self) -> PackageCommandStubs {
        let bin_dir = self.harness.temp_path("stub-bin");
        std::fs::create_dir_all(&bin_dir).expect("create stub bin dir");

        let npm_log = self.harness.temp_path("npm-invocations.jsonl");
        let git_log = self.harness.temp_path("git-invocations.jsonl");
        let _ = std::fs::write(&npm_log, "");
        let _ = std::fs::write(&git_log, "");

        let npm_global_root = self.env_root.join("npm-global").join("node_modules");
        std::fs::create_dir_all(&npm_global_root).expect("create npm global root");

        let npm_path = bin_dir.join("npm");
        fs::write(
            &npm_path,
            r#"#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

def log_invocation():
    log_path = os.environ.get("PI_E2E_NPM_LOG")
    if not log_path:
        return
    entry = {"argv": sys.argv[1:], "cwd": os.getcwd()}
    with open(log_path, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, sort_keys=True))
        f.write("\n")

def parse_name(spec: str) -> str:
    spec = spec.strip()
    if spec.startswith("@"):
        pos = spec.rfind("@")
        return spec[:pos] if pos > 0 else spec
    pos = spec.find("@")
    return spec[:pos] if pos > 0 else spec

def ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)

def main() -> int:
    log_invocation()
    args = sys.argv[1:]
    if args == ["root", "-g"]:
        root = os.environ.get("PI_E2E_NPM_GLOBAL_ROOT", "")
        if not root:
            print("npm stub: missing PI_E2E_NPM_GLOBAL_ROOT", file=sys.stderr)
            return 2
        print(root)
        return 0

    if not args:
        print("npm stub: no args", file=sys.stderr)
        return 2

    cmd = args[0]
    if cmd == "install":
        if "-g" in args:
            idx = args.index("-g")
            if idx + 1 >= len(args):
                print("npm stub: install -g missing spec", file=sys.stderr)
                return 2
            spec = args[idx + 1]
            name = parse_name(spec)
            root = os.environ.get("PI_E2E_NPM_GLOBAL_ROOT", "")
            if not root:
                print("npm stub: missing PI_E2E_NPM_GLOBAL_ROOT", file=sys.stderr)
                return 2
            ensure_dir(Path(root) / name)
            return 0

        if "--prefix" in args:
            idx = args.index("--prefix")
            if idx + 1 >= len(args):
                print("npm stub: --prefix missing value", file=sys.stderr)
                return 2
            prefix = args[idx + 1]
            spec = None
            for candidate in args[1:]:
                if candidate.startswith("-"):
                    continue
                spec = candidate
                break
            if spec:
                name = parse_name(spec)
                ensure_dir(Path(prefix) / "node_modules" / name)
            return 0

        ensure_dir(Path.cwd() / "node_modules")
        return 0

    if cmd == "uninstall":
        return 0

    print(f"npm stub: unsupported args: {args}", file=sys.stderr)
    return 2

if __name__ == "__main__":
    sys.exit(main())
"#,
        )
        .expect("write npm stub");

        let mut perms = fs::metadata(&npm_path)
            .expect("stat npm stub")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&npm_path, perms).expect("chmod npm stub");

        let git_path = bin_dir.join("git");
        fs::write(
            &git_path,
            r#"#!/usr/bin/env python3
import json
import os
import sys

log_path = os.environ.get("PI_E2E_GIT_LOG")
if log_path:
    entry = {"argv": sys.argv[1:], "cwd": os.getcwd()}
    with open(log_path, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, sort_keys=True))
        f.write("\n")

print("git stub invoked unexpectedly", file=sys.stderr)
sys.exit(2)
"#,
        )
        .expect("write git stub");

        let mut perms = fs::metadata(&git_path)
            .expect("stat git stub")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_path, perms).expect("chmod git stub");

        let inherited_path = std::env::var("PATH").unwrap_or_default();
        self.env.insert(
            "PATH".to_string(),
            format!("{}:{inherited_path}", bin_dir.display()),
        );
        self.env
            .insert("PI_E2E_NPM_LOG".to_string(), npm_log.display().to_string());
        self.env
            .insert("PI_E2E_GIT_LOG".to_string(), git_log.display().to_string());
        self.env.insert(
            "PI_E2E_NPM_GLOBAL_ROOT".to_string(),
            npm_global_root.display().to_string(),
        );

        PackageCommandStubs {
            npm_log,
            git_log,
            npm_global_root,
        }
    }

    fn run(&self, args: &[&str]) -> CliResult {
        self.run_with_stdin(args, None)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> CliResult {
        self.harness
            .log()
            .info("action", format!("Running CLI: {}", args.join(" ")));
        self.harness.log().info_ctx("action", "CLI env", |ctx| {
            for (key, value) in &self.env {
                ctx.push((key.clone(), value.clone()));
            }
        });
        if let Some(bytes) = stdin {
            self.harness.log().info_ctx("action", "CLI stdin", |ctx| {
                ctx.push(("bytes".to_string(), bytes.len().to_string()));
            });
        }

        let start = Instant::now();
        let mut command = Command::new(&self.binary_path);
        command
            .args(args)
            .envs(self.env.clone())
            .current_dir(self.harness.temp_dir())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command.spawn().expect("run pi");
        if let Some(input) = stdin {
            if let Some(mut child_stdin) = child.stdin.take() {
                child_stdin.write_all(input).expect("write stdin");
            }
        }
        let output = child.wait_with_output().expect("run pi");
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        self.harness
            .log()
            .info_ctx("result", "CLI completed", |ctx| {
                ctx.push(("exit_code".to_string(), exit_code.to_string()));
                ctx.push(("duration_ms".to_string(), duration.as_millis().to_string()));
                ctx.push(("stdout_len".to_string(), stdout.len().to_string()));
                ctx.push(("stderr_len".to_string(), stderr.len().to_string()));
            });

        let seq = self.run_seq.get();
        self.run_seq.set(seq.saturating_add(1));

        let stdout_name = format!("stdout.{seq}.txt");
        let stderr_name = format!("stderr.{seq}.txt");
        let stdout_path = self.harness.temp_path(&stdout_name);
        let stderr_path = self.harness.temp_path(&stderr_name);
        let _ = std::fs::write(&stdout_path, &stdout);
        let _ = std::fs::write(&stderr_path, &stderr);
        self.harness.record_artifact(stdout_name, &stdout_path);
        self.harness.record_artifact(stderr_name, &stderr_path);

        CliResult {
            exit_code,
            stdout,
            stderr,
            duration,
        }
    }
}

fn assert_contains(harness: &TestHarness, haystack: &str, needle: &str) {
    harness.assert_log(format!("assert contains: {needle}").as_str());
    assert!(
        haystack.contains(needle),
        "expected output to contain '{needle}'"
    );
}

fn assert_contains_case_insensitive(harness: &TestHarness, haystack: &str, needle: &str) {
    harness.assert_log(format!("assert contains (ci): {needle}").as_str());
    assert!(
        haystack.to_lowercase().contains(&needle.to_lowercase()),
        "expected output to contain (case-insensitive) '{needle}'"
    );
}

fn assert_exit_code(harness: &TestHarness, result: &CliResult, expected: i32) {
    harness.assert_log(format!("assert exit_code == {expected}").as_str());
    assert_eq!(result.exit_code, expected);
}

#[cfg(unix)]
fn read_invocations(path: &Path) -> Vec<CommandInvocation> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };

    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<CommandInvocation>(trimmed).ok()
        })
        .collect()
}

fn read_json_value(path: &Path) -> serde_json::Value {
    let content = fs::read_to_string(path).expect("read json file");
    serde_json::from_str(&content).expect("parse json")
}

fn write_minimal_session(path: &Path, cwd: &Path) -> (String, String, String, String) {
    let session_id = "session-test-123";
    let timestamp = "2026-02-04T00:00:00.000Z";
    let message = "Hello export";
    let cwd_str = cwd.display().to_string();

    let header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": timestamp,
        "cwd": cwd_str,
        "provider": "anthropic",
        "modelId": "claude-3-opus-20240229"
    });
    let entry = json!({
        "type": "message",
        "timestamp": "2026-02-04T00:00:01.000Z",
        "message": {
            "role": "user",
            "content": message
        }
    });

    let content = format!("{header}\n{entry}\n");
    fs::write(path, content).expect("write session jsonl");

    (
        session_id.to_string(),
        timestamp.to_string(),
        cwd.display().to_string(),
        message.to_string(),
    )
}

fn count_jsonl_files(path: &Path) -> usize {
    let mut count = 0usize;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            count += count_jsonl_files(&entry_path);
        } else if entry_path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext == "jsonl")
        {
            count += 1;
        }
    }

    count
}

#[test]
fn e2e_cli_extension_compat_ledger_logged_when_enabled() {
    let mut harness = CliTestHarness::new("e2e_cli_extension_compat_ledger_logged_when_enabled");
    harness
        .env
        .insert("PI_EXT_COMPAT_SCAN".to_string(), "1".to_string());
    harness
        .env
        .insert("RUST_LOG".to_string(), "info".to_string());

    let ext_path = harness.harness.temp_path("ext.ts");
    std::fs::write(
        &ext_path,
        "import fs from 'fs';\nimport { spawn } from 'child_process';\npi.tool('read', { path: 'README.md' });\nnew Function('return 1');\neval('1');\n",
    )
    .expect("write ext.ts");

    let ext_arg = ext_path.display().to_string();
    let result = harness.run(&["--list-models", "--extension", ext_arg.as_str()]);

    assert_exit_code(&harness.harness, &result, 0);
    let combined = format!("{}\n{}", result.stdout, result.stderr);
    assert_contains(&harness.harness, &combined, "pi.ext.compat_ledger.v1");
}

#[test]
fn e2e_cli_version_flag() {
    let harness = CliTestHarness::new("e2e_cli_version_flag");
    let result = harness.run(&["--version"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "pi ");
    assert_contains(&harness.harness, &result.stdout, env!("CARGO_PKG_VERSION"));
    assert_contains(&harness.harness, &result.stdout, "\n");
}

#[test]
fn e2e_cli_help_flag() {
    let harness = CliTestHarness::new("e2e_cli_help_flag");
    let result = harness.run(&["--help"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains_case_insensitive(&harness.harness, &result.stdout, "usage");
    assert_contains(&harness.harness, &result.stdout, "pi");
}

#[test]
fn e2e_cli_invalid_flag_is_error() {
    let harness = CliTestHarness::new("e2e_cli_invalid_flag_is_error");
    let result = harness.run(&["--invalid-flag"]);

    harness
        .harness
        .assert_log("assert exit_code != 0 for invalid flag");
    assert_ne!(result.exit_code, 0);
    assert_contains_case_insensitive(&harness.harness, &result.stderr, "error");
}

#[test]
fn e2e_cli_config_subcommand_prints_paths() {
    let harness = CliTestHarness::new("e2e_cli_config_subcommand_prints_paths");
    let result = harness.run(&["config"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "Settings paths:");
    assert_contains(&harness.harness, &result.stdout, "Global:");
    assert_contains(&harness.harness, &result.stdout, "Project:");
    assert_contains(&harness.harness, &result.stdout, "Sessions:");
}

#[test]
fn e2e_cli_export_html_creates_file_and_contains_metadata() {
    let harness = CliTestHarness::new("e2e_cli_export_html_creates_file_and_contains_metadata");
    let session_path = harness.harness.temp_path("session.jsonl");
    let export_path = harness.harness.temp_path("export/session.html");

    let (session_id, timestamp, cwd, message) =
        write_minimal_session(&session_path, harness.harness.temp_dir());

    let session_arg = session_path.display().to_string();
    let export_arg = export_path.display().to_string();
    let result = harness.run(&["--export", session_arg.as_str(), export_arg.as_str()]);

    assert_exit_code(&harness.harness, &result, 0);
    assert!(export_path.exists(), "expected export file to exist");
    let html = fs::read_to_string(&export_path).expect("read export html");
    harness.harness.record_artifact("export.html", &export_path);

    assert_contains(&harness.harness, &html, "Pi Session");
    assert_contains(&harness.harness, &html, &format!("Session {session_id}"));
    assert_contains(&harness.harness, &html, &timestamp);
    assert_contains(&harness.harness, &html, &cwd);
    assert_contains(&harness.harness, &html, &message);
}

#[test]
fn e2e_cli_export_missing_input_is_error() {
    let harness = CliTestHarness::new("e2e_cli_export_missing_input_is_error");
    let missing = harness.harness.temp_path("missing.jsonl");
    let missing_arg = missing.display().to_string();
    let result = harness.run(&["--export", missing_arg.as_str()]);

    harness
        .harness
        .assert_log("assert exit_code != 0 for missing export input");
    assert_ne!(result.exit_code, 0);
    assert_contains_case_insensitive(&harness.harness, &result.stderr, "file not found");
}

#[cfg(unix)]
#[test]
fn e2e_cli_export_permission_denied_is_error() {
    use std::os::unix::fs::PermissionsExt;

    let harness = CliTestHarness::new("e2e_cli_export_permission_denied_is_error");
    let session_path = harness.harness.temp_path("session.jsonl");
    let _ = write_minimal_session(&session_path, harness.harness.temp_dir());

    let readonly_dir = harness.harness.temp_path("readonly");
    fs::create_dir_all(&readonly_dir).expect("create readonly dir");
    let mut perms = fs::metadata(&readonly_dir)
        .expect("stat readonly dir")
        .permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&readonly_dir, perms).expect("set readonly perms");

    let export_path = readonly_dir.join("export.html");
    let session_arg = session_path.display().to_string();
    let export_arg = export_path.display().to_string();
    let result = harness.run(&["--export", session_arg.as_str(), export_arg.as_str()]);

    harness
        .harness
        .assert_log("assert exit_code != 0 for permission denied");
    assert_ne!(result.exit_code, 0);
    assert_contains_case_insensitive(&harness.harness, &result.stderr, "permission");
}

#[test]
fn e2e_cli_print_mode_with_stdin_does_not_create_session_files() {
    let harness =
        CliTestHarness::new("e2e_cli_print_mode_with_stdin_does_not_create_session_files");
    let sessions_dir = PathBuf::from(
        harness
            .env
            .get("PI_SESSIONS_DIR")
            .expect("PI_SESSIONS_DIR")
            .clone(),
    );

    let result = harness.run_with_stdin(
        &[
            "--provider",
            "anthropic",
            "--model",
            "claude-3-opus-20240229",
            "-p",
        ],
        Some(b"Hello from stdin\n"),
    );

    harness
        .harness
        .assert_log("assert exit_code != 0 for missing API key in print mode");
    assert_ne!(result.exit_code, 0);
    let stderr_lower = result.stderr.to_lowercase();
    assert!(
        stderr_lower.contains("no api key") || stderr_lower.contains("no models"),
        "expected stderr to mention missing api key or models"
    );

    let jsonl_count = count_jsonl_files(&sessions_dir);
    harness
        .harness
        .assert_log("assert no session jsonl files created");
    assert_eq!(jsonl_count, 0, "expected no session jsonl files");
}

#[test]
fn e2e_cli_config_paths_honor_env_overrides() {
    let mut harness = CliTestHarness::new("e2e_cli_config_paths_honor_env_overrides");

    let env_root = harness.harness.temp_path("env-overrides");
    let agent_dir = env_root.join("agent-root");
    let config_path = env_root.join("settings-override.json");
    let sessions_dir = env_root.join("sessions-root");
    let packages_dir = env_root.join("packages-root");

    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(&config_path, "{}").expect("write override settings");

    harness.env.insert(
        "PI_CODING_AGENT_DIR".to_string(),
        agent_dir.display().to_string(),
    );
    harness.env.insert(
        "PI_CONFIG_PATH".to_string(),
        config_path.display().to_string(),
    );
    harness.env.insert(
        "PI_SESSIONS_DIR".to_string(),
        sessions_dir.display().to_string(),
    );
    harness.env.insert(
        "PI_PACKAGE_DIR".to_string(),
        packages_dir.display().to_string(),
    );

    let result = harness.run(&["config"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Global:  {}", config_path.display()),
    );
    let project_path = harness.harness.temp_dir().join(".pi/settings.json");
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Project: {}", project_path.display()),
    );
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Sessions: {}", sessions_dir.display()),
    );
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Auth:     {}", agent_dir.join("auth.json").display()),
    );
}

#[test]
fn e2e_cli_config_paths_fallback_to_agent_dir() {
    let mut harness = CliTestHarness::new("e2e_cli_config_paths_fallback_to_agent_dir");

    let env_root = harness.harness.temp_path("env-fallback");
    let agent_dir = env_root.join("agent-root");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    harness.env.insert(
        "PI_CODING_AGENT_DIR".to_string(),
        agent_dir.display().to_string(),
    );
    harness.env.remove("PI_CONFIG_PATH");
    harness.env.remove("PI_SESSIONS_DIR");
    harness.env.remove("PI_PACKAGE_DIR");

    let result = harness.run(&["config"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Global:  {}", agent_dir.join("settings.json").display()),
    );
    let project_path = harness.harness.temp_dir().join(".pi/settings.json");
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Project: {}", project_path.display()),
    );
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Sessions: {}", agent_dir.join("sessions").display()),
    );
    assert_contains(
        &harness.harness,
        &result.stdout,
        &format!("Auth:     {}", agent_dir.join("auth.json").display()),
    );
}

#[test]
fn e2e_cli_list_subcommand_works_offline() {
    let harness = CliTestHarness::new("e2e_cli_list_subcommand_works_offline");
    let result = harness.run(&["list"]);

    assert_exit_code(&harness.harness, &result, 0);
    assert_contains_case_insensitive(&harness.harness, &result.stdout, "packages");
}

#[cfg(unix)]
#[test]
fn e2e_cli_packages_install_list_remove_offline() {
    let mut harness = CliTestHarness::new("e2e_cli_packages_install_list_remove_offline");
    let stubs = harness.enable_offline_package_stubs();
    harness
        .harness
        .record_artifact("npm-invocations.jsonl", &stubs.npm_log);
    harness
        .harness
        .record_artifact("git-invocations.jsonl", &stubs.git_log);

    harness.harness.section("install local (project)");
    harness.harness.create_dir("local-pkg");
    fs::write(
        harness.harness.temp_path("local-pkg/README.md"),
        "local test package\n",
    )
    .expect("write local package marker");

    let result = harness.run(&["install", "local-pkg", "-l"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "Installed local-pkg");
    harness.snapshot_settings("after_install_local_project");

    harness.harness.section("install npm (project)");
    let result = harness.run(&["install", "npm:demo-pkg@1.0.0", "-l"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(
        &harness.harness,
        &result.stdout,
        "Installed npm:demo-pkg@1.0.0",
    );
    harness.snapshot_settings("after_install_npm_project");

    harness.harness.section("list (project)");
    let result = harness.run(&["list"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "Project packages:");
    assert_contains(&harness.harness, &result.stdout, "local-pkg");
    assert_contains(&harness.harness, &result.stdout, "npm:demo-pkg@1.0.0");

    let local_path = harness.harness.temp_dir().join("local-pkg");
    assert_contains(
        &harness.harness,
        &result.stdout,
        &local_path.display().to_string(),
    );
    let npm_install_path = harness
        .harness
        .temp_dir()
        .join(".pi")
        .join("npm")
        .join("node_modules")
        .join("demo-pkg");
    assert_contains(
        &harness.harness,
        &result.stdout,
        &npm_install_path.display().to_string(),
    );
    assert!(
        npm_install_path.exists(),
        "stub npm should create install path"
    );

    harness.harness.section("remove (project)");
    let result = harness.run(&["remove", "local-pkg", "-l"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "Removed local-pkg");
    let result = harness.run(&["remove", "npm:demo-pkg@1.0.0", "-l"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(
        &harness.harness,
        &result.stdout,
        "Removed npm:demo-pkg@1.0.0",
    );
    harness.snapshot_settings("after_remove_project");

    let result = harness.run(&["list"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "No packages installed.");
}

#[cfg(unix)]
#[test]
fn e2e_cli_packages_update_respects_pinning_offline() {
    let mut harness = CliTestHarness::new("e2e_cli_packages_update_respects_pinning_offline");
    let stubs = harness.enable_offline_package_stubs();
    harness
        .harness
        .record_artifact("npm-invocations.jsonl", &stubs.npm_log);
    harness
        .harness
        .record_artifact("git-invocations.jsonl", &stubs.git_log);

    harness.harness.section("install pinned + unpinned (user)");
    let result = harness.run(&["install", "npm:pinned@1.0.0"]);
    assert_exit_code(&harness.harness, &result, 0);
    let result = harness.run(&["install", "npm:unpinned"]);
    assert_exit_code(&harness.harness, &result, 0);
    harness.snapshot_settings("after_install_user");

    let settings_path = harness.global_settings_path();
    let settings = read_json_value(&settings_path);
    let packages = settings
        .get("packages")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let packages = packages
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>();
    assert!(packages.contains(&"npm:pinned@1.0.0"));
    assert!(packages.contains(&"npm:unpinned"));

    // Clear log so we only inspect the update stage.
    fs::write(&stubs.npm_log, "").expect("truncate npm log");

    harness.harness.section("update");
    let result = harness.run(&["update"]);
    assert_exit_code(&harness.harness, &result, 0);
    assert_contains(&harness.harness, &result.stdout, "Updated packages");
    harness.snapshot_settings("after_update_user");

    let invocations = read_invocations(&stubs.npm_log);
    let install_specs = invocations
        .iter()
        .filter_map(|inv| {
            if inv.argv.first().map(String::as_str) != Some("install") {
                return None;
            }
            let g_idx = inv.argv.iter().position(|arg| arg == "-g")?;
            inv.argv.get(g_idx + 1).cloned()
        })
        .collect::<Vec<_>>();

    assert!(
        install_specs.iter().any(|spec| spec == "unpinned"),
        "expected update to reinstall unpinned package; installs={install_specs:?}"
    );
    assert!(
        install_specs.iter().all(|spec| spec != "pinned@1.0.0"),
        "expected pinned package to be skipped on update; installs={install_specs:?}"
    );

    // Ensure the global settings file was not mutated by update.
    let settings_after = read_json_value(&settings_path);
    assert_eq!(
        settings.get("packages"),
        settings_after.get("packages"),
        "update should not rewrite settings.json"
    );
}

#[test]
fn e2e_cli_version_is_fast_enough_for_test_env() {
    let harness = CliTestHarness::new("e2e_cli_version_is_fast_enough_for_test_env");
    let result = harness.run(&["--version"]);

    assert_exit_code(&harness.harness, &result, 0);

    // Avoid hard <100ms assertions in CI; we only enforce that the CLI isn't hanging.
    harness.harness.assert_log("assert duration < 5s (sanity)");
    assert!(result.duration < Duration::from_secs(5));
}
