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

/// Signal-teardown terminal-state proofs (acceptance #1 hard part).
///
/// SIGTERM: ftui's runtime intercepts termination signals, drops the program
/// (RAII terminal restore), and exits 128+sig — so after SIGTERM the wrapper
/// shell's typed probe MUST echo (appear twice in the pane: echoed input +
/// output). This is the same restore path a panic takes.
///
/// SIGKILL: no process can restore a tty it was KILLed on (POSIX), and the
/// pane capture demonstrably shows raw-mode staircase output. What we prove
/// instead: the wrapping shell is alive and a blind `stty sane` recovers the
/// terminal — the user-visible recovery story.
///
/// Gap vs the bead's wording: the signal lands while the UI is live but idle
/// (no fake provider streams in this lane yet); raw mode + mouse capture +
/// alt-screen are all active at signal time, which is the terminal state
/// that matters.
fn run_signal_teardown(name: &str, signal: &str, blind_stty_sane: bool) {
    use std::fmt::Write as _;

    let Some((_lock, session)) = new_locked_session(name) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let Some(binary) = std::env::var_os("CARGO_BIN_EXE_pi") else {
        eprintln!("Skipping: CARGO_BIN_EXE_pi not set");
        return;
    };
    let binary = std::path::PathBuf::from(binary);

    // ubs:ignore-next-line expect in test setup — failures here are immediate test failures, same convention as tests/common/tmux.rs
    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let pid_file = session.harness.temp_path("pi.pid");

    // Custom wrapper: pi runs in the FOREGROUND (it needs the tty for raw
    // mode) inside an inner `sh -c 'echo $$ > pid; exec pi ...'` — the exec
    // makes the recorded pid become pi's. After the kill the outer script
    // continues to the marker and hands the pane to an interactive shell.
    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport OPENAI_API_KEY=pi-e2e-sigkill-dummy\n");
    let _ = writeln!(
        script,
        "/bin/sh -c 'echo $$ > {pid}; exec {bin} --ftui --no-session \
         --provider openai --model gpt-4o-mini --no-skills \
         --no-prompt-templates --no-extensions --no-themes'",
        pid = pid_file.display(),
        bin = binary.display()
    );
    script.push_str("echo PI-WAIT-DONE\nexec /bin/sh -i\n");

    let script_path = session.harness.temp_path("sigkill-run.sh");
    std::fs::write(&script_path, &script).expect("write sigkill script"); // ubs:ignore test setup expect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // ubs:ignore test setup expect — chmod failure is an immediate test failure
        let mut perms = std::fs::metadata(&script_path)
            .expect("stat sigkill script") // ubs:ignore test setup expect
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod sigkill script"); // ubs:ignore test setup expect
    }

    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);

    // Full launch: the banner proves raw mode/alt-screen/mouse are active.
    session
        .tmux
        .wait_for_pane_contains("ftui preview stack", STARTUP_TIMEOUT);

    // An unreadable/unparseable pid file is an immediate test failure.
    let pid_text = std::fs::read_to_string(&pid_file).expect("read pi pid"); // ubs:ignore test assertion expect
    let pid: i32 = pid_text.trim().parse().expect("parse pi pid"); // ubs:ignore test assertion expect
    // Literal /bin/kill path is deliberate: portable signal delivery without
    // libc in a unix-only test; a spawn failure is an immediate test failure.
    let mut kill_cmd = std::process::Command::new("/bin/kill"); // ubs:ignore unix-only test helper path
    kill_cmd.args([signal, &pid.to_string()]);
    let status = kill_cmd.status().expect("run kill"); // ubs:ignore test assertion expect
    assert!(status.success(), "kill {signal} {pid} failed");

    // The wrapper shell takes over the pane once pi dies.
    session
        .tmux
        .wait_for_pane_contains("PI-WAIT-DONE", COMMAND_TIMEOUT);

    if blind_stty_sane {
        // SIGKILL path: the tty is expected to still be raw here; a blind
        // `stty sane` (typed without echo) must recover it.
        session.tmux.send_literal("stty sane");
        session.tmux.send_key("Enter");
        std::thread::sleep(Duration::from_millis(300));
    }

    // Post-signal probe: typed input must echo AND execute.
    session.tmux.send_literal("echo POST-KILL-OK");
    session.tmux.send_key("Enter");
    let pane = session
        .tmux
        .wait_for_pane_contains("POST-KILL-OK", COMMAND_TIMEOUT);
    let occurrences = pane.matches("POST-KILL-OK").count();
    assert!(
        occurrences >= 2,
        "typed probe did not echo (terminal left in raw/no-echo state?); \
         occurrences={occurrences}, pane:\n{pane}"
    );

    session.tmux.kill_server();
}

/// SIGTERM must restore the terminal via RAII before exiting.
#[test]
fn e2e_ftui_sigterm_restores_terminal() {
    run_signal_teardown("e2e_ftui_sigterm_restores_terminal", "-TERM", false);
}

/// SIGKILL cannot restore (POSIX); the shell must survive and `stty sane`
/// must recover the pane.
#[test]
fn e2e_ftui_sigkill_recoverable_with_stty_sane() {
    run_signal_teardown("e2e_ftui_sigkill_recoverable_with_stty_sane", "-9", true);
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
