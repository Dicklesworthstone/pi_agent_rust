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
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::time::Duration;

// ─── Helpers ─────────────────────────────────────────────────────────────────

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const SHARE_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall budget for resending `/share` while the session reports busy.
const SHARE_RETRY_BUDGET: Duration = Duration::from_secs(30);

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
        // Classic charmed stack: these scenarios assert its pane text; the
        // default FTUI stack has its own coverage in tests/e2e_ftui.rs.
        "--classic",
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

/// Send `/share`, resending while each attempt draws a fresh "Session is busy"
/// refusal (the documented client contract while a session update is still
/// persisting), and return the pane once the success message's last paragraph
/// ("Share URL:") is on screen, plus the attempt and refusal counts.
fn send_share_until_ready(session: &TuiSession) -> (String, usize, usize) {
    let share_deadline = std::time::Instant::now() + SHARE_RETRY_BUDGET;
    let mut busy_replies = 0usize;
    let mut attempts = 0usize;
    let pane = loop {
        attempts += 1;
        session.tmux.send_literal("/share");
        session.tmux.send_key("Enter");
        let pane = session
            .tmux
            .wait_for_pane_contains_any(&["Share URL:", "Session is busy"], SHARE_TIMEOUT);
        if pane.contains("Share URL:") {
            break pane;
        }
        let busy_now = pane.matches("Session is busy").count();
        if busy_now > busy_replies && std::time::Instant::now() < share_deadline {
            busy_replies = busy_now;
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        // No fresh refusal: the share is in flight, wait for its URL.
        break session
            .tmux
            .wait_for_pane_contains("Share URL:", SHARE_TIMEOUT);
    };
    (pane, attempts, busy_replies)
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
    let uploaded_path = dir.join("uploaded.html");
    let script = format!(
        r#"#!/bin/sh
set -e

# Record all invocations
echo "$@" >> "{args_log}"

if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi

if [ "$1" = "gist" ] && [ "$2" = "create" ]; then
  upload_path=""
  for arg in "$@"; do
    upload_path="$arg"
  done
  cp "$upload_path" "{uploaded}"
  echo "{gist_url}"
  exit 0
fi

echo "unexpected gh args: $@" >&2
exit 2
"#,
        args_log = args_path.display(),
        uploaded = uploaded_path.display(),
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

/// E2E: `/share` creates a secret, unlisted gist and shows viewer URL.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_tui_share_creates_secret_gist_with_visibility_warning() {
    let Some((_lock, mut session)) = new_locked_tui_session("e2e_tui_share_creates_gist") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_share_creates_secret_gist_with_visibility_warning";
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
    // `.pi/settings.json` inside the tmux working directory is a workspace
    // trust surface; without the automation override the classic TUI shows
    // the trust prompt instead of the welcome banner.
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    session.send_text_and_wait(
        "name_shared_session",
        "/name share-upload-sentinel",
        "Session name: share-upload-sentinel",
        COMMAND_TIMEOUT,
    );

    log_test_event(
        test_name,
        "share_initiated",
        &json!({"visibility": "secret"}),
    );

    // Issue /share command (secret/unlisted, but not access-controlled).
    // The preceding /name update may still be persisting, in which case the
    // product answers "Session is busy; retry `/share` after the current
    // session update finishes". Retrying is the documented contract, so
    // resend while each attempt draws a fresh refusal, then wait for the last
    // paragraph of the success message ("Share URL:") so the capture is not
    // taken between two frames of the same message.
    let (pane, attempts, busy_replies) = send_share_until_ready(&session);
    log_test_event(
        test_name,
        "share_sent",
        &json!({"attempts": attempts, "busy_replies": busy_replies}),
    );

    assert!(
        pane.contains("Created secret gist"),
        "Expected the secret-gist notice in output: {pane}"
    );
    assert!(pane.contains("Share URL:"), "Expected share URL in output");
    assert!(
        pane.contains("https://buildwithpi.ai/session/#e2e_share_id_123"),
        "Expected viewer URL"
    );

    let uploaded_html = fs::read_to_string(session.harness.temp_path("mock_bin/uploaded.html"))
        .expect("mock gh must preserve the exact uploaded HTML");
    assert!(
        uploaded_html.contains("share-upload-sentinel"),
        "uploaded HTML omitted current session content"
    );
    assert!(
        !uploaded_html.contains(session.harness.temp_dir().to_string_lossy().as_ref()),
        "uploaded HTML leaked local cwd: {uploaded_html}"
    );
    assert!(
        !uploaded_html.contains("cwd:"),
        "uploaded HTML retained cwd metadata: {uploaded_html}"
    );

    log_test_event(
        test_name,
        "share_completed",
        &json!({"visibility": "secret", "access_controlled": false, "gist_url": gist_url}),
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

/// E2E: `/share` rejects public sharing before invoking `gh`.
#[test]
fn e2e_tui_share_rejects_public_argument_without_invoking_gh() {
    let Some((_lock, mut session)) =
        new_locked_tui_session("e2e_tui_share_rejects_public_argument")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let test_name = "e2e_tui_share_rejects_public_argument_without_invoking_gh";
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
    // `.pi/settings.json` inside the tmux working directory is a workspace
    // trust surface; without the automation override the classic TUI shows
    // the trust prompt instead of the welcome banner.
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

    session.harness.section("launch");
    session.launch(&minimal_interactive_args());
    session.wait_and_capture("startup", "Welcome to Pi!", STARTUP_TIMEOUT);

    log_test_event(test_name, "public_share_rejected", &json!({}));

    let pane = session.send_text_and_wait(
        "share_public_rejected",
        "/share public",
        "public sharing is disabled",
        SHARE_TIMEOUT,
    );

    assert!(
        pane.contains("public sharing is disabled"),
        "Expected explicit secret-gist guidance"
    );

    let args_log = session.harness.temp_path("mock_bin/gh_args.log");
    assert!(
        !args_log.exists(),
        "rejected public share unexpectedly invoked gh: {}",
        fs::read_to_string(&args_log).unwrap_or_default()
    );

    log_test_event(
        test_name,
        "gh_not_invoked",
        &json!({"secret_by_construction": true}),
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
    // `.pi/settings.json` inside the tmux working directory is a workspace
    // trust surface; without the automation override the classic TUI shows
    // the trust prompt instead of the welcome banner.
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

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
