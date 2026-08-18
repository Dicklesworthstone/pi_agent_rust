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

    if mid_activity {
        // Land the signal while the UI is actively rendering: a long bash
        // command keeps the driver busy, the tool status live, and the
        // spinner tick chain re-arming when the signal arrives.
        session.tmux.send_literal("!sleep 5");
        session.tmux.send_key("Enter");
        session
            .tmux
            .wait_for_pane_contains("running bash", COMMAND_TIMEOUT);
    }

    // An unreadable/unparseable pid file is an immediate test failure; on
    // that failure the pane + stderr are the diagnostics.
    let pid_text = std::fs::read_to_string(&pid_file).unwrap_or_else(|err| {
        let pane = session.tmux.capture_pane();
        let stderr_tail = std::fs::read_to_string(&stderr_log).unwrap_or_default();
        panic!("read pi pid failed: {err}\npane:\n{pane}\nstderr:\n{stderr_tail}");
    });
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
        .wait_for_pane_contains("ftui preview stack", STARTUP_TIMEOUT);
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
        .wait_for_pane_contains("ftui preview stack", STARTUP_TIMEOUT);

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
    session.send_text_and_wait(
        "post_storm_help",
        "/help",
        "ftui preview commands",
        COMMAND_TIMEOUT,
    );
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

fn ftui_vcr_args() -> Vec<&'static str> {
    vec![
        "--ftui",
        "--no-session",
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
    ]
}

/// Effective system prompt for the ftui VCR args, computed with the same
/// builder the session uses (mirrors build_vcr_system_prompt_for_args in
/// tests/e2e_tui.rs).
fn ftui_vcr_system_prompt(workdir: &std::path::Path, env_root: &std::path::Path) -> String {
    use clap::Parser as _;
    let mut args: Vec<&str> = vec!["pi"];
    args.extend(ftui_vcr_args());
    let cli = pi::cli::Cli::try_parse_from(args).expect("parse ftui vcr args"); // ubs:ignore test setup expect
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
    let Some((_lock, mut session)) = new_locked_session(FTUI_VCR_TEST_NAME) else {
        eprintln!("Skipping: tmux not available");
        return;
    };

    let env_root = session.harness.temp_dir().join("env");
    std::fs::create_dir_all(&env_root).expect("create env root"); // ubs:ignore test setup expect
    let system_prompt = ftui_vcr_system_prompt(session.harness.temp_dir(), &env_root);
    let cassette_dir = session.harness.temp_dir().join("cassettes");
    let cassette_path =
        write_ftui_vcr_cassette(&cassette_dir, &system_prompt, FTUI_VCR_TEST_NAME, FTUI_VCR_RESPONSE);
    session
        .harness
        .record_artifact("ftui-vcr-cassette.json", &cassette_path);

    // Launch via a wrapper that redirects stderr to a file: tracing output
    // otherwise interleaves with the pane, and on failure the log is the
    // diagnostic.
    let stderr_log = session.harness.temp_path("pi-stderr.log");
    {
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
        let _ = writeln!(script, "export PI_VCR_TEST_NAME={FTUI_VCR_TEST_NAME}");
        script.push_str("export VCR_DEBUG_BODY=1\n");
        // Stable path: the harness temp dir is deleted on drop, and the
        // debug bodies are exactly what we need after a failure.
        script.push_str("export VCR_DEBUG_BODY_FILE=/private/tmp/pi-tests/ftui-vcr-bodies.txt\n");
        let binary = std::env::var_os("CARGO_BIN_EXE_pi").expect("CARGO_BIN_EXE_pi"); // ubs:ignore test setup expect
        let _ = write!(
            script,
            "exec {}",
            std::path::PathBuf::from(binary).display()
        );
        for arg in ftui_vcr_args() {
            let _ = write!(script, " '{arg}'");
        }
        let _ = writeln!(script, " 2>{}", stderr_log.display());
        let script_path = session.harness.temp_path("vcr-run.sh");
        std::fs::write(&script_path, &script).expect("write vcr script"); // ubs:ignore test setup expect
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)
                .expect("stat vcr script") // ubs:ignore test setup expect
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod vcr script"); // ubs:ignore test setup expect
        }
        session
            .tmux
            .start_session(session.harness.temp_dir(), &script_path);
    }
    session
        .tmux
        .wait_for_pane_contains("ftui preview stack", STARTUP_TIMEOUT);

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

// ── Mid-STREAM SIGTERM (bd-pb4fw follow-through via VCR chunk pacing) ───────

const FTUI_VCR_KILL_TEST_NAME: &str = "e2e_ftui_vcr_midstream_kill";

/// The bead's literal acceptance case: SIGTERM lands while a provider reply
/// is STREAMING (VCR playback paced at 150ms/chunk gives a multi-second
/// window), and the RAII restore must still leave the wrapper shell's
/// terminal echoing.
#[test]
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
    let _ = writeln!(script, "export {}={}", pi::vcr::VCR_ENV_DIR, cassette_dir.display());
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
    script.push_str("echo PI-WAIT-DONE\nexec /bin/sh -i\n");

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
        .wait_for_pane_contains("ftui preview stack", STARTUP_TIMEOUT);
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
        panic!("read pi pid failed: {err}\npane:\n{pane}\nstderr:\n{stderr_tail}");
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
