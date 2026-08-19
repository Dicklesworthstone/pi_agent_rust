//! Agent-facing `debug` tool: DAP debugger driving with adapter
//! auto-selection (bd-cv653.1.2).
//!
//! One active session per tool instance (omp semantics). State-gated
//! operations (stack/scopes/variables/step/evaluate-in-frame) require the
//! stopped state and fail fast with named errors while running or exited.

pub mod adapters;
pub mod dap;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use adapters::AdapterSpec;
use session::{DapSession, ExecState};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("debug", format!("[{code}] {}", message.into()))
}

fn text_output(text: String, details: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `debug` tool.
pub struct DebugTool {
    cwd: PathBuf,
    adapters: Vec<AdapterSpec>,
    session: Mutex<Option<Arc<DapSession>>>,
}

impl DebugTool {
    #[must_use]
    pub fn new(cwd: &Path, _config: Option<&Config>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            adapters: adapters::default_adapters(),
            session: Mutex::new(None),
        }
    }

    /// The live session or a named error.
    fn session(&self) -> Result<Arc<DapSession>> {
        lock(&self.session).clone().ok_or_else(|| {
            tool_err(
                "DAP_NO_SESSION",
                "no active debug session; launch or attach first",
            )
        })
    }

    /// Resolve a user path against the tool cwd.
    fn resolve(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.cwd.join(candidate)
        }
    }

    async fn run_launch(&self, input: &DebugInput) -> Result<ToolOutput> {
        let program = input
            .program
            .as_deref()
            .ok_or_else(|| tool_err("DAP_USAGE", "debug launch requires `program`"))?;
        let program_path = self.resolve(program);
        if !program_path.exists() {
            return Err(tool_err(
                "DAP_TARGET_MISSING",
                format!("program does not exist: {}", program_path.display()),
            ));
        }
        let adapter = adapters::select_adapter(
            Some(&program_path),
            input.adapter.as_deref(),
            &self.adapters,
        )
        .ok_or_else(|| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!(
                    "no debug adapter for {} resolved on this machine; install lldb-dap, debugpy, or dlv",
                    program_path.display()
                ),
            )
        })?;
        let command = adapter.resolve_command().ok_or_else(|| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!(
                    "adapter {} not on PATH. hint: {}",
                    adapter.id, adapter.install_hint
                ),
            )
        })?;
        let mut args = adapter.adapter_args.clone();
        args.extend(
            adapter
                .adapter_args
                .is_empty()
                .then(Vec::new)
                .unwrap_or_default(),
        );
        let transport = dap::DapTransport::spawn(&command, &args, &[], &self.cwd)?;
        let session = DapSession::begin(transport).await?;
        let launch_args = adapters::launch_arguments(
            &adapter,
            &program_path,
            &input.args.clone().unwrap_or_default(),
            &self.cwd,
        );
        // DAP ordering reality: some adapters answer `launch` immediately
        // (lldb-dap), others only after `configurationDone` (debugpy).
        // Awaiting launch first deadlocks the second kind — drive both
        // concurrently.
        let (launch_result, _) =
            futures::future::join(session.call("launch", launch_args.clone()), async {
                session.wait_initialized().await;
                session
                    .call("configurationDone", json!({}))
                    .await
                    .unwrap_or(Value::Null)
            })
            .await;
        if let Err(err) = launch_result {
            let tail = session.output_tail();
            return Err(tool_err(
                "DAP_LAUNCH_FAILED",
                format!("launch failed: {err}; adapter stderr: {tail}; args: {launch_args}"),
            ));
        }
        // stopOnEntry=true: the debuggee is stopped at the entry point so
        // breakpoints set next always land before the program runs.
        let entry_stop = session
            .wait_stopped(std::time::Duration::from_secs(10))
            .await;
        let adapter_id = adapter.id.clone();
        *lock(&self.session) = Some(Arc::new(session));
        let payload = json!({
            "action": "launch",
            "program": program_path.display().to_string(),
            "adapter": adapter_id,
            "state": if entry_stop.is_some() { "stopped_entry" } else { "running" },
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_attach(&self, input: &DebugInput) -> Result<ToolOutput> {
        let pid = input
            .pid
            .ok_or_else(|| tool_err("DAP_USAGE", "debug attach requires `pid`"))?;
        let adapter = adapters::select_adapter(None, input.adapter.as_deref(), &self.adapters)
            .ok_or_else(|| tool_err("DAP_ADAPTER_MISSING", "no adapter resolved for attach"))?;
        let command = adapter.resolve_command().ok_or_else(|| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!(
                    "adapter {} not on PATH. hint: {}",
                    adapter.id, adapter.install_hint
                ),
            )
        })?;
        let transport = dap::DapTransport::spawn(&command, &adapter.adapter_args, &[], &self.cwd)?;
        let session = DapSession::begin(transport).await?;
        // Same adapter-ordering reality as launch: drive attach and
        // configurationDone concurrently.
        let (attach_result, _) = futures::future::join(
            session.call("attach", adapters::attach_arguments(&adapter, pid)),
            async {
                session.wait_initialized().await;
                session
                    .call("configurationDone", json!({}))
                    .await
                    .unwrap_or(Value::Null)
            },
        )
        .await;
        attach_result?;
        let adapter_id = adapter.id.clone();
        *lock(&self.session) = Some(Arc::new(session));
        let payload = json!({
            "action": "attach",
            "pid": pid,
            "adapter": adapter_id,
            "state": "running",
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_simple(&self, input: &DebugInput) -> Result<ToolOutput> {
        let session = self.session()?;
        match input.action.as_str() {
            "set_breakpoint"
            | "remove_breakpoint"
            | "set_function_breakpoint"
            | "set_instruction_breakpoint"
            | "remove_instruction_breakpoint"
            | "data_breakpoint_info"
            | "set_data_breakpoint"
            | "remove_data_breakpoint" => self.run_breakpoints(&session, input).await,
            "continue" | "step_over" | "step_in" | "step_out" | "pause" => {
                self.run_execution(&session, input).await
            }
            "evaluate" | "stack_trace" | "threads" | "scopes" | "variables" => {
                self.run_inspection(&session, input).await
            }
            "disassemble" | "read_memory" | "write_memory" | "modules" | "loaded_sources"
            | "custom_request" | "output" => self.run_inspection_ex(&session, input).await,
            "terminate" | "sessions" => self.run_lifecycle(&session, input).await,
            other => Err(tool_err(
                "DAP_USAGE",
                format!("unknown debug action {other:?}"),
            )),
        }
    }

    async fn run_breakpoints(
        &self,
        session: &DapSession,
        input: &DebugInput,
    ) -> Result<ToolOutput> {
        match input.action.as_str() {
            "set_breakpoint" => {
                let file = input
                    .file
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "set_breakpoint requires `file`"))?;
                let line = input
                    .line
                    .ok_or_else(|| tool_err("DAP_USAGE", "set_breakpoint requires `line`"))?;
                let path = self.resolve(file);
                let body = session
                    .call(
                        "setBreakpoints",
                        json!({
                            "source": { "path": path.display().to_string() },
                            "breakpoints": [{ "line": line }],
                        }),
                    )
                    .await?;
                let verified = body
                    .get("breakpoints")
                    .and_then(Value::as_array)
                    .and_then(|bps| bps.first())
                    .and_then(|bp| bp.get("verified"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let payload = json!({
                    "action": "set_breakpoint",
                    "file": path.display().to_string(),
                    "line": line,
                    "verified": verified,
                });
                Ok(text_output(payload.to_string(), payload))
            }
            "remove_breakpoint" => {
                let file = input
                    .file
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "remove_breakpoint requires `file`"))?;
                let path = self.resolve(file);
                session
                    .call(
                        "setBreakpoints",
                        json!({
                            "source": { "path": path.display().to_string() },
                            "breakpoints": [],
                        }),
                    )
                    .await?;
                let payload =
                    json!({ "action": "remove_breakpoint", "file": path.display().to_string() });
                Ok(text_output(payload.to_string(), payload))
            }
            "set_function_breakpoint" => {
                let name = input.name.as_deref().ok_or_else(|| {
                    tool_err("DAP_USAGE", "set_function_breakpoint requires `name`")
                })?;
                let body = session
                    .call(
                        "setFunctionBreakpoints",
                        json!({ "breakpoints": [{ "name": name }] }),
                    )
                    .await?;
                let verified = body
                    .get("breakpoints")
                    .and_then(Value::as_array)
                    .and_then(|bps| bps.first())
                    .and_then(|bp| bp.get("verified"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let payload = json!({
                    "action": "set_function_breakpoint",
                    "name": name,
                    "verified": verified,
                });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_breakpoints action {:?}", input.action),
            )),
        }
    }

    async fn run_breakpoints_ex(
        &self,
        session: &DapSession,
        input: &DebugInput,
    ) -> Result<ToolOutput> {
        match input.action.as_str() {
            "set_instruction_breakpoint" => {
                let reference = input.reference.as_deref().ok_or_else(|| {
                    tool_err(
                        "DAP_USAGE",
                        "set_instruction_breakpoint requires `reference`",
                    )
                })?;
                session
                    .call(
                        "setInstructionBreakpoints",
                        json!({ "breakpoints": [{ "instructionReference": reference }] }),
                    )
                    .await?;
                let payload =
                    json!({ "action": "set_instruction_breakpoint", "reference": reference });
                Ok(text_output(payload.to_string(), payload))
            }
            "remove_instruction_breakpoint" => {
                session
                    .call("setInstructionBreakpoints", json!({ "breakpoints": [] }))
                    .await?;
                let payload = json!({ "action": "remove_instruction_breakpoint" });
                Ok(text_output(payload.to_string(), payload))
            }
            "data_breakpoint_info" => {
                let name = input
                    .name
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "data_breakpoint_info requires `name`"))?;
                let body = session
                    .call(
                        "dataBreakpointInfo",
                        json!({ "name": name, "frameId": input.frame_id }),
                    )
                    .await?;
                let payload = json!({ "action": "data_breakpoint_info", "result": body });
                Ok(text_output(payload.to_string(), payload))
            }
            "set_data_breakpoint" => {
                let name = input
                    .name
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "set_data_breakpoint requires `name`"))?;
                session
                    .call(
                        "setDataBreakpoints",
                        json!({ "breakpoints": [{ "dataId": name, "accessType": "write" }] }),
                    )
                    .await?;
                let payload = json!({ "action": "set_data_breakpoint", "name": name });
                Ok(text_output(payload.to_string(), payload))
            }
            "remove_data_breakpoint" => {
                session
                    .call("setDataBreakpoints", json!({ "breakpoints": [] }))
                    .await?;
                let payload = json!({ "action": "remove_data_breakpoint" });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_breakpoints action {:?}", input.action),
            )),
        }
    }

    async fn run_execution(&self, session: &DapSession, input: &DebugInput) -> Result<ToolOutput> {
        match input.action.as_str() {
            "continue" => {
                let thread = Self::current_thread(session)?;
                session
                    .call("continue", json!({ "threadId": thread }))
                    .await?;
                let payload = json!({ "action": "continue", "threadId": thread });
                Ok(text_output(payload.to_string(), payload))
            }
            "step_over" | "step_in" | "step_out" => {
                let thread = Self::current_thread(session)?;
                let command = match input.action.as_str() {
                    "step_over" => "next",
                    "step_in" => "stepIn",
                    _ => "stepOut",
                };
                session.call(command, json!({ "threadId": thread })).await?;
                // Stepping usually stops again quickly; report the new state.
                let stopped = session
                    .wait_stopped(std::time::Duration::from_secs(5))
                    .await;
                let payload = json!({
                    "action": input.action,
                    "threadId": thread,
                    "stopped": stopped.map(|(t, r)| json!({"threadId": t, "reason": r})),
                });
                Ok(text_output(payload.to_string(), payload))
            }
            "pause" => {
                let thread = self.any_thread(session).await?;
                session.call("pause", json!({ "threadId": thread })).await?;
                let stopped = session
                    .wait_stopped(std::time::Duration::from_secs(5))
                    .await;
                let payload = json!({
                    "action": "pause",
                    "threadId": thread,
                    "stopped": stopped.map(|(t, r)| json!({"threadId": t, "reason": r})),
                });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_execution action {:?}", input.action),
            )),
        }
    }

    async fn run_inspection(&self, session: &DapSession, input: &DebugInput) -> Result<ToolOutput> {
        match input.action.as_str() {
            "evaluate" => {
                let expression = input
                    .expression
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "evaluate requires `expression`"))?;
                let body = session
                    .call_stopped(
                        "evaluate",
                        json!({
                            "expression": expression,
                            "frameId": input.frame_id,
                            "context": input.context.as_deref().unwrap_or("repl"),
                        }),
                    )
                    .await?;
                let payload = json!({
                    "action": "evaluate",
                    "expression": expression,
                    "result": body.get("result").cloned().unwrap_or(Value::Null),
                    "type": body.get("type").cloned().unwrap_or(Value::Null),
                });
                Ok(text_output(payload.to_string(), payload))
            }
            "stack_trace" => {
                let thread = Self::current_thread(session)?;
                let body = session
                    .call_stopped(
                        "stackTrace",
                        json!({ "threadId": thread, "startFrame": 0, "levels": input.limit.unwrap_or(50) }),
                    )
                    .await?;
                let payload = json!({ "action": "stack_trace", "threadId": thread, "frames": body.get("stackFrames").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "threads" => {
                let body = session.call("threads", json!({})).await?;
                let payload = json!({ "action": "threads", "threads": body.get("threads").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "scopes" => {
                let frame = match input.frame_id {
                    Some(frame) => frame,
                    None => self.top_frame(session).await?,
                };
                let body = session
                    .call_stopped("scopes", json!({ "frameId": frame }))
                    .await?;
                let payload = json!({ "action": "scopes", "frameId": frame, "scopes": body.get("scopes").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "variables" => {
                let reference = input.variables_reference.ok_or_else(|| {
                    tool_err(
                        "DAP_USAGE",
                        "variables requires `variablesReference` (from scopes)",
                    )
                })?;
                let body = session
                    .call_stopped("variables", json!({ "variablesReference": reference }))
                    .await?;
                let payload = json!({ "action": "variables", "variablesReference": reference, "variables": body.get("variables").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_inspection action {:?}", input.action),
            )),
        }
    }

    async fn run_inspection_ex(
        &self,
        session: &DapSession,
        input: &DebugInput,
    ) -> Result<ToolOutput> {
        match input.action.as_str() {
            "disassemble" => {
                let reference = input.reference.as_deref().ok_or_else(|| {
                    tool_err(
                        "DAP_USAGE",
                        "disassemble requires `reference` (address or instruction ref)",
                    )
                })?;
                let body = session
                    .call(
                        "disassemble",
                        json!({
                            "memoryReference": reference,
                            "instructionOffset": input.offset.unwrap_or(0),
                            "instructionCount": input.limit.unwrap_or(50),
                            "resolveSymbols": true,
                        }),
                    )
                    .await?;
                let payload = json!({ "action": "disassemble", "instructions": body.get("instructions").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "read_memory" => {
                let address = input
                    .address
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "read_memory requires `address`"))?;
                let count = input.limit.unwrap_or(64);
                let body = session
                    .call(
                        "readMemory",
                        json!({ "memoryReference": address, "count": count }),
                    )
                    .await?;
                let payload =
                    json!({ "action": "read_memory", "address": address, "result": body });
                Ok(text_output(payload.to_string(), payload))
            }
            "write_memory" => {
                let address = input
                    .address
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "write_memory requires `address`"))?;
                let data = input.data.as_deref().ok_or_else(|| {
                    tool_err("DAP_USAGE", "write_memory requires `data` (base64)")
                })?;
                let body = session
                    .call(
                        "writeMemory",
                        json!({ "memoryReference": address, "data": data }),
                    )
                    .await?;
                let payload =
                    json!({ "action": "write_memory", "address": address, "result": body });
                Ok(text_output(payload.to_string(), payload))
            }
            "modules" => {
                let body = session.call("modules", json!({})).await?;
                let payload = json!({ "action": "modules", "modules": body.get("modules").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "loaded_sources" => {
                let body = session.call("loadedSources", json!({})).await?;
                let payload = json!({ "action": "loaded_sources", "sources": body.get("sources").cloned().unwrap_or(Value::Null) });
                Ok(text_output(payload.to_string(), payload))
            }
            "custom_request" => {
                let command = input
                    .command
                    .as_deref()
                    .ok_or_else(|| tool_err("DAP_USAGE", "custom_request requires `command`"))?;
                let body = session
                    .call(command, input.payload.clone().unwrap_or_else(|| json!({})))
                    .await?;
                let payload =
                    json!({ "action": "custom_request", "command": command, "result": body });
                Ok(text_output(payload.to_string(), payload))
            }
            "output" => {
                let tail = session.output_tail();
                let payload = json!({ "action": "output", "tail": tail });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_inspection action {:?}", input.action),
            )),
        }
    }

    async fn run_lifecycle(&self, session: &DapSession, input: &DebugInput) -> Result<ToolOutput> {
        match input.action.as_str() {
            "terminate" => {
                session.terminate().await;
                lock(&self.session).take();
                let payload = json!({ "action": "terminate", "state": "exited" });
                Ok(text_output(payload.to_string(), payload))
            }
            "sessions" => {
                let state = session.state();
                let payload = json!({
                    "action": "sessions",
                    "sessions": [{ "id": 0, "state": state }],
                });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => Err(tool_err(
                "DAP_USAGE",
                format!("unknown run_lifecycle action {:?}", input.action),
            )),
        }
    }
    /// The stopped thread, or a named state error.
    fn current_thread(session: &DapSession) -> Result<u64> {
        match session.state() {
            ExecState::Stopped { thread_id, .. } => Ok(thread_id),
            ExecState::Running => Err(tool_err(
                "DAP_STATE_RUNNING",
                "debuggee is running; pause first or wait for a breakpoint",
            )),
            ExecState::Exited => Err(tool_err("DAP_STATE_EXITED", "debuggee exited")),
        }
    }

    /// Any thread id (for pause while running).
    async fn any_thread(&self, session: &DapSession) -> Result<u64> {
        if let ExecState::Stopped { thread_id, .. } = session.state() {
            return Ok(thread_id);
        }
        let body = session.call("threads", json!({})).await?;
        body.get("threads")
            .and_then(Value::as_array)
            .and_then(|threads| threads.first())
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| tool_err("DAP_NO_THREADS", "adapter reported no threads"))
    }

    /// Top frame of the stopped thread.
    async fn top_frame(&self, session: &DapSession) -> Result<u64> {
        let thread = Self::current_thread(session)?;
        let body = session
            .call_stopped(
                "stackTrace",
                json!({ "threadId": thread, "startFrame": 0, "levels": 1 }),
            )
            .await?;
        body.get("stackFrames")
            .and_then(Value::as_array)
            .and_then(|frames| frames.first())
            .and_then(|frame| frame.get("id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| tool_err("DAP_NO_FRAMES", "no stack frames"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DebugInput {
    action: String,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    adapter: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    expression: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    frame_id: Option<u64>,
    #[serde(default)]
    variables_reference: Option<u64>,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for DebugTool {
    fn name(&self) -> &str {
        "debug"
    }

    fn label(&self) -> &str {
        "debug"
    }

    fn description(&self) -> &str {
        "Drive a real debugger (DAP): launch/attach, breakpoints, step, evaluate, stack/memory reads. \
         One active session. Actions: launch, attach, set_breakpoint, remove_breakpoint, \
         set_function_breakpoint, set_instruction_breakpoint, remove_instruction_breakpoint, \
         data_breakpoint_info, set_data_breakpoint, remove_data_breakpoint, continue, step_over, \
         step_in, step_out, pause, evaluate, stack_trace, threads, scopes, variables, disassemble, \
         read_memory, write_memory, modules, loaded_sources, custom_request, output, terminate, \
         sessions. Stack operations require the stopped state and fail fast while running."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "launch", "attach", "set_breakpoint", "remove_breakpoint",
                        "set_function_breakpoint", "set_instruction_breakpoint",
                        "remove_instruction_breakpoint", "data_breakpoint_info",
                        "set_data_breakpoint", "remove_data_breakpoint", "continue",
                        "step_over", "step_in", "step_out", "pause", "evaluate",
                        "stack_trace", "threads", "scopes", "variables", "disassemble",
                        "read_memory", "write_memory", "modules", "loaded_sources",
                        "custom_request", "output", "terminate", "sessions"
                    ]
                },
                "program": { "type": "string", "description": "Binary/script to launch" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Program argv" },
                "adapter": { "type": "string", "description": "Adapter id override (lldb-dap, debugpy, dlv)" },
                "pid": { "type": "integer", "description": "Process id for attach" },
                "file": { "type": "string", "description": "Source file for breakpoints" },
                "line": { "type": "integer", "description": "1-indexed source line" },
                "name": { "type": "string", "description": "Function or data symbol name" },
                "reference": { "type": "string", "description": "Instruction/memory reference" },
                "address": { "type": "string", "description": "Memory address for read/write" },
                "data": { "type": "string", "description": "Base64 bytes for write_memory" },
                "expression": { "type": "string", "description": "Expression to evaluate" },
                "context": { "type": "string", "description": "Evaluate context (watch/repl/hover/clipboard)" },
                "frameId": { "type": "integer", "description": "Stack frame id (from stack_trace)" },
                "variablesReference": { "type": "integer", "description": "Scope/struct reference (from scopes/variables)" },
                "offset": { "type": "integer", "description": "Instruction offset for disassemble" },
                "limit": { "type": "integer", "description": "Result cap (frames/instructions/bytes)" },
                "command": { "type": "string", "description": "Raw DAP command for custom_request" },
                "payload": { "description": "Raw JSON arguments for custom_request" }
            },
            "required": ["action"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: DebugInput = serde_json::from_value(input)
            .map_err(|err| tool_err("DAP_USAGE", format!("invalid input: {err}")))?;
        match input.action.as_str() {
            "launch" => self.run_launch(&input).await,
            "attach" => self.run_attach(&input).await,
            "sessions" => {
                let has = lock(&self.session).is_some();
                if has {
                    self.run_simple(&input).await
                } else {
                    let payload = json!({ "action": "sessions", "sessions": [] });
                    Ok(text_output(payload.to_string(), payload))
                }
            }
            _ => self.run_simple(&input).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_is_named_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = DebugTool::new(temp.path(), None);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("runtime");
        let err = runtime
            .block_on(tool.execute("t", json!({"action": "threads"}), None))
            .expect_err("no session");
        assert!(err.to_string().contains("DAP_NO_SESSION"), "{err}");
    }

    #[test]
    fn launch_without_program_is_usage_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = DebugTool::new(temp.path(), None);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("runtime");
        let err = runtime
            .block_on(tool.execute("t", json!({"action": "launch"}), None))
            .expect_err("usage error");
        assert!(err.to_string().contains("DAP_USAGE"), "{err}");
    }

    #[test]
    fn missing_target_is_named_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = DebugTool::new(temp.path(), None);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("runtime");
        let err = runtime
            .block_on(tool.execute(
                "t",
                json!({"action": "launch", "program": "/nonexistent/binary"}),
                None,
            ))
            .expect_err("missing target");
        assert!(err.to_string().contains("DAP_TARGET_MISSING"), "{err}");
    }

    #[test]
    fn sessions_empty_without_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = DebugTool::new(temp.path(), None);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("runtime");
        let out = runtime
            .block_on(tool.execute("t", json!({"action": "sessions"}), None))
            .expect("sessions executes");
        let text = out.content.first().and_then(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|t| t.contains("\"sessions\":[]")));
    }
}
