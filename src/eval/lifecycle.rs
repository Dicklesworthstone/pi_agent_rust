//! Exclusive cell admission and explicit, language-scoped kernel reset.
//!
//! The agent scheduler is not the only caller of EvalTool. A slot that is
//! temporarily empty while its kernel executes is not permission for another
//! SDK call to spawn a competing kernel and overwrite the returned state.

use super::{DEFAULT_CELL_TIMEOUT_SECS, EvalTool};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

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

impl EvalTool {
    pub(super) async fn execute_cell(&self, input: Value) -> Result<ToolOutput> {
        // Validate before waiting, spawning, or destroying any retained state.
        // A misspelled language or a string-valued reset cannot silently reset
        // Python. Unknown extension fields remain ignored as before.
        let cell: Cell = serde_json::from_value(input)
            .map_err(|error| Error::tool("eval", format!("EVAL_INPUT: {error}")))?;
        let timeout = Duration::from_secs(
            cell.timeout_secs
                .unwrap_or(DEFAULT_CELL_TIMEOUT_SECS)
                .clamp(1, 600),
        );
        // Waiting is asynchronous and consumes no cell runtime budget. The
        // guard releases on success, error, panic, or future abandonment.
        // No FIFO ordering is promised between simultaneous callers.
        let _execution = self.execution.lock().await;
        if cell.reset {
            // Drop outside the short slot lock, before starting the next cell.
            // This uses the same process-tree/worker cleanup as session end.
            match cell.kernel {
                Kernel::Python => {
                    let retired = self
                        .kernel
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    drop(retired);
                }
                Kernel::Js => {
                    let retired = self
                        .js
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    drop(retired);
                }
            }
        }
        let outcome = match cell.kernel {
            Kernel::Python => self.run_py_cell(&cell.code, timeout).await,
            Kernel::Js => self.run_js_cell(&cell.code, timeout).await,
        };
        let mut output = outcome.map_err(|error| {
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
