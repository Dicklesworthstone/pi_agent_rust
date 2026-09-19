//! Agent-facing DAP debugger: owned sessions, retained breakpoint sets,
//! pre-execution configuration and suspension-scoped inspection (bd-cv653.1.2).

pub mod adapters;
mod breakpoints;
pub mod dap;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asupersync::sync::OwnedMutexGuard;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use adapters::AdapterSpec;
use breakpoints::{Change, Group};
use session::{DapSession, ExecState};

use crate::agent_cx::AgentCx;
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

pub struct DebugTool {
    cwd: PathBuf,
    adapters: Vec<AdapterSpec>,
    session: Mutex<Option<Arc<DapSession>>>,
    operations: Arc<asupersync::sync::Mutex<()>>,
}

impl DebugTool {
    #[must_use]
    pub fn new(cwd: &Path, _config: Option<&Config>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            adapters: adapters::default_adapters(),
            session: Mutex::new(None),
            operations: Arc::new(asupersync::sync::Mutex::new(())),
        }
    }

    #[must_use]
    pub fn with_adapters(mut self, adapters: Vec<AdapterSpec>) -> Self {
        self.adapters = adapters;
        self
    }

    fn session(&self) -> Result<Arc<DapSession>> {
        lock(&self.session).clone().ok_or_else(|| {
            tool_err(
                "DAP_NO_SESSION",
                "no active debug session; launch or attach first",
            )
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn run_start(&self, input: &DebugInput) -> Result<ToolOutput> {
        if lock(&self.session).is_some() {
            return Err(tool_err(
                "DAP_SESSION_EXISTS",
                "terminate or disconnect the existing debug session before launch or attach",
            ));
        }
        let program = if input.action == "launch" {
            let name = input.required("program", input.program.as_deref())?;
            let path = std::fs::canonicalize(self.cwd.join(name)).map_err(|error| {
                tool_err(
                    "DAP_TARGET_MISSING",
                    format!("cannot resolve launch target: {error}"),
                )
            })?;
            if !path.is_file() && !path.is_dir() {
                return Err(tool_err(
                    "DAP_TARGET_MISSING",
                    "program must be a regular file or a Go package directory",
                ));
            }
            Some(path)
        } else {
            input
                .pid
                .filter(|pid| *pid > 0)
                .ok_or_else(|| tool_err("DAP_USAGE", "debug attach requires a positive pid"))?;
            None
        };
        let initial = breakpoints::initial(
            &self.cwd,
            input.initial_breakpoints.as_deref().unwrap_or(&[]),
        )?;
        let requested = input
            .adapter
            .as_deref()
            .or_else(|| input.go_mode.as_ref().map(|_| "dlv"));
        let adapter = adapters::select_adapter(program.as_deref(), requested, &self.adapters)
            .ok_or_else(|| {
                tool_err(
                    "DAP_ADAPTER_MISSING",
                    "no matching debug adapter; install lldb-dap, debugpy, or dlv",
                )
            })?;
        if input.go_mode.is_some() && adapter.id != "dlv" {
            return Err(tool_err("DAP_USAGE", "goMode requires the dlv adapter"));
        }
        if program.as_ref().is_some_and(|path| path.is_dir()) && adapter.id != "dlv" {
            return Err(tool_err(
                "DAP_USAGE",
                "directory launch targets require the dlv adapter",
            ));
        }
        let command = adapter.resolve_command().ok_or_else(|| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!(
                    "adapter {} not on PATH. hint: {}",
                    adapter.id, adapter.install_hint
                ),
            )
        })?;
        let owner = AgentCx::for_current_or_request();
        if !owner.capabilities().io || !owner.capabilities().spawn || !owner.capabilities().time {
            return Err(tool_err(
                "DAP_PERMISSION",
                "debug launch requires I/O, spawn and timer capabilities",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "debug launch cancelled"))?;
        let mut arguments = program.as_ref().map_or_else(
            || adapters::attach_arguments(&adapter, input.pid.expect("validated pid")),
            |p| {
                adapters::launch_arguments(
                    &adapter,
                    p,
                    input.args.as_deref().unwrap_or(&[]),
                    &self.cwd,
                )
            },
        );
        if let Some(stop) = input.stop_on_entry {
            arguments["stopOnEntry"] = json!(stop);
        }
        if let Some(mode) = &input.go_mode {
            arguments["mode"] = json!(mode);
        }
        let mut artifacts = None;
        if adapter.id == "dlv"
            && let Some(program) = &program
        {
            if arguments["mode"] == "exec" {
                if !program.is_file() {
                    return Err(tool_err(
                        "DAP_USAGE",
                        "Go exec mode requires a prebuilt binary, not a directory",
                    ));
                }
            } else {
                if !(program.is_dir()
                    || program
                        .extension()
                        .is_some_and(|extension| extension == "go"))
                {
                    return Err(tool_err(
                        "DAP_USAGE",
                        "Go debug/test mode requires a .go file or package directory",
                    ));
                }
                let directory = tempfile::Builder::new().prefix("pi-debug-go-").tempdir()?;
                let output = directory.path().join(if cfg!(windows) {
                    "debuggee.exe"
                } else {
                    "debuggee"
                });
                arguments["output"] = json!(output.to_str().ok_or_else(|| {
                    tool_err(
                        "DAP_USAGE",
                        "Go build output directory must be representable as UTF-8",
                    )
                })?);
                artifacts = Some(directory);
            }
        }
        let go_mode = (adapter.id == "dlv" && program.is_some()).then(|| arguments["mode"].clone());
        let transport = if adapter.id == "dlv" {
            dap::DapTransport::spawn_delve(&command, &adapter.adapter_args, &[], &self.cwd).await?
        } else {
            dap::DapTransport::spawn(&command, &adapter.adapter_args, &[], &self.cwd)?
        };
        let session = DapSession::begin(transport)
            .await?
            .with_launch_artifacts(artifacts);
        let timeout_ms = input.startup_timeout_ms.unwrap_or(120_000);
        session
            .start(
                &input.action,
                arguments,
                &initial,
                input.exception_filters.as_deref(),
                Duration::from_millis(timeout_ms),
            )
            .await?;
        if program.is_some() && input.stop_on_entry.unwrap_or(true) {
            session.wait_stopped(Duration::from_secs(10)).await;
        }
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "debug startup cancelled"))?;
        let state = session.state();
        let payload = json!({
            "action": input.action, "program": program.as_ref().map(|path| path.display().to_string()),
            "pid": input.pid, "adapter": adapter.id, "goMode": go_mode,
            "transport": if adapter.id == "dlv" { "tcp" } else { "stdio" },
            "startupTimeoutMs": timeout_ms,
            "state": match &state {
                ExecState::Stopped { reason, .. } if reason == "entry" => "stopped_entry",
                ExecState::Stopped { .. } => "stopped",
                ExecState::Running => "running",
                ExecState::Exited => "exited"
            },
            "execution": state, "threadState": session.execution_snapshot(),
            "capabilities": session.capabilities()
        });
        *lock(&self.session) = Some(Arc::new(session));
        Ok(text_output(payload.to_string(), payload))
    }

    #[allow(clippy::too_many_lines)]
    async fn run_simple(&self, input: &DebugInput) -> Result<ToolOutput> {
        let session = self.session()?;
        let mut payload = match input.action.as_str() {
            "set_breakpoint"
            | "remove_breakpoint"
            | "set_function_breakpoint"
            | "remove_function_breakpoint"
            | "set_instruction_breakpoint"
            | "remove_instruction_breakpoint"
            | "set_data_breakpoint"
            | "remove_data_breakpoint" => self.run_breakpoints(&session, input).await?,
            "list_breakpoints" => breakpoints::inventory(&session).await?,
            "data_breakpoint_info" => {
                session.require_capability("supportsDataBreakpoints")?;
                let name = input.required("name", input.name.as_deref())?;
                let mut args = json!({"name": name});
                if let Some(reference) = input.variables_reference {
                    args["variablesReference"] = json!(reference);
                }
                if let Some(frame) = input.frame_id {
                    args["frameId"] = json!(frame);
                } else if input.variables_reference.is_none() {
                    args["frameId"] = json!(self.top_frame(&session, input.thread_id).await?);
                }
                json!({"result": session.call_stopped("dataBreakpointInfo", input.inspection_arguments(args)).await?})
            }
            "set_exception_breakpoints" => {
                let filters = input.exception_filters.as_deref().ok_or_else(|| {
                    tool_err(
                        "DAP_USAGE",
                        "set_exception_breakpoints requires exceptionFilters",
                    )
                })?;
                let caps = session.capabilities();
                for filter in filters {
                    if !caps["exceptionBreakpointFilters"]
                        .as_array()
                        .is_some_and(|supported| {
                            supported
                                .iter()
                                .any(|entry| entry["filter"].as_str() == Some(filter.as_str()))
                        })
                    {
                        return Err(tool_err(
                            "DAP_UNSUPPORTED",
                            format!("unsupported exception filter: {filter}"),
                        ));
                    }
                }
                json!({"result":session.call("setExceptionBreakpoints", json!({"filters":filters})).await?})
            }
            "continue" | "step_over" | "step_in" | "step_out" | "pause" => {
                self.run_execution(&session, input).await?
            }
            "evaluate" | "stack_trace" | "threads" | "scopes" | "variables" | "exception_info"
            | "set_variable" | "set_expression" => self.run_inspection(&session, input).await?,
            "disassemble" => {
                session.require_capability("supportsDisassembleRequest")?;
                let reference = input.required("reference", input.reference.as_deref())?;
                let body = session.call_stopped("disassemble", json!({
                    "memoryReference":reference, "instructionOffset":input.offset.unwrap_or(0),
                    "instructionCount":input.limit.unwrap_or(50), "resolveSymbols":true
                })).await?;
                json!({"instructions":body.get("instructions")})
            }
            "read_memory" | "write_memory" => {
                let writing = input.action == "write_memory";
                session.require_capability(if writing {
                    "supportsWriteMemoryRequest"
                } else {
                    "supportsReadMemoryRequest"
                })?;
                let address = input.required("address", input.address.as_deref())?;
                let mut args =
                    json!({"memoryReference":address, "offset":input.offset.unwrap_or(0)});
                if writing {
                    args["data"] = json!(input.required("data", input.data.as_deref())?);
                } else {
                    args["count"] = json!(input.limit.unwrap_or(64));
                }
                let body = session
                    .call_stopped(if writing { "writeMemory" } else { "readMemory" }, args)
                    .await?;
                json!({"address":address,"result":body})
            }
            "modules" => {
                session.require_capability("supportsModulesRequest")?;
                let body = session.call("modules", json!({"startModule":input.start.unwrap_or(0),"moduleCount":input.limit.unwrap_or(100)})).await?;
                json!({"modules":body.get("modules"),"totalModules":body.get("totalModules")})
            }
            "loaded_sources" => {
                session.require_capability("supportsLoadedSourcesRequest")?;
                let body = session.call("loadedSources", json!({})).await?;
                json!({"sources":body.get("sources")})
            }
            "custom_request" => {
                let command = input.required("command", input.command.as_deref())?;
                if matches!(
                    command,
                    "initialize"
                        | "launch"
                        | "attach"
                        | "configurationDone"
                        | "setBreakpoints"
                        | "setFunctionBreakpoints"
                        | "setInstructionBreakpoints"
                        | "setDataBreakpoints"
                        | "setExceptionBreakpoints"
                        | "terminate"
                        | "disconnect"
                        | "stackTrace"
                        | "scopes"
                        | "variables"
                        | "evaluate"
                        | "setVariable"
                        | "setExpression"
                        | "exceptionInfo"
                        | "dataBreakpointInfo"
                        | "continue"
                        | "next"
                        | "stepIn"
                        | "stepOut"
                        | "pause"
                        | "stepBack"
                        | "reverseContinue"
                        | "restartFrame"
                ) {
                    return Err(tool_err(
                        "DAP_USAGE",
                        "stateful DAP commands require a supported typed action, not custom_request",
                    ));
                }
                // Vendor requests may change debug state. Expire frame/object
                // handles before dispatch, including on cancellation or failure.
                session.invalidate_inspection()?;
                json!({"command":command,"result":session.call(command,input.payload.clone().unwrap_or_else(||json!({}))).await?})
            }
            "output" => json!({"tail":session.output_tail()}),
            "terminate" | "disconnect" => {
                let terminate = input.action == "terminate";
                let result = session.disconnect(terminate).await;
                if result.is_ok() || !session.is_connected() {
                    lock(&self.session).take();
                }
                result?;
                json!({"state":"disconnected","adapterAcknowledged":true,"debuggeeTerminationRequested":terminate})
            }
            "sessions" => json!({"sessions":[{"id":0,"state":session.state(),
                "execution":session.execution_snapshot(),"capabilities":session.capabilities()}]}),
            other => {
                return Err(tool_err(
                    "DAP_USAGE",
                    format!("unknown debug action {other:?}"),
                ));
            }
        };
        payload["action"] = json!(input.action);
        Ok(text_output(payload.to_string(), payload))
    }

    fn breakpoint_target(
        &self,
        input: &DebugInput,
        setting: bool,
    ) -> Result<(Group, Option<Value>, Value)> {
        match input.action.as_str() {
            "set_breakpoint" | "remove_breakpoint" => {
                let path = breakpoints::source_path(
                    &self.cwd,
                    input.required("file", input.file.as_deref())?,
                )?;
                let line = if setting {
                    Some(
                        input
                            .line
                            .ok_or_else(|| tool_err("DAP_USAGE", "set_breakpoint requires line"))?,
                    )
                } else {
                    input.line
                };
                let key = line
                    .map(|line| {
                        breakpoints::source_spec(
                            line,
                            input.column,
                            if setting {
                                input.condition.as_deref()
                            } else {
                                None
                            },
                            if setting {
                                input.hit_condition.as_deref()
                            } else {
                                None
                            },
                            if setting {
                                input.log_message.as_deref()
                            } else {
                                None
                            },
                        )
                    })
                    .transpose()?;
                Ok((
                    Group::Source(path.clone()),
                    key,
                    json!({"file":path,"line":line,"column":input.column}),
                ))
            }
            "set_function_breakpoint" | "remove_function_breakpoint" => {
                let name = if setting {
                    Some(input.required("name", input.name.as_deref())?)
                } else {
                    input.name.as_deref()
                };
                Ok((
                    Group::Function,
                    name.map(|name| json!({"name":name})),
                    json!({"name":name}),
                ))
            }
            "set_instruction_breakpoint" | "remove_instruction_breakpoint" => {
                let reference = if setting {
                    Some(input.required("reference", input.reference.as_deref())?)
                } else {
                    input.reference.as_deref()
                };
                Ok((
                    Group::Instruction,
                    reference.map(|reference| {
                        json!({"instructionReference":reference,"offset":input.offset.unwrap_or(0)})
                    }),
                    json!({"reference":reference,"offset":input.offset.unwrap_or(0)}),
                ))
            }
            "set_data_breakpoint" | "remove_data_breakpoint" => {
                let data_id = input.data_id.as_deref().or(input.name.as_deref());
                let data_id = if setting {
                    Some(input.required("dataId", data_id)?)
                } else {
                    data_id
                };
                Ok((
                    Group::Data,
                    data_id.map(|id| {
                        json!({"dataId":id,"accessType":input.access_type.as_deref().unwrap_or("write")})
                    }),
                    json!({"dataId":data_id}),
                ))
            }
            _ => Err(tool_err("DAP_USAGE", "unknown breakpoint action")),
        }
    }

    async fn run_breakpoints(&self, session: &DapSession, input: &DebugInput) -> Result<Value> {
        let setting = input.action.starts_with("set_");
        let (group, key, mut payload) = self.breakpoint_target(input, setting)?;
        let change = if setting {
            let mut spec = key.expect("setting requires a key");
            if !matches!(&group, Group::Source(_)) {
                if input.log_message.is_some() {
                    return Err(tool_err(
                        "DAP_USAGE",
                        "logMessage is supported on source breakpoints only",
                    ));
                }
                breakpoints::options(
                    &mut spec,
                    input.condition.as_deref(),
                    input.hit_condition.as_deref(),
                    None,
                )?;
            }
            breakpoints::check_options(session, &spec)?;
            Change::Upsert(spec)
        } else {
            Change::Remove(key)
        };
        let result = breakpoints::apply(session, group, change).await?;
        payload["verified"] = result["selected"]["verified"].clone();
        payload["breakpoint"] = result["selected"].clone();
        payload["breakpoints"] = result["breakpoints"].clone();
        payload["count"] = result["count"].clone();
        payload["synchronized"] = result["synchronized"].clone();
        Ok(payload)
    }

    async fn run_execution(&self, session: &DapSession, input: &DebugInput) -> Result<Value> {
        let thread = if input.action == "pause" {
            self.pause_thread(session, input.thread_id).await?
        } else {
            Self::current_thread(session, input.thread_id)?
        };
        if input.action == "pause" && session.require_thread(Some(thread)).is_ok() {
            return Ok(
                json!({"threadId":thread,"alreadyStopped":true,"execution":session.execution_snapshot()}),
            );
        }
        let command = match input.action.as_str() {
            "continue" => "continue",
            "step_over" => "next",
            "step_in" => "stepIn",
            "step_out" => "stepOut",
            _ => "pause",
        };
        let mut args = json!({"threadId":thread});
        if let Some(single) = input.single_thread {
            args["singleThread"] = json!(single);
        }
        if let Some(granularity) = &input.granularity {
            session.require_capability("supportsSteppingGranularity")?;
            args["granularity"] = json!(granularity);
        }
        let response = session.call(command, args).await?;
        let stopped = if input.action == "continue" {
            None
        } else {
            session.wait_stopped(Duration::from_secs(5)).await
        };
        AgentCx::for_current_or_request()
            .checkpoint()
            .map_err(|_| {
                tool_err(
                    "DAP_CANCELLED",
                    "debug execution wait cancelled; execution may already have resumed",
                )
            })?;
        Ok(
            json!({"threadId":thread,"response":response,"state":session.state(),
            "execution":session.execution_snapshot(),
            "stopped":stopped.map(|(thread,reason)|json!({"threadId":thread,"reason":reason}))}),
        )
    }

    #[allow(clippy::too_many_lines)]
    async fn run_inspection(&self, session: &DapSession, input: &DebugInput) -> Result<Value> {
        match input.action.as_str() {
            "evaluate" | "set_expression" => {
                let assigning = input.action == "set_expression";
                if assigning {
                    session.require_capability("supportsSetExpression")?;
                }
                let expression = input.required("expression", input.expression.as_deref())?;
                let frame = match input.frame_id {
                    Some(frame) => frame,
                    None => self.top_frame(session, input.thread_id).await?,
                };
                let mut args = json!({"expression":expression,"frameId":frame});
                if assigning {
                    args["value"] = json!(input.assignment_value()?);
                } else {
                    args["context"] = json!(input.context.as_deref().unwrap_or("repl"));
                }
                let mut body = session
                    .call_stopped(
                        if assigning {
                            "setExpression"
                        } else {
                            "evaluate"
                        },
                        input.inspection_arguments(args),
                    )
                    .await?;
                body["expression"] = json!(expression);
                body["frameId"] = json!(frame);
                if assigning {
                    body["refreshVariableReferences"] = json!(true);
                }
                Ok(body)
            }
            "set_variable" => {
                session.require_capability("supportsSetVariable")?;
                let reference = input
                    .variables_reference
                    .filter(|reference| *reference > 0)
                    .ok_or_else(|| {
                        tool_err(
                            "DAP_USAGE",
                            "set_variable requires a positive variablesReference",
                        )
                    })?;
                let name = input.required("name", input.name.as_deref())?;
                let args = json!({"variablesReference":reference,"name":name,"value":input.assignment_value()?});
                let mut body = session
                    .call_stopped("setVariable", input.inspection_arguments(args))
                    .await?;
                body["name"] = json!(name);
                body["refreshVariableReferences"] = json!(true);
                Ok(body)
            }
            "exception_info" => {
                session.require_capability("supportsExceptionInfoRequest")?;
                let thread = Self::current_thread(session, input.thread_id)?;
                let mut body = session
                    .call_stopped("exceptionInfo", json!({"threadId":thread}))
                    .await?;
                body["threadId"] = json!(thread);
                Ok(body)
            }
            "stack_trace" => {
                let thread = Self::current_thread(session, input.thread_id)?;
                let body = session
                    .call_stopped(
                        "stackTrace",
                        json!({"threadId":thread,
                    "startFrame":input.start.unwrap_or(0),"levels":input.limit.unwrap_or(50)}),
                    )
                    .await?;
                Ok(
                    json!({"threadId":thread,"frames":body.get("stackFrames"),"totalFrames":body.get("totalFrames")}),
                )
            }
            "threads" => {
                let body = session.call("threads", json!({})).await?;
                Ok(json!({"threads":body.get("threads"),"execution":session.execution_snapshot()}))
            }
            "scopes" => {
                let frame = match input.frame_id {
                    Some(frame) => frame,
                    None => self.top_frame(session, input.thread_id).await?,
                };
                let body = session
                    .call_stopped(
                        "scopes",
                        input.inspection_arguments(json!({"frameId":frame})),
                    )
                    .await?;
                Ok(json!({"frameId":frame,"scopes":body.get("scopes")}))
            }
            "variables" => {
                let reference = input
                    .variables_reference
                    .filter(|reference| *reference > 0)
                    .ok_or_else(|| {
                        tool_err(
                            "DAP_USAGE",
                            "variables requires a positive variablesReference",
                        )
                    })?;
                let mut args = json!({"variablesReference":reference});
                if let Some(start) = input.start {
                    args["start"] = json!(start);
                }
                if let Some(count) = input.limit {
                    args["count"] = json!(count);
                }
                if let Some(filter) = &input.filter {
                    args["filter"] = json!(filter);
                }
                let body = session
                    .call_stopped("variables", input.inspection_arguments(args))
                    .await?;
                Ok(json!({"variablesReference":reference,"variables":body.get("variables")}))
            }
            _ => Err(tool_err("DAP_USAGE", "unknown inspection action")),
        }
    }

    fn current_thread(session: &DapSession, selected: Option<u64>) -> Result<u64> {
        let stopped = if selected.is_some() {
            session.require_thread(selected)?
        } else {
            session.require_stopped()?
        };
        (stopped != 0).then_some(stopped).ok_or_else(|| {
            tool_err(
                "DAP_NO_THREADS",
                "stopped event omitted threadId; query threads and supply threadId",
            )
        })
    }

    async fn pause_thread(&self, session: &DapSession, selected: Option<u64>) -> Result<u64> {
        let body = session.call("threads", json!({})).await?;
        let threads = body["threads"]
            .as_array()
            .ok_or_else(|| tool_err("DAP_NO_THREADS", "adapter reported no threads"))?;
        if let Some(selected) = selected {
            return threads
                .iter()
                .any(|thread| thread["id"].as_u64() == Some(selected))
                .then_some(selected)
                .ok_or_else(|| {
                    tool_err(
                        "DAP_THREAD_UNKNOWN",
                        "pause thread is not in the current inventory",
                    )
                });
        }
        let execution = session.execution_snapshot();
        execution["threads"]
            .as_array()
            .and_then(|threads| threads.iter().find(|thread| thread["state"] == "running"))
            .and_then(|thread| thread["id"].as_u64())
            .or_else(|| threads.first().and_then(|thread| thread["id"].as_u64()))
            .ok_or_else(|| tool_err("DAP_NO_THREADS", "adapter reported no threads"))
    }

    async fn top_frame(&self, session: &DapSession, thread: Option<u64>) -> Result<u64> {
        let thread = Self::current_thread(session, thread)?;
        let body = session
            .call_stopped(
                "stackTrace",
                json!({"threadId":thread,"startFrame":0,"levels":1}),
            )
            .await?;
        body["stackFrames"]
            .as_array()
            .and_then(|frames| frames.first())
            .and_then(|frame| frame["id"].as_u64())
            .ok_or_else(|| tool_err("DAP_NO_FRAMES", "no stack frames"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DebugInput {
    action: String,
    program: Option<String>,
    args: Option<Vec<String>>,
    adapter: Option<String>,
    go_mode: Option<String>,
    startup_timeout_ms: Option<u64>,
    pid: Option<u32>,
    file: Option<String>,
    line: Option<u64>,
    column: Option<u64>,
    condition: Option<String>,
    hit_condition: Option<String>,
    log_message: Option<String>,
    name: Option<String>,
    value: Option<String>,
    data_id: Option<String>,
    access_type: Option<String>,
    reference: Option<String>,
    address: Option<String>,
    data: Option<String>,
    expression: Option<String>,
    context: Option<String>,
    thread_id: Option<u64>,
    single_thread: Option<bool>,
    granularity: Option<String>,
    frame_id: Option<u64>,
    variables_reference: Option<u64>,
    offset: Option<i64>,
    start: Option<u64>,
    limit: Option<u64>,
    filter: Option<String>,
    command: Option<String>,
    payload: Option<Value>,
    initial_breakpoints: Option<Vec<breakpoints::SourceInput>>,
    exception_filters: Option<Vec<String>>,
    stop_on_entry: Option<bool>,
}

impl DebugInput {
    fn required<'a>(&self, field: &str, value: Option<&'a str>) -> Result<&'a str> {
        value
            .filter(|value| {
                !value.is_empty() && !value.contains('\0') && value.len() <= 1024 * 1024
            })
            .ok_or_else(|| {
                tool_err(
                    "DAP_USAGE",
                    format!("{} requires nonempty, NUL-free {field}", self.action),
                )
            })
    }

    fn assignment_value(&self) -> Result<&str> {
        self.value
            .as_deref()
            .ok_or_else(|| tool_err("DAP_USAGE", "assignment requires value as a string"))
    }

    fn inspection_arguments(&self, mut args: Value) -> Value {
        if let Some(thread) = self.thread_id {
            args["threadId"] = json!(thread);
        }
        args
    }

    fn validate(&self) -> Result<()> {
        if self
            .limit
            .is_some_and(|limit| limit == 0 || limit > 1_000_000)
            || self.start.is_some_and(|start| start > 2_147_483_647)
            || self
                .offset
                .is_some_and(|offset| !(-2_147_483_648..=2_147_483_647).contains(&offset))
            || self
                .thread_id
                .is_some_and(|thread| thread == 0 || thread > 2_147_483_647)
            || self
                .frame_id
                .is_some_and(|frame| frame == 0 || frame > session::MAX_INSPECTION_HANDLE)
            || self
                .variables_reference
                .is_some_and(|reference| reference > session::MAX_INSPECTION_HANDLE)
        {
            return Err(tool_err(
                "DAP_USAGE",
                "invalid DAP identifier, handle, offset or result limit",
            ));
        }
        let stepping = matches!(self.action.as_str(), "step_over" | "step_in" | "step_out");
        if self.single_thread.is_some() && !stepping && self.action != "continue" {
            return Err(tool_err(
                "DAP_USAGE",
                "singleThread applies to continue or step actions only",
            ));
        }
        if self.granularity.as_deref().is_some_and(|value| {
            !stepping || !matches!(value, "statement" | "line" | "instruction")
        }) {
            return Err(tool_err(
                "DAP_USAGE",
                "granularity is step-only and must be statement, line or instruction",
            ));
        }
        if self.value.as_ref().is_some_and(|value| {
            value.len() > 64 * 1024
                || value.contains('\0')
                || !matches!(self.action.as_str(), "set_variable" | "set_expression")
        }) {
            return Err(tool_err(
                "DAP_USAGE",
                "value is assignment-only, NUL-free and at most 64 KiB",
            ));
        }
        if self.go_mode.as_deref().is_some_and(|mode| {
            self.action != "launch" || !matches!(mode, "debug" | "test" | "exec")
        }) {
            return Err(tool_err(
                "DAP_USAGE",
                "goMode is launch-only and must be debug, test or exec",
            ));
        }
        if self.startup_timeout_ms.is_some_and(|timeout| {
            !(1..=300_000).contains(&timeout)
                || !matches!(self.action.as_str(), "launch" | "attach")
        }) {
            return Err(tool_err(
                "DAP_USAGE",
                "startupTimeoutMs is launch/attach-only and must be in 1..=300000",
            ));
        }
        if self
            .filter
            .as_deref()
            .is_some_and(|filter| !matches!(filter, "named" | "indexed"))
        {
            return Err(tool_err("DAP_USAGE", "filter must be named or indexed"));
        }
        if self
            .access_type
            .as_deref()
            .is_some_and(|access| !matches!(access, "read" | "write" | "readWrite"))
        {
            return Err(tool_err(
                "DAP_USAGE",
                "accessType must be read, write or readWrite",
            ));
        }
        if self.exception_filters.as_ref().is_some_and(|filters| {
            filters.len() > 64
                || filters
                    .iter()
                    .any(|filter| filter.is_empty() || filter.len() > 256)
        }) {
            return Err(tool_err(
                "DAP_USAGE",
                "exceptionFilters must contain at most 64 nonempty filter IDs",
            ));
        }
        Ok(())
    }
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
        "Drive a real native/Python/Go debugger: launch/attach, retained breakpoints, thread-aware stepping, exception diagnosis and expandable object inspection. set_variable/set_expression change live debuggee state when supported. Pass frameId/variablesReference returned by this tool, never raw adapter IDs; refresh them after resuming. singleThread requires adapter support. Use disconnect to preserve an attached target, terminate to request termination."
    }
    fn parameters(&self) -> Value {
        json!({
            "type":"object", "required":["action"],
            "properties": {
                "action":{"type":"string","enum":["launch","attach","set_breakpoint","remove_breakpoint",
                    "set_function_breakpoint","remove_function_breakpoint","set_instruction_breakpoint","remove_instruction_breakpoint",
                    "data_breakpoint_info","set_data_breakpoint","remove_data_breakpoint","list_breakpoints","set_exception_breakpoints",
                    "continue","step_over","step_in","step_out","pause","evaluate","stack_trace","threads","scopes","variables",
                    "exception_info","set_variable","set_expression","disassemble","read_memory","write_memory","modules",
                    "loaded_sources","custom_request","output","terminate","disconnect","sessions"]},
                "program":{"type":"string","description":"Binary/script or local Go package directory to launch"},
                "args":{"type":"array","items":{"type":"string"}},
                "adapter":{"type":"string","description":"Registered adapter ID; use dlv for a precompiled Go binary or attach"},
                "goMode":{"type":"string","enum":["debug","test","exec"]},
                "startupTimeoutMs":{"type":"integer","minimum":1,"maximum":300_000,"default":120_000,"description":"Launch/attach and configuration budget, including Go compilation; excludes adapter discovery and initialize"},
                "pid":{"type":"integer","minimum":1},
                "file":{"type":"string","description":"Source path for breakpoint set/remove"},
                "line":{"type":"integer","minimum":1,"description":"1-based line; omit on removal to clear the file's set"},
                "column":{"type":"integer","minimum":1},
                "condition":{"type":"string","maxLength":4096},
                "hitCondition":{"type":"string","maxLength":4096},
                "logMessage":{"type":"string","maxLength":4096},
                "name":{"type":"string","description":"Function, data symbol or variable name in its parent container"},
                "value":{"type":"string","maxLength":65536,"description":"set_variable/set_expression value, parsed by the debuggee language; changes live state"},
                "dataId":{"type":"string","description":"Opaque data breakpoint ID returned by data_breakpoint_info"},
                "accessType":{"type":"string","enum":["read","write","readWrite"]},
                "reference":{"type":"string","description":"Instruction reference"},
                "address":{"type":"string","description":"Memory reference"},
                "data":{"type":"string","description":"Base64 for write_memory"},
                "expression":{"type":"string"}, "context":{"type":"string"},
                "threadId":{"type":"integer","minimum":1,"description":"Selected thread; when combined with a handle, must own it"},
                "singleThread":{"type":"boolean","description":"Continue/step only this thread; requires supportsSingleThreadExecutionRequests"},
                "granularity":{"type":"string","enum":["statement","line","instruction"],"description":"Step granularity; requires adapter support"},
                "frameId":{"type":"integer","minimum":1,"description":"Opaque frame handle from stack_trace; expires when its thread resumes"},
                "variablesReference":{"type":"integer","minimum":0,"description":"Opaque object/container handle from scopes/evaluate/variables; zero means no children"},
                "offset":{"type":"integer"}, "start":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":1_000_000},
                "filter":{"type":"string","enum":["named","indexed"]},
                "command":{"type":"string"}, "payload":{"type":"object"},
                "stopOnEntry":{"type":"boolean","default":true},
                "exceptionFilters":{"type":"array","maxItems":64,"items":{"type":"string"}},
                "initialBreakpoints":{"type":"array","maxItems":1024,
                    "items":{"type":"object","required":["file","line"],"additionalProperties":false,
                        "properties":{"file":{"type":"string"},"line":{"type":"integer","minimum":1},
                            "column":{"type":"integer","minimum":1},"condition":{"type":"string"},
                            "hitCondition":{"type":"string"},"logMessage":{"type":"string"}}}}
            }
        })
    }
    fn effects(&self) -> ToolEffects {
        ToolEffects::process().union(ToolEffects::network())
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: DebugInput = serde_json::from_value(input)
            .map_err(|error| tool_err("DAP_USAGE", format!("invalid input: {error}")))?;
        input.validate()?;
        let owner = AgentCx::for_current_or_request();
        let _operation = OwnedMutexGuard::lock(Arc::clone(&self.operations), owner.cx())
            .await
            .map_err(|_| tool_err("DAP_CANCELLED", "debug operation cancelled while queued"))?;
        match input.action.as_str() {
            "launch" | "attach" => self.run_start(&input).await,
            "sessions" if lock(&self.session).is_none() => {
                let payload = json!({"action":"sessions","sessions":[]});
                Ok(text_output(payload.to_string(), payload))
            }
            _ => self.run_simple(&input).await,
        }
    }
}

#[cfg(test)]
mod tests;
