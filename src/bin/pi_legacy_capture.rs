//! Legacy pi-mono capture runner (bd-3on).
//!
//! Runs a small subset of deterministic scenarios against the pinned legacy
//! `pi-mono` implementation in RPC mode and records raw stdout/stderr plus a
//! metadata blob for later normalization + conformance comparisons.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use pi::extensions::{LogComponent, LogCorrelation, LogLevel, LogPayload, LogSource};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(name = "pi_legacy_capture")]
#[command(about = "Run legacy pi-mono RPC scenarios and record raw outputs", long_about = None)]
struct Args {
    /// Path to `docs/extension-sample.json`
    #[arg(long, default_value = "docs/extension-sample.json")]
    manifest: PathBuf,

    /// Path to pinned legacy `pi-mono/` repo root
    #[arg(long, default_value = "legacy_pi_mono_code/pi-mono")]
    pi_mono_root: PathBuf,

    /// Output directory for capture artifacts (defaults to target/ for git-ignore)
    #[arg(long, default_value = "target/legacy_capture")]
    out_dir: PathBuf,

    /// Provider to select in legacy pi-mono (required for RPC mode even for slash-command-only scenarios)
    #[arg(long, default_value = "openai")]
    provider: String,

    /// Model ID to select in legacy pi-mono (required for RPC mode even for slash-command-only scenarios)
    #[arg(long, default_value = "gpt-4o-mini")]
    model: String,

    /// Run only these scenario IDs (repeatable). If omitted, runs all supported headless scenarios.
    #[arg(long)]
    scenario_id: Vec<String>,

    /// Timeout for each scenario run.
    #[arg(long, default_value_t = 20)]
    timeout_secs: u64,

    /// Use `pi-test.sh --no-env` (recommended for deterministic/offline scenarios).
    #[arg(long, default_value_t = true)]
    no_env: bool,
}

#[derive(Debug, Deserialize)]
struct ExtensionSampleManifest {
    items: Vec<ExtensionSampleItem>,
    scenario_suite: ScenarioSuite,
}

#[derive(Debug, Deserialize)]
struct ExtensionSampleItem {
    id: String,
    source: ExtensionSource,
    #[serde(default)]
    checksum: Option<ExtensionChecksum>,
}

#[derive(Debug, Deserialize)]
struct ExtensionSource {
    commit: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct ExtensionChecksum {
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct ScenarioSuite {
    schema: String,
    items: Vec<ScenarioSuiteItem>,
}

#[derive(Debug, Deserialize)]
struct ScenarioSuiteItem {
    extension_id: String,
    scenarios: Vec<ScenarioSuiteScenario>,
}

#[derive(Debug, Deserialize)]
struct ScenarioSuiteScenario {
    id: String,
    kind: String,
    #[serde(default)]
    command_name: Option<String>,
    #[serde(default)]
    event_name: Option<String>,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    setup: Option<Value>,
}

#[derive(Debug)]
struct CaptureRunIds {
    run_id: String,
    pid: Option<u32>,
}

struct CaptureWriter {
    stdout: File,
    stderr: File,
    meta: File,
    log: File,
}

impl CaptureWriter {
    fn write_stdout_line(&mut self, line: &str) -> Result<()> {
        writeln!(self.stdout, "{line}")?;
        Ok(())
    }

    fn write_meta_json(&mut self, value: &Value) -> Result<()> {
        let text = serde_json::to_string_pretty(value)?;
        writeln!(self.meta, "{text}")?;
        Ok(())
    }

    fn write_capture_log(&mut self, payload: &LogPayload) -> Result<()> {
        let line = serde_json::to_string(payload)?;
        writeln!(self.log, "{line}")?;
        Ok(())
    }
}

fn now_rfc3339_millis_z() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn capture_ids() -> CaptureRunIds {
    CaptureRunIds {
        run_id: format!("run-{}", uuid::Uuid::new_v4()),
        pid: Some(std::process::id()),
    }
}

fn log_payload(ids: &CaptureRunIds, extension_id: &str, scenario_id: &str) -> LogPayload {
    LogPayload {
        schema: "pi.ext.log.v1".to_string(),
        ts: now_rfc3339_millis_z(),
        level: LogLevel::Info,
        event: "capture".to_string(),
        message: String::new(),
        correlation: LogCorrelation {
            extension_id: extension_id.to_string(),
            scenario_id: scenario_id.to_string(),
            session_id: None,
            run_id: Some(ids.run_id.clone()),
            artifact_id: None,
            tool_call_id: None,
            slash_command_id: None,
            event_id: None,
            host_call_id: None,
            rpc_id: None,
            trace_id: None,
            span_id: None,
        },
        source: Some(LogSource {
            component: LogComponent::Capture,
            host: None,
            pid: ids.pid,
        }),
        data: None,
    }
}

fn child_stdout_thread(stdout: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn child_stderr_thread(stderr: impl std::io::Read + Send + 'static, mut writer: File) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let _ = writeln!(writer, "{line}");
                }
                Err(_) => break,
            }
        }
    });
}

fn run_cmd_capture_stdout(cmd: &mut Command) -> Option<String> {
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn git_rev_parse_head(repo: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-C", repo.to_string_lossy().as_ref(), "rev-parse", "HEAD"]);
    run_cmd_capture_stdout(&mut cmd)
}

fn node_version() -> Option<String> {
    let mut cmd = Command::new("/usr/bin/node");
    cmd.arg("-v");
    run_cmd_capture_stdout(&mut cmd)
}

fn npm_version() -> Option<String> {
    let mut cmd = Command::new("/usr/bin/npm");
    cmd.arg("--version");
    run_cmd_capture_stdout(&mut cmd)
}

fn reorder_path_for_system_node() -> Option<String> {
    let current = std::env::var("PATH").ok()?;
    let mut parts = Vec::<String>::new();

    for fixed in ["/usr/bin", "/bin"] {
        parts.push(fixed.to_string());
    }

    for entry in current.split(':') {
        let entry = entry.trim();
        if entry.is_empty() || entry == "/usr/bin" || entry == "/bin" {
            continue;
        }
        parts.push(entry.to_string());
    }

    Some(parts.join(":"))
}

fn ensure_models_json(agent_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create agent dir {}", agent_dir.display()))?;

    let path = agent_dir.join("models.json");
    if path.is_file() {
        return Ok(path);
    }

    let content = json!({
        "providers": {
            // Provide a dummy provider config so legacy pi-mono has at least one available model.
            // The capture runner does not trigger any LLM calls for supported headless scenarios.
            "openai": {
                "baseUrl": "https://api.openai.com/v1",
                "apiKey": "DUMMY"
            }
        }
    });
    let text = serde_json::to_string_pretty(&content)?;
    std::fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn spawn_pi_mono_rpc(
    pi_mono_root: &Path,
    extension_path: &str,
    agent_dir: &Path,
    provider: &str,
    model: &str,
    no_env: bool,
) -> Result<Child> {
    let pi_test = pi_mono_root.join("pi-test.sh");
    if !pi_test.is_file() {
        bail!("missing legacy runner: {}", pi_test.display());
    }

    let mut cmd = Command::new("./pi-test.sh");
    cmd.current_dir(pi_mono_root)
        .arg("--mode")
        .arg("rpc")
        .arg("--extension")
        .arg(extension_path)
        .arg("--provider")
        .arg(provider)
        .arg("--model")
        .arg(model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if no_env {
        cmd.arg("--no-env");
    }

    // Determinism: use UTC timestamps wherever possible.
    cmd.env("TZ", "UTC");
    if let Some(path) = reorder_path_for_system_node() {
        cmd.env("PATH", path);
    }
    cmd.env("PI_CODING_AGENT_DIR", agent_dir);

    let child = cmd.spawn().context("spawn pi-mono rpc")?;
    Ok(child)
}

fn send_json(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let line = serde_json::to_string(value)?;
    writeln!(stdin, "{line}")?;
    stdin.flush()?;
    Ok(())
}

fn extract_bool(input: &Value, pointer: &str, default: bool) -> bool {
    input
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn extract_string(input: &Value, pointer: &str) -> Option<String> {
    input
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn extract_command_args(input: &Value) -> String {
    let args = input.pointer("/args");
    match args {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn wait_for<F>(
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    mut on_value: F,
    timeout: Duration,
) -> Result<Value>
where
    F: FnMut(&Value) -> bool,
{
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            bail!("timed out waiting for legacy output");
        }

        match stdout_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                writer.write_stdout_line(&line)?;
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if on_value(&value) {
                        return Ok(value);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("legacy stdout closed unexpectedly"),
        }
    }
}

fn run_headless_command_scenario(
    stdin: &mut ChildStdin,
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    command_name: &str,
    input: &Value,
    timeout: Duration,
) -> Result<()> {
    let args = extract_command_args(input);
    let message = if args.is_empty() {
        format!("/{command_name}")
    } else {
        format!("/{command_name} {args}")
    };

    // Prompt triggers a turn; completion signaled by agent_end event.
    send_json(stdin, &json!({"id":"1","type":"prompt","message": message}))?;
    let _ = wait_for(
        stdout_rx,
        writer,
        |value| value.get("type").and_then(Value::as_str) == Some("agent_end"),
        timeout,
    )?;
    Ok(())
}

fn run_headless_bash_event_scenario(
    stdin: &mut ChildStdin,
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    input: &Value,
    timeout: Duration,
) -> Result<()> {
    let command = extract_string(input, "/event/input/command").unwrap_or_default();
    if command.is_empty() {
        bail!("missing event.input.command");
    }

    send_json(stdin, &json!({"id":"1","type":"bash","command": command}))?;
    let _ = wait_for(
        stdout_rx,
        writer,
        |value| {
            value.get("type").and_then(Value::as_str) == Some("response")
                && value.get("id").and_then(Value::as_str) == Some("1")
                && value.get("command").and_then(Value::as_str) == Some("bash")
        },
        timeout,
    )?;
    Ok(())
}

fn scenario_is_supported_headless(scenario: &ScenarioSuiteScenario) -> bool {
    let has_ui = extract_bool(&scenario.input, "/ctx/has_ui", false);
    if has_ui {
        return false;
    }

    match scenario.kind.as_str() {
        "command" => scenario.command_name.is_some(),
        "event" => {
            if scenario.event_name.as_deref() != Some("tool_call") {
                return false;
            }
            scenario
                .input
                .pointer("/event/toolName")
                .and_then(Value::as_str)
                == Some("bash")
        }
        _ => false,
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();
    let ids = capture_ids();

    let manifest_bytes = std::fs::read(&args.manifest)
        .with_context(|| format!("read manifest {}", args.manifest.display()))?;
    let manifest: ExtensionSampleManifest =
        serde_json::from_slice(&manifest_bytes).context("parse extension-sample manifest")?;

    if manifest.scenario_suite.schema != "pi.ext.scenario-suite.v1" {
        bail!(
            "unsupported scenario_suite schema: {}",
            manifest.scenario_suite.schema
        );
    }

    let mut by_id: HashMap<String, ExtensionSampleItem> = HashMap::new();
    for item in manifest.items {
        by_id.insert(item.id.clone(), item);
    }

    let mut targets = Vec::new();
    for entry in manifest.scenario_suite.items {
        let Some(item) = by_id.get(&entry.extension_id) else {
            continue;
        };
        for scenario in entry.scenarios {
            if !scenario_is_supported_headless(&scenario) {
                continue;
            }
            if !args.scenario_id.is_empty() && !args.scenario_id.contains(&scenario.id) {
                continue;
            }
            targets.push((item, scenario));
        }
    }

    if targets.is_empty() {
        bail!("no supported scenarios matched selection");
    }

    let legacy_head = git_rev_parse_head(&args.pi_mono_root);
    let node = node_version();
    let npm = npm_version();

    for (item, scenario) in targets {
        let started_at = now_rfc3339_millis_z();
        let scenario_dir = args.out_dir.join(&scenario.id).join(&ids.run_id);
        std::fs::create_dir_all(&scenario_dir)
            .with_context(|| format!("create {}", scenario_dir.display()))?;

        let stdout = File::create(scenario_dir.join("stdout.jsonl"))?;
        let stderr = File::create(scenario_dir.join("stderr.txt"))?;
        let meta = File::create(scenario_dir.join("meta.json"))?;
        let log = File::create(scenario_dir.join("capture.log.jsonl"))?;

        let mut writer = CaptureWriter {
            stdout,
            stderr,
            meta,
            log,
        };

        let mut payload = log_payload(&ids, &item.id, &scenario.id);
        payload.message = "capture.start".to_string();
        payload.data = Some(json!({
            "started_at": started_at,
            "pi_mono_root": args.pi_mono_root.display().to_string(),
            "extension_path": item.source.path.clone(),
            "manifest_commit": item.source.commit.clone(),
            "manifest_checksum_sha256": item.checksum.as_ref().map(|c| c.sha256.clone()),
            "legacy_head": legacy_head.clone(),
            "node_version": node.clone(),
            "npm_version": npm.clone(),
            "provider": args.provider.clone(),
            "model": args.model.clone(),
        }));
        writer.write_capture_log(&payload)?;

        let agent_dir = scenario_dir.join("agent");
        let models_json_path = ensure_models_json(&agent_dir)?;
        let mut child = spawn_pi_mono_rpc(
            &args.pi_mono_root,
            &item.source.path,
            &agent_dir,
            &args.provider,
            &args.model,
            args.no_env,
        )?;
        let mut stdin = child.stdin.take().context("take child stdin")?;
        let stdout_pipe = child.stdout.take().context("take child stdout")?;
        let stderr_pipe = child.stderr.take().context("take child stderr")?;

        // Stream stderr directly into stderr.txt.
        child_stderr_thread(stderr_pipe, writer.stderr.try_clone()?);
        let stdout_rx = child_stdout_thread(stdout_pipe);

        let timeout = Duration::from_secs(args.timeout_secs);

        // Apply setup preconditions we know how to model in RPC mode.
        if scenario.kind == "event"
            && scenario
                .setup
                .as_ref()
                .and_then(|s| s.pointer("/state/plan_mode_enabled"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            // best-effort: enable plan mode via /plan before running the event scenario.
            let _ = run_headless_command_scenario(
                &mut stdin,
                &stdout_rx,
                &mut writer,
                "plan",
                &json!({"args": ""}),
                timeout,
            );
        }

        match scenario.kind.as_str() {
            "command" => {
                let name = scenario.command_name.as_deref().unwrap_or_default();
                run_headless_command_scenario(
                    &mut stdin,
                    &stdout_rx,
                    &mut writer,
                    name,
                    &scenario.input,
                    timeout,
                )?;
            }
            "event" => {
                run_headless_bash_event_scenario(
                    &mut stdin,
                    &stdout_rx,
                    &mut writer,
                    &scenario.input,
                    timeout,
                )?;
            }
            other => bail!("unsupported scenario kind: {other}"),
        }

        // Always include a final get_state snapshot in capture output.
        send_json(&mut stdin, &json!({"id":"2","type":"get_state"}))?;
        let _ = wait_for(
            &stdout_rx,
            &mut writer,
            |value| {
                value.get("type").and_then(Value::as_str) == Some("response")
                    && value.get("id").and_then(Value::as_str) == Some("2")
                    && value.get("command").and_then(Value::as_str) == Some("get_state")
            },
            timeout,
        )?;

        let finished_at = now_rfc3339_millis_z();
        writer.write_meta_json(&json!({
            "schema": "pi.legacy_capture.v1",
            "run_id": ids.run_id.clone(),
            "extension_id": item.id.clone(),
            "scenario_id": scenario.id.clone(),
            "started_at": started_at,
            "finished_at": finished_at,
            "agent_dir": agent_dir.display().to_string(),
            "models_json": models_json_path.display().to_string(),
            "provider": args.provider.clone(),
            "model": args.model.clone(),
            "pi_mono": {
                "root": args.pi_mono_root.display().to_string(),
                "head": legacy_head.clone(),
                "extension_path": item.source.path.clone(),
                "manifest_commit": item.source.commit.clone(),
                "manifest_checksum_sha256": item.checksum.as_ref().map(|c| c.sha256.clone()),
            },
            "env": {
                "TZ": "UTC",
                "no_env": args.no_env,
            },
        }))?;

        // Best-effort teardown: kill the forever-running RPC process.
        let _ = child.kill();
        let _ = child.wait();

        let mut end = log_payload(&ids, &item.id, &scenario.id);
        end.message = "capture.finish".to_string();
        writer.write_capture_log(&end)?;
    }

    Ok(())
}
