//! Eval tool (bd-cv653.1.4): persistent code kernels with Jupyter-like cell
//! semantics — state persists across cells within a session.
//!
//! Python uses a retained subprocess and JavaScript a dedicated QuickJS realm.
//! Cells on one tool instance execute exclusively, including SDK callers that
//! bypass agent scheduling. Explicit reset discards only the selected language.
//! Timeout/crash state loss is reported; neither language silently substitutes
//! for the other. Agent-owned cells re-enter the owning Agent's live tool
//! policy; standalone tools retain a read-only local bridge.

use crate::error::{Error, Result};
#[cfg(test)]
use crate::model::ContentBlock;
use crate::tools::{ProcessCleanupMode, ProcessGuard, Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod bridge_io;
mod bridge_result;
mod display;
mod isolation;
mod js_kernel;
mod judgment;
mod lifecycle;
mod output;
mod python_runtime;
mod session;
mod transport;

pub(crate) use display::image_dimensions;

/// The kernel server script, shipped inside the binary.
const PY_KERNEL_SERVER: &str = include_str!("eval/py_kernel_server.py");
const PY_BROWSER_BOOTSTRAP: &str = include_str!("eval/python_browser.py");
const PY_AGENT_RECOVERY: &str = include_str!("eval/python_agent_recovery.py");
const PY_WORKPOOL_PIPELINE: &str = include_str!("eval/python_workpool_pipeline.py");

/// Default per-cell budget.
const DEFAULT_CELL_TIMEOUT_SECS: u64 = 30;

/// Bound outstanding native console frames independently of capture storage.
const JS_OUTPUT_QUEUE: usize = 8;

struct PyKernel {
    child: ProcessGuard,
    stdin: Option<ChildStdin>,
    // One response may arrive before the input writer receipt is scheduled.
    pending_line: Option<String>,
    /// Lines from the kernel's stdout, streamed by a dedicated reader thread
    /// (a cell may emit several bridge-request lines before its final
    /// response). Disconnection is EOF; protocol read failures are explicit.
    lines: std::sync::Mutex<std::sync::mpsc::Receiver<std::result::Result<String, String>>>,
    // Revoke both idle reads and a full response queue before process cleanup.
    reader_stop: transport::ReaderStop,
    next_id: u64,
    cells_run: u64,
    runtime: python_runtime::Resolved,
    // Retired after the process tree, including on reset/eviction/failure.
    // Also holds the bootstrap script so Windows command-line limits cannot
    // be exceeded as the embedded kernel grows.
    _packages: tempfile::TempDir,
}

impl PyKernel {
    fn spawn(runtime: python_runtime::Resolved, cwd: &Path) -> Result<Self> {
        let packages = tempfile::Builder::new()
            .prefix("pi-eval-python-")
            .tempdir()
            .map_err(|err| Error::tool("eval", format!("EVAL_PACKAGE_STORAGE: {err}")))?;
        let script = packages.path().join("kernel.py");
        std::fs::write(&script, PY_KERNEL_SERVER)
            .map_err(|err| Error::tool("eval", format!("EVAL_PACKAGE_STORAGE: {err}")))?;
        let bootstrap = packages.path().join("browser.py");
        std::fs::write(&bootstrap, PY_BROWSER_BOOTSTRAP)
            .map_err(|err| Error::tool("eval", format!("EVAL_PACKAGE_STORAGE: {err}")))?;
        let recovery = packages.path().join("agent_recovery.py");
        std::fs::write(&recovery, PY_AGENT_RECOVERY)
            .map_err(|err| Error::tool("eval", format!("EVAL_PACKAGE_STORAGE: {err}")))?;
        let pipeline = packages.path().join("workpool_pipeline.py");
        std::fs::write(&pipeline, PY_WORKPOOL_PIPELINE)
            .map_err(|err| Error::tool("eval", format!("EVAL_PACKAGE_STORAGE: {err}")))?;
        let mut command = runtime.command();
        command
            // Keep -c's workspace import semantics; only the large bootstrap
            // moves to owned storage, not the kernel's default sys.path.
            .args([
                "-c",
                "import sys\nwith open(sys.argv[1], encoding='utf-8') as source:\n code = source.read()\nexec(compile(code, sys.argv[1], 'exec'))",
            ])
            .arg(&bootstrap)
            .arg(&script)
            .arg(&recovery)
            .arg(&pipeline)
            .env("_PI_EVAL_PACKAGE_ROOT", packages.path())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Own process group so shutdown kills kernel-spawned children too
        // (session-end tree discipline, bd-cv653.1.4 acceptance #5).
        crate::tools::isolate_command_process_group(&mut command);
        let child =
            crate::tools::spawn_command_with_job_discipline(&mut command).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Error::tool(
                        "eval",
                        format!(
                            "EVAL_PY_MISSING: selected interpreter {} disappeared. \
                             No fallback after selection; retry explicitly or set PI_EVAL_PYTHON.",
                            runtime.executable.display()
                        ),
                    )
                } else {
                    Error::tool("eval", format!("EVAL_SPAWN: {err}"))
                }
            })?;
        let mut child = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
        let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
            let _ = child.terminate_and_wait();
            return Err(Error::tool("eval", "EVAL_SPAWN: kernel pipes unavailable"));
        };
        // Keep the child guarded through all fallible pipe/worker setup. Only
        // parent endpoints become nonblocking; input delivery already owns a
        // revocable write lease and now retries backpressure under that lease.
        #[cfg(unix)]
        transport::prepare_pipe(&stdin).map_err(|err| {
            let _ = child.terminate_and_wait();
            Error::tool("eval", format!("EVAL_SPAWN: prepare kernel stdin: {err}"))
        })?;
        let (lines, reader_stop) = transport::spawn_reader(stdout).map_err(|err| {
            let _ = child.terminate_and_wait();
            Error::tool("eval", format!("EVAL_SPAWN: {err}"))
        })?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            pending_line: None,
            lines: std::sync::Mutex::new(lines),
            reader_stop,
            next_id: 1,
            cells_run: 0,
            runtime,
            _packages: packages,
        })
    }

    /// Await the next stdout line under a budget. Ok(None) = EOF.
    ///
    /// Takes `&mut self` deliberately: `mpsc::Receiver` is `!Sync`, so a
    /// shared borrow held across the await would make the future `!Send`.
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn next_line(
        &mut self,
        deadline: Instant,
        capture: &mut output::Capture,
        session_guard: &session::CellSession,
    ) -> Result<Option<String>> {
        loop {
            session_guard.check_tool().await?;
            capture.publish(false);
            if let Some(line) = self.pending_line.take() {
                return Ok(Some(line));
            }
            let received = self
                .lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv();
            match received {
                Ok(Ok(line)) => return Ok(Some(line)),
                Ok(Err(message)) => return Err(Error::tool("eval", message)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if Instant::now() > deadline {
                        return Err(Error::tool("eval", "EVAL_TIMEOUT: cell budget exhausted"));
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(25),
                    )
                    .await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
            }
        }
    }

    fn kill(&mut self) {
        // An escaped descendant may retain a pipe endpoint, and the response
        // queue may be full. Revoke reads and disconnect the owned receiver
        // before waiting for any process. Exclusive kernel ownership makes
        // get_mut nonblocking; no live-cell receiver lock is needed here.
        self.reader_stop.stop_receiving(
            self.lines
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // The guard releases its child after reaping, so repeated cleanup
        // cannot signal a recycled PID or replace the kernel's Windows Job.
        let _ = self.child.terminate_and_wait();
    }
}

impl Drop for PyKernel {
    fn drop(&mut self) {
        // Session-end process discipline: no orphan kernels.
        self.kill();
    }
}

pub struct EvalTool {
    cwd: PathBuf,
    python_path: std::ffi::OsString,
    python_environment: python_runtime::Environment,
    kernel: Mutex<Option<PyKernel>>,
    js: Mutex<Option<js_kernel::JsKernel>>,
    // Registry snapshots share this resolver. The active slots above belong
    // only to the selected session; other retained namespaces are bounded.
    session_scope: crate::jobs::JobSessionScope,
    cost_scope: crate::cost::CostScope,
    retained_sessions: Mutex<session::RetainedSessions>,
    // Slot mutexes protect storage, not execution: each runner takes its
    // kernel out across awaits. Keep this lease until it returns or drops.
    execution: Arc<futures::lock::Mutex<()>>,
    // Fixed host binding only; invocation privacy is always borrowed per cell.
    judge: Option<crate::judgment::Judge>,
    // Capture host storage policy with the tool, not from a later eval cell.
    completion_artifacts: PathBuf,
}

impl EvalTool {
    pub fn new(cwd: &Path) -> Self {
        let (python_path, python_environment) = python_runtime::Environment::capture();
        Self {
            cwd: cwd.to_path_buf(),
            python_path,
            python_environment,
            kernel: Mutex::new(None),
            js: Mutex::new(None),
            session_scope: crate::jobs::JobSessionScope::default(),
            cost_scope: crate::cost::CostScope::default(),
            retained_sessions: Mutex::new(session::RetainedSessions::default()),
            execution: Arc::new(futures::lock::Mutex::new(())),
            judge: None,
            completion_artifacts: crate::config::Config::global_dir().join("tool-output-artifacts"),
        }
    }

    /// Bind eval judgments and completions to a host-owned provider and options.
    /// Cells cannot override this binding or access its credentials. Ordinary
    /// eval instances require an explicit judge_model on each requesting cell.
    #[must_use]
    pub fn with_judge(mut self, judge: crate::judgment::Judge) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Run one JavaScript cell on the persistent QuickJS kernel. Both native
    /// console output and bridge requests are serviced by the owning host.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn run_js_cell(
        &self,
        code: &str,
        filename: Option<&str>,
        timeout: Duration,
        capture: &mut output::Capture,
        session_guard: &session::CellSession,
        judgments: &mut judgment::Invocation<'_>,
    ) -> Result<ToolOutput> {
        let kernel = {
            let mut slot = self
                .js
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.take()
        };
        let mut restarted = false;
        let mut kernel = if let Some(kernel) = kernel {
            kernel
        } else {
            restarted = true;
            js_kernel::JsKernel::spawn_in(&self.cwd)
                .map_err(|err| Error::tool("eval", format!("EVAL_SPAWN: {err}")))?
        };
        let deadline = Instant::now() + timeout;
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel(JS_OUTPUT_QUEUE);
        let reply_rx = kernel
            .submit_source_with_output(
                code.to_string(),
                filename.map(str::to_owned),
                deadline,
                Some(output_tx),
            )
            .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;

        let mut response = loop {
            owner.checkpoint().map_err(|_| {
                Error::tool(
                    "eval",
                    "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                )
            })?;
            session_guard.check_tool().await?;
            let received = drain_js_console(&output_rx, capture)?;
            capture.publish(false);
            // Callbacks may cancel or switch sessions. Observe both before
            // admitting a bridge, even with a continuously ready producer.
            owner.checkpoint().map_err(|_| {
                Error::tool(
                    "eval",
                    "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                )
            })?;
            session_guard.check_tool().await?;
            if let Ok(bridge) = kernel.bridge.try_recv() {
                // At most one queue's worth of frames can precede this bridge.
                // Publish them before entering a potentially slow native tool.
                drain_js_console(&output_rx, capture)?;
                capture.publish(true);
                owner.checkpoint().map_err(|_| {
                    Error::tool(
                        "eval",
                        "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                    )
                })?;
                session_guard.check_tool().await?;
                let mut pending = Box::pin(judgments.dispatch(self, &bridge.tool, bridge.input));
                let reply = loop {
                    // An async JS timer can print while its tool request is in
                    // flight. Keep draining instead of blocking that producer.
                    session_guard.check_tool().await?;
                    drain_js_console(&output_rx, capture)?;
                    capture.publish(false);
                    owner.checkpoint().map_err(|_| {
                        Error::tool(
                            "eval",
                            "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                        )
                    })?;
                    session_guard.check_tool().await?;
                    let sleeper = Box::pin(asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(15),
                    ));
                    match futures::future::select(pending, sleeper).await {
                        futures::future::Either::Left((reply, _)) => break reply,
                        futures::future::Either::Right(((), operation)) => pending = operation,
                    }
                };
                session_guard.check_tool().await?;
                let _ = bridge.reply.send(reply);
            }
            match reply_rx.try_recv() {
                Ok(response) => break response,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if Instant::now() > deadline + Duration::from_secs(5) {
                        return Err(Error::tool(
                            "eval",
                            "EVAL_TIMEOUT: js kernel wedged past its budget; kernel \
                             discarded — state was lost, next cell starts fresh",
                        ));
                    }
                    // Bound each drain batch and yield even under continuous
                    // output/fast bridges so the whole-cell watchdog can run.
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(if received == 0 { 15 } else { 1 }),
                    )
                    .await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::tool(
                        "eval",
                        "EVAL_KERNEL_CRASH: the js kernel exited mid-cell — state was \
                         lost, next cell starts fresh",
                    ));
                }
            }
        };
        // The native sink closes before this reply. Drain all remaining frames
        // (bounded by queue capacity), then assemble console output only once.
        drain_js_console(&output_rx, capture)?;
        capture.finish_javascript(&mut response)?;
        kernel.cells_run += 1;
        let cells_run = kernel.cells_run;
        // Reject the entire rich result before making this realm reusable.
        // On malformed output, dropping the owned kernel revokes its worker.
        let mut output = display::javascript_response(&response, restarted, cells_run)?;
        capture.annotate(&mut output);
        session_guard.check_tool().await?;
        capture.publish(true);
        {
            let mut slot = self
                .js
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(kernel);
        }
        Ok(output)
    }

    /// Bridge dispatch shared by both kernels.
    async fn run_bridge_call_parts(
        &self,
        tool_name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        // Result projection is an operation on the existing bridge, not a
        // second dispatch path or authority to call additional tools.
        let request = bridge_result::Request::parse(tool_name, input)?;
        let tool: Box<dyn Tool> = match request.name.as_str() {
            "read" => Box::new(crate::tools::ReadTool::new(&self.cwd)),
            "grep" => Box::new(crate::tools::GrepTool::new(&self.cwd)),
            "find" => Box::new(crate::tools::FindTool::new(&self.cwd)),
            "ls" => Box::new(crate::tools::LsTool::new(&self.cwd)),
            other => {
                return Err(format!(
                    "EVAL_BRIDGE_DENIED: tool `{other}` is not on the bridge whitelist (read|grep|find|ls)"
                ));
            }
        };
        match tool.execute("eval-bridge", request.args, None).await {
            Ok(output) if request.structured => bridge_result::structured(&output),
            Ok(output) => bridge_result::text(&output),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Run one cell: writes the request line, then reads the response on a
    /// blocking thread while this async fn polls with the cell budget. On
    /// timeout the kernel is killed (state loss) and the next cell restarts.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn run_py_cell(
        &self,
        code: &str,
        timeout: Duration,
        soft_timeout_secs: Option<u64>,
        capture: &mut output::Capture,
        session_guard: &session::CellSession,
        judgments: &mut judgment::Invocation<'_>,
    ) -> Result<ToolOutput> {
        let deadline = Instant::now() + timeout;
        // Take the kernel out (or spawn) so the mutex is not held across await.
        let taken = self
            .kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut restarted = taken.is_none();
        let mut kernel = match taken {
            Some(kernel) => kernel,
            None => self.spawn_python(deadline, session_guard).await?,
        };

        // An observed idle exit can be replaced before request delivery. A
        // write failure is ambiguous and must never replay application code.
        if kernel
            .child
            .try_wait_child()
            .map_err(|error| Error::tool("eval", format!("EVAL_IO: {error}")))?
            .is_some()
        {
            kernel.kill();
            kernel = self.spawn_python(deadline, session_guard).await?;
            restarted = true;
        }

        let id = kernel.next_id;
        kernel.next_id += 1;
        let request = json!({"id": id, "code": code, "softTimeoutSecs": soft_timeout_secs});
        bridge_io::delivery::send_python(
            &mut kernel,
            id,
            deadline,
            capture,
            session_guard,
            &request,
        )
        .await?;

        // Read lines until the final cell response; bridge requests re-enter
        // pi tools mid-cell and their results resume the kernel.
        let (final_line, soft_timed_out) = loop {
            // A continuously readable stream may not yield Pending to the
            // enclosing watchdog. Observe its captured owner here too.
            crate::agent_cx::AgentCx::for_current_or_request()
                .checkpoint()
                .map_err(|_| {
                    Error::tool(
                        "eval",
                        "EVAL_CANCELLED; EVAL_STATE_RESET: Python kernel discarded",
                    )
                })?;
            session_guard.check_tool().await?;
            // Deadline check up front: a cell looping on bridge calls
            // keeps lines flowing, so the Empty-branch check inside
            // next_line alone would never fire and the cell could run
            // forever.
            if Instant::now() > deadline {
                kernel.kill();
                return Err(Error::tool(
                    "eval",
                    format!(
                        "EVAL_TIMEOUT: cell exceeded {}s; kernel discarded —                          state was lost, next cell starts fresh",
                        timeout.as_secs()
                    ),
                ));
            }
            let line = match kernel.next_line(deadline, capture, session_guard).await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    // EOF: crashed mid-cell (e.g. os._exit). State lost.
                    kernel.kill();
                    return Err(Error::tool(
                        "eval",
                        "EVAL_KERNEL_CRASH: the Python kernel exited mid-cell — state was \
                         lost, next cell starts fresh",
                    ));
                }
                Err(error) => {
                    // Transport failures are not deadlines. Keep their named
                    // diagnostic and retire this connection in either case.
                    kernel.kill();
                    return Err(Error::tool(
                        "eval",
                        format!(
                            "{error}; EVAL_STATE_RESET: Python kernel discarded — \
                             state was lost, next cell starts fresh"
                        ),
                    ));
                }
            };
            session_guard.check_tool().await?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut parsed: Value = match serde_json::from_str(trimmed) {
                Ok(parsed) => parsed,
                Err(err) => {
                    kernel.kill();
                    return Err(Error::tool(
                        "eval",
                        format!(
                            "EVAL_PROTOCOL: non-protocol output on the kernel channel                              ({err}); kernel discarded — state was lost, next cell                              starts fresh"
                        ),
                    ));
                }
            };
            if parsed.get("stream").is_some() {
                capture.accept(&parsed, id)?;
                capture.publish(false);
                continue;
            }
            if let Some(bridge) = parsed.get("bridge") {
                transport::validate_bridge_owner(&parsed, id, line.len())?;
                capture.publish(true);
                session_guard.check_tool().await?;
                let reply = bridge_io::wait_python(
                    &kernel.lines,
                    id,
                    deadline,
                    capture,
                    session_guard,
                    self.run_bridge_call(bridge, judgments),
                )
                .await?;
                session_guard.check_tool().await?;
                bridge_io::delivery::send_python(
                    &mut kernel,
                    id,
                    deadline,
                    capture,
                    session_guard,
                    &json!({"bridge_result": reply}),
                )
                .await?;
                continue;
            }
            transport::validate_response(&parsed, id)?;
            let soft_timed_out = parsed
                .get("softTimeout")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if soft_timed_out && soft_timeout_secs.is_none() {
                return Err(Error::tool(
                    "eval",
                    "EVAL_PROTOCOL: unsolicited soft timeout; EVAL_STATE_RESET: Python kernel discarded",
                ));
            }
            capture.finish_response(&mut parsed)?;
            break (serde_json::to_string(&parsed)?, soft_timed_out);
        };
        kernel.cells_run += 1;
        let cells_run = kernel.cells_run;

        // Validate all rich output before making the kernel reusable. A
        // malformed protocol result discards this connection, not just its
        // bad frame, and cannot partially publish a successful tool result.
        let mut output = format_cell_response(&final_line, restarted, cells_run)?;
        kernel.runtime.annotate(&mut output);
        if soft_timed_out && let Some(Value::Object(details)) = output.details.as_mut() {
            details.insert("errorKind".into(), json!("soft_timeout"));
            details.insert("softTimeout".into(), json!(true));
            details.insert("stateReset".into(), json!(false));
        }
        capture.annotate(&mut output);
        session_guard.check_tool().await?;
        capture.publish(true);

        // Return the kernel to the slot for the next cell.
        {
            let mut slot = self
                .kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(kernel);
        }

        Ok(output)
    }

    async fn spawn_python(
        &self,
        deadline: Instant,
        session: &session::CellSession,
    ) -> Result<PyKernel> {
        let runtime = self
            .python_environment
            .resolve(&self.python_path, &self.cwd, deadline, session)
            .await?;
        PyKernel::spawn(runtime, &self.cwd)
    }

    /// Execute one whitelisted bridge call through the SAME tool
    /// implementations a direct call uses — identical path policy.
    async fn run_bridge_call(
        &self,
        bridge: &Value,
        judgments: &mut judgment::Invocation<'_>,
    ) -> Value {
        let call = bridge.get("call").cloned().unwrap_or(Value::Null);
        let tool_name = bridge.get("tool").and_then(Value::as_str).unwrap_or("");
        let input = bridge.get("input").cloned().unwrap_or_else(|| json!({}));
        match judgments.dispatch(self, tool_name, input).await {
            Ok(content) => json!({"call": call, "ok": true, "content": content}),
            Err(error) => json!({"call": call, "ok": false, "error": error}),
        }
    }
}

/// Consume no more than one bounded queue in a poll. Callback delivery stays
/// outside this loop and outside every native mutex/QuickJS RefCell borrow.
fn drain_js_console(
    receiver: &std::sync::mpsc::Receiver<Value>,
    capture: &mut output::Capture,
) -> Result<usize> {
    let mut received = 0;
    for _ in 0..JS_OUTPUT_QUEUE {
        match receiver.try_recv() {
            Ok(frame) => {
                capture.accept(&frame, 1)?;
                received += 1;
            }
            Err(
                std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected,
            ) => break,
        }
    }
    Ok(received)
}

/// Turn a kernel protocol response line into the tool output contract.
fn format_cell_response(line: &str, restarted: bool, cells_run: u64) -> Result<ToolOutput> {
    display::python_response(line, restarted, cells_run)
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for EvalTool {
    fn bind_cost_scope(&mut self, scope: crate::cost::CostScope) {
        self.cost_scope = scope;
    }

    fn name(&self) -> &str {
        "eval"
    }

    fn label(&self) -> &str {
        "Eval"
    }

    #[allow(clippy::too_many_lines)] // one long model-facing tool description literal
    fn description(&self) -> &str {
        "Run a Python or JavaScript cell in a persistent kernel. Cells execute \
         one at a time within this session and return the final expression. \
         Set kernel_mode=per_call for a fresh private kernel, retired after \
         this call without reading or resetting either session kernel. \
         Set reset=true to discard the selected language's state before this \
         cell; the other language is unaffected. Timeouts or crashes report \
         state loss explicitly. POSIX Python may set soft_timeout_secs below \
         timeout_secs to attempt interruption while retaining variables. The \
         hard deadline remains unchanged; unsafe or wedged kernels are discarded. \
         Python discovers active and project virtual environments before PATH; \
         PI_EVAL_PYTHON explicitly selects an interpreter. Selection is retained \
         with the kernel and reported in pythonRuntime result details. \
         Rich output: Python display(value, mime=...) \
         or JS display(value, mimeType) returns JSON, text, Markdown or PNG. \
         PNG input is bytes/base64 (JS Uint8Array/base64), never a path or URL. \
         Both kernels stream stdout/stderr live; large console output retains a \
         labelled bounded head/tail rather than filling session memory. \
         Python %pip install --managed <packages> installs into private \
         kernel-local storage without losing variables or changing system \
         Python. Reset discards managed packages; installation time counts \
         against timeout_secs. Plain %pip install keeps normal pip behavior. \
         Set output_path to a new workspace-relative .jsonl file to retain \
         complete received console frames on disk (64 MiB encoded limit). \
         JavaScript can await import('./helper.js') for local .js/.mjs ES modules \
         inside this workspace; imports in %load scripts resolve beside the script. \
         Module state is cached until reset. No package, network or Node imports. \
         In either language await tool.call(name, args) calls an enabled Agent \
         tool and returns text; await tool.result(name, args) preserves native \
         content blocks, details and isError. Every nested call uses the current \
         approval, plan, extension-hook and secrets policies. At most 64 host \
         calls per cell, all within this cell's deadline; eval and xdev cannot \
         be called recursively. Call discoverable tools by their actual names. \
         Standalone embeddings retain only the local read/grep/find/ls bridge. \
         Both kernels expose browser handles over the enabled native browser \
         tool: JS await browser.open({name,url}), Python await browser.open(name=...,url=...). \
         Use browser.tab(name), explicit frame handles and locator(selector); \
         Python get_by_label/get_by_role/wait_for/drag_to correspond to JS \
         getByLabel/getByRole/waitFor/dragTo. Await tab/locator actions and \
         tab.batch for native sequences. Results keep content and details; \
         failed actions raise with the native partial result, never replay. \
         When subagent and jobs are enabled, await agent(prompt, options) in JS \
         or await agent(prompt, **options) in Python starts a native background \
         subagent. Retain its handle across cells to check status, read output, \
         wait or cancel; await wait(handles) returns ordered native results. \
         Set keepAlive:true in JS or keep_alive=True in Python to retain one \
         live child conversation; await worker.send(prompt) queues a follow-up \
         and returns a handle pinned to that turn. await worker.close() cancels \
         its running and queued turns. JS agent.attachWorker(id) or Python \
         agent.attach_worker(id) recovers an existing worker without launching \
         or replaying work; worker.turn(id) selects an existing turn. Recovered \
         handles disable new sends unless allowSend:true / allow_send=True is \
         explicit. An uncertain send acknowledgment disables further sends. \
         await workpool({agent,concurrency}) in JS \
         or await workpool(agent=...,concurrency=...) in Python creates a lazy \
         native pool; await pool.push([prompts]) atomically admits a batch and \
         returns fixed item handles. One session job owns 1-8 reusable workers. \
         pool.items retains handles; await pool.peek() or pool.status() returns \
         one bounded summary manifest. Use item.output() or item.wait() for full \
         results. pool.drain({cursor,timeout,maxItems}) in JS or \
         pool.drain(cursor=...,timeout=...,max_items=...) in Python returns up \
         to four finished full results without waiting for earlier items or \
         sealing the pool; save nextCursor only after processing the page. \
         JS workpool.mapReduce(source,map,reduce,initial,options) and Python \
         workpool.map_reduce(source,map,reduce,initial,**options) consume \
         iterables with bounded admission and reduce full results as they settle. \
         Put launch options in pool; errors retain pool, cursor and progress. \
         Idle workers prefer lower observed active context pressure. \
         await pool.finish() seals admission and returns the aggregate \
         job; await pool.wait() seals and waits for all accepted work to drain. \
         await pool.close() cancels running and queued work. Pools admit at most \
         32 items with fixed total lifetimes; split pushes above 256 KiB encoded. \
         Native results survive kernel reset; jobs list exposes recovery IDs. \
         await workpool.attach(poolId) recovers the existing pool and ordered \
         item handles without changing its deadline. New pushes remain disabled \
         unless allowPush:true / allow_push=True is explicit and the native pool \
         is open. Inspect accepted work before allowing new sends or pushes. \
         Await every pool command. Each operation \
         uses the current cell's host and policy. JS handles can be retained on \
         globalThis; all waits remain within the current cell's deadline. \
         Native call receipts are included in result details. For typed judgments, \
         set judge_model={provider,model} or {role:'smol'} for this cell, then call \
         __pi_judge through tool.call with {state,questions,images?} and parse its JSON. \
         Questions use bool/choice/score rubrics. images is an array of local paths \
         or {type:'image',data:base64,mimeType}; detail may only be omitted or auto. \
         PNG/JPEG/GIF/WebP require host/catalog-declared image support, at most \
         4 images, 4.5 MiB decoded per image and 8 MiB encoded per request. Paths \
         use the current Agent's enabled read tool, policy and host-call budget; \
         standalone bindings must embed images. Text privacy excludes image \
         pixels and metadata. For independent states call __pi_judge_batch through \
         tool.call with {states,questions,concurrency?,images?}. Batch images is \
         a map of exact state keys or zero-based array index strings to image \
         arrays, with a 16 MiB encoded total. All attachments are prepared before \
         any model request. \
         states is an array or keyed object of 1-16 states; concurrency is 1-8 \
         (default 4). Parse the JSON and check allOk or each items entry's ok \
         before using its verdict; ordinary item failures retain successful peers. \
         Single judgments and batch items share 16 reserved attempts per cell; \
         ordinary batches keep the original cell deadline. \
         No model fallback or tools within a judgment request. \
         General text-only one-shot completion uses __pi_complete through tool.call \
         with {prompt,system?,max_tokens?,schema?}, returning JSON with text and \
         usage. Optional schema is a bounded object/boolean JSON Schema; valid \
         terminal JSON also returns data and schemaValid=true. Use data for \
         typed extraction. Invalid schemas fail before model admission; bad \
         output, duplicate keys and prose wrappers fail without repair/retry. \
         External schema resources are refused; privacy must preserve the schema. \
         It uses the same judge_model/host binding, privacy and original cell \
         deadline; max_tokens may only lower the host ceiling (at most 4096). \
         Completions and judgments share the 16-attempt allowance. No history \
         or tools are passed to these completions. For retained model work use \
         await completion(prompt, options), await judge(state, questions, options), \
         or await judgeBatch(states, questions, options) in JS; Python uses \
         keyword options and judge_batch. Retain the returned handle, then use \
         wait(timeoutSeconds), output(), status(), done() or cancel() in a later \
         cell. Completion wait returns text or schema data; judge wait returns \
         answers; batch wait returns the aggregate. Mixed global wait accepts \
         these handles too. Use output() or wait(timeout,false) in JS / \
         wait(timeout,raise_errors=False) in Python to inspect failures. \
         Batch handles also provide progress(): ordered completed verdicts, \
         per-item errors, queued/running states and counts. Cancellation or \
         task timeout retains the last screened progress with complete=false; \
         it never turns unfinished items into verdicts or retries them. \
         completion.attach(id), judge.attach(id) and judgeBatch.attach(id) / \
         judge_batch.attach(id) recover known native jobs after reset. JS options \
         use maxTokens/timeoutSeconds; Python uses max_tokens/timeout_seconds. \
         In an explicitly admitted judge_model cell, model:'smol' or model:'slow' \
         (Python model='smol') selects a configured native modelRoles entry for \
         one completion, judgment or batch. All native role names are accepted. \
         Each selected role resolves once per cell from trusted settings, with \
         its configured thinking level; missing/disabled roles fail without \
         fallback. Omission uses the cell binding. SDK host-bound Judges cannot \
         be overridden. Retained batches accept retries (0-3, default 0) and \
         minOk in JS / min_ok in Python (0 through item count, default all). \
         Every possible retry reserves one of the launching cell's 16 attempts. \
         Only malformed answers or transient provider failures retry, after untouched peers; \
         successful peers never rerun. Retries retain the original job deadline. \
         Batch allOk remains true only when every item succeeded; meetsMinimum \
         separately controls aggregate success without skipping remaining items. The native \
         bridge for explicitly retained work is available through tool.call: \
         call __pi_model_complete_start, __pi_judge_start or __pi_judge_batch_start with \
         the same fields and optional timeout_secs (1-600; default 60 for single \
         calls and 300 for batches). These require the current Agent host and \
         enabled jobs controls. All attachments, schemas and text privacy are \
         prepared in the launching cell before any model request. The JSON \
         acknowledgment contains jobId, operation and timeoutSeconds. Use jobs \
         wait/cancel with jobId; completed results and attempt receipts are in \
         the job's result.details. A mixed failed batch keeps ordered peer \
         results with allOk=false. Running batches expose explicit progress \
         wrappers; interrupted batches retain accepted partial evidence under \
         an error wrapper, separate from the final model-result schema. \
         Accepted jobs retain their selected model, \
         prepared inputs and fixed deadline across cell completion or kernel \
         reset; they never retain a tool host or replay failed work. Native \
         attempt/usage receipts are included in result details. The separate \
         __pi_complete_start operation accepts ordinary completion fields and \
         returns a pi.completion.background.v1 acknowledgment with a jobId and \
         fixed 60-second budget. Its settled job contains the plain completion \
         envelope; use jobs directly for those handles. All admitted jobs \
         survive kernel reset and stop during session shutdown."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Source for the selected language; empty code is allowed for reset-only calls"
                },
                "kernel": {
                    "type": "string",
                    "enum": ["python", "js"],
                    "description": "Kernel to use (default python)"
                },
                "kernel_mode": {
                    "type": "string",
                    "enum": ["session", "per_call"],
                    "default": "session",
                    "description": "session retains interpreter state; per_call starts fresh and retires it after the call. Neither mode isolates filesystem/process effects."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Active cell budget in seconds (default 30, clamped to 1–600); excludes waiting for another cell"
                },
                "soft_timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 599,
                    "description": "Optional POSIX Python soft deadline, strictly below timeout_secs. Attempts to retain state after interruption; does not extend the hard deadline. Reserves SIGALRM; interrupted tool exchanges still discard the kernel."
                },
                "reset": {
                    "type": "boolean",
                    "default": false,
                    "description": "Discard only the selected language's retained state before executing code"
                },
                "judge_model": {
                    "type": "object",
                    "description": "Explicit model admission and default binding for this cell's judgment and schema-validated completion calls, including __pi_judge, __pi_judge_batch, __pi_complete and their retained start operations. Supply provider/model or a role configured in trusted modelRoles settings; role specs preserve their thinking suffix. Each helper may select another configured role with model. Only native registry credentials are used; missing/disabled roles fail without fallback. Omit when the host binds a Judge, which helpers cannot override. Requests share 16 reserved attempts per launching cell. Retained jobs freeze their selected binding and timeout; later cells must opt in again for new requests.",
                    "oneOf": [
                        {
                            "properties": {
                                "provider": {"type":"string", "minLength":1, "maxLength":256},
                                "model": {"type":"string", "minLength":1, "maxLength":256}
                            },
                            "required": ["provider", "model"],
                            "additionalProperties": false
                        },
                        {
                            "properties": {
                                "role": {"type":"string", "enum":["default","smol","slow","plan","commit","vision","designer","task","advisor","tiny"]}
                            },
                            "required": ["role"],
                            "additionalProperties": false
                        }
                    ]
                },
                "output_path": {
                    "type": "string",
                    "description": "Optional new workspace-relative .jsonl console archive; create-only, up to 64 MiB encoded. Validated before reset/execution. No file is published for an incomplete run."
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        self.execute_with_mode(input, on_update, None, None, None)
            .await
    }

    async fn execute_with_auxiliary_privacy(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        protect: &(dyn for<'a, 'b> Fn(&'a [&'b str]) -> Result<Vec<String>> + Send + Sync),
    ) -> Result<ToolOutput> {
        self.execute_with_mode(input, on_update, Some(protect), None, None)
            .await
    }

    async fn execute_with_host(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        protect: &(dyn for<'a, 'b> Fn(&'a [&'b str]) -> Result<Vec<String>> + Send + Sync),
        host: &dyn crate::tools::ToolInvocationHost,
    ) -> Result<ToolOutput> {
        self.execute_with_mode(input, on_update, Some(protect), Some(host), None)
            .await
    }

    async fn execute_with_invocation(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        context: crate::tools::ToolInvocationContext<'_>,
    ) -> Result<ToolOutput> {
        self.execute_with_mode(
            input,
            on_update,
            Some(context.protect),
            context.host,
            Some(context.snapshot_privacy),
        )
        .await
    }

    fn effects(&self) -> ToolEffects {
        // Arbitrary code: process-level effects, serialized fail-closed.
        ToolEffects::process()
    }

    fn effects_for_input(&self, input: &Value) -> ToolEffects {
        if self.judge.is_some() || input.get("judge_model").is_some() {
            ToolEffects::process().union(ToolEffects::network())
        } else {
            self.effects()
        }
    }

    fn bind_job_session_scope(&mut self, scope: crate::jobs::JobSessionScope) {
        self.bind_eval_session_scope(scope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn run_cell_sync(tool: &EvalTool, code: &str) -> Result<ToolOutput> {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        runtime.block_on(tool.execute("t", json!({"code": code}), None))
    }

    fn output_text(output: &ToolOutput) -> &str {
        match &output.content[0] {
            // ubs:ignore test index — single-block output is the assertion
            ContentBlock::Text(text) => &text.text,
            other => panic!("unexpected block: {other:?}"), // ubs:ignore test assertion panic
        }
    }

    #[test]
    fn state_persists_across_cells_and_last_expression_returns() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_cell_sync(&tool, "x = 40 + 1").expect("cell 1");
        assert!(!out.is_error, "cell 1: {}", output_text(&out));
        let out = run_cell_sync(&tool, "x += 1\nx").expect("cell 2");
        assert!(!out.is_error);
        assert!(
            output_text(&out).contains("42"),
            "got: {}",
            output_text(&out)
        );
        // Imports persist too.
        let out = run_cell_sync(&tool, "import math").expect("cell 3");
        assert!(!out.is_error);
        let out = run_cell_sync(&tool, "int(math.sqrt(x * 0 + 49))").expect("cell 4");
        assert!(
            output_text(&out).contains('7'),
            "got: {}",
            output_text(&out)
        );
    }

    #[test]
    fn stdout_and_exceptions_are_captured() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_cell_sync(&tool, "print('hello-eval')").expect("print cell");
        assert!(output_text(&out).contains("hello-eval"));
        let out = run_cell_sync(&tool, "1 / 0").expect("exception cell returns output");
        assert!(out.is_error);
        assert!(output_text(&out).contains("ZeroDivisionError"));
        // The kernel survives an exception: state still works.
        let out = run_cell_sync(&tool, "'alive'").expect("after exception");
        assert!(!out.is_error);
        assert!(output_text(&out).contains("alive"));
    }

    #[test]
    fn session_end_kills_kernel_tree_no_orphans() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let pids: [u32; 2] = {
            let tool = EvalTool::new(dir.path());
            let out = run_cell_sync(
                &tool,
                "import os, sys, subprocess\nchild = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])",
            )
            .expect("spawn cell");
            assert!(!out.is_error, "cell: {}", output_text(&out));
            let out = run_cell_sync(&tool, "[os.getpid(), child.pid]").expect("pid cell");
            serde_json::from_str(output_text(&out).trim()).expect("kernel and descendant PIDs")
            // tool (and its kernel) drop here
        };
        // Probe the exact processes through the same portable API on Windows
        // and Unix. No external sleep/pgrep or Linux-only /proc oracle.
        for (index, pid) in pids.into_iter().enumerate() {
            let pid = sysinfo::Pid::from_u32(pid);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut system = sysinfo::System::new();
                system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                if system.process(pid).is_none_or(|process| {
                    index != 0 && cfg!(unix) && process.status() == sysinfo::ProcessStatus::Zombie
                }) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "kernel-owned process {pid} survived session end"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn kernel_crash_reports_state_loss_and_restarts() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_cell_sync(&tool, "y = 7").expect("seed");
        assert!(!out.is_error);
        let err = run_cell_sync(&tool, "import os\nos._exit(3)").expect_err("crash");
        assert!(err.to_string().contains("EVAL_KERNEL_CRASH"), "err: {err}");
        // Next cell auto-restarts with fresh state: y is gone.
        let out = run_cell_sync(&tool, "'y' in dir()").expect("restarted");
        assert!(
            output_text(&out).contains("False"),
            "state leaked: {}",
            output_text(&out)
        );
    }

    #[test]
    fn bridge_read_returns_file_content_and_denies_unknown_tools() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("data.txt"), "bridge-payload-77\n").expect("write fixture");
        let tool = EvalTool::new(dir.path());
        // tool.read from INSIDE Python returns the file content.
        let out = run_cell_sync(
            &tool,
            "content = tool.read('data.txt')\n'bridge-payload-77' in content",
        )
        .expect("bridge read");
        assert!(!out.is_error, "bridge read failed: {}", output_text(&out));
        assert!(
            output_text(&out).contains("True"),
            "got: {}",
            output_text(&out)
        );
        // Off-whitelist tools are denied host-side with the named taxonomy —
        // probed via the bridge internals (cells cannot spoof the bridge by
        // printing: cell stdout is captured, only the real stdout reaches
        // the host).
        let probe = concat!(
            "bridge = tool.read.__globals__['_bridge_call']\n",
            "try:\n",
            "    bridge('bash', {'command': 'true'})\n",
            "    verdict = 'allowed'\n",
            "except Exception as e:\n",
            "    verdict = str(e)\n",
            "verdict",
        );
        let out = run_cell_sync(&tool, probe).expect("denial probe");
        assert!(
            output_text(&out).contains("EVAL_BRIDGE_DENIED"),
            "bash escaped the whitelist: {}",
            output_text(&out)
        );
    }

    #[test]
    fn bridge_denies_paths_outside_workspace_like_direct_reads() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        // A path-escape read through the bridge must fail the same way a
        // direct ReadTool call would (same implementation = same policy).
        let out = run_cell_sync(
            &tool,
            "try:\n    tool.read('../../../../etc/hosts')\n    verdict = 'allowed'\nexcept Exception as e:\n    verdict = 'denied: ' + str(e)[:60]\nverdict",
        )
        .expect("escape probe");
        let text = output_text(&out);
        // ReadTool resolves relative paths against cwd; traversal outside is
        // permitted only if the direct tool permits it — assert parity by
        // running the direct tool and comparing verdicts.
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let direct = runtime.block_on(crate::tools::ReadTool::new(dir.path()).execute(
            "t",
            json!({"path": "../../../../etc/hosts"}),
            None,
        ));
        let direct_allowed = direct.is_ok_and(|o| !o.is_error);
        let bridge_allowed = text.contains("allowed");
        assert_eq!(
            bridge_allowed, direct_allowed,
            "bridge/direct policy divergence: bridge={text}"
        );
    }

    fn run_js_cell_sync(tool: &EvalTool, code: &str) -> Result<ToolOutput> {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        runtime.block_on(tool.execute("t", json!({"code": code, "kernel": "js"}), None))
    }

    #[test]
    fn js_state_persists_and_top_level_await_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_js_cell_sync(&tool, "const base = 40; let acc = base;").expect("cell 1");
        assert!(!out.is_error, "cell 1: {}", output_text(&out));
        let out = run_js_cell_sync(&tool, "acc += 2; acc").expect("cell 2");
        assert!(
            output_text(&out).contains("42"),
            "got: {}",
            output_text(&out)
        );
        // Top-level await settles via the job pump.
        let out = run_js_cell_sync(&tool, "await Promise.resolve(base + acc)").expect("await cell");
        assert!(!out.is_error, "await: {}", output_text(&out));
        assert!(
            output_text(&out).contains("82"),
            "got: {}",
            output_text(&out)
        );
    }

    #[test]
    fn js_console_capture_and_exception_survival() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_js_cell_sync(&tool, "console.log('js-hello', 1 + 1); 'done'")
            .expect("console cell");
        assert!(
            output_text(&out).contains("js-hello 2"),
            "got: {}",
            output_text(&out)
        );
        let out = run_js_cell_sync(&tool, "throw new Error('boom-js')").expect("throw cell");
        assert!(out.is_error);
        assert!(output_text(&out).contains("boom-js"));
        // Kernel (and state) survive the exception.
        let out = run_js_cell_sync(&tool, "globalThis.z = 9; z").expect("after throw");
        assert!(!out.is_error);
        assert!(output_text(&out).contains('9'));
    }

    #[test]
    fn js_bridge_read_and_whitelist_denial() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        std::fs::write(dir.path().join("data.txt"), "js-bridge-payload-88\n")
            .expect("write fixture");
        let out = run_js_cell_sync(
            &tool,
            "const c = tool.read('data.txt'); c.includes('js-bridge-payload-88')",
        )
        .expect("bridge read");
        assert!(!out.is_error, "bridge: {}", output_text(&out));
        assert!(
            output_text(&out).contains("true"),
            "got: {}",
            output_text(&out)
        );
        let out = run_js_cell_sync(
            &tool,
            "try { __pi_bridge('bash', '{}'); 'allowed' } catch (e) { String(e) }",
        )
        .expect("denial probe");
        assert!(
            output_text(&out).contains("EVAL_BRIDGE_DENIED"),
            "bash escaped: {}",
            output_text(&out)
        );
    }

    #[test]
    fn js_infinite_loop_times_out_and_discards_kernel_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let out = run_js_cell_sync(&tool, "globalThis.keep = 5").expect("seed");
        assert!(!out.is_error);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t",
                json!({"code": "for(;;){}", "kernel": "js", "timeout_secs": 2}),
                None,
            ))
            .expect("interrupted cell returns");
        assert!(out.is_error, "loop should abort: {}", output_text(&out));
        // Timed-out realms are retired so old async continuations cannot
        // execute under another cell's deadline (the async-host contract).
        assert!(output_text(&out).contains("EVAL_STATE_RESET"));
        let out = run_js_cell_sync(&tool, "typeof keep").expect("post-timeout");
        assert!(!out.is_error, "kernel died: {}", output_text(&out));
        assert!(
            output_text(&out).contains("undefined"),
            "timed-out state leaked: {}",
            output_text(&out)
        );
    }

    #[test]
    fn missing_python_is_named_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tool = EvalTool::new(dir.path());
        tool.python_path = "/nonexistent/python-binary".into();
        let err = run_cell_sync(&tool, "1").expect_err("should fail");
        assert!(err.to_string().contains("EVAL_PY_MISSING"), "err: {err}");
    }

    #[test]
    fn cell_timeout_is_named_and_bounded() {
        if !python_available() {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let started = Instant::now();
        let err = runtime
            .block_on(tool.execute(
                "t",
                json!({"code": "import time\ntime.sleep(60)", "timeout_secs": 2}),
                None,
            ))
            .expect_err("timeout");
        assert!(err.to_string().contains("EVAL_TIMEOUT"), "err: {err}");
        assert!(started.elapsed() < Duration::from_secs(20), "took too long");
    }
}
