//! Hub-style process supervision (bd-cv653.5.4).
//!
//! Long-running services, watchers, REPLs, and debuggers live here instead
//! of timeout-hacked `bash` calls. Every service spawns on a PTY (stdin
//! stays writable for `send`), output streams to a rolling artifact log plus
//! a bounded line ring, and readiness is *observed* — a `ready.log` regex
//! and/or a `ready.port` TCP accept must both pass within the timeout before
//! `start` returns.
//!
//! Lifecycle: session-scoped by default (killed at the main shutdown
//! chokepoint, same as background jobs); `detached: true` services survive
//! session exit and are re-discovered from the state file under the hub
//! artifact dir.
//!
//! The `hub` tool's `jobs` action group wraps the background-jobs registry
//! (bd-cv653.3.10); the `messaging` action group lands with the agent-hub
//! registry (bd-cv653.5.3).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for service descriptors (stable audit contract).
pub const SERVICE_SCHEMA: &str = "pi.hub.service.v1";

/// Default readiness budget when the caller passes none.
const DEFAULT_READY_TIMEOUT_SECS: u64 = 30;

/// Bounded line ring kept per service for `logs` cursors.
const RING_LINE_CAP: usize = 10_000;

/// Grace window between TERM and KILL on stop, mirroring the bash tool.
const TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Readiness gates for `start`. Both supplied gates MUST pass.
#[derive(Debug, Clone, Default)]
pub struct ReadySpec {
    /// Regex that must match the accumulated service output.
    pub log: Option<String>,
    /// TCP port on 127.0.0.1 that must accept a connection.
    pub port: Option<u16>,
    /// Overall readiness budget in seconds (default 30).
    pub timeout_secs: Option<u64>,
}

/// Retained launch spec (restart reuses it verbatim).
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub ready: Option<ReadySpec>,
    pub detached: bool,
}

/// Live service state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceStatus {
    Starting,
    Running,
    Exited,
    Killed,
    Failed,
}

impl ServiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }

    const fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// One ring entry: a completed output line with its cursor index.
struct Ring {
    lines: VecDeque<String>,
    next_index: u64,
    /// Partial line currently being assembled (not yet cursor-addressable).
    partial: String,
    cap: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap.min(256)),
            next_index: 0,
            partial: String::new(),
            cap,
        }
    }

    fn push_chunk(&mut self, chunk: &str) {
        self.partial.push_str(chunk);
        while let Some(pos) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=pos).collect();
            if self.lines.len() == self.cap {
                self.lines.pop_front();
            }
            self.lines
                .push_back(line.trim_end_matches(['\n', '\r']).to_string()); // ubs:ignore per-line ring push is the design
            self.next_index = self.next_index.saturating_add(1);
        }
    }

    /// Lines with index >= `since`, plus the current head cursor.
    fn since(&self, since: u64) -> (Vec<String>, u64) {
        let oldest = self.next_index.saturating_sub(self.lines.len() as u64);
        let skip = usize::try_from(since.saturating_sub(oldest)).unwrap_or(usize::MAX);
        let lines = self.lines.iter().skip(skip).cloned().collect();
        (lines, self.next_index)
    }
}

struct ServiceEntry {
    spec: LaunchSpec,
    status: ServiceStatus,
    pid: Option<u32>,
    started_ms: i64,
    exit_code: Option<i32>,
    log_path: PathBuf,
    ring: Arc<Mutex<Ring>>,
    /// PTY master (resize/future); the writer is taken once at spawn —
    /// portable-pty's `UnixMasterWriter::drop` sends `\n`+VEOF, so caching
    /// the writer is what keeps the child's stdin open across sends.
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

/// Serializable service descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub schema: String,
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: String,
    pub started_ms: i64,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub detached: bool,
    pub ready: bool,
}

impl ServiceSnapshot {
    fn from_entry(entry: &ServiceEntry) -> Self {
        Self {
            schema: SERVICE_SCHEMA.to_string(),
            name: entry.spec.name.clone(),
            command: format!("{} {}", entry.spec.program, entry.spec.args.join(" ")),
            cwd: entry.spec.cwd.display().to_string(),
            pid: entry.pid,
            status: entry.status.as_str().to_string(),
            started_ms: entry.started_ms,
            exit_code: entry.exit_code,
            log_path: entry.log_path.display().to_string(),
            detached: entry.spec.detached,
            ready: entry.status == ServiceStatus::Running,
        }
    }
}

/// One page of service log lines with the cursor for the next read.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub schema: String,
    pub name: String,
    pub lines: Vec<String>,
    /// Opaque cursor for the next `logs` call (returns newer lines only).
    pub cursor: u64,
    pub status: String,
}

#[derive(Default)]
struct ServiceRegistry {
    services: HashMap<String, ServiceEntry>,
}

fn registry() -> &'static Mutex<ServiceRegistry> {
    static REGISTRY: std::sync::LazyLock<Mutex<ServiceRegistry>> =
        std::sync::LazyLock::new(|| Mutex::new(ServiceRegistry::default()));
    &REGISTRY
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn hub_artifact_dir() -> PathBuf {
    crate::config::Config::global_dir()
        .join("tool-output-artifacts")
        .join("hub")
}

fn detached_state_path() -> PathBuf {
    hub_artifact_dir().join("detached-services.json")
}

fn registry_err() -> Error {
    Error::tool("hub", "hub registry poisoned".to_string())
}

/// Persist the detached-service roster (name, pid, log, spec) so a later
/// session can rediscover survivors.
fn persist_detached_state(reg: &ServiceRegistry) {
    #[derive(Serialize)]
    struct DetachedRecord {
        name: String,
        pid: Option<u32>,
        log_path: String,
        program: String,
        args: Vec<String>,
        cwd: String,
    }
    let records: Vec<DetachedRecord> = reg
        .services
        .values()
        .filter(|entry| entry.spec.detached && entry.status.live())
        .map(|entry| DetachedRecord {
            name: entry.spec.name.clone(),
            pid: entry.pid,
            log_path: entry.log_path.display().to_string(),
            program: entry.spec.program.clone(),
            args: entry.spec.args.clone(),
            cwd: entry.spec.cwd.display().to_string(),
        })
        .collect();
    let path = detached_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&records) {
        let _ = std::fs::write(path, json);
    }
}

/// Spawn a service and block until readiness is observed (or the budget
/// expires). Readiness MUST be observed — process creation alone is not
/// ready.
///
/// # Errors
/// `PI_HUB_NAME_TAKEN` for a duplicate live name; `PI_HUB_NOT_READY` when
/// the gates do not pass in time (the process is killed — no half-started
/// surprise daemons); tool errors for spawn failures.
#[allow(clippy::too_many_lines)]
#[allow(clippy::significant_drop_tightening)]
pub fn start(spec: &LaunchSpec) -> Result<ServiceSnapshot> {
    let name = spec.name.trim().to_string();
    if name.is_empty() {
        return Err(Error::validation(
            "hub start requires a non-empty service name".to_string(),
        ));
    }
    if !spec.cwd.exists() {
        return Err(Error::tool(
            "hub",
            format!("Working directory does not exist: {}", spec.cwd.display()),
        ));
    }

    {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        if let Some(existing) = reg.services.get(&name)
            && existing.status.live()
        {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NAME_TAKEN: a live service named '{name}' already exists (pid {:?}); \
                     stop it first or pick another name",
                    existing.pid
                ),
            ));
        }
        // Completed names may be reused: drop the settled entry.
        reg.services.remove(&name);
    }

    let log_dir = hub_artifact_dir();
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| Error::tool("hub", format!("Failed to create hub artifact dir: {e}")))?;
    let log_path = log_dir.join(format!("{name}.log"));

    let ring = Arc::new(Mutex::new(Ring::new(RING_LINE_CAP)));
    let (mut child, master) = spawn_pty(spec)?;
    let pid = child.process_id();
    let reader = master
        .try_clone_reader()
        .map_err(|e| Error::tool("hub", format!("Failed to clone PTY reader: {e}")))?;
    let writer = master
        .take_writer()
        .map_err(|e| Error::tool("hub", format!("Failed to open PTY writer: {e}")))?;

    {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.services.insert(
            name.clone(),
            ServiceEntry {
                spec: spec.clone(),
                status: ServiceStatus::Starting,
                pid,
                started_ms: now_ms(),
                exit_code: None,
                log_path: log_path.clone(),
                ring: Arc::clone(&ring),
                writer: Arc::new(Mutex::new(Some(writer))),
            },
        );
        persist_detached_state(&reg);
    }

    // Pump thread: PTY master reader → artifact file + line ring.
    let artifact = std::fs::File::create(&log_path)
        .map_err(|e| Error::tool("hub", format!("Failed to create service log: {e}")))?;
    let pump_ring = Arc::clone(&ring);
    std::thread::spawn(move || pump_service_stream(reader, artifact, &pump_ring));

    // Exit monitor: record final status + refresh the detached roster.
    let monitor_name = name.clone();
    let monitor_ring = Arc::clone(&ring);
    std::thread::spawn(move || {
        let code: i64 = child
            .wait()
            .map_or(-1, |status| i64::from(status.exit_code()));
        if let Ok(mut ring) = monitor_ring.lock() {
            // Flush any trailing partial line so cursors cover it.
            let trailing = std::mem::take(&mut ring.partial);
            if !trailing.is_empty() {
                ring.push_chunk(&format!("{trailing}\n"));
            }
        }
        if let Ok(mut reg) = registry().lock()
            && let Some(entry) = reg.services.get_mut(&monitor_name)
        {
            if entry.status.live() {
                entry.status = if code == 0 {
                    ServiceStatus::Exited
                } else {
                    ServiceStatus::Failed
                };
            }
            entry.exit_code = Some(i32::try_from(code).unwrap_or(-1));
            entry.pid = None;
            persist_detached_state(&reg);
        }
    });

    // Readiness gate: block until BOTH supplied gates pass.
    let ready = spec.ready.clone().unwrap_or_default();
    let has_gates = ready.log.is_some() || ready.port.is_some();
    let budget = Duration::from_secs(ready.timeout_secs.unwrap_or(DEFAULT_READY_TIMEOUT_SECS));
    let deadline = Instant::now() + budget;
    let log_regex = ready
        .log
        .as_deref()
        .map(|pattern| {
            regex::Regex::new(pattern)
                .map_err(|e| Error::validation(format!("Invalid ready.log regex '{pattern}': {e}")))
        })
        .transpose()?;

    // Cold error paths, hoisted out of the poll loop (ubs loop-allocation
    // heuristic).
    let not_ready_exited = |tail: &str| {
        Error::tool(
            "hub",
            format!(
                "PI_HUB_NOT_READY: service '{name}' exited before readiness was observed.\n\
                 Log tail:\n{tail}"
            ),
        )
    };
    let not_ready_timeout = |tail: &str, log_passed: bool, port_passed: bool| {
        Error::tool(
            "hub",
            format!(
                "PI_HUB_NOT_READY: service '{name}' failed readiness within {}s \
                 (log gate passed: {log_passed}, port gate passed: {port_passed}). \
                 The process was killed.\nLog tail:\n{tail}",
                budget.as_secs()
            ),
        )
    };

    loop {
        let log_passed = log_regex.as_ref().is_none_or(|re| {
            ring.lock().is_ok_and(|ring| {
                let (lines, _) = ring.since(0);
                let body = lines.join("\n") + "\n" + &ring.partial;
                re.is_match(&body)
            })
        });
        let port_passed = ready
            .port
            .is_none_or(|port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

        if log_passed && port_passed {
            let mut reg = registry().lock().map_err(|_| registry_err())?;
            if reg.services.contains_key(&name) {
                if let Some(entry) = reg.services.get_mut(&name) {
                    entry.status = ServiceStatus::Running;
                }
                persist_detached_state(&reg);
                let entry = &reg.services[&name]; // ubs:ignore key presence checked above
                return Ok(ServiceSnapshot::from_entry(entry));
            }
            return Err(Error::tool(
                "hub",
                "service vanished before ready".to_string(), // ubs:ignore cold error path
            ));
        }

        // Process died before becoming ready?
        let early_status = {
            let reg = registry().lock().map_err(|_| registry_err())?;
            reg.services.get(&name).map(|entry| entry.status)
        };
        if matches!(
            early_status,
            Some(ServiceStatus::Exited | ServiceStatus::Failed)
        ) {
            let tail = ring_tail(&ring, 20);
            return Err(not_ready_exited(&tail));
        }

        if !has_gates || Instant::now() >= deadline {
            if has_gates {
                // Readiness timeout: kill — no half-started daemons.
                let pid = {
                    let reg = registry().lock().map_err(|_| registry_err())?;
                    reg.services.get(&name).and_then(|entry| entry.pid)
                };
                crate::tools::kill_process_group_tree(pid);
                if let Ok(mut reg) = registry().lock()
                    && let Some(entry) = reg.services.get_mut(&name)
                {
                    entry.status = ServiceStatus::Killed;
                }
                let tail = ring_tail(&ring, 20);
                return Err(not_ready_timeout(&tail, log_passed, port_passed));
            }
            // No gates supplied: process creation is the readiness signal.
            let mut reg = registry().lock().map_err(|_| registry_err())?;
            if let Some(entry) = reg.services.get_mut(&name) {
                entry.status = ServiceStatus::Running;
                return Ok(ServiceSnapshot::from_entry(entry));
            }
            return Err(Error::tool("hub", "service vanished".to_string())); // ubs:ignore cold error path
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_pty(
    spec: &LaunchSpec,
) -> Result<(
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn portable_pty::MasterPty + Send>,
)> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::tool("hub", format!("Failed to allocate PTY: {e}")))?;

    let mut cmd = CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| Error::tool("hub", format!("Failed to spawn service: {e}")))?;
    drop(pair.slave);
    Ok((child, pair.master))
}

fn pump_service_stream<R: Read>(mut reader: R, mut artifact: std::fs::File, ring: &Mutex<Ring>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let data = &chunk[..n]; // ubs:ignore n bounded by read into chunk
                let _ = artifact.write_all(data);
                if let Ok(mut ring) = ring.lock() {
                    ring.push_chunk(&String::from_utf8_lossy(data));
                }
            }
        }
    }
}

fn ring_tail(ring: &Mutex<Ring>, count: usize) -> String {
    ring.lock().map_or_else(
        |_| "(log unavailable)".to_string(),
        |ring| {
            let (lines, _) = ring.since(0);
            let tail: Vec<String> = lines.iter().rev().take(count).rev().cloned().collect();
            if tail.is_empty() {
                "(no output)".to_string()
            } else {
                tail.join("\n")
            }
        },
    )
}

/// List every service this session, plus detached survivors from the state
/// file that this process has not adopted.
///
/// # Errors
/// Registry lock failure.
pub fn ps() -> Result<Vec<ServiceSnapshot>> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    Ok(reg
        .services
        .values()
        .map(ServiceSnapshot::from_entry)
        .collect())
}

/// Read service logs.
///
/// `since` returns lines newer than the cursor; `tail` returns the last N
/// lines; `grep` filters (substring, case-sensitive); `wait_ms` bounds how
/// long `logs` blocks waiting for new lines when `since` is supplied.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
#[allow(clippy::significant_drop_tightening)]
pub fn logs(
    name: &str,
    since: Option<u64>,
    tail: Option<usize>,
    grep: Option<&str>,
    wait_ms: u64,
) -> Result<LogPage> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(60_000));
    loop {
        {
            let reg = registry().lock().map_err(|_| registry_err())?;
            let Some(entry) = reg.services.get(name) else {
                return Err(Error::tool(
                    "hub",
                    format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
                ));
            };
            let ring = entry.ring.lock().map_err(|_| registry_err())?;
            let (mut lines, cursor) = ring.since(since.unwrap_or(0));
            if since.is_none()
                && let Some(count) = tail
            {
                lines = lines.iter().rev().take(count).rev().cloned().collect();
            }
            if let Some(needle) = grep {
                lines.retain(|line| line.contains(needle));
            }
            // Wait only makes sense when the caller is looking for something:
            // an incremental cursor or a grep filter. A bare snapshot read
            // returns immediately.
            let seeking = since.is_some() || grep.is_some();
            if !lines.is_empty() || !seeking || Instant::now() >= deadline {
                return Ok(LogPage {
                    schema: "pi.hub.logs.v1".to_string(), // ubs:ignore loop returns immediately after
                    name: name.to_string(), // ubs:ignore loop returns immediately after
                    lines,
                    cursor,
                    status: entry.status.as_str().to_string(), // ubs:ignore loop returns after
                });
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Send text to a running service's PTY stdin (`enter` appends CR).
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING` for invalid targets.
#[allow(clippy::significant_drop_tightening)]
pub fn send_text(name: &str, text: &str, enter: bool) -> Result<()> {
    write_to_master(name, |writer| {
        writer.write_all(text.as_bytes())?;
        if enter {
            writer.write_all(b"\r")?;
        }
        writer.flush()
    })
}

/// Send named keys: ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, LEFT,
/// RIGHT.
///
/// # Errors
/// Named validation error for unknown key names.
pub fn send_keys(name: &str, keys: &[String]) -> Result<()> {
    // Map every key up front so the write loop carries no allocation.
    let mapped: Vec<&[u8]> = keys
        .iter()
        .map(|key| key_bytes(key))
        .collect::<Result<_>>()?;
    for bytes in mapped {
        write_to_master(name, |writer| {
            writer.write_all(bytes)?;
            writer.flush()
        })?;
    }
    Ok(())
}

fn key_bytes(key: &str) -> Result<&'static [u8]> {
    match key.to_ascii_uppercase().as_str() {
        "ENTER" => Ok(b"\r"),
        "TAB" => Ok(b"\t"),
        "ESCAPE" => Ok(b"\x1b"),
        "CTRL_C" => Ok(b"\x03"),
        "CTRL_D" => Ok(b"\x04"),
        "UP" => Ok(b"\x1b[A"),
        "DOWN" => Ok(b"\x1b[B"),
        "RIGHT" => Ok(b"\x1b[C"),
        "LEFT" => Ok(b"\x1b[D"),
        other => Err(Error::validation(format!(
            "Unknown key '{other}'; expected ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, \
             UP, DOWN, LEFT, RIGHT"
        ))),
    }
}

#[allow(clippy::significant_drop_tightening)]
fn write_to_master(
    name: &str,
    write: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
) -> Result<()> {
    let writer = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {} — stdin is closed",
                    entry.status.as_str()
                ),
            ));
        }
        Arc::clone(&entry.writer)
    };
    let mut guard = writer.lock().map_err(|_| registry_err())?;
    let Some(writer) = guard.as_mut() else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NO_INPUT: service '{name}' has no writable PTY master"),
        ));
    };
    write(writer.as_mut())
        .map_err(|e| Error::tool("hub", format!("Failed to write to service stdin: {e}")))
}

/// Send a signal to the service's process tree.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`.
#[allow(clippy::significant_drop_tightening)]
pub fn send_signal(name: &str, signal: sysinfo::Signal) -> Result<()> {
    let pid = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {}",
                    entry.status.as_str()
                ),
            ));
        }
        entry.pid
    };
    let Some(pid) = pid else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NOT_RUNNING: service '{name}' has no live pid"),
        ));
    };
    signal_pid_tree(pid, signal);
    Ok(())
}

fn signal_pid_tree(pid: u32, signal: sysinfo::Signal) {
    let root = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut children: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (pid_key, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children.entry(parent).or_default().push(*pid_key);
        }
    }
    let mut stack = vec![root];
    let mut visited = std::collections::HashSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        if let Some(proc_) = sys.process(current) {
            let _ = proc_.kill_with(signal).unwrap_or_else(|| proc_.kill());
        }
        if let Some(kids) = children.get(&current) {
            stack.extend(kids.iter().copied());
        }
    }
}

/// Stop a service: TERM → grace → KILL with the full process-tree walk
/// (same discipline as the bash tool).
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`.
#[allow(clippy::significant_drop_tightening)]
pub fn stop(name: &str) -> Result<ServiceSnapshot> {
    let pid = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get_mut(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' already settled ({})",
                    entry.status.as_str()
                ),
            ));
        }
        entry.status = ServiceStatus::Killed;
        entry.pid
    };
    crate::tools::terminate_process_group_tree(pid);
    // The exit monitor records the settle; give it the grace window.
    std::thread::sleep(TERMINATE_GRACE.min(Duration::from_millis(500)));
    if let Some(pid) = pid {
        let still_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        if still_alive {
            crate::tools::kill_process_group_tree(Some(pid));
        }
    }
    describe(name)
}

/// Restart reuses the retained launch spec. Running services are stopped
/// first; completed names re-spawn directly.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names; start errors otherwise.
#[allow(clippy::significant_drop_tightening)]
pub fn restart(name: &str) -> Result<ServiceSnapshot> {
    let (spec, was_live) = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        (entry.spec.clone(), entry.status.live())
    };
    if was_live {
        let _ = stop(name);
    }
    start(&spec)
}

/// Full descriptor for one service.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
#[allow(clippy::significant_drop_tightening)]
pub fn describe(name: &str) -> Result<ServiceSnapshot> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    let Some(entry) = reg.services.get(name) else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
        ));
    };
    Ok(ServiceSnapshot::from_entry(entry))
}

/// Kill every non-detached service (session exit). Called once from the
/// main shutdown chokepoint next to `jobs::kill_all`.
pub fn kill_session_services() {
    let pids: Vec<(String, Option<u32>)> = {
        let Ok(mut reg) = registry().lock() else {
            return;
        };
        let victims: Vec<(String, Option<u32>)> = reg
            .services
            .values_mut()
            .filter(|entry| entry.status.live() && !entry.spec.detached)
            .map(|entry| {
                entry.status = ServiceStatus::Killed;
                (entry.spec.name.clone(), entry.pid)
            })
            .collect();
        persist_detached_state(&reg);
        victims
    };
    for (_, pid) in pids {
        crate::tools::kill_process_group_tree(pid);
    }
}

/// Tests share the process-global registry; serialize them. Poison from a
/// failed peer is tolerated (the lock only serializes).
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-hub-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn spec(name: &str, program: &str, args: &[&str], ready: Option<ReadySpec>) -> LaunchSpec {
        LaunchSpec {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: temp_root(),
            env: Vec::new(),
            ready,
            detached: false,
        }
    }

    #[test]
    fn ring_cursor_semantics() {
        let mut ring = Ring::new(8);
        ring.push_chunk("one\ntwo\nthr");
        ring.push_chunk("ee\nfour\nfive\n");
        let (lines, cursor) = ring.since(0);
        assert_eq!(lines, vec!["one", "two", "three", "four", "five"]);
        assert_eq!(cursor, 5);
        let (newer, _) = ring.since(3);
        assert_eq!(newer, vec!["four", "five"]);

        // Cap eviction drops the oldest lines while cursors stay monotonic.
        let mut capped = Ring::new(3);
        capped.push_chunk("a\nb\nc\nd\n");
        let (retained, cursor) = capped.since(0);
        assert_eq!(retained, vec!["b", "c", "d"]);
        assert_eq!(cursor, 4);
    }

    #[test]
    fn readiness_requires_observed_port() {
        let _guard = crate::hub::test_lock();
        // No listener on the port → readiness must time out and kill.
        let result = start(&spec(
            "hub-test-dead",
            "sleep",
            &["30"],
            Some(ReadySpec {
                log: None,
                port: Some(39_991),
                timeout_secs: Some(1),
            }),
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NOT_READY"),
            "expected not-ready error, got: {err}"
        );
    }

    #[test]
    fn duplicate_live_name_rejected() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-dupe";
        let first = start(&spec(name, "sleep", &["30"], None)).expect("first start");
        assert_eq!(first.status, "running");
        let second = start(&spec(name, "sleep", &["30"], None));
        let err = second.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NAME_TAKEN"),
            "expected name-taken error, got: {err}"
        );
        let _ = stop(name);
    }

    #[test]
    fn send_text_drives_repl() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-repl";
        let snapshot = start(&spec(
            name,
            "python3",
            &["-i", "-q"],
            Some(ReadySpec {
                log: Some(">>>".to_string()),
                port: None,
                timeout_secs: Some(10),
            }),
        ))
        .expect("repl start");
        assert_eq!(snapshot.status, "running");
        send_text(name, "print(40 + 2)", true).expect("send");
        let page = logs(name, None, Some(50), Some("42"), 5_000).expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("42")),
            "repl output must contain 42: {:?}",
            page.lines
        );
        let _ = stop(name);
    }

    #[test]
    fn restart_after_completion_works() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-restart";
        // A service that exits on its own, then restarts from the retained spec.
        let first = start(&spec(name, "echo", &["first-run"], None)).expect("first run");
        assert_eq!(first.status, "running");
        std::thread::sleep(Duration::from_millis(400));
        let settled = describe(name).expect("describe");
        assert!(
            settled.status == "exited",
            "echo should have exited: {settled:?}"
        );
        let restarted = restart(name).expect("restart");
        assert_eq!(restarted.status, "running");
        std::thread::sleep(Duration::from_millis(400));
        let page = logs(name, None, Some(50), Some("first-run"), 5_000).expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("first-run")),
            "restarted service must run the retained spec: {:?}",
            page.lines
        );
        let _ = stop(name).ok();
    }

    #[test]
    fn status_stays_running_for_live_repl() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stable";
        let snapshot = start(&spec(
            name,
            "python3",
            &["-i", "-q"],
            Some(ReadySpec {
                log: Some(">>>".to_string()),
                port: None,
                timeout_secs: Some(10),
            }),
        ))
        .expect("repl start");
        assert_eq!(snapshot.status, "running");
        for wait_ms in [100u64, 300, 600] {
            std::thread::sleep(Duration::from_millis(wait_ms));
            let current = describe(name).expect("describe");
            assert_eq!(
                current.status, "running",
                "status flipped to {} after {}ms with python alive (exit_code {:?})",
                current.status, wait_ms, current.exit_code
            );
        }
        send_keys(name, &["CTRL_C".to_string()]).expect("keys");
        std::thread::sleep(Duration::from_millis(200));
        let after = describe(name).expect("describe after keys");
        assert_eq!(after.status, "running", "status after CTRL_C: {after:?}");
        let _ = stop(name);
    }

    #[test]
    fn stop_leaves_no_survivors() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stop";
        let snapshot = start(&spec(name, "sleep", &["300"], None)).expect("start");
        let pid = snapshot.pid.expect("pid");
        let stopped = stop(name).expect("stop");
        assert_eq!(stopped.status, "killed");
        std::thread::sleep(Duration::from_millis(500));
        // A reaped-away process is dead; a not-yet-reaped zombie (state Z)
        // is dead too — only a live state fails the assertion.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(')').next()?.trim().chars().next());
        assert!(
            state.is_none() || state == Some('Z'),
            "process {pid} survived stop (state {state:?})"
        );
    }

    #[test]
    fn logs_cursor_advances_incrementally() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-cursor";
        let _ = start(&spec(
            name,
            "sh",
            &["-c", "echo first; sleep 300"],
            Some(ReadySpec {
                log: Some("first".to_string()),
                port: None,
                timeout_secs: Some(10),
            }),
        ))
        .expect("start");
        let first_page = logs(name, None, None, None, 0).expect("first page");
        assert!(first_page.lines.iter().any(|line| line.contains("first")));
        let cursor = first_page.cursor;
        send_text(name, "echo second", true).expect("send");
        let second_page = logs(name, Some(cursor), None, Some("second"), 5_000).expect("page 2");
        assert!(
            second_page.lines.iter().any(|line| line.contains("second")),
            "incremental page must contain the new line: {:?}",
            second_page.lines
        );
        assert!(second_page.cursor > cursor);
        let _ = stop(name);
    }
}
