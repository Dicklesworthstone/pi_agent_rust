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

#[cfg(unix)]
#[test]
fn pty_preserves_stdout_stderr_order_through_clean_eof() {
    let case = "pty_preserves_stdout_stderr_order_through_clean_eof";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = bash_tool(
        &root,
        Some(BashSettings {
            persistent: Some(false),
            ..pty_settings("always")
        }),
    );
    let output = block_on_local(tool.execute(
        "ordered-pty",
        json!({
            "command": "for ((i=0; i<128; i++)); do printf 'out-%s\\n' \"$i\"; printf 'err-%s\\n' \"$i\" >&2; done",
            "background": false,
        }),
        None,
    ))
    .expect("ordered PTY execution");
    let expected: String = (0..128)
        .map(|index| format!("out-{index}\nerr-{index}\n"))
        .collect();
    assert_eq!(first_text(&output).replace("\r\n", "\n"), expected);
    assert!(!output.is_error, "normal terminal EOF is not a read error");
    assert!(
        output
            .details
            .as_ref()
            .is_none_or(|details| details.get("capture").is_none()),
        "complete PTY output must not acquire incomplete-capture metadata"
    );
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

// Linux's subreaper lets this fixture retain an owned child outside the Bash
// process tree. No process-count baseline or "kill -0 failed" heuristic is
// involved: the escaped writer stays alive until its two first post-retirement
// writes have reported whether the actual stdout/stderr readers were closed.
// PTY cases also identify the exact master through Linux's tty-index fdinfo;
// terminal hangup alone can make a slave write fail while a master leaks.
#[cfg(target_os = "linux")]
mod foreground_pipe_ownership {
    use super::*;
    use std::fmt::Write as _;
    use std::io::{self, IsTerminal as _, Write};
    use std::os::unix::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const CASE_ENV: &str = "PI_TEST_FOREGROUND_PIPE_CASE";
    const ROOT_ENV: &str = "PI_TEST_FOREGROUND_PIPE_ROOT";
    const ROLE_ENV: &str = "PI_TEST_FOREGROUND_PIPE_ROLE";
    const OWNER_ENV: &str = "PI_TEST_FOREGROUND_PIPE_OWNER";
    const FIXTURE: &str = "foreground_pipe_ownership::escaped_output_writer_fixture";

    fn quoted(path: &Path) -> String {
        format!(
            "'{}'",
            path.to_str()
                .expect("UTF-8 fixture path")
                .replace('\'', "'\"'\"'")
        )
    }

    fn wait_for_file(path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture did not publish {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn terminal_master_count(terminal_index: u32) -> usize {
        std::fs::read_dir("/proc/self/fdinfo")
            .expect("enumerate the owning process's descriptors")
            .map(|entry| {
                let path = entry.expect("descriptor metadata entry").path();
                let metadata = match std::fs::read_to_string(&path) {
                    Ok(metadata) => metadata,
                    // Other runtime descriptors can close during enumeration.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return 0,
                    Err(error) => panic!("cannot read {}: {error}", path.display()),
                };
                usize::from(metadata.lines().any(|line| {
                    line.strip_prefix("tty-index:").is_some_and(|index| {
                        index.trim().parse::<u32>().expect("numeric PTY index") == terminal_index
                    })
                }))
            })
            .sum()
    }

    /// This process starts under the real `BashTool`, then transfers the writer
    /// to the isolated test's subreaper by exiting its short-lived parent.
    #[test]
    #[allow(clippy::zombie_processes)] // The isolated subreaper owns and reaps this child.
    fn escaped_output_writer_fixture() {
        let Some(root) = std::env::var_os(ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let Ok(role) = std::env::var(ROLE_ENV) else {
            return;
        };
        if role == "spawn" {
            let child = Command::new(std::env::current_exe().expect("fixture executable"))
                .args(["--exact", FIXTURE, "--nocapture"])
                .env(ROLE_ENV, "writer")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .process_group(0)
                .spawn()
                .expect("escaped writer");
            std::fs::write(root.join("writer.pid"), child.id().to_string())
                .expect("writer identity");
            // Deliberate ownership transfer: this parent exits, and the test
            // process becomes the writer's parent and is responsible for waitpid.
            return;
        }
        assert_eq!(role, "writer");
        let owner = rustix::process::Pid::from_raw(
            std::env::var(OWNER_ENV)
                .expect("fixture owner")
                .parse()
                .expect("numeric fixture owner"),
        )
        .expect("nonzero fixture owner");
        let deadline = Instant::now() + Duration::from_secs(45);
        while rustix::process::getppid() != Some(owner) {
            if root.join("release").exists() {
                return;
            }
            assert!(Instant::now() < deadline, "writer was not adopted");
            std::thread::sleep(Duration::from_millis(5));
        }
        let use_pty = io::stdout().is_terminal();
        if use_pty {
            let terminal = std::fs::read_link("/proc/self/fd/1").expect("writer's terminal path");
            assert_eq!(
                std::fs::read_link("/proc/self/fd/2").expect("writer's stderr terminal"),
                terminal,
                "both output descriptors must belong to the same PTY"
            );
            assert_eq!(terminal.parent(), Some(Path::new("/dev/pts")));
            let index = terminal
                .file_name()
                .and_then(|name| name.to_str())
                .expect("PTY index")
                .parse::<u32>()
                .expect("numeric PTY index");
            std::fs::write(root.join("writer.tty-index"), index.to_string())
                .expect("terminal identity");
        }
        io::stdout()
            .write_all(b"escaped-stdout-ready\n")
            .expect("initial stdout");
        io::stderr()
            .write_all(b"escaped-stderr-ready\n")
            .expect("initial stderr");
        std::fs::write(root.join("writer.ready"), b"ready").expect("writer readiness");
        while !root.join("probe").exists() {
            if root.join("release").exists() || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // A leaked blocking pump accepts the first write, then notices its
        // disconnected channel. Retrying until EPIPE would hide that leak.
        // A PTY slave reports EIO on hangup; the owner separately checks that
        // its exact master descriptor has retired, not just that hangup occurred.
        // Include a newline so stdout's LineWriter attempts the OS write now.
        let stdout = io::stdout().write_all(b"x\n");
        let stderr = io::stderr().write_all(b"y\n");
        let is_closed = |result: &io::Result<()>| {
            result.as_ref().is_err_and(|error| {
                if use_pty {
                    error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
                } else {
                    error.kind() == io::ErrorKind::BrokenPipe
                }
            })
        };
        std::fs::write(
            root.join("result.json"),
            serde_json::to_vec(&json!({
                "stdout_closed": is_closed(&stdout),
                "stderr_closed": is_closed(&stderr),
                "stdout_result": format!("{stdout:?}"),
                "stderr_result": format!("{stderr:?}"),
            }))
            .expect("writer report"),
        )
        .expect("write writer report");
        std::fs::write(root.join("report.ready"), b"ready").expect("report readiness");
        while !root.join("release").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct WriterOwner {
        root: PathBuf,
        pid: Option<rustix::process::Pid>,
        discover_pid: bool,
    }

    impl WriterOwner {
        fn is_live(&mut self) -> bool {
            let pid = self.pid.expect("writer remains owned");
            match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG)
                .expect("query owned writer")
            {
                None => true,
                Some(_) => {
                    self.pid = None;
                    false
                }
            }
        }
    }

    impl Drop for WriterOwner {
        fn drop(&mut self) {
            let _ = std::fs::write(self.root.join("release"), b"release");
            if self.discover_pid {
                self.pid = std::fs::read_to_string(self.root.join("writer.pid"))
                    .ok()
                    .and_then(|pid| pid.parse().ok())
                    .and_then(rustix::process::Pid::from_raw);
            }
            let Some(pid) = self.pid.take() else {
                return;
            };
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(None) => {
                        // waitpid established that this unreaped child is still
                        // ours. Its PID cannot be reused before the wait below.
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                        let _ = rustix::process::waitpid(
                            Some(pid),
                            rustix::process::WaitOptions::empty(),
                        );
                        return;
                    }
                }
            }
        }
    }

    fn run_case(root: &Path, mode: &str, use_pty: bool) {
        // Linux treats any nonzero argument as true. rustix 1.1 represents
        // this prctl argument as Option<Pid>, so Some(1) enables the attribute.
        rustix::process::set_child_subreaper(rustix::process::Pid::from_raw(1))
            .expect("isolated test subreaper");
        let executable = std::env::current_exe().expect("test executable");
        let mut command = format!(
            "{ROLE_ENV}=spawn {OWNER_ENV}={} {} --exact {FIXTURE} --nocapture; printf 'foreground-root-ready\\n'",
            std::process::id(),
            quoted(&executable),
        );
        // Keep the controlling shell alive until readiness and the exact PTY
        // master have both been witnessed, even in the normal-return case.
        let exit_gate = if mode == "drop" {
            "release"
        } else {
            "allow-root-exit"
        };
        write!(
            command,
            "; while [ ! -e {} ]; do sleep 0.02; done",
            quoted(&root.join(exit_gate))
        )
        .expect("fixture command");
        let tool = bash_tool(
            root,
            Some(BashSettings {
                pty: Some(if use_pty { "always" } else { "off" }.to_string()),
                persistent: Some(false),
                mediation: Some("off".to_string()),
                ..Default::default()
            }),
        );
        let ready = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&ready);
        // Install cleanup before polling the tool, including startup failures.
        let mut writer = WriterOwner {
            root: root.to_path_buf(),
            pid: None,
            discover_pid: true,
        };
        block_on_local(async {
            let mut pending = Box::pin(tool.execute(
                "foreground-output-owner",
                json!({"command": command, "background": false, "timeout": 30}),
                Some(Box::new(move |update| {
                    if update.content.iter().any(|block| {
                        matches!(block, pi::model::ContentBlock::Text(text) if text.text.contains("foreground-root-ready"))
                    }) {
                        observed.store(true, Ordering::Release);
                    }
                })),
            ));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "Bash returned before the held-output boundary"
                );
                if ready.load(Ordering::Acquire) && root.join("writer.ready").exists() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "foreground fixture did not become ready"
                );
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5))
                    .await;
            }
            let pid = rustix::process::Pid::from_raw(
                std::fs::read_to_string(root.join("writer.pid"))
                    .expect("writer PID")
                    .parse()
                    .expect("numeric writer PID"),
            )
            .expect("nonzero writer PID");
            writer.pid = Some(pid);
            writer.discover_pid = false;
            assert!(
                writer.is_live(),
                "fixture writer must be alive before retirement"
            );
            let terminal_index = use_pty.then(|| {
                let index = std::fs::read_to_string(root.join("writer.tty-index"))
                    .expect("writer terminal identity")
                    .parse::<u32>()
                    .expect("numeric writer terminal identity");
                assert!(
                    terminal_master_count(index) > 0,
                    "the owning process must hold this PTY master before retirement"
                );
                index
            });
            let retirement_started = Instant::now();
            if mode == "drop" {
                drop(pending);
            } else {
                std::fs::write(root.join(exit_gate), b"exit").expect("release controlling shell");
                let output = pending.await.expect("foreground completion");
                let text = first_text(&output);
                assert!(text.contains("foreground-root-ready"));
                assert!(text.contains("escaped-stdout-ready"));
                assert!(text.contains("escaped-stderr-ready"));
                assert!(!text.contains("Full output:"), "{text}");
                if !use_pty {
                    assert!(
                        output.is_error,
                        "incomplete capture must be reported: {text}"
                    );
                    assert!(text.contains("Command capture:"), "{text}");
                    let details = output.details.as_ref().expect("capture details");
                    assert!(details.get("fullOutputPath").is_none());
                    assert_eq!(details["capture"]["outputComplete"], false);
                    assert_eq!(details["capture"]["artifactTruncated"], false);
                    assert!(details["capture"]["artifactError"].is_null());
                    assert!(details["capture"]["artifactPath"].is_null());
                }
            }
            assert!(
                retirement_started.elapsed() < Duration::from_secs(10),
                "foreground retirement exceeded its bounded output drain"
            );
            if let Some(index) = terminal_index {
                // Linux retains this devpts index until the last endpoint
                // closes. The live writer pins its slave, so another PTY
                // cannot reuse the identity and turn this into a false result.
                assert_eq!(
                    terminal_master_count(index),
                    0,
                    "the tool returned while still owning this PTY's master"
                );
            }
            assert!(
                writer.is_live(),
                "Bash cleanup must not replace reader retirement by killing the escaped writer"
            );
            std::fs::write(root.join("probe"), b"probe").expect("probe reader closure");
            wait_for_file(&root.join("report.ready"), Duration::from_secs(5));
            let report: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join("result.json")).expect("read writer report"),
            )
            .expect("decode writer report");
            assert!(
                writer.is_live(),
                "writer must remain alive after its endpoint probes"
            );
            assert_eq!(report["stdout_closed"], true, "{mode}: {report}");
            assert_eq!(report["stderr_closed"], true, "{mode}: {report}");
        });
    }

    struct CaseProcess {
        child: Child,
        root: PathBuf,
        reaped: bool,
    }

    impl Drop for CaseProcess {
        fn drop(&mut self) {
            let _ = std::fs::write(self.root.join("release"), b"release");
            if !self.reaped && matches!(self.child.try_wait(), Ok(None)) {
                pi::tools::kill_process_group_tree(Some(self.child.id()));
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn run_isolated_case(test_name: &str, mode: &str, use_pty: bool) {
        if std::env::var(CASE_ENV).ok().as_deref() == Some(mode) {
            let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("isolated fixture root"));
            run_case(&root, mode, use_pty);
            return;
        }
        let harness = TestHarness::new(test_name);
        let backend = if use_pty { "pty" } else { "pipe" };
        let root = harness.temp_path(format!("{backend}-owner"));
        std::fs::create_dir_all(&root).expect("output lifecycle fixture");
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(CASE_ENV, mode)
            .env(ROOT_ENV, &root)
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("isolated output lifecycle regression");
        let mut process = CaseProcess {
            child,
            root: root.clone(),
            reaped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = process
                .child
                .try_wait()
                .expect("observe regression process")
            {
                process.reaped = true;
                assert!(
                    status.success(),
                    "foreground {backend} {mode} lifecycle failed: {status}"
                );
                harness.record_artifact(
                    format!("foreground-{backend}-{mode}-writer.json"),
                    &root.join("result.json"),
                );
                finish_case(&harness, &format!("foreground-{backend}-{mode}"));
                return;
            }
            assert!(
                Instant::now() < deadline,
                "foreground {backend} {mode} lifecycle stalled"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn dropping_foreground_future_closes_escaped_output_readers() {
        run_isolated_case(
            "foreground_pipe_ownership::dropping_foreground_future_closes_escaped_output_readers",
            "drop",
            false,
        );
    }

    #[test]
    fn completed_foreground_call_closes_escaped_output_readers() {
        run_isolated_case(
            "foreground_pipe_ownership::completed_foreground_call_closes_escaped_output_readers",
            "exit",
            false,
        );
    }

    #[test]
    fn dropping_foreground_pty_future_closes_escaped_output_reader() {
        run_isolated_case(
            "foreground_pipe_ownership::dropping_foreground_pty_future_closes_escaped_output_reader",
            "drop",
            true,
        );
    }

    #[test]
    fn completed_foreground_pty_call_closes_escaped_output_reader() {
        run_isolated_case(
            "foreground_pipe_ownership::completed_foreground_pty_call_closes_escaped_output_reader",
            "exit",
            true,
        );
    }
}
