//! Eval tool (bd-cv653.1.4): persistent code kernels with Jupyter-like cell
//! semantics — state persists across cells within a session.
//!
//! v1 ships the **Python** kernel: a `python3` subprocess running an embedded
//! JSON-lines REPL server (`src/eval/py_kernel_server.py`) with a persistent
//! namespace, per-cell timeouts enforced host-side (kill + restart with an
//! explicit state-loss warning), and stdout/stderr/result capture. The JS
//! kernel (dedicated QuickJS realm) and the tool re-entry bridge are tracked
//! follow-ups on the bead.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod js_kernel;

/// The kernel server script, shipped inside the binary.
const PY_KERNEL_SERVER: &str = include_str!("eval/py_kernel_server.py");

/// Default per-cell budget.
const DEFAULT_CELL_TIMEOUT_SECS: u64 = 30;

struct PyKernel {
    child: Child,
    stdin: ChildStdin,
    /// Lines from the kernel's stdout, streamed by a dedicated reader thread
    /// (a cell may emit several bridge-request lines before its final
    /// response). `None` = EOF (kernel exited).
    lines: std::sync::Mutex<std::sync::mpsc::Receiver<Option<String>>>,
    next_id: u64,
    cells_run: u64,
    /// Set by the first kill(): the pid is reaped and may be recycled, so a
    /// second group-kill must never run.
    killed: bool,
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
        let mut child = command.spawn().map_err(|err| {
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
        crate::tools::attach_child_job_discipline(&child);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("eval", "EVAL_SPAWN: no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("eval", "EVAL_SPAWN: no stdout pipe"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("eval-py-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                        Ok(_) => {
                            if tx.send(Some(line)).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|err| Error::tool("eval", format!("EVAL_SPAWN: {err}")))?;
        Ok(Self {
            child,
            stdin,
            lines: std::sync::Mutex::new(rx),
            next_id: 1,
            cells_run: 0,
            killed: false,
        })
    }

    /// Await the next stdout line under a budget. Ok(None) = EOF.
    ///
    /// Takes `&mut self` deliberately: `mpsc::Receiver` is `!Sync`, so a
    /// shared borrow held across the await would make the future `!Send`.
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn next_line(&mut self, deadline: Instant) -> Result<Option<String>> {
        loop {
            let received = self
                .lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv();
            match received {
                Ok(line) => return Ok(line),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if Instant::now() > deadline {
                        return Err(Error::tool("eval", "EVAL_DEADLINE"));
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
        // Guard against double-kill: after the first wait() the pid is
        // freed and the OS may recycle it — a second group-kill could hit
        // an innocent process group.
        if self.killed {
            return;
        }
        self.killed = true;
        // Process-tree discipline: the kernel's own children die with it.
        crate::tools::kill_process_group_tree(Some(self.child.id()));
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        }
    }

    /// Run one JavaScript cell on the persistent QuickJS kernel. Bridge
    /// requests re-enter pi tools exactly like the Python path.
    #[allow(clippy::too_many_lines)]
    async fn run_js_cell(&self, code: &str, timeout: Duration) -> Result<ToolOutput> {
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
            {
                restarted = true;
                js_kernel::JsKernel::spawn()
                    .map_err(|err| Error::tool("eval", format!("EVAL_SPAWN: {err}")))?
            }
        };
        let deadline = Instant::now() + timeout;
        let reply_rx = kernel
            .submit(code.to_string(), deadline)
            .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;

        // Poll for the cell response, servicing bridge requests as they come.
        let response = loop {
            if let Ok(bridge) = kernel.bridge.try_recv() {
                let reply = self.run_bridge_call_parts(&bridge.tool, bridge.input).await;
                let _ = bridge.reply.send(reply);
                continue;
            }
            match reply_rx.try_recv() {
                Ok(response) => break response,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // The interrupt handler aborts the CELL at the deadline
                    // (kernel survives); allow slack, then declare it wedged.
                    if Instant::now() > deadline + Duration::from_secs(5) {
                        return Err(Error::tool(
                            "eval",
                            "EVAL_TIMEOUT: js kernel wedged past its budget; kernel \
                             discarded — state was lost, next cell starts fresh",
                        ));
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(15),
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
        kernel.cells_run += 1;
        let cells_run = kernel.cells_run;
        {
            let mut slot = self
                .js
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(kernel);
        }

        let mut text = String::new();
        if restarted && cells_run == 1 {
            text.push_str("(js kernel started)\n");
        }
        if !response.console.is_empty() {
            text.push_str(&response.console);
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        if response.ok {
            if let Some(result) = &response.result {
                text.push_str(result);
                text.push('\n');
            }
            if text.is_empty() {
                text.push_str("(no output)\n");
            }
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(json!({
                    "kernel": "js",
                    "cell": cells_run,
                    "restarted": restarted,
                })),
                is_error: false,
            })
        } else {
            text.push_str(response.error.as_deref().unwrap_or("?"));
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(json!({
                    "kernel": "js",
                    "cell": cells_run,
                    "restarted": restarted,
                    "errorKind": "exception",
                })),
                is_error: true,
            })
        }
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
    async fn run_py_cell(&self, code: &str, timeout: Duration) -> Result<ToolOutput> {
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

        let id = kernel.next_id;
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
            let id = fresh.next_id;
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
            let line = match kernel.next_line(deadline).await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    // EOF: crashed mid-cell (e.g. os._exit). State lost.
                    kernel.kill();
                    return Err(Error::tool(
                        "eval",
                        "EVAL_KERNEL_CRASH: the Python kernel exited mid-cell — state \
                         was lost, next cell starts fresh",
                    ));
                }
                Err(_) => {
                    // Budget exhausted: kill and report state loss.
                    kernel.kill();
                    return Err(Error::tool(
                        "eval",
                        format!(
                            "EVAL_TIMEOUT: cell exceeded {}s; kernel discarded — \
                             state was lost, next cell starts fresh",
                            timeout.as_secs()
                        ),
                    ));
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Value = match serde_json::from_str(trimmed) {
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
            if let Some(bridge) = parsed.get("bridge") {
                let reply = self.run_bridge_call(bridge).await;
                let line = json!({"bridge_result": reply}).to_string();
                kernel
                    .stdin
                    .write_all(format!("{line}\n").as_bytes())
                    .and_then(|()| kernel.stdin.flush())
                    .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;
                continue;
            }
            break trimmed.to_string();
        };
        kernel.cells_run += 1;
        let cells_run = kernel.cells_run;

        // Return the kernel to the slot for the next cell.
        {
            let mut slot = self
                .kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(kernel);
        }

        format_cell_response(&final_line, restarted, cells_run)
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

/// Turn a kernel protocol response line into the tool output contract.
fn format_cell_response(line: &str, restarted: bool, cells_run: u64) -> Result<ToolOutput> {
    let response: Value = serde_json::from_str(line)
        .map_err(|err| Error::tool("eval", format!("EVAL_PROTOCOL: {err}")))?;
    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let stdout = response.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = response.get("stderr").and_then(Value::as_str).unwrap_or("");
    let mut text = String::new();
    if restarted && cells_run == 1 {
        text.push_str("(kernel started)\n");
    }
    if !stdout.is_empty() {
        text.push_str(stdout);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    if !stderr.is_empty() {
        text.push_str(stderr);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    if ok {
        if let Some(result) = response.get("result").and_then(Value::as_str) {
            text.push_str(result);
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("(no output)\n");
        }
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "kernel": "python",
                "cell": cells_run,
                "restarted": restarted,
            })),
            is_error: false,
        })
    } else {
        let error = response.get("error").and_then(Value::as_str).unwrap_or("?");
        text.push_str(error);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "kernel": "python",
                "cell": cells_run,
                "restarted": restarted,
                "errorKind": "exception",
            })),
            is_error: true,
        })
    }
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
        "Run code in a persistent Python kernel (Jupyter-like cells): variables \
         and imports persist across calls within the session. The final \
         expression's value is returned like a REPL. Timeouts or crashes \
         restart the kernel with an explicit state-loss notice."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute in the persistent kernel"
                },
                "kernel": {
                    "type": "string",
                    "enum": ["python", "js"],
                    "description": "Kernel to use (default python)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Per-cell budget in seconds (default 30)"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("eval", "missing required field: code"))?;
        let kernel = input
            .get("kernel")
            .and_then(Value::as_str)
            .unwrap_or("python");
        let timeout = Duration::from_secs(
            input
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_CELL_TIMEOUT_SECS)
                .clamp(1, 600),
        );
        match kernel {
            "python" => self.run_py_cell(code, timeout).await,
            "js" => self.run_js_cell(code, timeout).await,
            other => Err(Error::tool(
                "eval",
                format!("unknown kernel: {other} (python|js)"),
            )),
        }
    }

    fn effects(&self) -> ToolEffects {
        // Arbitrary code: process-level effects, serialized fail-closed.
        ToolEffects::process()
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
        // A unique sleep duration marks OUR child: `pgrep -f "sleep 60"`
        // substring-matches any concurrent process mentioning "sleep 60x"
        // (other agents' polling shells), which made this test fail on
        // shared machines through no fault of the kill discipline.
        let marker_secs = 50_000 + std::process::id() % 10_000;
        let kernel_pid = {
            let tool = EvalTool::new(dir.path());
            let out = run_cell_sync(
                &tool,
                &format!(
                    "import os\nimport subprocess\npid = os.getpid()\nsubprocess.Popen(['sleep', '{marker_secs}'])"
                ),
            )
            .expect("spawn cell");
            assert!(!out.is_error, "cell: {}", output_text(&out));
            let out = run_cell_sync(&tool, "pid").expect("pid cell");
            let text = output_text(&out);
            text.trim()
                .trim_matches('"')
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("pid from cell output: {text}"))
            // tool (and its kernel) drop here
        };
        std::thread::sleep(std::time::Duration::from_millis(400));
        let state = std::fs::read_to_string(format!("/proc/{kernel_pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(')').next()?.trim().chars().next());
        assert!(
            state.is_none() || state == Some('Z'),
            "kernel pid {kernel_pid} survived session end (state {state:?})"
        );
        // The kernel's own `sleep` child died with it (tree discipline).
        let survivor = std::process::Command::new("pgrep")
            .args(["-f", &format!("sleep {marker_secs}")])
            .output()
            .is_ok_and(|output| output.status.success() && !output.stdout.is_empty());
        assert!(!survivor, "kernel-spawned sleep survived session end");
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
    fn js_infinite_loop_times_out_and_kernel_state_survives() {
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
        // The interrupt aborts the CELL, not the kernel: state survives.
        let out = run_js_cell_sync(&tool, "keep").expect("post-timeout");
        assert!(!out.is_error, "kernel died: {}", output_text(&out));
        assert!(
            output_text(&out).contains('5'),
            "state lost: {}",
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
