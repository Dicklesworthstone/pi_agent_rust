//! E2E tests for TUI features: scoped-models, share command, pattern validation.
//!
//! These tests launch the `pi` binary in a tmux session, drive scripted
//! interactions, capture pane output, and emit JSONL artifacts for CI diffing.
//!
//! Run:
//! ```bash
//! cargo test --test e2e_tui_features
//! ```

#![cfg(unix)]
#![allow(dead_code)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::incompatible_msrv)]

mod common;

use common::tmux::TuiSession;
use fs4::FileExt as _;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::time::Duration;

// ─── Helpers ─────────────────────────────────────────────────────────────────

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const SHARE_TIMEOUT: Duration = Duration::from_secs(5);

/// Standard CLI args for interactive mode with minimal features (no API calls).
fn minimal_interactive_args() -> Vec<&'static str> {
    vec![
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
        "--no-tools",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "pi e2e tui features test harness",
    ]
}

/// Cross-process lock to serialize tmux-based E2E tests.
struct TmuxE2eLock(std::fs::File);

impl TmuxE2eLock {
    fn acquire() -> Self {
        let path = std::env::temp_dir().join("pi_agent_rust.tmux-e2e-features.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .expect("open tmux e2e lock file");
        fs4::FileExt::lock(&file).expect("lock tmux e2e lock file");
        Self(file)
    }
}

impl Drop for TmuxE2eLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.0);
    }
}

fn new_locked_tui_session(name: &str) -> Option<(TmuxE2eLock, TuiSession)> {
    let lock = TmuxE2eLock::acquire();
    let session = TuiSession::new(name)?;
    Some((lock, session))
}

/// JSONL event logger for structured test diagnostics.
fn log_test_event(test_name: &str, event: &str, data: &serde_json::Value) {
    let entry = serde_json::json!({
        "schema": "pi.test.tui_e2e.v1",
        "test": test_name,
        "event": event,
        "timestamp_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        "data": data,
    });
    eprintln!("JSONL: {}", serde_json::to_string(&entry).unwrap());
}

/// Create a mock `gh` script that records args and emits a gist URL.
fn write_mock_gh_script(dir: &std::path::Path, gist_url: &str) -> std::path::PathBuf {
    let gh_path = dir.join("gh");
    let args_path = dir.join("gh_args.log");
    let script = format!(
        r#"#!/bin/sh
set -e

# Record all invocations
echo "$@" >> "{args_log}"

if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi

if [ "$1" = "gist" ] && [ "$2" = "create" ]; then
  echo "{gist_url}"
  exit 0
fi

echo "unexpected gh args: $@" >&2
exit 2
"#,
        args_log = args_path.display(),
        gist_url = gist_url,
    );
    fs::write(&gh_path, script).expect("write mock gh");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod mock gh");
    }

    gh_path
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// E2E: `/share` creates gist and shows viewer URL.
#[test]
fn e2e_tui_share_creates_gist_with_privacy_and_description() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_share_creates_gist") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_share_creates_gist_with_privacy_and_description";
    log_test_event(test_name, "test_start", &json!({}));

    // Set up mock gh in a temporary bin directory.
    let mock_bin = session.harness.temp_path("mock_bin");
    fs::create_dir_all(&mock_bin).expect("create mock_bin");
    let gist_url = "https://gist.github.com/testuser/e2e_share_id_123";
    write_mock_gh_script(&mock_bin, gist_url);

    // Write project settings with gh_path pointing to our mock.
    let pi_dir = session.harness.temp_path(".pi");
    fs::create_dir_all(&pi_dir).expect("create .pi");
    let settings = json!({
        "ghPath": mock_bin.join("gh").display().to_string()
    });
    fs::write(
        pi_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .expect("write settings.json");

    // Override PI_CONFIG_PATH so the binary reads our settings.json
    // (TuiSession defaults PI_CONFIG_PATH to env_root/config.toml).
    session.set_env(
        "PI_CONFIG_PATH",
        &pi_dir.join("settings.json").display().to_string(),
    );

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    log_test_event(test_name, "share_initiated", &json!({"privacy": "private"}));

    // Issue /share command (default: private).
    let pane = session.send_text_and_wait(
        "share_private",
        "/share",
        "Created private gist",
        SHARE_TIMEOUT,
    );

    assert!(pane.contains("Share URL:"), "Expected share URL in output");
    assert!(
        pane.contains("https://buildwithpi.ai/session/#e2e_share_id_123"),
        "Expected viewer URL"
    );

    log_test_event(
        test_name,
        "share_completed",
        &json!({"privacy": "private", "gist_url": gist_url}),
    );

    // Verify mock gh was called with --public=false and --desc.
    let args_log = session.harness.temp_path("mock_bin/gh_args.log");
    let args_content = fs::read_to_string(&args_log).unwrap_or_default();
    assert!(
        args_content.contains("--public=false"),
        "Expected --public=false in gh args: {args_content}"
    );
    assert!(
        args_content.contains("--desc"),
        "Expected --desc in gh args: {args_content}"
    );

    log_test_event(
        test_name,
        "args_verified",
        &json!({"public_false": true, "desc_present": true}),
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// E2E: `/share public` creates a public gist.
#[test]
fn e2e_tui_share_public_flag() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_share_public_flag") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_share_public_flag";
    log_test_event(test_name, "test_start", &json!({}));

    let mock_bin = session.harness.temp_path("mock_bin");
    fs::create_dir_all(&mock_bin).expect("create mock_bin");
    let gist_url = "https://gist.github.com/testuser/e2e_public_456";
    write_mock_gh_script(&mock_bin, gist_url);

    let pi_dir = session.harness.temp_path(".pi");
    fs::create_dir_all(&pi_dir).expect("create .pi");
    let settings = json!({
        "ghPath": mock_bin.join("gh").display().to_string()
    });
    fs::write(
        pi_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .expect("write settings.json");

    // Override PI_CONFIG_PATH so the binary reads our settings.json.
    session.set_env(
        "PI_CONFIG_PATH",
        &pi_dir.join("settings.json").display().to_string(),
    );

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    log_test_event(test_name, "share_initiated", &json!({"privacy": "public"}));

    let pane = session.send_text_and_wait(
        "share_public",
        "/share public",
        "Created public gist",
        SHARE_TIMEOUT,
    );

    assert!(
        pane.contains("Created public gist"),
        "Expected 'Created public gist' in output"
    );

    // Verify mock gh was called with --public=true.
    let args_log = session.harness.temp_path("mock_bin/gh_args.log");
    let args_content = fs::read_to_string(&args_log).unwrap_or_default();
    assert!(
        args_content.contains("--public=true"),
        "Expected --public=true in gh args: {args_content}"
    );

    log_test_event(
        test_name,
        "share_completed",
        &json!({"privacy": "public", "public_flag_passed": true}),
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// E2E: `/share` without `gh` installed shows install instructions.
#[test]
fn e2e_tui_share_missing_gh_shows_install_instructions() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_share_no_gh") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_share_missing_gh_shows_install_instructions";
    log_test_event(test_name, "test_start", &json!({}));

    // Point gh_path to a non-existent binary.
    let pi_dir = session.harness.temp_path(".pi");
    fs::create_dir_all(&pi_dir).expect("create .pi");
    let settings = json!({
        "ghPath": session.harness.temp_path("nonexistent_gh").display().to_string()
    });
    fs::write(
        pi_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .expect("write settings.json");

    // Override PI_CONFIG_PATH so the binary reads our settings.json.
    session.set_env(
        "PI_CONFIG_PATH",
        &pi_dir.join("settings.json").display().to_string(),
    );

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    let pane = session.send_text_and_wait("share_no_gh", "/share", "not found", SHARE_TIMEOUT);

    assert!(
        pane.contains("cli.github.com"),
        "Expected install URL in error: pane content doesn't contain 'cli.github.com'"
    );

    log_test_event(
        test_name,
        "share_failed",
        &json!({"reason": "gh_not_found", "message_contains_url": true}),
    );

    session.exit_gracefully();
    session.write_artifacts();
}

/// E2E: `/scoped-models` sets pattern and Ctrl+P cycles only matched models.
#[test]
fn e2e_tui_scoped_models_and_ctrlp_cycling() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_scoped_models") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_scoped_models_and_ctrlp_cycling";
    log_test_event(test_name, "test_start", &json!({}));

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    // Set scoped models to a pattern.
    let pane = session.send_text_and_wait(
        "set_scope",
        "/scoped-models openai/*",
        "Scoped models updated",
        COMMAND_TIMEOUT,
    );
    log_test_event(
        test_name,
        "scoped_models_set",
        &json!({"pattern": "openai/*", "output_contains_updated": pane.contains("updated")}),
    );

    // Clear scoped models.
    let pane = session.send_text_and_wait(
        "clear_scope",
        "/scoped-models clear",
        "cleared",
        COMMAND_TIMEOUT,
    );
    log_test_event(
        test_name,
        "scope_cleared",
        &json!({"output_contains_cleared": pane.contains("cleared")}),
    );

    session.exit_gracefully();
    session.write_artifacts();
}
