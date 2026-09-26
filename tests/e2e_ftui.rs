//! `FrankenTUI` preview stack E2E via `tmux` (bd-cv653.9.1, acceptance lane T8).
//!
//! Launches `pi --ftui` in a real PTY (tmux pane), drives the ported surfaces
//! (banner, `/help`, display-only `!` bash, quit), and proves the session tears
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

/// Patience budgets for the tmux-driven FTUI lane.
///
/// These drive a real `pi` inside a real tmux pane and poll the pane's contents
/// until the expected text appears, so every wait here is bounded by how fast
/// the machine can start a process, render a frame, and let tmux report it.
///
/// They were 30 and 15 seconds. That is comfortable on an idle worker and not
/// comfortable in a full lane: `e2e_ftui_wheel_scroll_inside_tmux` failed once
/// during the ftui 0.7 bump with "wheel-up did not scroll the conversation",
/// and the run that failed took 117.73 seconds for this binary against roughly
/// 12 to 14 seconds for two runs that passed — the same suite, the same commit,
/// a busier machine. Read literally, a 15 second budget on a worker running an
/// order of magnitude slow is about a second and a half of effective time.
///
/// Raising them costs nothing when things are healthy, because
/// `wait_for_pane_contains` returns as soon as the pane matches and the budget
/// is only ever spent on the way to a failure.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

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
    // OMP semantics: the first ctrl+c clears the editor, a second one within
    // 500ms quits.
    session.tmux.send_key("C-c");
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
    let pane = session.wait_and_capture("startup", "pi interactive stack", STARTUP_TIMEOUT);
    assert!(
        pane.contains("pi ·"),
        "header missing from ftui frame; got:\n{pane}"
    );

    // UI-side slash routing.
    let pane = session.send_text_and_wait("help", "/help", "pi commands", COMMAND_TIMEOUT);
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

/// bd-2crrf: a default (FTUI) launch initializes ONE extension runtime and
/// one agent session. main used to build a classic `AgentSession`, boot the
/// extension runtime on it and drop it, before the FTUI driver built its own
/// SDK session, so every extension loaded, and ran its startup hooks, twice.
///
/// The extension records each load and each `session_start` by running a
/// shell command (its `node:fs` is a virtual overlay, so a file write from JS
/// would never reach the host). Exactly one of each must appear.
#[test]
fn e2e_ftui_launch_initializes_extensions_once() {
    let Some((_lock, mut session)) =
        new_locked_session("e2e_ftui_launch_initializes_extensions_once")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };
    let log = session.harness.temp_path("init-count.log");
    let ext = session.harness.temp_path("count-init.mjs");
    let record = |what: &str| {
        format!(
            "pi.exec(\"sh\", [\"-c\", \"echo {what} >> '{}'\"])",
            log.display()
        )
    };
    std::fs::write(
        &ext,
        format!(
            "export default function (pi) {{\n  void {load};\n  pi.on(\"session_start\", async () => {{ await {start}; }});\n}}\n",
            load = record("load"),
            start = record("session_start"),
        ),
    )
    .expect("write counting extension"); // ubs:ignore test setup expect
    let ext_arg = ext.display().to_string();
    let mut args = ftui_args();
    args.extend([
        "--extension",
        ext_arg.as_str(),
        "--extension-policy",
        "permissive",
        "--trust",
    ]);
    session.launch(&args);
    session.wait_and_capture("startup", "pi interactive stack", STARTUP_TIMEOUT);

    // Both records land shortly after startup; then give a duplicate runtime
    // time to show itself before counting.
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let start = std::time::Instant::now();
    while !(read().contains("load") && read().contains("session_start"))
        && start.elapsed() < COMMAND_TIMEOUT
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    quit_and_assert_clean(&session);
    session.write_artifacts();

    let records = read();
    let count = |what: &str| records.lines().filter(|line| line.trim() == what).count();
    assert_eq!(count("load"), 1, "extension loaded {records:?}");
    assert_eq!(count("session_start"), 1, "session_start ran {records:?}");
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
#[allow(clippy::too_many_lines)]
fn run_signal_teardown(name: &str, signal: &str, blind_stty_sane: bool, mid_activity: bool) {
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
    // Outer-script xtrace to a file: survives instant session death and
    // names the exact line that killed the wrapper before pi drew anything
    // (same diagnostic the VCR mid-stream lane uses).
    let trace_log = session.harness.temp_path("wrapper-trace.log");

    // Custom wrapper: pi runs in the FOREGROUND (it needs the tty for raw
    // mode) inside an inner `sh -c 'echo $$ > pid; exec pi ...'` — the exec
    // makes the recorded pid become pi's. After the kill the outer script
    // continues to the marker and hands the pane to an interactive shell.
    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    let _ = writeln!(script, "exec 2>{}\nset -x", trace_log.display());
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport OPENAI_API_KEY=pi-e2e-sigkill-dummy\n");
    // Environment forensics: rch execution contexts inherit variables that
    // can leak through tmux into pi and break startup; dump the pane-side
    // environment for diffing against a known-good interactive run.
    let _ = writeln!(
        script,
        "env | sort > {}",
        session.harness.temp_path("wrapper-env.txt").display()
    );
    let _ = writeln!(
        script,
        "/bin/sh -c 'echo $$ > {pid}; exec {bin} --ftui --no-session \
         --provider openai --model gpt-4o-mini --no-skills \
         --no-prompt-templates --no-extensions --no-themes'",
        pid = pid_file.display(),
        bin = binary.display()
    );
    // `-m` gives the recovery shell JOB CONTROL: after SIGKILL the tty's
    // foreground process group is stale, and a job-control-less shell never
    // reclaims it — every later keystroke vanishes. A real user's shell has
    // job control and recovers; model that.
    // `2>/dev/tty` gives the recovery shell its tty stderr back: the
    // wrapper's xtrace redirect would otherwise make `sh -i` decide it is
    // non-interactive (POSIX sh checks stdin AND stderr), suppressing the
    // prompt and confounding the echo probe.
    script.push_str("echo PI-WAIT-DONE\nexec /bin/sh -i -m 2>/dev/tty\n");

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
    // Loud assert with environment forensics: wait_for_pane_contains
    // returns silently on timeout, and the pane-side env dump diffs a
    // poisoned rch execution context against a known-good interactive run.
    let startup_pane = session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    let wrapper_env = std::fs::read_to_string(session.harness.temp_path("wrapper-env.txt"))
        .unwrap_or_else(|_| String::from("<no dump>"));
    let wrapper_trace =
        std::fs::read_to_string(&trace_log).unwrap_or_else(|_| String::from("<no trace>"));
    let script_present = script_path.exists();
    assert!(
        startup_pane.contains("pi interactive stack"),
        "startup banner never appeared; session_alive={}; script_present={script_present}; wrapper_env:\n{wrapper_env}\nwrapper_trace:\n{wrapper_trace}\nscript:\n{script}\npane:\n{startup_pane}",
        session.tmux.session_exists()
    );

    if mid_activity {
        // Land the signal while the UI is actively rendering: a long bash
        // command keeps the driver busy, the tool status live, and the
        // spinner tick chain re-arming when the signal arrives. Loud assert:
        // wait_for_pane_contains returns silently on timeout.
        session.tmux.send_literal("!sleep 5");
        session.tmux.send_key("Enter");
        let activity_pane = session
            .tmux
            .wait_for_pane_contains("running bash", COMMAND_TIMEOUT);
        assert!(
            activity_pane.contains("running bash"),
            "'running bash' status never appeared (UI did not take the !command); \
             pane:\n{activity_pane}"
        );
    }

    // An unreadable/unparseable pid file is an immediate test failure; the
    // pane is the diagnostic.
    let pid_text = std::fs::read_to_string(&pid_file).unwrap_or_else(|err| {
        let pane = session.tmux.capture_pane();
        let trace = std::fs::read_to_string(&trace_log).unwrap_or_default();
        panic!("read pi pid failed: {err}\npane:\n{pane}\nwrapper trace:\n{trace}");
    });
    let pid: i32 = pid_text.trim().parse().expect("parse pi pid"); // ubs:ignore test assertion expect
    // Literal /bin/kill path is deliberate: portable signal delivery without
    // libc in a unix-only test; a spawn failure is an immediate test failure.
    let mut kill_cmd = std::process::Command::new("/bin/kill"); // ubs:ignore unix-only test helper path
    kill_cmd.args([signal, &pid.to_string()]);
    let status = kill_cmd.status().expect("run kill"); // ubs:ignore test assertion expect
    assert!(status.success(), "kill {signal} {pid} failed");

    // The wrapper shell takes over the pane once pi dies.
    let done_pane = session
        .tmux
        .wait_for_pane_contains("PI-WAIT-DONE", COMMAND_TIMEOUT);
    assert!(
        done_pane.contains("PI-WAIT-DONE"),
        "wrapper never reached PI-WAIT-DONE (pi survived the signal or the \
         wrapper died); pane:\n{done_pane}"
    );
    if blind_stty_sane {
        // SIGKILL path: the tty is expected to still be raw here; a blind
        // `stty sane` (typed without echo) must recover it.
        session.tmux.send_literal("stty sane");
        session.tmux.send_key("C-j");
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
         occurrences={occurrences}, pane:\n{pane}\nescaped pane:\n{}",
        escaped_pane(&session.tmux)
    );

    session.tmux.kill_server();
}

/// Capture the pane WITH escape sequences (capture-pane -e) so failure
/// payloads show the raw terminal state: alternate-buffer content, cursor
/// position, and any sync-bracket residue the rendered view hides.
fn escaped_pane(tmux: &common::tmux::TmuxInstance) -> String {
    let output = std::process::Command::new("tmux")
        .args([
            "-L",
            &tmux.socket_name,
            "capture-pane",
            "-t",
            &format!("{}:0.0", tmux.session_name),
            "-p",
            "-e",
            "-S",
            "-2000",
        ])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
        Err(err) => format!("<capture-pane -e failed: {err}>"),
    }
}

/// SIGTERM must restore the terminal via RAII before exiting.
#[test]
fn e2e_ftui_sigterm_restores_terminal() {
    run_signal_teardown("e2e_ftui_sigterm_restores_terminal", "-TERM", false, false);
}

/// SIGTERM while the UI is mid-activity (bash running, spinner animating)
/// must still restore — the closest lane to "SIGKILL mid-stream" until a VCR
/// streamed-turn variant lands (a KILLed process can never restore, so TERM
/// is the restorable signal worth proving under load).
#[test]
fn e2e_ftui_sigterm_mid_activity_restores_terminal() {
    run_signal_teardown(
        "e2e_ftui_sigterm_mid_activity_restores_terminal",
        "-TERM",
        false,
        true,
    );
}

/// SIGKILL cannot restore (POSIX); the shell must survive and `stty sane`
/// must recover the pane.
#[test]
fn e2e_ftui_sigkill_recoverable_with_stty_sane() {
    run_signal_teardown(
        "e2e_ftui_sigkill_recoverable_with_stty_sane",
        "-9",
        true,
        false,
    );
}

/// Acceptance #2 capture proof: with `--inline`, shell content printed
/// BEFORE pi launches stays visible above the live UI (no alt-screen
/// takeover). The fullscreen control group proves the assertion has teeth:
/// there the alt screen hides the sentinel while the UI runs.
/// Ctrl+z suspend/resume (bd-cv653.9.1 round-4): SIGTSTP hands the tty back
/// to the wrapping shell (job-control `-m` sh reports "Stopped"), `fg`
/// delivers SIGCONT, and pi must re-acquire raw mode + alternate screen and
/// repaint. Ordered waits prove each transition: the initial banner lives in
/// the alternate screen and vanishes from capture on suspend, so a second
/// sighting after `fg` can only come from the resumed repaint.
/// IGNORED (infrastructure-flaky, bd-cv653.9.1 follow-up): the suspend
/// mechanism itself is proven — unit tests cover dispatch/freeze/resume
/// semantics, and a marker-instrumented live run captured the full
/// dispatched -> entered -> stopped -> resumed cycle on ovh-a. But ctrl+z
/// key delivery into the pane is environment-dependent across rch workers
/// (some runs show the keystroke reaching pi and suspending; identical
/// builds elsewhere log ZERO Key events at update() while the process stays
/// SNl+). Re-enable once key delivery is deterministic under tmux+rch.
#[test]
#[ignore = "ctrl+z pane delivery is worker-dependent; see bd-cv653.9.1 notes"]
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
fn e2e_ftui_ctrl_z_suspend_fg_resumes() {
    use std::fmt::Write as _;

    let Some((_lock, session)) = new_locked_session("e2e_ftui_ctrl_z_suspend_fg_resumes") else {
        eprintln!("Skipping: tmux not available");
        return;
    };
    let Some(binary) = std::env::var_os("CARGO_BIN_EXE_pi") else {
        eprintln!("Skipping: CARGO_BIN_EXE_pi not set");
        return;
    };
    let binary = std::path::PathBuf::from(binary);

    let env_root = session.harness.temp_dir().join("env"); // ubs:ignore test setup expect
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let pid_file = session.harness.temp_path("pi.pid");
    let trace_log = session.harness.temp_path("wrapper-trace.log");

    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    let _ = writeln!(script, "exec 2>{}\nset -x", trace_log.display());
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport OPENAI_API_KEY=pi-e2e-suspend-dummy\n");
    let _ = writeln!(
        script,
        "/bin/sh -c 'echo $$ > {pid}; exec {bin} --ftui --no-session \
         --provider openai --model gpt-4o-mini --no-skills \
         --no-prompt-templates --no-extensions --no-themes'",
        pid = pid_file.display(),
        bin = binary.display()
    );
    script.push_str("echo PI-WAIT-DONE\nexec /bin/sh -i -m 2>/dev/tty\n");

    let script_path = session.harness.temp_path("suspend-run.sh");
    std::fs::write(&script_path, &script).expect("write suspend script"); // ubs:ignore test setup expect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("stat suspend script") // ubs:ignore test setup expect
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod suspend script"); // ubs:ignore test setup expect
    }
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);

    // 1) Live launch on the ftui runtime.
    let pane = session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    assert!(
        pane.contains("pi interactive stack"),
        "startup banner never appeared; pane:\n{pane}"
    );

    // 2) Ctrl+z: pi raises SIGTSTP against itself after restoring cooked
    // mode, so the kernel stops its process group. Detect the stop through
    // the recorded pid's process state ('T') rather than pane text: the
    // stopped foreground process cannot echo anything, and shell job notices
    // race our keystrokes into the stopped process's input buffer.
    let pid_text = std::fs::read_to_string(&pid_file).unwrap_or_else(|err| {
        let pane = session.tmux.capture_pane();
        panic!("read pi pid failed: {err}\npane:\n{pane}");
    });
    let pid: u32 = pid_text.trim().parse().expect("parse pi pid"); // ubs:ignore test assertion expect

    fn process_state(pid: u32) -> String {
        String::from_utf8_lossy(
            &std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|out| out.stdout)
                .unwrap_or_default(),
        )
        .trim()
        .to_string()
    }

    let start = std::time::Instant::now();
    loop {
        let state = process_state(pid);
        if state.starts_with('T') {
            break;
        }
        assert!(
            !state.is_empty(),
            "pi pid {pid} vanished instead of stopping"
        );
        assert!(
            start.elapsed() < COMMAND_TIMEOUT,
            "pi pid {pid} never entered stopped state (state={state})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // 3) `fg` resumes pi: SIGCONT wakes the suspend task, which re-enters
    // raw mode + alt screen and forces the full repaint. Confirm both the
    // banner repaint AND the process actually leaving the stopped state.
    session.tmux.send_literal("fg");
    session.tmux.send_key("Enter");

    let start = std::time::Instant::now();
    loop {
        let state = process_state(pid);
        if !state.is_empty() && !state.starts_with('T') {
            break;
        }
        assert!(
            start.elapsed() < COMMAND_TIMEOUT,
            "pi pid {pid} never left stopped state after fg"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let pane = session
        .tmux
        .wait_for_pane_contains("pi interactive stack", COMMAND_TIMEOUT);
    assert!(
        pane.contains("pi interactive stack"),
        "banner never repainted after fg; pi did not resume cleanly; pane:\n{pane}"
    );

    // 4) The resumed session still exits cleanly on ctrl+c (RAII teardown).
    quit_and_assert_clean(&session);
    session.tmux.kill_server();
}

fn launch_with_sentinel(session: &TuiSession, sentinel: &str, inline: bool) {
    use std::fmt::Write as _;

    let Some(binary) = std::env::var_os("CARGO_BIN_EXE_pi") else {
        panic!("CARGO_BIN_EXE_pi not set");
    };
    let binary = std::path::PathBuf::from(binary);
    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect

    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport OPENAI_API_KEY=pi-e2e-scrollback-dummy\n");
    let _ = writeln!(script, "echo {sentinel}");
    let _ = writeln!(
        script,
        "exec {} --ftui{} --no-session --provider openai --model gpt-4o-mini \
         --no-skills --no-prompt-templates --no-extensions --no-themes",
        binary.display(),
        if inline { " --inline" } else { "" }
    );

    let script_path = session.harness.temp_path("scrollback-run.sh");
    std::fs::write(&script_path, &script).expect("write scrollback script"); // ubs:ignore test setup expect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("stat scrollback script") // ubs:ignore test setup expect
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod scrollback script"); // ubs:ignore test setup expect
    }
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);
}

/// Capture the pane INCLUDING scrollback history. The inline UI may occupy
/// the whole visible pane (its body region is `Fill`), pushing pre-launch
/// content into history — which is exactly where "preserved scrollback"
/// lives. The alternate screen has no history, so in fullscreen mode this
/// still cannot see the primary screen's hidden content.
fn capture_with_history(session: &TuiSession) -> String {
    let mut cmd = std::process::Command::new("tmux"); // ubs:ignore test helper — same tmux invocation pattern as tests/common/tmux.rs
    let output = cmd
        .args([
            "-L",
            &session.tmux.socket_name,
            "capture-pane",
            "-p",
            "-t",
            &session.tmux.session_name,
            "-S",
            "-200",
        ])
        .output()
        .expect("tmux capture-pane with history"); // ubs:ignore test assertion expect
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One scrollback case in its own lock scope (the tmux e2e lock is
/// non-reentrant, so the inline and control cases must not overlap).
fn scrollback_case(name: &str, sentinel: &str, inline: bool, expect_visible: bool) -> bool {
    let Some((_lock, session)) = new_locked_session(name) else {
        eprintln!("Skipping: tmux not available");
        return false;
    };
    launch_with_sentinel(&session, sentinel, inline);
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    let pane = capture_with_history(&session);
    if expect_visible {
        assert!(
            pane.contains(sentinel),
            "inline mode lost pre-launch shell content (not even in history); pane:\n{pane}"
        );
    } else {
        assert!(
            !pane.contains(sentinel),
            "fullscreen (alt-screen) unexpectedly shows pre-launch content; pane:\n{pane}"
        );
    }
    session.tmux.kill_server();
    true
}

#[test]
fn e2e_ftui_inline_preserves_scrollback_fullscreen_hides_it() {
    const SENTINEL: &str = "SCROLLBACK-SENTINEL-4271";
    // Inline: sentinel and live UI must coexist in the visible pane.
    if !scrollback_case("e2e_ftui_scrollback_inline", SENTINEL, true, true) {
        return;
    }
    // Control group — fullscreen: the alt screen must HIDE the sentinel
    // while the UI runs, proving the inline assertion has teeth.
    scrollback_case("e2e_ftui_scrollback_fullscreen", SENTINEL, false, false);
}

/// Acceptance #3 lane (tmux-achievable part): a resize storm while the UI is
/// live must not crash, wedge, or corrupt the session — after the storm the
/// UI still routes input and quits cleanly. Torn-frame detection proper
/// belongs to the ftui-harness flicker tooling; this proves survival and
/// post-storm correctness end to end.
#[test]
fn e2e_ftui_resize_storm_survives() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_resize_storm_survives") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&ftui_args());
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);

    // Storm: rapid alternating geometries, ending back at 80x24.
    for (w, h) in [
        ("40", "12"),
        ("120", "40"),
        ("32", "10"),
        ("100", "30"),
        ("60", "18"),
        ("80", "24"),
    ] {
        let mut cmd = std::process::Command::new("tmux"); // ubs:ignore test helper — same tmux invocation pattern as tests/common/tmux.rs
        let status = cmd
            .args([
                "-L",
                &session.tmux.socket_name,
                "resize-window",
                "-t",
                &session.tmux.session_name,
                "-x",
                w,
                "-y",
                h,
            ])
            .status()
            .expect("tmux resize-window"); // ubs:ignore test assertion expect
        assert!(status.success(), "resize to {w}x{h} failed");
        std::thread::sleep(Duration::from_millis(60));
    }

    // Let the resize coalescer settle on the final geometry: transiently
    // rendering for a stale size during the storm is expected (latest-wins
    // with bounded latency); the assertions below are about steady state.
    std::thread::sleep(Duration::from_millis(500));

    // Post-storm: the UI must still route input correctly...
    session.send_text_and_wait("post_storm_help", "/help", "pi commands", COMMAND_TIMEOUT);
    // ...and the steady-state frame must be laid out for the final geometry
    // (a stale-size frame pushes the header off the top of the pane).
    std::thread::sleep(Duration::from_millis(300));
    let pane = session.tmux.capture_pane();
    assert!(
        pane.contains("pi ·"),
        "header missing after resize storm settled; got:\n{pane}"
    );

    // ...and still tear down cleanly.
    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// Acceptance #3 residual (bd-bi0qc): torn-frame detection over the raw
/// pane stream. `tmux pipe-pane` records every byte pi writes into the pane
/// while a resize storm plus input churn run; the capture is then fed to
/// the upstream FrankenTUI analyzer (ftui-harness `flicker_scan`, wired via
/// `PI_FTUI_FLICKER_SCAN_BIN`), whose detector flags unsynchronized full
/// repaints, partial clears, and unpaired frame markers. Skipped when tmux
/// or the analyzer binary is unavailable so CI and worker runs stay green;
/// owner hosts with `/dp/frankentui` get real detection:
///
/// ```sh
/// cd /dp/frankentui && cargo build -p ftui-harness --bin flicker_scan
/// export PI_FTUI_FLICKER_SCAN_BIN="$CARGO_TARGET_DIR/debug/flicker_scan"
/// ```
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_ftui_resize_storm_stream_is_flicker_free() {
    let analyzer = std::env::var("PI_FTUI_FLICKER_SCAN_BIN")
        .ok()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file());
    let Some(analyzer) = analyzer else {
        eprintln!(
            "Skipping: PI_FTUI_FLICKER_SCAN_BIN must point at a built ftui-harness \
             flicker_scan binary"
        );
        return;
    };
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_resize_storm_flicker_free")
    else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&ftui_args());
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);

    // Tap the RAW output stream (escape sequences included — capture-pane
    // only exposes rendered text). `-o` pipes everything the pane emits.
    let capture_path = session.harness.temp_path("ftui-stream.raw");
    let pane_target = format!("{}:0.0", session.tmux.session_name);
    let pipe_cmd = format!("cat >> {}", capture_path.display());
    let mut tap = std::process::Command::new("tmux"); // ubs:ignore test helper — direct tmux invocation, same pattern as the resize loop below
    let tap_status = tap
        .args([
            "-L",
            &session.tmux.socket_name,
            "pipe-pane",
            "-o",
            "-t",
            &pane_target,
            &pipe_cmd,
        ])
        .status()
        .expect("tmux pipe-pane start"); // ubs:ignore test assertion expect — failed tap open is an immediate failure, same convention as the resize storm lane
    assert!(tap_status.success(), "pipe-pane start failed");

    // Storm + churn: geometry flapping forces repeated full re-renders while
    // editor input and /help force incremental redraws interleaved mid-stream.
    for (w, h) in [
        ("40", "12"),
        ("120", "40"),
        ("32", "10"),
        ("100", "30"),
        ("60", "18"),
        ("80", "24"),
    ] {
        let mut cmd = std::process::Command::new("tmux"); // ubs:ignore test helper — same tmux invocation pattern as tests/common/tmux.rs
        let status = cmd
            .args([
                "-L",
                &session.tmux.socket_name,
                "resize-window",
                "-t",
                &session.tmux.session_name,
                "-x",
                w,
                "-y",
                h,
            ])
            .status()
            .expect("tmux resize-window"); // ubs:ignore test assertion expect
        assert!(status.success(), "resize to {w}x{h} failed");
        std::thread::sleep(Duration::from_millis(60));
    }
    session.send_text_and_wait("flicker_help", "/help", "pi commands", COMMAND_TIMEOUT);
    for (w, h) in [("90", "28"), ("50", "16"), ("80", "24")] {
        let mut cmd = std::process::Command::new("tmux"); // ubs:ignore test helper — same tmux invocation pattern as tests/common/tmux.rs
        let status = cmd
            .args([
                "-L",
                &session.tmux.socket_name,
                "resize-window",
                "-t",
                &session.tmux.session_name,
                "-x",
                w,
                "-y",
                h,
            ])
            .status()
            .expect("tmux resize-window"); // ubs:ignore test assertion expect
        assert!(status.success(), "resize to {w}x{h} failed");
        std::thread::sleep(Duration::from_millis(60));
    }

    // Close the tap (pipe-pane with no command detaches) and let the
    // recorder shell flush its buffered tail.
    let mut untap = std::process::Command::new("tmux"); // ubs:ignore test helper — direct tmux invocation
    let untap_status = untap
        .args([
            "-L",
            &session.tmux.socket_name,
            "pipe-pane",
            "-t",
            &pane_target,
        ])
        .status()
        .expect("tmux pipe-pane close"); // ubs:ignore test assertion expect
    assert!(untap_status.success(), "pipe-pane close failed");
    std::thread::sleep(Duration::from_millis(400));

    // Analyze with the upstream detector; keep both artifacts.
    let output = std::process::Command::new(&analyzer)
        .arg(&capture_path)
        .output()
        .expect("run flicker_scan analyzer"); // ubs:ignore test assertion expect
    session
        .harness
        .record_artifact("ftui-flicker-stream.raw", &capture_path);
    let verdict_path = session.harness.temp_path("ftui-flicker-verdict.txt");
    std::fs::write(&verdict_path, &output.stdout).expect("write verdict artifact");
    session
        .harness
        .record_artifact("ftui-flicker-verdict.txt", &verdict_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let verdict_line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("FLICKER_VERDICT "))
        .unwrap_or_else(|| {
            panic!(
                "analyzer produced no verdict; stdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    let payload = &verdict_line["FLICKER_VERDICT ".len()..];
    let verdict: serde_json::Value =
        serde_json::from_str(payload).expect("parse FLICKER_VERDICT JSON");
    let bytes_total = verdict["bytes_total"].as_u64().unwrap_or(0);
    assert!(bytes_total > 0, "analyzer consumed an empty stream");
    // Inside a tmux pane the capability probe correctly reports DEC-2026
    // unsupported, so upstream's presenter uses its designed cursor-hide
    // fallback (presenter.rs: bracket_supported gate) and a sync-bracket
    // detector necessarily reports total_frames=0 with a sync gap. The
    // in-mux contract is therefore: NO partial clears and NO unpaired
    // frames — torn output would show up as partial_clears > 0 or a
    // complete_frames mismatch.
    let partial_clears = verdict["partial_clears"].as_u64().unwrap_or(0);
    assert_eq!(
        partial_clears, 0,
        "partial clears during resize storm: {payload}"
    );
    let total_frames = verdict["total_frames"].as_u64().unwrap_or(0);
    let complete_frames = verdict["complete_frames"].as_u64().unwrap_or(0);
    if total_frames > 0 {
        assert_eq!(
            total_frames, complete_frames,
            "unpaired frame markers during resize storm: {payload}"
        );
    }

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// Acceptance #2 lane: the inline (scrollback-preserving) runtime path boots,
/// renders, and quits cleanly.
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

// ── VCR streamed-turn lane (bd-pb4fw) ───────────────────────────────────────

const FTUI_VCR_TEST_NAME: &str = "e2e_ftui_vcr_streamed_turn";
const FTUI_VCR_MODEL: &str = "claude-sonnet-4-20250514";
const FTUI_VCR_MAX_TOKENS: u32 = 64_000;
const FTUI_VCR_PROMPT: &str = "ftui vcr prompt: say the marker";
const FTUI_VCR_RESPONSE: &str = "ftui-vcr-response-marker alpha beta gamma";
const FTUI_VCR_SYSTEM_PROMPT_ARG: &str = "pi e2e ftui vcr harness";

fn ftui_vcr_args() -> Vec<String> {
    ftui_vcr_args_with_session(false, false)
}

/// Write the launch wrapper a VCR scenario starts `pi` through, and return its
/// path.
///
/// Stderr goes to a file because tracing output otherwise interleaves with the
/// pane, and on failure that log is the diagnostic. Extracted so a scenario can
/// launch more than once — relaunching with different args against the same
/// env root is how the `--continue` case is expressed.
#[allow(clippy::too_many_arguments)]
fn write_ftui_vcr_launcher(
    script_path: &std::path::Path,
    env_root: &std::path::Path,
    cassette_dir: &std::path::Path,
    stderr_log: &std::path::Path,
    test_name: &str,
    args: &[String],
) {
    use std::fmt::Write as _;
    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport ANTHROPIC_API_KEY=pi-e2e-vcr-dummy\n");
    let _ = writeln!(script, "export {}=playback", pi::vcr::VCR_ENV_MODE);
    let _ = writeln!(
        script,
        "export {}={}",
        pi::vcr::VCR_ENV_DIR,
        cassette_dir.display()
    );
    let _ = writeln!(script, "export PI_VCR_TEST_NAME={test_name}");
    script.push_str("export VCR_DEBUG_BODY=1\n");
    // Stable path: the harness temp dir is deleted on drop, and the debug
    // bodies are exactly what we need after a failure.
    script.push_str("export VCR_DEBUG_BODY_FILE=/private/tmp/pi-tests/ftui-vcr-bodies.txt\n");
    let binary = std::env::var_os("CARGO_BIN_EXE_pi").expect("CARGO_BIN_EXE_pi"); // ubs:ignore test setup expect
    let _ = write!(
        script,
        "exec {}",
        std::path::PathBuf::from(binary).display()
    );
    for arg in args {
        let _ = write!(script, " '{arg}'");
    }
    let _ = writeln!(script, " 2>{}", stderr_log.display());
    std::fs::write(script_path, &script).expect("write vcr script"); // ubs:ignore test setup expect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(script_path)
            .expect("stat vcr script") // ubs:ignore test setup expect
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(script_path, perms).expect("chmod vcr script"); // ubs:ignore test setup expect
    }
}

/// The VCR arg list, optionally with session persistence on and `--continue`
/// requested.
///
/// `persist` drops `--no-session`, which the other VCR scenarios want precisely
/// because they must not leave session files behind; the continue scenario
/// needs the opposite. Sessions land under the harness's `PI_SESSIONS_DIR`, so
/// nothing escapes the temp root either way.
fn ftui_vcr_args_with_session(persist: bool, continue_recent: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["--ftui".to_string()];
    if !persist {
        args.push("--no-session".to_string());
    }
    if continue_recent {
        args.push("--continue".to_string());
    }
    for arg in [
        "--provider",
        "anthropic",
        "--model",
        FTUI_VCR_MODEL,
        "--no-tools",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        FTUI_VCR_SYSTEM_PROMPT_ARG,
    ] {
        args.push(arg.to_string());
    }
    args
}

/// Effective system prompt for the ftui VCR args, computed with the same
/// builder the session uses (mirrors build_vcr_system_prompt_for_args in
/// tests/e2e_tui.rs).
fn ftui_vcr_system_prompt(workdir: &std::path::Path, env_root: &std::path::Path) -> String {
    ftui_vcr_system_prompt_for(&ftui_vcr_args(), workdir, env_root)
}

/// The same builder for an arbitrary arg list, so a scenario that changes the
/// CLI (session persistence, `--continue`) still computes the system prompt the
/// session will actually send and its cassette still matches on the body.
fn ftui_vcr_system_prompt_for(
    args: &[String],
    workdir: &std::path::Path,
    env_root: &std::path::Path,
) -> String {
    use clap::Parser as _;
    let mut parsed: Vec<&str> = vec!["pi"];
    parsed.extend(args.iter().map(String::as_str));
    let cli = pi::cli::Cli::try_parse_from(parsed).expect("parse ftui vcr args"); // ubs:ignore test setup expect
    let enabled_tools = cli.enabled_tools();
    let global_dir = env_root.join("agent");
    let package_dir = env_root.join("packages");
    pi::app::build_system_prompt(
        &cli,
        workdir,
        &enabled_tools,
        None,
        &global_dir,
        &package_dir,
        true,
        true,
        None,
        &pi::config::Config::default(),
    )
    .expect("build ftui vcr system prompt") // ubs:ignore test setup expect
}

fn write_ftui_vcr_cassette(
    dir: &std::path::Path,
    system_prompt: &str,
    test_name: &str,
    response_text: &str,
) -> std::path::PathBuf {
    use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse};
    use serde_json::json;

    let cassette_path = dir.join(format!("{test_name}.json"));
    // The SDK path enables prompt caching: text blocks carry
    // cache_control and `system` is an array of blocks, not a string.
    let request = json!({
        "model": FTUI_VCR_MODEL,
        "messages": [
            { "role": "user", "content": [ {
                "type": "text",
                "text": FTUI_VCR_PROMPT,
                "cache_control": { "type": "ephemeral" }
            } ] }
        ],
        "system": [ {
            "type": "text",
            "text": system_prompt,
            "cache_control": { "type": "ephemeral" }
        } ],
        "max_tokens": FTUI_VCR_MAX_TOKENS,
        "stream": true,
    });
    let sse_chunk = |event: &str, data: serde_json::Value| -> String {
        let payload = serde_json::to_string(&data).expect("serialize sse payload"); // ubs:ignore test setup expect
        format!("event: {event}\ndata: {payload}\n\n")
    };
    // The response streams word by word so the lane exercises progressive
    // markdown rendering, not just a single-delta append.
    let mut body_chunks = vec![
        sse_chunk(
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 12}}}),
        ),
        sse_chunk(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ),
    ];
    for word in response_text.split_inclusive(' ') {
        body_chunks.push(sse_chunk(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word}}),
        ));
    }
    body_chunks.push(sse_chunk(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    ));
    body_chunks.push(sse_chunk(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
    ));
    body_chunks.push(sse_chunk("message_stop", json!({"type": "message_stop"})));

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
        recorded_at: "1970-01-01T00:00:00Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: "https://api.anthropic.com/v1/messages".to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                ],
                body: Some(request),
                body_text: None,
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body_chunks,
                body_chunks_base64: None,
            },
        }],
    };
    std::fs::create_dir_all(dir).expect("create cassette dir"); // ubs:ignore test setup expect
    let json = serde_json::to_string_pretty(&cassette).expect("serialize cassette"); // ubs:ignore test setup expect
    std::fs::write(&cassette_path, json).expect("write cassette"); // ubs:ignore test setup expect
    cassette_path
}

/// bd-pb4fw: a REAL streamed provider turn through the preview stack — the
/// VCR cassette plays an SSE stream back word by word, and the pane must show
/// the full assistant reply (progressive streaming render + finalization).
#[test]
fn e2e_ftui_vcr_streamed_turn() {
    let Some((_lock, session)) = new_locked_session(FTUI_VCR_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let system_prompt = ftui_vcr_system_prompt(session.harness.temp_dir(), &env_root);
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    let cassette_path = write_ftui_vcr_cassette(
        &cassette_dir,
        &system_prompt,
        FTUI_VCR_TEST_NAME,
        FTUI_VCR_RESPONSE,
    );
    session
        .harness
        .record_artifact("ftui-vcr-cassette.json", &cassette_path);

    // Launch via a wrapper that redirects stderr to a file: tracing output
    // otherwise interleaves with the pane, and on failure the log is the
    // diagnostic.
    let stderr_log = session.harness.temp_path("pi-stderr.log");
    let script_path = session.harness.temp_path("vcr-run.sh");
    write_ftui_vcr_launcher(
        &script_path,
        &env_root,
        &cassette_dir,
        &stderr_log,
        FTUI_VCR_TEST_NAME,
        &ftui_vcr_args(),
    );
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);

    session.tmux.send_literal(FTUI_VCR_PROMPT);
    session.tmux.send_key("Enter");
    let pane = session
        .tmux
        .wait_for_pane_contains("ftui-vcr-response-marker", COMMAND_TIMEOUT);
    let stderr_tail = std::fs::read_to_string(&stderr_log).unwrap_or_default();
    assert!(
        pane.contains("alpha beta gamma"),
        "full streamed response missing; pane:\n{pane}\nstderr tail:\n{}",
        &stderr_tail[stderr_tail.len().saturating_sub(2000)..]
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

// ── `--continue` reopens the latest session (bd-ydz1t.3) ───────────────────

const FTUI_CONTINUE_TEST_NAME: &str = "e2e_ftui_continue";

/// `pi -c` was silently ignored on this stack: the classic path resolves it
/// inside `Session::from_cli`, which the FTUI branch does not use, and
/// `SessionOptions` carried no "reopen the latest" concept — so the flag fell
/// on the floor and the user got a fresh session (bd-ydz1t.3).
///
/// The unit test pins which PATH `--continue` resolves to. This pins the thing
/// the user actually cares about: after quitting and relaunching with `-c`, the
/// previous turn is on screen. It relaunches with NO prompt, so the marker can
/// only come from restored history — a second provider turn would need a
/// cassette hit that never happens.
#[test]
fn e2e_ftui_continue_reopens_the_previous_session() {
    let Some((_lock, session)) = new_locked_session(FTUI_CONTINUE_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let first_args = ftui_vcr_args_with_session(true, false);
    let system_prompt =
        ftui_vcr_system_prompt_for(&first_args, session.harness.temp_dir(), &env_root);
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    let cassette_path = write_ftui_vcr_cassette(
        &cassette_dir,
        &system_prompt,
        FTUI_CONTINUE_TEST_NAME,
        FTUI_VCR_RESPONSE,
    );
    session
        .harness
        .record_artifact("ftui-continue-cassette.json", &cassette_path);

    // First launch: a real turn, persisted under the harness's sessions dir.
    let stderr_log = session.harness.temp_path("pi-stderr-first.log");
    let script_path = session.harness.temp_path("continue-first.sh");
    write_ftui_vcr_launcher(
        &script_path,
        &env_root,
        &cassette_dir,
        &stderr_log,
        FTUI_CONTINUE_TEST_NAME,
        &first_args,
    );
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    session.tmux.send_literal(FTUI_VCR_PROMPT);
    session.tmux.send_key("Enter");
    session
        .tmux
        .wait_for_pane_contains("ftui-vcr-response-marker", COMMAND_TIMEOUT);
    quit_and_assert_clean(&session);

    // The session file must exist before the relaunch can mean anything: with
    // nothing saved, `--continue` correctly starts fresh and the assertion
    // below would be testing the fallback rather than the feature.
    let sessions_root = env_root.join("sessions");
    let saved = walk_session_files(&sessions_root);
    assert!(
        !saved.is_empty(),
        "first launch saved no session under {}; --continue has nothing to reopen",
        sessions_root.display()
    );

    // Second launch: `-c`, and NO prompt. Anything on screen came from history.
    let resume_stderr = session.harness.temp_path("pi-stderr-continue.log");
    let resume_script = session.harness.temp_path("continue-second.sh");
    write_ftui_vcr_launcher(
        &resume_script,
        &env_root,
        &cassette_dir,
        &resume_stderr,
        FTUI_CONTINUE_TEST_NAME,
        &ftui_vcr_args_with_session(true, true),
    );
    session
        .tmux
        .start_session(session.harness.temp_dir(), &resume_script);
    let pane = session
        .tmux
        .wait_for_pane_contains("ftui-vcr-response-marker", COMMAND_TIMEOUT);
    let stderr_tail = std::fs::read_to_string(&resume_stderr).unwrap_or_default();
    assert!(
        pane.contains(FTUI_VCR_PROMPT),
        "the restored transcript must show the earlier USER turn too; pane:\n{pane}\nstderr tail:\n{}",
        &stderr_tail[stderr_tail.len().saturating_sub(2000)..]
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

const FTUI_RESTART_TEST_NAME: &str = "e2e_ftui_restart";

/// OMP `/restart`: after a real turn, `/restart` tears the UI down, saves
/// the session, and re-execs pi with the launch flags plus `--session
/// <file>`. The relaunch shows the earlier turn from history, followed by the
/// startup banner, while the first launch showed the banner BEFORE the turn:
/// that ordering is what proves a new process drew the screen rather than
/// the old frame surviving.
#[test]
fn e2e_ftui_restart_relaunches_into_the_same_session() {
    let Some((_lock, session)) = new_locked_session(FTUI_RESTART_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let args = ftui_vcr_args_with_session(true, false);
    let system_prompt = ftui_vcr_system_prompt_for(&args, session.harness.temp_dir(), &env_root);
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    write_ftui_vcr_cassette(
        &cassette_dir,
        &system_prompt,
        FTUI_RESTART_TEST_NAME,
        FTUI_VCR_RESPONSE,
    );
    let stderr_log = session.harness.temp_path("pi-stderr-restart.log");
    let script_path = session.harness.temp_path("restart.sh");
    write_ftui_vcr_launcher(
        &script_path,
        &env_root,
        &cassette_dir,
        &stderr_log,
        FTUI_RESTART_TEST_NAME,
        &args,
    );
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    session.tmux.send_literal(FTUI_VCR_PROMPT);
    session.tmux.send_key("Enter");
    let before = session
        .tmux
        .wait_for_pane_contains("ftui-vcr-response-marker", COMMAND_TIMEOUT);
    let banner_first = |pane: &str| {
        pane.find("pi interactive stack")
            .zip(pane.find("ftui-vcr-response-marker"))
            .map(|(banner, marker)| banner < marker)
    };
    assert_eq!(banner_first(&before), Some(true), "first launch:\n{before}");
    // The reply text lands before the turn ends; commands typed mid-turn are
    // refused, so wait for the turn's usage footer.
    session
        .tmux
        .wait_for_pane_contains("tokens ", COMMAND_TIMEOUT);

    session.tmux.send_literal("/restart");
    session.tmux.send_key("Enter");
    let start = std::time::Instant::now();
    let after = loop {
        let pane = session.tmux.capture_pane();
        if banner_first(&pane) == Some(false) {
            break pane;
        }
        assert!(
            start.elapsed() < STARTUP_TIMEOUT,
            "no relaunch into the saved session; pane:\n{pane}\nstderr tail:\n{}",
            std::fs::read_to_string(&stderr_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        after.contains(FTUI_VCR_PROMPT),
        "the relaunch shows the earlier user turn from history:\n{after}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

const FTUI_DELETE_TEST_NAME: &str = "e2e_ftui_delete";

/// OMP `/delete`: after a saved turn, `/delete yes` moves to a new session
/// and removes the old session's file from disk.
#[test]
fn e2e_ftui_delete_removes_the_saved_session_file() {
    let Some((_lock, session)) = new_locked_session(FTUI_DELETE_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let args = ftui_vcr_args_with_session(true, false);
    let system_prompt = ftui_vcr_system_prompt_for(&args, session.harness.temp_dir(), &env_root);
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    write_ftui_vcr_cassette(
        &cassette_dir,
        &system_prompt,
        FTUI_DELETE_TEST_NAME,
        FTUI_VCR_RESPONSE,
    );
    let stderr_log = session.harness.temp_path("pi-stderr-delete.log");
    let script_path = session.harness.temp_path("delete.sh");
    write_ftui_vcr_launcher(
        &script_path,
        &env_root,
        &cassette_dir,
        &stderr_log,
        FTUI_DELETE_TEST_NAME,
        &args,
    );
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    session.tmux.send_literal(FTUI_VCR_PROMPT);
    session.tmux.send_key("Enter");
    session
        .tmux
        .wait_for_pane_contains("ftui-vcr-response-marker", COMMAND_TIMEOUT);
    session
        .tmux
        .wait_for_pane_contains("tokens ", COMMAND_TIMEOUT);

    let sessions_root = env_root.join("sessions");
    let saved = walk_session_files(&sessions_root);
    assert_eq!(
        saved.len(),
        1,
        "one saved session before /delete: {saved:?}"
    );

    session.tmux.send_literal("/delete yes");
    session.tmux.send_key("Enter");
    let pane = session
        .tmux
        .wait_for_pane_contains("this is a new session", COMMAND_TIMEOUT);
    assert!(
        !saved[0].exists(),
        "the deleted session's file is gone; pane:\n{pane}"
    );
    assert!(
        !pane.contains("ftui-vcr-response-marker"),
        "the new session starts empty:\n{pane}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// Every `*.jsonl` under the harness sessions root, at any depth: sessions are
/// filed under an encoded-cwd subdirectory.
fn walk_session_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                found.push(path);
            }
        }
    }
    found
}

// ── Mid-STREAM SIGTERM (bd-pb4fw follow-through via VCR chunk pacing) ───────

const FTUI_VCR_KILL_TEST_NAME: &str = "e2e_ftui_vcr_midstream_kill";

/// The bead's literal acceptance case: SIGTERM lands while a provider reply
/// is STREAMING (VCR playback paced at 150ms/chunk gives a multi-second
/// window), and the RAII restore must still leave the wrapper shell's
/// terminal echoing.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_ftui_sigterm_mid_stream_restores_terminal() {
    use std::fmt::Write as _;

    let Some((_lock, session)) = new_locked_session(FTUI_VCR_KILL_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };
    let Some(binary) = std::env::var_os("CARGO_BIN_EXE_pi") else {
        eprintln!("Skipping: CARGO_BIN_EXE_pi not set");
        return;
    };
    let binary = std::path::PathBuf::from(binary);

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let system_prompt = ftui_vcr_system_prompt(session.harness.temp_dir(), &env_root);
    // A long response (80 words) at 150ms/chunk ≈ 12s of streaming: plenty
    // of window to observe the first words and land the signal mid-stream.
    let mut long_response = String::from("midstream-first-marker ");
    for i in 0..78 {
        let _ = write!(long_response, "word{i} ");
    }
    long_response.push_str("midstream-last-marker");
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    write_ftui_vcr_cassette(
        &cassette_dir,
        &system_prompt,
        FTUI_VCR_KILL_TEST_NAME,
        &long_response,
    );

    let pid_file = session.harness.temp_path("pi.pid");
    let stderr_log = session.harness.temp_path("pi-stderr.log");
    let trace_log = session.harness.temp_path("wrapper-trace.log");
    let mut script = String::from("#!/usr/bin/env sh\nset -u\n");
    // Outer-script xtrace to a file: the definitive diagnostic when the
    // wrapper dies before pi draws anything.
    let _ = write!(script, "exec 2>{}\nset -x\n", trace_log.display());
    for (key, sub) in [
        ("PI_CODING_AGENT_DIR", "agent"),
        ("PI_CONFIG_PATH", "config.toml"),
        ("PI_SESSIONS_DIR", "sessions"),
        ("PI_PACKAGE_DIR", "packages"),
    ] {
        let _ = writeln!(script, "export {key}={}", env_root.join(sub).display());
    }
    script.push_str("export PI_TEST_MODE=1\nexport ANTHROPIC_API_KEY=pi-e2e-vcr-dummy\n");
    let _ = writeln!(script, "export {}=playback", pi::vcr::VCR_ENV_MODE);
    let _ = writeln!(
        script,
        "export {}={}",
        pi::vcr::VCR_ENV_DIR,
        cassette_dir.display()
    );
    let _ = writeln!(script, "export PI_VCR_TEST_NAME={FTUI_VCR_KILL_TEST_NAME}");
    let _ = writeln!(script, "export {}=150", pi::vcr::VCR_ENV_CHUNK_DELAY_MS);
    let _ = write!(
        script,
        "/bin/sh -c 'echo $$ > {pid}; exec {bin}",
        pid = pid_file.display(),
        bin = binary.display()
    );
    for arg in ftui_vcr_args() {
        let _ = write!(script, " \"{arg}\"");
    }
    let _ = writeln!(script, " 2>{}'", stderr_log.display());
    // Give the post-kill shell its tty stderr back: the wrapper's xtrace
    // redirect would otherwise make `sh -i` decide it is non-interactive
    // (POSIX sh checks stdin AND stderr), which suppresses the prompt and
    // confounds the echo probe.
    script.push_str("echo PI-WAIT-DONE\nexec /bin/sh -i 2>/dev/tty\n");

    let script_path = session.harness.temp_path("midstream-run.sh");
    std::fs::write(&script_path, &script).expect("write midstream script"); // ubs:ignore test setup expect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("stat midstream script") // ubs:ignore test setup expect
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod midstream script"); // ubs:ignore test setup expect
    }
    session
        .tmux
        .start_session(session.harness.temp_dir(), &script_path);

    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);
    session.tmux.send_literal(FTUI_VCR_PROMPT);
    session.tmux.send_key("Enter");

    // First words visible = the reply is actively streaming; the tail marker
    // must NOT be there yet, or the kill wouldn't be mid-stream.
    let pane = session
        .tmux
        .wait_for_pane_contains("midstream-first-marker", COMMAND_TIMEOUT);
    assert!(
        !pane.contains("midstream-last-marker"),
        "stream already finished — pacing window too small; pane:\n{pane}"
    );

    // An unreadable/unparseable pid file is an immediate test failure; on
    // that failure the pane + stderr are the diagnostics.
    let pid_text = std::fs::read_to_string(&pid_file).unwrap_or_else(|err| {
        let pane = session.tmux.capture_pane();
        let stderr_tail = std::fs::read_to_string(&stderr_log).unwrap_or_default();
        let trace = std::fs::read_to_string(&trace_log).unwrap_or_default();
        panic!(
            "read pi pid failed: {err}\npane:\n{pane}\nstderr:\n{stderr_tail}\nwrapper trace:\n{trace}"
        );
    });
    let pid: i32 = pid_text.trim().parse().expect("parse pi pid"); // ubs:ignore test assertion expect
    let mut kill_cmd = std::process::Command::new("/bin/kill"); // ubs:ignore unix-only test helper path
    kill_cmd.args(["-TERM", &pid.to_string()]);
    let status = kill_cmd.status().expect("run kill"); // ubs:ignore test assertion expect
    assert!(status.success(), "kill -TERM {pid} failed");

    session
        .tmux
        .wait_for_pane_contains("PI-WAIT-DONE", COMMAND_TIMEOUT);
    session.tmux.send_literal("echo POST-KILL-OK");
    session.tmux.send_key("Enter");
    let pane = session
        .tmux
        .wait_for_pane_contains("POST-KILL-OK", COMMAND_TIMEOUT);
    let occurrences = pane.matches("POST-KILL-OK").count();
    assert!(
        occurrences >= 2,
        "typed probe did not echo after mid-stream SIGTERM; occurrences={occurrences}, pane:\n{pane}"
    );
    session.tmux.kill_server();
}

// ── Wheel passthrough inside tmux (bd-bi0qc) ────────────────────────────────

/// bd-bi0qc: mouse wheel works when pi runs INSIDE tmux — SGR wheel-up
/// sequences injected into the pane (what tmux passthrough delivers) must
/// scroll the conversation (the footer pins the scroll indicator).
#[test]
fn e2e_ftui_wheel_scroll_inside_tmux() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_wheel_scroll_inside_tmux") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    session.launch(&ftui_args());
    session
        .tmux
        .wait_for_pane_contains("pi interactive stack", STARTUP_TIMEOUT);

    // Fill the transcript well past one screen. A single tall tool output
    // no longer guarantees overflow: card collapsing (bd-cv653.9.2) can
    // render it as a short preview, leaving the whole transcript inside
    // one viewport so max_scroll_from_tail() clamps to zero and wheel-up
    // becomes a no-op. Send distinct bash passthroughs instead — each
    // renders its own card head and unique output line, so the transcript
    // overflows regardless of collapsing behavior.
    session.send_text_and_wait("fill", "!!seq 1 120", "120", COMMAND_TIMEOUT);
    for i in 1..=12 {
        // Unique per-iteration labels are the point of the fixture: each
        // marker both drives and verifies its own transcript entry.
        let marker = format!("wheel-filler-{i:02}"); // ubs:ignore fixture label — distinct per-iteration marker is required
        session.send_text_and_wait(
            "fill",
            &format!("!!echo {marker}"), // ubs:ignore fixture label — command text must embed the unique marker
            &marker,
            COMMAND_TIMEOUT,
        );
    }

    // Inject SGR mouse wheel-up (button 64) at col 10, row 5 — the byte
    // sequence a wheel event delivers under SGR 1006 mouse reporting:
    // ESC [ < 6 4 ; 1 0 ; 5 M
    for _ in 0..5 {
        let mut cmd = std::process::Command::new("tmux"); // ubs:ignore test helper — same tmux invocation pattern as tests/common/tmux.rs
        let status = cmd
            .args([
                "-L",
                &session.tmux.socket_name,
                "send-keys",
                "-t",
                &session.tmux.session_name,
                "-H",
                "1b",
                "5b",
                "3c",
                "36",
                "34",
                "3b",
                "31",
                "30",
                "3b",
                "35",
                "4d",
            ])
            .status()
            .expect("tmux send-keys -H"); // ubs:ignore test assertion expect
        assert!(status.success(), "wheel injection failed");
        std::thread::sleep(Duration::from_millis(60));
    }

    let pane = session
        .tmux
        .wait_for_pane_contains("lines up] End to follow", COMMAND_TIMEOUT);
    assert!(
        pane.contains("lines up] End to follow"),
        "wheel-up did not scroll the conversation; pane:\n{pane}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// bd-ydz1t.1: `/share` reaches `gh` on the ftui stack and reports the URL.
///
/// The classic stack has had this scenario since the command existed; this is
/// the same scenario against the stack most people run, driving the same
/// `run_share` implementation through a mock `gh`. Without it, the ftui port
/// was covered only by unit tests of the ROUTING — that the driver then
/// produces a gist was true by construction and unasserted.
#[test]
fn e2e_ftui_share_creates_secret_gist() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_share_creates_gist") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let mock_bin = session.harness.temp_path("mock_bin");
    std::fs::create_dir_all(&mock_bin).expect("create mock_bin");
    let gist_url = "https://gist.github.com/testuser/e2e_ftui_share_id";
    let gh_path = common::mocks::write_mock_gh_script(&mock_bin, gist_url);

    let pi_dir = session.harness.temp_path(".pi");
    std::fs::create_dir_all(&pi_dir).expect("create .pi");
    std::fs::write(
        pi_dir.join("settings.json"),
        format!("{{\"ghPath\": \"{}\"}}", gh_path.display()),
    )
    .expect("write settings.json");
    session.set_env(
        "PI_CONFIG_PATH",
        &pi_dir.join("settings.json").display().to_string(),
    );
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

    session.launch(&ftui_args());
    session.wait_and_capture("startup", "pi interactive stack", STARTUP_TIMEOUT);

    // The success message's LAST paragraph, so the capture cannot land between
    // two frames of the same multi-paragraph message.
    let pane = session.send_text_and_wait("share", "/share", "Gist:", COMMAND_TIMEOUT);
    assert!(
        pane.contains(gist_url),
        "the gist URL the mock printed must reach the transcript; pane:\n{pane}"
    );
    assert!(
        pane.contains("Share URL:"),
        "the viewer URL paragraph is missing; pane:\n{pane}"
    );

    // What `gh` was actually asked to do. `--public=false` is the whole
    // security claim of this command and it is invisible in the transcript.
    let args_log = std::fs::read_to_string(mock_bin.join("gh_args.log"))
        .expect("the mock gh must have been invoked");
    assert!(
        args_log.contains("--public=false"),
        "the gist must be created secret; gh saw:\n{args_log}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// bd-ydz1t.1: `/share public` is refused WITHOUT invoking `gh`.
///
/// The unit test proves the command never reaches the driver. This proves the
/// consequence that actually matters: no process was spawned, so there is no
/// path by which a public gist could have been created. Asserted on the
/// absence of the mock's argument log, because "the command errored" and "the
/// command never ran" look identical in a transcript.
#[test]
fn e2e_ftui_share_public_never_invokes_gh() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_share_public_refused") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let mock_bin = session.harness.temp_path("mock_bin");
    std::fs::create_dir_all(&mock_bin).expect("create mock_bin");
    let gh_path =
        common::mocks::write_mock_gh_script(&mock_bin, "https://gist.github.com/testuser/never");

    let pi_dir = session.harness.temp_path(".pi");
    std::fs::create_dir_all(&pi_dir).expect("create .pi");
    std::fs::write(
        pi_dir.join("settings.json"),
        format!("{{\"ghPath\": \"{}\"}}", gh_path.display()),
    )
    .expect("write settings.json");
    session.set_env(
        "PI_CONFIG_PATH",
        &pi_dir.join("settings.json").display().to_string(),
    );
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

    session.launch(&ftui_args());
    session.wait_and_capture("startup", "pi interactive stack", STARTUP_TIMEOUT);

    let pane = session.send_text_and_wait(
        "share_public",
        "/share public",
        "public sharing is disabled",
        COMMAND_TIMEOUT,
    );
    assert!(
        !pane.contains("Sharing session"),
        "a refused /share must not start the export; pane:\n{pane}"
    );
    assert!(
        !mock_bin.join("gh_args.log").exists(),
        "gh must never be invoked for `/share public`"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}

/// SSE body for a one-shot OpenAI-compatible completion.
fn openai_sse(text: &str) -> common::harness::MockHttpResponse {
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": text}}]
    });
    let done = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let body = format!("data: {delta}\n\ndata: {done}\n\ndata: [DONE]\n\n");
    common::harness::MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

/// bd-ydz1t.2: `/tan` runs a child agent on the ftui stack and its summary
/// reaches the PARENT AGENT at the next turn boundary.
///
/// The mirror of `e2e_tan_runs_in_background_and_delivers_at_next_turn_boundary`
/// in tests/btw_tan.rs, which carried a comment claiming "FTUI is covered by
/// tests/e2e_ftui.rs" while no such coverage existed. It does now.
///
/// The load-bearing assertions are the SERVER-SIDE ones. A pane can show
/// "(/tan completed)" whether or not the agent ever learned anything; only the
/// second parent request proves the summary was actually fed back into the
/// conversation, which is the whole point of the command.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_ftui_tan_delivers_its_summary_to_the_parent_turn() {
    let Some((_lock, mut session)) = new_locked_session("e2e_ftui_tan_delivery") else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let server = session.harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/tan-role/v1/chat/completions",
        openai_sse("ftui tan child summary marker"),
    );
    server.add_route_queue(
        "POST",
        "/parent/v1/chat/completions",
        vec![
            openai_sse("ftui parent main turn marker"),
            openai_sse("ftui parent processed tan follow-up marker"),
        ],
    );

    let env_root = session.harness.temp_path("ftui-tan-env");
    let coding_dir = env_root.join("agent");
    let sessions_dir = env_root.join("sessions");
    let packages_dir = env_root.join("packages");
    for dir in [&coding_dir, &sessions_dir, &packages_dir] {
        std::fs::create_dir_all(dir).expect("create env dir");
    }

    let models = serde_json::json!({
        "providers": {
            "parent": {
                "api": "openai-completions",
                "baseUrl": format!("{}/parent/v1", server.base_url()),
                "apiKey": "test-key",
                "models": [{"id": "parent-model", "contextWindow": 128_000}]
            },
            "tan-role": {
                "api": "openai-completions",
                "baseUrl": format!("{}/tan-role/v1", server.base_url()),
                "apiKey": "test-key",
                "models": [{"id": "task-model", "contextWindow": 128_000}]
            }
        }
    });
    std::fs::write(
        coding_dir.join("models.json"),
        serde_json::to_vec_pretty(&models).expect("serialize models"),
    )
    .expect("write models");
    let settings_path = env_root.join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{"modelRoles":{"task":"tan-role/task-model"},"checkForUpdates":false,"approval":{"mode":"yolo"}}"#,
    )
    .expect("write settings");

    session.set_env("PI_CODING_AGENT_DIR", &coding_dir.display().to_string());
    session.set_env("PI_CONFIG_PATH", &settings_path.display().to_string());
    session.set_env("PI_SESSIONS_DIR", &sessions_dir.display().to_string());
    session.set_env("PI_PACKAGE_DIR", &packages_dir.display().to_string());
    session.set_env("PI_NO_AUTO_UPDATE_CHECK", "1");
    session.set_env("PI_WORKSPACE_TRUST", "trusted");

    session.launch(&[
        "--ftui",
        "--provider",
        "parent",
        "--model",
        "parent-model",
        "--tools",
        "subagent",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "ftui tan e2e parent",
    ]);
    session.wait_and_capture("startup", "pi interactive stack", STARTUP_TIMEOUT);

    let started = session.send_text_and_wait(
        "start_tan",
        "/tan update the changelog",
        "(/tan started)",
        COMMAND_TIMEOUT,
    );
    assert!(
        started.contains("update the changelog"),
        "the started note must echo the work; pane:\n{started}"
    );

    let completed =
        session.wait_and_capture("tan_completed", "(/tan completed)", Duration::from_secs(60));
    assert!(
        completed.contains("ftui tan child summary marker"),
        "the child's answer must reach the user; pane:\n{completed}"
    );

    let main_turn = session.send_text_and_wait(
        "main_turn",
        "continue main work",
        "ftui parent main turn marker",
        Duration::from_secs(30),
    );
    assert!(main_turn.contains("ftui parent main turn marker"));

    session.wait_and_capture(
        "tan_follow_up_boundary",
        "ftui parent processed tan follow-up marker",
        Duration::from_secs(30),
    );

    // The assertions that actually prove the feature. The pane above could
    // look right with the agent none the wiser.
    let requests = server.requests();
    let role_requests = requests
        .iter()
        .filter(|request| request.path == "/tan-role/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(
        role_requests.len(),
        1,
        "exactly one child agent request was expected"
    );
    let role_body = role_requests
        .first()
        .map(|request| String::from_utf8_lossy(&request.body))
        .unwrap_or_default();
    assert!(
        role_body.contains("Task: update the changelog"),
        "the child must receive the work; body:\n{role_body}"
    );

    let parent_requests = requests
        .iter()
        .filter(|request| request.path == "/parent/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(
        parent_requests.len(),
        2,
        "expected the main turn plus the tan follow-up turn"
    );
    let follow_up_body = parent_requests
        .get(1)
        .map(|request| String::from_utf8_lossy(&request.body))
        .unwrap_or_default();
    assert!(
        follow_up_body.contains("[background tan"),
        "the follow-up seam must feed the summary back to the parent agent, \
         not just to the screen; body:\n{follow_up_body}"
    );
    assert!(
        follow_up_body.contains("ftui tan child summary marker"),
        "the parent must receive the CHILD's answer; body:\n{follow_up_body}"
    );

    quit_and_assert_clean(&session);
    session.write_artifacts();
}
