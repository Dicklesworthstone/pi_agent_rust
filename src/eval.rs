//! Eval tool (bd-cv653.1.4): persistent code kernels with Jupyter-like cell
//! semantics — state persists across cells within a session.
//!
//! Python uses a retained subprocess and JavaScript a dedicated QuickJS realm.
//! Cells on one tool instance execute exclusively, including SDK callers that
//! bypass agent scheduling. Explicit reset discards only the selected language.
//! Timeout/crash state loss is reported; neither language silently substitutes
//! for the other. Both retain the host's read-only tool bridge policy.

use crate::error::{Error, Result};
use crate::model::ContentBlock;
use crate::tools::{ProcessCleanupMode, ProcessGuard, Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod display;
mod js_kernel;
mod lifecycle;
mod output;
mod session;
mod transport;

/// The kernel server script, shipped inside the binary.
const PY_KERNEL_SERVER: &str = include_str!("eval/py_kernel_server.py");

/// Default per-cell budget.
const DEFAULT_CELL_TIMEOUT_SECS: u64 = 30;

/// Bound outstanding native console frames independently of capture storage.
const JS_OUTPUT_QUEUE: usize = 8;

struct PyKernel {
    child: ProcessGuard,
    stdin: ChildStdin,
    /// Lines from the kernel's stdout, streamed by a dedicated reader thread
    /// (a cell may emit several bridge-request lines before its final
    /// response). Disconnection is EOF; protocol read failures are explicit.
    lines: std::sync::Mutex<std::sync::mpsc::Receiver<std::result::Result<String, String>>>,
    next_id: u64,
    cells_run: u64,
}

impl PyKernel {
    fn spawn(python_path: &str, cwd: &Path) -> Result<Self> {
        let mut command = Command::new(python_path);
        command
            .args(["-c", PY_KERNEL_SERVER])
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
                            "EVAL_PY_MISSING: `{python_path}` not found. Install Python 3 \
                             or set PI_EVAL_PYTHON."
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
        // Backpressure bounds complete queued frames independently of their
        // individual byte limit. Dropping the receiver unblocks the reader.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // Own the child before starting the reader. A thread-creation failure
        // must close the Job/process group and reap the kernel as well.
        let kernel = Self {
            child,
            stdin,
            lines: std::sync::Mutex::new(rx),
            next_id: 1,
            cells_run: 0,
        };
        std::thread::Builder::new()
            .name("eval-py-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match transport::read_frame(&mut reader) {
                        Ok(None) => return,
                        Err(error) => {
                            let _ = tx.send(Err(format!("EVAL_PROTOCOL_READ: {error}")));
                            return;
                        }
                        Ok(Some(line)) => {
                            if tx.send(Ok(line)).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|err| Error::tool("eval", format!("EVAL_SPAWN: {err}")))?;
        Ok(kernel)
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
    python_path: String,
    kernel: Mutex<Option<PyKernel>>,
    js: Mutex<Option<js_kernel::JsKernel>>,
    // Registry snapshots share this resolver. The active slots above belong
    // only to the selected session; other retained namespaces are bounded.
    session_scope: crate::jobs::JobSessionScope,
    retained_sessions: Mutex<session::RetainedSessions>,
    // Slot mutexes protect storage, not execution: each runner takes its
    // kernel out across awaits. Keep this lease until it returns or drops.
    execution: futures::lock::Mutex<()>,
}

impl EvalTool {
    pub fn new(cwd: &Path) -> Self {
        let python_path =
            std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| String::from("python3"));
        Self {
            cwd: cwd.to_path_buf(),
            python_path,
            kernel: Mutex::new(None),
            js: Mutex::new(None),
            session_scope: crate::jobs::JobSessionScope::default(),
            retained_sessions: Mutex::new(session::RetainedSessions::default()),
            execution: futures::lock::Mutex::new(()),
        }
    }

    /// Run one JavaScript cell on the persistent QuickJS kernel. Both native
    /// console output and bridge requests are serviced by the owning host.
    #[allow(clippy::too_many_lines)]
    async fn run_js_cell(
        &self,
        code: &str,
        filename: Option<&str>,
        timeout: Duration,
        capture: &mut output::Capture,
        session_guard: &session::CellSession,
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
            .submit_source_with_output(code.to_string(), filename.map(str::to_owned), deadline, Some(output_tx))
            .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;

        let mut response = loop {
            owner.checkpoint().map_err(|_| Error::tool(
                "eval", "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
            ))?;
            session_guard.check_tool().await?;
            let received = drain_js_console(&output_rx, capture)?;
            capture.publish(false);
            // Callbacks may cancel or switch sessions. Observe both before
            // admitting a bridge, even with a continuously ready producer.
            owner.checkpoint().map_err(|_| Error::tool(
                "eval", "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
            ))?;
            session_guard.check_tool().await?;
            if let Ok(bridge) = kernel.bridge.try_recv() {
                // At most one queue's worth of frames can precede this bridge.
                // Publish them before entering a potentially slow native tool.
                drain_js_console(&output_rx, capture)?;
                capture.publish(true);
                owner.checkpoint().map_err(|_| Error::tool(
                    "eval", "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                ))?;
                session_guard.check_tool().await?;
                let mut pending = Box::pin(self.run_bridge_call_parts(&bridge.tool, bridge.input));
                let reply = loop {
                    // An async JS timer can print while its tool request is in
                    // flight. Keep draining instead of blocking that producer.
                    session_guard.check_tool().await?;
                    drain_js_console(&output_rx, capture)?;
                    capture.publish(false);
                    owner.checkpoint().map_err(|_| Error::tool(
                        "eval", "EVAL_CANCELLED; EVAL_STATE_RESET: JavaScript kernel discarded",
                    ))?;
                    session_guard.check_tool().await?;
                    let sleeper = Box::pin(asupersync::time::sleep(
                        asupersync::time::wall_now(), Duration::from_millis(15),
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
                    ).await;
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
        let mut output = display::javascript_response(response, restarted, cells_run)?;
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
        let tool: Box<dyn Tool> = match tool_name {
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
        match tool.execute("eval-bridge", input, None).await {
            Ok(output) => {
                let mut text = String::new();
                for block in &output.content {
                    if let ContentBlock::Text(t) = block {
                        text.push_str(&t.text);
                    }
                }
                if output.is_error { Err(text) } else { Ok(text) }
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Run one cell: writes the request line, then reads the response on a
    /// blocking thread while this async fn polls with the cell budget. On
    /// timeout the kernel is killed (state loss) and the next cell restarts.
    #[allow(clippy::too_many_lines)]
    async fn run_py_cell(
        &self,
        code: &str,
        timeout: Duration,
        capture: &mut output::Capture,
        session_guard: &session::CellSession,
    ) -> Result<ToolOutput> {
        // Take the kernel out (or spawn) so the mutex is not held across await.
        let taken = self
            .kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut restarted = taken.is_none();
        let mut kernel = match taken {
            Some(kernel) => kernel,
            None => PyKernel::spawn(&self.python_path, &self.cwd)?,
        };

        let mut id = kernel.next_id;
        kernel.next_id += 1;
        let request = json!({"id": id, "code": code}).to_string();
        if kernel
            .stdin
            .write_all(format!("{request}\n").as_bytes())
            .and_then(|()| kernel.stdin.flush())
            .is_err()
        {
            // Kernel died between cells: restart once, transparently-but-loudly.
            kernel.kill();
            let mut fresh = PyKernel::spawn(&self.python_path, &self.cwd)?;
            id = fresh.next_id;
            fresh.next_id += 1;
            let request = json!({"id": id, "code": code}).to_string();
            fresh
                .stdin
                .write_all(format!("{request}\n").as_bytes())
                .and_then(|()| fresh.stdin.flush())
                .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;
            kernel = fresh;
            restarted = true;
        }

        // Read lines until the final cell response; bridge requests re-enter
        // pi tools mid-cell and their results resume the kernel.
        let deadline = Instant::now() + timeout;
        let final_line = loop {
            // A continuously readable stream may not yield Pending to the
            // enclosing watchdog. Observe its captured owner here too.
            crate::agent_cx::AgentCx::for_current_or_request().checkpoint().map_err(|_| {
                Error::tool("eval", "EVAL_CANCELLED; EVAL_STATE_RESET: Python kernel discarded")
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
                transport::validate_bridge(&parsed)?;
                capture.publish(true);
                session_guard.check_tool().await?;
                let reply = self.run_bridge_call(bridge).await;
                session_guard.check_tool().await?;
                let line = json!({"bridge_result": reply}).to_string();
                kernel
                    .stdin
                    .write_all(format!("{line}\n").as_bytes())
                    .and_then(|()| kernel.stdin.flush())
                    .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;
                continue;
            }
            transport::validate_response(&parsed, id)?;
            capture.finish_response(&mut parsed)?;
            break serde_json::to_string(&parsed)?;
        };
        kernel.cells_run += 1;
        let cells_run = kernel.cells_run;

        // Validate all rich output before making the kernel reusable. A
        // malformed protocol result discards this connection, not just its
        // bad frame, and cannot partially publish a successful tool result.
        let mut output = format_cell_response(&final_line, restarted, cells_run)?;
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

    /// Execute one whitelisted bridge call through the SAME tool
    /// implementations a direct call uses — identical path policy.
    async fn run_bridge_call(&self, bridge: &Value) -> Value {
        let call = bridge.get("call").cloned().unwrap_or(Value::Null);
        let tool_name = bridge.get("tool").and_then(Value::as_str).unwrap_or("");
        let input = bridge.get("input").cloned().unwrap_or_else(|| json!({}));
        match self.run_bridge_call_parts(tool_name, input).await {
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
            Err(std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected) => break,
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
    fn name(&self) -> &str {
        "eval"
    }

    fn label(&self) -> &str {
        "Eval"
    }

    fn description(&self) -> &str {
        "Run a Python or JavaScript cell in a persistent kernel. Cells execute \
         one at a time within this session and return the final expression. \
         Set reset=true to discard the selected language's state before this \
         cell; the other language is unaffected. Timeouts or crashes report \
         state loss explicitly. Rich output: Python display(value, mime=...) \
         or JS display(value, mimeType) returns JSON, text, Markdown or PNG. \
         PNG input is bytes/base64 (JS Uint8Array/base64), never a path or URL. \
         Both kernels stream stdout/stderr live; large console output retains a \
         labelled bounded head/tail rather than filling session memory. \
         Set output_path to a new workspace-relative .jsonl file to retain \
         complete received console frames on disk (64 MiB encoded limit). \
         JavaScript can await import('./helper.js') for local .js/.mjs ES modules \
         inside this workspace; imports in %load scripts resolve beside the script. \
         Module state is cached until reset. No package, network or Node imports."
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
                "timeout_secs": {
                    "type": "integer",
                    "description": "Active cell budget in seconds (default 30, clamped to 1–600); excludes waiting for another cell"
                },
                "reset": {
                    "type": "boolean",
                    "default": false,
                    "description": "Discard only the selected language's retained state before executing code"
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
        self.execute_cell(input, on_update).await
    }

    fn effects(&self) -> ToolEffects {
        // Arbitrary code: process-level effects, serialized fail-closed.
        ToolEffects::process()
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
        std::fs::write(dir.path().join("data.txt"), "js-bridge-payload-88\n")
            .expect("write fixture");
        let tool = EvalTool::new(dir.path());
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
        tool.python_path = String::from("/nonexistent/python-binary");
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
