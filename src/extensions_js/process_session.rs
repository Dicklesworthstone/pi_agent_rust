//! Bounded, extension-owned subprocess sessions for full-duplex stdio.
//!
//! Creation is reachable only after the shared `exec` admission and mediation
//! path. Native JS callbacks can operate on an existing opaque handle, never
//! spawn a process or address an arbitrary OS PID. All blocking pipe I/O runs
//! on bounded worker threads; the JS thread only admits input and drains output.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rquickjs::prelude::Func;
use rquickjs::{Ctx, Exception};
use serde_json::{Value, json};

const MAX_SESSIONS: usize = 8;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const INPUT_QUEUE_CAP: usize = 16;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_FRAMES: usize = 2048;
const MAX_POLL_FRAMES: usize = 64;
const MAX_ENV_BYTES: usize = 128 * 1024;
const MAX_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;
const WORKER_POLL: Duration = Duration::from_millis(10);
const TERMINATE_GRACE: Duration = Duration::from_secs(1);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Completion(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(finish) = self
            .0
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            finish();
        }
    }
}

/// Keep the admission lease attached to the actual child during partial
/// startup too. Failure to create an I/O/monitor worker must reap the child
/// before returning its subprocess budget to the admission controller.
struct OwnedProcess {
    child: crate::tools::ProcessGuard,
    _slot: Arc<SessionSlot>,
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.child.terminate_and_wait();
    }
}

/// Closing a guest handle requests termination; it does not mean the process
/// and its I/O workers have exited. Keep the per-runtime admission slot until
/// teardown completes so rapid close/spawn cycles cannot exceed the budget.
struct SessionSlot {
    active: Arc<AtomicUsize>,
    _completion: Completion,
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct Output {
    frames: VecDeque<(Value, usize)>,
    bytes: usize,
    terminal: Option<Value>,
}

struct SessionState {
    alive: Arc<AtomicBool>,
    stopping: AtomicBool,
    signal: Mutex<Option<String>>,
    error: Mutex<Option<String>>,
    output: Mutex<Output>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            stopping: AtomicBool::new(false),
            signal: Mutex::new(None),
            error: Mutex::new(None),
            output: Mutex::new(Output::default()),
        }
    }

    fn fail(&self, message: impl Into<String>) {
        let mut error = lock(&self.error);
        if error.is_none() {
            *error = Some(message.into());
        }
        self.stopping.store(true, Ordering::Release);
    }

    fn emit(&self, frame: Value, bytes: usize) -> bool {
        let accepted = {
            let mut output = lock(&self.output);
            if output.frames.len() >= MAX_OUTPUT_FRAMES
                || output.bytes.saturating_add(bytes) > MAX_OUTPUT_BYTES
            {
                false
            } else {
                output.bytes += bytes;
                output.frames.push_back((frame, bytes));
                true
            }
        };
        if !accepted {
            self.fail("Persistent process output exceeded the bounded receive queue");
        }
        accepted
    }
}

enum Input {
    Write { id: u64, data: Vec<u8> },
    End { id: u64 },
}

struct Session {
    owner: Option<String>,
    input: SyncSender<Input>,
    state: Arc<SessionState>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.alive.store(false, Ordering::Release);
    }
}

struct Registry {
    sessions: Mutex<HashMap<String, Session>>,
    active_sessions: Arc<AtomicUsize>,
    default_cwd: String,
    environment: HashMap<String, String>,
    deny_env: bool,
    owner: Option<Arc<str>>,
}

#[derive(Clone)]
pub(super) struct ProcessSessions(Arc<Registry>);

impl ProcessSessions {
    pub(super) fn new(
        default_cwd: String,
        environment: HashMap<String, String>,
        deny_env: bool,
        owner: Option<Arc<str>>,
    ) -> Self {
        Self(Arc::new(Registry {
            sessions: Mutex::new(HashMap::new()),
            active_sessions: Arc::new(AtomicUsize::new(0)),
            default_cwd,
            environment,
            deny_env,
            owner,
        }))
    }

    pub(super) fn close_all(&self) {
        lock(&self.0.sessions).clear();
    }

    fn check_owner(&self, owner: Option<&str>) -> Result<(), String> {
        if self
            .0
            .owner
            .as_deref()
            .is_some_and(|fixed| owner != Some(fixed))
        {
            return Err("Persistent process owner mismatch".to_string());
        }
        Ok(())
    }

    fn with_session<T>(
        &self,
        owner: Option<&str>,
        handle: &str,
        operation: impl FnOnce(&Session) -> Result<T, String>,
    ) -> Result<T, String> {
        self.check_owner(owner)?;
        let sessions = lock(&self.0.sessions);
        let session = sessions
            .get(handle)
            .filter(|session| session.owner.as_deref() == owner)
            .ok_or_else(|| {
                "Persistent process handle is unavailable to this extension".to_string()
            })?;
        operation(session)
    }

    pub(super) fn install(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        let global = ctx.globals();
        global.set(
            "__pi_process_session_poll_native",
            Func::from({
                let sessions = self.clone();
                move |ctx: Ctx<'_>, handle: String| -> rquickjs::Result<String> {
                    sessions
                        .poll(super::current_extension_id(&ctx).as_deref(), &handle)
                        .map(|value| value.to_string())
                        .map_err(|message| Exception::throw_message(&ctx, &message))
                }
            }),
        )?;
        global.set(
            "__pi_process_session_write_native",
            Func::from({
                let sessions = self.clone();
                move |ctx: Ctx<'_>,
                      handle: String,
                      id: u64,
                      data: String|
                      -> rquickjs::Result<bool> {
                    sessions
                        .write(
                            super::current_extension_id(&ctx).as_deref(),
                            &handle,
                            id,
                            &data,
                        )
                        .map_err(|message| Exception::throw_message(&ctx, &message))
                }
            }),
        )?;
        global.set(
            "__pi_process_session_end_native",
            Func::from({
                let sessions = self.clone();
                move |ctx: Ctx<'_>, handle: String, id: u64| -> rquickjs::Result<bool> {
                    sessions
                        .end(super::current_extension_id(&ctx).as_deref(), &handle, id)
                        .map_err(|message| Exception::throw_message(&ctx, &message))
                }
            }),
        )?;
        global.set(
            "__pi_process_session_kill_native",
            Func::from({
                let sessions = self.clone();
                move |ctx: Ctx<'_>, handle: String, signal: String| -> rquickjs::Result<bool> {
                    sessions
                        .kill(
                            super::current_extension_id(&ctx).as_deref(),
                            &handle,
                            &signal,
                        )
                        .map_err(|message| Exception::throw_message(&ctx, &message))
                }
            }),
        )?;
        global.set(
            "__pi_process_session_close_native",
            Func::from({
                let sessions = self.clone();
                move |ctx: Ctx<'_>, handle: String| -> rquickjs::Result<bool> {
                    sessions
                        .release(super::current_extension_id(&ctx).as_deref(), &handle)
                        .map_err(|message| Exception::throw_message(&ctx, &message))
                }
            }),
        )?;
        Ok(())
    }

    /// Called exclusively by the admitted shared exec dispatcher. `on_exit`
    /// retains its subprocess accounting guard through real child teardown.
    #[allow(clippy::too_many_lines)]
    pub(super) fn spawn(
        &self,
        owner: Option<&str>,
        command: &str,
        args: &[String],
        options: &Value,
        on_exit: Box<dyn FnOnce() + Send>,
    ) -> Result<Value, String> {
        let completion = Completion(Mutex::new(Some(on_exit)));
        self.check_owner(owner)?;
        let opts = options
            .as_object()
            .ok_or_else(|| "Persistent exec options must be an object".to_string())?;
        if opts
            .get("shell")
            .is_some_and(|value| value != &Value::Bool(false))
            || opts
                .get("detached")
                .is_some_and(|value| value != &Value::Bool(false))
        {
            return Err("Persistent processes require shell=false and detached=false".to_string());
        }
        if command.is_empty() || command.len() > 4096 || command.contains('\0') {
            return Err("Invalid persistent process command".to_string());
        }
        if args.len() > 256
            || args.iter().any(|arg| arg.contains('\0'))
            || args.iter().map(String::len).sum::<usize>() > MAX_ENV_BYTES
        {
            return Err(
                "Persistent process arguments exceed the bounded argument budget".to_string(),
            );
        }
        let timeout = opts
            .get("timeout")
            .or_else(|| opts.get("timeoutMs"))
            .or_else(|| opts.get("timeout_ms"));
        let lifetime_ms = match timeout {
            None => MAX_LIFETIME_MS,
            Some(value) => match value.as_u64() {
                Some(0) => MAX_LIFETIME_MS,
                Some(ms) if ms <= MAX_LIFETIME_MS => ms,
                _ => return Err("Persistent process timeout must be 0..=86400000 ms".to_string()),
            },
        };
        let cwd = opts
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or(&self.0.default_cwd);
        if cwd.contains('\0') {
            return Err("Persistent process cwd contains NUL".to_string());
        }
        let cwd = if Path::new(cwd).is_absolute() {
            Path::new(cwd).to_path_buf()
        } else {
            Path::new(&self.0.default_cwd).join(cwd)
        };
        let environment = self.environment(opts.get("env"))?;
        let executable = resolve_executable(command, &environment, &cwd)?;
        let stdio = match opts.get("stdio") {
            None => vec!["pipe", "pipe", "pipe"],
            Some(Value::Array(values)) if values.len() == 3 => values
                .iter()
                .map(|value| match value.as_str() {
                    Some(mode @ ("pipe" | "ignore")) => Ok(mode),
                    _ => Err("Persistent process stdio entries must be pipe or ignore".to_string()),
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("Persistent process stdio must contain three entries".to_string()),
        };
        let mut sessions = lock(&self.0.sessions);
        if sessions.len() >= MAX_SESSIONS
            || self.0.active_sessions.load(Ordering::Acquire) >= MAX_SESSIONS
        {
            return Err("Persistent process session limit reached (8 per runtime)".to_string());
        }
        // Spawn admission is serialized by the session registry. Workers only
        // decrement this count, after the child and all pipe workers retire.
        self.0.active_sessions.fetch_add(1, Ordering::AcqRel);
        let slot = Arc::new(SessionSlot {
            active: Arc::clone(&self.0.active_sessions),
            _completion: completion,
        });
        let mut builder = crate::tools::command_with_default_sigpipe_in_dir(
            executable
                .to_str()
                .ok_or("Persistent process executable path is not UTF-8")?,
            &cwd,
        )
        .map_err(|error| error.to_string())?;
        builder
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::tools::isolate_command_process_group(&mut builder);
        let child = crate::tools::spawn_command_with_job_discipline(&mut builder)
            .map_err(|error| format!("Persistent process spawn: {error}"))?;
        let pid = child.id();
        let mut process = OwnedProcess {
            child: crate::tools::ProcessGuard::new(
                child,
                crate::tools::ProcessCleanupMode::ProcessGroupTree,
            ),
            _slot: Arc::clone(&slot),
        };
        let stdin = process
            .child
            .take_stdin()
            .ok_or("Persistent process has no stdin")?;
        let stdout = process
            .child
            .take_stdout()
            .ok_or("Persistent process has no stdout")?;
        let stderr = process
            .child
            .take_stderr()
            .ok_or("Persistent process has no stderr")?;
        let state = Arc::new(SessionState::new());
        let (input, receiver) = sync_channel(INPUT_QUEUE_CAP);
        let session = Session {
            owner: owner.map(ToString::to_string),
            input,
            state: Arc::clone(&state),
        };
        #[cfg(unix)]
        let (stdin, stdout, stderr) = {
            let pipes = crate::mcp::transport::cancellable_stdio::prepare(
                stdin,
                stdout,
                stderr,
                &state.alive,
            )
            .map_err(|error| format!("Persistent process pipe setup: {error}"))?;
            (pipes.stdin, pipes.stdout, pipes.stderr)
        };
        let stdout_worker = start_reader(
            stdout,
            Arc::clone(&state),
            "stdout",
            stdio[1] == "pipe",
            Arc::clone(&slot),
        )?;
        let stderr_worker = start_reader(
            stderr,
            Arc::clone(&state),
            "stderr",
            stdio[2] == "pipe",
            Arc::clone(&slot),
        )?;
        let input_worker = {
            let state = Arc::clone(&state);
            let slot = Arc::clone(&slot);
            std::thread::Builder::new()
                .name("pi-ext-stdin".to_string())
                .spawn(move || {
                    let _slot = slot;
                    write_input(stdin, receiver, &state);
                })
                .map_err(|error| format!("Persistent process stdin worker: {error}"))?
        };
        let worker_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("pi-ext-process".to_string())
            .spawn(move || {
                let mut process = process;
                monitor(
                    &mut process.child,
                    pid,
                    &worker_state,
                    Duration::from_millis(lifetime_ms),
                    [stdout_worker, stderr_worker, input_worker],
                );
            })
            .map_err(|error| format!("Persistent process monitor: {error}"))?;
        let handle = uuid::Uuid::new_v4().simple().to_string();
        sessions.insert(handle.clone(), session);
        Ok(json!({"session": handle, "pid": pid}))
    }

    pub(super) fn environment(
        &self,
        explicit: Option<&Value>,
    ) -> Result<BTreeMap<String, String>, String> {
        let mut environment = BTreeMap::new();
        if let Some(explicit) = explicit {
            let values = explicit
                .as_object()
                .ok_or_else(|| "Persistent process env must be an object".to_string())?;
            for (name, value) in values {
                if value.is_null() {
                    continue;
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| "Persistent process env values must be strings".to_string())?;
                if !safe_environment_name(name) {
                    return Err(format!(
                        "Persistent process env variable {name:?} is not permitted"
                    ));
                }
                if value.contains('\0') {
                    return Err("Persistent process env value contains NUL".to_string());
                }
                #[cfg(windows)]
                if environment
                    .keys()
                    .any(|key: &String| key.eq_ignore_ascii_case(name))
                {
                    return Err(
                        "Persistent process env has duplicate Windows variable names".to_string(),
                    );
                }
                environment.insert(name.clone(), value.to_string());
            }
        } else {
            for (name, value) in &self.0.environment {
                let permitted = !self.0.deny_env
                    || crate::lsp::jsonrpc::MCP_ENV_ALLOWLIST
                        .iter()
                        .any(|allowed| {
                            if cfg!(windows) {
                                name.eq_ignore_ascii_case(allowed)
                            } else {
                                name == allowed
                            }
                        });
                if permitted && safe_environment_name(name) && !value.contains('\0') {
                    #[cfg(windows)]
                    if environment
                        .keys()
                        .any(|key: &String| key.eq_ignore_ascii_case(name))
                    {
                        return Err(
                            "Persistent process env has duplicate Windows variable names"
                                .to_string(),
                        );
                    }
                    environment.insert(name.clone(), value.clone());
                }
            }
        }
        if environment.len() > 512
            || environment
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                > MAX_ENV_BYTES
        {
            return Err(
                "Persistent process env exceeds its bounded environment budget".to_string(),
            );
        }
        Ok(environment)
    }

    fn write(
        &self,
        owner: Option<&str>,
        handle: &str,
        id: u64,
        encoded: &str,
    ) -> Result<bool, String> {
        if encoded.len() > MAX_INPUT_BYTES.div_ceil(3) * 4 {
            return Err("Persistent process stdin chunk exceeds 256 KiB".to_string());
        }
        let data = BASE64
            .decode(encoded)
            .map_err(|_| "Invalid stdin base64".to_string())?;
        if data.len() > MAX_INPUT_BYTES {
            return Err("Persistent process stdin chunk exceeds 256 KiB".to_string());
        }
        self.enqueue(owner, handle, Input::Write { id, data })
    }

    fn end(&self, owner: Option<&str>, handle: &str, id: u64) -> Result<bool, String> {
        self.enqueue(owner, handle, Input::End { id })
    }

    fn enqueue(&self, owner: Option<&str>, handle: &str, input: Input) -> Result<bool, String> {
        self.with_session(owner, handle, |session| {
            if !session.state.alive.load(Ordering::Acquire) {
                return Err("EPIPE: persistent process stdin is closed".to_string());
            }
            match session.input.try_send(input) {
                Ok(()) => Ok(true),
                Err(TrySendError::Full(_)) => Ok(false),
                Err(TrySendError::Disconnected(_)) => {
                    Err("EPIPE: persistent process stdin is closed".to_string())
                }
            }
        })
    }

    fn poll(&self, owner: Option<&str>, handle: &str) -> Result<Value, String> {
        let result = self.with_session(owner, handle, |session| {
            let mut output = lock(&session.state.output);
            let mut frames = Vec::new();
            for _ in 0..MAX_POLL_FRAMES {
                let Some((frame, bytes)) = output.frames.pop_front() else {
                    break;
                };
                output.bytes = output.bytes.saturating_sub(bytes);
                frames.push(frame);
            }
            let terminal = if output.frames.is_empty() {
                output.terminal.take()
            } else {
                None
            };
            Ok(json!({"frames": frames, "terminal": terminal}))
        })?;
        if !result["terminal"].is_null() {
            lock(&self.0.sessions).remove(handle);
        }
        Ok(result)
    }

    fn kill(&self, owner: Option<&str>, handle: &str, signal: &str) -> Result<bool, String> {
        if !matches!(signal, "SIGTERM" | "SIGKILL") {
            return Err("Persistent processes support SIGTERM and SIGKILL".to_string());
        }
        self.with_session(owner, handle, |session| {
            if !session.state.alive.load(Ordering::Acquire) {
                return Ok(false);
            }
            *lock(&session.state.signal) = Some(signal.to_string());
            session.state.stopping.store(true, Ordering::Release);
            Ok(true)
        })
    }

    fn release(&self, owner: Option<&str>, handle: &str) -> Result<bool, String> {
        self.check_owner(owner)?;
        let mut sessions = lock(&self.0.sessions);
        if sessions
            .get(handle)
            .is_some_and(|session| session.owner.as_deref() != owner)
        {
            return Err("Persistent process handle is unavailable to this extension".to_string());
        }
        Ok(sessions.remove(handle).is_some())
    }
}

/// Names that can be copied into a child environment without introducing a
/// secret, loader hook or invalid OS binding. Snapshot enumeration shares
/// this restriction so `{ ...process.env }` remains usable with the process
/// bridge. Individual reads still use the realm's disclosure policy (for
/// example, an allowlisted secret or a Windows per-drive `=D:` value).
pub fn safe_environment_name(name: &str) -> bool {
    if name.is_empty() || name.contains(['\0', '=']) || !super::is_env_var_allowed(name) {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    !upper.starts_with("LD_")
        && !upper.starts_with("DYLD_")
        && !upper.starts_with("BUN_JSC_")
        && !matches!(
            upper.as_str(),
            "NODE_OPTIONS"
                | "NODE_PATH"
                | "BUN_OPTIONS"
                | "BASH_ENV"
                | "ENV"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "IFS"
                | "CDPATH"
                | "GCONV_PATH"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "PYTHONSTARTUP"
                | "RUBYOPT"
                | "RUBYLIB"
                | "PERL5OPT"
                | "PERL5LIB"
                | "JAVA_TOOL_OPTIONS"
                | "_JAVA_OPTIONS"
                | "JDK_JAVA_OPTIONS"
                | "CLASSPATH"
        )
}

fn environment_value<'a>(environment: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    environment.get(key).map(String::as_str).or_else(|| {
        if cfg!(windows) {
            environment
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.as_str())
        } else {
            None
        }
    })
}

/// Resolve before constructing the SIGPIPE trampoline. Its historical bare
/// command lookup uses the host PATH, which must not override a session's
/// explicit/configured environment after env_clear().
fn resolve_executable(
    command: &str,
    environment: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    let requested = Path::new(command);
    #[cfg(windows)]
    if !requested.is_absolute()
        && requested
            .components()
            .any(|part| matches!(part, std::path::Component::Prefix(_)))
    {
        return Err(
            "Persistent processes do not accept drive-relative executable paths".to_string(),
        );
    }
    if requested.is_absolute() || requested.components().count() > 1 {
        let path = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            cwd.join(requested)
        };
        return std::fs::canonicalize(&path)
            .map_err(|error| format!("Persistent process executable: {error}"));
    }
    let configured_path = environment_value(environment, "PATH").unwrap_or_default();
    if configured_path.is_empty() || configured_path.len() > 32 * 1024 {
        return Err(
            "Persistent process bare executable requires a configured PATH of at most 32 KiB"
                .to_string(),
        );
    }
    let directories: Vec<_> = std::env::split_paths(configured_path).take(257).collect();
    if directories.len() > 256 {
        return Err("Persistent process PATH exceeds 256 entries".to_string());
    }
    let names = if cfg!(windows) && requested.extension().is_none() {
        environment_value(environment, "PATHEXT")
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .take(16)
            .filter(|suffix| {
                suffix.starts_with('.')
                    && (2..=16).contains(&suffix.len())
                    && suffix.as_bytes()[1..].iter().all(u8::is_ascii_alphanumeric)
            })
            .map(|suffix| format!("{command}{suffix}"))
            .collect::<Vec<_>>()
    } else {
        vec![command.to_string()]
    };
    for directory in directories {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        for name in &names {
            let candidate = directory.join(name);
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            if let Ok(executable) = std::fs::canonicalize(candidate) {
                return Ok(executable);
            }
        }
    }
    Err(format!(
        "Persistent process executable {command:?} was not found in its configured PATH"
    ))
}

fn start_reader(
    mut reader: impl Read + Send + 'static,
    state: Arc<SessionState>,
    stream: &'static str,
    visible: bool,
    slot: Arc<SessionSlot>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name(format!("pi-ext-{stream}"))
        .spawn(move || {
            let _slot = slot;
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if visible
                            && !state.emit(
                                json!({"stream": stream, "data": BASE64.encode(&buffer[..count])}),
                                count,
                            )
                        {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        if state.alive.load(Ordering::Acquire) {
                            state.fail(format!("Persistent process {stream}: {error}"));
                        }
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("Persistent process {stream} worker: {error}"))
}

fn write_input(mut stdin: impl Write, receiver: Receiver<Input>, state: &SessionState) {
    while state.alive.load(Ordering::Acquire) {
        match receiver.recv_timeout(WORKER_POLL) {
            Ok(Input::Write { id, data }) => {
                let error = stdin.write_all(&data).and_then(|()| stdin.flush()).err();
                let message = error.as_ref().map(ToString::to_string);
                if !state.emit(json!({"writeId": id, "error": message}), 128) || error.is_some() {
                    break;
                }
            }
            Ok(Input::End { id }) => {
                drop(stdin);
                state.emit(json!({"writeId": id, "end": true}), 128);
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn monitor(
    child: &mut crate::tools::ProcessGuard,
    pid: u32,
    state: &SessionState,
    lifetime: Duration,
    workers: [std::thread::JoinHandle<()>; 3],
) {
    let started = Instant::now();
    let mut terminating = None;
    let status = loop {
        match child.try_wait_child() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        if started.elapsed() >= lifetime {
            state.fail("Persistent process lifetime exceeded");
        }
        if state.stopping.load(Ordering::Acquire) {
            let signal = lock(&state.signal).clone();
            if signal.as_deref() == Some("SIGKILL")
                || terminating.is_some_and(|when: Instant| when.elapsed() >= TERMINATE_GRACE)
            {
                break child.terminate_and_wait();
            }
            if terminating.is_none() {
                // The guard still owns a live child at this point. Never signal
                // a reaped PID or process outside this runtime's registry.
                crate::tools::terminate_process_group_tree(Some(pid));
                terminating = Some(Instant::now());
            }
        }
        std::thread::sleep(WORKER_POLL);
    };
    // Preserve buffered stdout/stderr on ordinary exit; cancellation of all
    // endpoints is bounded even when a descendant retained a pipe descriptor.
    let drain_started = Instant::now();
    while !(workers[0].is_finished() && workers[1].is_finished())
        && drain_started.elapsed() < TERMINATE_GRACE
    {
        std::thread::sleep(WORKER_POLL);
    }
    state.alive.store(false, Ordering::Release);
    if status.is_err() {
        let _ = child.kill();
    } else {
        // A normally exiting root can leave descendants in its owned group.
        // Retire that group before releasing I/O or admission accounting;
        // terminate_and_wait handles a reaped root without walking its PID.
        let _ = child.terminate_and_wait();
    }
    for worker in workers {
        // Unix endpoints use the shared cancellation-aware pipe wrapper;
        // Windows endpoints close when the ProcessGuard-owned Job is reaped.
        let _ = worker.join();
    }
    let requested_signal = lock(&state.signal).clone();
    let (code, signal) = match status {
        Ok(status) => {
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt as _;
                status.signal().map(|number| match number {
                    2 => "SIGINT".to_string(),
                    9 => "SIGKILL".to_string(),
                    15 => "SIGTERM".to_string(),
                    value => format!("SIG{value}"),
                })
            };
            #[cfg(not(unix))]
            let signal = requested_signal.clone();
            (status.code(), signal)
        }
        Err(error) => {
            state.fail(format!("Persistent process wait: {error}"));
            (None, requested_signal)
        }
    };
    let error = lock(&state.error).clone();
    lock(&state.output).terminal = Some(json!({"code": code, "signal": signal, "error": error}));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn process_environment_filters_secrets_and_loader_injection() {
        for name in [
            "OPENAI_API_KEY",
            "PASSWORD",
            "LD_PRELOAD",
            "Dyld_Insert_Libraries",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "BASH_ENV",
            "A=B",
        ] {
            assert!(!safe_environment_name(name), "{name}");
        }
        for name in [
            "PATH",
            "SystemRoot",
            "CONTEXT_MODE_PLATFORM",
            "PI_WORKSPACE_DIR",
        ] {
            assert!(safe_environment_name(name), "{name}");
        }
    }

    #[test]
    fn inherited_environment_filters_loader_hooks_but_explicit_hooks_are_rejected() {
        let sessions = ProcessSessions::new(
            ".".into(),
            HashMap::from([
                ("PATH".to_string(), "/configured/bin".to_string()),
                (
                    "NODE_OPTIONS".to_string(),
                    "--require=untrusted".to_string(),
                ),
                ("LD_PRELOAD".to_string(), "/untrusted.so".to_string()),
                ("=D:".to_string(), "D:\\project".to_string()),
            ]),
            false,
            Some(Arc::from("owner")),
        );
        assert_eq!(
            sessions.environment(None).unwrap(),
            BTreeMap::from([("PATH".to_string(), "/configured/bin".to_string())])
        );
        for name in ["NODE_OPTIONS", "LD_PRELOAD", "PYTHONPATH", "=D:"] {
            let explicit = json!({name: "injected"});
            assert!(sessions.environment(Some(&explicit)).is_err(), "{name}");
        }
    }

    #[test]
    fn session_handles_are_owned_and_input_queues_are_bounded() {
        let sessions =
            ProcessSessions::new(".".into(), HashMap::new(), false, Some(Arc::from("owner")));
        let (sender, _receiver) = sync_channel(1);
        lock(&sessions.0.sessions).insert(
            "handle".into(),
            Session {
                owner: Some("owner".into()),
                input: sender,
                state: Arc::new(SessionState::new()),
            },
        );
        assert!(sessions.poll(Some("other"), "handle").is_err());
        assert!(sessions.write(Some("other"), "handle", 1, "eA==").is_err());
        assert!(sessions.kill(Some("other"), "handle", "SIGKILL").is_err());
        assert!(sessions.write(Some("owner"), "handle", 1, "eA==").unwrap());
        assert!(!sessions.write(Some("owner"), "handle", 2, "eA==").unwrap());
        assert!(
            sessions
                .write(Some("owner"), "handle", 3, &"A".repeat(MAX_INPUT_BYTES * 2))
                .is_err()
        );
        sessions.close_all();
        assert!(sessions.poll(Some("owner"), "handle").is_err());
    }

    #[test]
    fn failed_process_start_releases_both_admission_leases() {
        let directory = tempfile::tempdir().expect("process fixture");
        let sessions = ProcessSessions::new(
            directory.path().display().to_string(),
            HashMap::new(),
            true,
            Some(Arc::from("owner")),
        );
        let finished = Arc::new(AtomicUsize::new(0));
        let marker = Arc::clone(&finished);
        let result = sessions.spawn(
            Some("owner"),
            std::env::current_exe()
                .expect("test executable")
                .to_str()
                .expect("UTF-8 executable"),
            &[],
            &json!({"cwd": directory.path().join("missing")}),
            Box::new(move || {
                marker.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert!(result.is_err(), "a missing working directory must fail");
        assert_eq!(sessions.0.active_sessions.load(Ordering::Acquire), 0);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert!(lock(&sessions.0.sessions).is_empty());
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn closed_handles_keep_process_slots_until_blocked_workers_are_reaped() {
        let directory = tempfile::tempdir().expect("process fixture");
        let sessions = ProcessSessions::new(
            directory.path().display().to_string(),
            HashMap::new(),
            true,
            Some(Arc::from("owner")),
        );
        let finished = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut handles = Vec::new();
        for _ in 0..MAX_SESSIONS {
            let marker = Arc::clone(&finished);
            let spawned = sessions
                .spawn(
                    Some("owner"),
                    "/bin/sh",
                    &[
                        "-c".to_string(),
                        "trap '' TERM; printf 'ready\\n'; while :; do sleep 1; done".to_string(),
                    ],
                    &json!({"env": {"PATH": "/usr/bin:/bin"}, "timeout": 10000}),
                    Box::new(move || {
                        marker.fetch_add(1, Ordering::SeqCst);
                    }),
                )
                .expect("spawn process that ignores graceful termination");
            let handle = spawned["session"].as_str().expect("handle").to_string();
            let mut output = String::new();
            loop {
                let batch = sessions
                    .poll(Some("owner"), &handle)
                    .expect("poll readiness");
                collect_frames(&batch, &mut output, &mut String::new(), &mut Vec::new());
                if output.contains("ready\n") {
                    break;
                }
                assert!(batch["terminal"].is_null(), "child exited early: {batch}");
                assert!(Instant::now() < deadline, "process readiness timed out");
                std::thread::sleep(WORKER_POLL);
            }
            handles.push(handle);
        }

        // The child never reads stdin. Fill the bounded input channel so a
        // real pipe writer is blocked during cancellation, not just sleeping.
        let encoded = BASE64.encode(vec![b'x'; MAX_INPUT_BYTES]);
        let mut saturated = false;
        for id in 1..=u64::try_from(INPUT_QUEUE_CAP + 2).unwrap() {
            if !sessions
                .write(Some("owner"), &handles[0], id, &encoded)
                .expect("enqueue blocked input")
            {
                saturated = true;
                break;
            }
        }
        assert!(saturated, "the unread pipe must exert bounded backpressure");
        sessions.close_all();
        assert!(lock(&sessions.0.sessions).is_empty());
        let refused = sessions.spawn(
            Some("owner"),
            "/bin/sh",
            &["-c".to_string(), "exit 0".to_string()],
            &json!({"env": {"PATH": "/usr/bin:/bin"}}),
            Box::new(|| {}),
        );
        assert!(
            refused.unwrap_err().contains("session limit"),
            "closing handles must not admit more live/retiring workers"
        );
        while finished.load(Ordering::SeqCst) < MAX_SESSIONS {
            assert!(
                Instant::now() < deadline,
                "cancelled process workers were not reaped"
            );
            std::thread::sleep(WORKER_POLL);
        }
        assert_eq!(sessions.0.active_sessions.load(Ordering::Acquire), 0);

        let spawned = sessions
            .spawn(
                Some("owner"),
                "/bin/sh",
                &["-c".to_string(), "exit 0".to_string()],
                &json!({"env": {"PATH": "/usr/bin:/bin"}}),
                Box::new(|| {}),
            )
            .expect("admission resumes after actual teardown");
        let handle = spawned["session"].as_str().expect("handle");
        loop {
            let batch = sessions
                .poll(Some("owner"), handle)
                .expect("poll replacement");
            if !batch["terminal"].is_null() {
                assert_eq!(batch["terminal"]["code"], 0, "{batch}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement process did not finish"
            );
            std::thread::sleep(WORKER_POLL);
        }
    }

    #[test]
    fn process_resolution_uses_its_own_path_and_never_ambient_path() {
        let directory = tempfile::tempdir().expect("executable fixture");
        let executable = directory.path().join(if cfg!(windows) {
            "fixture.EXE"
        } else {
            "fixture"
        });
        std::fs::write(&executable, "metadata only, never executed").expect("fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
                .expect("mode");
        }
        let environment = BTreeMap::from([
            ("PATH".to_string(), directory.path().display().to_string()),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]);
        assert_eq!(
            resolve_executable("fixture", &environment, directory.path()).unwrap(),
            std::fs::canonicalize(executable).unwrap()
        );
        assert!(resolve_executable("fixture", &BTreeMap::new(), directory.path()).is_err());
        assert!(resolve_executable("absent", &environment, directory.path()).is_err());
    }

    /// A real portable subprocess peer: the same test executable is launched
    /// with this exact test selected and an explicit, nonsensitive environment.
    #[test]
    fn stdio_peer() {
        if std::env::var("PIJS_PROCESS_PEER").as_deref() != Ok("json-rpc") {
            return;
        }
        for line in std::io::stdin().lock().lines() {
            let request: Value =
                serde_json::from_str(&line.expect("peer input")).expect("JSON-RPC request");
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"protocolVersion":"2024-11-05", "capabilities":{"tools":{}}})
                }
                Some("tools/list") => {
                    json!({"tools":[{"name":"echo", "inputSchema":{"type":"object"}}]})
                }
                Some("tools/call") => {
                    json!({"content":[{"type":"text", "text":request["params"]["text"]}]})
                }
                _ => panic!("unexpected peer request"),
            };
            println!(
                "{}",
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            );
            std::io::stdout().flush().expect("peer flush");
        }
        eprintln!("persistent-stdio-peer-closed");
    }

    #[test]
    fn real_process_supports_multiple_json_rpc_roundtrips_and_eof() {
        let root = std::env::current_dir().expect("cwd");
        let executable = std::env::current_exe().expect("test executable");
        let name = format!(
            "{}::stdio_peer",
            module_path!().split_once("::").expect("crate module").1
        );
        let sessions = ProcessSessions::new(
            root.display().to_string(),
            HashMap::new(),
            true,
            Some(Arc::from("owner")),
        );
        let finished = Arc::new(AtomicUsize::new(0));
        let marker = Arc::clone(&finished);
        let mut environment = serde_json::Map::new();
        for &key in crate::lsp::jsonrpc::MCP_ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(key) {
                environment.insert(key.to_string(), Value::String(value));
            }
        }
        environment.insert("PIJS_PROCESS_PEER".into(), json!("json-rpc"));
        let spawned = sessions
            .spawn(
                Some("owner"),
                &executable.display().to_string(),
                &[
                    "--exact".into(),
                    name,
                    "--nocapture".into(),
                    "--quiet".into(),
                ],
                &json!({"env":environment, "timeout":10000}),
                Box::new(move || {
                    marker.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .expect("spawn portable peer");
        let handle = spawned["session"].as_str().expect("opaque handle");
        assert!(sessions.poll(Some("other"), handle).is_err());
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut acknowledgements = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        for (id, method) in [(1, "initialize"), (2, "tools/list"), (3, "tools/call")] {
            let message = format!(
                "{}\n",
                json!({"jsonrpc":"2.0", "id":id, "method":method, "params":{"text":"persistent λ"}})
            );
            assert!(
                sessions
                    .write(Some("owner"), handle, id, &BASE64.encode(message))
                    .expect("admit frame")
            );
            // Wait for each genuine response before sending the next request.
            // This proves duplex communication, not a finite stdin fixture.
            loop {
                let batch = sessions.poll(Some("owner"), handle).expect("poll peer");
                collect_frames(&batch, &mut stdout, &mut stderr, &mut acknowledgements);
                assert!(
                    batch["terminal"].is_null(),
                    "peer exited before EOF: {batch}"
                );
                if stdout.lines().any(|line| {
                    line.find("{\"jsonrpc\"")
                        .or_else(|| line.find("{\"id\""))
                        .and_then(|start| serde_json::from_str::<Value>(&line[start..]).ok())
                        .is_some_and(|response| response["id"] == id)
                }) {
                    break;
                }
                assert!(Instant::now() < deadline, "peer response timeout: {stdout}");
                std::thread::sleep(WORKER_POLL);
            }
        }
        assert!(sessions.end(Some("owner"), handle, 4).expect("close stdin"));
        let terminal = loop {
            let batch = sessions.poll(Some("owner"), handle).expect("poll EOF");
            collect_frames(&batch, &mut stdout, &mut stderr, &mut acknowledgements);
            if !batch["terminal"].is_null() {
                break batch["terminal"].clone();
            }
            assert!(Instant::now() < deadline, "peer EOF timeout");
            std::thread::sleep(WORKER_POLL);
        };
        assert_eq!(terminal["code"], 0, "{terminal}");
        assert!(terminal["error"].is_null(), "{terminal}");
        assert!(stdout.contains("persistent λ"), "{stdout}");
        assert!(stderr.contains("persistent-stdio-peer-closed"), "{stderr}");
        assert_eq!(acknowledgements, vec![1, 2, 3, 4]);
        assert!(sessions.poll(Some("owner"), handle).is_err());
        // The terminal record and completion guard become visible in the
        // same worker teardown; allow the final instruction to retire.
        while finished.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn natural_root_exit_retires_its_remaining_process_group() {
        let directory = tempfile::tempdir().expect("process fixture");
        let escaped = directory.path().join("descendant-survived");
        let sessions = ProcessSessions::new(
            directory.path().display().to_string(),
            HashMap::new(),
            true,
            Some(Arc::from("owner")),
        );
        let spawned = sessions
            .spawn(
                Some("owner"),
                "/bin/sh",
                &[
                    "-c".into(),
                    "(sleep 2; printf leaked > \"$1\") & exit 0".into(),
                    "pijs-peer".into(),
                    escaped.display().to_string(),
                ],
                &json!({"env":{"PATH":"/usr/bin:/bin"},"timeout":5000}),
                Box::new(|| {}),
            )
            .expect("spawn owned process group");
        let handle = spawned["session"].as_str().expect("handle");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let batch = sessions
                .poll(Some("owner"), handle)
                .expect("poll natural exit");
            if !batch["terminal"].is_null() {
                assert_eq!(batch["terminal"]["code"], 0, "{batch}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant held the session open"
            );
            std::thread::sleep(WORKER_POLL);
        }
        std::thread::sleep(Duration::from_millis(2200));
        assert!(!escaped.exists(), "a descendant survived its session root");
    }

    fn collect_frames(
        batch: &Value,
        stdout: &mut String,
        stderr: &mut String,
        acknowledgements: &mut Vec<u64>,
    ) {
        for frame in batch["frames"].as_array().expect("frames") {
            if let Some(stream) = frame["stream"].as_str() {
                let data = BASE64
                    .decode(frame["data"].as_str().expect("data"))
                    .expect("base64 output");
                let output = if stream == "stdout" {
                    &mut *stdout
                } else {
                    &mut *stderr
                };
                output.push_str(&String::from_utf8_lossy(&data));
            } else if let Some(id) = frame["writeId"].as_u64() {
                assert!(frame["error"].is_null(), "{frame}");
                acknowledgements.push(id);
            }
        }
    }
}
