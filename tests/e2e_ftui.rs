//! FrankenTUI preview stack E2E via tmux (bd-cv653.9.1, acceptance lane T8).
//!
//! Launches `pi --ftui` in a real PTY (tmux pane), drives the ported surfaces
//! (banner, /help, display-only `!` bash, quit), and proves the session tears
//! down cleanly. The inline-mode smoke covers the scrollback-preserving
//! runtime path end to end.
//!
//! Run (the `ftui` feature gates both the test and the binary build):
//! ```bash
//! cargo test --test e2e_ftui --features ftui
//! ```

#![cfg(all(unix, feature = "ftui"))]
#![allow(dead_code)]
#![allow(clippy::doc_markdown)]

mod common;

use common::tmux::TuiSession;
use std::fs::OpenOptions;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Serialize against every other tmux-based E2E lane (same lock file as
/// tests/e2e_tui.rs — cross-process via fs4, in-process via a static mutex).
static TMUX_E2E_IN_PROCESS_LOCK: Mutex<()> = Mutex::new(());

struct TmuxE2eLock {
    _thread_guard: MutexGuard<'static, ()>,
    file: std::fs::File,
}

impl TmuxE2eLock {
    fn acquire() -> Self {
        let thread_guard = TMUX_E2E_IN_PROCESS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = std::env::temp_dir().join("pi_agent_rust.tmux-e2e.lock");
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true).truncate(false);
        let file = opts.open(&path).expect("open tmux e2e lock file"); // ubs:ignore test harness setup — failed lock open is an immediate test failure (same pattern as tests/e2e_tui.rs)
        fs4::FileExt::lock(&file).expect("lock tmux e2e lock file");
        Self {
            _thread_guard: thread_guard,
            file,
        }
    }
}

impl Drop for TmuxE2eLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

fn new_locked_session(name: &str) -> Option<(TmuxE2eLock, TuiSession)> {
    let lock = TmuxE2eLock::acquire();
    let session = TuiSession::new(name)?;
    Some((lock, session))
}

/// CLI args for the preview stack: resource classes disabled so the
/// workspace-trust gate stays out of the way (same rationale as
/// `base_interactive_args` in tests/e2e_tui.rs), ephemeral session, pinned
/// provider/model against the harness's dummy keys.
fn ftui_args() -> Vec<&'static str> {
    vec![
        "--ftui",
        "--no-session",
        "--provider",
        "openai",
        "--model",
        "gpt-4o-mini",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
    ]
}

fn quit_and_assert_clean(session: &TuiSession) {
    session.tmux.send_key("C-c");
    let start = std::time::Instant::now();
    while session.tmux.session_exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "pi --ftui did not exit within 10s of ctrl+c"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Acceptance #1 lane: launch on the ftui runtime, exercise UI-side routing
/// (/help) and a driver round-trip (`!` bash), quit on ctrl+c, and verify the
/// tmux session tears down (RAII terminal restore — a stuck raw-mode terminal
/// would leave the pane process alive).
#[test]
fn e2e_ftui_launch_help_bash_quit() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_launch_help_bash_quit") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&ftui_args());

    // The banner is sent by the driver AFTER the SDK session is created, so
    // seeing it proves the full launch path (runtime, session, bridge).
    let pane = session.wait_and_capture("startup", "ftui preview stack", STARTUP_TIMEOUT);
    assert!(
        pane.contains("pi ·"),
        "header missing from ftui frame; got:\n{pane}"
    );

    // UI-side slash routing.
    let pane =
        session.send_text_and_wait("help", "/help", "ftui preview commands", COMMAND_TIMEOUT);
    assert!(
        pane.contains("/model"),
        "help text incomplete; got:\n{pane}"
    );

    // Driver round-trip: display-only bash.
    let pane = session.send_text_and_wait(
        "bash",
        "!echo pi-ftui-e2e-marker",
        "pi-ftui-e2e-marker",
        COMMAND_TIMEOUT,
    );
    assert!(
        pane.contains("$ echo pi-ftui-e2e-marker") || pane.contains("pi-ftui-e2e-marker"),
        "bash output missing; got:\n{pane}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// Acceptance #2 lane: the inline (scrollback-preserving) runtime path boots,
/// renders, and quits cleanly. Scrollback-content preservation itself is
/// asserted by the doctor capture follow-up; this pins the mode end to end.
#[test]
fn e2e_ftui_inline_smoke() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_inline_smoke") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let mut args = ftui_args();
    args.push("--inline");
    session.launch(&args);

    let pane = session.wait_and_capture("inline_startup", "pi ·", STARTUP_TIMEOUT);
    assert!(pane.contains("pi ·"), "inline header missing; got:\n{pane}");

    quit_and_assert_clean(&session);
    session.write_artifacts();
}
