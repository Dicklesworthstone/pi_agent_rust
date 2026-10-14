//! Exclusive cell admission and explicit, language-scoped kernel reset.
//!
//! The agent scheduler is not the only caller of EvalTool. A slot that is
//! temporarily empty while its kernel executes is not permission for another
//! SDK call to spawn a competing kernel and overwrite the returned state.

use super::{DEFAULT_CELL_TIMEOUT_SECS, EvalTool};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{ToolOutput, ToolUpdate};
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::time::{Duration, Instant};

mod source;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kernel {
    #[default]
    Python,
    Js,
}

impl Kernel {
    const fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Js => "js",
        }
    }
}

#[derive(Deserialize)]
struct Cell {
    code: String,
    #[serde(default)]
    kernel: Kernel,
    timeout_secs: Option<u64>,
    #[serde(default)]
    reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellStop {
    Cancelled,
    Timeout,
}

impl CellStop {
    const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "EVAL_CANCELLED",
            Self::Timeout => "EVAL_TIMEOUT",
        }
    }

    const fn kind(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

fn checkpoint(owner: &AgentCx, deadline: Option<Instant>) -> std::result::Result<(), CellStop> {
    owner.checkpoint().map_err(|_| CellStop::Cancelled)?;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(CellStop::Timeout);
    }
    Ok(())
}

/// Enclose the whole operation, not just the kernel's receive loop. A bridge
/// read that is itself awaiting I/O must not suspend eval's owner or watchdog.
/// The native future is polled AND retired with its original scoped context.
async fn wait_owned<F: Future>(
    owner: &AgentCx,
    deadline: Option<Instant>,
    future: F,
) -> std::result::Result<F::Output, CellStop> {
    let mut pending = Box::pin(owner.with_current(future));
    loop {
        checkpoint(owner, deadline)?;
        let tick = Duration::from_millis(20);
        let wait = deadline.map_or(tick, |deadline| {
            deadline.saturating_duration_since(Instant::now()).min(tick)
        });
        let time = owner.time();
        let sleeper = Box::pin(time.sleep(wait));
        match futures::future::select(pending, sleeper).await {
            futures::future::Either::Left((output, _)) => {
                // A blocking section may have returned Ready after its budget.
                // Do not publish that as success merely because it won a poll.
                checkpoint(owner, deadline)?;
                return Ok(output);
            }
            futures::future::Either::Right(((), operation)) => pending = operation,
        }
    }
}

fn before_execution_stop(stop: CellStop) -> Error {
    Error::tool("eval", format!(
        "{}: request stopped before execution; retained kernel state unchanged",
        stop.code()
    ))
}

impl EvalTool {
    pub(super) async fn execute_cell(
        &self,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        // Validate before waiting, spawning, or destroying any retained state.
        // A misspelled language or a string-valued reset cannot silently reset
        // Python. Unknown extension fields remain ignored as before.
        let cell: Cell = serde_json::from_value(input)
            .map_err(|error| Error::tool("eval", format!("EVAL_INPUT: {error}")))?;
        let load_path = match cell.kernel {
            Kernel::Js => source::load_path(&cell.code)?,
            Kernel::Python => None,
        };
        let timeout = Duration::from_secs(
            cell.timeout_secs
                .unwrap_or(DEFAULT_CELL_TIMEOUT_SECS)
                .clamp(1, 600),
        );
        let owner = AgentCx::for_current_or_request();
        // Waiting is asynchronous and consumes no cell runtime budget. The
        // guard releases on success, error, panic, or future abandonment.
        // No FIFO ordering is promised between simultaneous callers.
        let _execution = wait_owned(&owner, None, self.execution.lock())
            .await.map_err(before_execution_stop)?;
        // Read on admission, not while queued. Preparation errors never
        // discard a retained kernel, even for a reset request.
        let prepared = match load_path {
            Some(path) => Some(wait_owned(&owner, None, source::load(self.cwd.clone(), path))
                .await.map_err(before_execution_stop)??),
            None => None,
        };
        if cell.reset {
            // Drop outside the short slot lock, before starting the next cell.
            // This uses the same process-tree/worker cleanup as session end.
            self.retire_kernel(cell.kernel, &owner);
        }
        // Keep the existing per-language error contract: Python timeout/crash
        // is a tool Error; JS timeout is an unsuccessful ToolOutput. Capture
        // JS sequencing before the runner checks its kernel out across awaits.
        let (js_cell, js_restarted) = {
            let slot = self.js.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.as_ref().map_or((1, true), |kernel| (kernel.cells_run.saturating_add(1), false))
        };
        // Capture outlives either checked-out kernel future: cancellation can
        // retire the producer without discarding already received output.
        let mut capture = super::output::Capture::new(cell.kernel.name(), on_update);
        let deadline = Instant::now() + timeout;
        let outcome = match cell.kernel {
            Kernel::Python => wait_owned(&owner, Some(deadline), self.run_py_cell(&cell.code, timeout, &mut capture)).await,
            Kernel::Js => {
                let code = prepared.as_ref().map_or(cell.code.as_str(), |file| file.code.as_str());
                wait_owned(&owner, Some(deadline), self.run_js_cell(code, timeout, &mut capture)).await
            }
        };
        let outcome = match outcome {
            Ok(output) => output,
            Err(stop) => {
                // Dropping wait_owned retired the active future and its
                // checked-out kernel. Also remove a kernel that was returned
                // to its slot during a late Ready poll before the final check.
                self.retire_kernel(cell.kernel, &owner);
                let message = format!(
                    "{}: {} cell stopped; EVAL_STATE_RESET: selected kernel state discarded, next cell starts fresh",
                    stop.code(), cell.kernel.name()
                );
                match cell.kernel {
                    Kernel::Python => Err(Error::tool("eval", message)),
                    Kernel::Js => {
                        let mut output = ToolOutput {
                            content: vec![ContentBlock::Text(TextContent::new(message))],
                            details: Some(json!({
                                "kernel":"js", "cell":js_cell, "restarted":js_restarted,
                                "errorKind":stop.kind(), "stateReset":true,
                            })),
                            is_error: true,
                        };
                        capture.append_partial(&mut output);
                        Ok(output)
                    }
                }
            }
        };
        capture.publish(true);
        let mut output = outcome.map_err(|error| {
            let error = capture.with_error(error);
            if cell.reset {
                Error::tool(
                    "eval",
                    format!(
                        "EVAL_STATE_RESET: {} state explicitly discarded before this cell; {error}",
                        cell.kernel.name()
                    ),
                )
            } else {
                error
            }
        })?;
        if let Some(Value::Object(details)) = output.details.as_mut() {
            details.insert("reset".to_string(), Value::Bool(cell.reset));
            if let Some(file) = &prepared {
                details.insert("sourcePath".into(), Value::String(file.path.clone()));
                details.insert("sourceCommand".into(), Value::String(cell.code));
            }
        }
        if let Some(file) = prepared.as_ref().filter(|_| output.is_error) {
            // The embedded engine still uses its ordinary eval stack labels;
            // report the inspected file without claiming a module loader.
            output.content.insert(0, ContentBlock::Text(TextContent::new(format!(
                "EVAL_LOAD: error executing {}\n", serde_json::to_string(&file.path)?
            ))));
        }
        if cell.reset {
            let notice = format!(
                "EVAL_STATE_RESET: {} state explicitly discarded before this cell.\n",
                cell.kernel.name()
            );
            match output.content.first_mut() {
                Some(ContentBlock::Text(text)) => text.text.insert_str(0, &notice),
                _ => output
                    .content
                    .insert(0, ContentBlock::Text(TextContent::new(notice))),
            }
        }
        Ok(output)
    }

    fn retire_kernel(&self, kernel: Kernel, owner: &AgentCx) {
        let _owner = owner.cx().clone().set_current_restricted();
        match kernel {
            Kernel::Python => {
                let retired = self.kernel.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).take();
                drop(retired);
            }
            Kernel::Js => {
                let retired = self.js.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).take();
                drop(retired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8)
            .build()
            .unwrap()
            .block_on(future)
    }

    fn text(output: &ToolOutput) -> &str {
        match &output.content[0] {
            ContentBlock::Text(text) => &text.text,
            _ => panic!("expected text output"),
        }
    }

    async fn js(tool: &EvalTool, code: &str) -> ToolOutput {
        let output = tool
            .execute("cell", json!({"kernel":"js", "code":code}), None)
            .await
            .unwrap();
        assert!(!output.is_error, "{}", text(&output));
        output
    }

    #[test]
    fn expired_or_cancelled_owner_never_polls_unstarted_work() {
        let owner = AgentCx::for_testing();
        let started = std::cell::Cell::new(false);
        let result = run(wait_owned(&owner, Some(Instant::now()), async {
            started.set(true);
        }));
        assert_eq!(result, Err(CellStop::Timeout));
        assert!(!started.get());
        owner.set_cancel_requested(true);
        let result = run(wait_owned(&owner, None, async { started.set(true); }));
        assert_eq!(result, Err(CellStop::Cancelled));
        assert!(!started.get());
    }

    #[test]
    fn late_ready_output_cannot_turn_cancellation_into_success() {
        let owner = AgentCx::for_testing();
        let result = run(wait_owned(&owner, None, async {
            owner.set_cancel_requested(true);
            42
        }));
        assert_eq!(result, Err(CellStop::Cancelled));
    }

    #[test]
    fn pending_native_operation_is_retired_at_the_cell_deadline() {
        struct OnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::SeqCst); }
        }
        let owner = AgentCx::for_testing();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = OnDrop(std::sync::Arc::clone(&dropped));
        let started = Instant::now();
        let result = run(wait_owned(&owner, Some(started + Duration::from_millis(30)), async move {
            let _owned_resource = guard;
            std::future::pending::<()>().await;
        }));
        assert_eq!(result, Err(CellStop::Timeout));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn queued_owner_cancellation_does_not_apply_a_reset_or_execute_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let owner = AgentCx::for_testing();
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let lease = tool.execution.lock().await;
            let mut request = Box::pin(owner.with_current(tool.execute(
                "cancelled-reset", json!({"kernel":"js", "code":"globalThis.keep = 99", "reset":true}), None,
            )));
            assert!(futures::poll!(request.as_mut()).is_pending());
            owner.set_cancel_requested(true);
            let error = request.await.unwrap_err();
            assert!(error.to_string().contains("EVAL_CANCELLED"));
            assert!(error.to_string().contains("state unchanged"));
            drop(lease);
            let kept = js(&tool, "keep").await;
            assert_eq!(text(&kept).trim(), "7");
            assert_eq!(kept.details.unwrap()["cell"], 2);
        });
    }

    #[test]
    fn active_javascript_cancellation_retires_the_kernel_and_releases_admission() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let owner = AgentCx::for_testing();
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let mut active = Box::pin(owner.with_current(tool.execute(
                "active", json!({"kernel":"js", "code":"await sleep(60000)", "timeout_secs":60}), None,
            )));
            assert!(futures::poll!(active.as_mut()).is_pending());
            assert!(tool.execution.try_lock().is_none());
            owner.set_cancel_requested(true);
            let stopped = active.await.unwrap();
            assert!(stopped.is_error);
            assert!(text(&stopped).contains("EVAL_CANCELLED"));
            assert_eq!(stopped.details.as_ref().unwrap()["stateReset"], true);
            assert!(tool.execution.try_lock().is_some());
            let fresh = js(&tool, "typeof keep").await;
            assert!(text(&fresh).contains("undefined"));
            assert_eq!(fresh.details.unwrap()["cell"], 1);
        });
    }

    #[test]
    fn javascript_budget_preserves_error_output_contract_and_discards_state() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let started = Instant::now();
            let stopped = tool.execute("timeout", json!({
                "kernel":"js", "code":"await sleep(60000)", "timeout_secs":1
            }), None).await.unwrap();
            assert!(stopped.is_error);
            assert!(text(&stopped).contains("EVAL_TIMEOUT"));
            assert!(text(&stopped).contains("EVAL_STATE_RESET"));
            assert!(started.elapsed() < Duration::from_secs(10));
            assert!(text(&js(&tool, "typeof keep").await).contains("undefined"));
        });
    }

    #[test]
    fn continuous_real_bridge_reads_cannot_extend_the_cell_deadline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input.txt"), "bridge-payload").unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            let started = Instant::now();
            let output = tool.execute("bridge-timeout", json!({
                "kernel":"js", "timeout_secs":1,
                "code":"while (true) { await tool.call('read', {path:'input.txt'}); }"
            }), None).await.unwrap();
            assert!(output.is_error);
            assert!(text(&output).contains("EVAL_TIMEOUT"), "{}", text(&output));
            assert!(text(&output).contains("EVAL_STATE_RESET"));
            assert!(started.elapsed() < Duration::from_secs(10));
            assert!(tool.execution.try_lock().is_some());
            assert!(text(&js(&tool, "42").await).trim().ends_with("42"));
        });
    }

    #[test]
    fn queued_time_does_not_consume_the_active_cell_budget() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            let lease = tool.execution.lock().await;
            let release = async {
                AgentCx::for_current_or_request().time().sleep(Duration::from_millis(1100)).await;
                drop(lease);
            };
            let (_, output) = futures::join!(release, tool.execute("queued", json!({
                "kernel":"js", "code":"42", "timeout_secs":1
            }), None));
            let output = output.unwrap();
            assert!(!output.is_error, "{}", text(&output));
            assert!(text(&output).trim().ends_with("42"));
        });
    }

    #[test]
    fn cancelled_python_cell_preserves_javascript_state() {
        if !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|status| status.success()) { return; }
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let owner = AgentCx::for_testing();
        run(async {
            js(&tool, "globalThis.keep = 42").await;
            let seeded = tool.execute("seed", json!({"code":"keep = 7"}), None).await.unwrap();
            assert!(!seeded.is_error);
            let mut active = Box::pin(owner.with_current(tool.execute(
                "python", json!({"code":"import time; time.sleep(60)", "timeout_secs":60}), None,
            )));
            assert!(futures::poll!(active.as_mut()).is_pending());
            owner.set_cancel_requested(true);
            let error = active.await.unwrap_err();
            assert!(error.to_string().contains("EVAL_CANCELLED"));
            assert!(error.to_string().contains("EVAL_STATE_RESET"));
            assert_eq!(text(&js(&tool, "keep").await).trim(), "42");
            let fresh = tool.execute("fresh", json!({"code":"'keep' in globals()"}), None).await.unwrap();
            assert!(!fresh.is_error);
            assert!(text(&fresh).contains("False"));
        });
    }

    #[test]
    fn simultaneous_sdk_cells_share_one_kernel_and_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.counter = 0").await;
            let (first, second) = futures::join!(
                js(&tool, "await sleep(30); globalThis.counter += 1"),
                js(&tool, "globalThis.counter += 1; counter"),
            );
            assert_eq!(first.details.as_ref().unwrap()["cell"], 2);
            assert_eq!(second.details.as_ref().unwrap()["cell"], 3);
            assert_eq!(text(&second).trim(), "2");
            assert_eq!(text(&js(&tool, "counter").await).trim(), "2");
        });
    }

    #[test]
    fn abandoning_a_waiting_reset_does_not_touch_retained_state() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let lease = tool.execution.lock().await;
            let mut waiting = tool.execute(
                "waiting",
                json!({"kernel":"js", "code":"globalThis.keep = 99", "reset":true}),
                None,
            );
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            drop(waiting);
            drop(lease);
            assert_eq!(text(&js(&tool, "keep").await).trim(), "7");
        });
    }

    #[test]
    fn invalid_reset_request_does_not_wait_or_discard_state() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let lease = tool.execution.lock().await;
            for input in [
                json!({"kernel":"js", "code":"", "reset":"true"}),
                json!({"kernel":"unknown", "code":"", "reset":true}),
                json!({"kernel":null, "code":"", "reset":true}),
                json!({"kernel":"js", "reset":true}),
                json!({"kernel":"js", "code":"", "timeout_secs":-1, "reset":true}),
            ] {
                let mut request = tool.execute("invalid", input, None);
                match futures::poll!(request.as_mut()) {
                    std::task::Poll::Ready(Err(error)) => {
                        assert!(error.to_string().contains("EVAL_INPUT"));
                    }
                    _ => panic!("invalid input must fail before admission"),
                }
            }
            drop(lease);
            assert_eq!(text(&js(&tool, "keep").await).trim(), "7");
        });
    }

    #[test]
    fn explicit_reset_recreates_lexical_bindings_and_restarts_cell_count() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "const value = 7").await;
            let output = tool.execute(
                "reset", json!({"kernel":"js", "code":"const value = 42; value", "reset":true}), None,
            ).await.unwrap();
            assert!(!output.is_error, "{}", text(&output));
            assert!(text(&output).contains("EVAL_STATE_RESET"));
            assert!(text(&output).trim().ends_with("42"));
            let details = output.details.as_ref().unwrap();
            assert_eq!(details["reset"], true);
            assert_eq!(details["restarted"], true);
            assert_eq!(details["cell"], 1);
            let next = js(&tool, "value").await;
            assert_eq!(next.details.as_ref().unwrap()["reset"], false);
            assert_eq!(next.details.as_ref().unwrap()["cell"], 2);
        });
    }

    #[test]
    fn queued_reset_runs_after_the_active_cell_not_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let (first, reset) = futures::join!(
                js(&tool, "await sleep(30); globalThis.keep = 8"),
                tool.execute("reset", json!({"kernel":"js", "code":"typeof keep", "reset":true}), None),
            );
            assert_eq!(first.details.as_ref().unwrap()["cell"], 2);
            let reset = reset.unwrap();
            assert!(!reset.is_error);
            assert!(text(&reset).contains("undefined"));
            assert_eq!(reset.details.as_ref().unwrap()["cell"], 1);
            assert_eq!(text(&js(&tool, "typeof keep").await).trim(), "\"undefined\"");
        });
    }

    #[test]
    fn abandoned_active_cell_releases_admission_and_never_returns_its_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let mut active = tool.execute("active", json!({"kernel":"js", "code":"await sleep(60000)"}), None);
            assert!(futures::poll!(active.as_mut()).is_pending());
            assert!(tool.execution.try_lock().is_none());
            drop(active);
            assert!(tool.execution.try_lock().is_some());
            let next = js(&tool, "typeof keep").await;
            assert!(text(&next).contains("undefined"));
            assert_eq!(next.details.as_ref().unwrap()["restarted"], true);
        });
    }

    #[test]
    fn separate_eval_instances_do_not_share_admission_or_state() {
        let dir = tempfile::tempdir().unwrap();
        let first = EvalTool::new(dir.path());
        let second = EvalTool::new(dir.path());
        run(async {
            let _lease = first.execution.lock().await;
            assert!(text(&js(&second, "typeof keep").await).contains("undefined"));
            js(&second, "globalThis.keep = 42").await;
        });
        assert!(text(&run(js(&first, "typeof keep"))).contains("undefined"));
    }

    #[test]
    fn reset_failure_reports_state_loss_and_releases_admission() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = EvalTool::new(dir.path());
        tool.python_path = "/nonexistent/pi-eval-python".into();
        let error = run(tool.execute("reset", json!({"code":"", "reset":true}), None)).unwrap_err();
        assert!(error.to_string().contains("EVAL_STATE_RESET"));
        assert!(error.to_string().contains("EVAL_PY_MISSING"));
        assert!(tool.execution.try_lock().is_some());
        assert!(text(&run(js(&tool, "42"))).trim().ends_with("42"));
    }

    #[test]
    fn resetting_one_language_preserves_the_other_and_empty_code_is_valid() {
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        run(async {
            js(&tool, "globalThis.keep = 7").await;
            let seeded = tool.execute("py", json!({"code":"keep = 42"}), None).await.unwrap();
            assert!(!seeded.is_error);
            let reset = tool.execute("js-reset", json!({"kernel":"js", "code":"", "reset":true}), None).await.unwrap();
            assert!(!reset.is_error);
            let kept = tool.execute("py", json!({"code":"keep"}), None).await.unwrap();
            assert!(!kept.is_error);
            assert_eq!(text(&kept).trim(), "42");
            js(&tool, "globalThis.keep = 8").await;
            let reset = tool.execute("py-reset", json!({"code":"'keep' in globals()", "reset":true}), None).await.unwrap();
            assert!(!reset.is_error);
            assert!(text(&reset).contains("False"));
            assert_eq!(reset.details.as_ref().unwrap()["cell"], 1);
            assert_eq!(text(&js(&tool, "keep").await).trim(), "8");
        });
    }
}
