//! Explicit local-launch terminal host. Only an opted-in DAP launch receives
//! this handler; externally connected/attached adapters never receive it.
//! Processes and PTYs belong to the original DapSession, not the live UI tab.

mod worker;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::dap::ReverseRequestHandler;
use super::{lock, tool_err};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::lsp::jsonrpc::{CompletionWaitError, await_completion};

const MAX_TERMINALS: usize = 8;
const OUTPUT_BYTES: usize = 32 * 1024;
const INPUT_BYTES: usize = 1024;
const INPUT_QUEUED: usize = 4;
const QUEUED: u8 = 0;
const WRITING: u8 = 1;
const CANCELLED: u8 = 2;
static TERMINALS: AtomicUsize = AtomicUsize::new(0);

type Connected = Arc<dyn Fn() -> bool + Send + Sync>;

/// Count a terminal until *all* its native lanes have ended, even if an
/// escaped descendant holds a PTY open after its owner was dropped.
struct Capacity;
impl Capacity {
    fn reserve() -> Result<Arc<Self>> {
        TERMINALS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            (count < MAX_TERMINALS).then_some(count + 1)
        }).map_err(|_| tool_err("DAP_TERMINAL_CAPACITY", "eight terminal lifetimes are still active"))?;
        Ok(Arc::new(Self))
    }
}
impl Drop for Capacity {
    fn drop(&mut self) { TERMINALS.fetch_sub(1, Ordering::SeqCst); }
}

#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    observed: u64,
    complete: bool,
    error: Option<String>,
}
impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.observed = self.observed.saturating_add(data.len() as u64);
        let data = &data[data.len().saturating_sub(OUTPUT_BYTES)..];
        let remove = (self.bytes.len() + data.len()).saturating_sub(OUTPUT_BYTES);
        self.bytes.drain(..remove);
        self.bytes.extend(data);
    }
    fn text(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        // Decode on read, not at chunk boundaries. An omitted leading UTF-8
        // fragment can be replaced, but a split read never corrupts a scalar.
        let text = String::from_utf8_lossy(&bytes);
        if self.observed > bytes.len() as u64 {
            format!("[terminal output prefix omitted]\n{text}")
        } else {
            text.into_owned()
        }
    }
}

struct Terminal {
    stop: AtomicBool,
    ended: AtomicBool,
    info: Mutex<Value>,
    tail: Mutex<Tail>,
    input: SyncSender<Input>,
}
impl Terminal {
    fn snapshot(&self) -> Value {
        let mut info = lock(&self.info).clone();
        let tail = lock(&self.tail);
        info["stopRequested"] = json!(self.stop.load(Ordering::SeqCst));
        info["outputBytes"] = json!(tail.observed);
        info["retainedBytes"] = json!(tail.bytes.len());
        info["outputComplete"] = json!(tail.complete);
        info["outputError"] = json!(tail.error);
        info
    }
}

struct Input {
    bytes: Vec<u8>,
    phase: Arc<AtomicU8>,
    reply: SyncSender<std::result::Result<usize, String>>,
}
struct InputLease(Arc<AtomicU8>);
impl Drop for InputLease {
    fn drop(&mut self) {
        let _ = self.0.compare_exchange(QUEUED, CANCELLED, Ordering::SeqCst, Ordering::SeqCst);
    }
}
struct Startup {
    terminal: Arc<Terminal>,
    accepted: bool,
}
impl Drop for Startup {
    fn drop(&mut self) {
        if !self.accepted { self.terminal.stop.store(true, Ordering::SeqCst); }
    }
}

pub(super) struct TerminalHost {
    workspace: PathBuf,
    connected: Connected,
    terminal: Mutex<Option<Arc<Terminal>>>,
}
impl TerminalHost {
    pub(super) fn new(workspace: &Path, connected: Connected) -> Result<Arc<Self>> {
        check_owner(&AgentCx::for_current_or_request(), true)?;
        let workspace = std::fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(tool_err("DAP_TERMINAL_CWD", "terminal workspace is not a directory"));
        }
        Ok(Arc::new(Self { workspace, connected, terminal: Mutex::new(None) }))
    }

    pub(super) fn handler(self: &Arc<Self>) -> ReverseRequestHandler {
        let weak = Arc::downgrade(self);
        Arc::new(move |command, arguments| {
            let weak = weak.clone();
            Box::pin(async move {
                if command != "runInTerminal" {
                    return Err("DAP_TERMINAL_DENIED: only runInTerminal is supported during this launch".into());
                }
                let host = weak.upgrade().ok_or("DAP_TERMINAL_CLOSED: launch host retired")?;
                host.launch(arguments).await.map_err(|error| error.to_string())
            })
        })
    }

    async fn launch(&self, arguments: Value) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner, true)?;
        if !(self.connected)() { return Err(tool_err("DAP_TERMINAL_CLOSED", "adapter connection closed")); }
        let request = Launch::parse(arguments)?;
        let capacity = Capacity::reserve()?;
        let (input, receiver) = sync_channel(INPUT_QUEUED);
        let terminal = Arc::new(Terminal {
            stop: AtomicBool::new(false), ended: AtomicBool::new(false),
            info: Mutex::new(json!({"state":"starting"})),
            tail: Mutex::new(Tail::default()), input,
        });
        {
            let mut current = lock(&self.terminal);
            if current.is_some() {
                return Err(tool_err("DAP_TERMINAL_EXISTS", "one terminal launch is permitted per debugger session"));
            }
            *current = Some(Arc::clone(&terminal));
        }
        let mut startup = Startup { terminal: Arc::clone(&terminal), accepted: false };
        let (sender, started) = futures::channel::oneshot::channel();
        let workspace = self.workspace.clone();
        let connected = Arc::clone(&self.connected);
        let worker_owner = owner.clone();
        std::thread::Builder::new().name("dap-terminal-owner".into()).spawn(move || {
            worker::run(request, workspace, worker_owner, connected, terminal, receiver, sender, capacity);
        }).map_err(|error| tool_err("DAP_TERMINAL_SPAWN", format!("cannot start terminal owner: {error}")))?;
        let pid = started.await
            .map_err(|_| tool_err("DAP_TERMINAL_SPAWN", "terminal owner ended before reporting startup"))?
            .map_err(|error| tool_err("DAP_TERMINAL_SPAWN", error))?;
        check_owner(&owner, true)?;
        if !(self.connected)() { return Err(tool_err("DAP_TERMINAL_CLOSED", "adapter closed during startup")); }
        startup.accepted = true;
        Ok(json!({"processId":pid}))
    }

    pub(super) fn require_started(&self) -> Result<()> {
        let terminal = lock(&self.terminal).clone()
            .ok_or_else(|| tool_err("DAP_TERMINAL_NOT_STARTED", "adapter completed launch without requesting a terminal"))?;
        let info = lock(&terminal.info);
        if info["pid"].as_u64().is_none() || info["state"] == "failed"
            || terminal.stop.load(Ordering::SeqCst)
        {
            return Err(tool_err("DAP_TERMINAL_NOT_STARTED", "adapter did not obtain a successfully started terminal"));
        }
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Value {
        lock(&self.terminal).clone().map_or_else(
            || json!({"state":"notStarted"}), |terminal| terminal.snapshot(),
        )
    }

    pub(super) fn output(&self) -> String {
        lock(&self.terminal).clone().map_or_else(String::new, |terminal| lock(&terminal.tail).text())
    }

    pub(super) async fn send(&self, text: &str, enter: bool) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner, false)?;
        if text.len() > INPUT_BYTES || text.contains('\0') || (text.is_empty() && !enter) {
            return Err(tool_err("DAP_TERMINAL_INPUT", "input must contain at most 1024 UTF-8 bytes, without NUL"));
        }
        let terminal = lock(&self.terminal).clone()
            .ok_or_else(|| tool_err("DAP_TERMINAL_NOT_STARTED", "no terminal was started"))?;
        if terminal.stop.load(Ordering::SeqCst) || terminal.ended.load(Ordering::SeqCst)
            || !(self.connected)()
        {
            return Err(tool_err("DAP_TERMINAL_CLOSED", "terminal is no longer accepting input"));
        }
        let mut bytes = text.as_bytes().to_vec();
        if enter { bytes.push(b'\r'); }
        let phase = Arc::new(AtomicU8::new(QUEUED));
        let _lease = InputLease(Arc::clone(&phase));
        let (reply, completed) = sync_channel(1);
        terminal.input.try_send(Input { bytes, phase, reply })
            .map_err(|_| tool_err("DAP_TERMINAL_BUSY", "terminal input queue is full or closed"))?;
        let result = await_completion(completed, Duration::from_secs(30), || {}).await
            .map_err(|error| match error {
                CompletionWaitError::Cancelled => tool_err("DAP_CANCELLED", "terminal input cancelled; an admitted write may complete"),
                CompletionWaitError::Timeout => tool_err("DAP_TERMINAL_TIMEOUT", "terminal input wait expired; an admitted write may complete"),
                CompletionWaitError::Closed => tool_err("DAP_TERMINAL_CLOSED", "terminal input writer closed"),
            })?;
        check_owner(&owner, false)?;
        let written = result.map_err(|error| tool_err("DAP_TERMINAL_INPUT", error))?;
        Ok(json!({"bytesWritten":written,"terminal":terminal.snapshot()}))
    }
}
impl Drop for TerminalHost {
    fn drop(&mut self) {
        if let Some(terminal) = lock(&self.terminal).as_ref() {
            terminal.stop.store(true, Ordering::SeqCst);
        }
    }
}

fn check_owner(owner: &AgentCx, spawning: bool) -> Result<()> {
    owner.checkpoint().map_err(|_| tool_err("DAP_CANCELLED", "terminal operation cancelled"))?;
    let caps = owner.capabilities();
    if !caps.io || !caps.time || (spawning && !caps.spawn) {
        return Err(tool_err("DAP_PERMISSION", "terminal operation lacks required I/O, timer or spawn authority"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Launch {
    kind: Option<String>,
    title: Option<String>,
    cwd: String,
    args: Vec<String>,
    env: Option<BTreeMap<String, Option<String>>>,
    args_can_be_interpreted_by_shell: Option<bool>,
}
impl Launch {
    fn parse(arguments: Value) -> Result<Self> {
        if serde_json::to_vec(&arguments)?.len() > 64 * 1024 {
            return Err(tool_err("DAP_TERMINAL_USAGE", "terminal launch exceeds 64 KiB"));
        }
        let request: Self = serde_json::from_value(arguments)
            .map_err(|error| tool_err("DAP_TERMINAL_USAGE", format!("invalid runInTerminal arguments: {error}")))?;
        if request.kind.as_deref().is_some_and(|kind| kind != "integrated")
            || request.args_can_be_interpreted_by_shell == Some(true)
            || request.title.as_ref().is_some_and(|title| title.len() > 256 || title.contains('\0'))
            || request.cwd.len() > 4096 || request.cwd.contains('\0')
            || request.args.is_empty() || request.args.len() > 128 || request.args[0].is_empty()
            || request.args.iter().any(|arg| arg.len() > 16 * 1024 || arg.contains('\0'))
            || request.env.as_ref().is_some_and(|env| {
                env.len() > 128 || env.iter().any(|(key, value)| {
                    key.is_empty() || key.len() > 256 || key.contains(['\0', '='])
                        || value.as_ref().is_some_and(|value| value.len() > 4096 || value.contains('\0'))
                })
            })
        {
            return Err(tool_err("DAP_TERMINAL_USAGE", "only bounded argv/environment and an integrated terminal without shell interpretation are supported"));
        }
        Ok(request)
    }

    fn directory(&self, workspace: &Path) -> Result<PathBuf> {
        let cwd = std::fs::canonicalize(workspace.join(&self.cwd))?;
        if !cwd.is_dir() || !cwd.starts_with(workspace) {
            return Err(tool_err("DAP_TERMINAL_CWD", "terminal directory must remain inside the original workspace"));
        }
        Ok(cwd)
    }
}
