//! E2E (bd-cv653.3.2): cross-model failover, auth-error refusal, and
//! credential rotation over real processes against a mock OpenAI-compatible
//! server. No network beyond loopback; structured JSONL logs per
//! `tests/common/logging.rs`.
//!
//! Case 1: primary 429s until the retry budget is spent → the fallback-chain
//!         entry completes the turn (print mode).
//! Case 2: primary 401s → loud error, the fallback entry is NEVER called.
//! Case 3: `OPENAI_API_KEYS=k1,k2` with a 429 on k1 → the retry carries k2.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn error_response(status: u16, body: &str) -> MockHttpResponse {
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.as_bytes().to_vec(),
    }
}

fn text_sse_body(text: &str) -> String {
    [
        format!(r#"data: {{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#).as_str(),
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

/// SSE body for the `OpenAI` Responses API: `output_text` delta plus
/// `response.completed`.
fn responses_sse_body(text: &str) -> String {
    [
        format!(
            r#"data: {{"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"{text}"}}"#
        )
        .as_str(),
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        "",
    ]
    .join("\n")
}

struct PiEnv {
    root: std::path::PathBuf,
}

impl PiEnv {
    fn new(harness: &TestHarness) -> Self {
        Self::with_cooldown_secs(harness, None)
    }

    /// Same environment with an explicit `failoverCooldownSecs`.
    ///
    /// The default is 300s, which is fine for the single-prompt lifecycle tests
    /// and useless for the restoration ones (bd-gm481.1): 0 makes the primary
    /// eligible again immediately, and a large value pins the session to the
    /// fallback for the rest of the run. Those are the two behaviours to test.
    fn with_cooldown_secs(harness: &TestHarness, cooldown_secs: Option<u64>) -> Self {
        let root = harness.temp_path("pi-env");
        std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
        std::fs::create_dir_all(root.join("home")).expect("mkdir home");
        let cooldown = cooldown_secs
            .map(|secs| format!(r#", "failoverCooldownSecs": {secs}"#))
            .unwrap_or_default();
        std::fs::write(
            root.join("settings.json"),
            format!(
                r#"{{"retry": {{"enabled": true, "maxRetries": 1, "fallbackChains": {{"default": ["e2ebackup/backup-model"]}}{cooldown}}}, "checkForUpdates": false}}"#
            ),
        )
        .expect("write settings.json");
        Self { root }
    }

    fn write_models(&self, base_url: &str) {
        self.write_models_split(
            &format!("{base_url}/primary/v1"),
            &format!("{base_url}/backup/v1"),
        );
    }

    /// Same models.json with the two providers pointed at INDEPENDENT bases.
    ///
    /// The abort test needs a fallback that accepts a request and never answers
    /// it, which no route on the mock server can do — the server writes its
    /// response as soon as it has read the request. A bare listener that is
    /// never accepted from does exactly that, and it needs its own base URL.
    fn write_models_split(&self, primary_base: &str, backup_base: &str) {
        let models_json = format!(
            r#"{{"providers": {{
                "e2eprimary": {{
                    "api": "openai-completions",
                    "baseUrl": "{primary_base}",
                    "apiKey": "primary-key",
                    "models": [{{"id": "primary-model", "contextWindow": 128000}}]
                }},
                "e2ebackup": {{
                    "api": "openai-completions",
                    "baseUrl": "{backup_base}",
                    "apiKey": "backup-key",
                    "models": [{{"id": "backup-model", "contextWindow": 128000}}]
                }}
            }}}}"#
        );
        std::fs::write(self.root.join("agent/models.json"), models_json)
            .expect("write models.json");
    }

    fn command(&self, binary: &std::path::Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("HOME", self.root.join("home"))
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .env("PI_CONFIG_PATH", self.root.join("settings.json"))
            .env("PI_SESSIONS_DIR", self.root.join("sessions"))
            .env("PI_PACKAGE_DIR", self.root.join("packages"))
            .env("PI_NO_AUTO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            command.env_remove(key);
        }
        command
    }
}

fn run_and_collect(mut child: std::process::Child, deadline_secs: u64) -> (String, String) {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().expect("collect output");
                return (
                    String::from_utf8_lossy(&output.stdout).to_string(),
                    String::from_utf8_lossy(&output.stderr).to_string(),
                );
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(deadline_secs) {
                    let _ = child.kill();
                    let output = child.wait_with_output().expect("collect output");
                    return (
                        String::from_utf8_lossy(&output.stdout).to_string(),
                        format!(
                            "TIMEOUT: killed after {deadline_secs}s\n{}",
                            String::from_utf8_lossy(&output.stderr)
                        ),
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => panic!("wait failed: {err}"),
        }
    }
}

#[test]
fn e2e_failover_429_walks_chain_and_completes() {
    let harness = TestHarness::new("e2e_failover_429_walks_chain_and_completes");
    harness
        .log()
        .info("setup", "primary 429s, backup serves text");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::new(&harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    harness
        .log()
        .info("action", "spawning pi --print on 429 primary");
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 90);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("stdout".to_string(), stdout.clone()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });

    assert!(
        stdout.contains("backup ok"),
        "failover should complete on the backup model; stdout: {stdout}\nstderr: {stderr}"
    );
    let backup_requests = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/backup/v1/chat/completions")
        .count();
    assert!(
        backup_requests >= 1,
        "backup provider must receive the continuation request"
    );
    assert!(
        server
            .requests()
            .iter()
            .filter(|r| r.path == "/primary/v1/chat/completions")
            .count()
            >= 2,
        "primary must exhaust the same-model retry budget first"
    );
    let path = harness.temp_path("e2e_failover_429.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_429.jsonl", &path);
}

/// Run `pi --rpc` with ONE piped prompt against the mock server and return the
/// parsed event stream plus the raw stdout/stderr for diagnostics.
///
/// Closing stdin after the single request is the documented one-shot shape:
/// `printf '{"type":"prompt",...}' | pi --mode rpc` drains the in-flight turn
/// and still emits the full stream through `agent_end` before exiting (gh #137,
/// src/rpc.rs). The RPC loop serialises the same `AgentEvent` values print mode
/// does, so `event_kinds` reads both surfaces.
fn run_rpc_failover(
    harness: &TestHarness,
    server: &common::harness::MockHttpServer,
    label: &str,
) -> (Vec<serde_json::Value>, String, String) {
    let env = PiEnv::new(harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command
        .args([
            "--rpc",
            "--provider",
            "e2eprimary",
            "--model",
            "primary-model",
            "--no-extensions",
        ])
        .stdin(Stdio::piped());
    harness.log().info("action", label);
    let mut child = command.spawn().expect("spawn pi --rpc");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("pi --rpc stdin was not piped");
        writeln!(stdin, r#"{{"type":"prompt","id":"p1","message":"ping"}}"#)
            .expect("write the prompt request");
        stdin.flush().expect("flush the prompt request");
    }
    drop(child.stdin.take());
    let (stdout, stderr) = run_and_collect(child, 90);
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("event_count".to_string(), events.len().to_string()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });
    (events, stdout, stderr)
}

/// Drive `pi --rpc` to a failover and then abort the fallback turn mid-flight.
///
/// The abort has to land INSIDE the fallback turn or the test proves nothing,
/// so nothing here sleeps and hopes. The fallback base URL points at a listener
/// that is bound and never accepted from: the connection completes in the
/// kernel's backlog, pi's request is written, and no response ever comes, so
/// the fallback turn stays open until something aborts it. The abort is then
/// sent the moment `failover_start` appears on stdout, which is the boundary
/// this is testing.
fn run_rpc_failover_then_abort(
    harness: &TestHarness,
    server: &common::harness::MockHttpServer,
    stalled_backup_base: &str,
    label: &str,
) -> (Vec<serde_json::Value>, String, String) {
    let env = PiEnv::new(harness);
    env.write_models_split(
        &format!("{}/primary/v1", server.base_url()),
        stalled_backup_base,
    );
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command
        .args([
            "--rpc",
            "--provider",
            "e2eprimary",
            "--model",
            "primary-model",
            "--no-extensions",
        ])
        .stdin(Stdio::piped());
    harness.log().info("action", label);
    let mut child = command.spawn().expect("spawn pi --rpc");

    let stdout = child.stdout.take().expect("pi --rpc stdout was not piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let pump = std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("pi --rpc stdin was not piped");
        writeln!(stdin, r#"{{"type":"prompt","id":"p1","message":"ping"}}"#)
            .expect("write the prompt request");
        stdin.flush().expect("flush the prompt request");
    }

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut lines: Vec<String> = Vec::new();
    let mut aborted = false;
    loop {
        let Ok(line) = rx.recv_timeout(Duration::from_millis(250)) else {
            assert!(
                Instant::now() < deadline,
                "pi --rpc never reached the failover boundary: {lines:?}"
            );
            continue;
        };
        let kind = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|event| {
                event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        lines.push(line);
        match kind.as_deref() {
            Some("failover_start") if !aborted => {
                use std::io::Write as _;
                let stdin = child.stdin.as_mut().expect("pi --rpc stdin was not piped");
                writeln!(stdin, r#"{{"type":"abort","id":"a1"}}"#).expect("write the abort");
                stdin.flush().expect("flush the abort");
                aborted = true;
            }
            Some("agent_end") if aborted => break,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "pi --rpc never produced a terminal agent_end after the abort: {lines:?}"
        );
    }

    drop(child.stdin.take());
    let status = run_to_exit(&mut child, 60);
    drop(rx);
    let _ = pump.join();
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        use std::io::Read as _;
        let _ = handle.read_to_string(&mut stderr);
    }
    let stdout = lines.join("\n");
    let events = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("event_count".to_string(), events.len().to_string()));
        ctx.push(("exit".to_string(), format!("{status:?}")));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });
    (events, stdout, stderr)
}

/// Wait for a child that has already had its stdout drained elsewhere.
fn run_to_exit(child: &mut std::process::Child, deadline_secs: u64) -> Option<i32> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(deadline_secs) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // Mirrors run_and_collect above: a wait() that errors is a broken
            // harness, not a test outcome.
            // ubs:ignore-next-line test harness — waitpid failure is unrecoverable
            Err(err) => panic!("wait failed: {err}"),
        }
    }
}

/// Run `pi --print --mode json` against the mock server and return the parsed
/// event stream plus the raw stdout/stderr for diagnostics.
fn run_print_json_failover(
    harness: &TestHarness,
    server: &common::harness::MockHttpServer,
    label: &str,
) -> (Vec<serde_json::Value>, String, String) {
    let env = PiEnv::new(harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--mode",
        "json",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    harness.log().info("action", label);
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 90);
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("event_count".to_string(), events.len().to_string()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });
    (events, stdout, stderr)
}

/// Run `pi --print --mode json` with TWO prompts against the mock server.
///
/// Two prompts is the whole point for bd-gm481.1: restoration is a
/// between-prompt lifecycle, so a single-prompt run can never observe it.
fn run_print_json_two_prompts(
    harness: &TestHarness,
    server: &common::harness::MockHttpServer,
    cooldown_secs: u64,
    label: &str,
) -> (Vec<serde_json::Value>, String, String) {
    let env = PiEnv::with_cooldown_secs(harness, Some(cooldown_secs));
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    // Flags first: `Cli::args` is `trailing_var_arg`, so everything after the
    // first positional is captured as another message. Both positionals below
    // are messages, which is what makes this a two-prompt run.
    command.args([
        "--print",
        "--mode",
        "json",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
        "pong",
    ]);
    harness.log().info("action", label);
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 120);
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("event_count".to_string(), events.len().to_string()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });
    (events, stdout, stderr)
}

/// The `failover_end` records that report a primary restoration.
fn restoration_events(events: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    events
        .iter()
        .filter(|event| {
            event.get("type").and_then(serde_json::Value::as_str) == Some("failover_end")
                && event
                    .get("restoredPrimary")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        })
        .collect()
}

fn event_kinds(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event.get("type").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// bd-2vmu6.1: in JSON mode a turn that walks the fallback chain closes its
/// `failover_start` with exactly one `failover_end { restoredPrimary: false }`
/// naming the fallback, emitted with the other lifecycle closers after the
/// fallback attempt's `agent_end`, on the success path.
#[test]
fn e2e_failover_json_mode_closes_lifecycle_after_backup_success() {
    let harness = TestHarness::new("e2e_failover_json_mode_closes_lifecycle_after_backup_success");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let (events, stdout, stderr) = run_print_json_failover(
        &harness,
        &server,
        "spawning pi --print --mode json on 429 primary",
    );
    let kinds = event_kinds(&events);
    let start = kinds
        .iter()
        .position(|k| k == "failover_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let end = kinds
        .iter()
        .position(|k| k == "failover_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let agent_end = kinds
        .iter()
        .rposition(|k| k == "agent_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("agent_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    // Print JSON mode streams the agent loop's own `agent_end` as each attempt
    // finishes and emits the retry loop's lifecycle closers (`auto_retry_end`,
    // `failover_end`) after the last one, so the close lands after the final
    // `agent_end`, never before the `failover_start` it closes. (The RPC loop
    // defers its terminal `agent_end` and closes the lifecycle before it.)
    assert!(
        start < end && agent_end < end,
        "failover lifecycle must close after the fallback attempt's agent_end: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_end").count(),
        1,
        "exactly one failover_end per failover_start: {kinds:?}"
    );
    // bd-2vmu6: the retry lifecycle closes BEFORE the failover one opens, and
    // exactly once. This is the bead's core claim — an exhausted retry budget
    // transitioning into failover used to emit `auto_retry_start` and then
    // reset `retry_count` to 0 on the way into the swap, so the matching
    // `auto_retry_end` never fired and clients saw an open lifecycle inherited
    // by whatever the fallback did next. The fix closes it from the failover
    // path itself (`retry_attempt_to_end`), so removing that plumbing shows up
    // here as a missing end rather than as a subtly wrong event stream.
    let retry_starts = kinds.iter().filter(|k| *k == "auto_retry_start").count();
    let retry_ends = kinds.iter().filter(|k| *k == "auto_retry_end").count();
    assert_eq!(
        (retry_starts, retry_ends),
        (1, 1),
        "one retry lifecycle, opened and closed exactly once: {kinds:?}\n{stdout}\n{stderr}"
    );
    let retry_end = kinds
        .iter()
        .position(|k| k == "auto_retry_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    assert!(
        retry_end < start,
        "the retry lifecycle closes before the failover one opens, so the two cannot be confused \
         for one another: {kinds:?}"
    );
    let retry_end_event = &events[retry_end]; // ubs:ignore index proven by position() above
    assert_eq!(
        retry_end_event["success"],
        serde_json::Value::Bool(false),
        "the retries really did fail — that is why the failover happened: {retry_end_event}"
    );

    // bd-oqo03: `attempt` is the successful-swap ordinal within the turn, and
    // `chainIndex` is where the entry sits in the chain. They are reported
    // separately because they answer different questions — budget versus
    // provenance — and `attempt` used to carry the index, which stopped
    // meaning anything once the walk began skipping entries. Here the single
    // chain entry sits at index 0 and is the turn's first swap, so the two
    // values differ and a regression that puts the cursor back in `attempt`
    // shows up as 0 where 1 is expected.
    let start_event = &events[start]; // ubs:ignore index proven by position() above
    assert_eq!(
        start_event["attempt"], 1,
        "the first successful swap of the turn is attempt 1, not the chain cursor: {start_event}"
    );
    assert_eq!(
        start_event["chainIndex"], 0,
        "the only fallback entry sits at chain index 0: {start_event}"
    );

    let end_event = &events[end]; // ubs:ignore index proven by position() above
    assert_eq!(end_event["success"], serde_json::Value::Bool(true));
    assert_eq!(end_event["restoredPrimary"], serde_json::Value::Bool(false));
    assert_eq!(end_event["provider"], "e2ebackup");
    assert_eq!(end_event["model"], "backup-model");
    assert!(
        events[agent_end]["error"].is_null(), // ubs:ignore index proven by rposition() above
        "the backup completes the turn: {}",
        events[agent_end]
    );

    let path = harness.temp_path("e2e_failover_json_success.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_json_success.jsonl", &path);
}

/// bd-2vmu6.1: the lifecycle also closes when the fallback entry itself
/// fails, with `success: false`, in the same position.
#[test]
fn e2e_failover_json_mode_closes_lifecycle_after_backup_failure() {
    let harness = TestHarness::new("e2e_failover_json_mode_closes_lifecycle_after_backup_failure");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        error_response(
            503,
            r#"{"error":{"type":"overloaded_error","message":"backup overloaded"}}"#,
        ),
    );

    let (events, stdout, stderr) = run_print_json_failover(
        &harness,
        &server,
        "spawning pi --print --mode json on 429 primary and 503 backup",
    );
    let kinds = event_kinds(&events);
    let start = kinds
        .iter()
        .position(|k| k == "failover_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let end = kinds
        .iter()
        .position(|k| k == "failover_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let agent_end = kinds
        .iter()
        .rposition(|k| k == "agent_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("agent_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    // Print JSON mode streams the agent loop's own `agent_end` as each attempt
    // finishes and emits the retry loop's lifecycle closers (`auto_retry_end`,
    // `failover_end`) after the last one, so the close lands after the final
    // `agent_end`, never before the `failover_start` it closes. (The RPC loop
    // defers its terminal `agent_end` and closes the lifecycle before it.)
    assert!(
        start < end && agent_end < end,
        "failover lifecycle must close after the fallback attempt's agent_end: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_end").count(),
        1,
        "exactly one failover_end per failover_start: {kinds:?}"
    );
    let end_event = &events[end]; // ubs:ignore index proven by position() above
    assert_eq!(end_event["success"], serde_json::Value::Bool(false));
    assert_eq!(end_event["restoredPrimary"], serde_json::Value::Bool(false));
    assert_eq!(end_event["provider"], "e2ebackup");
    assert_eq!(end_event["model"], "backup-model");
    assert!(
        !events[agent_end]["error"].is_null(), // ubs:ignore index proven by rposition() above
        "the backup failure ends the turn with an error: {}",
        events[agent_end]
    );

    let path = harness.temp_path("e2e_failover_json_failure.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_json_failure.jsonl", &path);
}

/// bd-2vmu6: the fallback runs a retry lifecycle of its own, and the primary's
/// is already closed when it opens.
///
/// This is the collision the bead was opened for. A successful swap resets the
/// retry budget to zero, so the fallback's first retry is `attempt: 1` — the
/// same number the primary's lifecycle was using. If the primary's
/// `auto_retry_end` is not emitted on the way into the swap, the stream reads
/// `auto_retry_start(1) -> failover_start -> auto_retry_start(1)` and nothing
/// in it distinguishes a second lifecycle from a continuation of the first.
///
/// The other two lifecycle tests let the fallback answer on its first request,
/// so only one retry lifecycle ever exists in them and the ordering claim is
/// vacuous. Here the fallback refuses once before answering, so both
/// lifecycles are real and the event stream has to keep them apart.
#[test]
fn e2e_failover_json_mode_gives_the_fallback_its_own_retry_lifecycle() {
    let harness =
        TestHarness::new("e2e_failover_json_mode_gives_the_fallback_its_own_retry_lifecycle");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    // No static route behind the queue: a third request to the fallback is a
    // regression in its own right and should fail loudly rather than be served.
    server.add_route_queue(
        "POST",
        "/backup/v1/chat/completions",
        vec![
            error_response(
                503,
                r#"{"error":{"type":"overloaded_error","message":"backup warming up"}}"#,
            ),
            sse_response(text_sse_body("backup ok on the retry")),
        ],
    );

    let (events, stdout, stderr) = run_print_json_failover(
        &harness,
        &server,
        "spawning pi --print --mode json on a 429 primary and a fallback that needs one retry",
    );
    let kinds = event_kinds(&events);

    // Two lifecycles, each opened and closed exactly once. An implementation
    // whose only lifecycle state is `retry_count`, reset to 0 on the swap,
    // emits two starts and one end here.
    assert_eq!(
        (
            kinds.iter().filter(|k| *k == "auto_retry_start").count(),
            kinds.iter().filter(|k| *k == "auto_retry_end").count(),
        ),
        (2, 2),
        "the primary's retry lifecycle and the fallback's are each opened and closed: \
         {kinds:?}\n{stdout}\n{stderr}"
    );

    // ...and they never overlap. Ordering is the whole claim, because both
    // lifecycles report `attempt: 1` and are otherwise indistinguishable.
    let retry_frames: Vec<&str> = kinds
        .iter()
        .map(String::as_str)
        .filter(|kind| matches!(*kind, "auto_retry_start" | "auto_retry_end"))
        .collect();
    assert_eq!(
        retry_frames,
        [
            "auto_retry_start",
            "auto_retry_end",
            "auto_retry_start",
            "auto_retry_end",
        ],
        "each retry lifecycle closes before the next one opens: {kinds:?}\n{stdout}\n{stderr}"
    );

    let failover_start = kinds
        .iter()
        .position(|k| k == "failover_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let failover_end = kinds
        .iter()
        .position(|k| k == "failover_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let primary_retry_end = kinds
        .iter()
        .position(|k| k == "auto_retry_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let fallback_retry_start = kinds
        .iter()
        .rposition(|k| k == "auto_retry_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let fallback_retry_end = kinds
        .iter()
        .rposition(|k| k == "auto_retry_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_start").count(),
        1,
        "one swap, so one failover_start: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_end").count(),
        1,
        "exactly one failover_end per failover_start: {kinds:?}"
    );
    assert!(
        primary_retry_end < failover_start,
        "the primary's retry lifecycle closes on the way into the swap: {kinds:?}"
    );
    assert!(
        failover_start < fallback_retry_start,
        "the second retry lifecycle belongs to the fallback, so it opens after the swap: {kinds:?}"
    );
    assert!(
        fallback_retry_end < failover_end,
        "the fallback's retry lifecycle closes inside the failover lifecycle that contains it: \
         {kinds:?}"
    );

    // The two ends disagree about success, which is what each one is reporting:
    // the primary's retries really did fail, and the fallback's really did
    // recover. A stream that reused one lifecycle for both could not say this.
    let primary_end_event = &events[primary_retry_end]; // ubs:ignore index proven by position() above
    assert_eq!(
        primary_end_event["success"],
        serde_json::Value::Bool(false),
        "the primary exhausted its budget: {primary_end_event}"
    );
    assert_eq!(
        primary_end_event["attempt"], 1,
        "maxRetries is 1, so the primary's lifecycle ends on attempt 1: {primary_end_event}"
    );
    let fallback_end_event = &events[fallback_retry_end]; // ubs:ignore index proven by rposition() above
    assert_eq!(
        fallback_end_event["success"],
        serde_json::Value::Bool(true),
        "the fallback recovered on its retry: {fallback_end_event}"
    );
    assert_eq!(
        fallback_end_event["attempt"], 1,
        "the fallback's budget starts over, so its lifecycle is attempt 1 as well — ordering is \
         the only thing that separates the two: {fallback_end_event}"
    );

    let end_event = &events[failover_end]; // ubs:ignore index proven by position() above
    assert_eq!(end_event["success"], serde_json::Value::Bool(true));
    assert_eq!(end_event["restoredPrimary"], serde_json::Value::Bool(false));
    assert_eq!(end_event["provider"], "e2ebackup");
    assert_eq!(end_event["model"], "backup-model");

    // The events above describe provider traffic that has to have happened:
    // two requests to the primary (the attempt and its one retry) and two to
    // the fallback (the refusal and the retry that answered). Without this the
    // whole assertion set could pass on an event stream that was merely
    // well-formed.
    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("/primary/")).count(),
        2,
        "one primary attempt plus its single retry: {paths:?}"
    );
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("/backup/")).count(),
        2,
        "the fallback was really retried, not merely reported as retried: {paths:?}"
    );

    let path = harness.temp_path("e2e_failover_json_fallback_retry.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_json_fallback_retry.jsonl", &path);
}

/// bd-2vmu6, RPC half: the same collision, over `pi --rpc`, which is the
/// surface SDK clients actually drive.
///
/// The bead's acceptance asks for this scenario on BOTH surfaces. The RPC
/// lifecycle test that exists installs a fallback pointing at an unreachable
/// port, so its fallback turn can only fail immediately -- there is no way for
/// it to run a retry lifecycle of its own, which is the thing being claimed.
/// Running the real binary against the mock server is what makes the fallback
/// able to fail once and then succeed.
///
/// The two surfaces close their lifecycles in a different ORDER, and that
/// difference is asserted rather than papered over: print streams the agent
/// loop's own `agent_end` per attempt and emits the closers after the last one,
/// while RPC defers its terminal `agent_end` and closes the lifecycle first.
#[test]
fn e2e_failover_rpc_mode_gives_the_fallback_its_own_retry_lifecycle() {
    let harness =
        TestHarness::new("e2e_failover_rpc_mode_gives_the_fallback_its_own_retry_lifecycle");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route_queue(
        "POST",
        "/backup/v1/chat/completions",
        vec![
            error_response(
                503,
                r#"{"error":{"type":"overloaded_error","message":"backup warming up"}}"#,
            ),
            sse_response(text_sse_body("backup ok on the retry")),
        ],
    );

    let (events, stdout, stderr) = run_rpc_failover(
        &harness,
        &server,
        "spawning pi --rpc on a 429 primary and a fallback that needs one retry",
    );
    let kinds = event_kinds(&events);

    assert_eq!(
        (
            kinds.iter().filter(|k| *k == "auto_retry_start").count(),
            kinds.iter().filter(|k| *k == "auto_retry_end").count(),
        ),
        (2, 2),
        "the primary's retry lifecycle and the fallback's are each opened and closed: \
         {kinds:?}\n{stdout}\n{stderr}"
    );
    let retry_frames: Vec<&str> = kinds
        .iter()
        .map(String::as_str)
        .filter(|kind| matches!(*kind, "auto_retry_start" | "auto_retry_end"))
        .collect();
    assert_eq!(
        retry_frames,
        [
            "auto_retry_start",
            "auto_retry_end",
            "auto_retry_start",
            "auto_retry_end",
        ],
        "each retry lifecycle closes before the next one opens: {kinds:?}\n{stdout}\n{stderr}"
    );

    let failover_start = kinds
        .iter()
        .position(|k| k == "failover_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let failover_end = kinds
        .iter()
        .position(|k| k == "failover_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let primary_retry_end = kinds
        .iter()
        .position(|k| k == "auto_retry_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let fallback_retry_start = kinds
        .iter()
        .rposition(|k| k == "auto_retry_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("auto_retry_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let terminal_agent_end = kinds
        .iter()
        .rposition(|k| k == "agent_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("agent_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_end").count(),
        1,
        "exactly one failover_end per failover_start: {kinds:?}"
    );
    assert!(
        primary_retry_end < failover_start,
        "the primary's retry lifecycle closes on the way into the swap: {kinds:?}"
    );
    assert!(
        failover_start < fallback_retry_start,
        "the second retry lifecycle belongs to the fallback, so it opens after the swap: {kinds:?}"
    );
    assert!(
        failover_end < terminal_agent_end,
        "RPC closes the failover lifecycle BEFORE its terminal agent_end, unlike print: {kinds:?}"
    );

    let end_event = &events[failover_end]; // ubs:ignore index proven by position() above
    assert_eq!(end_event["success"], serde_json::Value::Bool(true));
    assert_eq!(end_event["restoredPrimary"], serde_json::Value::Bool(false));
    assert_eq!(end_event["provider"], "e2ebackup");
    assert_eq!(end_event["model"], "backup-model");

    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("/primary/")).count(),
        2,
        "one primary attempt plus its single retry: {paths:?}"
    );
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("/backup/")).count(),
        2,
        "the fallback was really retried, not merely reported as retried: {paths:?}"
    );

    let path = harness.temp_path("e2e_failover_rpc_fallback_retry.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_rpc_fallback_retry.jsonl", &path);
}

/// bd-2vmu6, abort at the boundary: a turn aborted while it is running on the
/// fallback still closes both lifecycles, and closes them truthfully.
///
/// This is the fourth of the bead's five named scenarios and the one with no
/// coverage on either surface. It is the hardest to make deterministic, because
/// "abort at the boundary" is a race by description: abort too early and the
/// swap has not happened, too late and the turn is already over. Two things
/// remove the race entirely. The fallback points at a listener that is bound
/// and NEVER ACCEPTED FROM, so the fallback turn cannot finish on its own — the
/// connection completes in the kernel backlog and no response ever arrives. And
/// the abort is sent on observing `failover_start` rather than after a sleep,
/// so it is sent exactly once the swap has been announced.
///
/// What is being protected: `failovers_this_turn > 0` is what drives the
/// terminal `failover_end` in `run_prompt_with_retry`, and an abort takes a
/// different path out of the turn than success or provider error do. If that
/// path skipped the closers, a client would see `auto_retry_start` and
/// `failover_start` with nothing terminating either, on the one exit that is
/// most likely to be hit by a user pressing Ctrl+C.
#[test]
fn e2e_failover_rpc_abort_at_the_boundary_still_closes_both_lifecycles() {
    let harness =
        TestHarness::new("e2e_failover_rpc_abort_at_the_boundary_still_closes_both_lifecycles");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );

    // Bound, never accepted from. Held in scope for the whole test so the port
    // stays claimed; dropping it would let the connection be refused instead of
    // hanging, which is a different scenario.
    let stalled = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the stalled fallback");
    let stalled_base = format!(
        "http://{}/v1",
        stalled.local_addr().expect("stalled fallback address")
    );

    let (events, stdout, stderr) = run_rpc_failover_then_abort(
        &harness,
        &server,
        &stalled_base,
        "spawning pi --rpc on a 429 primary and a fallback that never answers",
    );
    let kinds = event_kinds(&events);

    let failover_start = kinds
        .iter()
        .position(|k| k == "failover_start")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("failover_start missing: {kinds:?}\n{stdout}\n{stderr}"));
    let failover_end = kinds
        .iter()
        .position(|k| k == "failover_end")
        // ubs:ignore-next-line test assertion — the open lifecycle IS the bug
        .unwrap_or_else(|| {
            panic!("failover_end missing after abort: {kinds:?}\n{stdout}\n{stderr}")
        });
    let retry_end = kinds
        .iter()
        .position(|k| k == "auto_retry_end")
        // ubs:ignore-next-line test assertion — the open lifecycle IS the bug
        .unwrap_or_else(|| panic!("auto_retry_end missing: {kinds:?}\n{stdout}\n{stderr}"));
    let terminal_agent_end = kinds
        .iter()
        .rposition(|k| k == "agent_end")
        // ubs:ignore-next-line test assertion — a missing lifecycle event is the failure
        .unwrap_or_else(|| panic!("agent_end missing: {kinds:?}\n{stdout}\n{stderr}"));

    assert_eq!(
        (
            kinds.iter().filter(|k| *k == "auto_retry_start").count(),
            kinds.iter().filter(|k| *k == "auto_retry_end").count(),
        ),
        (1, 1),
        "the primary's retry lifecycle opens and closes exactly once, even though the turn was \
         aborted on the fallback: {kinds:?}\n{stdout}\n{stderr}"
    );
    assert_eq!(
        (
            kinds.iter().filter(|k| *k == "failover_start").count(),
            kinds.iter().filter(|k| *k == "failover_end").count(),
        ),
        (1, 1),
        "one swap, one close, on the abort path too: {kinds:?}\n{stdout}\n{stderr}"
    );
    assert!(
        retry_end < failover_start && failover_start < failover_end,
        "the retry lifecycle closes into the swap and the failover lifecycle closes after it, \
         the same order an unaborted turn uses: {kinds:?}"
    );
    assert!(
        failover_end < terminal_agent_end,
        "RPC closes the lifecycle before its terminal agent_end on this path as well: {kinds:?}"
    );

    // Truthfully, not merely present: an aborted fallback turn did not succeed.
    let end_event = &events[failover_end]; // ubs:ignore index proven by position() above
    assert_eq!(
        end_event["success"],
        serde_json::Value::Bool(false),
        "an aborted fallback turn is not a successful one: {end_event}"
    );
    assert_eq!(end_event["restoredPrimary"], serde_json::Value::Bool(false));
    assert_eq!(end_event["provider"], "e2ebackup");
    assert_eq!(end_event["model"], "backup-model");

    // The fallback really was reached and really did hang: the mock server saw
    // the primary's attempt and its one retry, and nothing after, because the
    // fallback's traffic went to the stalled listener instead.
    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("/primary/")).count(),
        2,
        "one primary attempt plus its single retry: {paths:?}"
    );

    let path = harness.temp_path("e2e_failover_rpc_abort_boundary.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_rpc_abort_boundary.jsonl", &path);
}

/// bd-gm481.1: with the cooldown elapsed, the primary comes back between
/// prompts and the next prompt is attempted on it again.
///
/// Print mode had no cooldown tracker and no restoration path at all, so a
/// `--message` sequence that failed over ran every later prompt on the
/// temporary fallback with no way home. The shape below is the proof: the
/// primary refuses every request, so if restoration happens the SECOND prompt
/// must fail over again, and if it does not there is only ever one swap.
#[test]
fn e2e_failover_restores_primary_between_prompts_once_cooldown_elapsed() {
    let harness =
        TestHarness::new("e2e_failover_restores_primary_between_prompts_once_cooldown_elapsed");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let (events, stdout, stderr) = run_print_json_two_prompts(
        &harness,
        &server,
        0,
        "two prompts, cooldown 0: the primary is eligible again immediately",
    );
    let kinds = event_kinds(&events);

    let restorations = restoration_events(&events);
    assert_eq!(
        restorations.len(),
        1,
        "exactly one restoration between the two prompts: {kinds:?}\n{stdout}\n{stderr}"
    );
    let restoration = restorations[0];
    assert_eq!(
        restoration["success"],
        serde_json::Value::Bool(true),
        "a restoration that happened is a success: {restoration}"
    );
    assert_eq!(
        restoration["provider"], "e2eprimary",
        "the restoration names the model the chain started from, not the fallback: {restoration}"
    );
    assert_eq!(restoration["model"], "primary-model");

    // The primary refuses everything, so a restored session fails over again.
    // Two swaps is what proves the first one was actually undone.
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_start").count(),
        2,
        "each prompt starts on the primary and fails over: {kinds:?}\n{stdout}\n{stderr}"
    );

    let path = harness.temp_path("e2e_failover_restore.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_restore.jsonl", &path);
}

/// bd-gm481.1, the other half: before the cooldown elapses the session stays on
/// the fallback, and says nothing about restoring.
///
/// Without this the test above would pass on an implementation that restored
/// unconditionally, which would defeat the point of a cooldown — the primary
/// just told us it was rate limited.
#[test]
fn e2e_failover_stays_on_fallback_while_the_cooldown_holds() {
    let harness = TestHarness::new("e2e_failover_stays_on_fallback_while_the_cooldown_holds");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let (events, stdout, stderr) = run_print_json_two_prompts(
        &harness,
        &server,
        600,
        "two prompts, cooldown 600: the primary stays quiesced",
    );
    let kinds = event_kinds(&events);

    assert!(
        restoration_events(&events).is_empty(),
        "nothing may be restored while the cooldown holds: {kinds:?}\n{stdout}\n{stderr}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "failover_start").count(),
        1,
        "only the first prompt fails over; the second runs on the fallback it is still pinned to: \
         {kinds:?}\n{stdout}\n{stderr}"
    );

    let path = harness.temp_path("e2e_failover_pinned.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_pinned.jsonl", &path);
}

#[test]
fn e2e_failover_401_never_fails_over() {
    let harness = TestHarness::new("e2e_failover_401_never_fails_over");
    harness.log().info("setup", "primary 401, backup ready");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            401,
            r#"{"error":{"type":"authentication_error","message":"invalid api key"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::new(&harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    let child = command.spawn().expect("spawn pi");
    let (_stdout, stderr) = run_and_collect(child, 60);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });

    let backup_requests = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/backup/v1/chat/completions")
        .count();
    assert_eq!(
        backup_requests, 0,
        "auth errors must never fail over — backup must not be called"
    );
    let path = harness.temp_path("e2e_failover_401.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_401.jsonl", &path);
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_credential_rotation_swaps_key_on_429() {
    let harness = TestHarness::new("e2e_credential_rotation_swaps_key_on_429");
    harness
        .log()
        .info("setup", "openai override → mock, OPENAI_API_KEYS=k1,k2");
    let server = harness.start_mock_http_server();
    // Every chat call 429s once then succeeds: the FIRST key sees 429s, the
    // rotated key sees success. Use a queue: two 429s, then text. (The
    // built-in openai provider uses the Responses API for gpt-4o here.)
    server.add_route_queue(
        "POST",
        "/v1/responses",
        vec![
            error_response(
                429,
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
            error_response(
                429,
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
            sse_response(responses_sse_body("rotated ok")),
        ],
    );

    let root = harness.temp_path("pi-env-rotate");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    // Override the built-in openai provider's base_url at the mock so the
    // canonical OPENAI_API_KEYS plural var applies.
    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{"openai": {{"baseUrl": "{}/v1"}}}}}}"#,
            server.base_url()
        ),
    )
    .expect("write models.json");
    std::fs::write(
        root.join("settings.json"),
        r#"{"retry": {"enabled": true, "maxRetries": 2}, "checkForUpdates": false}"#,
    )
    .expect("write settings.json");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--print",
            "--no-session",
            "--provider",
            "openai",
            "--model",
            "gpt-4o",
            "ping",
        ])
        .env("HOME", root.join("home"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_CONFIG_PATH", root.join("settings.json"))
        .env("PI_SESSIONS_DIR", root.join("sessions"))
        .env("PI_PACKAGE_DIR", root.join("packages"))
        .env("PI_NO_AUTO_UPDATE_CHECK", "1")
        .env("OPENAI_API_KEYS", "k-aaa,k-bbb")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 90);

    let auth_headers: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/responses")
        .map(|r| {
            r.headers
                .iter()
                .rev()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        })
        .collect();
    harness
        .log()
        .info_ctx("verify", "auth headers observed", |ctx| {
            ctx.push(("auth_headers".to_string(), auth_headers.join(" | ")));
            ctx.push(("stdout".to_string(), stdout.clone()));
            ctx.push((
                "stderr_tail".to_string(),
                stderr.chars().take(300).collect(),
            ));
        });

    assert!(
        stdout.contains("rotated ok"),
        "rotation should complete; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        auth_headers.len() >= 2,
        "expected at least two attempts, got {auth_headers:?}"
    );
    assert_eq!(
        auth_headers[0],
        format!("Bearer {}", auth_headers[0].trim_start_matches("Bearer ")),
        "first request carries a bearer header"
    );
    let distinct: std::collections::HashSet<_> = auth_headers.iter().collect();
    assert!(
        distinct.len() >= 2 || stdout.contains("rotated ok"),
        "a 429 on the first key must rotate to the sibling key (headers: {auth_headers:?})"
    );
    assert!(
        auth_headers.iter().any(|h| h == "Bearer k-bbb"),
        "after the 429 on k-aaa, a retry must carry k-bbb: {auth_headers:?}"
    );

    let path = harness.temp_path("e2e_rotation_429.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_rotation_429.jsonl", &path);
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_path_scope_pins_repo_model_set() {
    let harness = TestHarness::new("e2e_path_scope_pins_repo_model_set");
    harness
        .log()
        .info("setup", "scope override pins repo A; repo B uses global");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/scoped/v1/chat/completions",
        sse_response(text_sse_body("scoped ok")),
    );
    server.add_route(
        "POST",
        "/global/v1/chat/completions",
        sse_response(text_sse_body("global ok")),
    );

    let root = harness.temp_path("pi-env-scope");
    let repo_a = harness.temp_path("repo-a");
    let repo_b = harness.temp_path("repo-b");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    std::fs::create_dir_all(&repo_a).expect("mkdir repo a");
    std::fs::create_dir_all(&repo_b).expect("mkdir repo b");

    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{
                "e2escoped": {{
                    "api": "openai-completions",
                    "baseUrl": "{}/scoped/v1",
                    "apiKey": "test-key",
                    "models": [{{"id": "scoped-model", "contextWindow": 128000}}]
                }},
                "e2eglobal": {{
                    "api": "openai-completions",
                    "baseUrl": "{}/global/v1",
                    "apiKey": "test-key",
                    "models": [{{"id": "global-model", "contextWindow": 128000}}]
                }}
            }}}}"#,
            server.base_url(),
            server.base_url()
        ),
    )
    .expect("write models.json");

    let settings = format!(
        r#"{{"enabledModels": ["e2eglobal/global-model"],
           "modelScopeOverrides": [{{"path": "{}", "enabledModels": ["e2escoped/scoped-model"]}}],
           "checkForUpdates": false}}"#,
        repo_a.display()
    );
    std::fs::write(root.join("settings.json"), settings).expect("write settings.json");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let run_in = |cwd: &std::path::Path| {
        let mut command = Command::new(&binary);
        command
            .args(["--print", "--no-session", "ping"])
            .current_dir(cwd)
            .env("HOME", root.join("home"))
            .env("PI_CODING_AGENT_DIR", root.join("agent"))
            .env("PI_CONFIG_PATH", root.join("settings.json"))
            .env("PI_SESSIONS_DIR", root.join("sessions"))
            .env("PI_PACKAGE_DIR", root.join("packages"))
            .env("PI_NO_AUTO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            command.env_remove(key);
        }
        let child = command.spawn().expect("spawn pi");
        run_and_collect(child, 90)
    };

    let (stdout_a, stderr_a) = run_in(&repo_a);
    let (stdout_b, stderr_b) = run_in(&repo_b);
    harness
        .log()
        .info_ctx("verify", "both runs finished", |ctx| {
            ctx.push(("stdout_a".to_string(), stdout_a.clone()));
            ctx.push(("stdout_b".to_string(), stdout_b.clone()));
            ctx.push((
                "stderr_a_tail".to_string(),
                stderr_a.chars().take(300).collect(),
            ));
            ctx.push((
                "stderr_b_tail".to_string(),
                stderr_b.chars().take(300).collect(),
            ));
        });

    assert!(
        stdout_a.contains("scoped ok"),
        "repo A must run the scoped model; stdout: {stdout_a}\nstderr: {stderr_a}"
    );
    assert!(
        stdout_b.contains("global ok"),
        "repo B must run the global default; stdout: {stdout_b}\nstderr: {stderr_b}"
    );

    let path = harness.temp_path("e2e_path_scope.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_path_scope.jsonl", &path);
}

/// bd-gm481.2: Restart durability for print mode.
/// A session that fails over in process 1 (with cooldown 0), when reopened
/// in a brand new process via `--session-dir` + `--continue`, restores the primary
/// on startup before running the prompt.
#[test]
fn e2e_failover_reopen_restores_primary_once_cooldown_elapsed() {
    let harness = TestHarness::new("e2e_failover_reopen_restores_primary_once_cooldown_elapsed");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::with_cooldown_secs(&harness, Some(0));
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let session_dir = env.root.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session_dir");

    // Process 1: Runs initial prompt "ping". Primary fails with 429, fails over to backup.
    let mut command1 = env.command(&binary);
    command1.args([
        "--print",
        "--mode",
        "json",
        "--session-dir",
        session_dir.to_str().unwrap(),
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    let child1 = command1.spawn().expect("spawn pi 1");
    let (stdout1, stderr1) = run_and_collect(child1, 120);
    let events1 = stdout1
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let kinds1 = event_kinds(&events1);
    assert_eq!(
        kinds1.iter().filter(|k| *k == "failover_start").count(),
        1,
        "process 1 must fail over once: {kinds1:?}\n{stdout1}\n{stderr1}"
    );

    // Process 2: Reopens the session via `--continue`. With cooldown 0 (elapsed),
    // it restores the primary, emits FailoverEnd{restoredPrimary: true}, and then runs "pong".
    // Since primary returns 429, "pong" fails over again!
    let mut command2 = env.command(&binary);
    command2.args([
        "--print",
        "--mode",
        "json",
        "--session-dir",
        session_dir.to_str().unwrap(),
        "--continue",
        "pong",
    ]);
    let child2 = command2.spawn().expect("spawn pi 2");
    let (stdout2, stderr2) = run_and_collect(child2, 120);
    let events2 = stdout2
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let kinds2 = event_kinds(&events2);

    let restorations = restoration_events(&events2);
    assert_eq!(
        restorations.len(),
        1,
        "process 2 must restore primary on reopen: {kinds2:?}\n{stdout2}\n{stderr2}"
    );
    let restoration = restorations[0];
    assert_eq!(restoration["provider"], "e2eprimary");
    assert_eq!(restoration["model"], "primary-model");
    assert_eq!(
        kinds2.iter().filter(|k| *k == "failover_start").count(),
        1,
        "process 2 started on restored primary which 429s, so it fails over again: {kinds2:?}\n{stdout2}\n{stderr2}"
    );
}

/// bd-gm481.2: Restart durability with active cooldown holding.
/// A session that fails over in process 1 with cooldown 600s, when reopened
/// in a brand new process via `--session-dir` + `--continue`, sees the cooldown
/// is still active, stays on the fallback, and does NOT restore the primary.
#[test]
fn e2e_failover_reopen_stays_on_fallback_while_cooldown_holds() {
    let harness = TestHarness::new("e2e_failover_reopen_stays_on_fallback_while_cooldown_holds");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::with_cooldown_secs(&harness, Some(600));
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let session_dir = env.root.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session_dir");

    // Process 1: Runs "ping", fails over to backup, records 600s cooldown deadline in session.
    let mut command1 = env.command(&binary);
    command1.args([
        "--print",
        "--mode",
        "json",
        "--session-dir",
        session_dir.to_str().unwrap(),
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    let child1 = command1.spawn().expect("spawn pi 1");
    let (stdout1, stderr1) = run_and_collect(child1, 120);
    let events1 = stdout1
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let kinds1 = event_kinds(&events1);
    assert_eq!(
        kinds1.iter().filter(|k| *k == "failover_start").count(),
        1,
        "process 1 must fail over once: {kinds1:?}\n{stdout1}\n{stderr1}"
    );

    // Process 2: Reopens the session via `--continue`. Cooldown is 600s, so it is still active.
    // It must NOT restore primary, staying on backup. "pong" succeeds directly on backup.
    let mut command2 = env.command(&binary);
    command2.args([
        "--print",
        "--mode",
        "json",
        "--session-dir",
        session_dir.to_str().unwrap(),
        "--continue",
        "pong",
    ]);
    let child2 = command2.spawn().expect("spawn pi 2");
    let (stdout2, stderr2) = run_and_collect(child2, 120);
    let events2 = stdout2
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let kinds2 = event_kinds(&events2);

    assert!(
        restoration_events(&events2).is_empty(),
        "nothing may be restored while cooldown holds across restart: {kinds2:?}\n{stdout2}\n{stderr2}"
    );
    assert_eq!(
        kinds2.iter().filter(|k| *k == "failover_start").count(),
        0,
        "process 2 stays on fallback, no new failover needed: {kinds2:?}\n{stdout2}\n{stderr2}"
    );
    assert!(
        stdout2.contains("backup ok"),
        "process 2 must successfully complete turn on fallback: {stdout2}\n{stderr2}"
    );
}
