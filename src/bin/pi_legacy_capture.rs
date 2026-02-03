//! Legacy pi-mono capture runner (bd-16n).
//!
//! Runs a small subset of deterministic scenarios against the pinned legacy
//! `pi-mono` implementation in print/json mode and records raw stdout/stderr plus a
//! metadata blob for later normalization + conformance comparisons.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, ValueEnum};
use pi::extensions::{LogComponent, LogCorrelation, LogLevel, LogPayload, LogSource};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
struct LegacyFixtureFile {
    schema: String,
    extension: ExtensionSampleItem,
    legacy: LegacyFixtureLegacy,
    capture: LegacyFixtureCapture,
    scenarios: Vec<LegacyFixtureScenario>,
}

#[derive(Debug, Serialize)]
struct LegacyFixtureLegacy {
    pi_mono_head: Option<String>,
    node_version: Option<String>,
    npm_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct LegacyFixtureCapture {
    provider: String,
    model: String,
    no_env: bool,
}

#[derive(Debug, Serialize)]
struct LegacyFixtureScenario {
    #[serde(flatten)]
    scenario: ScenarioSuiteScenario,
    outputs: LegacyFixtureOutputs,
}

#[derive(Debug, Serialize)]
struct LegacyFixtureOutputs {
    stdout_normalized_jsonl: Vec<String>,
    meta_normalized: Value,
    capture_log_normalized_jsonl: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(name = "pi_legacy_capture")]
#[command(about = "Run legacy pi-mono RPC scenarios and record raw outputs", long_about = None)]
struct Args {
    /// View a `pi.ext.log.v1` JSONL file and render a human-readable trace (use "-" for stdin).
    #[arg(long, value_name = "PATH")]
    view_log: Option<PathBuf>,

    /// Trace output format (pretty summary lines or raw JSONL).
    #[arg(long, value_enum, default_value_t = TraceViewMode::Pretty)]
    view_mode: TraceViewMode,

    /// Minimum log level to display when viewing traces.
    #[arg(long, value_enum, default_value_t = TraceViewLevel::Debug)]
    view_min_level: TraceViewLevel,

    /// Filter: only show these extension IDs (repeatable).
    #[arg(long)]
    view_extension_id: Vec<String>,

    /// Filter: only show these scenario IDs (repeatable).
    #[arg(long)]
    view_scenario_id: Vec<String>,

    /// Filter: only show events with these prefixes (repeatable).
    #[arg(long)]
    view_event_prefix: Vec<String>,

    /// Suppress the summary footer when viewing traces.
    #[arg(long, default_value_t = false)]
    view_no_summary: bool,

    /// Path to `docs/extension-sample.json`
    #[arg(long, default_value = "docs/extension-sample.json")]
    manifest: PathBuf,

    /// Path to pinned legacy `pi-mono/` repo root
    #[arg(long, default_value = "legacy_pi_mono_code/pi-mono")]
    pi_mono_root: PathBuf,

    /// Output directory for capture artifacts (defaults to target/ for git-ignore)
    #[arg(long, default_value = "target/legacy_capture")]
    out_dir: PathBuf,

    /// Output directory for generated per-extension fixtures.
    #[arg(long, default_value = "tests/ext_conformance/fixtures")]
    fixtures_dir: PathBuf,

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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TraceViewMode {
    Pretty,
    Jsonl,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TraceViewLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl TraceViewLevel {
    fn allows(self, level: &LogLevel) -> bool {
        level_rank(level) >= self.min_rank()
    }

    fn min_rank(self) -> u8 {
        match self {
            TraceViewLevel::Debug => 0,
            TraceViewLevel::Info => 1,
            TraceViewLevel::Warn => 2,
            TraceViewLevel::Error => 3,
        }
    }
}

fn level_rank(level: &LogLevel) -> u8 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
    }
}

fn level_label(level: &LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn run_trace_viewer(args: &Args, input: &Path) -> Result<()> {
    let mut reader: Box<dyn BufRead> = if input.as_os_str() == "-" {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        let file =
            File::open(input).with_context(|| format!("open trace log {}", input.display()))?;
        Box::new(BufReader::new(file))
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut total: usize = 0;
    let mut kept: usize = 0;
    let mut levels: BTreeMap<String, usize> = BTreeMap::new();
    let mut events: BTreeMap<String, usize> = BTreeMap::new();
    let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
    let mut scenarios: BTreeMap<String, usize> = BTreeMap::new();

    let mut line = String::new();
    let mut line_idx: usize = 0;
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .with_context(|| format!("read trace log line {}", line_idx + 1))?;
        if n == 0 {
            break;
        }
        line_idx += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total += 1;

        let payload: LogPayload = serde_json::from_str(trimmed)
            .with_context(|| format!("parse trace log JSON on line {}", line_idx))?;

        if !args.view_min_level.allows(&payload.level) {
            continue;
        }
        if !args.view_extension_id.is_empty()
            && !args
                .view_extension_id
                .iter()
                .any(|id| id == &payload.correlation.extension_id)
        {
            continue;
        }
        if !args.view_scenario_id.is_empty()
            && !args
                .view_scenario_id
                .iter()
                .any(|id| id == &payload.correlation.scenario_id)
        {
            continue;
        }
        if !args.view_event_prefix.is_empty()
            && !args
                .view_event_prefix
                .iter()
                .any(|prefix| payload.event.starts_with(prefix))
        {
            continue;
        }

        kept += 1;
        *levels
            .entry(level_label(&payload.level).to_string())
            .or_default() += 1;
        *events.entry(payload.event.clone()).or_default() += 1;
        *extensions
            .entry(payload.correlation.extension_id.clone())
            .or_default() += 1;
        *scenarios
            .entry(payload.correlation.scenario_id.clone())
            .or_default() += 1;

        match args.view_mode {
            TraceViewMode::Jsonl => {
                writeln!(out, "{trimmed}")?;
            }
            TraceViewMode::Pretty => {
                writeln!(out, "{}", format_trace_line(&payload))?;
            }
        }
    }

    if matches!(args.view_mode, TraceViewMode::Pretty) && !args.view_no_summary {
        writeln!(out)?;
        writeln!(out, "--- trace summary ---")?;
        writeln!(out, "lines: {kept} (of {total})")?;
        if !levels.is_empty() {
            writeln!(out, "levels: {}", render_count_map(&levels))?;
        }
        if !extensions.is_empty() {
            writeln!(out, "extensions: {}", render_count_map(&extensions))?;
        }
        if !scenarios.is_empty() {
            writeln!(out, "scenarios: {}", render_count_map(&scenarios))?;
        }
        if !events.is_empty() {
            writeln!(out, "events: {}", render_count_map(&events))?;
        }
    }

    Ok(())
}

fn render_count_map(map: &BTreeMap<String, usize>) -> String {
    let mut out = String::new();
    for (idx, (key, value)) in map.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(key);
        out.push('=');
        out.push_str(&value.to_string());
    }
    out
}

fn format_trace_line(payload: &LogPayload) -> String {
    let mut out = String::new();

    out.push_str(&payload.ts);
    out.push(' ');
    out.push_str(level_label(&payload.level));
    out.push(' ');
    out.push_str(&payload.event);
    if !payload.message.trim().is_empty() {
        out.push(' ');
        out.push_str(&payload.message);
    }

    out.push_str(" ext=");
    out.push_str(&payload.correlation.extension_id);
    out.push_str(" scn=");
    out.push_str(&payload.correlation.scenario_id);

    append_correlation_id(&mut out, "sess", payload.correlation.session_id.as_deref());
    append_correlation_id(&mut out, "run", payload.correlation.run_id.as_deref());
    append_correlation_id(&mut out, "art", payload.correlation.artifact_id.as_deref());
    append_correlation_id(
        &mut out,
        "tool",
        payload.correlation.tool_call_id.as_deref(),
    );
    append_correlation_id(
        &mut out,
        "cmd",
        payload.correlation.slash_command_id.as_deref(),
    );
    append_correlation_id(&mut out, "evt", payload.correlation.event_id.as_deref());
    append_correlation_id(
        &mut out,
        "host",
        payload.correlation.host_call_id.as_deref(),
    );
    append_correlation_id(&mut out, "rpc", payload.correlation.rpc_id.as_deref());
    append_correlation_id(&mut out, "trace", payload.correlation.trace_id.as_deref());
    append_correlation_id(&mut out, "span", payload.correlation.span_id.as_deref());

    if let Some(source) = &payload.source {
        out.push_str(" src=");
        out.push_str(match source.component {
            LogComponent::Capture => "capture",
            LogComponent::Harness => "harness",
            LogComponent::Runtime => "runtime",
            LogComponent::Extension => "extension",
        });
        if let Some(pid) = source.pid {
            out.push_str(" pid=");
            out.push_str(&pid.to_string());
        }
        if let Some(host) = source.host.as_deref() {
            if !host.trim().is_empty() {
                out.push_str(" host=");
                out.push_str(host);
            }
        }
    }

    if let Some(data) = &payload.data {
        if let Ok(text) = serde_json::to_string(data) {
            let truncated = truncate_chars(&text, 200);
            out.push_str(" data=");
            out.push_str(&truncated);
        }
    }

    out
}

fn append_correlation_id(out: &mut String, label: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    out.push(' ');
    out.push_str(label);
    out.push('=');
    out.push_str(value);
}

fn truncate_chars(input: &str, max_len: usize) -> String {
    if input.chars().count() <= max_len {
        return input.to_string();
    }
    let mut out = String::new();
    for ch in input.chars().take(max_len) {
        out.push(ch);
    }
    out.push('…');
    out
}

#[derive(Debug, Deserialize)]
struct ExtensionSampleManifest {
    items: Vec<ExtensionSampleItem>,
    scenario_suite: ScenarioSuite,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ExtensionSampleItem {
    id: String,
    source: ExtensionSource,
    #[serde(default)]
    checksum: Option<ExtensionChecksum>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ExtensionSource {
    commit: String,
    path: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ScenarioStep {
    EmitEvent {
        event_name: String,
        #[serde(default)]
        event: Value,
        #[serde(default)]
        ctx: Value,
    },
    InvokeTool {
        tool_name: String,
        arguments: Value,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ScenarioSuiteScenario {
    id: String,
    kind: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    event_name: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    command_name: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    setup: Option<Value>,
    #[serde(default)]
    steps: Vec<ScenarioStep>,
    #[serde(default)]
    expect: Option<Value>,
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

#[derive(Debug)]
struct MockOpenAiState {
    responses: Vec<Vec<u8>>,
    next_index: AtomicUsize,
    stop: AtomicBool,
}

#[derive(Debug)]
struct MockOpenAiServer {
    base_url: String,
    state: Arc<MockOpenAiState>,
    join: Option<JoinHandle<()>>,
    listener_addr: Option<std::net::SocketAddr>,
}

impl MockOpenAiServer {
    fn start(responses: Vec<Vec<u8>>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind mock openai server")?;
        listener.set_nonblocking(true).context("set_nonblocking")?;
        let addr = listener.local_addr().context("listener.local_addr")?;

        let state = Arc::new(MockOpenAiState {
            responses,
            next_index: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });

        let thread_state = Arc::clone(&state);
        let join = std::thread::spawn(move || {
            loop {
                if thread_state.stop.load(Ordering::SeqCst) {
                    break;
                }

                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_openai_connection(stream, &thread_state);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url: format!("http://{addr}/v1"),
            state,
            join: Some(join),
            listener_addr: Some(addr),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(addr) = self.listener_addr.take() {
            // Best-effort: connect once to wake the accept loop.
            let _ = TcpStream::connect(addr);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

const OPENAI_DONE_EVENT: &[u8] = b"data: [DONE]\n\n";
const SEED_EPOCH_MS: u64 = 1_770_076_800_000;

fn handle_openai_connection(mut stream: TcpStream, state: &MockOpenAiState) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let (method, path, remaining_body) = read_http_request_head(&mut stream)?;
    if method != "POST" || path != "/v1/responses" {
        let body = b"not found\n";
        write_http_response(&mut stream, 404, "text/plain", body)?;
        return Ok(());
    }

    // Drain request body to keep clients happy before we close the socket.
    drain_http_body(&mut stream, remaining_body)?;

    let idx = state.next_index.fetch_add(1, Ordering::SeqCst);
    let body = state
        .responses
        .get(idx)
        .or_else(|| state.responses.last())
        .map_or(OPENAI_DONE_EVENT, Vec::as_slice);

    write_http_response(&mut stream, 200, "text/event-stream", body)?;
    Ok(())
}

fn read_http_request_head(stream: &mut TcpStream) -> Result<(String, String, usize)> {
    let mut buf = Vec::<u8>::new();
    let mut scratch = [0_u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);

    let header_end = loop {
        if Instant::now() > deadline {
            bail!("mock openai: timed out reading request headers");
        }

        let n = stream.read(&mut scratch).context("read request")?;
        if n == 0 {
            bail!("mock openai: connection closed while reading headers");
        }
        buf.extend_from_slice(&scratch[..n]);

        if let Some(end) = find_header_end(&buf) {
            break end;
        }

        if buf.len() > 128 * 1024 {
            bail!("mock openai: header too large");
        }
    };

    let head = std::str::from_utf8(&buf[..header_end]).context("utf8 headers")?;
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().unwrap_or(0);
        }
    }

    let already_read_body = buf.len().saturating_sub(header_end);
    let remaining_body = content_length.saturating_sub(already_read_body);
    Ok((method, path, remaining_body))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(pos + 4);
    }
    buf.windows(2).position(|w| w == b"\n\n").map(|pos| pos + 2)
}

fn drain_http_body(stream: &mut TcpStream, mut remaining: usize) -> Result<()> {
    let mut scratch = [0_u8; 4096];
    while remaining > 0 {
        let to_read = remaining.min(scratch.len());
        let n = stream.read(&mut scratch[..to_read]).context("read body")?;
        if n == 0 {
            break;
        }
        remaining = remaining.saturating_sub(n);
    }
    Ok(())
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        404 => "Not Found",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
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

fn ensure_models_json(agent_dir: &Path, base_url: &str, setup: Option<&Value>) -> Result<PathBuf> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create agent dir {}", agent_dir.display()))?;

    let path = agent_dir.join("models.json");
    let mut providers = serde_json::Map::new();

    // Provide a dummy OpenAI provider config so legacy pi-mono has at least one available model.
    // We point baseUrl at a local mock server for deterministic tool-call scenarios.
    providers.insert(
        "openai".to_string(),
        json!({
            "baseUrl": base_url,
            "apiKey": "DUMMY",
        }),
    );

    if let Some(mock_registry) = setup
        .and_then(|value| value.pointer("/mock_model_registry"))
        .and_then(Value::as_object)
    {
        for (provider, raw) in mock_registry {
            let Some(api_key) = raw.as_str() else {
                continue;
            };
            providers.insert(
                provider.clone(),
                json!({
                    "baseUrl": "https://example.invalid",
                    "apiKey": api_key,
                }),
            );
        }
    }

    let content = Value::Object({
        let mut root = serde_json::Map::new();
        root.insert("providers".to_string(), Value::Object(providers));
        root
    });
    let text = serde_json::to_string_pretty(&content)?;
    std::fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

struct PiMonoSpawnConfig<'a> {
    pi_mono_root: &'a Path,
    cwd: &'a Path,
    extension_path: &'a str,
    agent_dir: &'a Path,
    provider: &'a str,
    model: &'a str,
    no_env: bool,
    node_preload: Option<&'a Path>,
    extra_cli_args: &'a [String],
}

fn ensure_settings_json(agent_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create agent dir {}", agent_dir.display()))?;

    let path = agent_dir.join("settings.json");
    // Safety net: if a dangerous bash tool call ever slips past an extension gate,
    // the commandPrefix causes the shell to exit before running the command body.
    let content = json!({
        "shellCommandPrefix": "echo \"[pi_legacy_capture] bash disabled\"; exit 123"
    });
    let text = serde_json::to_string_pretty(&content)?;
    std::fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn spawn_pi_mono_print_json(config: &PiMonoSpawnConfig<'_>, messages: &[String]) -> Result<Child> {
    let pi_mono_root = config
        .pi_mono_root
        .canonicalize()
        .unwrap_or_else(|_| config.pi_mono_root.to_path_buf());

    let tsx_cli = pi_mono_root.join("node_modules/tsx/dist/cli.mjs");
    if !tsx_cli.is_file() {
        bail!("missing tsx runner: {}", tsx_cli.display());
    }

    let cli = pi_mono_root.join("packages/coding-agent/src/cli.ts");
    if !cli.is_file() {
        bail!("missing legacy coding-agent CLI: {}", cli.display());
    }

    let extension_abs = pi_mono_root.join(config.extension_path);
    if !extension_abs.exists() {
        bail!("missing extension source: {}", extension_abs.display());
    }

    let agent_dir = config
        .agent_dir
        .canonicalize()
        .unwrap_or_else(|_| config.agent_dir.to_path_buf());

    let mut cmd = Command::new("/usr/bin/node");
    cmd.current_dir(config.cwd)
        .arg(tsx_cli)
        .arg(cli)
        .arg("--print")
        .arg("--mode")
        .arg("json")
        .arg("--extension")
        .arg(extension_abs)
        .arg("--provider")
        .arg(config.provider)
        .arg("--model")
        .arg(config.model)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if config.no_env {
        // Mirror `pi-test.sh --no-env` by stripping known API key / cloud env vars.
        for var in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GROQ_API_KEY",
            "CEREBRAS_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "ZAI_API_KEY",
            "MISTRAL_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "AI_GATEWAY_API_KEY",
            "OPENCODE_API_KEY",
            "COPILOT_GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "AWS_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_BASE_URL",
            "AZURE_OPENAI_RESOURCE_NAME",
        ] {
            cmd.env_remove(var);
        }
    }
    cmd.args(config.extra_cli_args);
    cmd.args(messages);

    // Determinism: use UTC timestamps wherever possible.
    cmd.env("TZ", "UTC");
    if let Some(path) = reorder_path_for_system_node() {
        cmd.env("PATH", path);
    }
    cmd.env("PI_CODING_AGENT_DIR", agent_dir);
    if let Some(preload) = config.node_preload {
        let preload = preload
            .canonicalize()
            .unwrap_or_else(|_| preload.to_path_buf());
        let preload_opt = format!("--require {}", preload.display());
        let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
        let combined = if existing.trim().is_empty() {
            preload_opt
        } else {
            format!("{preload_opt} {existing}")
        };
        cmd.env("NODE_OPTIONS", combined);
    }

    let child = cmd.spawn().context("spawn pi-mono print/json")?;
    Ok(child)
}

fn spawn_pi_mono_rpc(config: &PiMonoSpawnConfig<'_>) -> Result<Child> {
    let pi_mono_root = config
        .pi_mono_root
        .canonicalize()
        .unwrap_or_else(|_| config.pi_mono_root.to_path_buf());

    let tsx_cli = pi_mono_root.join("node_modules/tsx/dist/cli.mjs");
    if !tsx_cli.is_file() {
        bail!("missing tsx runner: {}", tsx_cli.display());
    }

    let cli = pi_mono_root.join("packages/coding-agent/src/cli.ts");
    if !cli.is_file() {
        bail!("missing legacy coding-agent CLI: {}", cli.display());
    }

    let extension_abs = pi_mono_root.join(config.extension_path);
    if !extension_abs.exists() {
        bail!("missing extension source: {}", extension_abs.display());
    }

    let agent_dir = config
        .agent_dir
        .canonicalize()
        .unwrap_or_else(|_| config.agent_dir.to_path_buf());

    let mut cmd = Command::new("/usr/bin/node");
    cmd.current_dir(config.cwd)
        .arg(tsx_cli)
        .arg(cli)
        .arg("--mode")
        .arg("rpc")
        .arg("--extension")
        .arg(extension_abs)
        .arg("--provider")
        .arg(config.provider)
        .arg("--model")
        .arg(config.model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if config.no_env {
        // Mirror `pi-test.sh --no-env` by stripping known API key / cloud env vars.
        for var in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GROQ_API_KEY",
            "CEREBRAS_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "ZAI_API_KEY",
            "MISTRAL_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "AI_GATEWAY_API_KEY",
            "OPENCODE_API_KEY",
            "COPILOT_GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "AWS_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_BASE_URL",
            "AZURE_OPENAI_RESOURCE_NAME",
        ] {
            cmd.env_remove(var);
        }
    }
    cmd.args(config.extra_cli_args);

    cmd.env("TZ", "UTC");
    if let Some(path) = reorder_path_for_system_node() {
        cmd.env("PATH", path);
    }
    cmd.env("PI_CODING_AGENT_DIR", agent_dir);
    if let Some(preload) = config.node_preload {
        let preload = preload
            .canonicalize()
            .unwrap_or_else(|_| preload.to_path_buf());
        let preload_opt = format!("--require {}", preload.display());
        let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
        let combined = if existing.trim().is_empty() {
            preload_opt
        } else {
            format!("{preload_opt} {existing}")
        };
        cmd.env("NODE_OPTIONS", combined);
    }

    let child = cmd.spawn().context("spawn pi-mono rpc")?;
    Ok(child)
}

fn extract_bool(input: &Value, pointer: &str, default: bool) -> bool {
    input
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn scenario_has_ui(scenario: &ScenarioSuiteScenario) -> bool {
    if extract_bool(&scenario.input, "/ctx/has_ui", false) {
        return true;
    }
    scenario.steps.iter().any(|step| match step {
        ScenarioStep::EmitEvent { ctx, .. } => extract_bool(ctx, "/has_ui", false),
        ScenarioStep::InvokeTool { .. } => false,
    })
}

fn setup_cli_args(setup: Option<&Value>) -> Vec<String> {
    let Some(flags) = setup
        .and_then(|value| value.pointer("/flags"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    let mut keys = flags.keys().cloned().collect::<Vec<_>>();
    keys.sort();

    let mut out = Vec::new();
    for key in keys {
        let Some(value) = flags.get(&key) else {
            continue;
        };
        match value {
            Value::Bool(true) => {
                out.push(format!("--{key}"));
            }
            Value::Bool(false) | Value::Null => {}
            Value::String(s) => {
                out.push(format!("--{key}"));
                out.push(s.clone());
            }
            Value::Number(n) => {
                out.push(format!("--{key}"));
                out.push(n.to_string());
            }
            other => {
                out.push(format!("--{key}"));
                out.push(other.to_string());
            }
        }
    }
    out
}

fn build_sse_body(events: &[Value]) -> Result<Vec<u8>> {
    let mut out = String::new();
    for event in events {
        let json = serde_json::to_string(event)?;
        out.push_str("data: ");
        out.push_str(&json);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    Ok(out.into_bytes())
}

fn build_openai_tool_call_responses(
    model: &str,
    tool_name: &str,
    tool_input: &Value,
) -> Result<Vec<Vec<u8>>> {
    let call_id = "call_1";
    let item_id = "fc_1";
    let response_id = "resp_1";
    let arguments = serde_json::to_string(tool_input)?;
    let preface_text = format!("Calling tool {tool_name}.");
    let message_item = json!({
        "type": "message",
        "id": "msg_0",
        "role": "assistant",
        "status": "completed",
        "content": [{"type":"output_text","text": preface_text, "annotations": []}]
    });
    let tool_item_added = json!({
        "type": "function_call",
        "id": item_id,
        "call_id": call_id,
        "name": tool_name,
        "arguments": "",
        "status": "in_progress"
    });
    let tool_item_done = json!({
        "type": "function_call",
        "id": item_id,
        "call_id": call_id,
        "name": tool_name,
        "arguments": arguments,
        "status": "completed"
    });

    let first = build_sse_body(&[
        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":message_item}),
        json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":message_item}),
        json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":tool_item_added}),
        json!({"type":"response.function_call_arguments.done","sequence_number":4,"output_index":1,"item_id": item_id,"name": tool_name, "arguments": arguments}),
        json!({"type":"response.output_item.done","sequence_number":5,"output_index":1,"item":tool_item_done}),
        json!({"type":"response.completed","sequence_number":6,"response": {
            "id": response_id,
            "object": "response",
            "created_at": 0,
            "model": model,
            "status": "completed",
            "output": [message_item, tool_item_done],
            "output_text": preface_text,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "metadata": null,
            "parallel_tool_calls": false,
            "temperature": null,
            "tool_choice": "auto",
            "tools": [],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0, "input_tokens_details": {"cached_tokens": 0}},
            "service_tier": null
        }}),
    ])?;

    let text = "ok";
    let message_item = json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "status": "completed",
        "content": [{"type":"output_text","text": text, "annotations": []}]
    });
    let second = build_sse_body(&[
        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":message_item}),
        json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":message_item}),
        json!({"type":"response.completed","sequence_number":3,"response": {
            "id": "resp_2",
            "object": "response",
            "created_at": 0,
            "model": model,
            "status": "completed",
            "output": [message_item],
            "output_text": text,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "metadata": null,
            "parallel_tool_calls": false,
            "temperature": null,
            "tool_choice": "auto",
            "tools": [],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0, "input_tokens_details": {"cached_tokens": 0}},
            "service_tier": null
        }}),
    ])?;

    Ok(vec![first, second])
}

fn build_openai_text_responses(model: &str, text: &str) -> Result<Vec<Vec<u8>>> {
    let message_item = json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "status": "completed",
        "content": [{"type":"output_text","text": text, "annotations": []}]
    });

    let body = build_sse_body(&[
        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":message_item}),
        json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":message_item}),
        json!({"type":"response.completed","sequence_number":3,"response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "model": model,
            "status": "completed",
            "output": [message_item],
            "output_text": text,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "metadata": null,
            "parallel_tool_calls": false,
            "temperature": null,
            "tool_choice": "auto",
            "tools": [],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0, "input_tokens_details": {"cached_tokens": 0}},
            "service_tier": null
        }}),
    ])?;

    Ok(vec![body])
}

fn run_pi_mono_to_completion(
    mut child: Child,
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    timeout: Duration,
) -> Result<ExitStatus> {
    let start = Instant::now();
    let mut exit_status: Option<ExitStatus> = None;

    loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out waiting for legacy pi-mono to finish");
        }

        if exit_status.is_none() {
            exit_status = child.try_wait().context("try_wait")?;
        }

        match stdout_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => writer.write_stdout_line(&line)?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(status) = exit_status {
            // Give stdout a brief moment to flush after the process exits.
            let drain_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < drain_deadline {
                match stdout_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => writer.write_stdout_line(&line)?,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            return Ok(status);
        }
    }

    exit_status.map_or_else(|| child.wait().context("wait"), Ok)
}

fn rpc_write_command(stdin: &mut std::process::ChildStdin, value: &Value) -> Result<()> {
    let text = serde_json::to_string(value)?;
    writeln!(stdin, "{text}")?;
    stdin.flush().ok();
    Ok(())
}

fn rpc_ui_response_value<'a>(scenario: &'a ScenarioSuiteScenario, key: &str) -> Option<&'a Value> {
    if let Some(value) = scenario
        .input
        .pointer("/ctx/ui_responses")
        .and_then(Value::as_object)
        .and_then(|map| map.get(key))
    {
        return Some(value);
    }

    for step in &scenario.steps {
        let ScenarioStep::EmitEvent { ctx, .. } = step else {
            continue;
        };
        if let Some(value) = ctx
            .pointer("/ui_responses")
            .and_then(Value::as_object)
            .and_then(|map| map.get(key))
        {
            return Some(value);
        }
    }

    None
}

fn rpc_handle_ui_request(
    value: &Value,
    scenario: &ScenarioSuiteScenario,
    stdin: &mut std::process::ChildStdin,
) -> Result<()> {
    if value.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
        return Ok(());
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Ok(());
    };

    match method {
        "select" => {
            let response = rpc_ui_response_value(scenario, "select")
                .and_then(Value::as_str)
                .map_or_else(
                    || json!({"type":"extension_ui_response","id": id, "cancelled": true}),
                    |choice| json!({"type":"extension_ui_response","id": id, "value": choice}),
                );
            rpc_write_command(stdin, &response)?;
        }
        "confirm" => {
            let response = rpc_ui_response_value(scenario, "confirm")
                .and_then(Value::as_bool)
                .map_or_else(
                    || json!({"type":"extension_ui_response","id": id, "cancelled": true}),
                    |confirmed| {
                        json!({"type":"extension_ui_response","id": id, "confirmed": confirmed})
                    },
                );
            rpc_write_command(stdin, &response)?;
        }
        "input" => {
            let response = rpc_ui_response_value(scenario, "input")
                .and_then(Value::as_str)
                .map_or_else(
                    || json!({"type":"extension_ui_response","id": id, "cancelled": true}),
                    |text| json!({"type":"extension_ui_response","id": id, "value": text}),
                );
            rpc_write_command(stdin, &response)?;
        }
        "editor" => {
            let response = rpc_ui_response_value(scenario, "editor")
                .and_then(Value::as_str)
                .map_or_else(
                    || json!({"type":"extension_ui_response","id": id, "cancelled": true}),
                    |text| json!({"type":"extension_ui_response","id": id, "value": text}),
                );
            rpc_write_command(stdin, &response)?;
        }
        _ => {}
    }
    Ok(())
}

fn rpc_recv_next(
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    scenario: &ScenarioSuiteScenario,
    stdin: &mut std::process::ChildStdin,
    timeout: Duration,
) -> Result<Option<Value>> {
    match stdout_rx.recv_timeout(timeout) {
        Ok(line) => {
            writer.write_stdout_line(&line)?;
            let value = serde_json::from_str::<Value>(&line).ok();
            if let Some(value) = value.as_ref() {
                rpc_handle_ui_request(value, scenario, stdin)?;
            }
            Ok(value)
        }
        Err(
            std::sync::mpsc::RecvTimeoutError::Timeout
            | std::sync::mpsc::RecvTimeoutError::Disconnected,
        ) => Ok(None),
    }
}

fn rpc_wait_for_response_id(
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    scenario: &ScenarioSuiteScenario,
    stdin: &mut std::process::ChildStdin,
    id: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            bail!("timed out waiting for rpc response id={id}");
        }

        let Some(value) = rpc_recv_next(
            stdout_rx,
            writer,
            scenario,
            stdin,
            Duration::from_millis(50),
        )?
        else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response") {
            continue;
        }
        if value.get("id").and_then(Value::as_str) != Some(id) {
            continue;
        }
        return Ok(value);
    }
}

fn rpc_wait_for_idle(
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    scenario: &ScenarioSuiteScenario,
    stdin: &mut std::process::ChildStdin,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut attempt = 0_u32;
    loop {
        if Instant::now() > deadline {
            bail!("timed out waiting for rpc session idle");
        }

        attempt = attempt.saturating_add(1);
        let id = format!("state-{attempt}");
        let cmd = json!({"id": id, "type": "get_state"});
        rpc_write_command(stdin, &cmd)?;

        let response = rpc_wait_for_response_id(stdout_rx, writer, scenario, stdin, &id, timeout)?;
        let Some(data) = response.get("data").and_then(Value::as_object) else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let is_streaming = data
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_compacting = data
            .get("isCompacting")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let pending = data
            .get("pendingMessageCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !is_streaming && !is_compacting && pending == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_pi_mono_rpc_scenario(
    mut child: Child,
    stdout_rx: &Receiver<String>,
    writer: &mut CaptureWriter,
    _scenario: &ScenarioSuiteScenario,
    _timeout: Duration,
    run: impl FnOnce(&mut std::process::ChildStdin, &Receiver<String>, &mut CaptureWriter) -> Result<()>,
) -> Result<ExitStatus> {
    let mut stdin = child.stdin.take().context("take child stdin")?;
    let run_result = run(&mut stdin, stdout_rx, writer);
    // Always attempt to kill the process after the scripted interaction; rpc mode doesn't exit on its own.
    let _ = stdin.flush();
    drop(stdin);
    let _ = child.kill();
    let status = child.wait().context("wait rpc child")?;

    // Best-effort drain of remaining stdout lines after killing.
    let drain_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < drain_deadline {
        match stdout_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => writer.write_stdout_line(&line)?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    run_result?;
    Ok(status)
}

// ============================================================================
// Normalization (bd-1oz)
// ============================================================================

#[derive(Debug, Clone)]
struct NormalizationContext {
    project_root: String,
    pi_mono_root: String,
}

impl NormalizationContext {
    fn from_args(args: &Args) -> Self {
        let project_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| cwd.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."))
            .display()
            .to_string();
        let pi_mono_root = args
            .pi_mono_root
            .canonicalize()
            .unwrap_or_else(|_| args.pi_mono_root.clone())
            .display()
            .to_string();
        Self {
            project_root,
            pi_mono_root,
        }
    }
}

fn normalize_string(value: &str, ctx: &NormalizationContext) -> String {
    static RUN_ID_RE: OnceLock<Regex> = OnceLock::new();
    static UUID_RE: OnceLock<Regex> = OnceLock::new();
    static MOCK_OPENAI_BASE_RE: OnceLock<Regex> = OnceLock::new();

    let run_id_re =
        RUN_ID_RE.get_or_init(|| Regex::new(r"\brun-[0-9a-fA-F-]{36}\b").expect("run id regex"));
    let uuid_re = UUID_RE.get_or_init(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .expect("uuid regex")
    });
    let openai_base_re = MOCK_OPENAI_BASE_RE
        .get_or_init(|| Regex::new(r"http://127\.0\.0\.1:\d+/v1").expect("openai base url regex"));

    let mut out = value.to_string();

    // Replace the pinned legacy root first (it includes the project_root prefix).
    if !ctx.pi_mono_root.is_empty() {
        out = out.replace(&ctx.pi_mono_root, "<PI_MONO_ROOT>");
    }
    if !ctx.project_root.is_empty() {
        out = out.replace(&ctx.project_root, "<PROJECT_ROOT>");
    }

    out = run_id_re.replace_all(&out, "<RUN_ID>").into_owned();
    out = openai_base_re
        .replace_all(&out, "http://127.0.0.1:<PORT>/v1")
        .into_owned();
    out = uuid_re.replace_all(&out, "<UUID>").into_owned();
    out
}

fn normalize_json_value(value: &mut Value, key: Option<&str>, ctx: &NormalizationContext) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                normalize_json_value(v, Some(k.as_str()), ctx);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_json_value(item, None, ctx);
            }
        }
        Value::String(s) => {
            if matches!(
                key,
                Some("timestamp" | "started_at" | "finished_at" | "created_at" | "createdAt")
            ) {
                *s = "<TIMESTAMP>".to_string();
            } else if matches!(key, Some("cwd")) {
                *s = "<PI_MONO_ROOT>".to_string();
            } else {
                *s = normalize_string(s, ctx);
            }
        }
        Value::Number(_) => {
            if matches!(
                key,
                Some("timestamp" | "started_at" | "finished_at" | "created_at" | "createdAt")
            ) {
                *value = Value::Number(serde_json::Number::from(0));
            }
        }
        _ => {}
    }
}

fn normalize_text_line(line: &str, ctx: &NormalizationContext) -> String {
    static TOTAL_OUTPUT_LINES_RE: OnceLock<Regex> = OnceLock::new();
    let total_lines_re = TOTAL_OUTPUT_LINES_RE
        .get_or_init(|| Regex::new(r"^Total output lines: \d+$").expect("total lines regex"));

    if total_lines_re.is_match(line) {
        return "Total output lines: <N>".to_string();
    }
    normalize_string(line, ctx)
}

fn normalize_jsonl_file(input: &Path, output: &Path, ctx: &NormalizationContext) -> Result<()> {
    let reader =
        BufReader::new(File::open(input).with_context(|| format!("open {}", input.display()))?);
    let mut out = File::create(output).with_context(|| format!("create {}", output.display()))?;
    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", input.display()))?;
        if line.trim_start().starts_with('{') {
            if let Ok(mut value) = serde_json::from_str::<Value>(&line) {
                normalize_json_value(&mut value, None, ctx);
                let normalized = serde_json::to_string(&value)?;
                writeln!(out, "{normalized}")?;
                continue;
            }
        }
        writeln!(out, "{}", normalize_text_line(&line, ctx))?;
    }
    Ok(())
}

fn normalize_json_file(input: &Path, output: &Path, ctx: &NormalizationContext) -> Result<()> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let mut value = serde_json::from_slice::<Value>(&bytes)
        .with_context(|| format!("parse {}", input.display()))?;
    normalize_json_value(&mut value, None, ctx);
    let normalized = serde_json::to_string_pretty(&value)?;
    std::fs::write(output, format!("{normalized}\n"))
        .with_context(|| format!("write {}", output.display()))?;
    Ok(())
}

fn command_line_for_scenario(scenario: &ScenarioSuiteScenario) -> Option<String> {
    let name = scenario.command_name.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    let mut cmd = format!("/{name}");

    match scenario.input.get("args") {
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                cmd.push(' ');
                cmd.push_str(trimmed);
            }
        }
        Some(Value::Array(args)) => {
            let parts = args
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                cmd.push(' ');
                cmd.push_str(&parts.join(" "));
            }
        }
        _ => {}
    }

    Some(cmd)
}

fn scenario_requires_rpc(scenario: &ScenarioSuiteScenario) -> bool {
    if scenario_has_ui(scenario) {
        return true;
    }
    // Scenarios without an explicit UI flag may still need RPC to observe
    // additional structured data (e.g., get_commands, provider registry).
    if scenario.kind == "provider" {
        return true;
    }
    if scenario.kind == "event" && scenario.event_name.as_deref() == Some("resources_discover") {
        return true;
    }
    false
}

fn scenario_pre_messages(scenario: &ScenarioSuiteScenario) -> Vec<String> {
    let mut out = Vec::new();
    if scenario
        .setup
        .as_ref()
        .and_then(|s| s.pointer("/state/plan_mode_enabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        // Plan mode is toggled via /plan command in the plan-mode extension.
        out.push("/plan".to_string());
    }
    out
}

fn build_mock_openai_responses(
    model: &str,
    scenario: &ScenarioSuiteScenario,
) -> Result<Vec<Vec<u8>>> {
    match scenario.kind.as_str() {
        "tool" => {
            let Some(tool_name) = scenario.tool_name.as_deref() else {
                // Some scenarios (e.g., sandbox-001) are metadata-only; keep the server alive.
                return build_openai_text_responses(model, "ok");
            };
            let args = scenario
                .input
                .pointer("/arguments")
                .cloned()
                .unwrap_or(Value::Null);
            build_openai_tool_call_responses(model, tool_name, &args)
        }
        "command" | "provider" => build_openai_text_responses(model, "ok"),
        "event" => match scenario.event_name.as_deref().unwrap_or_default() {
            "tool_call" => {
                let tool_name = scenario
                    .input
                    .pointer("/event/toolName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if tool_name.is_empty() {
                    bail!("missing input.event.toolName for {}", scenario.id);
                }
                let args = scenario
                    .input
                    .pointer("/event/input")
                    .cloned()
                    .unwrap_or(Value::Null);
                build_openai_tool_call_responses(model, tool_name, &args)
            }
            "input" | "turn_start" | "resources_discover" => {
                build_openai_text_responses(model, "ok")
            }
            "session_start" => scenario
                .steps
                .iter()
                .find_map(|step| match step {
                    ScenarioStep::InvokeTool {
                        tool_name,
                        arguments,
                    } => Some((tool_name, arguments)),
                    ScenarioStep::EmitEvent { .. } => None,
                })
                .map_or_else(
                    || build_openai_text_responses(model, "ok"),
                    |step| build_openai_tool_call_responses(model, step.0, step.1),
                ),
            "session_before_fork" => {
                // For git-checkpoint, we need at least one tool execution so the extension's
                // tool_result handler can capture an entryId before turn_start snapshots.
                build_openai_tool_call_responses(model, "read", &json!({"path": "dummy.txt"}))
            }
            other => bail!("unsupported event_name {other} for {}", scenario.id),
        },
        other => bail!("unsupported scenario kind {other} for {}", scenario.id),
    }
}

fn maybe_write_seed_session_file(
    scenario_dir: &Path,
    cwd_for_header: &Path,
    scenario: &ScenarioSuiteScenario,
) -> Result<Option<PathBuf>> {
    let Some(setup) = scenario.setup.as_ref() else {
        return Ok(None);
    };

    let session_branch = setup.pointer("/session_branch").and_then(Value::as_array);
    let leaf_entry_id = setup
        .pointer("/session_leaf_entry/id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if session_branch.is_none() && leaf_entry_id.is_none() {
        return Ok(None);
    }

    let path = scenario_dir.join("seed_session.jsonl");
    let mut out = File::create(&path).with_context(|| format!("create {}", path.display()))?;

    write_seed_session_header(&mut out, cwd_for_header)?;

    if let Some(branch) = session_branch {
        write_seed_session_branch(&mut out, branch, leaf_entry_id)?;
    } else if let Some(id) = leaf_entry_id {
        write_seed_leaf_entry(&mut out, id)?;
    }

    Ok(Some(path))
}

fn write_seed_session_header(out: &mut File, cwd_for_header: &Path) -> Result<()> {
    let header = json!({
        "type": "session",
        "version": 3,
        "id": "seed-session",
        "timestamp": "2026-02-03T00:00:00.000Z",
        "cwd": cwd_for_header.display().to_string(),
    });
    writeln!(out, "{}", serde_json::to_string(&header)?)?;
    Ok(())
}

fn write_seed_leaf_entry(out: &mut File, leaf_entry_id: &str) -> Result<()> {
    let message_ts = SEED_EPOCH_MS.saturating_add(1);
    let entry = json!({
        "type": "message",
        "id": leaf_entry_id,
        "parentId": null,
        "timestamp": "2026-02-03T00:00:00.001Z",
        "message": { "role": "user", "content": "seed", "timestamp": message_ts }
    });
    writeln!(out, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

fn write_seed_session_branch(
    out: &mut File,
    branch: &[Value],
    leaf_entry_id: Option<&str>,
) -> Result<()> {
    let mut parent_id: Option<String> = None;
    let mut ts_ms = 1_u32;

    for (idx, raw_entry) in branch.iter().enumerate() {
        let entry_type = raw_entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        let id = raw_entry
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                if idx + 1 == branch.len() {
                    leaf_entry_id.map(ToString::to_string)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| format!("entry-{}", idx + 1));

        let mut entry = serde_json::Map::new();
        entry.insert("type".to_string(), Value::String(entry_type.to_string()));
        entry.insert("id".to_string(), Value::String(id.clone()));
        entry.insert(
            "parentId".to_string(),
            parent_id
                .as_ref()
                .map_or(Value::Null, |p| Value::String(p.clone())),
        );
        entry.insert(
            "timestamp".to_string(),
            Value::String(format!("2026-02-03T00:00:00.{ts_ms:03}Z")),
        );

        match entry_type {
            "message" => {
                let message_ts = SEED_EPOCH_MS.saturating_add(u64::from(ts_ms));
                let raw_message = raw_entry.get("message").cloned().unwrap_or(Value::Null);
                let message = normalize_seed_message(raw_message, message_ts, &id);
                entry.insert("message".to_string(), message);
            }
            "custom" => {
                if let Some(custom_type) = raw_entry.get("customType").cloned() {
                    entry.insert("customType".to_string(), custom_type);
                }
                if let Some(data) = raw_entry.get("data").cloned() {
                    entry.insert("data".to_string(), data);
                }
            }
            other => {
                if let Some(obj) = raw_entry.as_object() {
                    for (k, v) in obj {
                        if ["type", "id", "parentId", "timestamp"].contains(&k.as_str()) {
                            continue;
                        }
                        entry.insert(k.clone(), v.clone());
                    }
                }
                entry.insert(
                    "note".to_string(),
                    Value::String(format!("seeded entry type {other}")),
                );
            }
        }

        writeln!(out, "{}", serde_json::to_string(&Value::Object(entry))?)?;
        parent_id = Some(id);
        ts_ms = ts_ms.saturating_add(1);
    }

    Ok(())
}

fn normalize_seed_message(raw: Value, message_ts: u64, entry_id: &str) -> Value {
    let Value::Object(mut message) = raw else {
        return json!({
            "role": "user",
            "content": "seed",
            "timestamp": message_ts,
        });
    };

    let role = message
        .get("role")
        .and_then(Value::as_str)
        .map_or("user", str::trim);

    match role {
        "toolResult" => {
            if message.get("toolCallId").and_then(Value::as_str).is_none() {
                message.insert(
                    "toolCallId".to_string(),
                    Value::String(format!("seed-toolcall-{entry_id}")),
                );
            }
            if message.get("toolName").and_then(Value::as_str).is_none() {
                message.insert("toolName".to_string(), Value::String("unknown".to_string()));
            }
            if !message.get("content").is_some_and(Value::is_array) {
                message.insert("content".to_string(), Value::Array(Vec::new()));
            }
            if message.get("isError").and_then(Value::as_bool).is_none() {
                message.insert("isError".to_string(), Value::Bool(false));
            }
            if message.get("timestamp").and_then(Value::as_u64).is_none() {
                message.insert("timestamp".to_string(), json!(message_ts));
            }
        }
        "user" => {
            if message.get("content").is_none() {
                message.insert("content".to_string(), Value::String("seed".to_string()));
            }
            if message.get("timestamp").and_then(Value::as_u64).is_none() {
                message.insert("timestamp".to_string(), json!(message_ts));
            }
        }
        "assistant" => {
            // Best-effort stabilization; seeded assistants are not expected for our fixtures.
            if !message.get("content").is_some_and(Value::is_array) {
                message.insert("content".to_string(), Value::Array(Vec::new()));
            }
            if message.get("api").is_none() {
                message.insert(
                    "api".to_string(),
                    Value::String("openai-responses".to_string()),
                );
            }
            if message.get("provider").is_none() {
                message.insert("provider".to_string(), Value::String("openai".to_string()));
            }
            if message.get("model").is_none() {
                message.insert("model".to_string(), Value::String("seed".to_string()));
            }
            if message.get("usage").is_none() {
                message.insert(
                    "usage".to_string(),
                    json!({
                        "input": 0,
                        "output": 0,
                        "cacheRead": 0,
                        "cacheWrite": 0,
                        "totalTokens": 0,
                        "cost": {
                            "input": 0,
                            "output": 0,
                            "cacheRead": 0,
                            "cacheWrite": 0,
                            "total": 0,
                        }
                    }),
                );
            }
            if message.get("stopReason").is_none() {
                message.insert("stopReason".to_string(), Value::String("stop".to_string()));
            }
            if message.get("timestamp").and_then(Value::as_u64).is_none() {
                message.insert("timestamp".to_string(), json!(message_ts));
            }
        }
        _ => {
            if message.get("timestamp").and_then(Value::as_u64).is_none() {
                message.insert("timestamp".to_string(), json!(message_ts));
            }
        }
    }

    Value::Object(message)
}

fn maybe_write_node_preload(
    scenario_dir: &Path,
    scenario: &ScenarioSuiteScenario,
) -> Result<Option<PathBuf>> {
    let setup = scenario.setup.as_ref();
    let mock_exec = setup
        .and_then(|value| value.pointer("/mock_exec"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let wants_fetch_stub = setup
        .and_then(|value| value.pointer("/mock_http"))
        .is_some_and(|v| !v.is_null());

    if mock_exec.is_empty() && !wants_fetch_stub {
        return Ok(None);
    }

    let path = scenario_dir.join("node_preload.cjs");
    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XvU8AAAAASUVORK5CYII=";

    let exec_specs = Value::Array(mock_exec);
    let exec_json = serde_json::to_string(&exec_specs)?;

    let mut script = String::new();
    script.push_str("// Auto-generated by pi_legacy_capture\n");
    script.push_str("'use strict';\n");
    script.push_str("const child_process = require('node:child_process');\n");
    script.push_str("const { PassThrough } = require('node:stream');\n");
    script.push_str("const { EventEmitter } = require('node:events');\n");
    script.push_str("const origSpawn = child_process.spawn.bind(child_process);\n");
    script.push_str("const mockExec = ");
    script.push_str(&exec_json);
    script.push_str(";\n");
    script.push_str(
        r"
function argsEqual(expected, actual) {
  if (!Array.isArray(expected) || !Array.isArray(actual)) return false;
  if (expected.length !== actual.length) return false;
  for (let i = 0; i < expected.length; i++) {
    if (String(expected[i]) !== String(actual[i])) return false;
  }
  return true;
}

function makeMockProc(spec) {
  const proc = new EventEmitter();
  proc.stdout = new PassThrough();
  proc.stderr = new PassThrough();
  proc.killed = false;
  proc.kill = (_signal) => {
    proc.killed = true;
    queueMicrotask(() => proc.emit('close', spec.code ?? 0));
    return true;
  };
  queueMicrotask(() => {
    if (spec.stdout) proc.stdout.write(String(spec.stdout));
    if (spec.stderr) proc.stderr.write(String(spec.stderr));
    proc.stdout.end();
    proc.stderr.end();
    proc.emit('close', spec.code ?? 0);
  });
  return proc;
}

child_process.spawn = (command, args, options) => {
  for (const spec of mockExec) {
    if (spec && spec.command === command && argsEqual(spec.args, args)) {
      return makeMockProc(spec);
    }
  }
  return origSpawn(command, args, options);
};
",
    );

    if wants_fetch_stub {
        let chunk = json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            { "text": "stubbed image response" },
                            { "inlineData": { "mimeType": "image/png", "data": png_base64 } }
                        ]
                    }
                }]
            }
        });
        let chunk_json = serde_json::to_string(&chunk)?;
        script.push_str("const origFetch = globalThis.fetch;\n");
        script.push_str("if (typeof origFetch === 'function') {\n");
        script.push_str("  globalThis.fetch = async (url, init) => {\n");
        script.push_str("    const u = typeof url === 'string' ? url : (url && url.url) ? url.url : String(url);\n");
        script.push_str("    if (u.startsWith('https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:streamGenerateContent')) {\n");
        script.push_str("      const body = 'data: ");
        script.push_str(&chunk_json.replace('\\', "\\\\").replace('\'', "\\'"));
        script.push_str("\\n\\n';\n");
        script.push_str("      return new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });\n");
        script.push_str("    }\n");
        script.push_str("    return origFetch(url, init);\n");
        script.push_str("  };\n");
        script.push_str("}\n");
    }

    std::fs::write(&path, script).with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(input) = args.view_log.as_deref() {
        return run_trace_viewer(&args, input);
    }

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

    let mut targets: Vec<(ExtensionSampleItem, ScenarioSuiteScenario)> = Vec::new();
    for entry in manifest.scenario_suite.items {
        let Some(item) = by_id.get(&entry.extension_id) else {
            continue;
        };
        for scenario in entry.scenarios {
            if !args.scenario_id.is_empty() && !args.scenario_id.contains(&scenario.id) {
                continue;
            }
            targets.push((item.clone(), scenario));
        }
    }

    if targets.is_empty() {
        bail!("no supported scenarios matched selection");
    }

    let legacy_head = git_rev_parse_head(&args.pi_mono_root);
    let node = node_version();
    let npm = npm_version();

    let mut fixture_builders: BTreeMap<String, (ExtensionSampleItem, Vec<LegacyFixtureScenario>)> =
        BTreeMap::new();

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
            "scenario_kind": scenario.kind.clone(),
            "scenario_event_name": scenario.event_name.clone(),
            "scenario_tool_name": scenario.tool_name.clone(),
            "scenario_command_name": scenario.command_name.clone(),
            "scenario_provider_id": scenario.provider_id.clone(),
        }));
        writer.write_capture_log(&payload)?;

        let mock_responses = build_mock_openai_responses(&args.model, &scenario)?;
        let mock_server = MockOpenAiServer::start(mock_responses)?;

        let agent_dir = scenario_dir.join("agent");
        let workspace_dir = scenario_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir)
            .with_context(|| format!("create {}", workspace_dir.display()))?;
        let models_json_path =
            ensure_models_json(&agent_dir, mock_server.base_url(), scenario.setup.as_ref())?;
        let settings_json_path = ensure_settings_json(&agent_dir)?;

        let mut extra_cli_args = setup_cli_args(scenario.setup.as_ref());

        let session_path = maybe_write_seed_session_file(&scenario_dir, &workspace_dir, &scenario)?;
        if let Some(path) = session_path.as_ref() {
            let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
            extra_cli_args.push("--session".to_string());
            extra_cli_args.push(abs.display().to_string());
        }

        if scenario.kind == "event" && scenario.event_name.as_deref() == Some("session_before_fork")
        {
            let dummy = workspace_dir.join("dummy.txt");
            std::fs::write(&dummy, "hello\n")?;
        }

        let node_preload = maybe_write_node_preload(&scenario_dir, &scenario)?;

        let timeout = Duration::from_secs(args.timeout_secs);
        let spawn_config = PiMonoSpawnConfig {
            pi_mono_root: &args.pi_mono_root,
            cwd: &workspace_dir,
            extension_path: &item.source.path,
            agent_dir: &agent_dir,
            provider: &args.provider,
            model: &args.model,
            no_env: args.no_env,
            node_preload: node_preload.as_deref(),
            extra_cli_args: &extra_cli_args,
        };
        let exit_status = if scenario_requires_rpc(&scenario) {
            let mut child = spawn_pi_mono_rpc(&spawn_config)?;
            let stdout_pipe = child.stdout.take().context("take child stdout")?;
            let stderr_pipe = child.stderr.take().context("take child stderr")?;

            child_stderr_thread(stderr_pipe, writer.stderr.try_clone()?);
            let stdout_rx = child_stdout_thread(stdout_pipe);

            run_pi_mono_rpc_scenario(
                child,
                &stdout_rx,
                &mut writer,
                &scenario,
                timeout,
                |stdin, stdout_rx, writer| {
                    match scenario.kind.as_str() {
                        "provider" => {
                            let id = "cmd-1";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "get_available_models"}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                        }
                        "event"
                            if scenario.event_name.as_deref() == Some("session_before_fork")
                                && scenario.id == "scn-git-checkpoint-001" =>
                        {
                            // 1) Trigger a turn with a tool call so git-checkpoint records an entryId.
                            let id = "cmd-1";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "prompt", "message": "Checkpoint setup turn."}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            rpc_wait_for_idle(stdout_rx, writer, &scenario, stdin, timeout)?;

                            // 2) Pick a fork candidate by text match.
                            let id = "cmd-2";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "get_fork_messages"}),
                            )?;
                            let response = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            let messages = response
                                .get("data")
                                .and_then(|d| d.get("messages"))
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();

                            let mut chosen: Option<String> = None;
                            for msg in &messages {
                                let entry_id = msg
                                    .get("entryId")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let text =
                                    msg.get("text").and_then(Value::as_str).unwrap_or_default();
                                if text.contains("Calling tool") && !entry_id.is_empty() {
                                    chosen = Some(entry_id.to_string());
                                    break;
                                }
                            }
                            if chosen.is_none() {
                                chosen = messages
                                    .first()
                                    .and_then(|msg| msg.get("entryId").and_then(Value::as_str))
                                    .map(ToString::to_string);
                            }
                            let entry_id = chosen.ok_or_else(|| {
                                anyhow::anyhow!("no fork messages available for {}", scenario.id)
                            })?;

                            // 3) Trigger another turn_start so the extension snapshots code state.
                            let id = "cmd-3";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "prompt", "message": "Checkpoint snapshot turn."}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            rpc_wait_for_idle(stdout_rx, writer, &scenario, stdin, timeout)?;

                            // 4) Fork from that entryId (should trigger session_before_fork).
                            let id = "cmd-4";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "fork", "entryId": entry_id}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            rpc_wait_for_idle(stdout_rx, writer, &scenario, stdin, timeout)?;
                        }
                        "event" if scenario.event_name.as_deref() == Some("resources_discover") => {
                            let id = "cmd-1";
                            rpc_write_command(stdin, &json!({"id": id, "type": "get_commands"}))?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                        }
                        "command" => {
                            let cmd = command_line_for_scenario(&scenario).ok_or_else(|| {
                                anyhow::anyhow!("missing command_name for {}", scenario.id)
                            })?;
                            let id = "cmd-1";
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "prompt", "message": cmd}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            rpc_wait_for_idle(stdout_rx, writer, &scenario, stdin, timeout)?;
                        }
                        "event" if scenario.event_name.as_deref() == Some("session_start") => {
                            // session_start side effects happen at startup; no-op unless UI requires prompt.
                        }
                        _ => {
                            // Default: send a single prompt to trigger a turn.
                            let id = "cmd-1";
                            let message = scenario
                                .input
                                .pointer("/event/text")
                                .and_then(Value::as_str)
                                .unwrap_or("Run scenario.");
                            rpc_write_command(
                                stdin,
                                &json!({"id": id, "type": "prompt", "message": message}),
                            )?;
                            let _ = rpc_wait_for_response_id(
                                stdout_rx, writer, &scenario, stdin, id, timeout,
                            )?;
                            rpc_wait_for_idle(stdout_rx, writer, &scenario, stdin, timeout)?;
                        }
                    }
                    Ok(())
                },
            )?
        } else {
            let mut messages = scenario_pre_messages(&scenario);
            match scenario.kind.as_str() {
                "command" => {
                    if let Some(cmd) = command_line_for_scenario(&scenario) {
                        messages.push(cmd);
                    } else {
                        bail!("missing command_name for {}", scenario.id);
                    }
                }
                "event" if scenario.event_name.as_deref() == Some("input") => {
                    let text = scenario
                        .input
                        .pointer("/event/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        bail!("missing input.event.text for {}", scenario.id);
                    }
                    messages.push(text.to_string());
                }
                _ => {
                    messages.push(format!("Run scenario {}.", scenario.id));
                }
            }

            let mut child = spawn_pi_mono_print_json(&spawn_config, &messages)?;
            let stdout_pipe = child.stdout.take().context("take child stdout")?;
            let stderr_pipe = child.stderr.take().context("take child stderr")?;

            child_stderr_thread(stderr_pipe, writer.stderr.try_clone()?);
            let stdout_rx = child_stdout_thread(stdout_pipe);
            run_pi_mono_to_completion(child, &stdout_rx, &mut writer, timeout)?
        };

        let finished_at = now_rfc3339_millis_z();
        let meta_value = json!({
            "schema": "pi.legacy_capture.v1",
            "run_id": ids.run_id.clone(),
            "extension_id": item.id.clone(),
            "scenario_id": scenario.id.clone(),
            "scenario_kind": scenario.kind.clone(),
            "scenario_event_name": scenario.event_name.clone(),
            "started_at": started_at,
            "finished_at": finished_at,
            "agent_dir": agent_dir.display().to_string(),
            "models_json": models_json_path.display().to_string(),
            "settings_json": settings_json_path.display().to_string(),
            "seed_session": session_path.as_ref().map(|p| p.display().to_string()),
            "node_preload": node_preload.as_ref().map(|p| p.display().to_string()),
            "provider": args.provider.clone(),
            "model": args.model.clone(),
            "exit": {
                "success": exit_status.success(),
                "code": exit_status.code(),
            },
            "mock_openai": {
                "base_url": mock_server.base_url(),
            },
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
        });
        writer.write_meta_json(&meta_value)?;

        let mut end = log_payload(&ids, &item.id, &scenario.id);
        end.message = "capture.finish".to_string();
        writer.write_capture_log(&end)?;

        drop(writer);

        let norm_ctx = NormalizationContext::from_args(&args);
        normalize_jsonl_file(
            &scenario_dir.join("stdout.jsonl"),
            &scenario_dir.join("stdout.normalized.jsonl"),
            &norm_ctx,
        )?;
        normalize_json_file(
            &scenario_dir.join("meta.json"),
            &scenario_dir.join("meta.normalized.json"),
            &norm_ctx,
        )?;
        normalize_jsonl_file(
            &scenario_dir.join("capture.log.jsonl"),
            &scenario_dir.join("capture.normalized.log.jsonl"),
            &norm_ctx,
        )?;

        let stdout_text = std::fs::read_to_string(scenario_dir.join("stdout.normalized.jsonl"))
            .with_context(|| format!("read stdout.normalized.jsonl for {}", scenario.id))?;
        let capture_text =
            std::fs::read_to_string(scenario_dir.join("capture.normalized.log.jsonl"))
                .with_context(|| {
                    format!("read capture.normalized.log.jsonl for {}", scenario.id)
                })?;
        let meta_bytes = std::fs::read(scenario_dir.join("meta.normalized.json"))
            .with_context(|| format!("read meta.normalized.json for {}", scenario.id))?;
        let meta_normalized =
            serde_json::from_slice::<Value>(&meta_bytes).context("parse meta.normalized.json")?;

        let record = LegacyFixtureScenario {
            scenario,
            outputs: LegacyFixtureOutputs {
                stdout_normalized_jsonl: stdout_text.lines().map(ToString::to_string).collect(),
                meta_normalized,
                capture_log_normalized_jsonl: capture_text
                    .lines()
                    .map(ToString::to_string)
                    .collect(),
            },
        };

        fixture_builders
            .entry(item.id.clone())
            .or_insert_with(|| (item.clone(), Vec::new()))
            .1
            .push(record);
    }

    std::fs::create_dir_all(&args.fixtures_dir)
        .with_context(|| format!("create fixtures dir {}", args.fixtures_dir.display()))?;

    for (extension_id, (item, mut scenarios)) in fixture_builders {
        scenarios.sort_by(|a, b| a.scenario.id.cmp(&b.scenario.id));
        let fixture = LegacyFixtureFile {
            schema: "pi.ext.legacy_fixtures.v1".to_string(),
            extension: item,
            legacy: LegacyFixtureLegacy {
                pi_mono_head: legacy_head.clone(),
                node_version: node.clone(),
                npm_version: npm.clone(),
            },
            capture: LegacyFixtureCapture {
                provider: args.provider.clone(),
                model: args.model.clone(),
                no_env: args.no_env,
            },
            scenarios,
        };

        let json = serde_json::to_string_pretty(&fixture)?;
        let path = args.fixtures_dir.join(format!("{extension_id}.json"));
        std::fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_string_rewrites_run_ids_and_ports_and_paths() {
        let ctx = NormalizationContext {
            project_root: "/repo".to_string(),
            pi_mono_root: "/repo/legacy_pi_mono_code/pi-mono".to_string(),
        };

        let input = "run-123e4567-e89b-12d3-a456-426614174000 http://127.0.0.1:4887/v1 /repo/legacy_pi_mono_code/pi-mono";
        let out = normalize_string(input, &ctx);
        assert!(out.contains("<RUN_ID>"), "{out}");
        assert!(out.contains("http://127.0.0.1:<PORT>/v1"), "{out}");
        assert!(out.contains("<PI_MONO_ROOT>"), "{out}");
    }

    #[test]
    fn normalize_json_value_masks_timestamps_and_cwd() {
        let ctx = NormalizationContext {
            project_root: "/repo".to_string(),
            pi_mono_root: "/repo/legacy_pi_mono_code/pi-mono".to_string(),
        };

        let mut value = serde_json::json!({
            "type": "session",
            "id": "6f48c50c-eb30-407c-a207-78beef805fc5",
            "timestamp": "2026-02-03T09:34:26.827Z",
            "cwd": "/repo/legacy_pi_mono_code/pi-mono"
        });
        normalize_json_value(&mut value, None, &ctx);
        assert_eq!(
            value,
            serde_json::json!({
                "type": "session",
                "id": "<UUID>",
                "timestamp": "<TIMESTAMP>",
                "cwd": "<PI_MONO_ROOT>"
            })
        );
    }

    #[test]
    fn normalize_json_value_does_not_touch_tool_call_ids() {
        let ctx = NormalizationContext {
            project_root: "/repo".to_string(),
            pi_mono_root: "/repo/legacy_pi_mono_code/pi-mono".to_string(),
        };

        let mut value = serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": "call_1|fc_1",
            "timestamp": 1_770_111_266_912_u64
        });
        normalize_json_value(&mut value, None, &ctx);
        assert_eq!(
            value,
            serde_json::json!({
                "type": "tool_execution_start",
                "toolCallId": "call_1|fc_1",
                "timestamp": 0
            })
        );
    }
}
