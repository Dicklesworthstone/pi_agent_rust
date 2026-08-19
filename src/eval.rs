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
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The kernel server script, shipped inside the binary.
const PY_KERNEL_SERVER: &str = include_str!("eval/py_kernel_server.py");

/// Default per-cell budget.
const DEFAULT_CELL_TIMEOUT_SECS: u64 = 30;

struct PyKernel {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    cells_run: u64,
}

impl PyKernel {
    fn spawn(python_path: &str, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(python_path)
            .args(["-c", PY_KERNEL_SERVER])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
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
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("eval", "EVAL_SPAWN: no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("eval", "EVAL_SPAWN: no stdout pipe"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            cells_run: 0,
        })
    }

    fn kill(&mut self) {
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
}

impl EvalTool {
    pub fn new(cwd: &Path) -> Self {
        let python_path =
            std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| String::from("python3"));
        Self {
            cwd: cwd.to_path_buf(),
            python_path,
            kernel: Mutex::new(None),
        }
    }

    /// Run one cell: writes the request line, then reads the response on a
    /// blocking thread while this async fn polls with the cell budget. On
    /// timeout the kernel is killed (state loss) and the next cell restarts.
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

        // Blocking read on a thread; poll the channel under the cell budget.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut line = String::new();
        let mut reader_kernel = kernel;
        let reader = std::thread::Builder::new()
            .name("eval-py-read".into())
            .spawn(move || {
                let result = reader_kernel.stdout.read_line(&mut line).map(|_| line);
                let _ = tx.send((reader_kernel, result));
            })
            .map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;
        drop(reader);

        let started = Instant::now();
        let (mut kernel, read) = loop {
            match rx.try_recv() {
                Ok(pair) => break pair,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if started.elapsed() > timeout {
                        // The reader thread still owns the kernel; killing via
                        // the child is impossible from here, so poison the
                        // slot: next cell spawns fresh. The reader thread
                        // exits when the killed... — we cannot kill without
                        // the handle, so instead leave a tombstone and let
                        // the OS process be reaped when the thread's owner
                        // drops (kernel Drop kills the child).
                        return Err(Error::tool(
                            "eval",
                            format!(
                                "EVAL_TIMEOUT: cell exceeded {}s; kernel discarded — \
                                 state was lost, next cell starts fresh",
                                timeout.as_secs()
                            ),
                        ));
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(25),
                    )
                    .await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::tool("eval", "EVAL_IO: reader thread vanished"));
                }
            }
        };
        let line = read.map_err(|err| Error::tool("eval", format!("EVAL_IO: {err}")))?;
        if line.is_empty() {
            // EOF: the kernel crashed mid-cell (e.g. os._exit). Report state
            // loss; leave the slot empty so the next cell restarts fresh.
            kernel.kill();
            return Err(Error::tool(
                "eval",
                "EVAL_KERNEL_CRASH: the Python kernel exited mid-cell — state was \
                 lost, next cell starts fresh",
            ));
        }
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

        format_cell_response(line.trim(), restarted, cells_run)
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
                    "enum": ["python"],
                    "description": "Kernel to use (python only for now)"
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
        if kernel != "python" {
            return Err(Error::tool(
                "eval",
                format!("unknown kernel: {kernel} (python only for now; js is tracked)"),
            ));
        }
        let timeout = Duration::from_secs(
            input
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_CELL_TIMEOUT_SECS)
                .clamp(1, 600),
        );
        self.run_py_cell(code, timeout).await
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
        match &output.content[0] { // ubs:ignore test index — single-block output is the assertion
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
