//! E2E: extension observation-event parity across every agent-driving surface
//! (bd-82331).
//!
//! ## What this proves that the inventory test cannot
//!
//! `tests/extension_event_routing.rs` reads the source and checks that every
//! surface still makes the routing call. That catches a surface being added or
//! rewritten without it, and it caught nothing when `/new` and `/resume` built
//! replacement sessions with no runtime to dispatch onto — because the routing
//! call was right where the table said it should be, in a file that correctly
//! declares `routes: false`, and the session handed to it simply could not use
//! it. Only running a real extension through a real turn finds that class.
//!
//! So: one fixture extension, one scripted turn with a streaming assistant
//! message and a tool call, driven through every surface, and the event streams
//! compared.
//!
//! ## How the events are observed
//!
//! Through pi's own log, not through the extension. An extension cannot report
//! what it received in a way that works on all five surfaces: its `node:fs` is
//! a sandboxed VFS whose writes need not reach the host disk, and its stdout is
//! swallowed by both TUIs. `ExtensionManager::dispatch_event_value` and
//! `dispatch_event_batch` both emit `ext.event.start` at info, which is the
//! host's own record of what it handed to extensions — after the hook check, so
//! a logged line means an extension really was called. `TuiAwareLogWriter`
//! sends that to stderr normally and to `<global_dir>/logs/tui.log` while a TUI
//! owns the terminal, which is exactly the two collection points below.
//!
//! ## What is compared, and what deliberately is not
//!
//! Observation events reach extensions by two internal routes with independent
//! timing: coalescable kinds (`message_update`, `tool_execution_update`) go
//! through the in-flight/pending path, everything else through a batch drain.
//! Their interleaving is therefore not deterministic and asserting on it would
//! buy a flaky test in exchange for nothing. What is deterministic, and what
//! this asserts:
//!
//! - the ORDERED sequence of non-coalescable observation events, which share
//!   one buffer and preserve arrival order;
//! - the SET of observation kinds delivered, which is the actual subject of
//!   bd-82331 — a surface that routes nothing has an empty set;
//! - that the four lifecycle events arrive on every surface, since they take
//!   the in-loop route and must not depend on the surface at all.
//!
//! Coalesced event COUNTS are recorded in the artifact but not asserted equal:
//! dropping duplicates is what the coalescer is for, and how many survive
//! depends on scheduling.

mod common;

use common::TestHarness;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Model id the cassette is recorded against.
const VCR_MODEL: &str = "claude-sonnet-4-20250514";
/// Cassette name, shared by every surface: the same turn, five times.
const VCR_TEST_NAME: &str = "extension_event_parity";
/// The user turn. Content is irrelevant; the cassette decides what happens.
const PROMPT: &str = "Read the sample file and summarise it.";
/// File the scripted tool call reads.
const SAMPLE_FILE: &str = "parity-sample.txt";
const SAMPLE_CONTENT: &str = "parity sample content\n";
/// Tool call id used by the cassette.
const TOOL_CALL_ID: &str = "toolu_parity_0001";
/// Text the second scripted response streams back.
const FINAL_TEXT: &str = "Parity done.";

/// Observation events that are coalesced, so their counts are not comparable.
const COALESCABLE: &[&str] = &["message_update", "tool_execution_update"];
/// Observation events that share the batch buffer and so keep arrival order.
const ORDERED_OBSERVATION: &[&str] = &[
    "message_start",
    "message_end",
    "tool_execution_start",
    "tool_execution_end",
];
/// Dispatched in-loop by the agent, so every surface gets them for free. Their
/// presence is the control: a surface missing these has a different problem.
const LIFECYCLE: &[&str] = &["agent_start", "agent_end", "turn_start", "turn_end"];
/// Hooks that can CHANGE what the agent does, also dispatched in-loop.
///
/// Included because bd-82331 rests on the claim that these were never affected
/// — an extension that modifies behaviour works everywhere, one that observes
/// did not — and a claim load-bearing enough to scope a bug around is worth a
/// test. If one of these ever moves to the per-surface route, this is where it
/// shows up, rather than in a bug report about one stack behaving differently.
const BEHAVIOUR: &[&str] = &[
    "input",
    "before_agent_start",
    "context",
    "before_provider_request",
    "tool_call",
    "tool_result",
];

/// A fixture extension that subscribes to everything and does nothing.
///
/// Subscribing is the whole point: `dispatch_agent_event_lazy` returns before
/// serializing when no extension has a hook for an event, so an extension that
/// registers no handlers would make every surface look identical and empty.
/// The handlers are deliberately inert — this test is about delivery, not about
/// what an extension does with what it receives.
fn fixture_extension_source() -> String {
    let mut names: Vec<&str> = Vec::new();
    names.extend_from_slice(LIFECYCLE);
    names.extend_from_slice(BEHAVIOUR);
    names.extend_from_slice(ORDERED_OBSERVATION);
    names.extend_from_slice(COALESCABLE);
    let list = names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r"export default function (pi) {{
  const observed = async (_event, _ctx) => {{}};
  for (const name of [{list}]) {{
    pi.on(name, observed);
  }}
}}
"
    )
}

/// One surface under test.
struct Surface {
    /// Name used in the artifact and in failure messages.
    name: &'static str,
    /// The file that owns this surface's event routing, for the failure text.
    owner: &'static str,
    /// Whether driving it needs tmux, which not every host has.
    needs_tmux: bool,
}

const SURFACES: &[Surface] = &[
    Surface {
        name: "print-text",
        owner: "src/main.rs",
        needs_tmux: false,
    },
    Surface {
        name: "print-json",
        owner: "src/main.rs",
        needs_tmux: false,
    },
    Surface {
        name: "rpc",
        owner: "src/rpc.rs",
        needs_tmux: false,
    },
    Surface {
        name: "classic-tui",
        owner: "src/interactive/agent.rs",
        needs_tmux: true,
    },
    Surface {
        name: "ftui",
        owner: "src/sdk.rs (driven by src/interactive_ftui.rs)",
        needs_tmux: true,
    },
];

/// What one surface delivered.
#[derive(Debug, Clone)]
struct SurfaceRun {
    name: String,
    owner: String,
    /// `None` when the surface could not be driven here (no tmux). Recorded
    /// explicitly so a skipped surface can never read as a passing one.
    outcome: Option<SurfaceEvents>,
    skip_reason: Option<String>,
    /// Raw dispatch records, kept for the artifact so a failure is diagnosable
    /// without re-running.
    raw: Vec<String>,
    /// The tail of everything the surface logged. A surface that routed no
    /// observation events cannot explain itself through `raw`, which is empty
    /// by definition in exactly that case — this is what says whether the
    /// extension loaded at all.
    log_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SurfaceEvents {
    /// Every event name in dispatch order, including coalescable ones.
    all: Vec<String>,
    /// Non-coalescable observation events in order. The comparable sequence.
    ordered_observation: Vec<String>,
    /// Every observation kind seen at least once.
    kinds: BTreeSet<String>,
    /// Lifecycle kinds seen at least once.
    lifecycle: BTreeSet<String>,
    /// Behaviour-hook kinds seen at least once.
    behaviour: BTreeSet<String>,
}

impl SurfaceEvents {
    fn from_names(names: Vec<String>) -> Self {
        let ordered_observation = names
            .iter()
            .filter(|name| ORDERED_OBSERVATION.contains(&name.as_str()))
            .cloned()
            .collect();
        let kinds = names
            .iter()
            .filter(|name| {
                ORDERED_OBSERVATION.contains(&name.as_str()) || COALESCABLE.contains(&name.as_str())
            })
            .cloned()
            .collect();
        let lifecycle = names
            .iter()
            .filter(|name| LIFECYCLE.contains(&name.as_str()))
            .cloned()
            .collect();
        let behaviour = names
            .iter()
            .filter(|name| BEHAVIOUR.contains(&name.as_str()))
            .cloned()
            .collect();
        Self {
            all: names,
            ordered_observation,
            kinds,
            lifecycle,
            behaviour,
        }
    }

    fn count_of(&self, name: &str) -> usize {
        self.all.iter().filter(|seen| seen.as_str() == name).count()
    }
}

/// Strip ANSI SGR sequences from log output.
///
/// `tracing_subscriber`'s fmt layer colourises field names, so a record reaches
/// this test as `\x1b[3mevent_name\x1b[0m\x1b[2m=\x1b[0mmessage_start` — the
/// literal `event_name=` never appears, and a parser looking for it silently
/// finds nothing on every surface. That is exactly the failure this test is
/// built to report, arriving from the instrument rather than the subject, which
/// is the worst way for it to be wrong. Strip first, then parse.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // CSI: ESC [ ... final byte in @-~. Anything else: drop the ESC and
        // the single byte after it, which covers the short escapes.
        if chars.next() == Some('[') {
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// Pull `ext.event.start` records out of pi's log output.
///
/// The fmt layer is installed with `with_target(false)`, so a record reads
/// `2026-..Z  INFO Extension event dispatch start event="ext.event.start"
/// event_name=message_start timeout_ms=30000` once the colour is stripped.
/// Accept the value quoted or not: whether a field is quoted depends on how it
/// was recorded, which is not a thing this test should be pinned to.
fn parse_dispatched_events(log: &str) -> Vec<String> {
    log.lines()
        .map(strip_ansi)
        .filter(|line| line.contains("ext.event.start"))
        .filter_map(|line| {
            let rest = line.split("event_name=").nth(1)?;
            let value = rest
                .split_whitespace()
                .next()?
                .trim_matches('"')
                .trim_end_matches(',');
            (!value.is_empty()).then(|| value.to_string())
        })
        .collect()
}

/// The last `LOG_TAIL_LINES` lines of a surface's log output.
fn log_tail(log: &str) -> Vec<String> {
    const LOG_TAIL_LINES: usize = 60;
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..]
        .iter()
        .map(|line| strip_ansi(line))
        .collect()
}

fn pi_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

/// Write the two-interaction cassette every surface replays.
///
/// The recorded requests constrain method and URL only. VCR treats an absent
/// recorded body as "do not constrain the body", and playback walks
/// interactions with a monotonic cursor, so the two are handed out in order:
/// the tool call first, the final text second.
///
/// That is deliberate. Pinning the body pins the system prompt and the tool
/// schema list, and this test would then fail whenever an unrelated prompt line
/// changed — the kind of failure that gets a test deleted rather than fixed. A
/// first version constrained `model` and `stream` and still failed to match,
/// which is the argument: what this test needs to be deterministic is the shape
/// of the TURN, and the responses below fix that completely. Request-shape
/// fidelity is the provider suites' job, and they do it properly.
#[allow(clippy::too_many_lines)]
fn write_parity_cassette(dir: &Path, read_path: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create cassette dir");
    let cassette_path = dir.join(format!("{VCR_TEST_NAME}.json"));

    let sse = |event: &str, data: &Value| -> String {
        let payload = serde_json::to_string(data).expect("serialize sse payload");
        format!("event: {event}\ndata: {payload}\n\n")
    };
    let tool_args = serde_json::to_string(&json!({ "path": read_path })).expect("tool args");

    // Turn one: a streaming tool call. Produces message_start, message_update,
    // message_end, then tool_execution_start / tool_execution_end.
    let tool_call_response = json!({
        "status": 200,
        "headers": [["Content-Type", "text/event-stream"]],
        "body_chunks": [
            sse("message_start", &json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 42 } }
            })),
            sse("content_block_start", &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": TOOL_CALL_ID, "name": "read" }
            })),
            sse("content_block_delta", &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": tool_args }
            })),
            sse("content_block_stop", &json!({ "type": "content_block_stop", "index": 0 })),
            sse("message_delta", &json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 12 }
            })),
            sse("message_stop", &json!({ "type": "message_stop" })),
        ]
    });

    // Turn two: streaming text, in more than one delta so `message_update`
    // has something to coalesce and a surface that drops updates is visible.
    let text_response = json!({
        "status": 200,
        "headers": [["Content-Type", "text/event-stream"]],
        "body_chunks": [
            sse("message_start", &json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 64 } }
            })),
            sse("content_block_start", &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text" }
            })),
            sse("content_block_delta", &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Parity " }
            })),
            sse("content_block_delta", &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "done." }
            })),
            sse("content_block_stop", &json!({ "type": "content_block_stop", "index": 0 })),
            sse("message_delta", &json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 8 }
            })),
            sse("message_stop", &json!({ "type": "message_stop" })),
        ]
    });

    let cassette = json!({
        "version": "1.0",
        "test_name": VCR_TEST_NAME,
        "recorded_at": "1970-01-01T00:00:00Z",
        "interactions": [
            {
                "request": {
                    "method": "POST",
                    "url": "https://api.anthropic.com/v1/messages",
                    "headers": [],
                },
                "response": tool_call_response,
            },
            {
                "request": {
                    "method": "POST",
                    "url": "https://api.anthropic.com/v1/messages",
                    "headers": [],
                },
                "response": text_response,
            },
        ]
    });

    std::fs::write(
        &cassette_path,
        serde_json::to_vec_pretty(&cassette).expect("serialize cassette"),
    )
    .expect("write cassette");
    cassette_path
}

/// Arguments every surface shares. Only the surface selector differs.
///
/// Extensions are ON (that is the subject), and everything else that could
/// vary between runs is OFF: skills, prompt templates, themes and migrations
/// all add startup work and none of it changes event routing.
fn common_args(extension_path: &Path) -> Vec<String> {
    vec![
        "--provider".to_string(),
        "anthropic".to_string(),
        "--model".to_string(),
        VCR_MODEL.to_string(),
        "--tools".to_string(),
        "read".to_string(),
        "--extension".to_string(),
        extension_path.display().to_string(),
        "--no-skills".to_string(),
        "--no-prompt-templates".to_string(),
        "--no-themes".to_string(),
        "--no-migrations".to_string(),
        "--thinking".to_string(),
        "off".to_string(),
        "--system-prompt".to_string(),
        "pi extension event parity harness".to_string(),
    ]
}

/// Write the settings every surface runs under.
///
/// Two features that default to ON make their own provider calls around a turn,
/// and both had to be turned off for this measurement to mean anything:
/// automatic session titling, which summarises the first exchange into a
/// session name, and the advisor, which reviews each turn with a second model.
/// The symptom was the scripted turn completing and printing its text, then the
/// run dying on a request the cassette had no interaction left for.
///
/// Padding the cassette instead would have hidden it and then produced a worse
/// failure: those extra calls emit `message_*` events of their own, on whichever
/// surfaces run them, and this test would have reported a parity failure that
/// was really a difference in post-turn housekeeping. Turning them off keeps the
/// measurement about the turn.
fn write_hermetic_settings(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create settings dir");
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "advisor": { "enabled": false },
            "titling": { "autoTitle": false },
        }))
        .expect("serialize settings"),
    )
    .expect("write hermetic settings");
}

/// Environment every surface shares: isolation, VCR playback, and a log filter
/// narrow enough that the surface's own output cannot drown the records.
fn apply_common_env(command: &mut Command, agent_dir: &Path, cassette_dir: &Path) {
    let settings_path = agent_dir.join("settings.json");
    write_hermetic_settings(&settings_path);
    command
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("PI_CONFIG_PATH", &settings_path)
        .env("PI_SESSIONS_DIR", agent_dir.join("sessions"))
        .env("PI_PACKAGE_DIR", agent_dir.join("packages"))
        .env("PI_TEST_MODE", "1")
        .env("ANTHROPIC_API_KEY", "pi-parity-fixture-key")
        .env(pi::vcr::VCR_ENV_MODE, "playback")
        .env(pi::vcr::VCR_ENV_DIR, cassette_dir)
        .env("PI_VCR_TEST_NAME", VCR_TEST_NAME)
        // Dumps every request body VCR was asked to match, next to the
        // interactions it compared them against. Without it an unmatched
        // request is a sha256 and nothing else, which costs a full build cycle
        // to identify.
        .env("VCR_DEBUG_BODY_FILE", agent_dir.join("vcr-bodies.txt"))
        .env("RUST_LOG", "pi=info");
}

/// Drive `pi -p`, optionally in JSON output mode, and collect what it logged.
#[allow(clippy::too_many_arguments)]
fn run_print_surface(
    harness: &TestHarness,
    name: &str,
    owner: &str,
    workdir: &Path,
    agent_dir: &Path,
    cassette_dir: &Path,
    extension_path: &Path,
    json_output: bool,
) -> SurfaceRun {
    // Flags first, prompt last. `Cli::args` is `trailing_var_arg`, so every
    // token after the first positional is captured verbatim as another message
    // — putting the prompt first made pi run the turn, then try to run
    // "--provider" as a second prompt, with none of the flags applied. The
    // symptom was a third provider request the cassette had no interaction for,
    // and a default model and thinking budget in a body that was supposed to be
    // pinned by --model and --thinking.
    let mut command = Command::new(pi_binary()); // ubs:ignore false positive: Cargo provides the compiled test binary path.
    command.arg("-p").args(common_args(extension_path));
    if json_output {
        command.args(["--output-format", "json"]);
    }
    command.arg(PROMPT);
    apply_common_env(&mut command, agent_dir, cassette_dir);
    command.current_dir(workdir);

    let output = command.output().expect("run pi print surface");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    harness
        .log()
        .info_ctx("surface", "print surface finished", |ctx| {
            ctx.push(("surface".into(), name.to_string()));
            ctx.push(("status".into(), format!("{:?}", output.status.code())));
            ctx.push(("stdout_bytes".into(), stdout.len().to_string()));
        });
    assert!(
        output.status.success(),
        "{name} must complete the scripted turn.\nstdout:\n{stdout}\nstderr:\n{stderr}\n\
         VCR request bodies:\n{}",
        std::fs::read_to_string(agent_dir.join("vcr-bodies.txt"))
            .unwrap_or_else(|err| format!("(no VCR body dump: {err})"))
    );

    let names = parse_dispatched_events(&stderr);
    SurfaceRun {
        name: name.to_string(),
        owner: owner.to_string(),
        outcome: Some(SurfaceEvents::from_names(names)),
        skip_reason: None,
        raw: stderr
            .lines()
            .map(strip_ansi)
            .filter(|line| line.contains("ext.event.start"))
            .collect(),
        log_tail: log_tail(&stderr),
    }
}

/// Drive `pi --rpc` through one prompt request and collect what it logged.
fn run_rpc_surface(
    harness: &TestHarness,
    workdir: &Path,
    agent_dir: &Path,
    cassette_dir: &Path,
    extension_path: &Path,
) -> SurfaceRun {
    use std::io::{BufRead as _, BufReader, Write as _};

    let mut command = Command::new(pi_binary()); // ubs:ignore false positive: Cargo provides the compiled test binary path.
    command.arg("--rpc").args(common_args(extension_path));
    apply_common_env(&mut command, agent_dir, cassette_dir);
    command
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn pi --rpc");
    {
        let stdin = child.stdin.as_mut().expect("rpc stdin");
        let request = json!({
            "id": "parity-prompt",
            "type": "prompt",
            "message": PROMPT,
        });
        writeln!(stdin, "{request}").expect("write rpc prompt");
        stdin.flush().expect("flush rpc prompt");
    }

    // Read until the reply to our request arrives, then close stdin so the
    // process exits on EOF. Reading to EOF first would deadlock: the RPC loop
    // keeps stdin open waiting for more requests.
    let stdout = child.stdout.take().expect("rpc stdout");
    let mut reader = BufReader::new(stdout);
    let mut transcript = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).expect("read rpc stdout");
        if read == 0 {
            break;
        }
        transcript.push_str(&line);
        if serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
            .is_some_and(|id| id == "parity-prompt")
        {
            break;
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for pi --rpc");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    harness
        .log()
        .info_ctx("surface", "rpc surface finished", |ctx| {
            ctx.push(("status".into(), format!("{:?}", output.status.code())));
            ctx.push((
                "transcript_lines".into(),
                transcript.lines().count().to_string(),
            ));
        });
    assert!(
        !transcript.trim().is_empty(),
        "pi --rpc produced no response to the prompt request.\nstderr:\n{stderr}"
    );

    let names = parse_dispatched_events(&stderr);
    SurfaceRun {
        name: "rpc".to_string(),
        owner: "src/rpc.rs".to_string(),
        outcome: Some(SurfaceEvents::from_names(names)),
        skip_reason: None,
        raw: stderr
            .lines()
            .map(strip_ansi)
            .filter(|line| line.contains("ext.event.start"))
            .collect(),
        log_tail: log_tail(&stderr),
    }
}

/// Drive one of the two interactive stacks under tmux.
///
/// These are the surfaces that cannot be driven any other way: they own the
/// terminal, so the event records go to `<global_dir>/logs/tui.log` instead of
/// stderr, and the turn has to be typed. `None` here means tmux is missing,
/// which the caller records as an explicit skip rather than a pass.
#[cfg(unix)]
fn run_tmux_surface(name: &str, owner: &str, classic: bool) -> Option<SurfaceRun> {
    use common::tmux::TuiSession;
    use std::time::Duration;

    let mut session = TuiSession::new(&format!("extension_event_parity_{name}"))?;
    let workdir = session.harness.temp_dir().to_path_buf();
    std::fs::write(workdir.join(SAMPLE_FILE), SAMPLE_CONTENT).expect("write sample file");
    let extension_path = session.harness.create_file(
        "parity-extension.mjs",
        fixture_extension_source().as_bytes(),
    );
    let cassette_dir = session.harness.temp_path("vcr");
    write_parity_cassette(&cassette_dir, SAMPLE_FILE);

    // TuiSession points PI_CONFIG_PATH at a .toml; give it the same hermetic
    // JSON settings the other surfaces run under so the advisor stays off here
    // too and all five drive the identical turn.
    let settings_path = session.harness.temp_path("pi-settings.json");
    write_hermetic_settings(&settings_path);
    session.set_env("PI_CONFIG_PATH", &settings_path.display().to_string());
    session.set_env(pi::vcr::VCR_ENV_MODE, "playback");
    session.set_env(pi::vcr::VCR_ENV_DIR, &cassette_dir.display().to_string());
    session.set_env("PI_VCR_TEST_NAME", VCR_TEST_NAME);
    // TuiSession sets RUST_LOG=info by default, which would bury the records
    // under everything else pi logs at info during startup.
    session.set_env("RUST_LOG", "pi=info");

    let mut args: Vec<String> = Vec::new();
    if classic {
        args.push("--classic".to_string());
    }
    args.extend(common_args(&extension_path));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    session.launch(&arg_refs);

    // Both stacks print a banner; neither shares the other's wording.
    session.tmux.wait_for_pane_contains_any(
        &["Welcome to Pi!", "ftui preview stack"],
        Duration::from_secs(30),
    );
    session.send_text_and_wait("parity_turn", PROMPT, FINAL_TEXT, Duration::from_secs(60));
    session.exit_gracefully();
    session.write_artifacts();

    let log_path = workdir
        .join("env")
        .join("agent")
        .join("logs")
        .join("tui.log");
    let missing_log = format!(
        "{name} must write its tracing output to {}",
        log_path.display()
    );
    let log = std::fs::read_to_string(&log_path).expect(&missing_log);
    let names = parse_dispatched_events(&log);
    Some(SurfaceRun {
        name: name.to_string(),
        owner: owner.to_string(),
        outcome: Some(SurfaceEvents::from_names(names)),
        skip_reason: None,
        raw: log
            .lines()
            .map(strip_ansi)
            .filter(|line| line.contains("ext.event.start"))
            .collect(),
        log_tail: log_tail(&log),
    })
}

#[cfg(not(unix))]
fn run_tmux_surface(_name: &str, _owner: &str, _classic: bool) -> Option<SurfaceRun> {
    None
}

/// Write the per-surface JSONL artifact bd-82331 asks for.
///
/// One object per surface, carrying everything a reader needs to tell WHICH
/// surface diverged and WHERE without re-running: the surface name and the file
/// that owns its routing, the full ordered event list, the comparable
/// sub-sequence, per-kind counts, the raw log lines behind all of it, and an
/// explicit verdict line. A skipped surface says so in the same field rather
/// than being absent, because an absent row reads as a pass.
fn write_parity_artifact(
    harness: &TestHarness,
    runs: &[SurfaceRun],
    verdicts: &[(String, String)],
) {
    let mut lines = String::new();
    for run in runs {
        let verdict = verdicts
            .iter()
            .find(|(name, _)| *name == run.name)
            .map_or("UNKNOWN", |(_, verdict)| verdict.as_str());
        let record = run.outcome.as_ref().map_or_else(
            || {
                json!({
                    "schema": "pi.ext.event_parity.v1",
                    "surface": run.name,
                    "routing_owner": run.owner,
                    "verdict": verdict,
                    "skipped": true,
                    "skip_reason": run.skip_reason,
                })
            },
            |events| {
                let mut counts = serde_json::Map::new();
                for name in ORDERED_OBSERVATION
                    .iter()
                    .chain(COALESCABLE)
                    .chain(LIFECYCLE)
                    .chain(BEHAVIOUR)
                {
                    counts.insert((*name).to_string(), json!(events.count_of(name)));
                }
                json!({
                    "schema": "pi.ext.event_parity.v1",
                    "surface": run.name,
                    "routing_owner": run.owner,
                    "verdict": verdict,
                    "skipped": false,
                    "event_sequence": events.all,
                    "ordered_observation_sequence": events.ordered_observation,
                    "observation_kinds": events.kinds,
                    "lifecycle_kinds": events.lifecycle,
                    "behaviour_kinds": events.behaviour,
                    "counts_per_kind": counts,
                    "raw_dispatch_records": run.raw,
                    "log_tail": run.log_tail,
                })
            },
        );
        let _ = writeln!(
            lines,
            "{}",
            serde_json::to_string(&record).expect("serialize parity record")
        );
    }
    let path = harness.temp_path("extension_event_parity.jsonl");
    std::fs::write(&path, lines).expect("write parity artifact");
    harness.record_artifact("extension_event_parity.jsonl", &path);
}

/// Render the first divergence in full, both sides, so a failure is readable.
fn describe_divergence(baseline: &SurfaceRun, other: &SurfaceRun) -> String {
    let (Some(left), Some(right)) = (baseline.outcome.as_ref(), other.outcome.as_ref()) else {
        return String::new();
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  {} ({}) vs {} ({})",
        baseline.name, baseline.owner, other.name, other.owner
    );
    let index = left
        .ordered_observation
        .iter()
        .zip(&right.ordered_observation)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| {
            left.ordered_observation
                .len()
                .min(right.ordered_observation.len())
        });
    let _ = writeln!(out, "    first differing index: {index}");
    let _ = writeln!(out, "    {}: {:?}", baseline.name, left.ordered_observation);
    let _ = writeln!(out, "    {}: {:?}", other.name, right.ordered_observation);
    let _ = writeln!(out, "    {} kinds: {:?}", baseline.name, left.kinds);
    let _ = writeln!(out, "    {} kinds: {:?}", other.name, right.kinds);
    out
}

/// The result of comparing what every surface delivered.
#[derive(Debug, Default)]
struct ParityOutcome {
    /// Surfaces that loaded the extension and routed nothing to it.
    silent: Vec<String>,
    /// Rendered differences against the baseline surface.
    divergences: Vec<String>,
    /// Per-surface PASS / FAIL / SKIPPED, for the artifact.
    verdicts: Vec<(String, String)>,
}

impl ParityOutcome {
    const fn is_clean(&self) -> bool {
        self.silent.is_empty() && self.divergences.is_empty()
    }
}

/// Compare the driven surfaces against the first of them.
///
/// Pure, and separated from the test body on purpose: bd-82331 asks that
/// removing the routing from one surface fail this test *naming that surface*,
/// and the honest way to check that is to feed this function a surface with the
/// routing removed and read the message. `parity_failure_names_the_surface_*`
/// below do exactly that, so the detector is tested rather than assumed.
fn evaluate_parity(runs: &[SurfaceRun]) -> ParityOutcome {
    let mut outcome = ParityOutcome::default();
    let driven: Vec<&SurfaceRun> = runs.iter().filter(|run| run.outcome.is_some()).collect();
    let Some(baseline) = driven.first().copied() else {
        return outcome;
    };
    let baseline_events = baseline
        .outcome
        .as_ref()
        .expect("driven surface has events");

    for run in &driven {
        let events = run.outcome.as_ref().expect("driven surface has events");
        if events.kinds.is_empty() {
            outcome.silent.push(format!(
                "  {} ({}) delivered no observation events at all\n    last lines it logged:\n{}",
                run.name,
                run.owner,
                run.log_tail
                    .iter()
                    .map(|line| format!("      {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    for run in driven.iter().skip(1) {
        let events = run.outcome.as_ref().expect("driven surface has events");
        if events.ordered_observation == baseline_events.ordered_observation
            && events.kinds == baseline_events.kinds
        {
            outcome
                .verdicts
                .push((run.name.clone(), "PASS".to_string()));
        } else {
            outcome.divergences.push(describe_divergence(baseline, run));
            outcome
                .verdicts
                .push((run.name.clone(), "FAIL".to_string()));
        }
    }
    let baseline_verdict = if outcome.is_clean() {
        "PASS (baseline)"
    } else {
        "BASELINE"
    };
    outcome
        .verdicts
        .push((baseline.name.clone(), baseline_verdict.to_string()));
    for run in runs.iter().filter(|run| run.outcome.is_none()) {
        outcome
            .verdicts
            .push((run.name.clone(), "SKIPPED".to_string()));
    }
    outcome
}

#[test]
#[allow(clippy::too_many_lines)]
fn extension_observation_events_are_identical_on_every_surface() {
    let harness = TestHarness::new("extension_event_parity");
    harness.section("drive every surface through one identical turn");

    let workdir = harness.create_dir("workspace");
    std::fs::write(workdir.join(SAMPLE_FILE), SAMPLE_CONTENT).expect("write sample file");
    let extension_path = harness.create_file(
        "parity-extension.mjs",
        fixture_extension_source().as_bytes(),
    );
    harness.record_artifact("parity-extension.mjs", &extension_path);
    let cassette_dir = harness.temp_path("vcr");
    let cassette_path = write_parity_cassette(&cassette_dir, SAMPLE_FILE);
    harness.record_artifact("extension_event_parity.cassette.json", &cassette_path);

    let mut runs: Vec<SurfaceRun> = Vec::new();
    for surface in SURFACES {
        if surface.needs_tmux {
            let classic = surface.name == "classic-tui";
            runs.push(
                run_tmux_surface(surface.name, surface.owner, classic).unwrap_or_else(|| {
                    SurfaceRun {
                        name: surface.name.to_string(),
                        owner: surface.owner.to_string(),
                        outcome: None,
                        skip_reason: Some(
                            "tmux is not available on this host; this surface was not driven"
                                .to_string(),
                        ),
                        raw: Vec::new(),
                        log_tail: Vec::new(),
                    }
                }),
            );
            continue;
        }
        // Each surface gets its own agent dir so one surface's session files
        // and logs can never be read as another's.
        let agent_dir = harness.create_dir(format!("agent-{}", surface.name));
        let run = if surface.name == "rpc" {
            run_rpc_surface(
                &harness,
                &workdir,
                &agent_dir,
                &cassette_dir,
                &extension_path,
            )
        } else {
            run_print_surface(
                &harness,
                surface.name,
                surface.owner,
                &workdir,
                &agent_dir,
                &cassette_dir,
                &extension_path,
                surface.name == "print-json",
            )
        };
        runs.push(run);
    }

    let driven = runs.iter().filter(|run| run.outcome.is_some()).count();
    assert!(
        driven >= 3,
        "at least the three non-tmux surfaces must be driven; got {driven}"
    );

    let outcome = evaluate_parity(&runs);
    write_parity_artifact(&harness, &runs, &outcome.verdicts);

    assert!(
        outcome.silent.is_empty(),
        "a surface loaded an extension and routed no observation events to it — this is bd-82331 \
         happening again:\n{}\n\nThe per-surface artifact (extension_event_parity.jsonl) has the \
         full event list for every surface.",
        outcome.silent.join("\n")
    );
    assert!(
        outcome.divergences.is_empty(),
        "extension observation events differ between surfaces. Same extension, same scripted \
         turn, different delivery:\n\n{}\nThe per-surface artifact \
         (extension_event_parity.jsonl) has the full ordered event list, per-kind counts and the \
         raw dispatch records for every surface.",
        outcome.divergences.join("\n")
    );

    // The in-loop route must be surface-independent. If this fails while the
    // observation comparison passes, the two halves have drifted apart and the
    // split this whole design rests on is no longer true.
    let driven_runs: Vec<&SurfaceRun> = runs.iter().filter(|run| run.outcome.is_some()).collect();
    for run in &driven_runs {
        let events = run.outcome.as_ref().expect("driven surface has events");
        for name in LIFECYCLE {
            assert!(
                events.lifecycle.contains(*name),
                "{} ({}) never received the lifecycle event {name}, which is dispatched from \
                 inside the agent loop and cannot depend on the surface",
                run.name,
                run.owner
            );
        }
    }

    // Behaviour hooks are the other in-loop half. bd-82331 scoped itself around
    // these being unaffected; this is what makes that a checked claim rather
    // than a remembered one.
    let behaviour_baseline = driven_runs[0];
    let baseline_behaviour = &behaviour_baseline
        .outcome
        .as_ref()
        .expect("driven surface has events")
        .behaviour;
    for run in driven_runs.iter().skip(1) {
        let events = run.outcome.as_ref().expect("driven surface has events");
        assert_eq!(
            &events.behaviour, baseline_behaviour,
            "{} ({}) received a different set of in-loop behaviour hooks than {} ({}). These are \
             dispatched from inside the agent loop and must not vary by surface; a difference \
             here means one of them has moved onto the per-surface route, which is how the \
             observation half broke in the first place.",
            run.name, run.owner, behaviour_baseline.name, behaviour_baseline.owner
        );
    }
}

/// Build a surface that delivered a full, healthy turn.
fn healthy_surface(name: &str) -> SurfaceRun {
    SurfaceRun {
        name: name.to_string(),
        owner: format!("src/{name}.rs"),
        outcome: Some(SurfaceEvents::from_names(vec![
            "input".to_string(),
            "before_agent_start".to_string(),
            "agent_start".to_string(),
            "turn_start".to_string(),
            "context".to_string(),
            "before_provider_request".to_string(),
            "message_start".to_string(),
            "message_update".to_string(),
            "message_end".to_string(),
            "tool_call".to_string(),
            "tool_execution_start".to_string(),
            "tool_execution_end".to_string(),
            "tool_result".to_string(),
            "turn_end".to_string(),
            "agent_end".to_string(),
        ])),
        skip_reason: None,
        raw: Vec::new(),
        log_tail: Vec::new(),
    }
}

/// MUTATION SENSITIVITY (bd-82331): a surface whose routing was removed fails,
/// and the failure says which one.
///
/// This is the check the acceptance criterion asks somebody to actually run.
/// Removing `dispatch_agent_event_lazy` from a surface makes it deliver the
/// four lifecycle events and nothing else — that is precisely the shape of the
/// original bug — so that is what is simulated here.
#[test]
fn parity_failure_names_the_surface_that_stopped_routing() {
    let mut broken = healthy_surface("rpc");
    broken.outcome = Some(SurfaceEvents::from_names(vec![
        "agent_start".to_string(),
        "turn_start".to_string(),
        "turn_end".to_string(),
        "agent_end".to_string(),
    ]));

    let outcome = evaluate_parity(&[healthy_surface("main"), broken]);

    assert!(
        !outcome.is_clean(),
        "a surface that routes nothing must fail"
    );
    let report = format!(
        "{}{}",
        outcome.silent.join("\n"),
        outcome.divergences.join("\n")
    );
    assert!(
        report.contains("rpc"),
        "the failure must name the surface that stopped routing, so nobody has to bisect five \
         surfaces to find it; got:\n{report}"
    );
    assert!(
        outcome
            .verdicts
            .iter()
            .any(|(name, verdict)| name == "rpc" && verdict == "FAIL"),
        "the artifact verdict for the broken surface must be FAIL: {:?}",
        outcome.verdicts
    );
}

/// A surface that loses only part of the stream is caught too, with the index.
#[test]
fn parity_failure_names_the_surface_that_lost_part_of_the_stream() {
    let mut partial = healthy_surface("ftui");
    partial.outcome = Some(SurfaceEvents::from_names(vec![
        "agent_start".to_string(),
        "turn_start".to_string(),
        "message_start".to_string(),
        "message_update".to_string(),
        "message_end".to_string(),
        "turn_end".to_string(),
        "agent_end".to_string(),
    ]));

    let outcome = evaluate_parity(&[healthy_surface("main"), partial]);

    assert!(
        !outcome.is_clean(),
        "a surface missing tool events must fail"
    );
    let report = outcome.divergences.join("\n");
    assert!(report.contains("ftui"), "{report}");
    assert!(
        report.contains("first differing index"),
        "the failure must point at where the streams diverged: {report}"
    );
    assert!(
        report.contains("tool_execution_start"),
        "the failure must show the events that went missing: {report}"
    );
}

/// Identical surfaces agree, so the comparison is not trivially failing.
#[test]
fn parity_passes_when_every_surface_delivers_the_same_stream() {
    let outcome = evaluate_parity(&[
        healthy_surface("main"),
        healthy_surface("rpc"),
        healthy_surface("sdk"),
    ]);
    assert!(
        outcome.is_clean(),
        "identical surfaces must compare clean: silent={:?} divergences={:?}",
        outcome.silent,
        outcome.divergences
    );
    assert_eq!(outcome.verdicts.len(), 3);
}

/// A skipped surface is recorded as skipped, never silently dropped.
#[test]
fn a_surface_that_could_not_be_driven_is_recorded_as_skipped() {
    let skipped = SurfaceRun {
        name: "classic-tui".to_string(),
        owner: "src/interactive/agent.rs".to_string(),
        outcome: None,
        skip_reason: Some("tmux is not available on this host".to_string()),
        raw: Vec::new(),
        log_tail: Vec::new(),
    };
    let outcome = evaluate_parity(&[healthy_surface("main"), skipped]);
    assert!(outcome.is_clean(), "a skip is not a failure");
    assert!(
        outcome
            .verdicts
            .iter()
            .any(|(name, verdict)| name == "classic-tui" && verdict == "SKIPPED"),
        "a surface that was not driven must say so rather than be absent: {:?}",
        outcome.verdicts
    );
}

/// The log parser reads what the host actually writes, including a TUI log.
#[test]
fn dispatch_records_are_parsed_from_pi_log_output() {
    // The third line carries the ANSI field colouring pi actually emits.
    let log = concat!(
        "2026-09-11T04:00:00.000000Z  INFO Extension event dispatch start ",
        "event=\"ext.event.start\" event_name=message_start timeout_ms=30000\n",
        "2026-09-11T04:00:00.001000Z  INFO something else entirely\n",
        "\u{1b}[2m2026-09-11T04:00:00.002000Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ",
        "Extension event dispatch start \u{1b}[3mevent\u{1b}[0m\u{1b}[2m=\u{1b}[0m",
        "\"ext.event.start\" \u{1b}[3mevent_name\u{1b}[0m\u{1b}[2m=\u{1b}[0m",
        "tool_execution_start \u{1b}[3mtimeout_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m30000\n",
    );
    assert_eq!(
        parse_dispatched_events(log),
        vec![
            "message_start".to_string(),
            "tool_execution_start".to_string()
        ],
        "the parser must accept the field quoted or bare: which one tracing emits depends on how \
         the field was recorded, and this test should not be pinned to that"
    );
}
